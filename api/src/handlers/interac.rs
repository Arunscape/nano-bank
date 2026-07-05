//! Interac e-Transfer product lifecycle. Money movement goes through the Rail
//! port (`rails::interac::InteracRail`); this module owns handle resolution,
//! the claim/decline/cancel/expiry state machine, and notifications.

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
    ClaimEtransferRequest, EtransferResponse, HandleResponse, RegisterAutodepositRequest,
    SendEtransferRequest,
};
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

async fn send_etransfer(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    AxumJson(req): AxumJson<SendEtransferRequest>,
) -> Result<(StatusCode, Json<EtransferResponse>), AppError> {
    req.validate()?;
    let amount = normalize_amount(req.amount)?;
    if amount > max_amount() {
        return Err(AppError::BadRequest(format!(
            "amount exceeds per-transfer max {}",
            max_amount()
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
    .bind(expiry_days().to_string())
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

/// Zero a deposit account's `available_balance` ahead of a debit leg so the
/// balance trigger can't transiently violate `chk_available_balance_logical`.
async fn zero_available(tx: &mut crate::rails::PgTx<'_>, account_id: Uuid) -> Result<(), AppError> {
    sqlx::query("UPDATE accounts SET available_balance = 0 WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Recompute a deposit account's available balance: `balance + overdraft − open holds`.
async fn recompute_available(
    tx: &mut crate::rails::PgTx<'_>,
    account_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE accounts SET available_balance = balance + overdraft_limit \
         - COALESCE((SELECT sum(amount) FROM account_holds \
                     WHERE account_id=$1 AND released_at IS NULL), 0), \
         updated_at = CURRENT_TIMESTAMP WHERE account_id = $1",
    )
    .bind(account_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

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
async fn list_etransfers() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
async fn get_etransfer() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }

/// Lock an available e-Transfer FOR UPDATE and return the fields we need, or the
/// right error (404 unknown, 409 if no longer 'available').
async fn lock_available(
    tx: &mut crate::rails::PgTx<'_>,
    id: Uuid,
) -> Result<(Decimal, Uuid, Option<Uuid>, Option<String>, i32, String), AppError> {
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
    Ok((row.1, row.2.unwrap_or_default(), row.3, row.4, row.5, row.6))
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
            sqlx::query("UPDATE interac_etransfers SET wrong_answer_attempts=$2 WHERE etransfer_id=$1")
                .bind(id).bind(n).execute(&mut *tx).await?;
            tx.commit().await?;
            return Err(AppError::BadRequest("incorrect security answer".into()));
        }
    }

    let hold = crate::rails::Hold {
        from_account: sender_account,
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
    let handle: String =
        sqlx::query_scalar("SELECT recipient_handle_value FROM interac_etransfers WHERE etransfer_id=$1")
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
    _caller: AuthenticatedCustomer,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<EtransferResponse>), AppError> {
    let rail = resolve_interac(&state).await?;
    let mut tx = state.pool.begin().await?;
    let (amount, sender_account, _r, _h, _a, hold_ref) = lock_available(&mut tx, id).await?;
    let hold = crate::rails::Hold {
        from_account: sender_account,
        amount,
        reference: hold_ref,
        transaction_id: Uuid::nil(),
    };
    rail.refund(&state, &mut tx, &hold, "Interac e-Transfer declined").await?;
    // Sender was just credited back (refund); refresh its available_balance too.
    recompute_available(&mut tx, sender_account).await?;
    sqlx::query("UPDATE interac_etransfers SET status='declined', resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
        .bind(id).execute(&mut *tx).await?;
    let handle: String =
        sqlx::query_scalar("SELECT recipient_handle_value FROM interac_etransfers WHERE etransfer_id=$1")
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

async fn cancel_etransfer() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
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
    let rows = sqlx::query_as::<_, (Uuid, crate::models::interac::HandleType, String, Option<Uuid>, bool)>(
        "SELECT handle_id, handle_type, handle_value, autodeposit_account_id, active \
         FROM interac_handles WHERE customer_id=$1 ORDER BY created_at",
    )
    .bind(caller.customer_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, ht, hv, ad, active)| HandleResponse {
                handle_id: id, handle_type: ht, handle_value: hv,
                autodeposit_account_id: ad, active,
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
async fn network_inbound() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
async fn network_settle() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
async fn sweep_expired() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
