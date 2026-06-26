use axum::{extract::{State, Query}, http::StatusCode, response::Json, routing::{get, post}, Router};
use uuid::Uuid;
use validator::Validate;

use crate::errors::AppError;
use crate::handlers::AppState;
use crate::handlers::cards::ensure_system_accounts;
use crate::models::account::AccountStatus;
use crate::models::transaction::{
    DepositRequest, MoneyTransferRequest, Transaction, TransactionEntry, TransactionHistoryQuery, TransactionHistoryResponse, TransactionResponse, TransactionEntryResponse, WithdrawalRequest
};
use crate::utils::db::{fetch_account_for_update, normalize_amount, post_two_legged, reference_number};

pub fn transaction_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(get_transactions))
        .route("/transfer", post(transfer_money))
        .route("/deposit", post(deposit_money))
        .route("/withdrawal", post(withdraw_money))
}

async fn get_transactions(
    State(state): State<AppState>,
    Query(query): Query<TransactionHistoryQuery>,
) -> Result<Json<TransactionHistoryResponse>, AppError> {
    query.validate()?;

    let limit = query.limit.unwrap_or(50).min(100) as i64;
    let offset = query.offset.unwrap_or(0) as i64;

    let mut sql = String::from(
        "SELECT t.* FROM transactions t"
    );
    let mut count_sql = String::from("SELECT COUNT(*) FROM transactions t");

    if query.account_id.is_some() {
        let join_clause = " JOIN transaction_entries e ON t.transaction_id = e.transaction_id";
        sql.push_str(join_clause);
        count_sql.push_str(join_clause);
    }

    let mut conditions = Vec::new();

    if query.account_id.is_some() {
        conditions.push("e.account_id = $1");
    }
    if query.start_date.is_some() {
        conditions.push("t.created_at >= $2");
    }
    if query.end_date.is_some() {
        conditions.push("t.created_at <= $3");
    }
    if query.transaction_type.is_some() {
        conditions.push("t.transaction_type = $4");
    }
    if query.status.is_some() {
        conditions.push("t.status = $5::transaction_status");
    }

    if !conditions.is_empty() {
        let where_clause = format!(" WHERE {}", conditions.join(" AND "));
        sql.push_str(&where_clause);
        count_sql.push_str(&where_clause);
    }

    sql.push_str(" ORDER BY t.created_at DESC LIMIT $6 OFFSET $7");

    let mut db_query = sqlx::query_as::<_, Transaction>(&sql);
    let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);

    if let Some(account_id) = query.account_id {
        db_query = db_query.bind(account_id);
        count_query = count_query.bind(account_id);
    } else {
        db_query = db_query.bind(Option::<Uuid>::None);
        count_query = count_query.bind(Option::<Uuid>::None);
    }

    db_query = db_query.bind(query.start_date);
    count_query = count_query.bind(query.start_date);

    db_query = db_query.bind(query.end_date);
    count_query = count_query.bind(query.end_date);

    db_query = db_query.bind(query.transaction_type.clone());
    count_query = count_query.bind(query.transaction_type.clone());

    db_query = db_query.bind(query.status.as_ref().map(|s| match s {
        crate::models::transaction::TransactionStatus::Pending => "pending",
        crate::models::transaction::TransactionStatus::Completed => "completed",
        crate::models::transaction::TransactionStatus::Failed => "failed",
        crate::models::transaction::TransactionStatus::Reversed => "reversed",
        crate::models::transaction::TransactionStatus::Cancelled => "cancelled",
    }));
    count_query = count_query.bind(query.status.as_ref().map(|s| match s {
        crate::models::transaction::TransactionStatus::Pending => "pending",
        crate::models::transaction::TransactionStatus::Completed => "completed",
        crate::models::transaction::TransactionStatus::Failed => "failed",
        crate::models::transaction::TransactionStatus::Reversed => "reversed",
        crate::models::transaction::TransactionStatus::Cancelled => "cancelled",
    }));

    db_query = db_query.bind(limit).bind(offset);

    let transactions = db_query.fetch_all(&state.pool).await?;
    let total_count = count_query.fetch_one(&state.pool).await? as u64;

    let transaction_ids: Vec<Uuid> = transactions.iter().map(|t| t.transaction_id).collect();

    let entries: Vec<TransactionEntry> = sqlx::query_as::<_, TransactionEntry>(
        "SELECT * FROM transaction_entries WHERE transaction_id = ANY($1) ORDER BY entry_order ASC"
    )
    .bind(&transaction_ids)
    .fetch_all(&state.pool)
    .await?;

    let mut response_txs = Vec::with_capacity(transactions.len());
    for tx in transactions {
        let mut response_tx: TransactionResponse = tx.into();
        response_tx.entries = entries
            .iter()
            .filter(|e| e.transaction_id == response_tx.transaction_id)
            .cloned()
            .map(|e| e.into())
            .collect();
        response_txs.push(response_tx);
    }

    let has_more = (offset as u64) + (response_txs.len() as u64) < total_count;
    let next_offset = if has_more { Some((offset + limit) as u32) } else { None };

    Ok(Json(TransactionHistoryResponse {
        transactions: response_txs,
        total_count,
        has_more,
        next_offset,
    }))
}

