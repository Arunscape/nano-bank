//! Shared rail plumbing. Every rail's `hold`/`release`/`refund`/`accept_inbound`
//! performs the *same* clearing/settlement double-entry plus aggregate GL post,
//! differing only in identity (the reference prefix, the `transactions` type
//! tag, and which clearing/settlement accounts to use). Those differences are
//! derived from `RailId`, so each concrete rail (`InteracRail`, `AftRail`,
//! `LynxRail`) is a thin delegation to the verbs here.
//!
//! Also home to the GL-account bootstrap helper and the `available_balance`
//! recompute helpers the rail handlers share.

use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

use crate::config::database::DatabasePool;
use crate::errors::AppError;
use crate::handlers::cards::{post_gl_entry, post_two_legged, reference_number};
use crate::handlers::AppState;
use crate::ledger::Account as GlAccount;

use super::{Destination, Hold, PgTx, RailId, RailPosting};

impl RailId {
    /// Reference-number prefix root for this rail's ledger postings (the op
    /// letter H/R/X/I is appended per verb).
    fn ref_root(self) -> &'static str {
        match self {
            RailId::Interac => "ETR",
            RailId::Aft => "AFT",
            RailId::Lynx => "LYX",
        }
    }
}

/// A rail's identity + resolved clearing/settlement accounts — everything the
/// shared verbs need to post.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RailCtx {
    pub id: RailId,
    pub clearing_id: Uuid,
    pub settlement_id: Uuid,
}

/// Create a completed `transactions` row for one rail movement; return its id.
/// `op` is the verb ("hold"/"release"/"refund"/"inbound"); the stored
/// `transaction_type` is `"<rail>_<op>"` and the metadata is tagged with the rail.
/// `pub(crate)` so a rail with bespoke GL (Lynx) can reuse it while keeping its
/// own verb bodies.
pub(crate) async fn new_txn(
    tx: &mut PgTx<'_>,
    ctx: RailCtx,
    op: &str,
    reference: &str,
    amount: Decimal,
    description: &str,
) -> Result<Uuid, AppError> {
    let id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO transactions
            (reference_number, transaction_type, amount, description, status,
             initiated_by, completed_at, metadata)
        VALUES ($1, $2, $3, $4, 'completed', NULL, CURRENT_TIMESTAMP, $5)
        RETURNING transaction_id
        "#,
    )
    .bind(reference)
    .bind(format!("{}_{}", ctx.id.as_str(), op))
    .bind(amount)
    .bind(description)
    .bind(json!({ "rail": ctx.id.as_str() }))
    .fetch_one(&mut **tx)
    .await?;
    Ok(id)
}

