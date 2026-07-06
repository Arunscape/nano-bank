//! Lynx RTGS high-value wire rail — product lifecycle. Money movement goes
//! through the Rail port (`rails::lynx::LynxRail`); this module owns the wire
//! lifecycle (two-step send→settle with finality, inbound credit, recall in
//! both directions, the stale-wire sweep) and the ISO 20022 messaging.
//!
//! Three auth planes: customer (`/wires`, `/wires/:id/recall`), service-token
//! network (`/network/*`), service-token admin (`/admin/*`).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use rust_decimal::Decimal;
use uuid::Uuid;
use validator::Validate;

use crate::errors::AppError;
use crate::handlers::cards::{fetch_account_for_update, normalize_amount};
use crate::handlers::AppState;
use crate::lynx::iso20022::{self, Camt029, Camt056, CreditTransfer};
use crate::middleware::auth::{AuthenticatedCustomer, AuthenticatedService};
use crate::models::lynx::{
    InboundRecallRequest, InitiateWireRequest, NetworkInboundRequest, RecallRequest,
    RecallResolveRequest, WireResponse,
};
use crate::rails::lynx::{ensure_lynx_accounts, LynxRail};
use crate::rails::{Destination, Hold, PgTx, Rail};

/// nano-bank's own institution number (see `07_rails.sql`).
const SELF_INSTITUTION: &str = "900";

pub fn lynx_routes() -> Router<AppState> {
    Router::new()
        // customer plane
        .route("/wires", post(initiate_wire).get(list_wires))
        .route("/wires/:id", get(get_wire))
        .route("/wires/:id/recall", post(request_recall))
        // network plane (service token)
        .route("/network/wires/:id/settle", post(network_settle))
        .route("/network/inbound", post(network_inbound))
        .route("/network/recalls/:id/resolve", post(network_recall_resolve))
        .route("/network/inbound-recall", post(network_inbound_recall))
        // admin plane (service token)
        .route("/admin/reject-stale", post(admin_reject_stale))
}

async fn resolve_lynx(state: &AppState) -> Result<LynxRail, AppError> {
    Ok(LynxRail::new(ensure_lynx_accounts(&state.pool).await?))
}

/// Minimum wire amount (high-value floor). Configurable; default $10,000.
fn min_amount() -> Decimal {
    std::env::var("NANO_BANK__LYNX__MIN_AMOUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| Decimal::new(1000000, 2))
}

/// How old (minutes) a `sent` wire must be before the admin sweep rejects it.
fn stale_minutes() -> i32 {
    std::env::var("NANO_BANK__LYNX__STALE_MINUTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60)
}

// --- available_balance helpers (customer accounts ONLY; never the system
// clearing/settlement accounts) — same rule as Interac/AFT. ---

async fn zero_available(tx: &mut PgTx<'_>, account_id: Uuid) -> Result<(), AppError> {
    sqlx::query("UPDATE accounts SET available_balance = 0 WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn recompute_available(tx: &mut PgTx<'_>, account_id: Uuid) -> Result<(), AppError> {
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

async fn caller_owns_account(
    state: &AppState,
    account_id: Uuid,
    customer_id: Uuid,
) -> Result<bool, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE account_id=$1 AND customer_id=$2)",
    )
    .bind(account_id)
    .bind(customer_id)
    .fetch_one(&state.pool)
    .await?)
}

/// Load a wire as a `WireResponse`.
async fn load_wire(state: &AppState, wire_id: Uuid) -> Result<WireResponse, AppError> {
    let r = sqlx::query_as::<
        _,
        (
            Uuid,
            Uuid,
            String,
            String,
            Decimal,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
        ),
    >(
        "SELECT wire_id, uetr, direction::text, status::text, amount, currency, \
         counterparty_name, counterparty_institution, message_type, reference_number, gl_entry \
         FROM lynx_wires WHERE wire_id = $1",
    )
    .bind(wire_id)
    .fetch_one(&state.pool)
    .await?;
    Ok(WireResponse {
        wire_id: r.0,
        uetr: r.1,
        direction: r.2,
        status: r.3,
        amount: r.4,
        currency: r.5,
        counterparty_name: r.6,
        counterparty_institution: r.7,
        message_type: r.8,
        reference_number: r.9,
        gl_entry: r.10,
    })
}

