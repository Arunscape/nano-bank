use axum::{
    routing::{get, post},
    Router,
};

use crate::handlers::AppState;

pub fn transaction_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(get_transactions))
        .route("/transfer", post(transfer_money))
        .route("/deposit", post(deposit_money))
        .route("/withdrawal", post(withdraw_money))
}

async fn get_transactions() -> &'static str {
    "Get transactions endpoint - TODO: implement"
}

async fn transfer_money() -> &'static str {
    "Transfer money endpoint - TODO: implement"
}

async fn deposit_money() -> &'static str {
    "Deposit money endpoint - TODO: implement"
}

async fn withdraw_money() -> &'static str {
    "Withdraw money endpoint - TODO: implement"
}