pub(crate) async fn tag_gl(tx: &mut PgTx<'_>, txn_id: Uuid, gl: &str) -> Result<(), AppError> {
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

/// Reserve `amount` from `from` into the rail's clearing account.
/// Local: Dr `from` / Cr CLEARING. GL: Dr Payable / Cr Payable.
pub(crate) async fn hold(
    ctx: RailCtx,
    state: &AppState,
    tx: &mut PgTx<'_>,
    from: Uuid,
    amount: Decimal,
    description: &str,
) -> Result<Hold, AppError> {
    let reference = reference_number(&format!("{}H", ctx.id.ref_root()));
    let txn_id = new_txn(tx, ctx, "hold", &reference, amount, description).await?;
    post_two_legged(tx, txn_id, from, "debit", ctx.clearing_id, "credit", amount).await?;
    let gl = post_gl_entry(state, &reference, description, GlAccount::Payable, GlAccount::Payable, amount).await?;
    tag_gl(tx, txn_id, &format!("{}:{}", gl.backend, gl.id)).await?;
    Ok(Hold { from_account: from, amount, reference, transaction_id: txn_id })
}

/// Release a hold to its destination.
/// Internal: Dr CLEARING / Cr account. External: Dr CLEARING / Cr SETTLEMENT.
pub(crate) async fn release(
    ctx: RailCtx,
    state: &AppState,
    tx: &mut PgTx<'_>,
    hold: &Hold,
    dest: Destination,
    description: &str,
) -> Result<RailPosting, AppError> {
    let reference = reference_number(&format!("{}R", ctx.id.ref_root()));
    let txn_id = new_txn(tx, ctx, "release", &reference, hold.amount, description).await?;
    let credit_account = match dest {
        Destination::Internal(acct) => acct,
        Destination::External(_) => ctx.settlement_id,
    };
    post_two_legged(tx, txn_id, ctx.clearing_id, "debit", credit_account, "credit", hold.amount).await?;
    let gl = post_gl_entry(state, &reference, description, GlAccount::Payable, GlAccount::Payable, hold.amount).await?;
    let gl_entry = format!("{}:{}", gl.backend, gl.id);
    tag_gl(tx, txn_id, &gl_entry).await?;
    Ok(RailPosting { transaction_id: txn_id, gl_entry: Some(gl_entry) })
}

/// Return a hold to its origin (Dr CLEARING / Cr `hold.from_account`).
pub(crate) async fn refund(
    ctx: RailCtx,
    state: &AppState,
    tx: &mut PgTx<'_>,
    hold: &Hold,
    description: &str,
) -> Result<RailPosting, AppError> {
    let reference = reference_number(&format!("{}X", ctx.id.ref_root()));
    let txn_id = new_txn(tx, ctx, "refund", &reference, hold.amount, description).await?;
    post_two_legged(tx, txn_id, ctx.clearing_id, "debit", hold.from_account, "credit", hold.amount).await?;
    let gl = post_gl_entry(state, &reference, description, GlAccount::Payable, GlAccount::Payable, hold.amount).await?;
    let gl_entry = format!("{}:{}", gl.backend, gl.id);
    tag_gl(tx, txn_id, &gl_entry).await?;
    Ok(RailPosting { transaction_id: txn_id, gl_entry: Some(gl_entry) })
}

/// Credit an incoming payment straight to a customer account (autodeposit fast
/// path). Local: Dr SETTLEMENT / Cr `to`. GL: Dr Receivable / Cr Payable.
pub(crate) async fn accept_inbound(
    ctx: RailCtx,
    state: &AppState,
    tx: &mut PgTx<'_>,
    to: Uuid,
    amount: Decimal,
    description: &str,
) -> Result<RailPosting, AppError> {
    let reference = reference_number(&format!("{}I", ctx.id.ref_root()));
    let txn_id = new_txn(tx, ctx, "inbound", &reference, amount, description).await?;
    post_two_legged(tx, txn_id, ctx.settlement_id, "debit", to, "credit", amount).await?;
    let gl = post_gl_entry(state, &reference, description, GlAccount::Receivable, GlAccount::Payable, amount).await?;
    let gl_entry = format!("{}:{}", gl.backend, gl.id);
    tag_gl(tx, txn_id, &gl_entry).await?;
    Ok(RailPosting { transaction_id: txn_id, gl_entry: Some(gl_entry) })
}

/// Create a rail's synthetic system customer (if absent) plus its two GL
/// accounts (chequing = CLEARING, savings = SETTLEMENT). Idempotent — mirrors
/// `handlers::cards::ensure_system_accounts`. Returns `(clearing_id, settlement_id)`.
pub(crate) async fn ensure_rail_accounts(
    pool: &DatabasePool,
    email: &str,
    phone: &str,
    last_name: &str,
    sin: &str,
    label: &str,
) -> Result<(Uuid, Uuid), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO customers (email, phone_number, first_name, last_name, date_of_birth, sin)
        VALUES ($1, $2, 'Nano', $3, '1970-01-01', $4)
        ON CONFLICT (email) DO NOTHING
        "#,
    )
    .bind(email)
    .bind(phone)
    .bind(last_name)
    .bind(sin)
    .execute(pool)
    .await?;

    let customer_id: Uuid = sqlx::query_scalar("SELECT customer_id FROM customers WHERE email = $1")
        .bind(email)
        .fetch_one(pool)
        .await?;

    let clearing_id = ensure_gl_account(pool, customer_id, "chequing").await?;
    let settlement_id = ensure_gl_account(pool, customer_id, "savings").await?;
    tracing::info!(%clearing_id, %settlement_id, "✅ {label} GL accounts ready");
    Ok((clearing_id, settlement_id))
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

/// Zero a customer account's `available_balance` ahead of a debit leg so the
/// balance trigger can't transiently violate `chk_available_balance_logical`.
/// NEVER call on a rail's own clearing/settlement account (they float at 0).
pub(crate) async fn zero_available(tx: &mut PgTx<'_>, account_id: Uuid) -> Result<(), AppError> {
    sqlx::query("UPDATE accounts SET available_balance = 0 WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Recompute a customer account's available balance:
/// `balance + overdraft − open holds`.
pub(crate) async fn recompute_available(tx: &mut PgTx<'_>, account_id: Uuid) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE accounts SET available_balance = balance + overdraft_limit \
         - COALESCE((SELECT sum(amount) FROM account_holds \
                     WHERE account_id=$1 AND released_at IS NULL), 0), \
         updated_at = CURRENT_TIMESTAMP WHERE account_id = $1",
    )
    .bind(account_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
