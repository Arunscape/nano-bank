//! Lynx RTGS wire rail: the clearing/settlement plumbing. The wire lifecycle
//! (send/settle, inbound, recall both ways, the stale-wire sweep, ISO 20022
//! messaging) is orchestration in `handlers/lynx.rs`, built on these verbs.
//!
//! Unlike Interac/AFT, Lynx's GL reflects real central-bank settlement: the
//! settle leg posts `Payable → Bank` (money leaves the bank) and inbound posts
//! `Bank → Payable` (central-bank money arrives immediately) — where AFT's
//! inbound is a `Receivable` until ACSS settles.

use async_trait::async_trait;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::config::database::DatabasePool;
use crate::errors::AppError;
use crate::handlers::cards::{post_gl_entry, post_two_legged, reference_number};
use crate::handlers::AppState;
use crate::ledger::Account as GlAccount;

use super::common::{self, RailCtx};
use super::{Destination, Hold, PgTx, Rail, RailId, RailPosting};

/// Lynx's own synthetic system customer — SEPARATE from the card rails'
/// `system@nano.bank`, Interac's `interac@nano.bank`, and AFT's `aft@nano.bank`,
/// because GL accounts are keyed by (customer, account_type).
const LYNX_CUSTOMER_EMAIL: &str = "lynx@nano.bank";

#[derive(Clone, Copy, Debug)]
pub struct LynxAccounts {
    pub clearing_id: Uuid,
    pub settlement_id: Uuid,
}

/// The Lynx rail. Carries the resolved clearing/settlement ids (re-resolved per
/// request by the handler, because a data wipe rebuilds them).
#[derive(Clone, Copy, Debug)]
pub struct LynxRail {
    pub accounts: LynxAccounts,
}

impl LynxRail {
    pub fn new(accounts: LynxAccounts) -> Self {
        Self { accounts }
    }
    pub fn id(&self) -> RailId {
        RailId::Lynx
    }

    fn ctx(&self) -> RailCtx {
        RailCtx {
            id: RailId::Lynx,
            clearing_id: self.accounts.clearing_id,
            settlement_id: self.accounts.settlement_id,
        }
    }

    /// Claw back a settled inbound wire from the beneficiary customer: Dr `from`
    /// (customer) / Cr LYNX_SETTLEMENT; GL Payable → Bank (money returned to the
    /// network). Used by the inbound-recall accept path.
    pub async fn clawback(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        from: Uuid,
        amount: Decimal,
        description: &str,
    ) -> Result<RailPosting, AppError> {
        let reference = reference_number("LYNXC");
        let txn_id =
            common::new_txn(tx, self.ctx(), "clawback", &reference, amount, description).await?;
        post_two_legged(
            tx,
            txn_id,
            from,
            "debit",
            self.accounts.settlement_id,
            "credit",
            amount,
        )
        .await?;
        let gl = post_gl_entry(
            state,
            &reference,
            description,
            GlAccount::Payable,
            GlAccount::Bank,
            amount,
        )
        .await?;
        let gl_ref = format!("{}:{}", gl.backend, gl.id);
        common::tag_gl(tx, txn_id, &gl_ref).await?;
        Ok(RailPosting {
            transaction_id: txn_id,
            gl_entry: Some(gl_ref),
        })
    }
}

/// Create Lynx's system customer + two GL accounts if absent; return ids.
/// Idempotent — delegates to the shared `rails::common` bootstrap.
pub async fn ensure_lynx_accounts(pool: &DatabasePool) -> Result<LynxAccounts, sqlx::Error> {
    let (clearing_id, settlement_id) = common::ensure_rail_accounts(
        pool,
        LYNX_CUSTOMER_EMAIL,
        "+10000000004",
        "Lynx",
        "000000004",
        "Lynx",
    )
    .await?;
    Ok(LynxAccounts {
        clearing_id,
        settlement_id,
    })
}

// Lynx keeps its own Rail verbs below because its GL differs from Interac/AFT
// (settle Payable→Bank, inbound Bank→Payable), but reuses the shared
// `common::{new_txn, tag_gl}` plumbing and the `common` account bootstrap.
#[async_trait]
impl Rail for LynxRail {
    fn id(&self) -> RailId {
        RailId::Lynx
    }

