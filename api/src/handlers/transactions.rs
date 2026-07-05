//! Customer money movement: deposit, withdrawal, transfer, and history.
//!
//! These endpoints post **balanced double-entry** transactions to the local
//! subledger (the `transactions` / `transaction_entries` tables) exactly like
//! [`crate::handlers::cards`] — both legs inserted in one statement so
//! `trigger_validate_transaction_balance` sees them balanced, and the balance
//! triggers maintain `accounts.balance` / `balance_before` / `balance_after`.
//! We reuse the low-level posting helpers from `cards` rather than duplicate
//! them.
//!
//! ## Counterparty for deposit / withdrawal
//! A transfer moves value between two customer accounts, so it is naturally
//! two-legged. A deposit or withdrawal touches only one customer account, so it
//! needs a counterparty: the internal **`EXTERNAL_CASH`** account, a single
//! `chequing` account owned by a synthetic `cash@nano.bank` system customer
//! with a $1T overdraft. Its `available_balance` is left at 0 so a very
//! negative balance never trips `chk_available_balance_logical`.
//!
//! ## General ledger of record
//! Deposit and withdrawal post their aggregate effect to the swappable core via
//! the `Ledger` port (deposit: debit `Bank` / credit `Payable`; withdrawal the
//! reverse). A **transfer is not posted to the core**: both customer accounts
//! map to the same `Payable` GL role, so the aggregate effect nets to zero — a
//! transfer is an internal reclassification recorded only in the local
//! subledger.
//!
//! Only `chequing` / `savings` accounts are accepted here; `credit_card`
//! accounts belong to the card rails.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;
use validator::Validate;

use crate::config::database::DatabasePool;
use crate::errors::AppError;
use crate::handlers::cards::{
    fetch_account_for_update, normalize_amount, post_gl_entry, post_two_legged, reference_number,
    Tx,
};
use crate::handlers::AppState;
use crate::ledger::Account as GlAccount;
use crate::middleware::auth::AuthenticatedCustomer;
use crate::models::account::{Account, AccountStatus, AccountType};
use crate::models::transaction::{
    DepositRequest, MoneyTransferRequest, ReverseRequest, Transaction, TransactionEntry,
    TransactionEntryResponse, TransactionHistoryQuery, TransactionHistoryResponse,
    TransactionResponse, WithdrawalRequest,
};

const CASH_CUSTOMER_EMAIL: &str = "cash@nano.bank";
/// The external-cash counterparty is a chequing account under the cash customer.
const CASH_ACCOUNT_TYPE: &str = "chequing";

const DEFAULT_HISTORY_LIMIT: u32 = 50;

/// Flat fee charged on an outbound transfer (placeholder schedule).
fn transfer_fee() -> Decimal {
    Decimal::new(150, 2) // $1.50
}

pub fn transaction_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(get_transactions))
        .route("/transfer", post(transfer_money))
        .route("/deposit", post(deposit_money))
        .route("/withdrawal", post(withdraw_money))
        .route("/:id", get(get_transaction))
        .route("/:id/reverse", post(reverse_transaction))
}

// ---------------------------------------------------------------------------
// External-cash counterparty bootstrap
// ---------------------------------------------------------------------------

/// Ensure the synthetic cash customer and its `EXTERNAL_CASH` account exist and
/// return the account id. Idempotent; re-resolved per request so a data wipe
/// self-heals (mirrors `cards::ensure_system_accounts`).
async fn ensure_external_cash_account(pool: &DatabasePool) -> Result<Uuid, sqlx::Error> {
    // email is the stable identity (ON CONFLICT). The other UNIQUE columns are
    // chosen so they can't collide with real customers: a non-numeric phone
    // sentinel (the column has no format constraint) and a NULL sin (nullable).
    sqlx::query(
        r#"
        INSERT INTO customers (email, phone_number, first_name, last_name, date_of_birth, sin)
        VALUES ($1, 'nano-external-cash', 'Nano', 'Cash', '1970-01-01', NULL)
        ON CONFLICT (email) DO NOTHING
        "#,
    )
    .bind(CASH_CUSTOMER_EMAIL)
    .execute(pool)
    .await?;

    let cash_customer_id: Uuid =
        sqlx::query_scalar("SELECT customer_id FROM customers WHERE email = $1")
            .bind(CASH_CUSTOMER_EMAIL)
            .fetch_one(pool)
            .await?;

    // account_number is overwritten by a trigger; available_balance defaults to
    // 0 on purpose (a large-overdraft account whose balance may run negative).
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
    .bind(cash_customer_id)
    .bind(CASH_ACCOUNT_TYPE)
    .execute(pool)
    .await?;

    sqlx::query_scalar(
        "SELECT account_id FROM accounts WHERE customer_id = $1 AND account_type = $2::account_type \
         ORDER BY created_at LIMIT 1",
    )
    .bind(cash_customer_id)
    .bind(CASH_ACCOUNT_TYPE)
    .fetch_one(pool)
    .await
}

