# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this project is

An experimental Canadian challenger-bank core API — Rust (`axum 0.7`) over a PostgreSQL 16 double-entry ledger running in a local Kind (Kubernetes-in-Docker) cluster. The schema and infrastructure are fully built; most handler business logic is still TODO stubs. The card payment rails (`/api/v1/cards/*`) are the most complete part.

## Commands

All Rust commands run from `api/`:

```bash
cargo check          # fast type-check without producing a binary
cargo build          # compile
cargo run            # build + start API on 0.0.0.0:8081
cargo fmt            # format
cargo clippy         # lint
cargo test           # run tests (dev-dependencies are wired but no tests exist yet)
```

Infrastructure (run from repo root):

```bash
kind create cluster --config k8s/kind-cluster-config.yaml   # one-time cluster setup
./k8s/deploy.sh                                              # deploy Postgres + run DDL init Job
kubectl port-forward -n nano-bank svc/postgres-service 5432:5432  # required before cargo run
kubectl exec -it -n nano-bank deployment/postgres -- psql -U nanobank_user -d nano_bank_db
testing/cleanup.sh --dry-run   # preview row counts
testing/cleanup.sh             # TRUNCATE customers CASCADE (wipes all data, GL accounts self-heal)
```

Config override via env vars uses prefix `NANO_BANK__` with `__` as separator (e.g. `NANO_BANK__DATABASE__HOST`). Layer order: `config/default.toml` → `config/{RUN_MODE}.toml` → `config/local.toml` → env vars.

## Architecture

### Request flow

```
HTTP request
  → axum middleware stack (CORS, gzip, 30s timeout, 10MB body limit, tracing)
  → handler fn (State<AppState>, Json<RequestType>) -> Result<(StatusCode, Json<ResponseType>), AppError>
  → sqlx raw query against PgPool
  → PostgreSQL triggers mutate balances / enforce invariants
  → AppError::into_response() serialises errors as { "error": { "code", "message", "details" } }
```

`AppState` (in `handlers/mod.rs`) holds a cloned `PgPool` and `Settings`. It is injected into every handler via axum's `State` extractor.

### Database interaction pattern

There is no ORM, no repository layer, and no service layer — those modules exist as empty placeholders. All SQL is inline in the handler using `sqlx::query_as::<_, ModelType>(raw_sql).bind(...).fetch_one(&pool)`.

Postgres constraint codes are matched directly in handlers to return correct HTTP statuses:
- `23505` → `AppError::Conflict` (unique violation)
- `23503` → `AppError::BadRequest` (FK violation)
- `23514` → `AppError::BadRequest` (CHECK violation)

### Double-entry ledger (critical invariant)

Both legs of every transaction **must be inserted in a single multi-row `INSERT` statement** — see `post_two_legged()` in `handlers/cards.rs`. The reason: `trigger_validate_transaction_balance` (AFTER INSERT on `transaction_entries`) checks that `SUM(debits) = SUM(credits)` for the transaction and raises an exception if they don't balance. Inserting one leg at a time trips this trigger between inserts.

Other key triggers (all in `src/core/tables/06_triggers.sql`):
- `trigger_update_account_balance` — BEFORE INSERT on `transaction_entries`: updates `accounts.balance` and fills `balance_before`/`balance_after`. Never update balances directly.
- `trigger_generate_account_number` — BEFORE INSERT on `accounts`: overwrites `account_number` with a random 12-digit value. GL accounts cannot be found by account number; they are keyed by `(customer_id, account_type)`.

### Card payment rails GL accounts

Two internal accounts exist under `system@nano.bank`:
- `VISA_CLEARING` (`account_type = chequing`) — carries a negative balance representing the issuer's obligation to the network
- `BANK_SETTLEMENT` (`account_type = savings`) — the funding account

Both carry a $1 trillion overdraft limit so their balances can go negative. They are bootstrapped idempotently at startup by `handlers::cards::ensure_system_accounts()` and their UUIDs are re-resolved per request (not cached in AppState) because a `testing/cleanup.sh` run wipes and rebuilds them.

### Enum serialisation quirk

`KycStatus` and similar enums use `#[sqlx(rename_all = "snake_case")]` for DB mapping but have no serde rename attribute, so JSON output is PascalCase (e.g. `"Pending"`, not `"pending"`).

