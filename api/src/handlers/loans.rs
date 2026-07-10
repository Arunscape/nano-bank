use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

use crate::errors::AppError;
use crate::handlers::AppState;
use crate::handlers::cards::{post_gl_entry, post_two_legged, Tx};
use crate::ledger::Account as GlAccount;
use crate::middleware::auth::AuthenticatedCustomer;
use crate::models::loan::{ApplyLoanRequest, Loan, LoanResponse, LoanSummary, RepayLoanRequest};

pub fn loan_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(get_loans).post(apply_for_loan))
        .route("/:id", get(get_loan))
        .route("/:id/disburse", post(disburse_loan))
        .route("/:id/repay", post(repay_loan))
        .route("/admin/accrue", post(admin_accrue_interest))
}

const CASH_CUSTOMER_EMAIL: &str = "cash@nano.bank";
const CASH_ACCOUNT_TYPE: &str = "chequing";

async fn get_loans(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
) -> Result<Json<Vec<LoanSummary>>, AppError> {
    let loans = sqlx::query_as::<_, Loan>(
        "SELECT loan_id, customer_id, account_id, principal_amount, interest_rate, \
         amortization_months, monthly_payment, status, next_payment_date, created_at, updated_at \
         FROM loans WHERE customer_id = $1 ORDER BY created_at DESC"
    )
    .bind(auth.customer_id)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::Database)?;

    let summaries = loans.into_iter().map(LoanSummary::from).collect();
    Ok(Json(summaries))
}

async fn apply_for_loan(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Json(payload): Json<ApplyLoanRequest>,
) -> Result<(StatusCode, Json<LoanResponse>), AppError> {
    let principal = payload.principal_amount.round_dp(2);
    let interest_rate = payload.interest_rate;
    let months = payload.amortization_months;

    if principal <= Decimal::ZERO {
        return Err(AppError::BadRequest("Principal amount must be positive".to_string()));
    }
    if interest_rate < Decimal::ZERO || interest_rate > Decimal::ONE {
        return Err(AppError::BadRequest("Interest rate must be between 0 and 1".to_string()));
    }
    if months <= 0 {
        return Err(AppError::BadRequest("Amortization months must be positive".to_string()));
    }

    // Check customer KYC verification status
    let kyc_status: String = sqlx::query_scalar("SELECT kyc_status::text FROM customers WHERE customer_id = $1")
        .bind(auth.customer_id)
        .fetch_one(&state.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => AppError::NotFound("Customer not found".to_string()),
            e => AppError::Database(e),
        })?;

    if kyc_status != "verified" {
        return Err(AppError::BadRequest("KYC verification is required to apply for a loan".to_string()));
    }

    // Calculate amortized monthly payment
    let monthly_payment = crate::utils::math::calculate_monthly_payment(principal, interest_rate, months as u32)
        .ok_or_else(|| AppError::BadRequest("Invalid loan parameters for math calculation".to_string()))?;

    let mut tx = state.pool.begin().await?;

    // Create a new loan account
    // Available balance starts at 0; interest_rate and overdraft_limit = 0.
    // The account-number trigger generates a unique 12-digit number automatically on insert.
    let account_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO accounts (customer_id, account_number, account_type, status, interest_rate, overdraft_limit, balance, available_balance)
        VALUES ($1, '000000000000', 'loan', 'pending_activation', $2, 0, 0, 0)
        RETURNING account_id
        "#
    )
    .bind(auth.customer_id)
    .bind(interest_rate)
    .fetch_one(&mut *tx)
    .await
    .map_err(AppError::Database)?;

    // Create the loans details row
    let loan: Loan = sqlx::query_as(
        r#"
        INSERT INTO loans (customer_id, account_id, principal_amount, interest_rate, amortization_months, monthly_payment, status, next_payment_date)
        VALUES ($1, $2, $3, $4, $5, $6, 'pending_disbursement', CURRENT_DATE + INTERVAL '1 month')
        RETURNING loan_id, customer_id, account_id, principal_amount, interest_rate, amortization_months, monthly_payment, status, next_payment_date, created_at, updated_at
        "#
    )
    .bind(auth.customer_id)
    .bind(account_id)
    .bind(principal)
    .bind(interest_rate)
    .bind(months)
    .bind(monthly_payment)
    .fetch_one(&mut *tx)
    .await
    .map_err(AppError::Database)?;

    tx.commit().await?;

    tracing::info!(
        customer_id = %auth.customer_id,
        loan_id = %loan.loan_id,
        principal = %principal,
        "✅ loan applied successfully"
    );

    Ok((StatusCode::CREATED, Json(loan.into())))
}

