//! Interac e-Transfer rail: the clearing/settlement plumbing. The product
//! lifecycle lives in `handlers/interac.rs`.

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

use crate::config::database::DatabasePool;
use crate::errors::AppError;
use crate::handlers::AppState;
use crate::handlers::cards::{post_gl_entry, post_two_legged, reference_number};
use crate::ledger::Account as GlAccount;
use crate::models::interac::HandleType;

use super::{Destination, Hold, PgTx, Rail, RailId, RailPosting};

/// Interac's own synthetic system customer — SEPARATE from the card rails'
/// `system@nano.bank`, because GL accounts are keyed by (customer, account_type)
/// and that customer already uses its chequing/savings for VISA_CLEARING /
/// BANK_SETTLEMENT.
const INTERAC_CUSTOMER_EMAIL: &str = "interac@nano.bank";
const CLEARING_TYPE: &str = "chequing"; // INTERAC_CLEARING
const SETTLEMENT_TYPE: &str = "savings"; // INTERAC_SETTLEMENT

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
pub async fn ensure_interac_accounts(
    pool: &DatabasePool,
) -> Result<InteracAccounts, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO customers (email, phone_number, first_name, last_name, date_of_birth, sin)
        VALUES ($1, '+10000000002', 'Nano', 'Interac', '1970-01-01', '000000002')
        ON CONFLICT (email) DO NOTHING
        "#,
    )
    .bind(INTERAC_CUSTOMER_EMAIL)
    .execute(pool)
    .await?;

    let customer_id: Uuid =
        sqlx::query_scalar("SELECT customer_id FROM customers WHERE email = $1")
            .bind(INTERAC_CUSTOMER_EMAIL)
            .fetch_one(pool)
            .await?;

    let clearing_id = ensure_gl_account(pool, customer_id, CLEARING_TYPE).await?;
    let settlement_id = ensure_gl_account(pool, customer_id, SETTLEMENT_TYPE).await?;
    tracing::info!(%clearing_id, %settlement_id, "✅ Interac GL accounts ready");
    Ok(InteracAccounts { clearing_id, settlement_id })
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
    .bind(json!({ "rail": "interac" }))
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

// GL note: hold/release/refund all post Payable→Payable — money staying inside
// the bank's obligation accounts — and only `accept_inbound` moves `Receivable`.
// So funds sent to an EXTERNAL bank never debit `Bank` at settle time; the GL
// carries an un-swept `Receivable`/`Payable` position until the ACSS-style
// INTERAC_SETTLEMENT→Bank settlement sweep lands (deferred to the AFT rail).
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
        let reference = reference_number("ETRH");
        let txn_id = new_txn(tx, &reference, "interac_hold", amount, description, None).await?;
        // Dr from / Cr INTERAC_CLEARING (holds the funds).
        post_two_legged(tx, txn_id, from, "debit", self.accounts.clearing_id, "credit", amount).await?;
        // Aggregate GL: owed-to-customer → owed-to-clearing (Payable/Payable).
        let gl = post_gl_entry(state, &reference, description, GlAccount::Payable, GlAccount::Payable, amount).await?;
        tag_gl(tx, txn_id, &format!("{}:{}", gl.backend, gl.id)).await?;
        Ok(Hold { from_account: from, amount, reference, transaction_id: txn_id })
    }

    async fn release(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        hold: &Hold,
        dest: Destination,
        description: &str,
    ) -> Result<RailPosting, AppError> {
        let reference = reference_number("ETRR");
        let txn_id = new_txn(tx, &reference, "interac_release", hold.amount, description, None).await?;
        let credit_account = match dest {
            Destination::Internal(acct) => acct,
            Destination::External(_) => self.accounts.settlement_id,
        };
        // Dr INTERAC_CLEARING / Cr destination (recipient or settlement).
        post_two_legged(tx, txn_id, self.accounts.clearing_id, "debit", credit_account, "credit", hold.amount).await?;
        let gl = post_gl_entry(state, &reference, description, GlAccount::Payable, GlAccount::Payable, hold.amount).await?;
        tag_gl(tx, txn_id, &format!("{}:{}", gl.backend, gl.id)).await?;
        Ok(RailPosting { transaction_id: txn_id, gl_entry: Some(format!("{}:{}", gl.backend, gl.id)) })
    }

    async fn refund(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        hold: &Hold,
        description: &str,
    ) -> Result<RailPosting, AppError> {
        let reference = reference_number("ETRX");
        let txn_id = new_txn(tx, &reference, "interac_refund", hold.amount, description, None).await?;
        // Dr INTERAC_CLEARING / Cr origin (sender for outbound; settlement for inbound).
        post_two_legged(tx, txn_id, self.accounts.clearing_id, "debit", hold.from_account, "credit", hold.amount).await?;
        let gl = post_gl_entry(state, &reference, description, GlAccount::Payable, GlAccount::Payable, hold.amount).await?;
        tag_gl(tx, txn_id, &format!("{}:{}", gl.backend, gl.id)).await?;
        Ok(RailPosting { transaction_id: txn_id, gl_entry: Some(format!("{}:{}", gl.backend, gl.id)) })
    }

    async fn accept_inbound(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        to: Uuid,
        amount: Decimal,
        description: &str,
    ) -> Result<RailPosting, AppError> {
        let reference = reference_number("ETRI");
        let txn_id = new_txn(tx, &reference, "interac_inbound", amount, description, None).await?;
        // Dr INTERAC_SETTLEMENT / Cr recipient (network → customer).
        post_two_legged(tx, txn_id, self.accounts.settlement_id, "debit", to, "credit", amount).await?;
        // GL: network owes us (Receivable) → customer payable.
        let gl = post_gl_entry(state, &reference, description, GlAccount::Receivable, GlAccount::Payable, amount).await?;
        tag_gl(tx, txn_id, &format!("{}:{}", gl.backend, gl.id)).await?;
        Ok(RailPosting { transaction_id: txn_id, gl_entry: Some(format!("{}:{}", gl.backend, gl.id)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_handles_are_lowercased_and_trimmed() {
        assert_eq!(normalize_handle(HandleType::Email, "  Alice@Example.COM "), "alice@example.com");
    }

    #[test]
    fn phone_handles_keep_only_digits_and_plus() {
        assert_eq!(normalize_handle(HandleType::Phone, "+1 (416) 555-0199"), "+14165550199");
    }
}
