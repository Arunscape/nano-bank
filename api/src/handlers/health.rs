use axum::{extract::State, http::StatusCode, response::Json};
use serde_json::{json, Value};

use crate::handlers::AppState;

pub async fn health_check(State(state): State<AppState>) -> Result<Json<Value>, StatusCode> {
    // Check database connectivity
    match crate::config::database::health_check(&state.pool).await {
        Ok(_) => Ok(Json(json!({
            "status": "healthy",
            "timestamp": chrono::Utc::now(),
            "version": env!("CARGO_PKG_VERSION"),
            "services": {
                "database": "healthy",
                "api": "healthy"
            }
        }))),
        Err(_) => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}
