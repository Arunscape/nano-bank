//! Credit-card payment rails (issuer side).
//!
//! nano-bank plays the **card issuer**. A mock "Visa network" (see
//! `testing/visa/`) drives these endpoints on behalf of merchants:
//!
//!   authorize → capture → settle (batch)
//!
//! - **authorize** places a hold on the card's available credit
//!   (`account_holds`), declining if it would exceed the limit. No money moves.
//! - **capture** posts a balanced double-entry transaction: the cardholder's
//!   card account is *credited* (balance owed goes up) and the internal
//!   `VISA_CLEARING` GL account is *debited*. The hold is released.
//! - **settle** runs the clearing batch: it zeroes `VISA_CLEARING` against the
//!   bank's `BANK_SETTLEMENT` funding account in one transaction and tags the
//!   captures it covered as settled.
//!
//! ## Working with the schema triggers (`06_triggers.sql`)
//! - `trigger_generate_account_number` overwrites `account_number` on every
//!   insert, so GL accounts can't be found by a sentinel number. They're keyed
//!   instead by `(system customer, account_type)` and their ids are cached in
//!   [`AppState`] at startup.
//! - `trigger_update_account_balance` maintains `accounts.balance` and fills in
//!   `balance_before/after` itself, so we never update balances by hand.
//! - `trigger_validate_transaction_balance` rejects any unbalanced transaction,
//!   so both legs are inserted in a single multi-row statement.
//! The GL accounts carry a large overdraft so their balances may run negative.

use axum::{extract::State, http::StatusCode, response::Json, routing::post, Router};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::config::database::DatabasePool;
use crate::errors::AppError;
use crate::handlers::AppState;
use crate::middleware::auth::AuthenticatedService;
use crate::models::account::{Account, AccountStatus, AccountType};
// The ledger port's account role (aliased to avoid clashing with the model's Account).
use crate::ledger::{Account as GlAccount, Direction, EntryLine, NewEntry, PostedEntry};

const SYSTEM_CUSTOMER_EMAIL: &str = "system@nano.bank";
/// GL accounts are distinguished by account_type under the system customer.
const CLEARING_TYPE: &str = "chequing"; // VISA_CLEARING
const SETTLEMENT_TYPE: &str = "savings"; // BANK_SETTLEMENT

/// Resolved ids of the internal general-ledger accounts, cached at startup.
#[derive(Clone, Copy, Debug)]
pub struct SystemAccounts {
    pub visa_clearing_id: Uuid,
    pub bank_settlement_id: Uuid,
}

pub fn card_routes() -> Router<AppState> {
    Router::new()
        .route("/authorize", post(authorize))
        .route("/capture", post(capture))
        .route("/settle", post(settle))
}

// ---------------------------------------------------------------------------
// System / GL account bootstrap
// ---------------------------------------------------------------------------

/// Create the synthetic system customer and the two internal GL accounts if
/// they don't already exist, and return their ids. Idempotent.
pub async fn ensure_system_accounts(pool: &DatabasePool) -> Result<SystemAccounts, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO customers (email, phone_number, first_name, last_name, date_of_birth, sin)
        VALUES ($1, '+10000000000', 'Nano', 'System', '1970-01-01', '000000000')
        ON CONFLICT (email) DO NOTHING
        "#,
    )
    .bind(SYSTEM_CUSTOMER_EMAIL)
    .execute(pool)
    .await?;

    let system_customer_id: Uuid =
        sqlx::query_scalar("SELECT customer_id FROM customers WHERE email = $1")
            .bind(SYSTEM_CUSTOMER_EMAIL)
            .fetch_one(pool)
            .await?;

    let visa_clearing_id = ensure_gl_account(pool, system_customer_id, CLEARING_TYPE).await?;
    let bank_settlement_id = ensure_gl_account(pool, system_customer_id, SETTLEMENT_TYPE).await?;

    tracing::info!(%visa_clearing_id, %bank_settlement_id, "✅ system GL accounts ready");
    Ok(SystemAccounts {
        visa_clearing_id,
        bank_settlement_id,
    })
}

