use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::get,
    Router,
};
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::errors::AppError;
use crate::handlers::AppState;
use crate::middleware::auth::AuthenticatedCustomer;
use crate::models::account::{
    Account, AccountBalanceResponse, AccountResponse, AccountSummary, AccountType, ActiveHold,
    CreateAccountRequest,
};

/// Opening terms for a new account, by type.
///
/// `interest_rate` is a fraction (3% = 0.03). For deposit accounts it's the
/// return paid to the customer; for a credit card it's the APR charged on the
/// balance. `credit_limit` reuses the `overdraft_limit` column — how far the
/// balance may run (0 for deposit accounts; the card's limit for a credit card).
struct OpeningTerms {
    interest_rate: Decimal,
    credit_limit: Decimal,
}

fn opening_terms(account_type: &AccountType) -> OpeningTerms {
    match account_type {
        // Chequing earns 3%; savings has no rate set yet.
        AccountType::Chequing => OpeningTerms {
            interest_rate: Decimal::new(300, 4), // 0.0300 = 3%
            credit_limit: Decimal::ZERO,
        },
        AccountType::Savings => OpeningTerms {
            interest_rate: Decimal::ZERO,
            credit_limit: Decimal::ZERO,
        },
        // Standard Canadian card terms: 19.99% APR, $5,000 limit.
        AccountType::CreditCard => OpeningTerms {
            interest_rate: Decimal::new(1999, 4), // 0.1999 = 19.99% APR
            credit_limit: Decimal::new(5000, 0),  // $5,000.00
        },
    }
}

/// Generate a 12-digit numeric account number (Canadian format, `^[0-9]{12}$`).
/// Derived from a v4 UUID's low bits — no `rand` dependency needed. Collisions
/// are astronomically unlikely and handled by a retry on the unique constraint.
fn generate_account_number() -> String {
    let n = (Uuid::new_v4().as_u128() % 1_000_000_000_000) as u64;
    format!("{:012}", n)
}

pub fn account_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(get_accounts).post(create_account))
        .route("/:id", get(get_account))
        .route("/:id/balance", get(get_balance))
}

async fn get_accounts(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
) -> Result<Json<Vec<AccountSummary>>, AppError> {
    let accounts = sqlx::query_as::<_, Account>(
        "SELECT account_id, customer_id, account_number, account_type, currency,
                balance, available_balance, status, interest_rate, overdraft_limit,
                minimum_balance, created_at, updated_at, activated_at, closed_at
         FROM accounts WHERE customer_id = $1 ORDER BY created_at DESC",
    )
    .bind(auth.customer_id)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::Database)?;

    let summaries = accounts
        .into_iter()
        .map(|a| AccountSummary {
            account_id: a.account_id,
            account_number: a.account_number,
            account_type: a.account_type,
            balance: a.balance,
            currency: a.currency,
            status: a.status,
        })
        .collect();

    Ok(Json(summaries))
}

