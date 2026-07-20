//! AFT/EFT batch-rail product lifecycle. Money movement goes through the Rail
//! port (`rails::aft::AftRail`); this module owns batch accrual, the CPA-005
//! file emit/ingest, the settlement-window sweep, PAD mandates, and returns.

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

use crate::aft::cpa005;
use crate::errors::AppError;
use crate::handlers::cards::{
    fetch_account_for_update, normalize_amount, post_gl_entry, reference_number,
};
use crate::handlers::AppState;
use crate::ledger::Account as GlAccount;
use crate::middleware::auth::{AuthenticatedCustomer, AuthenticatedService};
use crate::models::aft::{
    BatchResponse, CreateCreditRequest, CreateDebitRequest, CreateMandateRequest, EntryResponse,
    MandateResponse,
};
use crate::rails::aft::{ensure_aft_accounts, AftRail};
use crate::rails::common::{recompute_available, zero_available};
use crate::rails::{Destination, Rail};

pub fn aft_routes() -> Router<AppState> {
    Router::new()
        // customer plane
        .route("/mandates", post(create_mandate).get(list_mandates))
        .route("/mandates/:id", delete(revoke_mandate))
        .route("/credits", post(create_credit))
        .route("/debits", post(create_debit))
        .route("/entries", get(list_entries))
        // network/admin plane (service token) — the ACSS simulator + bank ops.
        // Batches are a bank-operational concept (one shared open outbound file),
        // so listing and submitting them are service-token ops, not customer ops;
        // customers see their own activity via /entries.
        .route("/batches", get(list_batches))
        .route("/batches/:id/submit", post(submit_batch))
        .route("/network/settle/:batch", post(network_settle))
        .route("/network/inbound-batch", post(network_inbound_batch))
        .route("/network/returns", post(network_returns))
}

/// Resolve AFT's clearing/settlement accounts (re-resolved per request) and
/// build the rail.
async fn resolve_aft(state: &AppState) -> Result<AftRail, AppError> {
    let accts = ensure_aft_accounts(&state.pool).await?;
    Ok(AftRail::new(accts))
}

/// Directory the emitted CPA-005 files are written to.
fn aft_file_dir() -> String {
    std::env::var("NANO_BANK__AFT__FILE_DIR").unwrap_or_else(|_| "/tmp/nano-bank-aft".to_string())
}

// available_balance helpers (customer accounts only; NEVER the system
// clearing/settlement accounts) are shared across rails in `rails::common`.

// --- batch/entry helpers ---

/// Get the single open outbound batch (creating one if none), locked FOR UPDATE.
async fn open_batch(tx: &mut crate::rails::PgTx<'_>) -> Result<Uuid, AppError> {
    if let Some(id) = select_open_outbound(tx).await? {
        return Ok(id);
    }
    // Create it. A concurrent originate may create it first; the partial unique
    // index `idx_aft_batches_one_open` turns that race into ON CONFLICT DO
    // NOTHING (no row returned), after which we re-read the winner. This can't
    // poison the tx the way a raw 23505 would.
    let created: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO aft_batches (direction, status) VALUES ('outbound','open') \
         ON CONFLICT (direction) WHERE status='open' DO NOTHING RETURNING batch_id",
    )
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(id) = created {
        return Ok(id);
    }
    select_open_outbound(tx)
        .await?
        .ok_or_else(|| AppError::Internal("open batch vanished after conflict".into()))
}

async fn select_open_outbound(tx: &mut crate::rails::PgTx<'_>) -> Result<Option<Uuid>, AppError> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT batch_id FROM aft_batches WHERE status='open' AND direction='outbound' \
         ORDER BY created_at LIMIT 1 FOR UPDATE",
    )
    .fetch_optional(&mut **tx)
    .await?)
}

