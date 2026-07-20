//! AFT/EFT rail: the clearing/settlement plumbing. Batch accrual, CPA-005 file
//! emit/ingest, the settlement-window sweep, and post-settlement returns are
//! orchestration in `handlers/aft.rs`, built on top of these verbs.

use async_trait::async_trait;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::config::database::DatabasePool;
use crate::errors::AppError;
use crate::handlers::AppState;

use super::common::{self, RailCtx};
use super::{Destination, Hold, PgTx, Rail, RailId, RailPosting};

/// AFT's own synthetic system customer — SEPARATE from the card rails'
/// `system@nano.bank` and Interac's `interac@nano.bank`, because GL accounts are
/// keyed by (customer, account_type). AFT does not reuse any other rail's
/// system account.
const AFT_CUSTOMER_EMAIL: &str = "aft@nano.bank";

#[derive(Clone, Copy, Debug)]
pub struct AftAccounts {
    pub clearing_id: Uuid,
    pub settlement_id: Uuid,
}

/// The AFT rail. Carries the resolved clearing/settlement ids (re-resolved per
/// request by the handler, because a data wipe rebuilds them).
#[derive(Clone, Copy, Debug)]
pub struct AftRail {
    pub accounts: AftAccounts,
}

impl AftRail {
    pub fn new(accounts: AftAccounts) -> Self {
        Self { accounts }
    }
    pub fn id(&self) -> RailId {
        RailId::Aft
    }
}

/// Create AFT's system customer + two GL accounts if absent; return ids.
/// Idempotent — mirrors `handlers::cards::ensure_system_accounts`.
pub async fn ensure_aft_accounts(pool: &DatabasePool) -> Result<AftAccounts, sqlx::Error> {
    let (clearing_id, settlement_id) = common::ensure_rail_accounts(
        pool,
        AFT_CUSTOMER_EMAIL,
        "+10000000003",
        "Aft",
        "000000003",
        "AFT",
    )
    .await?;
    Ok(AftAccounts {
        clearing_id,
        settlement_id,
    })
}

impl AftRail {
    fn ctx(&self) -> RailCtx {
        RailCtx {
            id: RailId::Aft,
            clearing_id: self.accounts.clearing_id,
            settlement_id: self.accounts.settlement_id,
        }
    }
}

// The clearing/settlement double-entry + aggregate GL post live in `rails::common`.
#[async_trait]
impl Rail for AftRail {
    fn id(&self) -> RailId {
        RailId::Aft
    }

    async fn hold(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        from: Uuid,
        amount: Decimal,
        description: &str,
    ) -> Result<Hold, AppError> {
        common::hold(self.ctx(), state, tx, from, amount, description).await
    }

    async fn release(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        hold: &Hold,
        dest: Destination,
        description: &str,
    ) -> Result<RailPosting, AppError> {
        common::release(self.ctx(), state, tx, hold, dest, description).await
    }

    async fn refund(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        hold: &Hold,
        description: &str,
    ) -> Result<RailPosting, AppError> {
        common::refund(self.ctx(), state, tx, hold, description).await
    }

    async fn accept_inbound(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        to: Uuid,
        amount: Decimal,
        description: &str,
    ) -> Result<RailPosting, AppError> {
        common::accept_inbound(self.ctx(), state, tx, to, amount, description).await
    }
}
