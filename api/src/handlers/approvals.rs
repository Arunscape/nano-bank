//! Step-up approvals (Phase 3) — the customer's side of a parked transfer.
//!
//! When an agent transfer breaches the mandate's amount caps, it parks in
//! `pending_approvals` (see `handlers/agent_api.rs`) instead of hard-failing.
//! These endpoints are **customer-plane only** (an agent token is rejected by
//! the extractor): the agent can never resolve its own ask. Approve executes
//! the transfer with the caps overridden for that one transfer — every other
//! check (mandate active, scope, payee allowlist, funds, account limits) still
//! runs. Decline kills it. Unresolved asks expire lazily on read/resolve.
//!
//! Status contract: `pending → executing → approved | pending(revert)`, or
//! `pending → declined | expired`. `approved` always carries `transaction_id`
//! (written atomically); `executing` is the short in-flight claim — never swept
//! by expiry, but **reclaimed** back to `pending` once `claimed_at` ages past
//! the lease window (a crash mid-execution can't strand the ask). Re-approve
//! after a reclaim is safe: the approve path first finalizes by idempotency
//! key, so money that already moved is adopted, never re-sent.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use rust_decimal::Decimal;
use serde::Deserialize;
use uuid::Uuid;

use crate::errors::AppError;
use crate::handlers::agent_api::transfer_failure_reason;
use crate::handlers::transactions::{
    execute_transfer, find_by_idempotency_key, load_transaction_response, AgentTransferCtx,
    TransferSpec,
};
use crate::handlers::AppState;
use crate::middleware::auth::AuthenticatedCustomer;
use crate::models::agent::PendingApprovalResponse;
use crate::models::transaction::TransactionResponse;
use crate::policy;

pub fn approval_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_approvals))
        .route("/:id/approve", post(approve_approval))
        .route("/:id/decline", post(decline_approval))
}

/// The customer-facing projection: joined with the agent's display name and
/// the funding account's last-4 so the owner can decide at a glance.
const APPROVAL_COLUMNS: &str = "p.approval_id, p.mandate_id, \
     a.display_name AS agent_display_name, p.account_id, \
     right(ac.account_number, 4) AS account_last4, p.to_account_id, p.amount, \
     p.description, p.reason, p.status, p.transaction_id, p.created_at, \
     p.expires_at, p.resolved_at";

const APPROVAL_FROM: &str = "FROM pending_approvals p \
     JOIN agents a ON a.agent_id = p.agent_id \
     JOIN accounts ac ON ac.account_id = p.account_id";

/// How long an `executing` claim may live before it is presumed dead and
/// reclaimed. 3× the 30s request timeout: an execution still in flight can't
/// outlive its request by that much.
pub(crate) const RECLAIM_AFTER_SECONDS: i32 = 90;

/// Reclaim-then-expire, called before every read/resolve so nobody ever acts
/// on a stale row (no sweeper needed): first revert dead `executing` claims
/// (crashed executor — the lease timed out) back to `pending`, then flip
/// overdue open asks to `expired`. Order matters: a reclaimed row already past
/// its `expires_at` correctly cascades to expired in the second statement.
async fn expire_overdue(
    pool: &crate::config::database::DatabasePool,
    customer_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE pending_approvals \
         SET status = 'pending', claimed_at = NULL \
         WHERE customer_id = $1 AND status = 'executing' AND transaction_id IS NULL \
           AND claimed_at <= CURRENT_TIMESTAMP - $2 * INTERVAL '1 second'",
    )
    .bind(customer_id)
    .bind(RECLAIM_AFTER_SECONDS)
    .execute(pool)
    .await
    .map_err(AppError::Database)?;
    sqlx::query(
        "UPDATE pending_approvals \
         SET status = 'expired', resolved_at = CURRENT_TIMESTAMP \
         WHERE customer_id = $1 AND status = 'pending' \
           AND expires_at <= CURRENT_TIMESTAMP",
    )
    .bind(customer_id)
    .execute(pool)
    .await
    .map_err(AppError::Database)?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ApprovalListQuery {
    /// Optional filter: `pending` / `approved` / `declined` / `expired`.
    status: Option<String>,
}

