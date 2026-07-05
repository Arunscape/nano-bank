# 🏦 nano-bank

> Why not vibe-coding a bank?

A toy challenger-bank core, built to explore what a production-shaped banking
backend looks like end to end: a **Rust API** on top of a **double-entry
PostgreSQL** schema, running on a local **Kind (Kubernetes-in-Docker)** cluster.

> [!WARNING]
> This is a learning/experimental project — **not** for handling real money.
> The database schema is substantial and the infrastructure works, but most API
> handlers are still stubs (they return `"... endpoint - TODO: implement"`).
> Default credentials and the JWT secret are committed in plaintext for local
> dev convenience. Don't deploy this anywhere real.

## Architecture

```
┌──────────────────────────────┐
│  Rust API  (axum)            │   http://localhost:8081
│  JWT auth · Decimal money    │
└───────────────┬──────────────┘
                │  sqlx (port-forward :5432)
┌───────────────▼──────────────┐
│  PostgreSQL 16               │   in a Kind cluster, namespace `nano-bank`
│  double-entry bookkeeping    │   init Job loads the DDL on first boot
└──────────────────────────────┘
```

## Pluggable accounting core (the Ledger port)

The general-ledger posting is being split out behind a backend-agnostic
**`Ledger` port** (`src/ledger/`), so nano-bank can post to either of two
interchangeable core services, chosen at startup by `CORE_BACKEND`:

```
handlers/ledger.rs  ──►  Ledger (trait, neutral Account/Direction/Decimal)
                          ├── ModernLedger ──HTTP──►  nano-bank-modern-core :8091 (Rust)
                          └── LegacyLedger ──HTTP──►  nano-bank-legacy-core  :8090 (Java)
```

The port speaks semantic accounts (`bank`, `receivable`, `revenue`, …); each
adapter maps them onto its backend's numbering. One representative flow is wired
through it today:

```bash
# CORE_BACKEND=modern (default) MODERN_CORE_URL=http://localhost:8091
# CORE_BACKEND=legacy          LEGACY_CORE_URL=http://localhost:8090
curl -X POST localhost:8081/api/v1/ledger/journal -H 'content-type: application/json' -d '{
  "lines":[
    {"account":"bank","direction":"debit","amount":250.00},
    {"account":"revenue","direction":"credit","amount":250.00}
  ]}'
curl localhost:8081/api/v1/ledger/balances
```

The same request posts to whichever core is configured.

### Cards post their GL to the core

The card rails (`cards.rs`) route their **aggregate general-ledger** effect
through the same port: `capture` posts *debit Receivable / credit Payable* and
`settle` posts *debit Payable / credit Bank* to the configured core, with the
core's document id recorded in `transactions.metadata.gl_entry`. The **per-card
subledger** (balance, holds, available credit) stays local — the core is the GL
of record, while nano-bank keeps the subledger that enforces credit limits. The
GL post happens inside the capture/settle transaction, so if the core can't
record it the operation fails rather than letting the ledger drift.

`transactions.rs` (deposit/transfer/withdrawal) is still stubbed and not yet
routed through the port.