/// Largest amount that fits the CPA-005 10-digit cents field ($99,999,999.99).
/// Guarded at originate so an oversized entry can't overflow the fixed width.
fn max_cpa_amount() -> Decimal {
    Decimal::new(9_999_999_999, 2)
}

async fn bump_batch(
    tx: &mut crate::rails::PgTx<'_>,
    batch_id: Uuid,
    credit: Decimal,
    debit: Decimal,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE aft_batches SET entry_count = entry_count + 1, \
         total_credits = total_credits + $2, total_debits = total_debits + $3 WHERE batch_id = $1",
    )
    .bind(batch_id)
    .bind(credit)
    .bind(debit)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn load_batch(state: &AppState, batch_id: Uuid) -> Result<BatchResponse, AppError> {
    let r = sqlx::query_as::<_, (Uuid, String, String, i32, Decimal, Decimal, Option<String>)>(
        "SELECT batch_id, direction::text, status::text, entry_count, total_credits, total_debits, file_ref \
         FROM aft_batches WHERE batch_id = $1",
    )
    .bind(batch_id)
    .fetch_one(&state.pool)
    .await?;
    Ok(BatchResponse {
        batch_id: r.0,
        direction: r.1,
        status: r.2,
        entry_count: r.3,
        total_credits: r.4,
        total_debits: r.5,
        file_ref: r.6,
    })
}

async fn load_entry(state: &AppState, entry_id: Uuid) -> Result<EntryResponse, AppError> {
    let r = sqlx::query_as::<_, (Uuid, Uuid, String, String, Decimal, String, Option<String>, Option<String>)>(
        "SELECT entry_id, batch_id, kind::text, direction::text, amount, status::text, payee_name, return_reason \
         FROM aft_entries WHERE entry_id = $1",
    )
    .bind(entry_id)
    .fetch_one(&state.pool)
    .await?;
    Ok(EntryResponse {
        entry_id: r.0,
        batch_id: r.1,
        kind: r.2,
        direction: r.3,
        amount: r.4,
        status: r.5,
        payee_name: r.6,
        return_reason: r.7,
    })
}

/// Replay lookup for an idempotent originate: the prior entry for this
/// (originating account, key), if any.
async fn load_entry_by_key(
    state: &AppState,
    originator: Uuid,
    key: &str,
) -> Result<Option<EntryResponse>, AppError> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT entry_id FROM aft_entries WHERE originator_account_id=$1 AND idempotency_key=$2",
    )
    .bind(originator)
    .bind(key)
    .fetch_optional(&state.pool)
    .await?;
    match id {
        Some(i) => Ok(Some(load_entry(state, i).await?)),
        None => Ok(None),
    }
}

/// Map an originate INSERT failure to the right HTTP status: a duplicate
/// idempotency key raced us (23505) → 409; an unknown counterparty institution
/// or other bad FK (23503) → 400 (rather than a 500).
fn originate_conflict(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db) = &e {
        match db.code().as_deref() {
            Some("23505") => return AppError::Conflict("idempotency_key already used".into()),
            Some("23503") => {
                return AppError::BadRequest("unknown counterparty institution".into())
            }
            _ => {}
        }
    }
    AppError::from(e)
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

/// A CPA-005 file carried in a JSON body (network inbound / returns).
#[derive(serde::Deserialize)]
struct FileRequest {
    file: String,
}

#[allow(clippy::too_many_arguments)]
async fn insert_inbound_entry(
    tx: &mut crate::rails::PgTx<'_>,
    batch_id: Uuid,
    kind: &str,
    originator_account: Uuid,
    payee: &str,
    amount: Decimal,
    status: &str,
    return_reason: Option<&str>,
    settle_txn: Option<Uuid>,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO aft_entries (batch_id, kind, direction, originator_account_id, payee_name, \
         amount, status, return_reason, settle_transaction_id) \
         VALUES ($1,$2::aft_entry_kind,'inbound',$3,$4,$5,$6::aft_entry_status,$7,$8)",
    )
    .bind(batch_id)
    .bind(kind)
    .bind(originator_account)
    .bind(payee)
    .bind(amount)
    .bind(status)
    .bind(return_reason)
    .bind(settle_txn)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// --- handlers (stubs replaced task-by-task) ---