/// Store an ISO 20022 message row; return its id.
async fn store_message(
    tx: &mut PgTx<'_>,
    wire_id: Uuid,
    message_type: &str,
    flow: &str,
    payload: &str,
) -> Result<Uuid, AppError> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO lynx_messages (wire_id, message_type, flow, payload) \
         VALUES ($1, $2, $3, $4) RETURNING message_id",
    )
    .bind(wire_id)
    .bind(message_type)
    .bind(flow)
    .bind(payload)
    .fetch_one(&mut **tx)
    .await?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// customer plane
// ---------------------------------------------------------------------------

/// Initiate an outbound wire: reserve funds, emit pacs.008, record `sent`.
async fn initiate_wire(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Json(req): Json<InitiateWireRequest>,
) -> Result<(StatusCode, Json<WireResponse>), AppError> {
    req.validate()?;
    let amount = normalize_amount(req.amount)?;
    if amount < min_amount() {
        return Err(AppError::BadRequest(format!(
            "amount below the high-value floor of {}",
            min_amount()
        )));
    }
    if !caller_owns_account(&state, req.from_account_id, caller.customer_id).await? {
        return Err(AppError::NotFound("account not found".into()));
    }
    // Counterparty institution must be a Lynx-capable, active participant.
    let ok: Option<(bool, bool)> = sqlx::query_as(
        "SELECT supports_lynx, active FROM rail_participants WHERE institution_number = $1",
    )
    .bind(&req.counterparty_institution)
    .fetch_optional(&state.pool)
    .await?;
    match ok {
        Some((true, true)) => {}
        Some(_) => return Err(AppError::BadRequest("institution is not Lynx-capable".into())),
        None => return Err(AppError::BadRequest("unknown counterparty institution".into())),
    }

    let rail = resolve_lynx(&state).await?;
    let mut tx = state.pool.begin().await?;

    let account = fetch_account_for_update(&mut tx, req.from_account_id)
        .await?
        .ok_or_else(|| AppError::NotFound("account not found".into()))?;
    if !matches!(account.status, crate::models::account::AccountStatus::Active) {
        return Err(AppError::BadRequest("account is not active".into()));
    }
    if account.available_balance < amount {
        return Err(AppError::InsufficientFunds);
    }

    zero_available(&mut tx, req.from_account_id).await?;
    let hold = rail
        .hold(&state, &mut tx, req.from_account_id, amount, "Lynx wire")
        .await?;
    recompute_available(&mut tx, req.from_account_id).await?;

    let uetr = Uuid::new_v4();
    let msg = CreditTransfer {
        uetr: uetr.to_string(),
        debtor_name: format!("nano-bank account {}", account.account_number),
        debtor_agent: SELF_INSTITUTION.to_string(),
        debtor_account: account.account_number.clone(),
        creditor_name: req.counterparty_name.clone(),
        creditor_agent: req.counterparty_institution.clone(),
        creditor_account: req.counterparty_account.clone(),
        amount,
        currency: "CAD".into(),
        remittance: req.remittance_info.clone(),
    };
    let payload = iso20022::encode_pacs008(&msg);

    let wire_id: Uuid = sqlx::query_scalar(
        "INSERT INTO lynx_wires \
         (uetr, direction, status, local_account_id, counterparty_name, counterparty_institution, \
          counterparty_account, amount, currency, remittance_info, message_type, \
          settlement_transaction_id, initiated_by, reference_number, sent_at) \
         VALUES ($1,'outbound','sent',$2,$3,$4,$5,$6,'CAD',$7,'pacs.008',$8,$9,$10,CURRENT_TIMESTAMP) \
         RETURNING wire_id",
    )
    .bind(uetr)
    .bind(req.from_account_id)
    .bind(&req.counterparty_name)
    .bind(&req.counterparty_institution)
    .bind(&req.counterparty_account)
    .bind(amount)
    .bind(&req.remittance_info)
    .bind(hold.transaction_id)
    .bind(caller.customer_id)
    .bind(&hold.reference)
    .fetch_one(&mut *tx)
    .await?;

    store_message(&mut tx, wire_id, "pacs.008", "emitted", &payload).await?;
    tx.commit().await?;

    tracing::info!(%wire_id, %uetr, amount = %amount, "🌐 Lynx wire sent");
    Ok((StatusCode::CREATED, Json(load_wire(&state, wire_id).await?)))
}

