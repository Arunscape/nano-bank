# AFT / EFT Batch Rail — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the Canadian AFT/EFT batch rail — direct-deposit credits + mandate-gated pre-authorized debits, real CPA-005-style files, settlement windows with a `Bank` sweep, and an NSF/returns cycle — on top of the Interac rail foundation.

**Architecture:** `AftRail` implements the existing `Rail` port for the per-transaction money moves (`hold`/`release`/`refund`/`accept_inbound`); AFT-specific orchestration (batch accrual, CPA-005 emit/ingest, the settlement-window sweep, and post-settlement returns) lives in `handlers/aft.rs`. AFT owns its own system accounts (`aft@nano.bank`: `AFT_CLEARING`, `AFT_SETTLEMENT`) and never touches the card rails' accounts.

**Tech Stack:** Rust, axum 0.7, sqlx 0.7 (Postgres 16), rust_decimal, tokio; Python (ACSS simulator + Streamlit viewer). Branch `aft-rail`, stacked on `interac-rail-foundation`.

## Global Constraints

- **CAD only**, `rust_decimal::Decimal`, amounts rounded to 2 dp. Reuse `handlers::cards::normalize_amount`.
- **Double-entry invariant**: both legs in ONE `post_two_legged` call (never update `accounts.balance` directly). Dual-post the aggregate GL via `handlers::cards::post_gl_entry` before commit; a GL failure fails the op (503).
- **AFT owns its accounts**: system customer `aft@nano.bank`, chequing=`AFT_CLEARING`, savings=`AFT_SETTLEMENT`, $1T overdraft. Do NOT reuse `BANK_SETTLEMENT`/`VISA_CLEARING` or any card/Interac system account.
- **available_balance**: recompute on CUSTOMER accounts around rail posts; NEVER on the system clearing/settlement accounts (they float on the $1T overdraft — the hard-won Interac lesson). Debits of a customer account need `available` lowered before the debit.
- **Do NOT edit `handlers/transactions.rs`** (PR #15 coexistence). Reuse the `pub(crate)` `cards.rs` helpers and the `Rail` port. New files only; `main.rs`/`handlers/mod.rs`/`models/mod.rs` get additive edits.
- **Auth planes**: customer endpoints use `AuthenticatedCustomer` (cross-customer → 404); `/aft/network/*` use `AuthenticatedService`.
- **Concurrency**: guard batch/entry state transitions with `FOR UPDATE` + status re-check (settle-once, return-once), mirroring `handlers::interac::lock_available`.
- **DB host `::1`**; run needs Kind Postgres + modern core `:8091`; verify on `:8081`.

## Templates (committed on this branch — read them; the mechanical tasks mirror them)

- `api/src/rails/interac.rs` — `InteracRail` + `ensure_interac_accounts` (AFT's rail + accounts mirror this almost exactly).
- `api/src/rails/mod.rs` — the `Rail` trait + `Hold`, `Destination`, `RailPosting`, `PgTx`.
- `api/src/handlers/interac.rs` — scaffold/routes, `resolve_*`, `lock_available` guard, `recompute_available`/`zero_available`, auth planes, curl-smoke style.
- `api/src/handlers/cards.rs` — `post_two_legged`, `post_gl_entry`, `reference_number`, `fetch_account_for_update`, `normalize_amount`, `Tx`.
- `api/src/models/interac.rs` — DTO/enum patterns (`sqlx::Type` with `type_name`).
- `src/core/tables/08_interac.sql`; `testing/interac/interac_simulator.py`; `testing/viewer/app.py` (`render_interac`); `bruno/6_Interac/`.

## Smoke bootstrap (recreate once, before the handler tasks)

Same recipe as the Interac build. Write `/home/bmartins/dev/nano-bank-aft/.superpowers/sdd/smoke-bootstrap.md` (gitignored) with: build+run the API on `:8081` (`CORE_BACKEND=modern cargo build && CORE_BACKEND=modern ./target/debug/nano-bank-api & echo $! > /tmp/aft_api.pid`; stop by PID, never `pkill -f target/debug/nano-bank-api`); mint `$STOK` via `POST /api/v1/auth/service-token {"client_secret":"nano-bank-visa-network-secret-change-me"}`; a `mkcust` helper that creates a customer (`POST /customers` with unique email/phone/sin + password), logs in (`POST /auth/login` → `access_token`), opens a chequing account (`POST /accounts {"account_type":"chequing"}` — opens ACTIVE with 0 balance), and funds it via `POST /transactions/deposit`. DB: `kubectl --context kind-nano-bank exec -i -n nano-bank deployment/postgres -- psql -U nanobank_user -d nano_bank_db`.

---

## Task 1: AFT schema (`09_aft.sql`)

**Files:** Create `src/core/tables/09_aft.sql`

**Interfaces:** Produces enums `aft_entry_kind`, `aft_batch_status`, `aft_entry_status`, `mandate_status`, `aft_direction`; tables `pad_mandates`, `aft_batches`, `aft_entries`.

- [ ] **Step 1: Write the DDL**

```sql
-- Nano Bank Core Database Schema — Part 9: AFT / EFT batch rail

CREATE TYPE aft_entry_kind   AS ENUM ('credit', 'debit');
CREATE TYPE aft_direction    AS ENUM ('outbound', 'inbound');
CREATE TYPE aft_batch_status AS ENUM ('open', 'submitted', 'settled');
CREATE TYPE aft_entry_status AS ENUM ('pending', 'settled', 'returned', 'rejected');
CREATE TYPE mandate_status   AS ENUM ('active', 'revoked');

-- Pre-authorized debit mandates: a payer authorizes a biller to pull funds.
CREATE TABLE pad_mandates (
    mandate_id       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    payer_account_id UUID NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    biller_name      VARCHAR(200) NOT NULL,
    originator_id    VARCHAR(50) NOT NULL,          -- the biller's AFT originator id
    amount_cap       DECIMAL(15,2) NOT NULL,
    frequency        VARCHAR(20) NOT NULL DEFAULT 'monthly',
    status           mandate_status NOT NULL DEFAULT 'active',
    created_at       TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    revoked_at       TIMESTAMP WITH TIME ZONE,
    CONSTRAINT chk_mandate_cap_positive CHECK (amount_cap > 0)
);
CREATE INDEX idx_pad_mandates_payer ON pad_mandates (payer_account_id);

CREATE TABLE aft_batches (
    batch_id      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    direction     aft_direction NOT NULL DEFAULT 'outbound',
    status        aft_batch_status NOT NULL DEFAULT 'open',
    entry_count   INTEGER NOT NULL DEFAULT 0,
    total_credits DECIMAL(15,2) NOT NULL DEFAULT 0,
    total_debits  DECIMAL(15,2) NOT NULL DEFAULT 0,
    file_ref      TEXT,
    cutoff_at     TIMESTAMP WITH TIME ZONE,
    settled_at    TIMESTAMP WITH TIME ZONE,
    created_at    TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
);
CREATE INDEX idx_aft_batches_status ON aft_batches (status);

CREATE TABLE aft_entries (
    entry_id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    batch_id               UUID NOT NULL REFERENCES aft_batches(batch_id) ON DELETE RESTRICT,
    kind                   aft_entry_kind NOT NULL,
    direction              aft_direction NOT NULL,
    originator_account_id  UUID REFERENCES accounts(account_id),   -- nano-bank side
    counterparty_account_id UUID REFERENCES accounts(account_id),  -- set when internal
    counterparty_institution VARCHAR(3) REFERENCES rail_participants(institution_number),
    counterparty_transit   VARCHAR(5),
    counterparty_account   VARCHAR(12),
    payee_name             VARCHAR(200),
    amount                 DECIMAL(15,2) NOT NULL,
    mandate_id             UUID REFERENCES pad_mandates(mandate_id),
    status                 aft_entry_status NOT NULL DEFAULT 'pending',
    return_reason          VARCHAR(80),
    hold_transaction_id    UUID REFERENCES transactions(transaction_id),
    settle_transaction_id  UUID REFERENCES transactions(transaction_id),
    return_transaction_id  UUID REFERENCES transactions(transaction_id),
    created_at             TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT chk_aft_amount_positive CHECK (amount > 0),
    CONSTRAINT chk_aft_amount_precision CHECK (amount = ROUND(amount, 2))
);
CREATE INDEX idx_aft_entries_batch ON aft_entries (batch_id);
CREATE INDEX idx_aft_entries_status ON aft_entries (status);
CREATE INDEX idx_aft_entries_originator ON aft_entries (originator_account_id);
```

- [ ] **Step 2: Apply & verify**

```bash
kubectl --context kind-nano-bank exec -i -n nano-bank deployment/postgres -- \
  psql -U nanobank_user -d nano_bank_db < src/core/tables/09_aft.sql
kubectl --context kind-nano-bank exec -i -n nano-bank deployment/postgres -- \
  psql -U nanobank_user -d nano_bank_db -c "\dt aft_* pad_mandates"
```
Expected: `CREATE TYPE` ×5, `CREATE TABLE` ×3, `CREATE INDEX` ×6; `\dt` lists `aft_batches`, `aft_entries`, `pad_mandates`. Apply once (re-running errors on existing types).

- [ ] **Step 3: Commit** — `git add src/core/tables/09_aft.sql && git commit -m "feat(aft): batch-rail schema — mandates, batches, entries"`

---

## Task 2: `AftRail` (impl `Rail`) + AFT system accounts

**Files:** Create `api/src/rails/aft.rs`; Modify `api/src/rails/mod.rs` (add `pub mod aft;`), `api/src/main.rs` (`mod rails;` already exists — add the AFT bootstrap call).

**Interfaces:**
- Consumes: `Rail`, `Hold`, `Destination`, `RailPosting`, `PgTx`, `RailId::Aft` (from `rails/mod.rs`); `handlers::cards::{post_two_legged, post_gl_entry, reference_number}`; `ledger::Account as GlAccount`.
- Produces: `rails::aft::{AftAccounts { clearing_id, settlement_id }, AftRail, ensure_aft_accounts(pool) -> Result<AftAccounts, sqlx::Error>}`.

**This is a near-exact copy of `api/src/rails/interac.rs`.** Read that file. Reproduce it for AFT with these deltas:
- Constant `AFT_CUSTOMER_EMAIL = "aft@nano.bank"`; the synthetic customer's phone/sin must be unique — use `'+10000000003'` / `'000000003'` (interac used `...0002`, cards `...0000`; verify no collision, bump if `INSERT` reports 23505 on a shared dev DB).
- `AftAccounts { clearing_id, settlement_id }`, `AftRail { accounts: AftAccounts }`, `ensure_aft_accounts`, `AftRail::id() -> RailId::Aft`.
- The `impl Rail for AftRail` bodies are identical to `InteracRail`'s (`hold`: Dr `from`/Cr clearing; `release` Internal→acct / External→settlement; `refund`: Dr clearing/Cr `hold.from_account`; `accept_inbound`: Dr settlement/Cr `to`), with transaction-type strings `aft_hold`/`aft_release`/`aft_refund`/`aft_inbound` and reference prefixes `AFTH`/`AFTR`/`AFTX`/`AFTI`. No `normalize_handle` (that's Interac-only).

- [ ] **Step 1: Write `rails/aft.rs`** mirroring `rails/interac.rs` with the deltas above (module doc: "AFT/EFT rail — clearing/settlement plumbing; batch/file/settlement/returns orchestration lives in handlers/aft.rs").

- [ ] **Step 2: Wire** `pub mod aft;` in `rails/mod.rs`; in `main.rs`, after the Interac bootstrap block, add:
```rust
    if let Err(e) = rails::aft::ensure_aft_accounts(&pool).await {
        warn!("❌ Failed to bootstrap AFT GL accounts: {}", e);
        std::process::exit(1);
    }
```

- [ ] **Step 3: Verify** — `cd api && cargo check` (compiles; unused-until-later warnings ok). Run the server (`CORE_BACKEND=modern`) and confirm the two accounts exist:
```bash
kubectl --context kind-nano-bank exec -i -n nano-bank deployment/postgres -- psql -U nanobank_user -d nano_bank_db -c \
 "SELECT c.email, a.account_type FROM accounts a JOIN customers c ON c.customer_id=a.customer_id WHERE c.email='aft@nano.bank' ORDER BY a.account_type;"
```
Expected: `aft@nano.bank | chequing` and `| savings`. Stop the server by PID.

- [ ] **Step 4: Commit** — `feat(aft): AftRail (impl Rail) + AFT system accounts`

---

## Task 3: CPA-005 file codec (`aft/cpa005.rs`)

**Files:** Create `api/src/aft/mod.rs` (`pub mod cpa005;`), `api/src/aft/cpa005.rs`; Modify `api/src/main.rs` (add `mod aft;`).

**Interfaces:** Produces
`cpa005::{Header, Detail, Trailer, encode(&Header,&[Detail],&Trailer) -> String, decode(&str) -> Result<(Header, Vec<Detail>, Trailer), CpaError>}`.
`Detail { txn_code: char /* 'C'|'D' */, amount: Decimal, institution: String, transit: String, account: String, payee_name: String, originator_short: String, due_date: String, return_reason: Option<String> }`.

- [ ] **Step 1: Write the failing round-trip test**

In `aft/cpa005.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;   // if not available, use Decimal::new(12345,2)

    fn sample() -> (Header, Vec<Detail>, Trailer) {
        let h = Header { originator_id: "0000000900".into(), created: "2026185".into(), file_seq: 1 };
        let d = vec![
            Detail { txn_code: 'C', amount: dec!(123.45), institution: "003".into(),
                     transit: "00001".into(), account: "000000000001".into(),
                     payee_name: "ALICE EXAMPLE".into(), originator_short: "NANO".into(),
                     due_date: "2026186".into(), return_reason: None },
            Detail { txn_code: 'D', amount: dec!(50.00), institution: "004".into(),
                     transit: "00002".into(), account: "000000000002".into(),
                     payee_name: "BOB PAYER".into(), originator_short: "NANO".into(),
                     due_date: "2026186".into(), return_reason: None },
        ];
        let t = Trailer { entry_count: 2, total_credits: dec!(123.45), total_debits: dec!(50.00) };
        (h, d, t)
    }

    #[test]
    fn round_trips() {
        let (h, d, t) = sample();
        let encoded = encode(&h, &d, &t);
        let (h2, d2, t2) = decode(&encoded).expect("decode");
        assert_eq!(h.originator_id, h2.originator_id);
        assert_eq!(d.len(), d2.len());
        assert_eq!(d[0].amount, d2[0].amount);
        assert_eq!(d[1].txn_code, d2[1].txn_code);
        assert_eq!(t.entry_count, t2.entry_count);
        assert_eq!(t.total_credits, t2.total_credits);
    }

    #[test]
    fn trailer_totals_match_details() {
        let (_h, d, t) = sample();
        let credits: Decimal = d.iter().filter(|x| x.txn_code == 'C').map(|x| x.amount).sum();
        assert_eq!(credits, t.total_credits);
    }
}
```

- [ ] **Step 2: Run — expect FAIL** (`cd api && cargo test aft::cpa005::tests`; "cannot find function `encode`").

- [ ] **Step 3: Implement `cpa005.rs`** — a line-oriented fixed-width format (one record per line; pad/truncate each field to a fixed width; amounts as zero-padded cents). Records: `H` header, `C`/`D` detail (transaction code is the first field so returns can reuse the layout + append a reason), `T` trailer.

```rust
//! CPA-005-style fixed-width AFT file codec. Authentic in shape (header / detail
//! per entry / trailer with totals), round-trippable — not byte-exact to the
//! 1464-byte CPA-005 logical-record spec.

use rust_decimal::Decimal;

#[derive(Debug, Clone)]
pub struct Header { pub originator_id: String, pub created: String, pub file_seq: u32 }
#[derive(Debug, Clone)]
pub struct Detail {
    pub txn_code: char, pub amount: Decimal, pub institution: String, pub transit: String,
    pub account: String, pub payee_name: String, pub originator_short: String,
    pub due_date: String, pub return_reason: Option<String>,
}
#[derive(Debug, Clone)]
pub struct Trailer { pub entry_count: u32, pub total_credits: Decimal, pub total_debits: Decimal }

#[derive(Debug, thiserror::Error)]
pub enum CpaError { #[error("malformed CPA-005 file: {0}")] Malformed(String) }

fn field(s: &str, width: usize) -> String {
    let mut t: String = s.chars().take(width).collect();
    while t.len() < width { t.push(' '); }
    t
}
fn cents(a: Decimal) -> String { format!("{:010}", (a * Decimal::from(100)).round().mantissa()) }
fn parse_cents(s: &str) -> Decimal { Decimal::new(s.trim().parse::<i64>().unwrap_or(0), 2) }

pub fn encode(h: &Header, details: &[Detail], t: &Trailer) -> String {
    let mut out = String::new();
    out.push_str(&format!("H{}{}{:06}\n", field(&h.originator_id, 10), field(&h.created, 7), h.file_seq));
    for d in details {
        out.push_str(&format!(
            "{}{}{}{}{}{}{}{}{}\n",
            d.txn_code,                          // 1 (C|D)
            cents(d.amount),                     // 10
            field(&d.institution, 3),            // 3
            field(&d.transit, 5),                // 5
            field(&d.account, 12),               // 12
            field(&d.payee_name, 30),            // 30
            field(&d.originator_short, 4),       // 4
            field(&d.due_date, 7),               // 7
            field(d.return_reason.as_deref().unwrap_or(""), 4), // 4
        ));
    }
    out.push_str(&format!("T{:06}{}{}\n", t.entry_count, cents(t.total_credits), cents(t.total_debits)));
    out
}

pub fn decode(s: &str) -> Result<(Header, Vec<Detail>, Trailer), CpaError> {
    let mut header = None;
    let mut details = Vec::new();
    let mut trailer = None;
    for line in s.lines() {
        match line.chars().next() {
            Some('H') => header = Some(Header {
                originator_id: line[1..11].trim().to_string(),
                created: line[11..18].trim().to_string(),
                file_seq: line[18..24].trim().parse().unwrap_or(0),
            }),
            Some(c @ ('C' | 'D')) => details.push(Detail {
                txn_code: c,
                amount: parse_cents(&line[1..11]),
                institution: line[11..14].trim().to_string(),
                transit: line[14..19].trim().to_string(),
                account: line[19..31].trim().to_string(),
                payee_name: line[31..61].trim().to_string(),
                originator_short: line[61..65].trim().to_string(),
                due_date: line[65..72].trim().to_string(),
                return_reason: {
                    let r = line.get(72..76).unwrap_or("").trim();
                    if r.is_empty() { None } else { Some(r.to_string()) }
                },
            }),
            Some('T') => trailer = Some(Trailer {
                entry_count: line[1..7].trim().parse().unwrap_or(0),
                total_credits: parse_cents(&line[7..17]),
                total_debits: parse_cents(&line[17..27]),
            }),
            _ => return Err(CpaError::Malformed(format!("unknown record: {line}"))),
        }
    }
    Ok((
        header.ok_or_else(|| CpaError::Malformed("missing header".into()))?,
        details,
        trailer.ok_or_else(|| CpaError::Malformed("missing trailer".into()))?,
    ))
}
```
Wire `mod aft;` in `main.rs` and `pub mod cpa005;` in `aft/mod.rs`. (If `rust_decimal_macros` isn't a dependency, use `Decimal::new(12345, 2)` in the test instead of `dec!`.)

- [ ] **Step 4: Run — expect PASS** (`cargo test aft::cpa005::tests` → 2 pass). `cargo check`.

- [ ] **Step 5: Commit** — `feat(aft): CPA-005-style fixed-width file codec (round-trippable)`

---

## Task 4: AFT models (`models/aft.rs`)

**Files:** Create `api/src/models/aft.rs`; Modify `api/src/models/mod.rs` (`pub mod aft;`).

**Interfaces:** Produces the enums as `sqlx::Type` (mirror `models/interac.rs`'s `HandleType` derive pattern: `#[sqlx(type_name="aft_entry_kind", rename_all="snake_case")]` etc. for `EntryKind`, `AftDirection`, `BatchStatus`, `EntryStatus`, `MandateStatus`) and DTOs:
- `CreateMandateRequest { payer_account_id: Uuid, biller_name: String, originator_id: String, amount_cap: Decimal, frequency: Option<String> }`, `MandateResponse { mandate_id, biller_name, amount_cap, status: String }`.
- `CreateCreditRequest { originator_account_id: Uuid, amount: Decimal, counterparty_institution: String, counterparty_transit: String, counterparty_account: String, payee_name: String }`.
- `CreateDebitRequest { originator_account_id: Uuid, amount: Decimal, mandate_id: Uuid }` (the debit pulls from the mandate's payer into the originator/biller account).
- `BatchResponse { batch_id, status: String, entry_count: i32, total_credits: Decimal, total_debits: Decimal, file_ref: Option<String> }`, `EntryResponse { entry_id, kind: String, direction: String, amount: Decimal, status: String, payee_name: Option<String>, return_reason: Option<String> }`.

- [ ] **Step 1:** Write `models/aft.rs` with the enums + DTOs above (exact field names — later tasks depend on them). Register `pub mod aft;`.
- [ ] **Step 2:** `cargo check` (unused-DTO warnings ok). **Commit** — `feat(aft): request/response models + enums`.

---

## Task 5: Handler scaffold, routes & wiring

**Files:** Create `api/src/handlers/aft.rs`; Modify `api/src/handlers/mod.rs` (`pub mod aft;`), `api/src/main.rs` (`.nest("/api/v1/aft", handlers::aft::aft_routes())`).

**Interfaces:** Produces `handlers::aft::aft_routes() -> Router<AppState>` with all routes registered + stubs; shared helpers `resolve_aft(&AppState) -> Result<AftRail, AppError>` (via `ensure_aft_accounts`, mirroring `resolve_interac`), and a local `recompute_available(&mut PgTx, Uuid)` / `zero_available(&mut PgTx, Uuid)` (copy from `handlers/interac.rs` — the codebase already keeps per-module balance helpers, e.g. `cards::recompute_card_available`).

- [ ] **Step 1:** Write the scaffold mirroring `handlers/interac.rs`'s scaffold (Task 6 of the Interac plan). Routes:
```rust
Router::new()
    .route("/mandates", post(create_mandate).get(list_mandates))
    .route("/mandates/:id", delete(revoke_mandate))
    .route("/credits", post(create_credit))
    .route("/debits", post(create_debit))
    .route("/batches", get(list_batches))
    .route("/batches/:id/submit", post(submit_batch))
    .route("/entries", get(list_entries))
    .route("/network/settle/:batch", post(network_settle))
    .route("/network/inbound-batch", post(network_inbound_batch))
    .route("/network/returns", post(network_returns))
```
Each handler is a `todo` stub (`Err(AppError::Internal("todo".into()))`) replaced in later tasks. Copy `recompute_available`/`zero_available` from `handlers/interac.rs` verbatim. Config helpers: `settlement_window`/`return_window`/`aft_file_dir` reading `NANO_BANK__AFT__*` (default file dir `/tmp/nano-bank-aft`).
- [ ] **Step 2:** Wire `pub mod aft;` + the nest. `cargo check`.
- [ ] **Step 3: Commit** — `feat(aft): handler scaffold, routes, wiring`.

---

## Task 6: Mandate endpoints

**Files:** Modify `api/src/handlers/aft.rs`.

**Interfaces:** Consumes `CreateMandateRequest`, `MandateResponse`, `AuthenticatedCustomer`. Produces `POST /aft/mandates` (201; `payer_account_id` must belong to caller → else 404), `GET /aft/mandates` (caller's mandates), `DELETE /aft/mandates/:id` (revoke — set `status='revoked'`, `revoked_at`; caller-owned via the payer account → else 404, 204).

- [ ] **Step 1:** Implement the three handlers (ownership check: `EXISTS(SELECT 1 FROM accounts WHERE account_id=$payer AND customer_id=$caller)`; revoke via `UPDATE ... WHERE mandate_id=$1 AND payer_account_id IN (SELECT account_id FROM accounts WHERE customer_id=$caller) AND status='active'`).
- [ ] **Step 2:** `cargo check`; smoke: create a mandate for a caller's account → 201; for someone else's account → 404; list → 1; revoke → 204; re-revoke → 404. Paste outputs.
- [ ] **Step 3: Commit** — `feat(aft): PAD mandate endpoints (create/list/revoke)`.

---

## Task 7: Originate credits + debits

**Files:** Modify `api/src/handlers/aft.rs`.

**Interfaces:** Consumes `CreateCreditRequest`, `CreateDebitRequest`, `EntryResponse`. Produces `POST /aft/credits` and `POST /aft/debits` — each appends an `aft_entries` row to the single **open** outbound batch (create one if none open), updates the batch counts/totals, returns 201 + `EntryResponse` (status `pending`). No money moves yet (that happens at settlement). A debit MUST cite an `active` mandate whose `payer_account_id` and `amount_cap` cover it, else 400/404.

- [ ] **Step 1:** Implement. Shared helper `open_batch(&mut tx) -> Uuid` (`SELECT batch_id FROM aft_batches WHERE status='open' AND direction='outbound' FOR UPDATE` or insert one). For credits: `originator_account_id` must belong to caller (404 else); insert entry `kind='credit', direction='outbound'`. For debits: validate the mandate (`SELECT ... WHERE mandate_id=$1 AND status='active'`; 404 if missing/revoked; 400 if `amount > amount_cap`); insert entry `kind='debit', direction='outbound', mandate_id=…`, originator = the biller's account (caller's account). Update `aft_batches.entry_count/total_credits/total_debits`.
- [ ] **Step 2:** `cargo check`; smoke: queue a credit → 201 pending, batch totals update; queue a debit citing an active mandate → 201; a debit over the cap → 400; a debit with a revoked mandate → 404. Paste outputs + `SELECT status, entry_count, total_credits, total_debits FROM aft_batches`.
- [ ] **Step 3: Commit** — `feat(aft): originate direct-deposit credits + PAD debits into the open batch`.

---

## Task 8: Submit batch + emit CPA-005 file

**Files:** Modify `api/src/handlers/aft.rs`.

**Interfaces:** Consumes `cpa005::{Header, Detail, Trailer, encode}`, `BatchResponse`. Produces `POST /aft/batches/:id/submit` (200) — lock the batch `FOR UPDATE` (must be `open`, else 409), build `Detail`s from its `aft_entries`, `encode` a CPA-005 file, write it to `aft_file_dir()/<batch_id>.005`, set `aft_batches.file_ref`, `status='submitted'`, `cutoff_at=now()`. Returns `BatchResponse` with `file_ref`.

- [ ] **Step 1:** Implement: read entries, map each to a `Detail` (`txn_code` `'C'`/`'D'` from `kind`; institution/transit/account/payee from the entry), compute trailer totals, `encode`, `std::fs::write` the file, update the batch. Guard: `open`-only (409 otherwise).
- [ ] **Step 2:** `cargo check`; smoke: submit the open batch → 200, `file_ref` set; `cat` the file and confirm H/detail/T lines; re-submit → 409. Paste the file contents.
- [ ] **Step 3: Commit** — `feat(aft): submit batch → emit CPA-005 file`.

---

## Task 9: Network settle + settlement sweep

**Files:** Modify `api/src/handlers/aft.rs`.

**Interfaces:** Consumes `AuthenticatedService`, `AftRail::{hold, release}`, `Destination`, `recompute_available`, `zero_available`. Produces `POST /aft/network/settle/:batch` (200) — the ACSS simulator settles a `submitted` batch. Lock the batch (`submitted`-only → 409). For EACH `pending` entry, in the batch tx: apply the settlement legs (per §5 of the spec — credit-out: `zero_available(originator)` then `hold(originator)` then `release(External)`; debit-out: `accept_inbound(biller)` + `recompute_available(biller)`), mark the entry `settled` + record `settle_transaction_id`. Then the **sweep**: post the net `Bank`/`Payable` GL via `post_gl_entry` (net = total_credits − total_debits; direction by sign) and move the in-flight net `AFT_CLEARING`→`AFT_SETTLEMENT` with `post_two_legged`. Set `status='settled'`, `settled_at`. Returns `{ "settled_entries": n, "net": … }`.

- [ ] **Step 1:** Implement. For a credit-out entry the originator is a nano-bank customer being debited → `zero_available` before, `recompute_available` after (it's a debit). For a debit-out (collection) the biller is credited → `recompute_available(biller)` after. **Never** recompute the AFT system accounts. Compute `net` from the batch totals; post one aggregate GL sweep entry (Dr `Bank`/Cr `Payable` if net cash in, reverse if out — follow the sign like `cards::settle` computes `net = -clearing.balance`). Guard `submitted`-only.
- [ ] **Step 2:** `cargo check`; smoke: after Task 8's submit, `POST /network/settle/:batch` (service token) → 200; originator debited (balance + available), `AFT_CLEARING`/`AFT_SETTLEMENT` moved, batch `settled`; re-settle → 409; customer token → 403. Paste DB balance checks.
- [ ] **Step 3: Commit** — `feat(aft): network settle + settlement-window sweep`.

---

## Task 10: Network inbound-batch (ingest CPA-005)

**Files:** Modify `api/src/handlers/aft.rs`.

**Interfaces:** Consumes `AuthenticatedService`, `cpa005::decode`, `AftRail::{accept_inbound, hold}`, `recompute_available`, `zero_available`. Produces `POST /aft/network/inbound-batch` (201) — body carries a CPA-005 file (as a string field, e.g. `{ "file": "H…\nC…\nT…" }`); `decode` it; create an `inbound` batch + entries; for each detail resolve the target nano-bank account by `(institution=900, transit, account)` (404 the whole file if a target is unknown, or skip+return-count unknowns — pick skip-with-count). A `C` detail → `accept_inbound` (credit the customer) + `recompute_available`. A `D` detail (external biller pulling a nano-bank customer) → `zero_available` + `hold` (debit the customer) → `release`, unless insufficient funds → mark the entry `rejected` (a return). Returns counts `{ "credited": n, "debited": n, "rejected": n }`.

- [ ] **Step 1:** Implement (resolve account: `SELECT account_id, customer_id FROM accounts WHERE institution_number=$inst AND transit_number=$transit AND account_number=$acct`). For debit rejections (NSF), don't post; set entry `rejected` + `return_reason='NSF'`.
- [ ] **Step 2:** `cargo check`; smoke: build a CPA-005 file crediting a seeded customer's account coords, POST it → 201 `credited:1`, customer balance+available +amount, `AFT_SETTLEMENT` moved; a debit exceeding funds → `rejected:1`. Paste outputs.
- [ ] **Step 3: Commit** — `feat(aft): ingest inbound CPA-005 batch (credits + PAD debits w/ NSF reject)`.

---

## Task 11: Network returns (ingest returns file → reverse)

**Files:** Modify `api/src/handlers/aft.rs`.

**Interfaces:** Consumes `AuthenticatedService`, `cpa005::decode`, `AftRail::refund`/`post_two_legged`+`post_gl_entry`, `recompute_available`. Produces `POST /aft/network/returns` (200) — body carries a returns CPA-005 file whose details carry a `return_reason`; for each, find the matching **settled** `aft_entries` row (by amount + counterparty coords, or an entry id echoed in the file — use the payee/account match), reverse its settlement (post the mirror legs of the original settle + reverse the aggregate GL), `recompute_available` on the affected customer, set entry `status='returned'` + `return_reason`, record `return_transaction_id`. Guard: only reverse `settled` entries (skip others). Returns `{ "returned": n }`.

- [ ] **Step 1:** Implement the reversal. For a returned credit-out (funds go back to the originator): Dr `AFT_SETTLEMENT` / Cr originator + `recompute_available(originator)`. For a returned debit-out collection: Dr biller / Cr `AFT_SETTLEMENT` + `recompute_available(biller)`. Reverse the aggregate GL accordingly. Match entries via a stable key — include the `entry_id` in the emitted file's `originator_short`/a spare field, or match on `(amount, counterparty_account, status='settled')` and take the oldest.
- [ ] **Step 2:** `cargo check`; smoke: settle a batch, then POST a returns file referencing one settled debit → 200 `returned:1`, entry `returned`, the affected customer's balance+available reversed. Paste outputs.
- [ ] **Step 3: Commit** — `feat(aft): ingest returns file → reverse settled entries`.

---

## Task 12: GET batches + entries

**Files:** Modify `api/src/handlers/aft.rs`.

**Interfaces:** Produces `GET /aft/batches` (recent batches, newest first, optional `?status=`) and `GET /aft/entries` (the caller's entries — where `originator_account_id` belongs to the caller — optional `?status=` / `?batch=`). Mirror `handlers::interac::{list_etransfers, get_etransfer}` ownership scoping.

- [ ] **Step 1:** Implement (parameterized filters, no interpolation). Batches are bank-level (not customer-scoped); entries are scoped to the caller's originator accounts.
- [ ] **Step 2:** `cargo check`; smoke: `GET /aft/batches?status=settled`, `GET /aft/entries` returns the caller's. **Commit** — `feat(aft): list batches + entries`.

---

## Task 13: Bruno `7_AFT` collection

**Files:** Create `bruno/7_AFT/*.bru`; Modify `bruno/environments/local.bru` (add `aftBatchId`, `mandateId`).

- [ ] **Step 1:** Author `.bru` files (Create/List/Revoke Mandate, Create Credit, Create Debit, Submit Batch, Network Settle, Network Inbound Batch, Network Returns, List Batches, List Entries) mirroring `bruno/6_Interac/` format (each `post {}` block declares `body: json` + the right `auth:` line — customer `auth: inherit`, network `auth: bearer` + `{{serviceToken}}`). **Commit** — `test(aft): Bruno collection`.

---

## Task 14: ACSS simulator (`testing/aft/`)

**Files:** Create `testing/aft/aft_simulator.py`, `testing/aft/Containerfile`, `testing/aft/requirements.txt`; Modify `testing/run-testing.sh` + `testing/stop-testing.sh` (mirror `testing/interac/` and the visa wiring — Containerfile, no docker-compose).

**Interfaces:** Plays ACSS. Loop: (1) poll `aft_batches` for `status='submitted'`, read `file_ref`, call `POST /aft/network/settle/{batch}` (service token); (2) occasionally originate an inbound batch — build a CPA-005 file crediting seeded customer account coords and `POST /aft/network/inbound-batch`; (3) for a fraction of settled debits, build a returns file (detail + `return_reason='NSF'`) and `POST /aft/network/returns`. Mirror `testing/interac/interac_simulator.py` (service-token mint, psycopg2 DB config, health-wait, logging). Use only seeded institution codes (`001/002/003/004/010`, `900` self).

- [ ] **Step 1:** Write it; `python3 -c "import ast; ast.parse(open('testing/aft/aft_simulator.py').read())"`. **Step 2:** Run directly against the live API with a submitted batch present; confirm it settles it and originates an inbound batch (paste log). **Step 3: Commit** — `test(aft): ACSS simulator (settle/inbound/returns)`.

---

## Task 15: Viewer AFT tab

**Files:** Modify `testing/viewer/app.py`.

- [ ] **Step 1:** Add `render_aft()` (mirror `render_interac`): batches by status, `AFT_CLEARING`/`AFT_SETTLEMENT` balances (join on `aft@nano.bank`), recent entries (kind/status/amount/return_reason), mandates. Wire into `st.tabs`. **Step 2:** `python3 -c "import ast; ast.parse(...)"` + run each new SQL via psql. **Step 3: Commit** — `test(aft): viewer AFT tab`.

---

## Task 16: Docs

**Files:** Modify `CLAUDE.md`, `api/CLAUDE.md`, `README.md`.

- [ ] **Step 1:** Add an "AFT/EFT rail" section (root `CLAUDE.md`) — the batch lifecycle (originate → submit → CPA-005 file → settle+sweep → returns), the `aft@nano.bank` accounts, mandates, the CPA-005 codec, three auth planes; note the settlement sweep is AFT's (decoupled from cards). `api/CLAUDE.md`: the `aft/` codec + `rails/aft.rs` + `handlers/aft.rs`. `README.md`: an AFT endpoints row. Cross-reference `docs/specs/2026-07-05-aft-eft-rail-design.md`. **Step 2: Commit** — `docs(aft): document the AFT/EFT batch rail`.

---

## Self-Review

**Spec coverage:** foundation/accounts → T2; CPA-005 file → T3; mandates → T1,T6; credits+debits origination → T7; batch submit+file → T8; settle+sweep → T9; inbound → T10; returns → T11; queries → T12; simulator/viewer/bruno/docs → T13–16. Both products (credits+PAD), returns, and the deferred sweep are all covered. ✓

**Type consistency:** `AftAccounts{clearing_id,settlement_id}`, `AftRail`, `RailId::Aft`, the `cpa005` types, and the DTO field names are used identically across tasks. The `Rail` verbs match `rails/mod.rs`.

**Placeholder note:** the Task 5 handler stubs and the "mirror `rails/interac.rs`" template references point at real committed files (not intra-plan forward-refs); each stub is replaced in its own task. Full code is given for the novel pieces (schema, CPA-005 codec, settle/sweep, returns, inbound); mechanical tasks (rail impl, scaffold, models, bruno, simulator, viewer) reference the committed Interac equivalents with explicit AFT deltas.

**Known follow-ups (carried from the spec §10):** promote the sweep to the `Rail` trait when Lynx lands; give Interac its own sweep; shared `account_limits` (pending PR #15).