/// The caller's step-up approvals, newest first (all statuses unless filtered).
async fn list_approvals(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Query(q): Query<ApprovalListQuery>,
) -> Result<Json<Vec<PendingApprovalResponse>>, AppError> {
    expire_overdue(&state.pool, auth.customer_id).await?;

    let approvals = sqlx::query_as::<_, PendingApprovalResponse>(&format!(
        "SELECT {APPROVAL_COLUMNS} {APPROVAL_FROM} \
         WHERE p.customer_id = $1 AND ($2::text IS NULL OR p.status = $2) \
         ORDER BY p.created_at DESC LIMIT 100"
    ))
    .bind(auth.customer_id)
    .bind(&q.status)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::Database)?;

    Ok(Json(approvals))
}

/// The claimed row's execution ingredients.
#[derive(sqlx::FromRow)]
struct ClaimedApproval {
    mandate_id: Uuid,
    agent_id: Uuid,
    account_id: Uuid,
    to_account_id: Uuid,
    amount: Decimal,
    description: String,
    idempotency_key: String,
}

/// The atomic finalization: `approved` is born WITH its transaction_id — one
/// write, guarded on the claim still being ours. If it matches 0 rows the
/// lease was reclaimed (and possibly re-resolved) while we executed — the
/// money response is still returned honestly, but the loser backs off: no
/// audit row (the winning resolution owns the trail) and a loud warning.
async fn finalize_approved(
    state: &AppState,
    approval_id: Uuid,
    claim: &ClaimedApproval,
    customer_id: Uuid,
    resp: &TransactionResponse,
) -> Result<(), AppError> {
    let updated = sqlx::query(
        "UPDATE pending_approvals \
         SET status = 'approved', transaction_id = $2, \
             resolved_at = CURRENT_TIMESTAMP, claimed_at = NULL \
         WHERE approval_id = $1 AND status = 'executing'",
    )
    .bind(approval_id)
    .bind(resp.transaction_id)
    .execute(&state.pool)
    .await
    .map_err(AppError::Database)?;

    if updated.rows_affected() != 1 {
        tracing::warn!(approval_id = %approval_id, transaction_id = %resp.transaction_id,
            "step-up finalize lost its claim (reclaimed mid-execution) — money moved, \
             state owned by the other resolution");
        return Ok(());
    }
    policy::record_action(
        &state.pool,
        claim.mandate_id,
        claim.agent_id,
        customer_id,
        claim.account_id,
        "transfer",
        Some(claim.amount),
        "allowed",
        Some(policy::REASON_STEP_UP_APPROVED),
        Some(resp.transaction_id),
    )
    .await
    .map_err(AppError::Database)?;
    tracing::info!(approval_id = %approval_id, transaction_id = %resp.transaction_id,
        "✅ step-up approval executed");
    Ok(())
}