async fn list_wires(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
) -> Result<Json<Vec<WireResponse>>, AppError> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT wire_id FROM lynx_wires WHERE local_account_id IN \
         (SELECT account_id FROM accounts WHERE customer_id = $1) ORDER BY created_at DESC",
    )
    .bind(caller.customer_id)
    .fetch_all(&state.pool)
    .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(load_wire(&state, id).await?);
    }
    Ok(Json(out))
}

async fn get_wire(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Path(wire_id): Path<Uuid>,
) -> Result<Json<WireResponse>, AppError> {
    let involved: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM lynx_wires w JOIN accounts a ON a.account_id = w.local_account_id \
         WHERE w.wire_id = $1 AND a.customer_id = $2)",
    )
    .bind(wire_id)
    .bind(caller.customer_id)
    .fetch_one(&state.pool)
    .await?;
    if !involved {
        return Err(AppError::NotFound("wire not found".into()));
    }
    Ok(Json(load_wire(&state, wire_id).await?))
}

/// Request recall of a settled outbound wire (initiator-only). Emits camt.056.
async fn request_recall(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Path(wire_id): Path<Uuid>,
    Json(req): Json<RecallRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    req.validate()?;
    let reason = req
        .reason
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| "customer request".into());

    let row: Option<(Uuid, String, String, Option<Uuid>)> = sqlx::query_as(
        "SELECT uetr, direction::text, status::text, initiated_by FROM lynx_wires WHERE wire_id = $1",
    )
    .bind(wire_id)
    .fetch_optional(&state.pool)
    .await?;
    let (uetr, direction, status, initiated_by) =
        row.ok_or_else(|| AppError::NotFound("wire not found".into()))?;
    if initiated_by != Some(caller.customer_id) {
        return Err(AppError::NotFound("wire not found".into()));
    }
    if direction != "outbound" || status != "settled" {
        return Err(AppError::Conflict(
            "only a settled outbound wire can be recalled".into(),
        ));
    }
    let open: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM lynx_recalls WHERE wire_id=$1 AND status='requested')",
    )
    .bind(wire_id)
    .fetch_one(&state.pool)
    .await?;
    if open {
        return Err(AppError::Conflict("a recall is already open".into()));
    }

    let msg = Camt056 {
        uetr: Uuid::new_v4().to_string(),
        original_uetr: uetr.to_string(),
        reason: reason.clone(),
    };
    let payload = iso20022::encode_camt056(&msg);

    let mut tx = state.pool.begin().await?;
    let msg_id = store_message(&mut tx, wire_id, "camt.056", "emitted", &payload).await?;
    let recall_id: Uuid = sqlx::query_scalar(
        "INSERT INTO lynx_recalls (wire_id, direction, requested_by, reason, status, camt056_message_id) \
         VALUES ($1,'outbound',$2,$3,'requested',$4) RETURNING recall_id",
    )
    .bind(wire_id)
    .bind(caller.customer_id)
    .bind(&reason)
    .bind(msg_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "recall_id": recall_id, "status": "requested" })),
    ))
}

// ---------------------------------------------------------------------------
// network plane (service token)
// ---------------------------------------------------------------------------