/// Open a new account for a customer.
///
/// Inserts a row into `accounts` with the per-type interest rate and an `active`
/// status. The account starts with a zero balance — funding happens through the
/// (not-yet-implemented) transaction ledger, so `initial_deposit` is accepted
/// but ignored for now to keep the double-entry invariant intact.
async fn create_account(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Json(payload): Json<CreateAccountRequest>,
) -> Result<(StatusCode, Json<AccountResponse>), AppError> {
    let terms = opening_terms(&payload.account_type);

    // Retry a few times in the (vanishingly rare) event of an account-number clash.
    let mut last_err = None;
    for _ in 0..5 {
        let account_number = generate_account_number();
        let result = sqlx::query_as::<_, Account>(
            r#"
            INSERT INTO accounts
                (customer_id, account_number, account_type, interest_rate,
                 overdraft_limit, available_balance, status, activated_at)
            VALUES ($1, $2, $3, $4, $5, $6, 'active', CURRENT_TIMESTAMP)
            RETURNING
                account_id, customer_id, account_number, account_type, currency,
                balance, available_balance, status, interest_rate, overdraft_limit,
                minimum_balance, created_at, updated_at, activated_at, closed_at
            "#,
        )
        .bind(auth.customer_id)
        .bind(&account_number)
        .bind(&payload.account_type)
        .bind(terms.interest_rate)
        // overdraft_limit doubles as the credit limit; a credit card's available
        // balance starts at its full limit, deposit accounts start at 0.
        .bind(terms.credit_limit)
        .bind(terms.credit_limit)
        .fetch_one(&state.pool)
        .await;

        match result {
            Ok(account) => {
                tracing::info!(
                    account_id = %account.account_id,
                    customer_id = %account.customer_id,
                    account_number = %account.account_number,
                    account_type = ?account.account_type,
                    interest_rate = %account.interest_rate,
                    "✅ account created"
                );
                return Ok((StatusCode::CREATED, Json(account.into())));
            }
            Err(sqlx::Error::Database(db)) => match db.code().as_deref() {
                // unique_violation on account_number: regenerate and retry.
                Some("23505") => {
                    last_err = Some(sqlx::Error::Database(db));
                    continue;
                }
                // foreign_key_violation: the customer_id doesn't exist.
                Some("23503") => {
                    return Err(AppError::BadRequest(
                        "No customer exists with that customer_id".to_string(),
                    ))
                }
                // check_violation: e.g. bad account-number format or balance rule.
                Some("23514") => {
                    return Err(AppError::BadRequest(format!(
                        "Account data rejected: {}",
                        db.message()
                    )))
                }
                _ => return Err(AppError::Database(sqlx::Error::Database(db))),
            },
            Err(e) => return Err(AppError::Database(e)),
        }
    }

    Err(AppError::Database(last_err.unwrap_or_else(|| {
        sqlx::Error::Protocol("could not allocate a unique account number".into())
    })))
}

async fn get_account(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Path(id): Path<Uuid>,
) -> Result<Json<AccountResponse>, AppError> {
    let account = sqlx::query_as::<_, Account>(
        "SELECT account_id, customer_id, account_number, account_type, currency,
                balance, available_balance, status, interest_rate, overdraft_limit,
                minimum_balance, created_at, updated_at, activated_at, closed_at
         FROM accounts WHERE account_id = $1",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => AppError::NotFound("Account not found".to_string()),
        e => AppError::Database(e),
    })?;

    // Ownership: don't reveal that another customer's account exists — 404, not 403.
    if account.customer_id != auth.customer_id {
        return Err(AppError::NotFound("Account not found".to_string()));
    }

    Ok(Json(account.into()))
}

async fn get_balance(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Path(id): Path<Uuid>,
) -> Result<Json<AccountBalanceResponse>, AppError> {
    let account = sqlx::query_as::<_, Account>(
        "SELECT account_id, customer_id, account_number, account_type, currency,
                balance, available_balance, status, interest_rate, overdraft_limit,
                minimum_balance, created_at, updated_at, activated_at, closed_at
         FROM accounts WHERE account_id = $1",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => AppError::NotFound("Account not found".to_string()),
        e => AppError::Database(e),
    })?;

    // Ownership: don't reveal that another customer's account exists — 404, not 403.
    if account.customer_id != auth.customer_id {
        return Err(AppError::NotFound("Account not found".to_string()));
    }

    let holds = sqlx::query_as::<_, ActiveHold>(
        "SELECT hold_id, amount, reason, expires_at
         FROM account_holds
         WHERE account_id = $1 AND released_at IS NULL
         ORDER BY created_at DESC",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::Database)?;

    Ok(Json(AccountBalanceResponse {
        account_id: account.account_id,
        account_number: account.account_number,
        balance: account.balance,
        available_balance: account.available_balance,
        currency: account.currency,
        status: account.status,
        holds,
    }))
}
