use axum::{
    routing::{get, post},
    Router,
};

use crate::handlers::AppState;

pub fn security_routes() -> Router<AppState> {
    Router::new()
        .route("/sessions", get(get_sessions))
        .route("/devices", get(get_devices))
        .route("/devices/trust", post(trust_device))
}

async fn get_sessions() -> &'static str {
    "Get sessions endpoint - TODO: implement"
}

async fn get_devices() -> &'static str {
    "Get devices endpoint - TODO: implement"
}

async fn trust_device() -> &'static str {
    "Trust device endpoint - TODO: implement"
}