/// Approve a parked transfer: claim the row (guarded, race-safe), then execute
/// with the caps overridden — this consent IS the authorization for the
/// overage. The claim state is the transient `executing`, NOT `approved`:
/// `approved` is only ever written together with `transaction_id`, so a
/// polling agent can treat approved as final — there is no observable
/// approved-with-no-transaction window. On an execution failure the claim
/// reverts to `pending` (with the failure audited), so the owner can fund the
/// account and retry, or decline.
async fn approve_approval(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Path(approval_id): Path<Uuid>,
) -> Result<(StatusCode, Json<TransactionResponse>), AppError> {
    expire_overdue(&state.pool, auth.customer_id).await?;

    // Guarded claim: only one approver wins; a lost race / resolved / already
    // in-flight row is a clean 409, someone else's approval is a 404 (no
    // existence leak).
    let claimed = sqlx::query_as::<_, ClaimedApproval>(
        "UPDATE pending_approvals \
         SET status = 'executing', claimed_at = CURRENT_TIMESTAMP \
         WHERE approval_id = $1 AND customer_id = $2 AND status = 'pending' \
         RETURNING mandate_id, agent_id, account_id, to_account_id, amount, \
                   description, idempotency_key",
    )
    .bind(approval_id)
    .bind(auth.customer_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::Database)?;

    let Some(claim) = claimed else {
        let status: Option<String> = sqlx::query_scalar(
            "SELECT status FROM pending_approvals \
             WHERE approval_id = $1 AND customer_id = $2",
        )
        .bind(approval_id)
        .bind(auth.customer_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(AppError::Database)?;
        return match status.as_deref() {
            Some("expired") => Err(AppError::Conflict("approval has expired".to_string())),
            Some(s) => Err(AppError::Conflict(format!("approval is already {s}"))),
            None => Err(AppError::NotFound("approval not found".to_string())),
        };
    };

    // At-least-once safety (the reclaim makes re-approve possible): if a
    // previous stranded attempt already moved the money — executed, then
    // crashed before the approved-write — ADOPT that transaction instead of
    // paying again. The key is namespaced to this mandate, so it can only ever
    // surface this approval's own transfer.
    if let Some(existing) = find_by_idempotency_key(
        &state.pool,
        &claim.idempotency_key,
        auth.customer_id,
        Some(claim.mandate_id),
    )
    .await?
    {
        let resp = load_transaction_response(&state.pool, existing).await?;
        finalize_approved(&state, approval_id, &claim, auth.customer_id, &resp).await?;
        tracing::info!(approval_id = %approval_id, transaction_id = %existing,
            "♻️ step-up approval finalized from a prior stranded execution");
        return Ok((StatusCode::OK, Json(resp)));
    }

    let result = execute_transfer(
        &state,
        auth.customer_id,
        TransferSpec {
            from_account_id: claim.account_id,
            to_account_id: claim.to_account_id,
            amount: claim.amount,
            description: &claim.description,
            external_reference: None,
            idempotency_key: Some(&claim.idempotency_key),
            agent: Some(AgentTransferCtx {
                agent_id: claim.agent_id,
                mandate_id: claim.mandate_id,
                cap_override: true,
            }),
        },
    )
    .await;

    match result {
        Ok(resp) => {
            finalize_approved(&state, approval_id, &claim, auth.customer_id, &resp).await?;
            Ok((StatusCode::CREATED, Json(resp)))
        }
        Err(err) => {
            // Revert the claim so the ask stays actionable (expiry still applies).
            sqlx::query(
                "UPDATE pending_approvals \
                 SET status = 'pending', claimed_at = NULL \
                 WHERE approval_id = $1 AND status = 'executing' AND transaction_id IS NULL",
            )
            .bind(approval_id)
            .execute(&state.pool)
            .await
            .map_err(AppError::Database)?;
            let reason = transfer_failure_reason(&err);
            policy::record_action(
                &state.pool,
                claim.mandate_id,
                claim.agent_id,
                auth.customer_id,
                claim.account_id,
                "transfer",
                Some(claim.amount),
                policy::decision_for(&reason),
                Some(&reason),
                None,
            )
            .await
            .map_err(AppError::Database)?;
            // A dead mandate is a 401 on the AGENT plane; here the customer's
            // credential is fine — the conflict is with the approval's state.
            Err(match err {
                AppError::MandateInactive => {
                    AppError::Conflict("the mandate is no longer active".to_string())
                }
                other => other,
            })
        }
    }
}

/// Decline a parked transfer. Guarded flip, audited as a denial.
async fn decline_approval(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Path(approval_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    expire_overdue(&state.pool, auth.customer_id).await?;

    let declined = sqlx::query_as::<_, ClaimedApproval>(
        "UPDATE pending_approvals \
         SET status = 'declined', resolved_at = CURRENT_TIMESTAMP \
         WHERE approval_id = $1 AND customer_id = $2 AND status = 'pending' \
         RETURNING mandate_id, agent_id, account_id, to_account_id, amount, \
                   description, idempotency_key",
    )
    .bind(approval_id)
    .bind(auth.customer_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::Database)?;

    let Some(claim) = declined else {
        let status: Option<String> = sqlx::query_scalar(
            "SELECT status FROM pending_approvals \
             WHERE approval_id = $1 AND customer_id = $2",
        )
        .bind(approval_id)
        .bind(auth.customer_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(AppError::Database)?;
        return match status.as_deref() {
            Some("expired") => Err(AppError::Conflict("approval has expired".to_string())),
            Some(s) => Err(AppError::Conflict(format!("approval is already {s}"))),
            None => Err(AppError::NotFound("approval not found".to_string())),
        };
    };

    policy::record_action(
        &state.pool,
        claim.mandate_id,
        claim.agent_id,
        auth.customer_id,
        claim.account_id,
        "transfer",
        Some(claim.amount),
        "denied",
        Some(policy::REASON_STEP_UP_DECLINED),
        None,
    )
    .await
    .map_err(AppError::Database)?;

    tracing::info!(approval_id = %approval_id, "🚫 step-up approval declined");
    Ok(StatusCode::NO_CONTENT)
}