// ---------------------------------------------------------------------------
// deposit
// ---------------------------------------------------------------------------

/// Deposit external cash into a customer account: customer credited (balance
/// up), `EXTERNAL_CASH` debited. Posts debit `Bank` / credit `Payable` to the GL.
async fn deposit_money(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Json(req): Json<DepositRequest>,
) -> Result<(StatusCode, Json<TransactionResponse>), AppError> {
    req.validate()?;
    let amount = normalize_amount(req.amount)?;
    let cash_id = ensure_external_cash_account(&state.pool).await?;

    let mut tx = state.pool.begin().await?;

    let account = fetch_account_for_update(&mut tx, req.account_id)
        .await?
        .ok_or_else(|| AppError::NotFound("account not found".to_string()))?;
    // Ownership: derive the actor from the token, not the request. Don't reveal
    // another customer's account exists — 404, not 403 (mirrors accounts.rs).
    if account.customer_id != auth.customer_id {
        return Err(AppError::NotFound("account not found".to_string()));
    }
    ensure_operable(&account)?;

    // Lock the counterparty too (customer first, then cash) for a stable order.
    let _cash = fetch_account_for_update(&mut tx, cash_id).await?;

    let reference = reference_number("DEP");
    let txn_id = insert_transaction(
        &mut tx,
        &reference,
        "deposit",
        amount,
        &req.description,
        account.customer_id,
        req.external_reference.as_deref(),
        json!({}),
    )
    .await?;

    // customer *credit* (+balance); EXTERNAL_CASH *debit*.
    post_two_legged(
        &mut tx,
        txn_id,
        account.account_id,
        "credit",
        cash_id,
        "debit",
        amount,
    )
    .await?;

    let new_balance = account_balance(&mut tx, account.account_id).await?;
    recompute_available(&mut tx, account.account_id).await?;
    record_summary(&mut tx, account.account_id, "credit", amount, new_balance).await?;

    // GL of record: bank cash up, customer-deposit liability up.
    let gl = post_gl_entry(
        &state,
        &reference,
        &req.description,
        GlAccount::Bank,
        GlAccount::Payable,
        amount,
    )
    .await?;
    tag_gl_entry(&mut tx, txn_id, &format!("{}:{}", gl.backend, gl.id)).await?;

    tx.commit().await?;

    tracing::info!(account_id = %account.account_id, transaction_id = %txn_id, amount = %amount, "💰 deposit posted");
    let resp = load_transaction_response(&state.pool, txn_id).await?;
    Ok((StatusCode::CREATED, Json(resp)))
}

// ---------------------------------------------------------------------------
// withdrawal
// ---------------------------------------------------------------------------

/// Withdraw cash from a customer account: customer debited (balance down),
/// `EXTERNAL_CASH` credited. Enforces the daily withdrawal limit. Posts debit
/// `Payable` / credit `Bank` to the GL.
async fn withdraw_money(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Json(req): Json<WithdrawalRequest>,
) -> Result<(StatusCode, Json<TransactionResponse>), AppError> {
    req.validate()?;
    let amount = normalize_amount(req.amount)?;
    let cash_id = ensure_external_cash_account(&state.pool).await?;

    let mut tx = state.pool.begin().await?;

    let account = fetch_account_for_update(&mut tx, req.account_id)
        .await?
        .ok_or_else(|| AppError::NotFound("account not found".to_string()))?;
    // Ownership: only the account holder may withdraw. 404 to avoid leaking existence.
    if account.customer_id != auth.customer_id {
        return Err(AppError::NotFound("account not found".to_string()));
    }
    ensure_operable(&account)?;

    if account.available_balance < amount {
        return Err(AppError::InsufficientFunds);
    }

    // Daily withdrawal limit.
    let limits = ensure_and_reset_limits(&mut tx, account.account_id).await?;
    if limits.daily_withdrawal_used + amount > limits.daily_withdrawal_limit {
        return Err(AppError::TransactionLimitExceeded);
    }

    let _cash = fetch_account_for_update(&mut tx, cash_id).await?;

    let reference = reference_number("WTH");
    let txn_id = insert_transaction(
        &mut tx,
        &reference,
        "withdrawal",
        amount,
        &req.description,
        account.customer_id,
        req.external_reference.as_deref(),
        json!({}),
    )
    .await?;

    // customer *debit* (−balance); EXTERNAL_CASH *credit*.
    set_available_zero(&mut tx, account.account_id).await?;
    post_two_legged(
        &mut tx,
        txn_id,
        account.account_id,
        "debit",
        cash_id,
        "credit",
        amount,
    )
    .await?;

    let new_balance = account_balance(&mut tx, account.account_id).await?;
    recompute_available(&mut tx, account.account_id).await?;
    record_summary(&mut tx, account.account_id, "debit", amount, new_balance).await?;

    sqlx::query(
        "UPDATE account_limits SET daily_withdrawal_used = daily_withdrawal_used + $2, \
         updated_at = CURRENT_TIMESTAMP WHERE account_id = $1",
    )
    .bind(account.account_id)
    .bind(amount)
    .execute(&mut *tx)
    .await?;

    let gl = post_gl_entry(
        &state,
        &reference,
        &req.description,
        GlAccount::Payable,
        GlAccount::Bank,
        amount,
    )
    .await?;
    tag_gl_entry(&mut tx, txn_id, &format!("{}:{}", gl.backend, gl.id)).await?;

    tx.commit().await?;

    tracing::info!(account_id = %account.account_id, transaction_id = %txn_id, amount = %amount, "💸 withdrawal posted");
    let resp = load_transaction_response(&state.pool, txn_id).await?;
    Ok((StatusCode::CREATED, Json(resp)))
}