    /// Reserve funds for an outbound wire: Dr `from` / Cr LYNX_CLEARING.
    /// GL: Payable → Payable (net zero — money hasn't left the bank yet).
    async fn hold(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        from: Uuid,
        amount: Decimal,
        description: &str,
    ) -> Result<Hold, AppError> {
        let reference = reference_number("LYNXH");
        let txn_id =
            common::new_txn(tx, self.ctx(), "hold", &reference, amount, description).await?;
        post_two_legged(
            tx,
            txn_id,
            from,
            "debit",
            self.accounts.clearing_id,
            "credit",
            amount,
        )
        .await?;
        let gl = post_gl_entry(
            state,
            &reference,
            description,
            GlAccount::Payable,
            GlAccount::Payable,
            amount,
        )
        .await?;
        common::tag_gl(tx, txn_id, &format!("{}:{}", gl.backend, gl.id)).await?;
        Ok(Hold {
            from_account: from,
            amount,
            reference,
            transaction_id: txn_id,
        })
    }

    /// Settle a held wire. External (the only Lynx case): Dr LYNX_CLEARING /
    /// Cr LYNX_SETTLEMENT; GL Payable → Bank (money leaves the bank — finality).
    /// Internal is retained for trait completeness (net-zero reclass).
    async fn release(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        hold: &Hold,
        dest: Destination,
        description: &str,
    ) -> Result<RailPosting, AppError> {
        let reference = reference_number("LYNXS");
        let txn_id = common::new_txn(
            tx,
            self.ctx(),
            "settle",
            &reference,
            hold.amount,
            description,
        )
        .await?;
        let (credit_account, gl_credit) = match dest {
            Destination::Internal(acct) => (acct, GlAccount::Payable),
            Destination::External(_) => (self.accounts.settlement_id, GlAccount::Bank),
        };
        post_two_legged(
            tx,
            txn_id,
            self.accounts.clearing_id,
            "debit",
            credit_account,
            "credit",
            hold.amount,
        )
        .await?;
        let gl = post_gl_entry(
            state,
            &reference,
            description,
            GlAccount::Payable,
            gl_credit,
            hold.amount,
        )
        .await?;
        let gl_ref = format!("{}:{}", gl.backend, gl.id);
        common::tag_gl(tx, txn_id, &gl_ref).await?;
        Ok(RailPosting {
            transaction_id: txn_id,
            gl_entry: Some(gl_ref),
        })
    }

    /// Return a never-settled hold to its origin: Dr LYNX_CLEARING / Cr origin.
    /// GL: Payable → Payable (the reservation is released; money never left).
    async fn refund(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        hold: &Hold,
        description: &str,
    ) -> Result<RailPosting, AppError> {
        let reference = reference_number("LYNXX");
        let txn_id = common::new_txn(
            tx,
            self.ctx(),
            "refund",
            &reference,
            hold.amount,
            description,
        )
        .await?;
        post_two_legged(
            tx,
            txn_id,
            self.accounts.clearing_id,
            "debit",
            hold.from_account,
            "credit",
            hold.amount,
        )
        .await?;
        let gl = post_gl_entry(
            state,
            &reference,
            description,
            GlAccount::Payable,
            GlAccount::Payable,
            hold.amount,
        )
        .await?;
        let gl_ref = format!("{}:{}", gl.backend, gl.id);
        common::tag_gl(tx, txn_id, &gl_ref).await?;
        Ok(RailPosting {
            transaction_id: txn_id,
            gl_entry: Some(gl_ref),
        })
    }

    /// Credit an inbound wire straight to a customer: Dr LYNX_SETTLEMENT / Cr
    /// `to`. GL: Bank → Payable (real central-bank money arrived immediately).
    async fn accept_inbound(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        to: Uuid,
        amount: Decimal,
        description: &str,
    ) -> Result<RailPosting, AppError> {
        let reference = reference_number("LYNXI");
        let txn_id =
            common::new_txn(tx, self.ctx(), "inbound", &reference, amount, description).await?;
        post_two_legged(
            tx,
            txn_id,
            self.accounts.settlement_id,
            "debit",
            to,
            "credit",
            amount,
        )
        .await?;
        let gl = post_gl_entry(
            state,
            &reference,
            description,
            GlAccount::Bank,
            GlAccount::Payable,
            amount,
        )
        .await?;
        let gl_ref = format!("{}:{}", gl.backend, gl.id);
        common::tag_gl(tx, txn_id, &gl_ref).await?;
        Ok(RailPosting {
            transaction_id: txn_id,
            gl_entry: Some(gl_ref),
        })
    }
}
