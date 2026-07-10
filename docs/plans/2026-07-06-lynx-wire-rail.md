# Lynx Wire Rail — Implementation Plan

> **For agentic workers:** Implement task-by-task. Steps use checkbox (`- [ ]`)
> syntax for tracking. Each task ends with an independently testable deliverable.

**Goal:** Add Lynx (Canada's RTGS high-value wire system) as the third external
payment rail, built on the existing `Rail` port — outbound + inbound wires,
two-step settlement with finality, ISO 20022 messaging, and recall both ways.

**Architecture:** A peer rail to Interac and AFT, fully decoupled, mirroring
AFT's layout. `LynxRail` implements the `Rail` port over its own
`lynx@nano.bank` clearing/settlement accounts; a dedicated `iso20022.rs` codec
handles pacs.008/pacs.009/camt.056/camt.029; `handlers/lynx.rs` owns the wire
lifecycle across three auth planes (customer / network / admin).

**Tech Stack:** Rust (axum 0.7, sqlx 0.7, rust_decimal), PostgreSQL 16 (Kind),
the swappable Ledger core (`CORE_BACKEND=modern` on `:8091`), Python
(simulator + Streamlit viewer), Bruno.

## Global Constraints

- **CAD only**; money is `rust_decimal::Decimal`, never floats.
- **Do NOT modify `api/src/handlers/transactions.rs`** (PR-coexistence rule).
- **Decoupled from the other rails** — Lynx uses its OWN `lynx@nano.bank`
  system accounts; it does not reuse Interac's, AFT's, or the cards' accounts.
- **High-value floor**: minimum wire amount, configurable via
  `NANO_BANK__LYNX__MIN_AMOUNT`, default **`10000.00`**; no ceiling. Wires
  bypass the retail `account_limits` counters.
- **Auth planes**: customer endpoints require `AuthenticatedCustomer`;
  `/network/*` and `/admin/*` require `AuthenticatedService`. Cross-customer
  access returns **404**, never 403.
- **GL through the Ledger port stays inside the DB transaction, before commit**
  (503 + rollback if the core is down) — no subledger/GL drift.
- **`available_balance`**: recompute only on **customer** accounts
  (`zero_available` before a customer debit, `recompute_available` after); NEVER
  on the system `LYNX_CLEARING` / `LYNX_SETTLEMENT` accounts (kept at 0).
- Built **inline** (no background agents). Plan/spec in `docs/`; no "superpowers"
  branding in code, commits, or docs.
- Branch `lynx-rail`, stacked on `aft-rail`; the stack rebases onto `main` after
  the Interac PR (#18) merges.

## Templates (already committed on this branch — read them; mechanical tasks mirror them)

- `api/src/rails/aft.rs`, `api/src/rails/interac.rs` — `Rail` impls (system
  accounts, `ensure_*_accounts`, `new_txn`, `tag_gl`, the four verbs).
- `api/src/handlers/aft.rs` — three planes, `resolve_aft`, `zero_available` /
  `recompute_available`, `caller_owns_account`, guarded status transitions,
  `AuthenticatedService` network handlers.
- `api/src/models/aft.rs` — `sqlx::Type` enum pattern + `Validate` DTOs.
- `api/src/aft/cpa005.rs` — codec with inline `#[cfg(test)]` round-trip tests.
- `testing/aft/aft_simulator.py` + `Containerfile` — the network simulator shape.
- `testing/viewer/app.py` — the per-rail viewer tab pattern (`render_aft`).
- `bruno/7_AFT/` + `bruno/environments/local.bru` — request collection pattern.

## Smoke bootstrap (once, before the handler tasks)

Have the stack up: Kind Postgres reachable on `localhost:30432`
(`kubectl --context kind-nano-bank port-forward -n nano-bank svc/postgres-service 30432:5432 &`),
the modern core on `:8091`, and the API on `:8081`
(`cd api && CORE_BACKEND=modern MODERN_CORE_URL=http://localhost:8091 cargo run`).
Seed two customers with a chequing account each and note their account ids +
`institution_number`/`transit_number`/`account_number` (via `POST /api/v1/customers`
and `POST /api/v1/accounts`, logging in for a bearer token — see `bruno/`). Mint a
service token the same way the AFT smoke did (the `AuthenticatedService` plane).

---

## Task 1: Lynx schema (`10_lynx.sql`)

**Files:** Create `src/core/tables/10_lynx.sql`.

- [ ] **Step 1: Write the DDL**

```sql
-- Nano Bank Core Database Schema — Part 10: Lynx RTGS high-value wire rail

CREATE TYPE lynx_direction    AS ENUM ('outbound', 'inbound');
CREATE TYPE lynx_wire_status  AS ENUM ('sent', 'settled', 'rejected', 'recalled');
CREATE TYPE lynx_recall_status AS ENUM ('requested', 'accepted', 'rejected');

CREATE TABLE lynx_wires (
    wire_id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    uetr                      UUID NOT NULL UNIQUE,            -- ISO 20022 end-to-end ref
    direction                 lynx_direction NOT NULL,
    status                    lynx_wire_status NOT NULL DEFAULT 'sent',
    local_account_id          UUID NOT NULL REFERENCES accounts(account_id),
    counterparty_name         VARCHAR(140) NOT NULL,
    counterparty_institution  VARCHAR(3) NOT NULL REFERENCES rail_participants(institution_number),
    counterparty_account      VARCHAR(34) NOT NULL,
    amount                    DECIMAL(19,4) NOT NULL,
    currency                  VARCHAR(3) NOT NULL DEFAULT 'CAD',
    remittance_info           VARCHAR(140),
    message_type              VARCHAR(12) NOT NULL DEFAULT 'pacs.008',
    settlement_transaction_id UUID REFERENCES transactions(transaction_id),
    gl_entry                  VARCHAR(120),
    initiated_by              UUID REFERENCES customers(customer_id),
    reference_number          VARCHAR(50) NOT NULL UNIQUE,
    created_at                TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    sent_at                   TIMESTAMP WITH TIME ZONE,
    settled_at                TIMESTAMP WITH TIME ZONE,
    CONSTRAINT chk_lynx_amount_positive  CHECK (amount > 0),
    CONSTRAINT chk_lynx_amount_precision CHECK (amount = ROUND(amount, 4)),
    CONSTRAINT chk_lynx_currency_cad     CHECK (currency = 'CAD')
);
CREATE INDEX idx_lynx_wires_status ON lynx_wires (status);
CREATE INDEX idx_lynx_wires_local  ON lynx_wires (local_account_id);
CREATE INDEX idx_lynx_wires_initiator ON lynx_wires (initiated_by);

CREATE TABLE lynx_messages (
    message_id   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    wire_id      UUID NOT NULL REFERENCES lynx_wires(wire_id) ON DELETE CASCADE,
    message_type VARCHAR(12) NOT NULL,     -- pacs.008 | pacs.009 | camt.056 | camt.029
    flow         VARCHAR(8)  NOT NULL,     -- emitted | received
    payload      TEXT NOT NULL,            -- the ISO 20022 XML
    created_at   TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT chk_lynx_msg_flow CHECK (flow IN ('emitted','received'))
);
CREATE INDEX idx_lynx_messages_wire ON lynx_messages (wire_id);

CREATE TABLE lynx_recalls (
    recall_id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    wire_id            UUID NOT NULL REFERENCES lynx_wires(wire_id) ON DELETE CASCADE,
    direction          lynx_direction NOT NULL,   -- who initiated the recall
    requested_by       UUID REFERENCES customers(customer_id),
    reason             VARCHAR(140),
    status             lynx_recall_status NOT NULL DEFAULT 'requested',
    resolution_reason  VARCHAR(140),
    camt056_message_id UUID REFERENCES lynx_messages(message_id),
    camt029_message_id UUID REFERENCES lynx_messages(message_id),
    created_at         TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    resolved_at        TIMESTAMP WITH TIME ZONE
);
CREATE INDEX idx_lynx_recalls_wire ON lynx_recalls (wire_id);
CREATE INDEX idx_lynx_recalls_status ON lynx_recalls (status);
```

- [ ] **Step 2: Apply & verify** — apply the DDL to the running DB and confirm the
  three tables + three enums exist:

```bash
kubectl exec -n nano-bank deployment/postgres -- \
  psql -U nanobank_user -d nano_bank_db -f - < src/core/tables/10_lynx.sql
kubectl exec -n nano-bank deployment/postgres -- \
  psql -U nanobank_user -d nano_bank_db -c '\dt lynx_*'
```
Expected: `lynx_wires`, `lynx_messages`, `lynx_recalls` listed. (The Kind init
Job loads `src/core/tables/*.sql` in order on a fresh cluster; applying by hand
keeps the running DB current.)

- [ ] **Step 3: Commit** — `git add src/core/tables/10_lynx.sql && git commit -m "feat(lynx): RTGS wire-rail schema — wires, messages, recalls"`

---

## Task 2: ISO 20022 codec (`lynx/iso20022.rs`)

**Files:** Create `api/src/lynx/mod.rs` (`pub mod iso20022;`), `api/src/lynx/iso20022.rs`; Modify `api/src/main.rs` (add `mod lynx;` beside `mod aft;`).

**Interfaces produced (later tasks depend on these exact names):**
- `pub struct Pacs008 { pub uetr: String, pub debtor_name: String, pub debtor_agent: String, pub debtor_account: String, pub creditor_name: String, pub creditor_agent: String, pub creditor_account: String, pub amount: Decimal, pub currency: String, pub remittance: Option<String> }`
- `pub struct Pacs009 { … same fields as Pacs008 … }` (FI transfer; identical shape here).
- `pub struct Camt056 { pub uetr: String, pub original_uetr: String, pub reason: String }`
- `pub struct Camt029 { pub uetr: String, pub original_uetr: String, pub status: String, pub reason: Option<String> }` (`status` = `"ACCP"` | `"RJCR"`).
- `pub fn encode_pacs008(m: &Pacs008) -> String`, `pub fn decode_pacs008(s: &str) -> Result<Pacs008, Iso20022Error>`; the same `encode_*`/`decode_*` pair for `pacs009`, `camt056`, `camt029`.
- `pub enum Iso20022Error { Malformed(String) }` (`#[derive(Debug, thiserror::Error)]`).

- [ ] **Step 1: Write the failing round-trip tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    #[test]
    fn pacs008_round_trips() {
        let m = Pacs008 {
            uetr: "11111111-1111-1111-1111-111111111111".into(),
            debtor_name: "Alice Payer".into(), debtor_agent: "900".into(),
            debtor_account: "000000000123".into(),
            creditor_name: "Bob Payee".into(), creditor_agent: "001".into(),
            creditor_account: "000000000456".into(),
            amount: Decimal::new(2500000, 2), currency: "CAD".into(),
            remittance: Some("invoice 42".into()),
        };
        assert_eq!(decode_pacs008(&encode_pacs008(&m)).unwrap(), m);
    }

    #[test]
    fn camt056_and_029_round_trip() {
        let r = Camt056 { uetr: "22222222-2222-2222-2222-222222222222".into(),
            original_uetr: "11111111-1111-1111-1111-111111111111".into(),
            reason: "DUPL".into() };
        assert_eq!(decode_camt056(&encode_camt056(&r)).unwrap(), r);
        let a = Camt029 { uetr: "33333333-3333-3333-3333-333333333333".into(),
            original_uetr: "11111111-1111-1111-1111-111111111111".into(),
            status: "ACCP".into(), reason: None };
        assert_eq!(decode_camt029(&encode_camt029(&a)).unwrap(), a);
    }
}
```
Derive `PartialEq` on all four structs so `assert_eq!` compiles.

- [ ] **Step 2: Run — expect FAIL** — `cd api && cargo test lynx::iso20022::tests` → "cannot find function `encode_pacs008`".

- [ ] **Step 3: Implement `iso20022.rs`** — authentic-shape ISO 20022 XML,
  round-trippable, NOT schema-validated (same philosophy as `cpa005.rs`). Build a
  small element writer/reader: `encode_*` emits a document like

```xml
<Document xmlns="urn:iso:std:iso:20022:tech:xsd:pacs.008.001.08"><FIToFICstmrCdtTrf>
  <CdtTrfTxInf><PmtId><UETR>…</UETR></PmtId>
    <IntrBkSttlmAmt Ccy="CAD">25000.00</IntrBkSttlmAmt>
    <Dbtr><Nm>Alice Payer</Nm></Dbtr><DbtrAcct><Id><Othr><Id>000000000123</Id></Othr></Id></DbtrAcct>
    <DbtrAgt><FinInstnId><ClrSysMmbId><MmbId>900</MmbId></ClrSysMmbId></FinInstnId></DbtrAgt>
    <Cdtr><Nm>Bob Payee</Nm></Cdtr><CdtrAcct><Id><Othr><Id>000000000456</Id></Othr></Id></CdtrAcct>
    <CdtrAgt><FinInstnId><ClrSysMmbId><MmbId>001</MmbId></ClrSysMmbId></FinInstnId></CdtrAgt>
    <RmtInf><Ustrd>invoice 42</Ustrd></RmtInf>
  </CdtTrfTxInf></FIToFICstmrCdtTrf></Document>
```
  `decode_*` extracts each element's text by tag. Implement a tiny helper
  `fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str>` (find `<name>`…`</name>`,
  return the inner slice) and an attribute reader for `Ccy`. Amount is formatted
  with 2 decimals (`format!("{:.2}", amount)`) and parsed with `Decimal::from_str`.
  `pacs009` reuses the same body under `urn:…:pacs.009.001.08` /
  `<FICdtTrf>`. `camt056` → `<FIToFIPmtCxlReq>` with `<Case><Id>uetr</Id></Case>`,
  `<Undrlyg>…<OrgnlUETR>original</OrgnlUETR>…</Undrlyg>`, `<CxlRsnInf><Rsn><Cd>reason</Cd></Rsn></CxlRsnInf>`.
  `camt029` → `<RsltnOfInvstgtn>` with `<Sts><Conf>ACCP|RJCR</Conf></Sts>` +
  the original UETR. A missing required tag → `Err(Iso20022Error::Malformed(...))`.

- [ ] **Step 4: Run — expect PASS** — `cargo test lynx::iso20022::tests` → 2 pass. `cargo check`.

- [ ] **Step 5: Commit** — `feat(lynx): ISO 20022 codec (pacs.008/009, camt.056/029, round-trippable)`

---

## Task 3: `LynxRail` (impl `Rail`) + Lynx system accounts

**Files:** Create `api/src/rails/lynx.rs`; Modify `api/src/rails/mod.rs` (`pub mod lynx;`), `api/src/main.rs` (startup bootstrap).

**Interfaces produced:**
- `pub struct LynxAccounts { pub clearing_id: Uuid, pub settlement_id: Uuid }`
- `pub struct LynxRail { pub accounts: LynxAccounts }` with `pub fn new(a: LynxAccounts) -> Self` and `pub fn id(&self) -> RailId { RailId::Lynx }`.
- `pub async fn ensure_lynx_accounts(pool: &DatabasePool) -> Result<LynxAccounts, sqlx::Error>`.
- `impl Rail for LynxRail` (the four verbs).
- Inherent `pub async fn clawback(&self, state, tx, from: Uuid, amount: Decimal, description: &str) -> Result<RailPosting, AppError>` — for inbound-recall accept.

- [ ] **Step 1: Write `rails/lynx.rs`** by copying `rails/aft.rs` and changing:
  system customer `lynx@nano.bank`, phone `+10000000004`, SIN `000000004`,
  `tracing` label "Lynx"; `CLEARING_TYPE = "chequing"` (LYNX_CLEARING),
  `SETTLEMENT_TYPE = "savings"` (LYNX_SETTLEMENT); reference prefixes and txn
  types below. The **GL choices differ from AFT** — set them exactly:

  | Verb | Local legs | GL (debit → credit) | ref / txn_type |
  |---|---|---|---|
  | `hold` | Dr `from` / Cr CLEARING | `Payable` → `Payable` | `LYNXH` / `lynx_hold` |
  | `release` (External) | Dr CLEARING / Cr SETTLEMENT | **`Payable` → `Bank`** | `LYNXS` / `lynx_settle` |
  | `refund` | Dr CLEARING / Cr `hold.from_account` | `Payable` → `Payable` | `LYNXX` / `lynx_refund` |
  | `accept_inbound` | Dr SETTLEMENT / Cr `to` | **`Bank` → `Payable`** | `LYNXI` / `lynx_inbound` |
  | `clawback` (inherent) | Dr `from` / Cr SETTLEMENT | **`Payable` → `Bank`** | `LYNXC` / `lynx_clawback` |

  For `release`, credit SETTLEMENT for `Destination::External(_)`; for the unused
  `Destination::Internal(a)` credit `a` with `Payable`→`Payable` (kept for trait
  completeness). `clawback` mirrors `refund` but debits the customer (`from`) and
  credits SETTLEMENT with GL `Payable`→`Bank`. Reuse `new_txn` and `tag_gl`
  copied from `aft.rs`.

- [ ] **Step 2: Wire** `pub mod lynx;` in `rails/mod.rs`; in `main.rs`, after the
  AFT bootstrap block, add:

```rust
if let Err(e) = rails::lynx::ensure_lynx_accounts(&pool).await {
    tracing::error!("failed to ensure Lynx system accounts: {e}");
}
```

- [ ] **Step 3: Verify** — `cd api && cargo check` (unused-until-later warnings ok).
  Run the server and confirm the two accounts exist:

```bash
kubectl exec -n nano-bank deployment/postgres -- psql -U nanobank_user -d nano_bank_db -c \
 "SELECT a.account_type, a.overdraft_limit FROM accounts a JOIN customers c USING (customer_id) WHERE c.email='lynx@nano.bank';"
```
Expected: `chequing` + `savings`, each `overdraft_limit = 1000000000000`.

- [ ] **Step 4: Commit** — `feat(lynx): LynxRail (impl Rail) + Lynx system accounts + clawback`

---

## Task 4: Lynx models (`models/lynx.rs`)

**Files:** Create `api/src/models/lynx.rs`; Modify `api/src/models/mod.rs` (`pub mod lynx;`).

- [ ] **Step 1:** Write the enums (mirror `models/aft.rs`'s `sqlx::Type` pattern)
  and DTOs with these exact names:

```rust
// enums: #[sqlx(type_name="lynx_direction", rename_all="snake_case")] + #[serde(rename_all="snake_case")]
pub enum LynxDirection { Outbound, Inbound }
// type_name="lynx_wire_status"
pub enum LynxWireStatus { Sent, Settled, Rejected, Recalled }
// type_name="lynx_recall_status"
pub enum LynxRecallStatus { Requested, Accepted, Rejected }

#[derive(Debug, Deserialize, Validate)]
pub struct InitiateWireRequest {
    pub from_account_id: Uuid,
    pub amount: Decimal,
    #[validate(length(min = 1, max = 140))] pub counterparty_name: String,
    #[validate(length(min = 3, max = 3))]   pub counterparty_institution: String,
    #[validate(length(min = 1, max = 34))]  pub counterparty_account: String,
    #[validate(length(max = 140))]          pub remittance_info: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct WireResponse {
    pub wire_id: Uuid, pub uetr: Uuid, pub direction: String, pub status: String,
    pub amount: Decimal, pub currency: String, pub counterparty_name: String,
    pub counterparty_institution: String, pub message_type: String,
    pub reference_number: String, pub gl_entry: Option<String>,
}

#[derive(Debug, Deserialize, Validate)]
pub struct RecallRequest { #[validate(length(max = 140))] pub reason: Option<String> }

#[derive(Debug, Deserialize)]
pub struct NetworkInboundRequest {
    pub debtor_name: String, pub debtor_institution: String, pub debtor_account: String,
    pub beneficiary_institution: String, pub beneficiary_transit: String,
    pub beneficiary_account: String, pub amount: Decimal,
    pub remittance_info: Option<String>, pub message_type: Option<String>, // pacs.008|pacs.009
    pub uetr: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct RecallResolveRequest { pub decision: String, pub reason: Option<String> } // accept|reject

#[derive(Debug, Deserialize)]
pub struct InboundRecallRequest { pub wire_id: Uuid, pub decision: String, pub reason: Option<String> }
```
  Register `pub mod lynx;`.

- [ ] **Step 2:** `cargo check` (unused-DTO warnings ok). **Commit** — `feat(lynx): request/response models + enums`.

---

## Task 5: Handler scaffold, routes, wiring & helpers

**Files:** Create `api/src/handlers/lynx.rs`; Modify `api/src/handlers/mod.rs` (`pub mod lynx;`), `api/src/main.rs` (`.nest("/api/v1/lynx", handlers::lynx::lynx_routes())`).

- [ ] **Step 1:** Write the scaffold mirroring `handlers/aft.rs`. Routes:

```rust
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
```
  Copy into this module: `resolve_lynx(state) -> LynxRail` (calls
  `ensure_lynx_accounts`), `zero_available`, `recompute_available`,
  `caller_owns_account` (all from `aft.rs`), plus:

```rust
/// Minimum wire amount (high-value floor). Configurable; default $10,000.
fn min_amount() -> Decimal {
    std::env::var("NANO_BANK__LYNX__MIN_AMOUNT").ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| Decimal::new(1000000, 2))
}

/// Load a wire as WireResponse (used by every handler's return).
async fn load_wire(state: &AppState, wire_id: Uuid) -> Result<WireResponse, AppError> { /* SELECT … */ }
```
  Add stub bodies (`todo!()`-free: return `AppError::Internal("unimplemented".into())`)
  so it compiles.

- [ ] **Step 2:** Wire `pub mod lynx;` + the nest. `cargo check`; `curl -s localhost:8081/api/v1/lynx/wires` with a bearer token returns something (not 404 routing).

- [ ] **Step 3: Commit** — `feat(lynx): handler scaffold, routes, wiring, helpers`.

---

## Task 6: Initiate outbound wire (`POST /wires`)

**Files:** Modify `api/src/handlers/lynx.rs`.

- [ ] **Step 1:** Implement `initiate_wire`:
  - `req.validate()?`; `amount = normalize_amount(req.amount)?`; reject `< min_amount()` → `AppError::BadRequest("amount below high-value floor")`.
  - `caller_owns_account(from_account_id, caller)` else 404; `fetch_account_for_update` + `ensure_operable`; `available_balance >= amount` else `InsufficientFunds`.
  - Counterparty institution must exist, be `active`, and `supports_lynx` (`SELECT supports_lynx, active FROM rail_participants WHERE institution_number=$1`) else `BadRequest`.
  - `rail = resolve_lynx(&state)`; `let mut tx = begin()`; `zero_available(from)`; `hold = rail.hold(&state,&mut tx, from, amount, "Lynx wire")`; `recompute_available(from)`.
  - Mint `uetr = Uuid::new_v4()`; build a `Pacs008` (debtor = the caller's account coords: `institution_number`/`account_number` from the locked row; creditor = the request fields); `payload = iso20022::encode_pacs008(&m)`.
  - Insert `lynx_wires` (`direction='outbound', status='sent', message_type='pacs.008', local_account_id=from, settlement_transaction_id=hold.transaction_id, gl_entry=hold-derived, initiated_by=caller, reference_number=hold.reference, sent_at=now`). Insert `lynx_messages` (`message_type='pacs.008', flow='emitted', payload`).
  - `tx.commit()`; return `201` + `load_wire`.

- [ ] **Step 2:** `cargo check`; smoke: send `$25,000` from customer A's chequing to institution `001` → 201, `status=sent`, A's balance/available down $25k, `LYNX_CLEARING` up $25k; a `$500` wire → 400 (below floor); a wire to institution `777` (absent) → 400. Paste outputs + a `SELECT status, amount FROM lynx_wires` and the emitted `SELECT message_type,flow FROM lynx_messages`.

- [ ] **Step 3: Commit** — `feat(lynx): initiate outbound wire (hold + emit pacs.008)`.

---

## Task 7: List + get wires (ownership-scoped)

**Files:** Modify `api/src/handlers/lynx.rs`.

- [ ] **Step 1:** `list_wires` — `SELECT … FROM lynx_wires WHERE local_account_id IN
  (SELECT account_id FROM accounts WHERE customer_id=$caller) ORDER BY created_at DESC`.
  `get_wire` — load by id; if its `local_account_id` isn't one of the caller's
  accounts → **404** (no existence leak).

- [ ] **Step 2:** `cargo check`; smoke: A lists → sees the wire from Task 6;
  `GET /wires/:id` as A → 200; as customer B → 404. Paste outputs.

- [ ] **Step 3: Commit** — `feat(lynx): list + single wire (ownership-scoped)`.

---

## Task 8: Network settle (`POST /network/wires/:id/settle`)

**Files:** Modify `api/src/handlers/lynx.rs`.

- [ ] **Step 1:** `network_settle(_svc: AuthenticatedService, Path(id))`:
  - `let mut tx = begin()`; guarded transition: `UPDATE lynx_wires SET status='settled', settled_at=now WHERE wire_id=$1 AND status='sent'`; if `rows_affected()!=1` → load current status: not found → 404, else `Conflict("wire is <status>")`.
  - Load the wire's `settlement_transaction_id`'s hold (rebuild a `Hold { from_account = local_account_id, amount, reference, transaction_id }` from `lynx_wires`), and the counterparty institution.
  - `rail.release(&state,&mut tx,&hold, Destination::External(counterparty_institution), "Lynx settlement")` → Dr CLEARING / Cr SETTLEMENT, GL `Payable`→`Bank`. (Do NOT recompute available on the system accounts.)
  - `tx.commit()`; return `200` with `{ "wire_id", "status":"settled" }`.

- [ ] **Step 2:** `cargo check`; smoke: settle Task 6's wire with the **service
  token** → 200 `settled`; `LYNX_CLEARING` back to 0, `LYNX_SETTLEMENT` up $25k;
  re-settle → 409; settle with a **customer** token → 403. Paste DB balances.

- [ ] **Step 3: Commit** — `feat(lynx): network settlement (finality; Payable→Bank)`.

---

## Task 9: Network inbound (`POST /network/inbound`)

**Files:** Modify `api/src/handlers/lynx.rs`.

- [ ] **Step 1:** `network_inbound(_svc, Json(req: NetworkInboundRequest))`:
  - Resolve the beneficiary account: `SELECT account_id, customer_id FROM accounts
    WHERE institution_number=$beneficiary_institution AND transit_number=$beneficiary_transit
    AND account_number=$beneficiary_account` → 404 if none.
  - `amount = normalize_amount(req.amount)?`; `rail = resolve_lynx`; `begin`;
    `posting = rail.accept_inbound(&state,&mut tx, acct, amount, "Lynx inbound wire")`
    (Dr SETTLEMENT / Cr customer, GL `Bank`→`Payable`); `recompute_available(acct)`.
  - `uetr = req.uetr.unwrap_or_else(Uuid::new_v4)`; `message_type = req.message_type.unwrap_or("pacs.008")`; decode-store: build the `lynx_messages` `flow='received'` payload from `encode_pacs008`/`encode_pacs009` of the request fields (so the outbox round-trips).
  - Insert `lynx_wires` (`direction='inbound', status='settled', local_account_id=acct, counterparty_* = debtor_*, settlement_transaction_id=posting.transaction_id, initiated_by=NULL, settled_at=now`).
  - `tx.commit()`; return `201` + `load_wire`.

- [ ] **Step 2:** `cargo check`; smoke: POST an inbound wire targeting customer B's
  chequing coords, `$40,000`, service token → 201; B balance/available +$40k,
  `LYNX_SETTLEMENT` moved; a POST to unknown coords → 404. Paste outputs.

- [ ] **Step 3: Commit** — `feat(lynx): network inbound wire (accept_inbound; Bank→Payable)`.

---

## Task 10: Request recall (`POST /wires/:id/recall`)

**Files:** Modify `api/src/handlers/lynx.rs`.

- [ ] **Step 1:** `request_recall(caller, Path(id), Json(req: RecallRequest))`:
  - Load the wire; initiator-only (`initiated_by == caller.customer_id` else 404).
  - Must be `direction='outbound'` and `status='settled'` else `Conflict`.
  - Reject if an open recall already exists (`SELECT 1 FROM lynx_recalls WHERE wire_id=$1 AND status='requested'`) → 409.
  - `reason = req.reason.unwrap_or("customer request")`; build `Camt056 { uetr=new, original_uetr=wire.uetr, reason }`; store `lynx_messages` (`camt.056, emitted`) → `msg_id`; insert `lynx_recalls (wire_id, direction='outbound', requested_by=caller, reason, status='requested', camt056_message_id=msg_id)`.
  - Return `201` with `{ "recall_id", "status":"requested" }`.

- [ ] **Step 2:** `cargo check`; smoke: recall the settled wire as A → 201; recall
  again → 409; recall as B → 404; recall an unsettled/inbound wire → 409. Paste.

- [ ] **Step 3: Commit** — `feat(lynx): customer recall request (emit camt.056)`.

---

## Task 11: Resolve recall (`POST /network/recalls/:id/resolve`)

**Files:** Modify `api/src/handlers/lynx.rs`.

- [ ] **Step 1:** `network_recall_resolve(_svc, Path(recall_id), Json(req: RecallResolveRequest))`:
  - `begin`; guarded: `UPDATE lynx_recalls SET status=$new, resolved_at=now, resolution_reason=$reason WHERE recall_id=$1 AND status='requested'` where `$new` = `accepted` if `req.decision=="accept"` else `rejected`; `rows_affected()!=1` → 404/409.
  - Load the recall's wire. Build `Camt029 { uetr=new, original_uetr=wire.uetr, status = if accept "ACCP" else "RJCR", reason }`; store `lynx_messages` (`camt.029, received`); set `lynx_recalls.camt029_message_id`.
  - **If accept:** `rail.accept_inbound(&state,&mut tx, wire.local_account_id, wire.amount, "Lynx recall refund")` (funds come back: Dr SETTLEMENT / Cr customer, GL `Bank`→`Payable`); `recompute_available(local_account_id)`; `UPDATE lynx_wires SET status='recalled' WHERE wire_id=…`.
  - **If reject:** nothing economic; wire stays `settled`.
  - `commit`; return `200` `{ "recall_id","status", "wire_status" }`.

- [ ] **Step 2:** `cargo check`; smoke: resolve the Task 10 recall `accept` (service
  token) → 200; A refunded +$25k, wire `recalled`; resolve again → 409. Then on a
  fresh wire+recall, resolve `reject` → 200, wire stays `settled`, no refund. Paste.

- [ ] **Step 3: Commit** — `feat(lynx): resolve outbound recall (camt.029 accept→refund / reject)`.

---

## Task 12: Inbound recall (`POST /network/inbound-recall`)

**Files:** Modify `api/src/handlers/lynx.rs`.

- [ ] **Step 1:** `network_inbound_recall(_svc, Json(req: InboundRecallRequest))` —
  an external sender wants back a wire we **received**:
  - Load the wire; must be `direction='inbound'` and `status='settled'` else `Conflict`.
  - Store the incoming `Camt056` (`camt.056, received`); insert `lynx_recalls (direction='inbound', status='requested', camt056_message_id)`.
  - Decision: on `accept`, attempt clawback — `fetch_account_for_update(local_account_id)`; if `available_balance < amount` → resolve **reject** (v1: can't force negative), `Camt029 RJCR reason "insufficient funds"`; else `zero_available`; `rail.clawback(&state,&mut tx, local_account_id, amount, "Lynx inbound recall")` (Dr customer / Cr SETTLEMENT, GL `Payable`→`Bank`); `recompute_available`; `UPDATE lynx_wires SET status='recalled'`; resolve **accepted**, `Camt029 ACCP`.
  - On `reject` decision: resolve rejected, `Camt029 RJCR`.
  - Store the `camt.029` (`emitted`), set `camt029_message_id`, `resolved_at`. `commit`; return `200`.

- [ ] **Step 2:** `cargo check`; smoke: inbound-recall B's Task 9 wire `accept` while
  B still holds the funds → 200 `recalled`, B −$40k, `LYNX_SETTLEMENT` moved back;
  a second inbound wire then `accept` after B spent it down → 200 but `rejected`
  (insufficient funds), wire stays `settled`. Paste DB balances.

- [ ] **Step 3: Commit** — `feat(lynx): inbound recall (accept→clawback / reject; camt.029)`.

---

## Task 13: Admin reject-stale sweep (`POST /admin/reject-stale`)

**Files:** Modify `api/src/handlers/lynx.rs`.

- [ ] **Step 1:** `admin_reject_stale(_svc)` — sweep `sent` wires older than a
  cutoff (`NANO_BANK__LYNX__STALE_MINUTES`, default `60`):
  - `SELECT wire_id, local_account_id, amount, reference_number, settlement_transaction_id FROM lynx_wires WHERE status='sent' AND sent_at < now() - $interval FOR UPDATE`.
  - Per row: rebuild the `Hold`; `zero_available(local_account_id)`;
    `rail.refund(&state,&mut tx,&hold, "Lynx stale wire rejected")` (Dr CLEARING / Cr customer); `recompute_available(local_account_id)`; `UPDATE lynx_wires SET status='rejected' WHERE wire_id=…`.
  - `commit`; return `200` `{ "rejected": <count> }`.

- [ ] **Step 2:** `cargo check`; smoke: send a wire, back-date its `sent_at`
  (`UPDATE lynx_wires SET sent_at = now() - interval '2 hours' WHERE …`), call the
  sweep (service token) → `{"rejected":1}`, the sender refunded, `LYNX_CLEARING`
  back to 0, wire `rejected`. Paste.

- [ ] **Step 3: Commit** — `feat(lynx): admin sweep of stale (unsettled) wires`.

---

## Task 14: Network simulator (`testing/lynx/`)

**Files:** Create `testing/lynx/lynx_simulator.py`, `testing/lynx/Containerfile`, `testing/lynx/requirements.txt`; Modify `testing/run-testing.sh`, `testing/stop-testing.sh`.

- [ ] **Step 1:** Copy `testing/aft/` as the base. The Lynx simulator (service
  token, base URL from env) periodically:
  - **settles** `sent` outbound wires: `GET`/derive pending wires (query the DB or
    a listing) and `POST /api/v1/lynx/network/wires/:id/settle`;
  - **originates inbound** wires: `POST /api/v1/lynx/network/inbound` against a
    configured beneficiary's coords;
  - **resolves** open outbound recalls: `POST /api/v1/lynx/network/recalls/:id/resolve` (accept by default);
  - optionally **requests inbound recalls**: `POST /api/v1/lynx/network/inbound-recall`.
  Include a small Python ISO 20022 encoder matching `iso20022.rs`'s field shape
  (only what the endpoints need in their JSON bodies). Mirror `aft_simulator.py`'s
  loop + logging.

- [ ] **Step 2:** Add the `nano-bank-lynx` container to `run-testing.sh` /
  `stop-testing.sh` (copy the AFT wiring block). Build + run; confirm it settles a
  freshly-sent wire and originates an inbound one. Paste the simulator log.

- [ ] **Step 3: Commit** — `test(lynx): network simulator (settle / inbound / recall)`.

---

## Task 15: Viewer Lynx tab

**Files:** Modify `testing/viewer/app.py`.

- [ ] **Step 1:** Add `render_lynx()` + a "🌐 Lynx" tab mirroring `render_aft`:
  a wires table (direction, status, amount, counterparty, uetr), the
  `LYNX_CLEARING` / `LYNX_SETTLEMENT` balances, and the `lynx_messages` log.

- [ ] **Step 2:** Run the viewer; confirm the tab renders the wires created above.
  Paste a screenshot path or the rendered row counts.

- [ ] **Step 3: Commit** — `test(lynx): viewer tab (wires, clearing/settlement, messages)`.

---

## Task 16: Bruno collection (`8_Lynx/`)

**Files:** Create `bruno/8_Lynx/*.bru`; Modify `bruno/environments/local.bru` (add `wireId`, `recallId`).

- [ ] **Step 1:** Create `.bru` requests (follow the `7_AFT/` format exactly —
  `body:json` + `auth:inherit` inside the `post {}` block): Initiate Wire, List
  Wires, Get Wire, Request Recall, Network Settle, Network Inbound, Resolve
  Recall, Inbound Recall, Reject Stale. Add `wireId`/`recallId` to
  `bruno/environments/local.bru` (union with the existing vars).

- [ ] **Step 2:** Open the collection in Bruno (or `bru run`) against the live API;
  confirm the happy-path flow (initiate → settle → recall → resolve) succeeds.

- [ ] **Step 3: Commit** — `test(lynx): Bruno collection for the wire flows`.

---

## Task 17: Docs + final smoke

**Files:** Modify `CLAUDE.md`, `api/CLAUDE.md`, `README.md`.

- [ ] **Step 1:** Add a "Lynx wire rail" section to `CLAUDE.md` (mirror the Interac
  / AFT sections: the two-step settlement, finality + recall, the `lynx@nano.bank`
  accounts, the GL distinction — inbound uses `Bank` not `Receivable`). Note
  `src/lynx/` + the Lynx rail in `api/CLAUDE.md`'s Layout/Rails. Add the endpoint
  rows to `README.md`.

- [ ] **Step 2: Full live smoke** — with the stack up, run the end-to-end flow once
  more (initiate → network settle → inbound → outbound recall accept → inbound
  recall accept → stale sweep), plus `cargo test` (codec unit tests green) and
  `cargo check`. Confirm `git diff --stat main...HEAD -- api/src/handlers/transactions.rs`
  is **empty**. Paste the smoke transcript.

- [ ] **Step 3: Commit** — `docs(lynx): document the Lynx RTGS wire rail`.

---

## Self-review notes

- **Spec coverage:** schema (T1), ISO 20022 codec (T2), Rail + system accounts
  (T3), models (T4), scaffold (T5), outbound send (T6), list/get (T7), settle
  (T8), inbound (T9), outbound recall request+resolve (T10–11), inbound recall
  (T12), stale sweep (T13), simulator/viewer/Bruno (T14–16), docs + finality/
  drift/`transactions.rs` checks (T17). Every spec section maps to a task.
- **GL directions** are pinned in the Task 3 table and reused by name in the
  handler tasks (settle `Payable→Bank`, inbound `Bank→Payable`, recall-refund
  `Bank→Payable` via `accept_inbound`, clawback `Payable→Bank`).
- **`available_balance`** recompute appears only on customer-account legs
  (T6/T9/T11/T12/T13); never on the system accounts (T8).
- **Names** are consistent: `LynxRail`, `LynxAccounts`, `ensure_lynx_accounts`,
  `resolve_lynx`, `min_amount`, `clawback`, `InitiateWireRequest`/`WireResponse`/
  `RecallRequest`/`NetworkInboundRequest`/`RecallResolveRequest`/
  `InboundRecallRequest`, and the `encode_*`/`decode_*` codec pairs.
```