// ---------------------------------------------------------------------------
// transfer
// ---------------------------------------------------------------------------

/// Transfer between two customer accounts: from debited, to credited. Enforces
/// the daily/monthly/annual transfer limits and honors `idempotency_key`.
/// Local-only (no GL post — see the module docs).
async fn transfer_money(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Json(req): Json<MoneyTransferRequest>,
) -> Result<(StatusCode, Json<TransactionResponse>), AppError> {
    req.validate()?;
    let amount = normalize_amount(req.amount)?;

    if req.from_account_id == req.to_account_id {
        return Err(AppError::BadRequest(
            "from and to accounts must differ".to_string(),
        ));
    }

    // Idempotent replay: return the already-posted transfer for a known key.
    // Scoped to the caller so a key can't surface another customer's transfer.
    // (Best-effort — no unique index, so tightly-concurrent duplicates with the
    // same key could still both post; acceptable for this toy.)
    if let Some(key) = req.idempotency_key.as_deref() {
        if let Some(existing) = find_by_idempotency_key(&state.pool, key, auth.customer_id).await? {
            let resp = load_transaction_response(&state.pool, existing).await?;
            return Ok((StatusCode::OK, Json(resp)));
        }
    }

    let fee = transfer_fee();
    let cash_id = ensure_external_cash_account(&state.pool).await?;

    let mut tx = state.pool.begin().await?;

    // Lock both accounts in a deterministic order (by id) to avoid deadlocks.
    let (first, second) = if req.from_account_id < req.to_account_id {
        (req.from_account_id, req.to_account_id)
    } else {
        (req.to_account_id, req.from_account_id)
    };
    fetch_account_for_update(&mut tx, first).await?;
    fetch_account_for_update(&mut tx, second).await?;

    let from = fetch_account_for_update(&mut tx, req.from_account_id)
        .await?
        .ok_or_else(|| AppError::NotFound("from account not found".to_string()))?;
    let to = fetch_account_for_update(&mut tx, req.to_account_id)
        .await?
        .ok_or_else(|| AppError::NotFound("to account not found".to_string()))?;

    // Ownership: the caller may only move money out of their own account. The
    // destination can belong to anyone. 404 (not 403) so a non-owned `from`
    // account is indistinguishable from a missing one.
    if from.customer_id != auth.customer_id {
        return Err(AppError::NotFound("from account not found".to_string()));
    }

    ensure_operable(&from)?;
    ensure_operable(&to)?;

    // Lock the fee counterparty (EXTERNAL_CASH) last, after the sorted account locks.
    let _cash = fetch_account_for_update(&mut tx, cash_id).await?;

    // The funding account must cover the amount *and* the fee.
    if from.available_balance < amount + fee {
        return Err(AppError::InsufficientFunds);
    }

    // Transfer limits apply to the funding (from) account.
    let limits = ensure_and_reset_limits(&mut tx, from.account_id).await?;
    if limits.daily_transfer_used + amount > limits.daily_transfer_limit
        || limits.monthly_transfer_used + amount > limits.monthly_transfer_limit
        || limits.annual_transfer_used + amount > limits.annual_transfer_limit
    {
        return Err(AppError::TransactionLimitExceeded);
    }

    let reference = reference_number("TXF");
    let metadata = match req.idempotency_key.as_deref() {
        Some(key) => json!({ "idempotency_key": key }),
        None => json!({}),
    };
    let txn_id = insert_transaction(
        &mut tx,
        &reference,
        "transfer",
        amount,
        &req.description,
        from.customer_id,
        req.reference.as_deref(),
        metadata,
    )
    .await?;

    // from *debit* (−balance); to *credit* (+balance).
    set_available_zero(&mut tx, from.account_id).await?;
    post_two_legged(
        &mut tx,
        txn_id,
        from.account_id,
        "debit",
        to.account_id,
        "credit",
        amount,
    )
    .await?;

    let from_balance = account_balance(&mut tx, from.account_id).await?;
    let to_balance = account_balance(&mut tx, to.account_id).await?;
    recompute_available(&mut tx, from.account_id).await?;
    recompute_available(&mut tx, to.account_id).await?;
    record_summary(&mut tx, from.account_id, "debit", amount, from_balance).await?;
    record_summary(&mut tx, to.account_id, "credit", amount, to_balance).await?;

    // Transfer fee: a separate `fee` transaction, funding account → EXTERNAL_CASH,
    // with the fee recognised as Revenue at the GL. (Idempotent replays return
    // early above, so a replayed transfer never double-charges.)
    if fee > Decimal::ZERO {
        let fee_ref = reference_number("FEE");
        let fee_txn = insert_transaction(
            &mut tx,
            &fee_ref,
            "fee",
            fee,
            &format!("transfer fee for {}", reference),
            from.customer_id,
            None,
            json!({ "fee_for": txn_id }),
        )
        .await?;
        set_available_zero(&mut tx, from.account_id).await?;
        post_two_legged(
            &mut tx,
            fee_txn,
            from.account_id,
            "debit",
            cash_id,
            "credit",
            fee,
        )
        .await?;
        let fee_balance = account_balance(&mut tx, from.account_id).await?;
        recompute_available(&mut tx, from.account_id).await?;
        record_summary(&mut tx, from.account_id, "debit", fee, fee_balance).await?;
        sqlx::query(
            "INSERT INTO transaction_fees (transaction_id, fee_type, fee_amount) \
             VALUES ($1, 'transfer', $2)",
        )
        .bind(txn_id)
        .bind(fee)
        .execute(&mut *tx)
        .await?;
        let gl = post_gl_entry(
            &state,
            &fee_ref,
            "transfer fee",
            GlAccount::Payable,
            GlAccount::Revenue,
            fee,
        )
        .await?;
        tag_gl_entry(&mut tx, fee_txn, &format!("{}:{}", gl.backend, gl.id)).await?;
    }

    sqlx::query(
        "UPDATE account_limits SET \
         daily_transfer_used = daily_transfer_used + $2, \
         monthly_transfer_used = monthly_transfer_used + $2, \
         annual_transfer_used = annual_transfer_used + $2, \
         updated_at = CURRENT_TIMESTAMP WHERE account_id = $1",
    )
    .bind(from.account_id)
    .bind(amount)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    tracing::info!(
        from = %from.account_id, to = %to.account_id, transaction_id = %txn_id, amount = %amount,
        "🔁 transfer posted"
    );
    let resp = load_transaction_response(&state.pool, txn_id).await?;
    Ok((StatusCode::CREATED, Json(resp)))
}