/// Bank-of-Canada settlement of a `sent` wire → finality.
async fn network_settle(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
    Path(wire_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let rail = resolve_lynx(&state).await?;
    let mut tx = state.pool.begin().await?;

    let marked = sqlx::query(
        "UPDATE lynx_wires SET status='settled', settled_at=CURRENT_TIMESTAMP \
         WHERE wire_id=$1 AND status='sent'",
    )
    .bind(wire_id)
    .execute(&mut *tx)
    .await?;
    if marked.rows_affected() != 1 {
        let cur: Option<String> =
            sqlx::query_scalar("SELECT status::text FROM lynx_wires WHERE wire_id=$1")
                .bind(wire_id)
                .fetch_optional(&mut *tx)
                .await?;
        return match cur {
            None => Err(AppError::NotFound("wire not found".into())),
            Some(s) => Err(AppError::Conflict(format!("wire is {s}"))),
        };
    }

    let (local_account_id, amount, reference, institution): (Uuid, Decimal, String, String) =
        sqlx::query_as(
            "SELECT local_account_id, amount, reference_number, counterparty_institution \
             FROM lynx_wires WHERE wire_id=$1",
        )
        .bind(wire_id)
        .fetch_one(&mut *tx)
        .await?;
    let hold = Hold {
        from_account: local_account_id,
        amount,
        reference,
        transaction_id: Uuid::nil(),
    };
    let posting = rail
        .release(
            &state,
            &mut tx,
            &hold,
            Destination::External(institution),
            "Lynx settlement",
        )
        .await?;
    sqlx::query("UPDATE lynx_wires SET gl_entry=$2 WHERE wire_id=$1")
        .bind(wire_id)
        .bind(&posting.gl_entry)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    tracing::info!(%wire_id, "🌐 Lynx wire settled (final)");
    Ok(Json(serde_json::json!({ "wire_id": wire_id, "status": "settled" })))
}

/// An inbound wire arriving from the network → credit the beneficiary customer.
async fn network_inbound(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
    Json(req): Json<NetworkInboundRequest>,
) -> Result<(StatusCode, Json<WireResponse>), AppError> {
    let amount = normalize_amount(req.amount)?;
    let acct: Option<Uuid> = sqlx::query_scalar(
        "SELECT account_id FROM accounts \
         WHERE institution_number=$1 AND transit_number=$2 AND account_number=$3",
    )
    .bind(&req.beneficiary_institution)
    .bind(&req.beneficiary_transit)
    .bind(&req.beneficiary_account)
    .fetch_optional(&state.pool)
    .await?;
    let acct = acct.ok_or_else(|| AppError::NotFound("beneficiary account not found".into()))?;

    let rail = resolve_lynx(&state).await?;
    let mut tx = state.pool.begin().await?;

    let posting = rail
        .accept_inbound(&state, &mut tx, acct, amount, "Lynx inbound wire")
        .await?;
    recompute_available(&mut tx, acct).await?;

    let uetr = req.uetr.unwrap_or_else(Uuid::new_v4);
    let message_type = req.message_type.as_deref().unwrap_or("pacs.008").to_string();
    let msg = CreditTransfer {
        uetr: uetr.to_string(),
        debtor_name: req.debtor_name.clone(),
        debtor_agent: req.debtor_institution.clone(),
        debtor_account: req.debtor_account.clone(),
        creditor_name: format!("account {}", req.beneficiary_account),
        creditor_agent: req.beneficiary_institution.clone(),
        creditor_account: req.beneficiary_account.clone(),
        amount,
        currency: "CAD".into(),
        remittance: req.remittance_info.clone(),
    };
    let payload = if message_type == "pacs.009" {
        iso20022::encode_pacs009(&msg)
    } else {
        iso20022::encode_pacs008(&msg)
    };

    let wire_id: Uuid = sqlx::query_scalar(
        "INSERT INTO lynx_wires \
         (uetr, direction, status, local_account_id, counterparty_name, counterparty_institution, \
          counterparty_account, amount, currency, remittance_info, message_type, \
          settlement_transaction_id, gl_entry, reference_number, sent_at, settled_at) \
         VALUES ($1,'inbound','settled',$2,$3,$4,$5,$6,'CAD',$7,$8,$9,$10,$11,CURRENT_TIMESTAMP,CURRENT_TIMESTAMP) \
         RETURNING wire_id",
    )
    .bind(uetr)
    .bind(acct)
    .bind(&req.debtor_name)
    .bind(&req.debtor_institution)
    .bind(&req.debtor_account)
    .bind(amount)
    .bind(&req.remittance_info)
    .bind(&message_type)
    .bind(posting.transaction_id)
    .bind(&posting.gl_entry)
    .bind(format!("LYNXIN-{}", &uetr.to_string()[..8]))
    .fetch_one(&mut *tx)
    .await?;
    store_message(&mut tx, wire_id, &message_type, "received", &payload).await?;
    tx.commit().await?;

    tracing::info!(%wire_id, %uetr, amount = %amount, "🌐 Lynx inbound wire credited");
    Ok((StatusCode::CREATED, Json(load_wire(&state, wire_id).await?)))
}

/// The beneficiary FI's camt.029 answer to our outbound recall.
async fn network_recall_resolve(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
    Path(recall_id): Path<Uuid>,
    Json(req): Json<RecallResolveRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let accept = req.decision == "accept";
    let new_status = if accept { "accepted" } else { "rejected" };
    let rail = resolve_lynx(&state).await?;
    let mut tx = state.pool.begin().await?;

    let marked = sqlx::query(
        "UPDATE lynx_recalls SET status=$2::lynx_recall_status, resolution_reason=$3, \
         resolved_at=CURRENT_TIMESTAMP WHERE recall_id=$1 AND status='requested'",
    )
    .bind(recall_id)
    .bind(new_status)
    .bind(&req.reason)
    .execute(&mut *tx)
    .await?;
    if marked.rows_affected() != 1 {
        return Err(AppError::Conflict("recall is not open".into()));
    }

    let (wire_id, uetr, local_account_id, amount): (Uuid, Uuid, Uuid, Decimal) = sqlx::query_as(
        "SELECT w.wire_id, w.uetr, w.local_account_id, w.amount \
         FROM lynx_recalls r JOIN lynx_wires w ON w.wire_id=r.wire_id WHERE r.recall_id=$1",
    )
    .bind(recall_id)
    .fetch_one(&mut *tx)
    .await?;

    let msg = Camt029 {
        uetr: Uuid::new_v4().to_string(),
        original_uetr: uetr.to_string(),
        status: if accept { "ACCP".into() } else { "RJCR".into() },
        reason: req.reason.clone(),
    };
    let payload = iso20022::encode_camt029(&msg);
    let msg_id = store_message(&mut tx, wire_id, "camt.029", "received", &payload).await?;
    sqlx::query("UPDATE lynx_recalls SET camt029_message_id=$2 WHERE recall_id=$1")
        .bind(recall_id)
        .bind(msg_id)
        .execute(&mut *tx)
        .await?;

    let wire_status = if accept {
        rail.accept_inbound(&state, &mut tx, local_account_id, amount, "Lynx recall refund")
            .await?;
        recompute_available(&mut tx, local_account_id).await?;
        sqlx::query("UPDATE lynx_wires SET status='recalled' WHERE wire_id=$1")
            .bind(wire_id)
            .execute(&mut *tx)
            .await?;
        "recalled"
    } else {
        "settled"
    };
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "recall_id": recall_id, "status": new_status, "wire_status": wire_status
    })))
}