async fn get_loan(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Path(id): Path<Uuid>,
) -> Result<Json<LoanResponse>, AppError> {
    let loan = sqlx::query_as::<_, Loan>(
        "SELECT loan_id, customer_id, account_id, principal_amount, interest_rate, \
         amortization_months, monthly_payment, status, next_payment_date, created_at, updated_at \
         FROM loans WHERE loan_id = $1"
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => AppError::NotFound("Loan not found".to_string()),
        e => AppError::Database(e),
    })?;

    if loan.customer_id != auth.customer_id {
        return Err(AppError::NotFound("Loan not found".to_string()));
    }

    Ok(Json(loan.into()))
}

async fn disburse_loan(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Path(id): Path<Uuid>,
) -> Result<Json<LoanResponse>, AppError> {
    let loan = sqlx::query_as::<_, Loan>(
        "SELECT loan_id, customer_id, account_id, principal_amount, interest_rate, \
         amortization_months, monthly_payment, status, next_payment_date, created_at, updated_at \
         FROM loans WHERE loan_id = $1"
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => AppError::NotFound("Loan not found".to_string()),
        e => AppError::Database(e),
    })?;

    if loan.customer_id != auth.customer_id {
        return Err(AppError::NotFound("Loan not found".to_string()));
    }
    if loan.status != "pending_disbursement" {
        return Err(AppError::BadRequest("Loan is not pending disbursement".to_string()));
    }

    // Locate the customer's first active chequing account
    let dest_account_id: Uuid = sqlx::query_scalar(
        "SELECT account_id FROM accounts \
         WHERE customer_id = $1 AND status = 'active' AND account_type = 'chequing' \
         ORDER BY created_at LIMIT 1"
    )
    .bind(auth.customer_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::Database)?
    .or(
        // fallback to savings if no active chequing
        sqlx::query_scalar(
            "SELECT account_id FROM accounts \
             WHERE customer_id = $1 AND status = 'active' AND account_type = 'savings' \
             ORDER BY created_at LIMIT 1"
        )
        .bind(auth.customer_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(AppError::Database)?
    )
    .ok_or_else(|| AppError::BadRequest("No active deposit account found to receive disbursement".to_string()))?;

    let mut tx = state.pool.begin().await?;

    // Lock both accounts in id-sorted order to prevent deadlocks
    let mut ids = vec![loan.account_id, dest_account_id];
    ids.sort();

    for account_id in ids {
        sqlx::query("SELECT 1 FROM accounts WHERE account_id = $1 FOR UPDATE")
            .bind(account_id)
            .execute(&mut *tx)
            .await?;
    }

    let reference = reference_number("LND");
    let txn_id = insert_transaction(
        &mut tx,
        &reference,
        "disbursement",
        loan.principal_amount,
        &format!("Loan disbursement for loan {}", loan.loan_id),
        auth.customer_id,
        None,
        json!({ "loan_id": loan.loan_id }),
    )
    .await?;

    // Post the local double-entry: debit Loan (-balance), credit Chequing (+balance)
    post_two_legged(
        &mut tx,
        txn_id,
        loan.account_id,
        "debit",
        dest_account_id,
        "credit",
        loan.principal_amount,
    )
    .await?;

    // Recompute available balance on accounts
    recompute_available(&mut tx, loan.account_id).await?;
    recompute_available(&mut tx, dest_account_id).await?;

    // Activate the loan and its account
    sqlx::query("UPDATE loans SET status = 'active', updated_at = CURRENT_TIMESTAMP WHERE loan_id = $1")
        .bind(loan.loan_id)
        .execute(&mut *tx)
        .await?;

    sqlx::query("UPDATE accounts SET status = 'active', updated_at = CURRENT_TIMESTAMP WHERE account_id = $1")
        .bind(loan.account_id)
        .execute(&mut *tx)
        .await?;

    // Dual-post GL Entry: Debit Receivable (asset up), Credit Payable (deposit liability up)
    let posted = post_gl_entry(
        &state,
        &reference,
        &format!("Loan disbursement for customer {}", auth.customer_id),
        GlAccount::Receivable,
        GlAccount::Payable,
        loan.principal_amount,
    )
    .await?;

    // Tag GL entry reference on transaction metadata
    sqlx::query(
        "UPDATE transactions SET metadata = jsonb_set(COALESCE(metadata, '{}'::jsonb), \
         '{gl_entry}', to_jsonb($2::text)) WHERE transaction_id = $1"
    )
    .bind(txn_id)
    .bind(&format!("{}:{}", posted.backend, posted.id))
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    tracing::info!(
        loan_id = %loan.loan_id,
        dest_account = %dest_account_id,
        principal = %loan.principal_amount,
        "💰 loan disbursed successfully"
    );

    // Fetch and return the updated loan response
    let updated_loan = sqlx::query_as::<_, Loan>(
        "SELECT loan_id, customer_id, account_id, principal_amount, interest_rate, \
         amortization_months, monthly_payment, status, next_payment_date, created_at, updated_at \
         FROM loans WHERE loan_id = $1"
    )
    .bind(loan.loan_id)
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::Database)?;

    Ok(Json(updated_loan.into()))
}