// ---------------------------------------------------------------------------
// single fetch
// ---------------------------------------------------------------------------

/// Fetch one transaction the caller is party to (has a leg on one of their
/// accounts). 404 otherwise, so a stranger's transaction is indistinguishable
/// from a missing one.
async fn get_transaction(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Path(txn_id): Path<Uuid>,
) -> Result<Json<TransactionResponse>, AppError> {
    let involved: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM transaction_entries e \
         JOIN accounts a ON a.account_id = e.account_id \
         WHERE e.transaction_id = $1 AND a.customer_id = $2)",
    )
    .bind(txn_id)
    .bind(auth.customer_id)
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::Database)?;
    if !involved {
        return Err(AppError::NotFound("transaction not found".to_string()));
    }
    let resp = load_transaction_response(&state.pool, txn_id).await?;
    Ok(Json(resp))
}

// ---------------------------------------------------------------------------
// reversal
// ---------------------------------------------------------------------------

/// Reverse a completed deposit/withdrawal/transfer by posting its mirror. Only
/// the initiator may reverse it. v1: rejects rather than forcing a negative
/// clawback, does not refund a transfer fee, and does not restore the
/// `account_limits` counters the original consumed.
async fn reverse_transaction(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Path(txn_id): Path<Uuid>,
    Json(req): Json<ReverseRequest>,
) -> Result<(StatusCode, Json<TransactionResponse>), AppError> {
    req.validate()?;
    let reason = req
        .reason
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| "customer reversal".to_string());

    // Load the original. Initiator-only; 404 (not 403) so we don't leak existence.
    let original: Option<(String, String, Option<Uuid>, Decimal)> = sqlx::query_as(
        "SELECT transaction_type, status::text, initiated_by, amount \
         FROM transactions WHERE transaction_id = $1",
    )
    .bind(txn_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::Database)?;
    let (otype, ostatus, initiated_by, amount) =
        original.ok_or_else(|| AppError::NotFound("transaction not found".to_string()))?;
    if initiated_by != Some(auth.customer_id) {
        return Err(AppError::NotFound("transaction not found".to_string()));
    }
    if ostatus != "completed" {
        return Err(AppError::BadRequest(
            "only a completed transaction can be reversed".to_string(),
        ));
    }
    if !matches!(otype.as_str(), "deposit" | "withdrawal" | "transfer") {
        return Err(AppError::BadRequest(
            "this transaction type cannot be reversed".to_string(),
        ));
    }

    // The reversal swaps the original legs.
    let legs: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT account_id, entry_type::text FROM transaction_entries WHERE transaction_id = $1",
    )
    .bind(txn_id)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::Database)?;
    let orig_debit = legs.iter().find(|(_, t)| t == "debit").map(|(a, _)| *a);
    let orig_credit = legs.iter().find(|(_, t)| t == "credit").map(|(a, _)| *a);
    let (Some(orig_debit), Some(orig_credit)) = (orig_debit, orig_credit) else {
        return Err(AppError::BadRequest(
            "transaction has no reversible legs".to_string(),
        ));
    };
    let rev_debit = orig_credit; // debit what was credited …
    let rev_credit = orig_debit; // … credit what was debited

    let cash_id = ensure_external_cash_account(&state.pool).await?;

    let mut tx = state.pool.begin().await?;

    // Lock in a globally consistent order to avoid deadlocks: customer
    // account(s) first (id-sorted), then EXTERNAL_CASH *last*. deposit/withdrawal
    // and the transfer-fee path all lock cash last, so the reversal must too —
    // sorting purely by id could otherwise lock cash before the customer account
    // (when cash_id sorts lower) and deadlock a concurrent deposit/withdrawal.
    let mut lock_order: Vec<Uuid> = [rev_debit, rev_credit]
        .into_iter()
        .filter(|a| *a != cash_id)
        .collect();
    lock_order.sort_unstable();
    if rev_debit == cash_id || rev_credit == cash_id {
        lock_order.push(cash_id);
    }
    for account_id in &lock_order {
        fetch_account_for_update(&mut tx, *account_id).await?;
    }

    // Guarded status transition: the first reverser wins; a concurrent second
    // sees 0 rows updated and gets a 409 — no double clawback.
    let marked = sqlx::query(
        "UPDATE transactions SET status = 'reversed' \
         WHERE transaction_id = $1 AND status = 'completed'",
    )
    .bind(txn_id)
    .execute(&mut *tx)
    .await?;
    if marked.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "transaction has already been reversed".to_string(),
        ));
    }

    // Funds check only on a *customer* debit — EXTERNAL_CASH carries a 1e12
    // overdraft and is allowed to run negative.
    if rev_debit != cash_id {
        let debit_acct = fetch_account_for_update(&mut tx, rev_debit)
            .await?
            .ok_or_else(|| AppError::NotFound("account not found".to_string()))?;
        if debit_acct.available_balance < amount {
            return Err(AppError::InsufficientFunds);
        }
    }

    let reference = reference_number("REV");
    let rev_txn = insert_transaction(
        &mut tx,
        &reference,
        "reversal",
        amount,
        &format!("reversal of {txn_id}: {reason}"),
        auth.customer_id,
        Some(&txn_id.to_string()),
        json!({ "reverses": txn_id }),
    )
    .await?;

    // Post the swapped legs. Floor only a customer debit's available first.
    if rev_debit != cash_id {
        set_available_zero(&mut tx, rev_debit).await?;
    }
    post_two_legged(
        &mut tx, rev_txn, rev_debit, "debit", rev_credit, "credit", amount,
    )
    .await?;

    // Recompute + summarise customer accounts only; leave EXTERNAL_CASH at 0.
    for (acct, entry) in [(rev_debit, "debit"), (rev_credit, "credit")] {
        if acct == cash_id {
            continue;
        }
        let bal = account_balance(&mut tx, acct).await?;
        recompute_available(&mut tx, acct).await?;
        record_summary(&mut tx, acct, entry, amount, bal).await?;
    }

    // Cross-link the reversal to the original.
    sqlx::query(
        "INSERT INTO transaction_reversals \
         (original_transaction_id, reversal_transaction_id, reason, authorized_by) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(txn_id)
    .bind(rev_txn)
    .bind(&reason)
    .bind(auth.customer_id)
    .execute(&mut *tx)
    .await?;

    // Reverse the GL by original type (a transfer posted none, so nothing to undo).
    let gl = match otype.as_str() {
        "deposit" => Some(
            post_gl_entry(
                &state,
                &reference,
                &reason,
                GlAccount::Payable,
                GlAccount::Bank,
                amount,
            )
            .await?,
        ),
        "withdrawal" => Some(
            post_gl_entry(
                &state,
                &reference,
                &reason,
                GlAccount::Bank,
                GlAccount::Payable,
                amount,
            )
            .await?,
        ),
        _ => None,
    };
    if let Some(gl) = gl {
        tag_gl_entry(&mut tx, rev_txn, &format!("{}:{}", gl.backend, gl.id)).await?;
    }

    tx.commit().await?;

    tracing::info!(original = %txn_id, reversal = %rev_txn, amount = %amount, "↩️  transaction reversed");
    let resp = load_transaction_response(&state.pool, rev_txn).await?;
    Ok((StatusCode::CREATED, Json(resp)))
}

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