### Configuration

`api/config/default.toml` is the source of truth for local dev credentials. The `database.host` is `::1` (IPv6 loopback) because `kubectl port-forward` creates a dead `0.0.0.0:5432` mapping — IPv4 connections are reset by the Kind/Docker proxy.

### What is and isn't implemented

Implemented handlers (real SQL + logic):
- `POST /api/v1/customers` — `handlers/customers.rs`
- `POST /api/v1/accounts` — `handlers/accounts.rs`
- `POST /api/v1/cards/authorize|capture|settle` — `handlers/cards.rs`
- `POST /api/v1/transactions/deposit|withdrawal|transfer`, `GET /api/v1/transactions` — `handlers/transactions.rs`
- `POST /api/v1/interac/*` — Interac e-Transfer rail — `handlers/interac.rs` (see "Interac e-Transfer rail" below)
- `GET /health`, `GET /docs`

Everything else (`auth`, `security`, GET endpoints for customers/accounts) returns a static `"... endpoint - TODO: implement"` string.

### Bruno collection

Bruno `.bru` files require `body: json` and `auth: inherit` declared **inside the `post {}` block** alongside the URL — without those, Bruno ignores the `body:json {}` content block entirely. See the working `_2` files in `bruno/` for the correct format Bruno generates from its own UI.

### Testing

No Rust tests exist. The test harness is three Python containers in `testing/`:
- `generator/generate_customers.py` — seeds fake Canadian customers and accounts via the API
- `visa/visa_simulator.py` — drives the full authorize → capture → settle loop
- `viewer/app.py` — Streamlit live dashboard on port 8504
# CLAUDE.md — nano-bank

Guidance for working in this repo. nano-bank is a toy challenger-bank backend
(Rust/axum over PostgreSQL on a local Kind cluster). This file focuses on the
parts that aren't obvious from the code.

## Big picture: the kernel split

nano-bank's general-ledger posting is **backend-agnostic**. The app posts
accounting entries through a small `Ledger` port, and the actual ledger lives in
one of two **interchangeable core services**, chosen at startup by an env var:

```
nano-bank app (this repo)            http://localhost:8081
  api/src/handlers/ledger.rs  ─┐
  api/src/handlers/cards.rs   ─┴─►  Ledger port (api/src/ledger/)
                                      ├── ModernLedger ──HTTP──► modern core  :8091
                                      └── LegacyLedger ──HTTP──► legacy core   :8090
  CORE_BACKEND=modern | legacy   picks the adapter at startup
```

The two cores are separate repos and run as peers:
- **`nano-bank-modern-core`** — a clean Rust/axum general-ledger service.
- **`nano-bank-legacy-core`** — a cleanroom ERP-style financial core (Java/Spring)
  that exposes document-posting contracts (REST/SOAP/OData/IDoc) using authentic,
  cryptic technical field names. Treat those names as neutral identifiers; do not
  describe in code or docs what product they resemble.

The port speaks **semantic** terms (an `Account` role like `bank`/`receivable`/
`revenue`, a `Direction` of `debit`/`credit`, `Decimal` money). Each adapter maps
those to its backend's numbering (modern GL codes like `BANK`/`AR` vs the legacy
core's `0000xxxxxx` numbers + `S/H` indicator), so nano-bank never needs to know
either backend's account scheme.

## Where things live

- `api/` — the Rust service (see `api/CLAUDE.md` for internals).
- `api/src/ledger/` — the `Ledger` port (`mod.rs`) and the two adapters
  (`modern.rs`, `legacy.rs`).
- `api/src/handlers/ledger.rs` — `POST /api/v1/ledger/journal`, `GET /api/v1/ledger/balances`.
- `api/src/handlers/cards.rs` — the card rails (authorize/capture/settle).
- `src/core/tables/` — the PostgreSQL DDL (loaded by the Kind init Job).
- `k8s/` — Kind cluster + Postgres manifests.
- `testing/` — a 3-container harness (data generator + payment-network sim + viewer).

## Running the stack

1. **Database** (Kind): `./k8s/deploy.sh` (or `./setup-k8s.sh`). The DB ends up on
   host **`::1:5432`** — note the IPv6 loopback; `127.0.0.1:5432` does *not* work
   (a dead docker-proxy listens there). Connection details are in
   `api/config/default.toml`.
