//! General-ledger journal posting through the swappable [`Ledger`] port. This is
//! the representative flow that proves nano-bank can post to either the modern or
//! the legacy core unchanged — selected at startup by `CORE_BACKEND`.

use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::errors::AppError;
use crate::handlers::AppState;
use crate::ledger::{
    Account, AccountBalance, Direction, EntryLine, LedgerError, NewEntry, PostedEntry,
};

/// Map a ledger-core failure to an HTTP error, preserving the upstream status so
/// a business rejection (e.g. unbalanced → 422) isn't masked as a 5xx; a
/// transport failure means the core is unreachable.
fn ledger_error(e: LedgerError) -> AppError {
    match e {
        LedgerError::Backend { status, body } => AppError::Upstream {
            status,
            message: body,
        },
        LedgerError::Transport(msg) => {
            AppError::ServiceUnavailable(format!("ledger core unreachable: {msg}"))
        }
    }
}

pub fn ledger_routes() -> Router<AppState> {
    Router::new()
        .route("/journal", post(post_journal))
        .route("/balances", get(get_balances))
}

#[derive(Deserialize)]
struct JournalRequest {
    #[serde(default)]
    reference: Option<String>,
    #[serde(default)]
    description: Option<String>,
    lines: Vec<JournalLine>,
}

#[derive(Deserialize)]
struct JournalLine {
    account: Account,
    direction: Direction,
    amount: Decimal,
}

/// Post a balanced journal entry to whichever core is configured.
async fn post_journal(
    State(state): State<AppState>,
    Json(req): Json<JournalRequest>,
) -> Result<Json<PostedEntry>, AppError> {
    let entry = NewEntry {
        reference: req.reference,
        description: req.description,
        lines: req
            .lines
            .into_iter()
            .map(|l| EntryLine {
                account: l.account,
                direction: l.direction,
                amount: l.amount,
            })
            .collect(),
    };
    let posted = state.ledger.post_entry(entry).await.map_err(ledger_error)?;
    Ok(Json(posted))
}

async fn get_balances(
    State(state): State<AppState>,
) -> Result<Json<Vec<AccountBalance>>, AppError> {
    let balances = state.ledger.balances().await.map_err(ledger_error)?;
    Ok(Json(balances))
}
