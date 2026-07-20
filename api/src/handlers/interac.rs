//! Interac e-Transfer product lifecycle. Money movement goes through the Rail
//! port (`rails::interac::InteracRail`); this module owns handle resolution,
//! the claim/decline/cancel/expiry state machine, and notifications.

use std::collections::HashMap;

use axum::Json as AxumJson;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::{delete, get, post},
    Router,
};
use rust_decimal::Decimal;
use uuid::Uuid;
use validator::Validate;

use crate::errors::AppError;
use crate::handlers::cards::{fetch_account_for_update, normalize_amount};
use crate::handlers::AppState;
use crate::middleware::auth::{AuthenticatedCustomer, AuthenticatedService};
use crate::models::interac::{
    ClaimEtransferRequest, EtransferResponse, HandleResponse, InboundEtransferRequest,
    RegisterAutodepositRequest, SendEtransferRequest, SettleEtransferRequest,
};
use crate::rails::common::{recompute_available, zero_available};
use crate::rails::interac::{ensure_interac_accounts, normalize_handle, InteracRail};
use crate::rails::{Destination, Rail};
use crate::utils::password::{hash_password, verify_password};

pub fn interac_routes() -> Router<AppState> {
    Router::new()
        // customer plane
        .route("/etransfers", post(send_etransfer).get(list_etransfers))
        .route("/etransfers/:id", get(get_etransfer))
        .route("/etransfers/:id/claim", post(claim_etransfer))
        .route("/etransfers/:id/decline", post(decline_etransfer))
        .route("/etransfers/:id/cancel", post(cancel_etransfer))
        .route(
            "/autodeposit",
            post(register_autodeposit).get(list_autodeposit),
        )
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

/// Interac's default hold lifetime before auto-expiry (from layered `Settings`).
fn expiry_days(state: &AppState) -> i64 {
    state.settings.interac.expiry_days
}

/// Max amount per e-Transfer (funds check aside), from layered `Settings`.
fn max_amount(state: &AppState) -> rust_decimal::Decimal {
    state.settings.interac.max_etransfer_amount
}

// -- Handler stubs (replaced wholesale in Tasks 7-14) ------------------------

async fn send_etransfer(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    AxumJson(req): AxumJson<SendEtransferRequest>,
) -> Result<(StatusCode, Json<EtransferResponse>), AppError> {
    req.validate()?;
    let amount = normalize_amount(req.amount)?;
    if amount > max_amount(&state) {
        return Err(AppError::BadRequest(format!(
            "amount exceeds per-transfer max {}",
            max_amount(&state)
        )));
    }
    let recipient_handle = normalize_handle(req.recipient_handle_type, &req.recipient_handle_value);
    let rail = resolve_interac(&state).await?;

    // Idempotency replay: same (sender, key) returns the original.
    if let Some(key) = &req.idempotency_key {
        if let Some(existing) = load_etransfer_by_key(&state, caller.customer_id, key).await? {
            return Ok((StatusCode::CREATED, Json(existing)));
        }
    }

    // Look up whether the recipient handle is registered here, and autodeposit.
    let registration = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
        "SELECT customer_id, autodeposit_account_id FROM interac_handles \
         WHERE handle_value=$1 AND active=TRUE",
    )
    .bind(&recipient_handle)
    .fetch_optional(&state.pool)
    .await?;

    // Non-autodeposit transfers require a security question + answer.
    let autodeposit = registration.as_ref().and_then(|(_, ad)| *ad);
    let (question, answer_hash) = if autodeposit.is_some() {
        (None, None)
    } else {
        let q = req.security_question.clone().ok_or_else(|| {
            AppError::BadRequest("security_question required (recipient has no autodeposit)".into())
        })?;
        let a = req
            .security_answer
            .clone()
            .ok_or_else(|| AppError::BadRequest("security_answer required".into()))?;
        (Some(q), Some(hash_password(&a.to_lowercase())?))
    };

    let mut tx = state.pool.begin().await?;

    // Fund the hold: sender account must belong to caller, be active, and have funds.
    let sender = fetch_account_for_update(&mut tx, req.from_account_id)
        .await?
        .ok_or_else(|| AppError::NotFound("account not found".into()))?;
    if sender.customer_id != caller.customer_id {
        return Err(AppError::NotFound("account not found".into())); // 404, not 403
    }
    if amount > sender.available_balance {
        return Err(AppError::InsufficientFunds);
    }

    // The balance trigger lowers `balance` as the debit leg is inserted, but
    // `available_balance` is maintained by the caller (as in transactions.rs).
    // Drop it to 0 first so `chk_available_balance_logical`
    // (available <= balance + overdraft) can't trip mid-statement; recompute
    // the true value right after the hold posts.
    zero_available(&mut tx, sender.account_id).await?;
    let hold = rail
        .hold(
            &state,
            &mut tx,
            sender.account_id,
            amount,
            &format!("Interac e-Transfer to {recipient_handle}"),
        )
        .await?;
    recompute_available(&mut tx, sender.account_id).await?;

    // Create the etransfer row (outbound, held).
    let claim_token = crate::handlers::cards::reference_number("CLM");
    let etransfer_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO interac_etransfers
            (direction, status, amount, sender_customer_id, sender_account_id,
             recipient_handle_type, recipient_handle_value, recipient_customer_id,
             security_question, security_answer_hash, claim_token, memo,
             hold_transaction_id, idempotency_key, expires_at)
        VALUES ('outbound','held',$1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,
                CURRENT_TIMESTAMP + ($13 || ' days')::interval)
        RETURNING etransfer_id
        "#,
    )
    .bind(amount)
    .bind(caller.customer_id)
    .bind(sender.account_id)
    .bind(req.recipient_handle_type)
    .bind(&recipient_handle)
    .bind(registration.as_ref().map(|(c, _)| *c))
    .bind(&question)
    .bind(&answer_hash)
    .bind(&claim_token)
    .bind(&req.memo)
    .bind(hold.transaction_id)
    .bind(&req.idempotency_key)
    .bind(expiry_days(&state).to_string())
    .fetch_one(&mut *tx)
    .await
    .map_err(idempotency_conflict)?;

    // Route based on the recipient.
    let status = match (registration.as_ref(), autodeposit) {
        (Some((recipient_customer, _)), Some(deposit_acct)) => {
            // Autodeposit: release into their account immediately.
            let _ = recipient_customer;
            rail.release(
                &state,
                &mut tx,
                &hold,
                Destination::Internal(deposit_acct),
                "Interac e-Transfer autodeposit",
            )
            .await?;
            // Recipient was just credited; refresh its available_balance too.
            recompute_available(&mut tx, deposit_acct).await?;
            mark_deposited(&mut tx, etransfer_id, deposit_acct).await?;
            notify(
                &mut tx,
                etransfer_id,
                &recipient_handle,
                "deposit_completed",
                &format!("${amount} was automatically deposited"),
                None,
            )
            .await?;
            "deposited"
        }
        (Some(_), None) => {
            // Registered here, manual claim.
            set_available(&mut tx, etransfer_id).await?;
            notify(
                &mut tx,
                etransfer_id,
                &recipient_handle,
                "incoming_transfer",
                &format!("You have an Interac e-Transfer of ${amount}"),
                Some(&claim_token),
            )
            .await?;
            "available"
        }
        (None, _) => {
            // External recipient — the network (simulator) settles later (Task 13).
            set_available(&mut tx, etransfer_id).await?;
            notify(
                &mut tx,
                etransfer_id,
                &recipient_handle,
                "incoming_transfer",
                &format!("You have an Interac e-Transfer of ${amount}"),
                Some(&claim_token),
            )
            .await?;
            "available"
        }
    };

    tx.commit().await?;
    tracing::info!(%etransfer_id, status, "📨 e-Transfer sent");
    Ok((
        StatusCode::CREATED,
        Json(load_etransfer(&state, etransfer_id).await?),
    ))
}