async fn create_mandate(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    AxumJson(req): AxumJson<CreateMandateRequest>,
) -> Result<(StatusCode, Json<MandateResponse>), AppError> {
    req.validate()?;
    let amount_cap = normalize_amount(req.amount_cap)?;
    let owns: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE account_id=$1 AND customer_id=$2)",
    )
    .bind(req.payer_account_id)
    .bind(caller.customer_id)
    .fetch_one(&state.pool)
    .await?;
    if !owns {
        return Err(AppError::NotFound("payer account not found".into()));
    }
    // The authorized biller account must exist. (It belongs to the biller, not
    // the caller, so we only check existence — a bad id is a 400, not a 404.)
    let biller_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE account_id=$1)")
            .bind(req.biller_account_id)
            .fetch_one(&state.pool)
            .await?;
    if !biller_exists {
        return Err(AppError::BadRequest("biller account not found".into()));
    }
    let freq = req
        .frequency
        .clone()
        .unwrap_or_else(|| "monthly".to_string());
    let row = sqlx::query_as::<_, (Uuid, String)>(
        "INSERT INTO pad_mandates (payer_account_id, biller_account_id, biller_name, originator_id, amount_cap, frequency) \
         VALUES ($1,$2,$3,$4,$5,$6) RETURNING mandate_id, status::text",
    )
    .bind(req.payer_account_id)
    .bind(req.biller_account_id)
    .bind(&req.biller_name)
    .bind(&req.originator_id)
    .bind(amount_cap)
    .bind(&freq)
    .fetch_one(&state.pool)
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(MandateResponse {
            mandate_id: row.0,
            payer_account_id: req.payer_account_id,
            biller_account_id: req.biller_account_id,
            biller_name: req.biller_name,
            amount_cap,
            status: row.1,
        }),
    ))
}

async fn list_mandates(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
) -> Result<Json<Vec<MandateResponse>>, AppError> {
    let rows = sqlx::query_as::<_, (Uuid, Uuid, Uuid, String, Decimal, String)>(
        "SELECT m.mandate_id, m.payer_account_id, m.biller_account_id, m.biller_name, m.amount_cap, m.status::text \
         FROM pad_mandates m JOIN accounts a ON a.account_id = m.payer_account_id \
         WHERE a.customer_id = $1 ORDER BY m.created_at DESC",
    )
    .bind(caller.customer_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| MandateResponse {
                mandate_id: r.0,
                payer_account_id: r.1,
                biller_account_id: r.2,
                biller_name: r.3,
                amount_cap: r.4,
                status: r.5,
            })
            .collect(),
    ))
}

