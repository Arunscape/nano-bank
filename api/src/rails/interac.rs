//! Interac e-Transfer rail: the clearing/settlement plumbing. The product
//! lifecycle lives in `handlers/interac.rs`.

use async_trait::async_trait;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::config::database::DatabasePool;
use crate::errors::AppError;
use crate::handlers::AppState;
use crate::models::interac::HandleType;

use super::common::{self, RailCtx};
use super::{Destination, Hold, PgTx, Rail, RailId, RailPosting};

/// Interac's own synthetic system customer — SEPARATE from the card rails'
/// `system@nano.bank`, because GL accounts are keyed by (customer, account_type)
/// and that customer already uses its chequing/savings for VISA_CLEARING /
/// BANK_SETTLEMENT.
const INTERAC_CUSTOMER_EMAIL: &str = "interac@nano.bank";

#[derive(Clone, Copy, Debug)]
pub struct InteracAccounts {
    pub clearing_id: Uuid,
    pub settlement_id: Uuid,
}

/// The Interac rail. Carries the resolved clearing/settlement ids (re-resolved
/// per request by the handler, because a data wipe rebuilds them).
#[derive(Clone, Copy, Debug)]
pub struct InteracRail {
    pub accounts: InteracAccounts,
}

impl InteracRail {
    pub fn new(accounts: InteracAccounts) -> Self {
        Self { accounts }
    }
    pub fn id(&self) -> RailId {
        RailId::Interac
    }
}

/// Normalise a handle for storage/lookup: emails lowercased+trimmed; phones
/// reduced to a leading '+' (if present) and digits.
pub fn normalize_handle(handle_type: HandleType, raw: &str) -> String {
    match handle_type {
        HandleType::Email => raw.trim().to_lowercase(),
        HandleType::Phone => {
            let mut out = String::new();
            for (i, c) in raw.trim().chars().enumerate() {
                if c == '+' && i == 0 {
                    out.push('+');
                } else if c.is_ascii_digit() {
                    out.push(c);
                }
            }
            out
        }
    }
}

/// Create Interac's system customer + two GL accounts if absent; return ids.
/// Idempotent — mirrors `handlers::cards::ensure_system_accounts`.
pub async fn ensure_interac_accounts(pool: &DatabasePool) -> Result<InteracAccounts, sqlx::Error> {
    let (clearing_id, settlement_id) = common::ensure_rail_accounts(
        pool,
        INTERAC_CUSTOMER_EMAIL,
        "+10000000002",
        "Interac",
        "000000002",
        "Interac",
    )
    .await?;
    Ok(InteracAccounts {
        clearing_id,
        settlement_id,
    })
}

impl InteracRail {
    fn ctx(&self) -> RailCtx {
        RailCtx {
            id: RailId::Interac,
            clearing_id: self.accounts.clearing_id,
            settlement_id: self.accounts.settlement_id,
        }
    }
}

// GL note: hold/release/refund all post Payable→Payable — money staying inside
// the bank's obligation accounts — and only `accept_inbound` moves `Receivable`.
// So funds sent to an EXTERNAL bank never debit `Bank` at settle time; the GL
// carries an un-swept `Receivable`/`Payable` position until the ACSS-style
// INTERAC_SETTLEMENT→Bank settlement sweep lands (deferred to the AFT rail).
// The clearing/settlement double-entry + aggregate GL post live in `rails::common`.
#[async_trait]
impl Rail for InteracRail {
    fn id(&self) -> RailId {
        RailId::Interac
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_handles_are_lowercased_and_trimmed() {
        assert_eq!(
            normalize_handle(HandleType::Email, "  Alice@Example.COM "),
            "alice@example.com"
        );
    }

    #[test]
    fn phone_handles_keep_only_digits_and_plus() {
        assert_eq!(
            normalize_handle(HandleType::Phone, "+1 (416) 555-0199"),
            "+14165550199"
        );
    }
}