async fn set_available(tx: &mut crate::rails::PgTx<'_>, id: Uuid) -> Result<(), AppError> {
    sqlx::query("UPDATE interac_etransfers SET status='available', notified_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
        .bind(id).execute(&mut **tx).await?;
    Ok(())
}

// `zero_available` / `recompute_available` (customer accounts only) are shared
// across rails in `rails::common`.

async fn mark_deposited(
    tx: &mut crate::rails::PgTx<'_>,
    id: Uuid,
    account: Uuid,
) -> Result<(), AppError> {
    sqlx::query("UPDATE interac_etransfers SET status='deposited', recipient_account_id=$2, resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
        .bind(id).bind(account).execute(&mut **tx).await?;
    Ok(())
}

async fn notify(
    tx: &mut crate::rails::PgTx<'_>,
    etransfer_id: Uuid,
    handle: &str,
    kind: &str,
    message: &str,
    claim_token: Option<&str>,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO interac_notifications (etransfer_id, handle_value, kind, message, claim_token) \
         VALUES ($1,$2,$3::interac_notification_kind,$4,$5)",
    )
    .bind(etransfer_id)
    .bind(handle)
    .bind(kind)
    .bind(message)
    .bind(claim_token)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn idempotency_conflict(e: sqlx::Error) -> AppError {
    match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            AppError::Conflict("idempotency_key already used with different parameters".into())
        }
        _ => AppError::from(e),
    }
}