async fn revoke_mandate(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Path(mandate_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let n = sqlx::query(
        "UPDATE pad_mandates SET status='revoked', revoked_at=CURRENT_TIMESTAMP \
         WHERE mandate_id=$1 AND status='active' \
           AND payer_account_id IN (SELECT account_id FROM accounts WHERE customer_id=$2)",
    )
    .bind(mandate_id)
    .bind(caller.customer_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound("mandate not found".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn create_credit(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    AxumJson(req): AxumJson<CreateCreditRequest>,
) -> Result<(StatusCode, Json<EntryResponse>), AppError> {
    req.validate()?;
    let amount = normalize_amount(req.amount)?;
    if amount > max_cpa_amount() {
        return Err(AppError::BadRequest(
            "amount exceeds AFT file field limit".into(),
        ));
    }
    if !caller_owns_account(&state, req.originator_account_id, caller.customer_id).await? {
        return Err(AppError::NotFound("originator account not found".into()));
    }
    // Idempotency replay: same (originating account, key) returns the original.
    if let Some(key) = &req.idempotency_key {
        if let Some(existing) = load_entry_by_key(&state, req.originator_account_id, key).await? {
            return Ok((StatusCode::CREATED, Json(existing)));
        }
    }
    let mut tx = state.pool.begin().await?;
    let batch_id = open_batch(&mut tx).await?;
    let entry_id: Uuid = sqlx::query_scalar(
        "INSERT INTO aft_entries (batch_id, kind, direction, originator_account_id, \
         counterparty_institution, counterparty_transit, counterparty_account, payee_name, amount, idempotency_key) \
         VALUES ($1,'credit','outbound',$2,$3,$4,$5,$6,$7,$8) RETURNING entry_id",
    )
    .bind(batch_id)
    .bind(req.originator_account_id)
    .bind(&req.counterparty_institution)
    .bind(&req.counterparty_transit)
    .bind(&req.counterparty_account)
    .bind(&req.payee_name)
    .bind(amount)
    .bind(&req.idempotency_key)
    .fetch_one(&mut *tx)
    .await
    .map_err(originate_conflict)?;
    bump_batch(&mut tx, batch_id, amount, Decimal::ZERO).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(load_entry(&state, entry_id).await?),
    ))
}

async fn create_debit(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    AxumJson(req): AxumJson<CreateDebitRequest>,
) -> Result<(StatusCode, Json<EntryResponse>), AppError> {
    req.validate()?;
    let amount = normalize_amount(req.amount)?;
    if amount > max_cpa_amount() {
        return Err(AppError::BadRequest(
            "amount exceeds AFT file field limit".into(),
        ));
    }
    // The biller's collecting account must belong to the caller.
    if !caller_owns_account(&state, req.originator_account_id, caller.customer_id).await? {
        return Err(AppError::NotFound("originator account not found".into()));
    }
    // Mandate must be active and its cap must cover the amount.
    let mandate = sqlx::query_as::<_, (Decimal, Uuid, Uuid)>(
        "SELECT amount_cap, payer_account_id, biller_account_id FROM pad_mandates \
         WHERE mandate_id=$1 AND status='active'",
    )
    .bind(req.mandate_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| AppError::NotFound("active mandate not found".into()))?;
    // Only the biller account the payer authorized may collect on this mandate.
    // Without this, any customer holding an active mandate_id could pull the
    // payer's funds into their own account.
    if req.originator_account_id != mandate.2 {
        return Err(AppError::Authorization(
            "not the authorized biller for this mandate".into(),
        ));
    }
    if amount > mandate.0 {
        return Err(AppError::BadRequest("amount exceeds mandate cap".into()));
    }
    // The payer is a nano-bank account (intra-bank PAD); record its routing for the file.
    let payer = sqlx::query_as::<_, (String, String, String)>(
        "SELECT institution_number, transit_number, account_number FROM accounts WHERE account_id=$1",
    )
    .bind(mandate.1)
    .fetch_one(&state.pool)
    .await?;
    // Idempotency replay: same (originating account, key) returns the original.
    if let Some(key) = &req.idempotency_key {
        if let Some(existing) = load_entry_by_key(&state, req.originator_account_id, key).await? {
            return Ok((StatusCode::CREATED, Json(existing)));
        }
    }
    let mut tx = state.pool.begin().await?;
    let batch_id = open_batch(&mut tx).await?;
    let entry_id: Uuid = sqlx::query_scalar(
        "INSERT INTO aft_entries (batch_id, kind, direction, originator_account_id, counterparty_account_id, \
         counterparty_institution, counterparty_transit, counterparty_account, payee_name, amount, mandate_id, idempotency_key) \
         VALUES ($1,'debit','outbound',$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING entry_id",
    )
    .bind(batch_id)
    .bind(req.originator_account_id)
    .bind(mandate.1)
    .bind(&payer.0)
    .bind(&payer.1)
    .bind(&payer.2)
    .bind("PAD DEBIT")
    .bind(amount)
    .bind(req.mandate_id)
    .bind(&req.idempotency_key)
    .fetch_one(&mut *tx)
    .await
    .map_err(originate_conflict)?;
    bump_batch(&mut tx, batch_id, Decimal::ZERO, amount).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(load_entry(&state, entry_id).await?),
    ))
}
async fn list_batches(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<BatchResponse>>, AppError> {
    let status = params.get("status").cloned();
    let rows = sqlx::query_as::<_, (Uuid, String, String, i32, Decimal, Decimal, Option<String>)>(
        "SELECT batch_id, direction::text, status::text, entry_count, total_credits, total_debits, file_ref \
         FROM aft_batches WHERE ($1::text IS NULL OR status::text=$1) ORDER BY created_at DESC LIMIT 100",
    )
    .bind(&status)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| BatchResponse {
                batch_id: r.0,
                direction: r.1,
                status: r.2,
                entry_count: r.3,
                total_credits: r.4,
                total_debits: r.5,
                file_ref: r.6,
            })
            .collect(),
    ))
}
async fn submit_batch(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
    Path(batch_id): Path<Uuid>,
) -> Result<Json<BatchResponse>, AppError> {
    let mut tx = state.pool.begin().await?;
    let status: String =
        sqlx::query_scalar("SELECT status::text FROM aft_batches WHERE batch_id=$1 FOR UPDATE")
            .bind(batch_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound("batch not found".into()))?;
    if status != "open" {
        return Err(AppError::Conflict(format!("batch is {status}")));
    }

    let rows = sqlx::query_as::<
        _,
        (
            String,
            Decimal,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ),
    >(
        "SELECT kind::text, amount, counterparty_institution, counterparty_transit, \
         counterparty_account, payee_name FROM aft_entries WHERE batch_id=$1 ORDER BY created_at",
    )
    .bind(batch_id)
    .fetch_all(&mut *tx)
    .await?;

    let (entry_count, total_credits, total_debits): (i32, Decimal, Decimal) = sqlx::query_as(
        "SELECT entry_count, total_credits, total_debits FROM aft_batches WHERE batch_id=$1",
    )
    .bind(batch_id)
    .fetch_one(&mut *tx)
    .await?;

    let due = chrono::Utc::now().format("%Y%j").to_string();
    let details: Vec<cpa005::Detail> = rows
        .iter()
        .map(|r| cpa005::Detail {
            txn_code: if r.0 == "credit" { 'C' } else { 'D' },
            amount: r.1,
            institution: r.2.clone().unwrap_or_default(),
            transit: r.3.clone().unwrap_or_default(),
            account: r.4.clone().unwrap_or_default(),
            payee_name: r.5.clone().unwrap_or_default(),
            originator_short: "NANO".into(),
            due_date: due.clone(),
            return_reason: None,
        })
        .collect();
    let header = cpa005::Header {
        originator_id: "0000000900".into(),
        created: due.clone(),
        file_seq: 1,
    };
    let trailer = cpa005::Trailer {
        entry_count: entry_count as u32,
        total_credits,
        total_debits,
    };
    let file = cpa005::encode(&header, &details, &trailer);

    let dir = aft_file_dir();
    let path = format!("{dir}/{batch_id}.005");

    sqlx::query("UPDATE aft_batches SET status='submitted', file_ref=$2, cutoff_at=CURRENT_TIMESTAMP WHERE batch_id=$1")
        .bind(batch_id)
        .bind(&path)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    // Write the emitted file AFTER commit, with async IO — don't block the
    // runtime on filesystem IO, and don't hold the batch row lock across it.
    // network_settle reads the DB (not the file), so a write failure is a 500
    // for the caller but leaves settlement able to proceed.
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| AppError::Internal(format!("create AFT file dir: {e}")))?;
    tokio::fs::write(&path, &file)
        .await
        .map_err(|e| AppError::Internal(format!("write CPA-005 file: {e}")))?;

    tracing::info!(%batch_id, entries = entry_count, file = %path, "📄 AFT batch submitted");
    Ok(Json(load_batch(&state, batch_id).await?))
}
async fn list_entries(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<EntryResponse>>, AppError> {
    let status = params.get("status").cloned();
    let rows = sqlx::query_as::<_, (Uuid, Uuid, String, String, Decimal, String, Option<String>, Option<String>)>(
        "SELECT e.entry_id, e.batch_id, e.kind::text, e.direction::text, e.amount, e.status::text, \
         e.payee_name, e.return_reason FROM aft_entries e \
         JOIN accounts a ON a.account_id = e.originator_account_id \
         WHERE a.customer_id=$1 AND ($2::text IS NULL OR e.status::text=$2) \
         ORDER BY e.created_at DESC LIMIT 100",
    )
    .bind(caller.customer_id)
    .bind(&status)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| EntryResponse {
                entry_id: r.0,
                batch_id: r.1,
                kind: r.2,
                direction: r.3,
                amount: r.4,
                status: r.5,
                payee_name: r.6,
                return_reason: r.7,
            })
            .collect(),
    ))
}
async fn network_settle(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
    Path(batch_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let rail = resolve_aft(&state).await?;
    let mut tx = state.pool.begin().await?;
    let status: String =
        sqlx::query_scalar("SELECT status::text FROM aft_batches WHERE batch_id=$1 FOR UPDATE")
            .bind(batch_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound("batch not found".into()))?;
    if status != "submitted" {
        return Err(AppError::Conflict(format!("batch is {status}")));
    }

    let entries = sqlx::query_as::<_, (Uuid, String, Decimal, Option<Uuid>, Option<Uuid>, Option<String>)>(
        "SELECT entry_id, kind::text, amount, originator_account_id, counterparty_account_id, \
         counterparty_institution FROM aft_entries WHERE batch_id=$1 AND status='pending' ORDER BY created_at",
    )
    .bind(batch_id)
    .fetch_all(&mut *tx)
    .await?;

    let mut settled = 0i64;
    let mut rejected = 0i64;
    let mut settled_credit_total = Decimal::ZERO;

    for (entry_id, kind, amount, originator, counterparty_acct, institution) in entries {
        if kind == "credit" {
            // Direct deposit to an external recipient: debit the originator, funds → settlement.
            let orig =
                originator.ok_or_else(|| AppError::Internal("credit missing originator".into()))?;
            let acct = fetch_account_for_update(&mut tx, orig)
                .await?
                .ok_or_else(|| AppError::Internal("originator account gone".into()))?;
            if amount > acct.available_balance {
                sqlx::query("UPDATE aft_entries SET status='rejected', return_reason='NSF' WHERE entry_id=$1")
                    .bind(entry_id).execute(&mut *tx).await?;
                rejected += 1;
                continue;
            }
            zero_available(&mut tx, orig).await?;
            let hold = rail
                .hold(&state, &mut tx, orig, amount, "AFT credit settlement")
                .await?;
            rail.release(
                &state,
                &mut tx,
                &hold,
                Destination::External(institution.unwrap_or_else(|| "000".into())),
                "AFT credit settlement",
            )
            .await?;
            recompute_available(&mut tx, orig).await?;
            sqlx::query("UPDATE aft_entries SET status='settled', settle_transaction_id=$2 WHERE entry_id=$1")
                .bind(entry_id).bind(hold.transaction_id).execute(&mut *tx).await?;
            settled_credit_total += amount;
            settled += 1;
        } else {
            // Intra-bank PAD: payer → biller.
            let payer = counterparty_acct
                .ok_or_else(|| AppError::Internal("debit missing payer".into()))?;
            let biller =
                originator.ok_or_else(|| AppError::Internal("debit missing biller".into()))?;
            let acct = fetch_account_for_update(&mut tx, payer)
                .await?
                .ok_or_else(|| AppError::Internal("payer account gone".into()))?;
            if amount > acct.available_balance {
                sqlx::query("UPDATE aft_entries SET status='rejected', return_reason='NSF' WHERE entry_id=$1")
                    .bind(entry_id).execute(&mut *tx).await?;
                rejected += 1;
                continue;
            }
            zero_available(&mut tx, payer).await?;
            let hold = rail
                .hold(&state, &mut tx, payer, amount, "AFT PAD settlement")
                .await?;
            rail.release(
                &state,
                &mut tx,
                &hold,
                Destination::Internal(biller),
                "AFT PAD settlement",
            )
            .await?;
            recompute_available(&mut tx, payer).await?;
            recompute_available(&mut tx, biller).await?;
            sqlx::query("UPDATE aft_entries SET status='settled', settle_transaction_id=$2 WHERE entry_id=$1")
                .bind(entry_id).bind(hold.transaction_id).execute(&mut *tx).await?;
            settled += 1;
        }
    }

    // Settlement sweep: realize the external direct-deposit cash to Bank (aggregate
    // GL). Per-entry posts are Payable/Payable reclasses; cash hits Bank here.
    if settled_credit_total > Decimal::ZERO {
        post_gl_entry(
            &state,
            &reference_number("AFTS"),
            "AFT settlement sweep",
            GlAccount::Payable,
            GlAccount::Bank,
            settled_credit_total,
        )
        .await?;
    }

    sqlx::query(
        "UPDATE aft_batches SET status='settled', settled_at=CURRENT_TIMESTAMP WHERE batch_id=$1",
    )
    .bind(batch_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    tracing::info!(%batch_id, settled, rejected, swept = %settled_credit_total, "🏦 AFT batch settled");
    Ok(Json(serde_json::json!({
        "settled_entries": settled, "rejected": rejected, "swept_credits": settled_credit_total
    })))
}
async fn network_inbound_batch(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
    AxumJson(req): AxumJson<FileRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let rail = resolve_aft(&state).await?;
    let (_h, details, _t) =
        cpa005::decode(&req.file).map_err(|e| AppError::BadRequest(format!("bad CPA-005: {e}")))?;
    let mut tx = state.pool.begin().await?;
    let batch_id: Uuid = sqlx::query_scalar(
        "INSERT INTO aft_batches (direction, status) VALUES ('inbound','settled') RETURNING batch_id",
    )
    .fetch_one(&mut *tx)
    .await?;

    let (mut credited, mut debited, mut rejected, mut unknown) = (0i64, 0i64, 0i64, 0i64);
    for d in &details {
        let target: Option<(Uuid,)> = sqlx::query_as(
            "SELECT account_id FROM accounts WHERE institution_number=$1 AND transit_number=$2 AND account_number=$3",
        )
        .bind(&d.institution)
        .bind(&d.transit)
        .bind(&d.account)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((acct,)) = target else {
            unknown += 1;
            continue;
        };
        if d.txn_code == 'C' {
            let posting = rail
                .accept_inbound(&state, &mut tx, acct, d.amount, "AFT inbound credit")
                .await?;
            recompute_available(&mut tx, acct).await?;
            insert_inbound_entry(
                &mut tx,
                batch_id,
                "credit",
                acct,
                &d.payee_name,
                d.amount,
                "settled",
                None,
                Some(posting.transaction_id),
            )
            .await?;
            credited += 1;
        } else {
            // Inbound external PAD debit: applied on an NSF check alone, with NO
            // mandate lookup — deliberately. This is the "trust ACSS + rely on
            // returns" model: the originating institution asserts it holds the
            // payer's authorization, and an unauthorized pull is recovered by a
            // return (POST /aft/network/returns), not blocked here. (Contrast the
            // OUTBOUND PAD in create_debit, which IS mandate-gated.)
            let a = fetch_account_for_update(&mut tx, acct)
                .await?
                .ok_or_else(|| AppError::Internal("target account gone".into()))?;
            if d.amount > a.available_balance {
                insert_inbound_entry(
                    &mut tx,
                    batch_id,
                    "debit",
                    acct,
                    &d.payee_name,
                    d.amount,
                    "rejected",
                    Some("NSF"),
                    None,
                )
                .await?;
                rejected += 1;
                continue;
            }
            zero_available(&mut tx, acct).await?;
            let hold = rail
                .hold(&state, &mut tx, acct, d.amount, "AFT inbound debit")
                .await?;
            rail.release(
                &state,
                &mut tx,
                &hold,
                Destination::External("000".into()),
                "AFT inbound debit",
            )
            .await?;
            recompute_available(&mut tx, acct).await?;
            insert_inbound_entry(
                &mut tx,
                batch_id,
                "debit",
                acct,
                &d.payee_name,
                d.amount,
                "settled",
                None,
                Some(hold.transaction_id),
            )
            .await?;
            debited += 1;
        }
    }
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "batch_id": batch_id, "credited": credited, "debited": debited,
            "rejected": rejected, "unknown": unknown
        })),
    ))
}