const TXN_COLUMNS: &str = "t.transaction_id, t.reference_number, t.transaction_type, t.amount, \
    t.currency, t.description, t.status, t.initiated_by, t.external_reference, t.metadata, \
    t.created_at, t.processed_at, t.completed_at, t.failed_at, t.failure_reason";

/// Query transaction history with optional filters and pagination.
async fn get_transactions(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Query(q): Query<TransactionHistoryQuery>,
) -> Result<Json<TransactionHistoryResponse>, AppError> {
    let limit = q.limit.unwrap_or(DEFAULT_HISTORY_LIMIT).clamp(1, 100);
    let offset = q.offset.unwrap_or(0);

    // Every query is scoped to the caller's own accounts (a leg on an account
    // they own), so history never leaks another customer's activity. That scope
    // always joins `transaction_entries`, so both queries are DISTINCT.
    let count_base = "SELECT COUNT(DISTINCT t.transaction_id) FROM transactions t \
         JOIN transaction_entries e ON e.transaction_id = t.transaction_id";
    let mut count_qb = QueryBuilder::<Postgres>::new(count_base);
    push_filters(&mut count_qb, &q, auth.customer_id);
    let total: i64 = count_qb
        .build_query_scalar()
        .fetch_one(&state.pool)
        .await
        .map_err(AppError::Database)?;

    // The page itself.
    let page_base = format!(
        "SELECT DISTINCT {TXN_COLUMNS} FROM transactions t \
         JOIN transaction_entries e ON e.transaction_id = t.transaction_id"
    );
    let mut page_qb = QueryBuilder::<Postgres>::new(page_base);
    push_filters(&mut page_qb, &q, auth.customer_id);
    page_qb.push(" ORDER BY t.created_at DESC LIMIT ");
    page_qb.push_bind(limit as i64);
    page_qb.push(" OFFSET ");
    page_qb.push_bind(offset as i64);

    let txns: Vec<Transaction> = page_qb
        .build_query_as::<Transaction>()
        .fetch_all(&state.pool)
        .await
        .map_err(AppError::Database)?;

    // Hydrate each transaction with its entries in a single follow-up query.
    let ids: Vec<Uuid> = txns.iter().map(|t| t.transaction_id).collect();
    let entries = load_entries_for(&state.pool, &ids).await?;

    let transactions: Vec<TransactionResponse> = txns
        .into_iter()
        .map(|t| {
            let id = t.transaction_id;
            let mut resp: TransactionResponse = t.into();
            resp.entries = entries
                .iter()
                .filter(|e| e.0 == id)
                .map(|e| e.1.clone())
                .collect();
            resp
        })
        .collect();

    let returned = transactions.len() as i64;
    let has_more = offset as i64 + returned < total;
    Ok(Json(TransactionHistoryResponse {
        transactions,
        total_count: total.max(0) as u64,
        has_more,
        next_offset: if has_more {
            Some(offset + returned as u32)
        } else {
            None
        },
    }))
}

