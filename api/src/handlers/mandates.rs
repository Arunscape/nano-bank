//! Mandate lifecycle — the consent records that gate agent access.
//!
//! Creating a mandate while authenticated as the account owner **is** the
//! consent act (API-first; a UI would be a thin form over these endpoints).
//! Consent events (grant/revoke) are written to `audit_logs` under the *user's*
//! identity, distinguishable from agent activity in `agent_actions`.
//!
//! Revocation is a guarded status flip (the transaction-reversal pattern): the
//! mandate row is re-read on every agent request, so a revoke kills every
//! outstanding agent token for the grant immediately.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{delete, post},
    Router,
};
use chrono::Utc;
use serde_json::json;
use uuid::Uuid;
use validator::Validate;

use crate::errors::AppError;
use crate::handlers::AppState;
use crate::middleware::auth::AuthenticatedCustomer;
use crate::models::agent::{
    CreateMandateRequest, MandateResponse, KNOWN_SCOPES, SCOPE_TRANSFER_INITIATE,
};

pub fn mandate_routes() -> Router<AppState> {
    Router::new()
        .route("/", post(create_mandate).get(list_mandates))
        .route("/:id", delete(revoke_mandate))
}

const MANDATE_COLUMNS: &str = "mandate_id, agent_id, account_id, scopes, max_per_tx, \
     daily_cap, allowed_payees, daily_used, status, expires_at, created_at, revoked_at";

/// Record a consent event (grant/revoke) in the general audit log under the
/// acting user's identity.
async fn audit_consent(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    action: &str,
    mandate_id: Uuid,
    auth: &AuthenticatedCustomer,
    details: serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_logs (entity_type, entity_id, action, new_values, user_id, session_id) \
         VALUES ('mandate', $1, $2::audit_action, $3, $4, $5)",
    )
    .bind(mandate_id)
    .bind(action)
    .bind(details)
    .bind(auth.customer_id)
    .bind(auth.session_id.map(|s| s.to_string()))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Grant a mandate — the consent act. Only the account owner can grant; a
/// non-owned account returns 404 (not 403) so existence isn't leaked.
async fn create_mandate(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Json(req): Json<CreateMandateRequest>,
) -> Result<(StatusCode, Json<MandateResponse>), AppError> {
    req.validate()?;

    if let Some(unknown) = req
        .scopes
        .iter()
        .find(|s| !KNOWN_SCOPES.contains(&s.as_str()))
    {
        return Err(AppError::BadRequest(format!("unknown scope: {unknown}")));
    }
    if req.expires_at <= Utc::now() {
        return Err(AppError::BadRequest(
            "expires_at must be in the future".to_string(),
        ));
    }
    // Money movement must always be bounded: granting transfer:initiate
    // requires both limits up front (Phase 2 enforces them under lock).
    if req.scopes.iter().any(|s| s == SCOPE_TRANSFER_INITIATE) {
        match (req.max_per_tx, req.daily_cap) {
            (Some(m), Some(d)) if m > rust_decimal::Decimal::ZERO && d >= m => {}
            _ => {
                return Err(AppError::BadRequest(
                    "transfer:initiate requires max_per_tx > 0 and daily_cap >= max_per_tx"
                        .to_string(),
                ))
            }
        }
    }

    // Ownership: derive the grantor from the token; 404 for a non-owned account.
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT customer_id FROM accounts WHERE account_id = $1")
            .bind(req.account_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::Database)?;
    if owner != Some(auth.customer_id) {
        return Err(AppError::NotFound("account not found".to_string()));
    }

    // The grantee must exist and not be globally disabled.
    let agent_status: Option<String> =
        sqlx::query_scalar("SELECT status FROM agents WHERE agent_id = $1")
            .bind(req.agent_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::Database)?;
    match agent_status.as_deref() {
        Some("active") => {}
        Some(_) => return Err(AppError::BadRequest("agent is disabled".to_string())),
        None => return Err(AppError::BadRequest("unknown agent".to_string())),
    }

    let mut tx = state.pool.begin().await?;

    let mandate = sqlx::query_as::<_, MandateResponse>(&format!(
        "INSERT INTO mandates \
         (customer_id, agent_id, account_id, scopes, max_per_tx, daily_cap, allowed_payees, \
          expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         RETURNING {MANDATE_COLUMNS}"
    ))
    .bind(auth.customer_id)
    .bind(req.agent_id)
    .bind(req.account_id)
    .bind(&req.scopes)
    .bind(req.max_per_tx)
    .bind(req.daily_cap)
    .bind(&req.allowed_payees)
    .bind(req.expires_at)
    .fetch_one(&mut *tx)
    .await
    .map_err(AppError::Database)?;

    audit_consent(
        &mut tx,
        "grant_mandate",
        mandate.mandate_id,
        &auth,
        json!({
            "agent_id": req.agent_id,
            "account_id": req.account_id,
            "scopes": req.scopes,
            "max_per_tx": req.max_per_tx,
            "daily_cap": req.daily_cap,
            "allowed_payees": req.allowed_payees,
            "expires_at": req.expires_at,
        }),
    )
    .await?;

    tx.commit().await?;

    tracing::info!(mandate_id = %mandate.mandate_id, agent_id = %req.agent_id, "🤝 mandate granted");
    Ok((StatusCode::CREATED, Json(mandate)))
}

/// List the caller's own mandates (all statuses, newest first).
async fn list_mandates(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
) -> Result<Json<Vec<MandateResponse>>, AppError> {
    let mandates = sqlx::query_as::<_, MandateResponse>(&format!(
        "SELECT {MANDATE_COLUMNS} FROM mandates \
         WHERE customer_id = $1 ORDER BY created_at DESC"
    ))
    .bind(auth.customer_id)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::Database)?;

    Ok(Json(mandates))
}

/// Revoke a mandate. Guarded status flip: only an `active` row transitions, so
/// a repeat revoke is a clean 409 and the audit trail records exactly one
/// revocation. Takes effect on the agent's next request (per-request lookup).
async fn revoke_mandate(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Path(mandate_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let mut tx = state.pool.begin().await?;

    let flipped = sqlx::query(
        "UPDATE mandates SET status = 'revoked', revoked_at = CURRENT_TIMESTAMP \
         WHERE mandate_id = $1 AND customer_id = $2 AND status = 'active'",
    )
    .bind(mandate_id)
    .bind(auth.customer_id)
    .execute(&mut *tx)
    .await?;

    if flipped.rows_affected() != 1 {
        // Owned but not active → 409; unknown or someone else's → 404.
        let status: Option<String> = sqlx::query_scalar(
            "SELECT status FROM mandates WHERE mandate_id = $1 AND customer_id = $2",
        )
        .bind(mandate_id)
        .bind(auth.customer_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(AppError::Database)?;
        return match status {
            Some(_) => Err(AppError::Conflict("mandate is not active".to_string())),
            None => Err(AppError::NotFound("mandate not found".to_string())),
        };
    }

    audit_consent(&mut tx, "revoke_mandate", mandate_id, &auth, json!({})).await?;
    tx.commit().await?;

    tracing::info!(mandate_id = %mandate_id, "🚫 mandate revoked");
    Ok(StatusCode::NO_CONTENT)
}