async fn transfer_money(
    State(state): State<AppState>,
    Json(req): Json<MoneyTransferRequest>,
) -> Result<(StatusCode, Json<TransactionResponse>), AppError> {
    req.validate()?;
    let amount = normalize_amount(req.amount)?;

    if req.from_account_id == req.to_account_id {
        return Err(AppError::BadRequest("Cannot transfer money to the same account".to_string()));
    }

    let mut tx = state.pool.begin().await?;

    // To prevent deadlocks, lock the accounts in consistent order (by UUID).
    let (first_id, second_id) = if req.from_account_id < req.to_account_id {
        (req.from_account_id, req.to_account_id)
    } else {
        (req.to_account_id, req.from_account_id)
    };

    let first_account = fetch_account_for_update(&mut tx, first_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Account {} not found", first_id)))?;
    let second_account = fetch_account_for_update(&mut tx, second_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Account {} not found", second_id)))?;

    let (from_account, _to_account) = if req.from_account_id == first_id {
        (first_account, second_account)
    } else {
        (second_account, first_account)
    };

    if !matches!(from_account.status, AccountStatus::Active) {
        return Err(AppError::InvalidAccountStatus);
    }

    if amount > from_account.available_balance {
        return Err(AppError::InsufficientFunds);
    }

    let reference = reference_number("TXN");
    
    // Check idempotency key if provided
    if let Some(ref idempotency_key) = req.idempotency_key {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM transactions WHERE external_reference = $1)"
        )
        .bind(idempotency_key)
        .fetch_one(&mut *tx)
        .await?;

        if exists {
            return Err(AppError::DuplicateTransaction);
        }
    }

    let txn: Transaction = sqlx::query_as::<_, Transaction>(
        r#"
        INSERT INTO transactions
            (reference_number, transaction_type, amount, description, status,
             initiated_by, completed_at, external_reference)
        VALUES ($1, 'transfer', $2, $3, 'completed', $4, CURRENT_TIMESTAMP, $5)
        RETURNING *
        "#,
    )
    .bind(&reference)
    .bind(amount)
    .bind(&req.description)
    .bind(from_account.customer_id)
    .bind(&req.idempotency_key)
    .fetch_one(&mut *tx)
    .await?;

    // from_account is *debited*, to_account is *credited*
    post_two_legged(
        &mut tx, txn.transaction_id,
        req.from_account_id, "debit",
        req.to_account_id, "credit",
        amount,
    )
    .await?;

    // Fetch the updated entries to return
    let entries: Vec<TransactionEntry> = sqlx::query_as::<_, TransactionEntry>(
        r#"
        SELECT * FROM transaction_entries
        WHERE transaction_id = $1
        ORDER BY entry_order ASC
        "#
    )
    .bind(txn.transaction_id)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;

    let mut response: TransactionResponse = txn.into();
    response.entries = entries.into_iter().map(|e| e.into()).collect();

    tracing::info!(
        transaction_id = %response.transaction_id, amount = %amount,
        "💸 transfer completed"
    );

    Ok((StatusCode::CREATED, Json(response)))
}

