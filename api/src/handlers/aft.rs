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
use crate::handlers::cards::{fetch_account_for_update, normalize_amount, post_gl_entry, post_two_legged, reference_number};
use crate::handlers::AppState;
use crate::ledger::Account as GlAccount;
use crate::middleware::auth::{AuthenticatedCustomer, AuthenticatedService};
use crate::models::aft::{
    BatchResponse, CreateCreditRequest, CreateDebitRequest, CreateMandateRequest, EntryResponse,
    MandateResponse,
};
use crate::rails::aft::{ensure_aft_accounts, AftRail};
use crate::rails::{Destination, Rail};

pub fn aft_routes() -> Router<AppState> {
    Router::new()
        // customer plane
        .route("/mandates", post(create_mandate).get(list_mandates))
        .route("/mandates/:id", delete(revoke_mandate))
        .route("/credits", post(create_credit))
        .route("/debits", post(create_debit))
        .route("/batches", get(list_batches))
        .route("/batches/:id/submit", post(submit_batch))
        .route("/entries", get(list_entries))
        // network plane (service token) — the ACSS simulator
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

// --- available_balance helpers (customer accounts only; NEVER the system
// clearing/settlement accounts) — copied from handlers/interac.rs. ---

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

// --- batch/entry helpers ---

/// Get the single open outbound batch (creating one if none), locked FOR UPDATE.
async fn open_batch(tx: &mut crate::rails::PgTx<'_>) -> Result<Uuid, AppError> {
    if let Some(id) = sqlx::query_scalar::<_, Uuid>(
        "SELECT batch_id FROM aft_batches WHERE status='open' AND direction='outbound' \
         ORDER BY created_at LIMIT 1 FOR UPDATE",
    )
    .fetch_optional(&mut **tx)
    .await?
    {
        return Ok(id);
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO aft_batches (direction, status) VALUES ('outbound','open') RETURNING batch_id",
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(id)
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
        batch_id: r.0, direction: r.1, status: r.2, entry_count: r.3,
        total_credits: r.4, total_debits: r.5, file_ref: r.6,
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
        entry_id: r.0, batch_id: r.1, kind: r.2, direction: r.3,
        amount: r.4, status: r.5, payee_name: r.6, return_reason: r.7,
    })
}

async fn caller_owns_account(state: &AppState, account_id: Uuid, customer_id: Uuid) -> Result<bool, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE account_id=$1 AND customer_id=$2)",
    )
    .bind(account_id)
    .bind(customer_id)
    .fetch_one(&state.pool)
    .await?)
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
    let freq = req.frequency.clone().unwrap_or_else(|| "monthly".to_string());
    let row = sqlx::query_as::<_, (Uuid, String)>(
        "INSERT INTO pad_mandates (payer_account_id, biller_name, originator_id, amount_cap, frequency) \
         VALUES ($1,$2,$3,$4,$5) RETURNING mandate_id, status::text",
    )
    .bind(req.payer_account_id)
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
    let rows = sqlx::query_as::<_, (Uuid, Uuid, String, Decimal, String)>(
        "SELECT m.mandate_id, m.payer_account_id, m.biller_name, m.amount_cap, m.status::text \
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
                biller_name: r.2,
                amount_cap: r.3,
                status: r.4,
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
    if !caller_owns_account(&state, req.originator_account_id, caller.customer_id).await? {
        return Err(AppError::NotFound("originator account not found".into()));
    }
    let mut tx = state.pool.begin().await?;
    let batch_id = open_batch(&mut tx).await?;
    let entry_id: Uuid = sqlx::query_scalar(
        "INSERT INTO aft_entries (batch_id, kind, direction, originator_account_id, \
         counterparty_institution, counterparty_transit, counterparty_account, payee_name, amount) \
         VALUES ($1,'credit','outbound',$2,$3,$4,$5,$6,$7) RETURNING entry_id",
    )
    .bind(batch_id)
    .bind(req.originator_account_id)
    .bind(&req.counterparty_institution)
    .bind(&req.counterparty_transit)
    .bind(&req.counterparty_account)
    .bind(&req.payee_name)
    .bind(amount)
    .fetch_one(&mut *tx)
    .await?;
    bump_batch(&mut tx, batch_id, amount, Decimal::ZERO).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(load_entry(&state, entry_id).await?)))
}