2. **A core** — start at least one:
   - modern: in `nano-bank-modern-core`, `docker compose up -d db` then
     `DATABASE_URL=postgres://core:core@localhost:5435/modern_core cargo run` (`:8091`).
   - legacy: in `nano-bank-legacy-core`, `./start-core.sh` (`:8090`).
3. **nano-bank**: `cd api && cargo run` (`:8081`). Pick the backend with env:
   - `CORE_BACKEND=modern MODERN_CORE_URL=http://localhost:8091`
   - `CORE_BACKEND=legacy LEGACY_CORE_URL=http://localhost:8090`
   Defaults: backend `modern`, the two URLs above.

## Trying the swap

```bash
# the SAME request posts to whichever core is configured
curl -X POST localhost:8081/api/v1/ledger/journal -H 'content-type: application/json' -d '{
  "lines":[{"account":"bank","direction":"debit","amount":250.00},
           {"account":"revenue","direction":"credit","amount":250.00}]}'
curl localhost:8081/api/v1/ledger/balances
```

Restart nano-bank with the other `CORE_BACKEND` and the same call lands in the
other core (a new entry id / `belnr`).

## Cards: subledger vs general ledger

`cards.rs` keeps a **per-card subledger locally** (the `transactions` /
`transaction_entries` tables, plus `account_holds`) because the GL core only has
**aggregate** accounts, and per-card balances drive credit-limit checks
(`available = overdraft_limit − balance − holds`).

On top of that, `capture` and `settle` post the **aggregate GL effect** to the
core via the port (capture: debit Receivable / credit Payable; settle: debit
Payable / credit Bank), recording the core's document id in
`transactions.metadata.gl_entry`. The GL post happens inside the capture/settle
DB transaction, before commit — so if the core can't record it, the operation
fails rather than letting the local subledger and the GL drift. `authorize` is
local-only (a hold; no money moves).

`transactions.rs` (deposit/withdrawal/transfer + history) is implemented.
Deposit and withdrawal move value across an internal **`EXTERNAL_CASH`** account
(a chequing account under a synthetic `cash@nano.bank` customer, $1T overdraft)
and post the aggregate effect through the port (deposit: debit `Bank` / credit
`Payable`; withdrawal the reverse). A **transfer is local-only** — both customer
accounts map to the same `Payable` GL role, so the net GL effect is zero. All
three enforce balance/status/type checks and the `account_limits` counters, and
update `daily_transaction_summaries`; transfer honors an `idempotency_key`.

## Interac e-Transfer rail

The first **external payment rail**, built on a small `Rail` port that sits
*beside* the `Ledger` port (`api/src/rails/`, see also `api/CLAUDE.md` and the
`.claude/skills/nano-bank-rails` skill; design spec in
`docs/specs/2026-07-04-interac-rail-foundation-design.md`).

- **Rail port** (`rails/mod.rs`): the verbs `hold` / `release` / `refund` /
  `accept_inbound`, each taking `&mut PgTx` so the local double-entry and the
  aggregate GL post commit or roll back together (503 if the core is down). A
  `Destination` is `Internal(account)` (a nano-bank customer) or
  `External(institution)` (settles via the settlement account).
- **Interac system accounts** (`rails/interac.rs`): a *separate* synthetic
  customer `interac@nano.bank` owns `INTERAC_CLEARING` (chequing, holds in-flight
  funds) and `INTERAC_SETTLEMENT` (savings, the interbank position), both with a
  $1T overdraft. Distinct from the card rails' `system@nano.bank` because GL
  accounts are keyed by `(customer, account_type)`.
- **Lifecycle** (`handlers/interac.rs`): send → held in `INTERAC_CLEARING` →
  autodeposit (registered handle) / claim (security Q&A, argon2, 3-strike lock) /
  decline / cancel / expire (sweep). Inbound: autodeposit fast-path
  (`accept_inbound`) or held-then-claim. Notifications go to the
  `interac_notifications` **outbox** table (no real email/SMS).