async fn load_etransfer(state: &AppState, id: Uuid) -> Result<EtransferResponse, AppError> {
    let r = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            Decimal,
            String,
            Option<String>,
            Option<String>,
            Option<chrono::DateTime<chrono::Utc>>,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        "SELECT etransfer_id, direction::text, status::text, amount, recipient_handle_value, \
         security_question, memo, expires_at, created_at FROM interac_etransfers WHERE etransfer_id=$1",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await?;
    Ok(EtransferResponse {
        etransfer_id: r.0,
        direction: r.1,
        status: r.2,
        amount: r.3,
        recipient_handle_value: r.4,
        security_question: r.5,
        memo: r.6,
        expires_at: r.7,
        created_at: r.8,
    })
}

async fn load_etransfer_by_key(
    state: &AppState,
    sender: Uuid,
    key: &str,
) -> Result<Option<EtransferResponse>, AppError> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT etransfer_id FROM interac_etransfers WHERE sender_customer_id=$1 AND idempotency_key=$2",
    )
    .bind(sender)
    .bind(key)
    .fetch_optional(&state.pool)
    .await?;
    match id {
        Some(i) => Ok(Some(load_etransfer(state, i).await?)),
        None => Ok(None),
    }
}
async fn list_etransfers(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<EtransferResponse>>, AppError> {
    let status = params.get("status").cloned();
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            Decimal,
            String,
            Option<String>,
            Option<String>,
            Option<chrono::DateTime<chrono::Utc>>,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        "SELECT etransfer_id, direction::text, status::text, amount, recipient_handle_value, \
         security_question, memo, expires_at, created_at FROM interac_etransfers \
         WHERE (sender_customer_id=$1 OR recipient_customer_id=$1) \
           AND ($2::text IS NULL OR status::text=$2) \
         ORDER BY created_at DESC LIMIT 100",
    )
    .bind(caller.customer_id)
    .bind(&status)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| EtransferResponse {
                etransfer_id: r.0,
                direction: r.1,
                status: r.2,
                amount: r.3,
                recipient_handle_value: r.4,
                security_question: r.5,
                memo: r.6,
                expires_at: r.7,
                created_at: r.8,
            })
            .collect(),
    ))
}

async fn get_etransfer(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Path(id): Path<Uuid>,
) -> Result<Json<EtransferResponse>, AppError> {
    let visible: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM interac_etransfers WHERE etransfer_id=$1 \
         AND (sender_customer_id=$2 OR recipient_customer_id=$2))",
    )
    .bind(id)
    .bind(caller.customer_id)
    .fetch_one(&state.pool)
    .await?;
    if !visible {
        return Err(AppError::NotFound("e-Transfer not found".into()));
    }
    Ok(Json(load_etransfer(&state, id).await?))
}