/// Ensure exactly one GL account of `account_type` exists for the system
/// customer (large overdraft so its balance may go negative), return its id.
async fn ensure_gl_account(
    pool: &DatabasePool,
    system_customer_id: Uuid,
    account_type: &str,
) -> Result<Uuid, sqlx::Error> {
    // account_number is overwritten by a trigger; the literal is just a placeholder.
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
    .bind(system_customer_id)
    .bind(account_type)
    .execute(pool)
    .await?;

    sqlx::query_scalar(
        "SELECT account_id FROM accounts WHERE customer_id = $1 AND account_type = $2::account_type \
         ORDER BY created_at LIMIT 1",
    )
    .bind(system_customer_id)
    .bind(account_type)
    .fetch_one(pool)
    .await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A reference number matching `^[A-Z0-9]{10,20}$`: `prefix` + 12 digits.
pub(crate) fn reference_number(prefix: &str) -> String {
    let n = (Uuid::new_v4().as_u128() % 1_000_000_000_000) as u64;
    format!("{}{:012}", prefix, n)
}

/// Round to 2 dp (the schema rejects anything else) and reject non-positive.
pub(crate) fn normalize_amount(amount: Decimal) -> Result<Decimal, AppError> {
    let amount = amount.round_dp(2);
    if amount <= Decimal::ZERO {
        return Err(AppError::BadRequest("amount must be positive".to_string()));
    }
    Ok(amount)
}

/// Post a balanced two-line entry to the general ledger of record (the swappable
/// core) via the Ledger port. The per-card subledger is kept locally; this is the
/// aggregate GL effect. A core failure is surfaced so the caller can fail the
/// operation rather than let the GL drift.
pub(crate) async fn post_gl_entry(
    state: &AppState,
    reference: &str,
    description: &str,
    debit: GlAccount,
    credit: GlAccount,
    amount: Decimal,
) -> Result<PostedEntry, AppError> {
    state
        .ledger
        .post_entry(NewEntry {
            reference: Some(reference.to_string()),
            description: Some(description.to_string()),
            lines: vec![
                EntryLine {
                    account: debit,
                    direction: Direction::Debit,
                    amount,
                },
                EntryLine {
                    account: credit,
                    direction: Direction::Credit,
                    amount,
                },
            ],
        })
        .await
        .map_err(|e| AppError::ServiceUnavailable(format!("GL core post failed: {e}")))
}

// ---------------------------------------------------------------------------
// authorize
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AuthorizeRequest {
    account_id: Uuid,
    amount: Decimal,
    merchant: Option<String>,
}

#[derive(Debug, Serialize)]
struct AuthorizeResponse {
    status: &'static str, // "approved" | "declined"
    auth_id: Option<Uuid>,
    account_id: Uuid,
    amount: Decimal,
    merchant: String,
    available_balance: Decimal,
    reason: Option<String>,
}

/// Authorize a card purchase: validate the card and place a hold on its
/// available credit. Returns 201 approved (with an `auth_id`) or 200 declined.
async fn authorize(
    State(state): State<AppState>,
    _network: AuthenticatedService,
    Json(req): Json<AuthorizeRequest>,
) -> Result<(StatusCode, Json<AuthorizeResponse>), AppError> {
    let amount = normalize_amount(req.amount)?;
    let merchant = req
        .merchant
        .unwrap_or_else(|| "Unknown Merchant".to_string());

    let mut tx = state.pool.begin().await?;

    // The network identifies the card; the issuer validates it. There is no
    // per-cardholder ownership check here — the network legitimately authorizes
    // against any card. `card.customer_id` is still resolved for the audit trail
    // and as fraud-scoring context below.
    let card = fetch_account_for_update(&mut tx, req.account_id)
        .await?
        .ok_or_else(|| AppError::NotFound("account not found".to_string()))?;

    if !matches!(card.account_type, AccountType::CreditCard) {
        return Err(AppError::BadRequest(
            "account is not a credit card".to_string(),
        ));
    }
    if !matches!(card.status, AccountStatus::Active) {
        return Err(AppError::InvalidAccountStatus);
    }

    // TODO(fraud): risk_assess(&card, amount, &merchant, recent velocity, device/geo)
    //   -> Approve | Decline { reason }. The issuer's fraud engine scores the
    //   authorization here, between card validation and the approve/decline
    //   branch, and may decline (write suspicious_activities / rule_violations).
    //   AuthorizeResponse.reason already carries a decline reason, so a future
    //   fraud decline needs no shape change.

    // available_balance already nets out balance and active holds.
    if amount > card.available_balance {
        tx.rollback().await?;
        return Ok((
            StatusCode::OK,
            Json(AuthorizeResponse {
                status: "declined",
                auth_id: None,
                account_id: card.account_id,
                amount,
                merchant,
                available_balance: card.available_balance,
                reason: Some("insufficient_credit".to_string()),
            }),
        ));
    }

    let auth_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO account_holds (account_id, amount, reason, reference_id, expires_at)
        VALUES ($1, $2, $3, $4, CURRENT_TIMESTAMP + interval '7 days')
        RETURNING hold_id
        "#,
    )
    .bind(card.account_id)
    .bind(amount)
    .bind(format!("visa_auth:{}", merchant))
    .bind(reference_number("AUTH"))
    .fetch_one(&mut *tx)
    .await?;

    let available = recompute_card_available(&mut tx, card.account_id).await?;
    tx.commit().await?;

    tracing::info!(
        account_id = %card.account_id, %auth_id, amount = %amount, merchant = %merchant,
        "💳 authorization approved"
    );

    Ok((
        StatusCode::CREATED,
        Json(AuthorizeResponse {
            status: "approved",
            auth_id: Some(auth_id),
            account_id: card.account_id,
            amount,
            merchant,
            available_balance: available,
            reason: None,
        }),
    ))
}