/// An external sender's camt.056 for a wire we received; we answer camt.029.
async fn network_inbound_recall(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
    Json(req): Json<InboundRecallRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let rail = resolve_lynx(&state).await?;
    let mut tx = state.pool.begin().await?;

    let (uetr, direction, status, local_account_id, amount): (Uuid, String, String, Uuid, Decimal) =
        sqlx::query_as(
            "SELECT uetr, direction::text, status::text, local_account_id, amount \
             FROM lynx_wires WHERE wire_id=$1 FOR UPDATE",
        )
        .bind(req.wire_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::NotFound("wire not found".into()))?;
    if direction != "inbound" || status != "settled" {
        return Err(AppError::Conflict(
            "only a settled inbound wire can be recalled".into(),
        ));
    }

    let camt056 = Camt056 {
        uetr: Uuid::new_v4().to_string(),
        original_uetr: uetr.to_string(),
        reason: req.reason.clone().unwrap_or_else(|| "sender request".into()),
    };
    let msg056 = store_message(
        &mut tx,
        req.wire_id,
        "camt.056",
        "received",
        &iso20022::encode_camt056(&camt056),
    )
    .await?;

    // Decide accept vs reject (reject if the beneficiary can't cover the clawback).
    let (recall_status, camt029_status, resolution): (&str, &str, String) = if req.decision
        == "accept"
    {
        let acct = fetch_account_for_update(&mut tx, local_account_id)
            .await?
            .ok_or_else(|| AppError::NotFound("beneficiary account gone".into()))?;
        if acct.available_balance < amount {
            ("rejected", "RJCR", "insufficient funds".into())
        } else {
            zero_available(&mut tx, local_account_id).await?;
            rail.clawback(&state, &mut tx, local_account_id, amount, "Lynx inbound recall")
                .await?;
            recompute_available(&mut tx, local_account_id).await?;
            sqlx::query("UPDATE lynx_wires SET status='recalled' WHERE wire_id=$1")
                .bind(req.wire_id)
                .execute(&mut *tx)
                .await?;
            (
                "accepted",
                "ACCP",
                req.reason.clone().unwrap_or_else(|| "returned".into()),
            )
        }
    } else {
        (
            "rejected",
            "RJCR",
            req.reason.clone().unwrap_or_else(|| "declined".into()),
        )
    };

    let camt029 = Camt029 {
        uetr: Uuid::new_v4().to_string(),
        original_uetr: uetr.to_string(),
        status: camt029_status.into(),
        reason: Some(resolution.clone()),
    };
    let msg029 = store_message(
        &mut tx,
        req.wire_id,
        "camt.029",
        "emitted",
        &iso20022::encode_camt029(&camt029),
    )
    .await?;

    sqlx::query(
        "INSERT INTO lynx_recalls \
         (wire_id, direction, reason, status, resolution_reason, camt056_message_id, \
          camt029_message_id, resolved_at) \
         VALUES ($1,'inbound',$2,$3::lynx_recall_status,$4,$5,$6,CURRENT_TIMESTAMP)",
    )
    .bind(req.wire_id)
    .bind(&camt056.reason)
    .bind(recall_status)
    .bind(&resolution)
    .bind(msg056)
    .bind(msg029)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "wire_id": req.wire_id, "recall_status": recall_status, "resolution": resolution
    })))
}