async fn network_returns(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
    AxumJson(req): AxumJson<FileRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let rail = resolve_aft(&state).await?;
    let (_h, details, _t) =
        cpa005::decode(&req.file).map_err(|e| AppError::BadRequest(format!("bad CPA-005: {e}")))?;
    let mut tx = state.pool.begin().await?;
    let mut returned = 0i64;
    for d in &details {
        let reason = d.return_reason.clone().unwrap_or_else(|| "RET".into());
        // Match a settled entry by amount + external counterparty account (the
        // primary return case: an outbound direct-deposit credit that bounced).
        let row: Option<(Uuid, String, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
            "SELECT entry_id, kind::text, originator_account_id, counterparty_account_id \
             FROM aft_entries WHERE status='settled' AND amount=$1 AND counterparty_account=$2 \
             ORDER BY created_at LIMIT 1 FOR UPDATE",
        )
        .bind(d.amount)
        .bind(&d.account)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((entry_id, kind, originator, counterparty_acct)) = row else {
            continue;
        };
        let posting = if kind == "credit" {
            // Funds return to the originator (Dr SETTLEMENT / Cr originator).
            let orig =
                originator.ok_or_else(|| AppError::Internal("return: no originator".into()))?;
            let p = rail
                .accept_inbound(&state, &mut tx, orig, d.amount, "AFT credit return")
                .await?;
            recompute_available(&mut tx, orig).await?;
            p
        } else {
            // Reverse an intra-bank PAD: biller → payer.
            let biller =
                originator.ok_or_else(|| AppError::Internal("return: no biller".into()))?;
            let payer =
                counterparty_acct.ok_or_else(|| AppError::Internal("return: no payer".into()))?;
            zero_available(&mut tx, biller).await?;
            let hold = rail
                .hold(&state, &mut tx, biller, d.amount, "AFT debit return")
                .await?;
            let p = rail
                .release(
                    &state,
                    &mut tx,
                    &hold,
                    Destination::Internal(payer),
                    "AFT debit return",
                )
                .await?;
            recompute_available(&mut tx, biller).await?;
            recompute_available(&mut tx, payer).await?;
            p
        };
        sqlx::query("UPDATE aft_entries SET status='returned', return_reason=$2, return_transaction_id=$3 WHERE entry_id=$1")
            .bind(entry_id)
            .bind(&reason)
            .bind(posting.transaction_id)
            .execute(&mut *tx)
            .await?;
        returned += 1;
    }
    tx.commit().await?;
    Ok(Json(serde_json::json!({ "returned": returned })))
}