// ---------------------------------------------------------------------------
// capture
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CaptureRequest {
    auth_id: Uuid,
}

#[derive(Debug, Serialize)]
struct CaptureResponse {
    transaction_id: Uuid,
    reference_number: String,
    account_id: Uuid,
    amount: Decimal,
    available_balance: Decimal,
    status: &'static str,
}

/// Capture a previously approved authorization: post the double-entry charge
/// and release the hold.
async fn capture(
    State(state): State<AppState>,
    _network: AuthenticatedService,
    Json(req): Json<CaptureRequest>,
) -> Result<(StatusCode, Json<CaptureResponse>), AppError> {
    let mut tx = state.pool.begin().await?;

    // The hold doubles as the authorization. Must still be open (not captured).
    let hold = sqlx::query_as::<_, (Uuid, Decimal, String)>(
        r#"
        SELECT account_id, amount, reason
        FROM account_holds
        WHERE hold_id = $1 AND released_at IS NULL
        FOR UPDATE
        "#,
    )
    .bind(req.auth_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("authorization not found or already captured".to_string()))?;

    let (card_id, amount, reason) = hold;
    let merchant = reason
        .strip_prefix("visa_auth:")
        .unwrap_or(&reason)
        .to_string();

    let system = ensure_system_accounts(&state.pool).await?;

    // Lock both legs (card first, then clearing). `card.customer_id` is used for
    // the transaction's `initiated_by` (audit) below — no ownership gate, since
    // the network drives the capture.
    let card = fetch_account_for_update(&mut tx, card_id)
        .await?
        .ok_or_else(|| AppError::NotFound("card account not found".to_string()))?;
    let _clearing = fetch_account_for_update(&mut tx, system.visa_clearing_id).await?;

    let reference = reference_number("VISA");
    let txn_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO transactions
            (reference_number, transaction_type, amount, description, status,
             initiated_by, completed_at, metadata)
        VALUES ($1, 'card_purchase', $2, $3, 'completed', $4, CURRENT_TIMESTAMP, $5)
        RETURNING transaction_id
        "#,
    )
    .bind(&reference)
    .bind(amount)
    .bind(format!("Card purchase — {}", merchant))
    .bind(card.customer_id)
    .bind(json!({ "merchant": merchant, "auth_id": req.auth_id, "settled": false }))
    .fetch_one(&mut *tx)
    .await?;

    // Both legs in one statement so the balance-validation trigger sees them
    // balanced. The balance-update trigger maintains accounts.balance and the
    // entries' balance_before/after, so we pass 0 placeholders for those.
    // Card is *credited* (debt up); VISA_CLEARING is *debited*.
    post_two_legged(
        &mut tx,
        txn_id,
        card.account_id,
        "credit",
        system.visa_clearing_id,
        "debit",
        amount,
    )
    .await?;

    sqlx::query("UPDATE account_holds SET released_at = CURRENT_TIMESTAMP WHERE hold_id = $1")
        .bind(req.auth_id)
        .execute(&mut *tx)
        .await?;
    let available = recompute_card_available(&mut tx, card.account_id).await?;

    // The core is the general ledger of record: post the aggregate GL effect of
    // the purchase (cardholder receivable up, network clearing payable up). The
    // per-card subledger above stays local. Done before commit so a core failure
    // fails the capture rather than letting the GL drift.
    let gl = post_gl_entry(
        &state,
        &reference,
        &format!("Card purchase — {}", merchant),
        GlAccount::Receivable,
        GlAccount::Payable,
        amount,
    )
    .await?;
    sqlx::query(
        "UPDATE transactions SET metadata = jsonb_set(COALESCE(metadata, '{}'::jsonb), \
         '{gl_entry}', to_jsonb($2::text)) WHERE transaction_id = $1",
    )
    .bind(txn_id)
    .bind(format!("{}:{}", gl.backend, gl.id))
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    tracing::info!(
        account_id = %card.account_id, transaction_id = %txn_id, amount = %amount,
        merchant = %merchant, gl_entry = %gl.id, "💳 capture posted"
    );

    Ok((
        StatusCode::CREATED,
        Json(CaptureResponse {
            transaction_id: txn_id,
            reference_number: reference,
            account_id: card.account_id,
            amount,
            available_balance: available,
            status: "completed",
        }),
    ))
}