// ---------------------------------------------------------------------------
// admin plane (service token)
// ---------------------------------------------------------------------------

/// Sweep `sent` wires the network never settled → refund the sender, reject.
async fn admin_reject_stale(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
) -> Result<Json<serde_json::Value>, AppError> {
    let rail = resolve_lynx(&state).await?;
    let mut tx = state.pool.begin().await?;

    let rows: Vec<(Uuid, Uuid, Decimal, String)> = sqlx::query_as(
        "SELECT wire_id, local_account_id, amount, reference_number FROM lynx_wires \
         WHERE status='sent' AND sent_at < CURRENT_TIMESTAMP - make_interval(mins => $1) FOR UPDATE",
    )
    .bind(stale_minutes())
    .fetch_all(&mut *tx)
    .await?;

    let mut rejected = 0i64;
    for (wire_id, local_account_id, amount, reference) in rows {
        let hold = Hold {
            from_account: local_account_id,
            amount,
            reference,
            transaction_id: Uuid::nil(),
        };
        zero_available(&mut tx, local_account_id).await?;
        rail.refund(&state, &mut tx, &hold, "Lynx stale wire rejected")
            .await?;
        recompute_available(&mut tx, local_account_id).await?;
        sqlx::query("UPDATE lynx_wires SET status='rejected' WHERE wire_id=$1")
            .bind(wire_id)
            .execute(&mut *tx)
            .await?;
        rejected += 1;
    }
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "rejected": rejected })))
}