async fn repay_loan(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Path(id): Path<Uuid>,
    Json(payload): Json<RepayLoanRequest>,
) -> Result<Json<LoanResponse>, AppError> {
    let amount = payload.amount.round_dp(2);
    if amount <= Decimal::ZERO {
        return Err(AppError::BadRequest("Repayment amount must be positive".to_string()));
    }

    let loan = sqlx::query_as::<_, Loan>(
        "SELECT loan_id, customer_id, account_id, principal_amount, interest_rate, \
         amortization_months, monthly_payment, status, next_payment_date, created_at, updated_at \
         FROM loans WHERE loan_id = $1"
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => AppError::NotFound("Loan not found".to_string()),
        e => AppError::Database(e),
    })?;

    if loan.customer_id != auth.customer_id {
        return Err(AppError::NotFound("Loan not found".to_string()));
    }
    if loan.status != "active" {
        return Err(AppError::BadRequest("Repayment is only permitted on active loans".to_string()));
    }

    // Fetch and validate the funding account
    let funding = sqlx::query_as::<_, crate::models::account::Account>(
        "SELECT account_id, customer_id, account_number, account_type, currency, \
         balance, available_balance, status, interest_rate, overdraft_limit, \
         minimum_balance, created_at, updated_at, activated_at, closed_at \
         FROM accounts WHERE account_id = $1"
    )
    .bind(payload.funding_account_id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => AppError::NotFound("Funding account not found".to_string()),
        e => AppError::Database(e),
    })?;

    if funding.customer_id != auth.customer_id {
        return Err(AppError::NotFound("Funding account not found".to_string()));
    }
    if funding.status != crate::models::account::AccountStatus::Active {
        return Err(AppError::BadRequest("Funding account is not active".to_string()));
    }
    if funding.available_balance < amount {
        return Err(AppError::InsufficientFunds);
    }

    // Determine remaining debt on the loan account
    let loan_account_balance: Decimal = sqlx::query_scalar("SELECT balance FROM accounts WHERE account_id = $1")
        .bind(loan.account_id)
        .fetch_one(&state.pool)
        .await
        .map_err(AppError::Database)?;

    let remaining_debt = -loan_account_balance;
    if remaining_debt <= Decimal::ZERO {
        return Err(AppError::BadRequest("Loan is already fully repaid".to_string()));
    }

    let amount_to_pay = amount.min(remaining_debt);

    let mut tx = state.pool.begin().await?;

    // Lock accounts in id-sorted order to prevent deadlocks
    let mut ids = vec![loan.account_id, funding.account_id];
    ids.sort();

    for account_id in ids {
        sqlx::query("SELECT 1 FROM accounts WHERE account_id = $1 FOR UPDATE")
            .bind(account_id)
            .execute(&mut *tx)
            .await?;
    }

    let reference = reference_number("PAY");
    let txn_id = insert_transaction(
        &mut tx,
        &reference,
        "loan_repayment",
        amount_to_pay,
        &format!("Repayment of loan {}", loan.loan_id),
        auth.customer_id,
        None,
        json!({ "loan_id": loan.loan_id }),
    )
    .await?;

    // Temporarily drop available balance of the funding account to prevent check constraint violations mid-statement
    set_available_zero(&mut tx, funding.account_id).await?;

    // Post the local double-entry: debit Chequing (-balance), credit Loan (+balance closer to 0)
    post_two_legged(
        &mut tx,
        txn_id,
        funding.account_id,
        "debit",
        loan.account_id,
        "credit",
        amount_to_pay,
    )
    .await?;

    // Recompute available balance on accounts
    recompute_available(&mut tx, funding.account_id).await?;
    recompute_available(&mut tx, loan.account_id).await?;

    // Fetch the new balance of the loan account to see if it is closed
    let new_loan_balance: Decimal = sqlx::query_scalar("SELECT balance FROM accounts WHERE account_id = $1")
        .bind(loan.account_id)
        .fetch_one(&mut *tx)
        .await?;

    if new_loan_balance >= Decimal::ZERO {
        // Loan is fully paid off!
        sqlx::query("UPDATE loans SET status = 'closed', updated_at = CURRENT_TIMESTAMP WHERE loan_id = $1")
            .bind(loan.loan_id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("UPDATE accounts SET status = 'closed', updated_at = CURRENT_TIMESTAMP WHERE account_id = $1")
            .bind(loan.account_id)
            .execute(&mut *tx)
            .await?;

        tracing::info!(loan_id = %loan.loan_id, "🎉 loan fully repaid and closed");
    } else {
        // Advance next payment date by 1 month
        sqlx::query("UPDATE loans SET next_payment_date = next_payment_date + INTERVAL '1 month', updated_at = CURRENT_TIMESTAMP WHERE loan_id = $1")
            .bind(loan.loan_id)
            .execute(&mut *tx)
            .await?;
    }

    // Dual-post GL Entry: Debit Payable (deposit liability down), Credit Receivable (loan receivable down)
    let posted = post_gl_entry(
        &state,
        &reference,
        &format!("Loan repayment for customer {}", auth.customer_id),
        GlAccount::Payable,
        GlAccount::Receivable,
        amount_to_pay,
    )
    .await?;

    // Tag GL entry reference on transaction metadata
    sqlx::query(
        "UPDATE transactions SET metadata = jsonb_set(COALESCE(metadata, '{}'::jsonb), \
         '{gl_entry}', to_jsonb($2::text)) WHERE transaction_id = $1"
    )
    .bind(txn_id)
    .bind(&format!("{}:{}", posted.backend, posted.id))
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    // Fetch and return the updated loan response
    let updated_loan = sqlx::query_as::<_, Loan>(
        "SELECT loan_id, customer_id, account_id, principal_amount, interest_rate, \
         amortization_months, monthly_payment, status, next_payment_date, created_at, updated_at \
         FROM loans WHERE loan_id = $1"
    )
    .bind(loan.loan_id)
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::Database)?;

    Ok(Json(updated_loan.into()))
}

