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

`transactions.rs` (deposit/transfer/withdrawal) is still stubbed and not yet
routed through the port.

## Gotchas

- **DB host is `::1`, not `127.0.0.1`** (dead docker-proxy on IPv4).
- The repo has no card accounts seeded by default — only two system GL accounts.
  Create a `credit_card` account (status `active`, an `overdraft_limit` as the
  credit limit, `available_balance` = the limit) to exercise the card rails.
- Config is layered: `api/config/default.toml` plus env vars with prefix
  `NANO_BANK` and `__` as the separator (e.g. `NANO_BANK__SERVER__PORT=8082` to
  run a second instance alongside one already holding `:8081`).
- Most non-card handlers (`auth`, `security`, `transactions`) are still stubs.