| Component | Tech |
|-----------|------|
| API server | Rust, [axum](https://github.com/tokio-rs/axum) 0.7, tokio |
| Database access | [sqlx](https://github.com/launchbadge/sqlx) 0.7 (Postgres) |
| Auth | JWT (`jsonwebtoken`), password hashing with `argon2` |
| Money | `rust_decimal` — no floats for currency |
| Database | PostgreSQL 16, double-entry ledger |
| Orchestration | Kind + Kubernetes manifests under `k8s/` |

## Repository layout

```
nano-bank/
├── api/                     # Rust API server (axum)
│   ├── src/
│   │   ├── main.rs          # router, middleware (CORS, compression, timeout), startup
│   │   ├── config/          # settings + database pool / health check / migration check
│   │   ├── handlers/        # route handlers: auth, customers, accounts, transactions,
│   │   │                    #   security, health, docs  (mostly TODO stubs)
│   │   ├── models/          # domain models (customer, account, transaction, security)
│   │   ├── middleware/  errors/  repositories/  services/  utils/
│   │   └── ...
│   └── config/default.toml  # default config (DB, server, JWT, security, logging)
├── src/core/tables/         # PostgreSQL DDL (the real substance of the schema)
│   ├── 00_init.sql … 06_triggers.sql
│   └── README.md            # schema docs
├── k8s/                     # Kind cluster config + Postgres manifests + init Job
│   ├── deploy.sh
│   └── README.md            # k8s setup docs
├── setup-k8s.sh             # one-time host setup (Docker perms, tool checks)
├── start-nano-bank.sh       # bring everything up
└── stop-nano-bank.sh        # tear everything down (deletes the cluster)
```

## Getting started

### Prerequisites

- **Docker** (running, with your user in the `docker` group)
- **kubectl** and **kind** on your `PATH`
- **Rust** toolchain (`cargo`)

On first run, make sure the tools are reachable and Docker permissions are set:

```bash
export PATH="$HOME/bin:$PATH"
./setup-k8s.sh
```

### Start

```bash
./start-nano-bank.sh
```

This does three things:

1. Creates the `nano-bank` Kind cluster (if it doesn't already exist) and
   deploys PostgreSQL via `k8s/deploy.sh`. An init Job loads the DDL scripts in
   order (enums → customers → accounts → transactions → security → triggers).
2. Port-forwards the in-cluster Postgres to `localhost:5432`.
3. Builds and runs the Rust API (`cargo run`) on `http://localhost:8081`.

### Stop

```bash
./stop-nano-bank.sh
```

Stops the API and port-forward, **deletes the Kind cluster** (and its data), and
cleans up log files.

## Services

| Service      | URL                            |
|--------------|--------------------------------|
| API Server   | http://localhost:8081          |
| Health Check | http://localhost:8081/health   |
| API Docs     | http://localhost:8081/docs     |
| PostgreSQL   | localhost:5432                 |

## Logs

The start script runs the API and port-forward in the background:

```bash
tail -f /tmp/nano-bank-api.log
tail -f /tmp/nano-bank-port-forward.log
```

## API overview

Full HTML docs are served at `http://localhost:8081/docs`. Routes are namespaced
under `/api/v1`:

| Area | Endpoints |
|------|-----------|
| **Auth** | `POST /auth/login`, `/auth/refresh`, `/auth/logout` |
| **Customers** | `POST /customers`, `GET`/`PUT /customers/profile`, `POST /customers/kyc/documents` |
| **Accounts** | `GET`/`POST /accounts`, `GET /accounts/{id}`, `GET /accounts/{id}/balance` |
| **Transactions** | `POST /transactions/transfer`, `/deposit`, `/withdrawal`, `GET /transactions` |
| **Interac** | `POST`/`GET /interac/etransfers`, `POST /interac/etransfers/{id}/{claim,decline,cancel}`, `POST`/`GET /interac/autodeposit`, `POST /interac/network/inbound`, `POST /interac/network/etransfers/{id}/settle`, `POST /interac/admin/sweep-expired` |
| **Security** | `GET /security/sessions`, `GET /security/devices`, `POST /security/devices/trust` |
| **System** | `GET /health`, `GET /docs` |

> Most handlers are placeholders today — the routing, middleware, config, and
> database layers are wired up, but business logic for transactions, accounts,
> etc. still needs implementing. See [Implementation status](#implementation-status).

## Implementation status

### ✅ Implemented

**Infrastructure & data layer**

- Kind cluster config and PostgreSQL Kubernetes manifests (`k8s/`), with an init
  Job that loads the DDL on first boot
- `setup-k8s.sh` / `start-nano-bank.sh` / `stop-nano-bank.sh` lifecycle scripts
- Full **double-entry PostgreSQL schema** (`src/core/tables/`, `00`–`06`):
  enums, customers, accounts, transactions + entries, security/audit tables, and
  trigger-based balance validation

**API server scaffolding** (`api/`)

- axum router with all `/api/v1` routes mounted
- Middleware stack: CORS, gzip/brotli compression, 30s request timeout, 10 MB
  body limit, structured tracing
- Configuration loading (`config` + `default.toml` + env overrides, `RUN_MODE`)
- Database connection pool with startup health check and a table-existence
  migration check
- Domain **models** as typed Rust structs: customer, account, transaction,
  security
- Centralized **error handling** module

**Working endpoints**

- `GET /health` — real database-backed health check
- `GET /docs` — HTML API documentation

### 🚧 Not yet implemented

**Handler business logic** — routes exist but return `"... endpoint - TODO:
implement"`:

- **Auth** — `login`, `refresh`, `logout` (JWT issuance/validation, argon2
  password verification, session handling)
- **Customers** — create, get/update profile, KYC document upload
- **Accounts** — list, create, get details, get balance
- **Transactions** — transfer, deposit, withdrawal, history (wired to the
  double-entry ledger)
- **Security** — sessions, devices, device trust
- The `repositories/`, `services/`, `middleware/`, and `utils/` modules are
  empty placeholders awaiting these implementations

**Planned subsystems** (not yet started):

- **CRM** — customer relationship management: contact/interaction history,
  support tickets/cases, segmentation, communications, and lifecycle tracking on
  top of the existing customer records
- **Fraud** — real-time fraud detection and prevention: transaction risk
  scoring, rules engine, velocity/anomaly checks, alerts and case management,
  building on the security/monitoring tables already in the schema
- **Agentic Governance** — guardrails and oversight for AI/agent-driven actions:
  policy enforcement, human-in-the-loop approvals, action audit trails, and
  explainability for any automated decisioning in the bank

## Database

The schema is the most developed part of the project. See
[`src/core/tables/README.md`](src/core/tables/README.md) for full details.
Highlights:

- **Double-entry bookkeeping** via a `transaction_entries` table with
  balance-validation triggers
- **Canadian compliance** flavor: CAD currency, postal-code / SIN validation,
  provincial codes, KYC/AML fields
- **Audit trails** and **security monitoring** (sessions, device fingerprinting,
  failed-login tracking, configurable risk rules)
- Extensive check / FK / unique constraints and a deliberate indexing strategy

### Connecting directly

With the port-forward running (default from `start-nano-bank.sh`):

```bash
psql -h localhost -p 5432 -U nanobank_user -d nano_bank_db
# password: secure_nano_password_2024!   (local dev only)
```

The k8s manifests also expose a NodePort on `30432`. See
[`k8s/README.md`](k8s/README.md) for cluster management commands.

## Configuration

Defaults live in [`api/config/default.toml`](api/config/default.toml) and cover
the database connection, server bind address (`0.0.0.0:8081`), JWT settings,
security policy (login attempts, lockout, session timeout), and logging.
`RUN_MODE` selects the environment; settings can be overridden via environment
variables (the app uses `config` + `dotenvy`).

## License

See [`LICENSE`](LICENSE).