async fn deposit_money(
    State(state): State<AppState>,
    Json(req): Json<DepositRequest>,
) -> Result<(StatusCode, Json<TransactionResponse>), AppError> {
    req.validate()?;
    let amount = normalize_amount(req.amount)?;

    let system = ensure_system_accounts(&state.pool).await?;

    let mut tx = state.pool.begin().await?;

    // Lock in consistent order
    let (first_id, second_id) = if req.account_id < system.bank_settlement_id {
        (req.account_id, system.bank_settlement_id)
    } else {
        (system.bank_settlement_id, req.account_id)
    };

    let first_account = fetch_account_for_update(&mut tx, first_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Account {} not found", first_id)))?;
    let second_account = fetch_account_for_update(&mut tx, second_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Account {} not found", second_id)))?;

    let (customer_account, _bank_account) = if req.account_id == first_id {
        (first_account, second_account)
    } else {
        (second_account, first_account)
    };

    if !matches!(customer_account.status, AccountStatus::Active) {
        return Err(AppError::InvalidAccountStatus);
    }

    let reference = reference_number("DEP");

    let txn: Transaction = sqlx::query_as::<_, Transaction>(
        r#"
        INSERT INTO transactions
            (reference_number, transaction_type, amount, description, status,
             initiated_by, completed_at, external_reference)
        VALUES ($1, 'deposit', $2, $3, 'completed', $4, CURRENT_TIMESTAMP, $5)
        RETURNING *
        "#,
    )
    .bind(&reference)
    .bind(amount)
    .bind(&req.description)
    .bind(customer_account.customer_id)
    .bind(&req.external_reference)
    .fetch_one(&mut *tx)
    .await?;

    // Customer account is *credited*, bank settlement account is *debited*
    post_two_legged(
        &mut tx, txn.transaction_id,
        req.account_id, "credit",
        system.bank_settlement_id, "debit",
        amount,
    )
    .await?;

    let entries: Vec<TransactionEntry> = sqlx::query_as::<_, TransactionEntry>(
        r#"
        SELECT * FROM transaction_entries
        WHERE transaction_id = $1
        ORDER BY entry_order ASC
        "#
    )
    .bind(txn.transaction_id)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;

    let mut response: TransactionResponse = txn.into();
    response.entries = entries.into_iter().map(|e| e.into()).collect();

    tracing::info!(
        transaction_id = %response.transaction_id, amount = %amount,
        "💰 deposit completed"
    );

    Ok((StatusCode::CREATED, Json(response)))
}

async fn withdraw_money(
    State(state): State<AppState>,
    Json(req): Json<WithdrawalRequest>,
) -> Result<(StatusCode, Json<TransactionResponse>), AppError> {
    req.validate()?;
    let amount = normalize_amount(req.amount)?;

    let system = ensure_system_accounts(&state.pool).await?;

    let mut tx = state.pool.begin().await?;

    // Lock in consistent order
    let (first_id, second_id) = if req.account_id < system.bank_settlement_id {
        (req.account_id, system.bank_settlement_id)
    } else {
        (system.bank_settlement_id, req.account_id)
    };

    let first_account = fetch_account_for_update(&mut tx, first_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Account {} not found", first_id)))?;
    let second_account = fetch_account_for_update(&mut tx, second_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Account {} not found", second_id)))?;

    let (customer_account, _bank_account) = if req.account_id == first_id {
        (first_account, second_account)
    } else {
        (second_account, first_account)
    };

    if !matches!(customer_account.status, AccountStatus::Active) {
        return Err(AppError::InvalidAccountStatus);
    }

    if amount > customer_account.available_balance {
        return Err(AppError::InsufficientFunds);
    }

    let reference = reference_number("WDL");

    let txn: Transaction = sqlx::query_as::<_, Transaction>(
        r#"
        INSERT INTO transactions
            (reference_number, transaction_type, amount, description, status,
             initiated_by, completed_at, external_reference)
        VALUES ($1, 'withdrawal', $2, $3, 'completed', $4, CURRENT_TIMESTAMP, $5)
        RETURNING *
        "#,
    )
    .bind(&reference)
    .bind(amount)
    .bind(&req.description)
    .bind(customer_account.customer_id)
    .bind(&req.external_reference)
    .fetch_one(&mut *tx)
    .await?;

    // Customer account is *debited*, bank settlement account is *credited*
    post_two_legged(
        &mut tx, txn.transaction_id,
        req.account_id, "debit",
        system.bank_settlement_id, "credit",
        amount,
    )
    .await?;

    let entries: Vec<TransactionEntry> = sqlx::query_as::<_, TransactionEntry>(
        r#"
        SELECT * FROM transaction_entries
        WHERE transaction_id = $1
        ORDER BY entry_order ASC
        "#
    )
    .bind(txn.transaction_id)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;

    let mut response: TransactionResponse = txn.into();
    response.entries = entries.into_iter().map(|e| e.into()).collect();

    tracing::info!(
        transaction_id = %response.transaction_id, amount = %amount,
        "🏧 withdrawal completed"
    );

    Ok((StatusCode::CREATED, Json(response)))
}
