use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use uuid::Uuid;
use validator::Validate;

use crate::errors::AppError;
use crate::handlers::AppState;
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
/// NOTE: the request carries a `password`, but there is no credentials/auth
/// table in the schema yet, so it is validated and then discarded. Persisting a
/// password hash (argon2) is a TODO for when `/auth/login` is implemented.
async fn create_customer(
    State(state): State<AppState>,
    Json(payload): Json<CreateCustomerRequest>,
) -> Result<(StatusCode, Json<CustomerResponse>), AppError> {
    payload.validate()?;

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
    .fetch_one(&state.pool)
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

    tracing::info!(
        customer_id = %customer.customer_id,
        email = %customer.email,
        "✅ customer created"
    );

    Ok((StatusCode::CREATED, Json(customer.into())))
}

// TODO: replace customer_id query param with JWT principal once /auth/login is implemented
#[derive(Deserialize)]
struct ProfileQuery {
    customer_id: Uuid,
}

async fn get_profile(
    State(state): State<AppState>,
    Query(params): Query<ProfileQuery>,
) -> Result<Json<CustomerResponse>, AppError> {
    let customer = sqlx::query_as::<_, Customer>(
        "SELECT customer_id, email, phone_number, first_name, last_name,
                date_of_birth, sin, kyc_status, kyc_completed_at, created_at, updated_at
         FROM customers WHERE customer_id = $1",
    )
    .bind(params.customer_id)
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
