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
use serde_json::json;
use uuid::Uuid;

use crate::config::database::DatabasePool;
use crate::errors::AppError;
use crate::handlers::cards::{post_gl_entry, post_two_legged, reference_number};
use crate::handlers::AppState;
use crate::ledger::Account as GlAccount;

use super::{Destination, Hold, PgTx, Rail, RailId, RailPosting};

/// Lynx's own synthetic system customer — SEPARATE from the card rails'
/// `system@nano.bank`, Interac's `interac@nano.bank`, and AFT's `aft@nano.bank`,
/// because GL accounts are keyed by (customer, account_type).
const LYNX_CUSTOMER_EMAIL: &str = "lynx@nano.bank";
const CLEARING_TYPE: &str = "chequing"; // LYNX_CLEARING
const SETTLEMENT_TYPE: &str = "savings"; // LYNX_SETTLEMENT

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
        let txn_id = new_txn(tx, &reference, "lynx_clawback", amount, description, None).await?;
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
        tag_gl(tx, txn_id, &gl_ref).await?;
        Ok(RailPosting {
            transaction_id: txn_id,
            gl_entry: Some(gl_ref),
        })
    }
}

/// Create Lynx's system customer + two GL accounts if absent; return ids.
/// Idempotent — mirrors `rails::aft::ensure_aft_accounts`.
pub async fn ensure_lynx_accounts(pool: &DatabasePool) -> Result<LynxAccounts, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO customers (email, phone_number, first_name, last_name, date_of_birth, sin)
        VALUES ($1, '+10000000004', 'Nano', 'Lynx', '1970-01-01', '000000004')
        ON CONFLICT (email) DO NOTHING
        "#,
    )
    .bind(LYNX_CUSTOMER_EMAIL)
    .execute(pool)
    .await?;

    let customer_id: Uuid =
        sqlx::query_scalar("SELECT customer_id FROM customers WHERE email = $1")
            .bind(LYNX_CUSTOMER_EMAIL)
            .fetch_one(pool)
            .await?;

    let clearing_id = ensure_gl_account(pool, customer_id, CLEARING_TYPE).await?;
    let settlement_id = ensure_gl_account(pool, customer_id, SETTLEMENT_TYPE).await?;
    tracing::info!(%clearing_id, %settlement_id, "✅ Lynx GL accounts ready");
    Ok(LynxAccounts {
        clearing_id,
        settlement_id,
    })
}

async fn ensure_gl_account(
    pool: &DatabasePool,
    customer_id: Uuid,
    account_type: &str,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO accounts
            (customer_id, account_number, account_type, status, overdraft_limit, activated_at)
        SELECT $1, '000000000000', $2::account_type, 'active', 1000000000000, CURRENT_TIMESTAMP
        WHERE NOT EXISTS (
            SELECT 1 FROM accounts WHERE customer_id = $1 AND account_type = $2::account_type
        )
        "#,
    )
    .bind(customer_id)
    .bind(account_type)
    .execute(pool)
    .await?;

    sqlx::query_scalar(
        "SELECT account_id FROM accounts WHERE customer_id = $1 AND account_type = $2::account_type \
         ORDER BY created_at LIMIT 1",
    )
    .bind(customer_id)
    .bind(account_type)
    .fetch_one(pool)
    .await
}

/// Create a completed `transactions` row for one rail movement; return its id.
async fn new_txn(
    tx: &mut PgTx<'_>,
    reference: &str,
    txn_type: &str,
    amount: Decimal,
    description: &str,
    initiated_by: Option<Uuid>,
) -> Result<Uuid, AppError> {
    let id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO transactions
            (reference_number, transaction_type, amount, description, status,
             initiated_by, completed_at, metadata)
        VALUES ($1, $2, $3, $4, 'completed', $5, CURRENT_TIMESTAMP, $6)
        RETURNING transaction_id
        "#,
    )
    .bind(reference)
    .bind(txn_type)
    .bind(amount)
    .bind(description)
    .bind(initiated_by)
    .bind(json!({ "rail": "lynx" }))
    .fetch_one(&mut **tx)
    .await?;
    Ok(id)
}

async fn tag_gl(tx: &mut PgTx<'_>, txn_id: Uuid, gl: &str) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE transactions SET metadata = jsonb_set(COALESCE(metadata,'{}'::jsonb), \
         '{gl_entry}', to_jsonb($2::text)) WHERE transaction_id = $1",
    )
    .bind(txn_id)
    .bind(gl)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

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
        let txn_id = new_txn(tx, &reference, "lynx_hold", amount, description, None).await?;
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
        tag_gl(tx, txn_id, &format!("{}:{}", gl.backend, gl.id)).await?;
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
        let txn_id = new_txn(tx, &reference, "lynx_settle", hold.amount, description, None).await?;
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
        tag_gl(tx, txn_id, &gl_ref).await?;
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
        let txn_id = new_txn(tx, &reference, "lynx_refund", hold.amount, description, None).await?;
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
        tag_gl(tx, txn_id, &gl_ref).await?;
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
        let txn_id = new_txn(tx, &reference, "lynx_inbound", amount, description, None).await?;
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
        tag_gl(tx, txn_id, &gl_ref).await?;
        Ok(RailPosting {
            transaction_id: txn_id,
            gl_entry: Some(gl_ref),
        })
    }
}
