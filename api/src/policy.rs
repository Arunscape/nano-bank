//! The deterministic policy engine for the agent plane.
//!
//! The agent's own AI reasoning never authorizes anything — it *proposes*
//! actions, and this module *disposes*, from the mandate row alone. One module
//! (not per-endpoint checks) so the decision logic and its audit trail can't
//! drift apart: every decision — allow **and** deny — is recorded append-only
//! in `agent_actions` before the caller proceeds.
//!
//! Decision vocabulary (`allowed` / `denied` / `step_up_required`) is fixed in
//! the schema from day one; Phase 3's step-up flow reuses it unchanged.
//!
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::config::database::DatabasePool;
use crate::errors::AppError;
use crate::middleware::auth::AuthenticatedAgent;
use crate::handlers::cards::Tx;
use crate::models::agent::SCOPE_TRANSFER_INITIATE;

/// Machine-readable denial reasons (surfaced in `POLICY_DENIED` responses and
/// recorded in `agent_actions.reason`).
pub const REASON_SCOPE_MISSING: &str = "SCOPE_MISSING";
pub const REASON_PAYEE_NOT_ALLOWED: &str = "PAYEE_NOT_ALLOWED";
/// Over-limit reasons audit as decision `step_up_required`, not `denied`:
/// Phase 3 turns exactly these into pending human approvals.
pub const REASON_MAX_PER_TX_EXCEEDED: &str = "MAX_PER_TX_EXCEEDED";
pub const REASON_DAILY_CAP_EXCEEDED: &str = "DAILY_CAP_EXCEEDED";

/// Audit decision for a reason code: the two cap overruns are step-up
/// candidates; everything else is a hard deny.
pub fn decision_for(reason: &str) -> &'static str {
    match reason {
        REASON_MAX_PER_TX_EXCEEDED | REASON_DAILY_CAP_EXCEEDED => "step_up_required",
        _ => "denied",
    }
}

/// Append one decision to the `agent_actions` audit. Part of the request path
/// by design: if the audit can't be written, the action doesn't happen.
#[allow(clippy::too_many_arguments)]
pub async fn record_action(
    pool: &DatabasePool,
    mandate_id: Uuid,
    agent_id: Uuid,
    customer_id: Uuid,
    account_id: Uuid,
    operation: &str,
    amount: Option<Decimal>,
    decision: &str,
    reason: Option<&str>,
    transaction_id: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO agent_actions \
         (mandate_id, agent_id, customer_id, account_id, operation, amount, \
          decision, reason, transaction_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(mandate_id)
    .bind(agent_id)
    .bind(customer_id)
    .bind(account_id)
    .bind(operation)
    .bind(amount)
    .bind(decision)
    .bind(reason)
    .bind(transaction_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Authorize a read operation under the agent's mandate: the operation name is
/// its scope (`read:balance`, `read:transactions`). Records the decision either
/// way, then allows or returns 403 `POLICY_DENIED`.
pub async fn authorize_read(
    pool: &DatabasePool,
    agent: &AuthenticatedAgent,
    operation: &str,
) -> Result<(), AppError> {
    let allowed = agent.scopes.iter().any(|s| s == operation);
    record_action(
        pool,
        agent.mandate_id,
        agent.agent_id,
        agent.customer_id,
        agent.account_id,
        operation,
        None,
        if allowed { "allowed" } else { "denied" },
        (!allowed).then_some(REASON_SCOPE_MISSING),
        None,
    )
    .await
    .map_err(AppError::Database)?;

    if allowed {
        Ok(())
    } else {
        Err(AppError::PolicyDenied(REASON_SCOPE_MISSING.to_string()))
    }
}

/// Like [`record_action`], but on an open transaction — used for the *allowed*
/// transfer row so the audit commits atomically with the money movement.
#[allow(clippy::too_many_arguments)]
pub async fn record_action_tx(
    tx: &mut Tx<'_>,
    mandate_id: Uuid,
    agent_id: Uuid,
    customer_id: Uuid,
    account_id: Uuid,
    operation: &str,
    amount: Option<Decimal>,
    decision: &str,
    reason: Option<&str>,
    transaction_id: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO agent_actions \
         (mandate_id, agent_id, customer_id, account_id, operation, amount, \
          decision, reason, transaction_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(mandate_id)
    .bind(agent_id)
    .bind(customer_id)
    .bind(account_id)
    .bind(operation)
    .bind(amount)
    .bind(decision)
    .bind(reason)
    .bind(transaction_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Authorize an agent transfer and **reserve** the spend, race-safely, inside
/// the transfer's own DB transaction.
///
/// Locks the mandate row (`FOR UPDATE`) — **lock-order rule: the mandate is
/// locked BEFORE any account row.** Only the agent path locks mandates, so no
/// cycle with the customer/cards paths is possible. Under the lock it
/// re-checks status/expiry (a racing revocation serializes here: whoever
/// commits first wins), scope, the payee allowlist, `max_per_tx`, and the
/// daily cap (lazily reset on date rollover, the `account_limits` pattern) —
/// then bumps `daily_used`. A deny returns `Err`, aborting the transfer's
/// transaction (the caller records the denial outside it).
pub async fn authorize_and_reserve_transfer(
    tx: &mut Tx<'_>,
    mandate_id: Uuid,
    to_account_id: Uuid,
    amount: Decimal,
) -> Result<(), AppError> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
        Vec<String>,
        String,
        bool,
        Option<Decimal>,
        Option<Decimal>,
        Decimal,
        bool,
        Option<Vec<Uuid>>,
    )> = sqlx::query_as(
        "SELECT scopes, status, expires_at <= CURRENT_TIMESTAMP, max_per_tx, daily_cap, \
                daily_used, last_reset_date < CURRENT_DATE, allowed_payees \
         FROM mandates WHERE mandate_id = $1 FOR UPDATE",
    )
    .bind(mandate_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(AppError::Database)?;

    let Some((scopes, status, expired, max_per_tx, daily_cap, daily_used, stale, payees)) = row
    else {
        return Err(AppError::MandateInactive);
    };
    if status != "active" || expired {
        return Err(AppError::MandateInactive);
    }
    if !scopes.iter().any(|s| s == SCOPE_TRANSFER_INITIATE) {
        return Err(AppError::PolicyDenied(REASON_SCOPE_MISSING.to_string()));
    }
    if let Some(payees) = &payees {
        if !payees.contains(&to_account_id) {
            return Err(AppError::PolicyDenied(REASON_PAYEE_NOT_ALLOWED.to_string()));
        }
    }
    if let Some(max) = max_per_tx {
        if amount > max {
            return Err(AppError::PolicyDenied(
                REASON_MAX_PER_TX_EXCEEDED.to_string(),
            ));
        }
    }
    // Day rollover: the reservation below also refreshes last_reset_date.
    let used_today = if stale { Decimal::ZERO } else { daily_used };
    if let Some(cap) = daily_cap {
        if used_today + amount > cap {
            return Err(AppError::PolicyDenied(
                REASON_DAILY_CAP_EXCEEDED.to_string(),
            ));
        }
    }

    sqlx::query(
        "UPDATE mandates SET daily_used = $2, last_reset_date = CURRENT_DATE \
         WHERE mandate_id = $1",
    )
    .bind(mandate_id)
    .bind(used_today + amount)
    .execute(&mut **tx)
    .await
    .map_err(AppError::Database)?;

    Ok(())
}
