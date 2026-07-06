//! The agent-facing surface (`/api/v1/agent/*`).
//!
//! Every handler takes [`AuthenticatedAgent`] — a live, re-validated mandate —
//! and **no account parameter**: the mandate pins the account, so an agent
//! token cannot name any other account (no confused-deputy surface). Each
//! operation passes through `policy::authorize_read`, which records the
//! decision (allow or deny) in `agent_actions` before anything is returned.
//!
//! Phase 2 adds `POST /transfers` here (mandatory `idempotency_key`, caps
//! checked under the mandate row lock).

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use validator::Validate;

use crate::errors::AppError;
use crate::handlers::cards::normalize_amount;
use crate::handlers::transactions::{
    execute_transfer, fetch_history, find_by_idempotency_key, load_transaction_response,
    AgentTransferCtx, TransferSpec,
};
use crate::handlers::AppState;
use crate::middleware::auth::AuthenticatedAgent;
use crate::models::account::{Account, AccountBalanceResponse, ActiveHold};
use crate::models::agent::{AgentTransferRequest, SCOPE_READ_BALANCE, SCOPE_READ_TRANSACTIONS};
use crate::models::transaction::{
    TransactionHistoryQuery, TransactionHistoryResponse, TransactionResponse,
};
use crate::policy;

pub fn agent_api_routes() -> Router<AppState> {
    Router::new()
        .route("/account", get(get_mandated_account))
        .route("/transactions", get(get_mandated_transactions))
        .route("/transfers", post(post_mandated_transfer))
}

/// Balance snapshot of the mandate's account (scope `read:balance`).
/// Same response shape as the customer's `GET /accounts/{id}/balance`.
async fn get_mandated_account(
    State(state): State<AppState>,
    agent: AuthenticatedAgent,
) -> Result<Json<AccountBalanceResponse>, AppError> {
    policy::authorize_read(&state.pool, &agent, SCOPE_READ_BALANCE).await?;

    let account = sqlx::query_as::<_, Account>(
        "SELECT account_id, customer_id, account_number, account_type, currency,
                balance, available_balance, status, interest_rate, overdraft_limit,
                minimum_balance, created_at, updated_at, activated_at, closed_at
         FROM accounts WHERE account_id = $1",
    )
    .bind(agent.account_id)
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::Database)?;

    let holds = sqlx::query_as::<_, ActiveHold>(
        "SELECT hold_id, amount, reason, expires_at
         FROM account_holds
         WHERE account_id = $1 AND released_at IS NULL
         ORDER BY created_at DESC",
    )
    .bind(agent.account_id)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::Database)?;

    Ok(Json(AccountBalanceResponse {
        account_id: account.account_id,
        account_number: account.account_number,
        balance: account.balance,
        available_balance: account.available_balance,
        currency: account.currency,
        status: account.status,
        holds,
    }))
}

/// History of the mandate's account (scope `read:transactions`). Reuses the
/// customer history machinery with `account_id` pinned to the mandate.
async fn get_mandated_transactions(
    State(state): State<AppState>,
    agent: AuthenticatedAgent,
    Query(mut q): Query<TransactionHistoryQuery>,
) -> Result<Json<TransactionHistoryResponse>, AppError> {
    policy::authorize_read(&state.pool, &agent, SCOPE_READ_TRANSACTIONS).await?;

    // The mandate decides the account — any client-supplied value is ignored.
    q.account_id = Some(agent.account_id);
    let history = fetch_history(&state, agent.customer_id, q).await?;
    Ok(Json(history))
}

/// Agent-initiated transfer out of the mandate's account (Phase 2).
///
/// Scope `transfer:initiate`; `idempotency_key` is REQUIRED (agents retry).
/// The mandate's `max_per_tx` / `daily_cap` / `allowed_payees` are enforced —
/// and the spend *reserved* — under the mandate row lock inside the transfer's
/// own DB transaction (`policy::authorize_and_reserve_transfer`), so a racing
/// duplicate or revocation serializes there. The funding account is implicitly
/// the mandate's; the standard flat fee applies (a bank charge — the caps
/// meter the transfer amount only).
async fn post_mandated_transfer(
    State(state): State<AppState>,
    agent: AuthenticatedAgent,
    Json(req): Json<AgentTransferRequest>,
) -> Result<(StatusCode, Json<TransactionResponse>), AppError> {
    req.validate()?;
    let amount = normalize_amount(req.amount)?;

    // No scope pre-check here: `authorize_and_reserve_transfer` checks scope
    // under the mandate lock, and the deny path below audits it — one audit
    // row per attempt, all under operation "transfer".

    // Idempotent replay: same key (scoped to the acted-for customer) returns
    // the already-posted transfer — no new movement, no new cap reservation.
    if let Some(existing) =
        find_by_idempotency_key(&state.pool, &req.idempotency_key, agent.customer_id).await?
    {
        policy::record_action(
            &state.pool,
            agent.mandate_id,
            agent.agent_id,
            agent.customer_id,
            agent.account_id,
            "transfer",
            Some(amount),
            "allowed",
            Some("IDEMPOTENT_REPLAY"),
            Some(existing),
        )
        .await
        .map_err(AppError::Database)?;
        let resp = load_transaction_response(&state.pool, existing).await?;
        return Ok((StatusCode::OK, Json(resp)));
    }

    let result = execute_transfer(
        &state,
        agent.customer_id,
        TransferSpec {
            from_account_id: agent.account_id,
            to_account_id: req.to_account_id,
            amount,
            description: &req.description,
            external_reference: None,
            idempotency_key: Some(&req.idempotency_key),
            agent: Some(AgentTransferCtx {
                agent_id: agent.agent_id,
                mandate_id: agent.mandate_id,
            }),
        },
    )
    .await;

    match result {
        Ok(resp) => Ok((StatusCode::CREATED, Json(resp))),
        Err(err) => {
            // A policy deny aborted the transfer's transaction, so the denial
            // is recorded here, outside it (audit-of-denials guarantee).
            if let AppError::PolicyDenied(reason) = &err {
                let reason = reason.clone();
                policy::record_action(
                    &state.pool,
                    agent.mandate_id,
                    agent.agent_id,
                    agent.customer_id,
                    agent.account_id,
                    "transfer",
                    Some(amount),
                    policy::decision_for(&reason),
                    Some(&reason),
                    None,
                )
                .await
                .map_err(AppError::Database)?;
            }
            Err(err)
        }
    }
}