fn push_filters(
    qb: &mut QueryBuilder<'_, Postgres>,
    q: &TransactionHistoryQuery,
    customer_id: Uuid,
) {
    // Ownership scope is always present (only legs on the caller's accounts), so
    // every subsequent user filter is an AND.
    qb.push(" WHERE e.account_id IN (SELECT account_id FROM accounts WHERE customer_id = ");
    qb.push_bind(customer_id);
    qb.push(")");
    if let Some(account_id) = q.account_id {
        qb.push(" AND e.account_id = ");
        qb.push_bind(account_id);
    }
    if let Some(ref transaction_type) = q.transaction_type {
        qb.push(" AND t.transaction_type = ");
        qb.push_bind(transaction_type.clone());
    }
    if let Some(ref status) = q.status {
        qb.push(" AND t.status = ");
        qb.push_bind(status.clone());
    }
    if let Some(start_date) = q.start_date {
        qb.push(" AND t.created_at >= ");
        qb.push_bind(start_date);
    }
    if let Some(end_date) = q.end_date {
        qb.push(" AND t.created_at <= ");
        qb.push_bind(end_date);
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Reject credit-card accounts (they use the card rails) and non-active status.
fn ensure_operable(account: &Account) -> Result<(), AppError> {
    if matches!(account.account_type, AccountType::CreditCard) {
        return Err(AppError::BadRequest(
            "credit card accounts use the card endpoints".to_string(),
        ));
    }
    match account.status {
        AccountStatus::Active => Ok(()),
        AccountStatus::Frozen => Err(AppError::AccountFrozen),
        _ => Err(AppError::InvalidAccountStatus),
    }
}

// Groups the `transactions` INSERT columns; the arg count mirrors the row.
#[allow(clippy::too_many_arguments)]
async fn insert_transaction(
    tx: &mut Tx<'_>,
    reference: &str,
    transaction_type: &str,
    amount: Decimal,
    description: &str,
    initiated_by: Uuid,
    external_reference: Option<&str>,
    metadata: serde_json::Value,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        INSERT INTO transactions
            (reference_number, transaction_type, amount, description, status,
             initiated_by, external_reference, completed_at, metadata)
        VALUES ($1, $2, $3, $4, 'completed', $5, $6, CURRENT_TIMESTAMP, $7)
        RETURNING transaction_id
        "#,
    )
    .bind(reference)
    .bind(transaction_type)
    .bind(amount)
    .bind(description)
    .bind(initiated_by)
    .bind(external_reference)
    .bind(metadata)
    .fetch_one(&mut **tx)
    .await
}

/// Record the id of the general-ledger entry the core assigned, on the txn.
async fn tag_gl_entry(tx: &mut Tx<'_>, txn_id: Uuid, gl_ref: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE transactions SET metadata = jsonb_set(COALESCE(metadata, '{}'::jsonb), \
         '{gl_entry}', to_jsonb($2::text)) WHERE transaction_id = $1",
    )
    .bind(txn_id)
    .bind(gl_ref)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn account_balance(tx: &mut Tx<'_>, account_id: Uuid) -> Result<Decimal, sqlx::Error> {
    sqlx::query_scalar("SELECT balance FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .fetch_one(&mut **tx)
        .await
}

/// Zero an account's `available_balance` ahead of a **debit** leg.
///
/// The `update_account_balance` trigger lowers `balance` as the entry is
/// inserted, and `chk_available_balance_logical` requires
/// `available_balance <= balance + overdraft_limit`. Since we recompute
/// `available_balance` only *after* posting, the stale (higher) value would trip
/// that CHECK mid-statement for a debited deposit account. Dropping it to 0
/// first is always safe (the post-debit balance stays ≥ 0 because we verified
/// `available_balance >= amount`); [`recompute_available`] restores the correct
/// value afterward. Credited accounts never need this (their balance only rises).
async fn set_available_zero(tx: &mut Tx<'_>, account_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE accounts SET available_balance = 0 WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Recompute a deposit account's available balance: `balance + overdraft − open holds`.
/// (Deposit accounts have a 0 overdraft; the term keeps the formula general.)
async fn recompute_available(tx: &mut Tx<'_>, account_id: Uuid) -> Result<Decimal, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        UPDATE accounts
        SET available_balance = balance + overdraft_limit
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

/// A customer account's per-day transaction limit counters + limits, after
/// resetting any that have rolled over (day / month / year).
#[derive(sqlx::FromRow)]
struct LimitState {
    daily_withdrawal_limit: Decimal,
    daily_withdrawal_used: Decimal,
    daily_transfer_limit: Decimal,
    daily_transfer_used: Decimal,
    monthly_transfer_limit: Decimal,
    monthly_transfer_used: Decimal,
    annual_transfer_limit: Decimal,
    annual_transfer_used: Decimal,
}

/// Ensure a limits row exists (table defaults) and roll over stale counters,
/// returning the current limits/usage. Uses the row lock held on the account.
async fn ensure_and_reset_limits(
    tx: &mut Tx<'_>,
    account_id: Uuid,
) -> Result<LimitState, sqlx::Error> {
    sqlx::query(
        "INSERT INTO account_limits (account_id) VALUES ($1) ON CONFLICT (account_id) DO NOTHING",
    )
    .bind(account_id)
    .execute(&mut **tx)
    .await?;

    // The right-hand expressions see the pre-update `last_reset_date`, so the
    // CASE checks compare the old reset date against today before it is bumped.
    sqlx::query_as::<_, LimitState>(
        r#"
        UPDATE account_limits SET
            daily_withdrawal_used = CASE WHEN last_reset_date < CURRENT_DATE
                THEN 0 ELSE daily_withdrawal_used END,
            daily_transfer_used = CASE WHEN last_reset_date < CURRENT_DATE
                THEN 0 ELSE daily_transfer_used END,
            monthly_transfer_used = CASE WHEN date_trunc('month', last_reset_date)
                < date_trunc('month', CURRENT_DATE) THEN 0 ELSE monthly_transfer_used END,
            annual_transfer_used = CASE WHEN date_trunc('year', last_reset_date)
                < date_trunc('year', CURRENT_DATE) THEN 0 ELSE annual_transfer_used END,
            last_reset_date = CURRENT_DATE,
            updated_at = CURRENT_TIMESTAMP
        WHERE account_id = $1
        RETURNING
            daily_withdrawal_limit, daily_withdrawal_used,
            daily_transfer_limit, daily_transfer_used,
            monthly_transfer_limit, monthly_transfer_used,
            annual_transfer_limit, annual_transfer_used
        "#,
    )
    .bind(account_id)
    .fetch_one(&mut **tx)
    .await
}

/// Upsert the account's daily summary row for today with this entry's effect.
async fn record_summary(
    tx: &mut Tx<'_>,
    account_id: Uuid,
    entry_type: &str,
    amount: Decimal,
    new_balance: Decimal,
) -> Result<(), sqlx::Error> {
    let (debit, credit) = if entry_type == "debit" {
        (amount, Decimal::ZERO)
    } else {
        (Decimal::ZERO, amount)
    };
    sqlx::query(
        r#"
        INSERT INTO daily_transaction_summaries
            (account_id, summary_date, total_debits, total_credits, transaction_count,
             largest_debit, largest_credit, end_of_day_balance)
        VALUES ($1, CURRENT_DATE, $2, $3, 1, $2, $3, $4)
        ON CONFLICT (account_id, summary_date) DO UPDATE SET
            total_debits = daily_transaction_summaries.total_debits + EXCLUDED.total_debits,
            total_credits = daily_transaction_summaries.total_credits + EXCLUDED.total_credits,
            transaction_count = daily_transaction_summaries.transaction_count + 1,
            largest_debit = GREATEST(daily_transaction_summaries.largest_debit, EXCLUDED.largest_debit),
            largest_credit = GREATEST(daily_transaction_summaries.largest_credit, EXCLUDED.largest_credit),
            end_of_day_balance = EXCLUDED.end_of_day_balance
        "#,
    )
    .bind(account_id)
    .bind(debit)
    .bind(credit)
    .bind(new_balance)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn find_by_idempotency_key(
    pool: &DatabasePool,
    key: &str,
    customer_id: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT transaction_id FROM transactions \
         WHERE transaction_type = 'transfer' AND initiated_by = $2 \
         AND metadata->>'idempotency_key' = $1 LIMIT 1",
    )
    .bind(key)
    .bind(customer_id)
    .fetch_optional(pool)
    .await
}

/// Load a full `TransactionResponse` (with entries) for one transaction.
async fn load_transaction_response(
    pool: &DatabasePool,
    txn_id: Uuid,
) -> Result<TransactionResponse, AppError> {
    let txn = sqlx::query_as::<_, Transaction>(
        "SELECT transaction_id, reference_number, transaction_type, amount, currency, description, \
         status, initiated_by, external_reference, metadata, created_at, processed_at, \
         completed_at, failed_at, failure_reason FROM transactions WHERE transaction_id = $1",
    )
    .bind(txn_id)
    .fetch_one(pool)
    .await
    .map_err(AppError::Database)?;

    let entries = load_entries_for(pool, &[txn_id]).await?;
    let mut resp: TransactionResponse = txn.into();
    resp.entries = entries.into_iter().map(|e| e.1).collect();
    Ok(resp)
}

/// Load entries for a set of transactions, paired with their transaction id.
async fn load_entries_for(
    pool: &DatabasePool,
    txn_ids: &[Uuid],
) -> Result<Vec<(Uuid, TransactionEntryResponse)>, AppError> {
    if txn_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, TransactionEntry>(
        "SELECT entry_id, transaction_id, account_id, entry_type, amount, balance_before, \
         balance_after, entry_order, created_at FROM transaction_entries \
         WHERE transaction_id = ANY($1) ORDER BY transaction_id, entry_order",
    )
    .bind(txn_ids)
    .fetch_all(pool)
    .await
    .map_err(AppError::Database)?;

    Ok(rows
        .into_iter()
        .map(|e| (e.transaction_id, e.into()))
        .collect())
}
