//! Sender-side Interac **saved payees** (an address book). Distinct from the
//! Interac e-Transfer *rail* (`handlers::interac`): this only stores/looks up a
//! customer's registered recipient handles. Sending money goes through the rail
//! (`POST /api/v1/interac/etransfers`); the manager uses these saved payees to
//! avoid re-typing a recipient email each time.
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{delete, post},
    Router,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::errors::AppError;
use crate::handlers::AppState;
use crate::middleware::auth::AuthenticatedCustomer;

/// Routes mounted under `/api/v1/customers` (merged with `customer_routes`).
pub fn recipient_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/interac-recipients",
            post(register_recipient).get(list_recipients),
        )
        .route(
            "/interac-recipients/:recipient_id",
            delete(remove_recipient),
        )
}

#[derive(Debug, Deserialize)]
struct CreateRecipientRequest {
    email: String,
    display_name: String,
}

#[derive(Debug, Serialize, FromRow)]
struct Recipient {
    recipient_id: Uuid,
    customer_id: Uuid,
    email: String,
    display_name: String,
    status: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

/// Register a saved Interac recipient (payee) for the authenticated customer.
async fn register_recipient(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Json(payload): Json<CreateRecipientRequest>,
) -> Result<(StatusCode, Json<Recipient>), AppError> {
    if payload.email.trim().is_empty() || payload.display_name.trim().is_empty() {
        return Err(AppError::BadRequest(
            "email and display_name are required".to_string(),
        ));
    }
    let rec = sqlx::query_as::<_, Recipient>(
        r#"
        INSERT INTO interac_recipients (customer_id, email, display_name)
        VALUES ($1, $2, $3)
        RETURNING recipient_id, customer_id, email, display_name, status, created_at
        "#,
    )
    .bind(auth.customer_id)
    .bind(payload.email.trim())
    .bind(payload.display_name.trim())
    .fetch_one(&state.pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) => match db.code().as_deref() {
            Some("23505") => {
                AppError::Conflict("This recipient email is already registered".to_string())
            }
            Some("23503") => AppError::BadRequest("Unknown customer".to_string()),
            _ => AppError::Database(e),
        },
        _ => AppError::Database(e),
    })?;
    Ok((StatusCode::CREATED, Json(rec)))
}

/// List the authenticated customer's active saved payees.
async fn list_recipients(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
) -> Result<Json<Vec<Recipient>>, AppError> {
    let rows = sqlx::query_as::<_, Recipient>(
        r#"
        SELECT recipient_id, customer_id, email, display_name, status, created_at
        FROM interac_recipients
        WHERE customer_id = $1 AND status = 'active'
        ORDER BY created_at DESC
        "#,
    )
    .bind(auth.customer_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows))
}

/// Soft-delete a saved payee (status = 'removed') scoped to the customer.
async fn remove_recipient(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Path(recipient_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let res = sqlx::query(
        "UPDATE interac_recipients SET status = 'removed' \
         WHERE recipient_id = $1 AND customer_id = $2 AND status = 'active'",
    )
    .bind(recipient_id)
    .bind(auth.customer_id)
    .execute(&state.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound("Recipient not found".to_string()));
    }
    Ok(StatusCode::NO_CONTENT)
}