/// Lock an available e-Transfer FOR UPDATE and return the fields we need, or the
/// right error (404 unknown, 409 if no longer 'available').
async fn lock_available(
    tx: &mut crate::rails::PgTx<'_>,
    id: Uuid,
) -> Result<
    (
        Decimal,
        Option<Uuid>,
        Option<Uuid>,
        Option<String>,
        i32,
        String,
    ),
    AppError,
> {
    let row = sqlx::query_as::<_, (String, Decimal, Option<Uuid>, Option<Uuid>, Option<String>, i32, String)>(
        "SELECT status::text, amount, sender_account_id, recipient_customer_id, \
         security_answer_hash, wrong_answer_attempts, \
         COALESCE((SELECT reference_number FROM transactions WHERE transaction_id=hold_transaction_id),'') \
         FROM interac_etransfers WHERE etransfer_id=$1 FOR UPDATE",
    )
    .bind(id).fetch_optional(&mut **tx).await?
    .ok_or_else(|| AppError::NotFound("e-Transfer not found".into()))?;
    if row.0 != "available" {
        return Err(AppError::Conflict(format!("e-Transfer is {}", row.0)));
    }
    // sender_account_id is None for inbound-held transfers (the hold sits on the
    // rail's SETTLEMENT account); callers that credit it back map None accordingly.
    Ok((row.1, row.2, row.3, row.4, row.5, row.6))
}

async fn claim_etransfer(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Path(id): Path<Uuid>,
    AxumJson(req): AxumJson<ClaimEtransferRequest>,
) -> Result<(StatusCode, Json<EtransferResponse>), AppError> {
    let rail = resolve_interac(&state).await?;
    let mut tx = state.pool.begin().await?;
    let (amount, sender_account, _rcpt, answer_hash, attempts, hold_ref) =
        lock_available(&mut tx, id).await?;

    // The deposit account must belong to the caller.
    let owns: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE account_id=$1 AND customer_id=$2)",
    )
    .bind(req.deposit_account_id)
    .bind(caller.customer_id)
    .fetch_one(&mut *tx)
    .await?;
    if !owns {
        return Err(AppError::NotFound("deposit account not found".into()));
    }

    // Verify the security answer (case-insensitive), 3-strike lock.
    if let Some(hash) = &answer_hash {
        if !verify_password(&req.security_answer.to_lowercase(), hash)? {
            let n = attempts + 1;
            if n >= 3 {
                sqlx::query("UPDATE interac_etransfers SET status='failed', wrong_answer_attempts=$2, resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
                    .bind(id).bind(n).execute(&mut *tx).await?;
                tx.commit().await?;
                return Err(AppError::Authorization(
                    "too many incorrect answers; e-Transfer locked".into(),
                ));
            }
            sqlx::query(
                "UPDATE interac_etransfers SET wrong_answer_attempts=$2 WHERE etransfer_id=$1",
            )
            .bind(id)
            .bind(n)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Err(AppError::BadRequest("incorrect security answer".into()));
        }
    }

    // `release` moves clearing→deposit and ignores `from_account`; supply the
    // settlement account as a harmless placeholder for inbound-held claims (None).
    let hold = crate::rails::Hold {
        from_account: sender_account.unwrap_or(rail.accounts.settlement_id),
        amount,
        reference: hold_ref,
        transaction_id: Uuid::nil(),
    };
    rail.release(
        &state,
        &mut tx,
        &hold,
        crate::rails::Destination::Internal(req.deposit_account_id),
        "Interac e-Transfer claim",
    )
    .await?;
    // Recipient was just credited; refresh its available_balance too.
    recompute_available(&mut tx, req.deposit_account_id).await?;
    mark_deposited(&mut tx, id, req.deposit_account_id).await?;
    // Record the claimant as the recipient so they keep the receipt (get/list
    // visibility). Guarded to NULL so we only fill the external case and never
    // overwrite an inbound-held/registered transfer that already has a recipient.
    sqlx::query(
        "UPDATE interac_etransfers SET recipient_customer_id=$2 \
         WHERE etransfer_id=$1 AND recipient_customer_id IS NULL",
    )
    .bind(id)
    .bind(caller.customer_id)
    .execute(&mut *tx)
    .await?;
    let handle: String = sqlx::query_scalar(
        "SELECT recipient_handle_value FROM interac_etransfers WHERE etransfer_id=$1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    notify(
        &mut tx,
        id,
        &handle,
        "deposit_completed",
        &format!("${amount} deposited"),
        None,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(load_etransfer(&state, id).await?)))
}