async fn admin_accrue_interest(
    State(state): State<AppState>,
) -> Result<StatusCode, AppError> {
    let active_loans = sqlx::query_as::<_, Loan>(
        "SELECT loan_id, customer_id, account_id, principal_amount, interest_rate, \
         amortization_months, monthly_payment, status, next_payment_date, created_at, updated_at \
         FROM loans WHERE status = 'active'"
    )
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::Database)?;

    let cash_id = ensure_external_cash_account(&state.pool).await.map_err(AppError::Database)?;

    for loan in active_loans {
        let balance: Decimal = sqlx::query_scalar("SELECT balance FROM accounts WHERE account_id = $1")
            .bind(loan.account_id)
            .fetch_one(&state.pool)
            .await
            .map_err(AppError::Database)?;

        let remaining_debt = -balance;
        if remaining_debt <= Decimal::ZERO {
            continue;
        }

        // Daily rate is annual rate / 365
        let daily_rate = loan.interest_rate / Decimal::from(365);
        let interest = (remaining_debt * daily_rate).round_dp(2);

        if interest <= Decimal::ZERO {
            continue;
        }

        let mut tx = state.pool.begin().await?;

        // Lock accounts in id-sorted order to prevent deadlocks
        let mut ids = vec![loan.account_id, cash_id];
        ids.sort();

        for account_id in ids {
            sqlx::query("SELECT 1 FROM accounts WHERE account_id = $1 FOR UPDATE")
                .bind(account_id)
                .execute(&mut *tx)
                .await?;
        }

        let reference = reference_number("INT");
        let txn_id = insert_transaction(
            &mut tx,
            &reference,
            "interest_accrual",
            interest,
            &format!("Daily interest accrual for loan {}", loan.loan_id),
            loan.customer_id,
            None,
            json!({ "loan_id": loan.loan_id }),
        )
        .await?;

        // Post the local double-entry: debit Loan (-balance decreases further), credit Cash (+balance)
        post_two_legged(
            &mut tx,
            txn_id,
            loan.account_id,
            "debit",
            cash_id,
            "credit",
            interest,
        )
        .await?;

        // Recompute available balance on accounts
        recompute_available(&mut tx, loan.account_id).await?;

        // Dual-post GL Entry: Debit Receivable (asset up), Credit Revenue (interest income revenue up)
        let posted = post_gl_entry(
            &state,
            &reference,
            &format!("Daily interest accrual for customer {}", loan.customer_id),
            GlAccount::Receivable,
            GlAccount::Revenue,
            interest,
        )
        .await?;

        // Tag GL entry reference on transaction metadata
        sqlx::query(
            "UPDATE transactions SET metadata = jsonb_set(COALESCE(metadata, '{}'::jsonb), \
             '{gl_entry}', to_jsonb($2::text)) WHERE transaction_id = $1"
        )
        .bind(txn_id)
        .bind(&format!("{}:{}", posted.backend, posted.id))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        tracing::info!(loan_id = %loan.loan_id, interest = %interest, "📈 interest accrued");
    }

    Ok(StatusCode::OK)
}

// ---------------------------------------------------------------------------
// Low-level DB helpers
// ---------------------------------------------------------------------------

async fn ensure_external_cash_account(
    pool: &crate::config::database::DatabasePool,
) -> Result<Uuid, sqlx::Error> {
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

fn reference_number(prefix: &str) -> String {
    let n = (Uuid::new_v4().as_u128() % 1_000_000_000_000) as u64;
    format!("{}{:012}", prefix, n)
}

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

async fn set_available_zero(
    tx: &mut Tx<'_>,
    account_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE accounts SET available_balance = 0 WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn recompute_available(
    tx: &mut Tx<'_>,
    account_id: Uuid,
) -> Result<Decimal, sqlx::Error> {
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