- **Three auth planes**: customer (`/etransfers`, `/autodeposit`), service-token
  **network** (`/network/inbound`, `/network/etransfers/:id/settle` — driven by
  `testing/interac/interac_simulator.py`), service-token **admin**
  (`/admin/sweep-expired`). The viewer (`testing/viewer`) has an Interac tab.
- **`available_balance` note**: the balance trigger maintains only `balance`, so
  the handlers hand-recompute `available_balance` around rail posts on **customer**
  accounts; the system clearing/settlement accounts intentionally keep it at 0
  (they float on the $1T overdraft).
- **Known v1 gaps / follow-ups**: the autodeposit registration endpoint always
  sets autodeposit, so there is no API path yet to register a handle *without*
  autodeposit (the "registered-no-autodeposit" internal-claim branch isn't
  API-reachable). Deferred: shared `account_limits` integration (pending PR #15),
  the ACSS-style `INTERAC_SETTLEMENT`→`Bank` settlement sweep (lands with the AFT
  rail), and Request Money.

## AFT / EFT batch rail

The second external rail (`handlers/aft.rs`, `rails/aft.rs`, `aft/cpa005.rs`) —
Canada's **batch** rail (Automated Funds Transfer over ACSS). Unlike Interac's
per-message model, AFT is file-based and batched. Design spec:
`docs/specs/2026-07-05-aft-eft-rail-design.md`.

- **Own system accounts (decoupled from cards/Interac):** `aft@nano.bank` owns
  `AFT_CLEARING` (chequing) + `AFT_SETTLEMENT` (savings), $1T overdraft. Reuses
  the `Rail` port (`hold`/`release`/`refund`/`accept_inbound`) for the money moves.
- **Two products:** direct-deposit **credits** (pay external accounts / receive
  inbound) and **pre-authorized debits (PAD)**, gated by a `pad_mandates` record
  (a payer authorizes a biller, with an amount cap; revocable).
- **Lifecycle:** originate credits/debits → they accrue in the single open
  outbound batch → `POST /aft/batches/:id/submit` closes it and emits a real
  **CPA-005-style fixed-width file** (`aft/cpa005.rs`, round-trippable) →
  `POST /aft/network/settle/:batch` applies the per-entry legs and posts the
  **settlement sweep** (external direct-deposit cash → `Bank` via the Ledger
  port; per-entry posts are `Payable`/`Payable` reclasses) → NSF entries are
  `rejected`; settled entries can be **returned** via `POST /aft/network/returns`
  (a returns file that reverses them).
- **Inbound:** `POST /aft/network/inbound-batch` ingests a CPA-005 file targeting
  nano-bank customers (credits `accept_inbound`; external PAD debits `hold`+
  `release`, or `rejected` on NSF).
- **Auth planes:** customer (`/aft/mandates`, `/aft/credits`, `/aft/debits`,
  `GET /aft/entries`); service-token **network/admin** (`/aft/batches`,
  `/aft/batches/:id/submit`, `/aft/network/settle|inbound-batch|returns`) — the
  shared outbound batch is a bank-operational concept, so listing and cutting it
  are bank ops, not customer ops. Driven by `testing/aft/aft_simulator.py` (plays
  the bank cutoff + ACSS). Customers see their own activity via `/aft/entries`.
  The viewer has an AFT tab.
- **available_balance:** recomputed on **customer** accounts around rail posts;
  the AFT system accounts stay at 0 (float on the $1T overdraft) — same rule as
  Interac.
- **v1 simplifications:** PAD payers are intra-bank (mandate `payer_account_id`
  is a nano-bank account); returns match a settled entry by amount +
  counterparty account; CPA-005 is authentic in shape, not byte-exact.

## Gotchas

- **DB host is `::1`, not `127.0.0.1`** (dead docker-proxy on IPv4).
- The repo has no card accounts seeded by default — only two system GL accounts.
  Create a `credit_card` account (status `active`, an `overdraft_limit` as the
  credit limit, `available_balance` = the limit) to exercise the card rails.
- Config is layered: `api/config/default.toml` plus env vars with prefix
  `NANO_BANK` and `__` as the separator (e.g. `NANO_BANK__SERVER__PORT=8082` to
  run a second instance alongside one already holding `:8081`).
- Remaining stub handlers: `auth`, `security` (and some GET endpoints).