async fn create_debit(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    AxumJson(req): AxumJson<CreateDebitRequest>,
) -> Result<(StatusCode, Json<EntryResponse>), AppError> {
    req.validate()?;
    let amount = normalize_amount(req.amount)?;
    // The biller's collecting account must belong to the caller.
    if !caller_owns_account(&state, req.originator_account_id, caller.customer_id).await? {
        return Err(AppError::NotFound("originator account not found".into()));
    }
    // Mandate must be active and its cap must cover the amount.
    let mandate = sqlx::query_as::<_, (Decimal, Uuid)>(
        "SELECT amount_cap, payer_account_id FROM pad_mandates WHERE mandate_id=$1 AND status='active'",
    )
    .bind(req.mandate_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| AppError::NotFound("active mandate not found".into()))?;
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
    let mut tx = state.pool.begin().await?;
    let batch_id = open_batch(&mut tx).await?;
    let entry_id: Uuid = sqlx::query_scalar(
        "INSERT INTO aft_entries (batch_id, kind, direction, originator_account_id, counterparty_account_id, \
         counterparty_institution, counterparty_transit, counterparty_account, payee_name, amount, mandate_id) \
         VALUES ($1,'debit','outbound',$2,$3,$4,$5,$6,$7,$8,$9) RETURNING entry_id",
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
    .fetch_one(&mut *tx)
    .await?;
    bump_batch(&mut tx, batch_id, Decimal::ZERO, amount).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(load_entry(&state, entry_id).await?)))
}
async fn list_batches() -> Result<StatusCode, AppError> {
    Err(AppError::Internal("todo".into()))
}
async fn submit_batch(
    State(state): State<AppState>,
    _caller: AuthenticatedCustomer,
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

    let rows = sqlx::query_as::<_, (String, Decimal, Option<String>, Option<String>, Option<String>, Option<String>)>(
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
    std::fs::create_dir_all(&dir).ok();
    let path = format!("{dir}/{batch_id}.005");
    std::fs::write(&path, &file).map_err(|e| AppError::Internal(format!("write CPA-005 file: {e}")))?;

    sqlx::query("UPDATE aft_batches SET status='submitted', file_ref=$2, cutoff_at=CURRENT_TIMESTAMP WHERE batch_id=$1")
        .bind(batch_id)
        .bind(&path)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    tracing::info!(%batch_id, entries = entry_count, file = %path, "📄 AFT batch submitted");
    Ok(Json(load_batch(&state, batch_id).await?))
}
async fn list_entries() -> Result<StatusCode, AppError> {
    Err(AppError::Internal("todo".into()))
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
            let orig = originator.ok_or_else(|| AppError::Internal("credit missing originator".into()))?;
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
            let hold = rail.hold(&state, &mut tx, orig, amount, "AFT credit settlement").await?;
            rail.release(&state, &mut tx, &hold,
                Destination::External(institution.unwrap_or_else(|| "000".into())),
                "AFT credit settlement").await?;
            recompute_available(&mut tx, orig).await?;
            sqlx::query("UPDATE aft_entries SET status='settled', settle_transaction_id=$2 WHERE entry_id=$1")
                .bind(entry_id).bind(hold.transaction_id).execute(&mut *tx).await?;
            settled_credit_total += amount;
            settled += 1;
        } else {
            // Intra-bank PAD: payer → biller.
            let payer = counterparty_acct.ok_or_else(|| AppError::Internal("debit missing payer".into()))?;
            let biller = originator.ok_or_else(|| AppError::Internal("debit missing biller".into()))?;
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
            let hold = rail.hold(&state, &mut tx, payer, amount, "AFT PAD settlement").await?;
            rail.release(&state, &mut tx, &hold, Destination::Internal(biller), "AFT PAD settlement").await?;
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
        post_gl_entry(&state, &reference_number("AFTS"), "AFT settlement sweep",
            GlAccount::Payable, GlAccount::Bank, settled_credit_total).await?;
    }

    sqlx::query("UPDATE aft_batches SET status='settled', settled_at=CURRENT_TIMESTAMP WHERE batch_id=$1")
        .bind(batch_id).execute(&mut *tx).await?;
    tx.commit().await?;

    tracing::info!(%batch_id, settled, rejected, swept = %settled_credit_total, "🏦 AFT batch settled");
    Ok(Json(serde_json::json!({
        "settled_entries": settled, "rejected": rejected, "swept_credits": settled_credit_total
    })))
}
async fn network_inbound_batch() -> Result<StatusCode, AppError> {
    Err(AppError::Internal("todo".into()))
}
async fn network_returns() -> Result<StatusCode, AppError> {
    Err(AppError::Internal("todo".into()))
}