// ---------------------------------------------------------------------------
// settle (clearing batch)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct SettleResponse {
    settled_transactions: i64,
    net_amount: Decimal,
    transaction_id: Option<Uuid>,
    reference_number: Option<String>,
    status: &'static str,
}

/// Run the settlement batch: move the outstanding `VISA_CLEARING` balance to
/// `BANK_SETTLEMENT` in one transaction and mark covered captures as settled.
async fn settle(
    State(state): State<AppState>,
    // Settlement runs on the network/service plane for now. In reality clearing
    // & settlement is the bank's own operations function — this should move to a
    // separate ops/admin credential in a later pass.
    _network: AuthenticatedService,
) -> Result<(StatusCode, Json<SettleResponse>), AppError> {
    let system = ensure_system_accounts(&state.pool).await?;

    let mut tx = state.pool.begin().await?;

    // Lock clearing first, then the bank funding account.
    let clearing = fetch_account_for_update(&mut tx, system.visa_clearing_id)
        .await?
        .ok_or_else(|| AppError::Internal("VISA_CLEARING account missing".to_string()))?;
    let _bank = fetch_account_for_update(&mut tx, system.bank_settlement_id).await?;

    // Clearing carries a negative balance (the issuer's obligation to the
    // network); the amount to settle is its magnitude.
    let net = -clearing.balance;
    if net <= Decimal::ZERO {
        tx.commit().await?;
        return Ok((
            StatusCode::OK,
            Json(SettleResponse {
                settled_transactions: 0,
                net_amount: Decimal::ZERO,
                transaction_id: None,
                reference_number: None,
                status: "nothing_to_settle",
            }),
        ));
    }

    let system_customer_id: Uuid =
        sqlx::query_scalar("SELECT customer_id FROM customers WHERE email = $1")
            .bind(SYSTEM_CUSTOMER_EMAIL)
            .fetch_one(&mut *tx)
            .await?;

    let reference = reference_number("STL");
    let txn_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO transactions
            (reference_number, transaction_type, amount, description, status,
             initiated_by, completed_at, metadata)
        VALUES ($1, 'card_settlement', $2, 'Visa settlement batch', 'completed', $3, CURRENT_TIMESTAMP, $4)
        RETURNING transaction_id
        "#,
    )
    .bind(&reference)
    .bind(net)
    .bind(system_customer_id)
    .bind(json!({ "kind": "settlement" }))
    .fetch_one(&mut *tx)
    .await?;

    // Clearing *credited* back toward zero; BANK_SETTLEMENT *debited* (paid out).
    post_two_legged(
        &mut tx,
        txn_id,
        system.visa_clearing_id,
        "credit",
        system.bank_settlement_id,
        "debit",
        net,
    )
    .await?;

    // Tag the captures this batch covers (all so-far-unsettled purchases).
    let settled_transactions: i64 = sqlx::query_scalar(
        r#"
        WITH updated AS (
            UPDATE transactions
            SET metadata = jsonb_set(COALESCE(metadata, '{}'::jsonb), '{settled}', 'true')
            WHERE transaction_type = 'card_purchase'
              AND (metadata->>'settled') IS DISTINCT FROM 'true'
            RETURNING 1
        )
        SELECT count(*) FROM updated
        "#,
    )
    .fetch_one(&mut *tx)
    .await?;

    // Mirror the settlement to the general ledger of record: pay down the network
    // payable from the bank account.
    let gl = post_gl_entry(
        &state,
        &reference,
        "Visa settlement batch",
        GlAccount::Payable,
        GlAccount::Bank,
        net,
    )
    .await?;
    sqlx::query(
        "UPDATE transactions SET metadata = jsonb_set(COALESCE(metadata, '{}'::jsonb), \
         '{gl_entry}', to_jsonb($2::text)) WHERE transaction_id = $1",
    )
    .bind(txn_id)
    .bind(format!("{}:{}", gl.backend, gl.id))
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    tracing::info!(
        transaction_id = %txn_id, net_amount = %net, settled_transactions,
        gl_entry = %gl.id, "🏦 settlement batch posted"
    );

    Ok((
        StatusCode::CREATED,
        Json(SettleResponse {
            settled_transactions,
            net_amount: net,
            transaction_id: Some(txn_id),
            reference_number: Some(reference),
            status: "settled",
        }),
    ))
}