async fn decline_etransfer(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<EtransferResponse>), AppError> {
    let rail = resolve_interac(&state).await?;
    let mut tx = state.pool.begin().await?;

    // Ownership: only the recipient may decline (mirrors cancel's sender check).
    // Registered manual-claim and inbound-held transfers both carry
    // recipient_customer_id; external-routed transfers leave it NULL, so a
    // non-recipient (or the wrong customer) gets 404, not 403.
    let recipient: Option<Uuid> = sqlx::query_scalar(
        "SELECT recipient_customer_id FROM interac_etransfers WHERE etransfer_id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("e-Transfer not found".into()))?;
    if recipient != Some(caller.customer_id) {
        return Err(AppError::NotFound("e-Transfer not found".into())); // 404, not 403
    }

    let (amount, sender_account, _r, _h, _a, hold_ref) = lock_available(&mut tx, id).await?;
    // Inbound held transfers have no sender_account_id (None); those were held
    // from the rail's SETTLEMENT account (network → clearing), so that's where
    // the refund must be credited back (mirrors sweep_expired).
    let sender_account = sender_account.unwrap_or(rail.accounts.settlement_id);
    let hold = crate::rails::Hold {
        from_account: sender_account,
        amount,
        reference: hold_ref,
        transaction_id: Uuid::nil(),
    };
    rail.refund(&state, &mut tx, &hold, "Interac e-Transfer declined")
        .await?;
    // Sender was just credited back (refund); refresh its available_balance too —
    // but only for a real CUSTOMER account. For inbound declines the "sender" is
    // remapped above to the rail's SETTLEMENT system account, which must keep
    // available_balance pinned at 0 (see rails/interac.rs invariant); recomputing
    // it here would pin it to balance+overdraft and later trip
    // chk_available_balance_logical when settlement is next debited.
    if sender_account != rail.accounts.settlement_id && sender_account != rail.accounts.clearing_id
    {
        recompute_available(&mut tx, sender_account).await?;
    }
    sqlx::query("UPDATE interac_etransfers SET status='declined', resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
        .bind(id).execute(&mut *tx).await?;
    let handle: String = sqlx::query_scalar(
        "SELECT recipient_handle_value FROM interac_etransfers WHERE etransfer_id=$1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    notify(
        &mut tx,
        id,
        &handle,
        "declined",
        &format!("${amount} was declined and returned"),
        None,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(load_etransfer(&state, id).await?)))
}

async fn cancel_etransfer(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<EtransferResponse>), AppError> {
    let rail = resolve_interac(&state).await?;
    let mut tx = state.pool.begin().await?;

    // Ownership check folded into the lock: only the sender may cancel.
    let sender: Option<Uuid> = sqlx::query_scalar(
        "SELECT sender_customer_id FROM interac_etransfers WHERE etransfer_id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("e-Transfer not found".into()))?;
    if sender != Some(caller.customer_id) {
        return Err(AppError::NotFound("e-Transfer not found".into())); // 404, not 403
    }
    let (amount, sender_account, _r, _h, _a, hold_ref) = lock_available(&mut tx, id).await?;
    // Cancel is outbound-only, so sender_account is always Some; fall back to
    // settlement defensively.
    let sender_account = sender_account.unwrap_or(rail.accounts.settlement_id);
    let hold = crate::rails::Hold {
        from_account: sender_account,
        amount,
        reference: hold_ref,
        transaction_id: Uuid::nil(),
    };
    rail.refund(&state, &mut tx, &hold, "Interac e-Transfer cancelled")
        .await?;
    // Sender was just credited back (refund); refresh its available_balance too.
    recompute_available(&mut tx, sender_account).await?;
    sqlx::query("UPDATE interac_etransfers SET status='cancelled', resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
        .bind(id).execute(&mut *tx).await?;
    let handle: String = sqlx::query_scalar(
        "SELECT recipient_handle_value FROM interac_etransfers WHERE etransfer_id=$1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    notify(
        &mut tx,
        id,
        &handle,
        "cancelled",
        &format!("${amount} transfer was cancelled"),
        None,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(load_etransfer(&state, id).await?)))
}
async fn register_autodeposit(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    AxumJson(req): AxumJson<RegisterAutodepositRequest>,
) -> Result<(StatusCode, Json<HandleResponse>), AppError> {
    req.validate()?;
    let handle = normalize_handle(req.handle_type, &req.handle_value);

    // The deposit account must belong to the caller.
    let owns: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE account_id=$1 AND customer_id=$2)",
    )
    .bind(req.deposit_account_id)
    .bind(caller.customer_id)
    .fetch_one(&state.pool)
    .await?;
    if !owns {
        return Err(AppError::NotFound("deposit account not found".into()));
    }

    let row = sqlx::query_as::<_, (Uuid, Option<Uuid>, bool)>(
        r#"
        INSERT INTO interac_handles (customer_id, handle_type, handle_value, autodeposit_account_id)
        VALUES ($1, $2, $3, $4)
        RETURNING handle_id, autodeposit_account_id, active
        "#,
    )
    .bind(caller.customer_id)
    .bind(req.handle_type)
    .bind(&handle)
    .bind(req.deposit_account_id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            AppError::Conflict("handle already registered".into())
        }
        _ => AppError::from(e),
    })?;

    Ok((
        StatusCode::CREATED,
        Json(HandleResponse {
            handle_id: row.0,
            handle_type: req.handle_type,
            handle_value: handle,
            autodeposit_account_id: row.1,
            active: row.2,
        }),
    ))
}

