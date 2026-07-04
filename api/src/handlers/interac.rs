//! Interac e-Transfer product lifecycle. Money movement goes through the Rail
//! port (`rails::interac::InteracRail`); this module owns handle resolution,
//! the claim/decline/cancel/expiry state machine, and notifications.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::{delete, get, post},
    Router,
};
use uuid::Uuid;

use crate::errors::AppError;
use crate::handlers::AppState;
use crate::middleware::auth::{AuthenticatedCustomer, AuthenticatedService};
use crate::rails::interac::{ensure_interac_accounts, InteracRail};

pub fn interac_routes() -> Router<AppState> {
    Router::new()
        // customer plane
        .route("/etransfers", post(send_etransfer).get(list_etransfers))
        .route("/etransfers/:id", get(get_etransfer))
        .route("/etransfers/:id/claim", post(claim_etransfer))
        .route("/etransfers/:id/decline", post(decline_etransfer))
        .route("/etransfers/:id/cancel", post(cancel_etransfer))
        .route("/autodeposit", post(register_autodeposit).get(list_autodeposit))
        .route("/autodeposit/:id", delete(deregister_autodeposit))
        // network plane (service token)
        .route("/network/inbound", post(network_inbound))
        .route("/network/etransfers/:id/settle", post(network_settle))
        // admin plane (service token)
        .route("/admin/sweep-expired", post(sweep_expired))
}

/// Resolve Interac's clearing/settlement accounts (re-resolved per request) and
/// build the rail.
async fn resolve_interac(state: &AppState) -> Result<InteracRail, AppError> {
    let accts = ensure_interac_accounts(&state.pool).await?;
    Ok(InteracRail::new(accts))
}

/// Interac's default hold lifetime before auto-expiry (real Interac: 30 days).
fn expiry_days() -> i64 {
    std::env::var("NANO_BANK__INTERAC__EXPIRY_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
}

/// Max amount per e-Transfer (funds check aside). Default $3,000 like real Interac.
fn max_amount() -> rust_decimal::Decimal {
    std::env::var("NANO_BANK__INTERAC__MAX_ETRANSFER_AMOUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| rust_decimal::Decimal::new(3000, 0))
}

// -- Handler stubs (replaced wholesale in Tasks 7-14) ------------------------

async fn send_etransfer() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
async fn list_etransfers() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
async fn get_etransfer() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
async fn claim_etransfer() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
async fn decline_etransfer() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
async fn cancel_etransfer() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
async fn register_autodeposit() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
async fn list_autodeposit() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
async fn deregister_autodeposit() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
async fn network_inbound() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
async fn network_settle() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
async fn sweep_expired() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