// ---------------------------------------------------------------------------
// Low-level DB helpers
// ---------------------------------------------------------------------------

pub(crate) type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

const ACCOUNT_COLUMNS: &str = "account_id, customer_id, account_number, account_type, currency, \
    balance, available_balance, status, interest_rate, overdraft_limit, minimum_balance, \
    created_at, updated_at, activated_at, closed_at";

pub(crate) async fn fetch_account_for_update(
    tx: &mut Tx<'_>,
    account_id: Uuid,
) -> Result<Option<Account>, sqlx::Error> {
    sqlx::query_as::<_, Account>(&format!(
        "SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE account_id = $1 FOR UPDATE"
    ))
    .bind(account_id)
    .fetch_optional(&mut **tx)
    .await
}

/// Insert both legs of a transaction in one statement, so the balance triggers
/// fire with a balanced set. The BEFORE-INSERT trigger fills in balance_before/
/// after and updates each account's balance; we pass 0 placeholders.
pub(crate) async fn post_two_legged(
    tx: &mut Tx<'_>,
    transaction_id: Uuid,
    account_a: Uuid,
    type_a: &str,
    account_b: Uuid,
    type_b: &str,
    amount: Decimal,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO transaction_entries
            (transaction_id, account_id, entry_type, amount, balance_before, balance_after, entry_order)
        VALUES
            ($1, $2, $3::entry_type, $6, 0, 0, 1),
            ($1, $4, $5::entry_type, $6, 0, 0, 2)
        "#,
    )
    .bind(transaction_id)
    .bind(account_a)
    .bind(type_a)
    .bind(account_b)
    .bind(type_b)
    .bind(amount)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Recompute a credit card's available credit: limit − balance − open holds.
async fn recompute_card_available(
    tx: &mut Tx<'_>,
    account_id: Uuid,
) -> Result<Decimal, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        UPDATE accounts
        SET available_balance = overdraft_limit - balance
            - COALESCE((SELECT sum(amount) FROM account_holds
                        WHERE account_id = $1 AND released_at IS NULL), 0),
            updated_at = CURRENT_TIMESTAMP
        WHERE account_id = $1
        RETURNING available_balance
        "#,
    )
    .bind(account_id)
    .fetch_one(&mut **tx)
    .await
}