async fn list_autodeposit(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
) -> Result<Json<Vec<HandleResponse>>, AppError> {
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            crate::models::interac::HandleType,
            String,
            Option<Uuid>,
            bool,
        ),
    >(
        "SELECT handle_id, handle_type, handle_value, autodeposit_account_id, active \
         FROM interac_handles WHERE customer_id=$1 ORDER BY created_at",
    )
    .bind(caller.customer_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, ht, hv, ad, active)| HandleResponse {
                handle_id: id,
                handle_type: ht,
                handle_value: hv,
                autodeposit_account_id: ad,
                active,
            })
            .collect(),
    ))
}

async fn deregister_autodeposit(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Path(handle_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let n = sqlx::query("DELETE FROM interac_handles WHERE handle_id=$1 AND customer_id=$2")
        .bind(handle_id)
        .bind(caller.customer_id)
        .execute(&state.pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound("handle not found".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn network_inbound(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
    AxumJson(req): AxumJson<InboundEtransferRequest>,
) -> Result<(StatusCode, Json<EtransferResponse>), AppError> {
    let amount = normalize_amount(req.amount)?;
    let handle = normalize_handle(req.recipient_handle_type, &req.recipient_handle_value);
    let rail = resolve_interac(&state).await?;

    // The recipient must be a known nano-bank handle.
    let reg = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
        "SELECT customer_id, autodeposit_account_id FROM interac_handles WHERE handle_value=$1 AND active=TRUE")
        .bind(&handle).fetch_optional(&state.pool).await?
        .ok_or_else(|| AppError::NotFound("recipient handle not registered at this institution".into()))?;

    let answer_hash = match &req.security_answer {
        Some(a) => Some(hash_password(&a.to_lowercase())?),
        None => None,
    };
    let claim_token = crate::handlers::cards::reference_number("CLM");

    let mut tx = state.pool.begin().await?;

    if let Some(deposit_acct) = reg.1 {
        // Autodeposit fast path.
        let posting = rail
            .accept_inbound(
                &state,
                &mut tx,
                deposit_acct,
                amount,
                &format!("Interac e-Transfer from {}", req.sender_name),
            )
            .await?;
        let id = insert_inbound(
            &mut tx,
            amount,
            &req,
            &handle,
            reg.0,
            &claim_token,
            None,
            Some(deposit_acct),
            Some(posting.transaction_id),
            "deposited",
            expiry_days(&state),
        )
        .await?;
        recompute_available(&mut tx, deposit_acct).await?;
        notify(
            &mut tx,
            id,
            &handle,
            "deposit_completed",
            &format!("${amount} auto-deposited"),
            None,
        )
        .await?;
        tx.commit().await?;
        return Ok((StatusCode::CREATED, Json(load_etransfer(&state, id).await?)));
    }

    // Held path: money arrives from the network into clearing (from = SETTLEMENT).
    let hold = rail
        .hold(
            &state,
            &mut tx,
            rail.accounts.settlement_id,
            amount,
            &format!("Interac inbound e-Transfer from {}", req.sender_name),
        )
        .await?;
    let id = insert_inbound(
        &mut tx,
        amount,
        &req,
        &handle,
        reg.0,
        &claim_token,
        answer_hash,
        None,
        Some(hold.transaction_id),
        "available",
        expiry_days(&state),
    )
    .await?;
    notify(
        &mut tx,
        id,
        &handle,
        "incoming_transfer",
        &format!(
            "You have an Interac e-Transfer of ${amount} from {}",
            req.sender_name
        ),
        Some(&claim_token),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(load_etransfer(&state, id).await?)))
}

#[allow(clippy::too_many_arguments)]
async fn insert_inbound(
    tx: &mut crate::rails::PgTx<'_>,
    amount: Decimal,
    req: &InboundEtransferRequest,
    handle: &str,
    recipient_customer: Uuid,
    claim_token: &str,
    answer_hash: Option<String>,
    recipient_account: Option<Uuid>,
    hold_txn: Option<Uuid>,
    status: &str,
    expiry_days: i64,
) -> Result<Uuid, AppError> {
    let id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO interac_etransfers
            (direction, status, amount, sender_name, recipient_handle_type, recipient_handle_value,
             recipient_customer_id, recipient_account_id, counterparty_institution,
             security_question, security_answer_hash, claim_token, memo, hold_transaction_id, expires_at)
        VALUES ('inbound',$1::interac_status,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,
                CURRENT_TIMESTAMP + ($14 || ' days')::interval)
        RETURNING etransfer_id
        "#,
    )
    .bind(status).bind(amount).bind(&req.sender_name).bind(req.recipient_handle_type)
    .bind(handle).bind(recipient_customer).bind(recipient_account).bind(&req.counterparty_institution)
    .bind(&req.security_question).bind(&answer_hash).bind(claim_token).bind(&req.memo).bind(hold_txn)
    .bind(expiry_days.to_string())
    .fetch_one(&mut **tx).await?;
    Ok(id)
}

async fn network_settle(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
    Path(id): Path<Uuid>,
    AxumJson(req): AxumJson<SettleEtransferRequest>,
) -> Result<(StatusCode, Json<EtransferResponse>), AppError> {
    let rail = resolve_interac(&state).await?;
    let mut tx = state.pool.begin().await?;

    // Must be an outbound, external, still-available transfer.
    let row = sqlx::query_as::<_, (String, String, Decimal, Option<Uuid>, Option<Uuid>, String)>(
        "SELECT status::text, direction::text, amount, sender_account_id, recipient_customer_id, \
         COALESCE((SELECT reference_number FROM transactions WHERE transaction_id=hold_transaction_id),'') \
         FROM interac_etransfers WHERE etransfer_id=$1 FOR UPDATE")
        .bind(id).fetch_optional(&mut *tx).await?
        .ok_or_else(|| AppError::NotFound("e-Transfer not found".into()))?;
    if row.0 != "available" {
        return Err(AppError::Conflict(format!("e-Transfer is {}", row.0)));
    }
    if row.1 != "outbound" || row.4.is_some() {
        return Err(AppError::BadRequest(
            "not an external outbound transfer".into(),
        ));
    }
    let hold = crate::rails::Hold {
        from_account: row
            .3
            .ok_or_else(|| AppError::Internal("missing sender account".into()))?,
        amount: row.2,
        reference: row.5,
        transaction_id: Uuid::nil(),
    };

    let (new_status, handle_kind, msg) = match req.outcome.as_str() {
        "claimed" => {
            rail.release(
                &state,
                &mut tx,
                &hold,
                crate::rails::Destination::External(req.institution.clone()),
                "Interac e-Transfer settled to external bank",
            )
            .await?;
            sqlx::query("UPDATE interac_etransfers SET status='deposited', counterparty_institution=$2, resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
                .bind(id).bind(&req.institution).execute(&mut *tx).await?;
            (
                "deposited",
                "deposit_completed",
                "deposited at the recipient's bank",
            )
        }
        "declined" => {
            rail.refund(
                &state,
                &mut tx,
                &hold,
                "Interac e-Transfer declined by network",
            )
            .await?;
            sqlx::query("UPDATE interac_etransfers SET status='declined', resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
                .bind(id).execute(&mut *tx).await?;
            // Sender was just credited back (refund); refresh its available_balance too.
            recompute_available(&mut tx, hold.from_account).await?;
            ("declined", "declined", "declined and returned")
        }
        other => return Err(AppError::BadRequest(format!("unknown outcome '{other}'"))),
    };

    let handle: String = sqlx::query_scalar(
        "SELECT recipient_handle_value FROM interac_etransfers WHERE etransfer_id=$1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    notify(
        &mut tx,
        id,
        &handle,
        handle_kind,
        &format!("Your e-Transfer was {msg}"),
        None,
    )
    .await?;
    let _ = new_status;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(load_etransfer(&state, id).await?)))
}
async fn sweep_expired(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
) -> Result<Json<serde_json::Value>, AppError> {
    let rail = resolve_interac(&state).await?;
    // Snapshot the due ids first (short read), then process each in its own tx so
    // one bad row can't roll back the batch.
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT etransfer_id FROM interac_etransfers \
         WHERE status='available' AND expires_at < CURRENT_TIMESTAMP",
    )
    .fetch_all(&state.pool)
    .await?;

    let mut expired = 0i64;
    for id in due {
        let mut tx = state.pool.begin().await?;
        // Re-lock + re-check (a concurrent claim may have won).
        let guard = lock_available(&mut tx, id).await;
        let (amount, from_account, _r, _h, _a, hold_ref) = match guard {
            Ok(v) => v,
            Err(_) => {
                tx.rollback().await?;
                continue;
            }
        };
        // Inbound held transfers have no sender_account_id (None); those were
        // held from the rail's SETTLEMENT account (network → clearing), so
        // that's where the refund must be credited back.
        let from_account = from_account.unwrap_or(rail.accounts.settlement_id);
        let hold = crate::rails::Hold {
            from_account,
            amount,
            reference: hold_ref,
            transaction_id: Uuid::nil(),
        };
        rail.refund(&state, &mut tx, &hold, "Interac e-Transfer expired")
            .await?;
        // Only recompute for a real CUSTOMER account; skip for the rail's own
        // SETTLEMENT/CLEARING accounts (inbound held transfers), whose
        // available_balance must stay pinned at 0 (see decline_etransfer above
        // for the full rationale).
        if from_account != rail.accounts.settlement_id && from_account != rail.accounts.clearing_id
        {
            recompute_available(&mut tx, from_account).await?;
        }
        sqlx::query("UPDATE interac_etransfers SET status='expired', resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
            .bind(id).execute(&mut *tx).await?;
        let handle: String = sqlx::query_scalar(
            "SELECT recipient_handle_value FROM interac_etransfers WHERE etransfer_id=$1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        notify(
            &mut tx,
            id,
            &handle,
            "expired",
            &format!("${amount} expired and was returned"),
            None,
        )
        .await?;
        tx.commit().await?;
        expired += 1;
    }
    Ok(Json(serde_json::json!({ "expired": expired })))
}
