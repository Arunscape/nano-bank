use axum::{
    extract::State,
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use validator::Validate;

use crate::errors::AppError;
use crate::handlers::AppState;
use crate::middleware::auth::AuthenticatedCustomer;
use crate::models::customer::{CreateCustomerRequest, Customer, CustomerResponse};

pub fn customer_routes() -> Router<AppState> {
    Router::new()
        .route("/", post(create_customer))
        .route("/profile", get(get_profile).put(update_profile))
        .route("/kyc/documents", post(upload_kyc_document))
}

/// Register a new customer.
///
/// Inserts a row into the `customers` table and returns the created record
/// (minus sensitive fields). The customer starts with `kyc_status = 'pending'`.
///
/// The request's `password` is argon2-hashed and stored in `customer_credentials`
/// in the same transaction as the customer row, so a customer always has a way to
/// authenticate via `POST /api/v1/auth/login`. The plaintext is never persisted.
async fn create_customer(
    State(state): State<AppState>,
    Json(payload): Json<CreateCustomerRequest>,
) -> Result<(StatusCode, Json<CustomerResponse>), AppError> {
    payload.validate()?;

    // Hash before opening the transaction so a hashing failure can't leave one
    // dangling. The plaintext password is never persisted or logged.
    let password_hash = crate::utils::password::hash_password(&payload.password)?;

    // The customer row and its credential row are inserted atomically: a
    // customer must never exist without a way to authenticate.
    let mut tx = state.pool.begin().await?;

    let customer = sqlx::query_as::<_, Customer>(
        r#"
        INSERT INTO customers
            (email, phone_number, first_name, last_name, date_of_birth, sin)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING
            customer_id, email, phone_number, first_name, last_name,
            date_of_birth, sin, kyc_status, kyc_completed_at, created_at, updated_at
        "#,
    )
    .bind(&payload.email)
    .bind(&payload.phone_number)
    .bind(&payload.first_name)
    .bind(&payload.last_name)
    .bind(payload.date_of_birth)
    .bind(payload.sin.as_deref())
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match &e {
        // Map Postgres constraint violations to client errors instead of 500s.
        sqlx::Error::Database(db) => match db.code().as_deref() {
            // unique_violation: duplicate email or phone_number
            Some("23505") => AppError::Conflict(
                "A customer with this email or phone number already exists".to_string(),
            ),
            // check_violation: e.g. under-18 (chk_age) or bad SIN format (chk_sin_format)
            Some("23514") => {
                AppError::BadRequest(format!("Customer data rejected: {}", db.message()))
            }
            _ => AppError::Database(e),
        },
        _ => AppError::Database(e),
    })?;

    sqlx::query("INSERT INTO customer_credentials (customer_id, password_hash) VALUES ($1, $2)")
        .bind(customer.customer_id)
        .bind(&password_hash)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    tracing::info!(
        customer_id = %customer.customer_id,
        email = %customer.email,
        "✅ customer created"
    );

    Ok((StatusCode::CREATED, Json(customer.into())))
}

async fn get_profile(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
) -> Result<Json<CustomerResponse>, AppError> {
    let customer = sqlx::query_as::<_, Customer>(
        "SELECT customer_id, email, phone_number, first_name, last_name,
                date_of_birth, sin, kyc_status, kyc_completed_at, created_at, updated_at
         FROM customers WHERE customer_id = $1",
    )
    .bind(auth.customer_id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => AppError::NotFound("Customer not found".to_string()),
        e => AppError::Database(e),
    })?;

    Ok(Json(customer.into()))
}

async fn update_profile() -> &'static str {
    "Update profile endpoint - TODO: implement"
}

async fn upload_kyc_document() -> &'static str {
    "Upload KYC document endpoint - TODO: implement"
}
