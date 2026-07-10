---
name: nano-bank-api
description: Use when writing or editing nano-bank axum handlers — SQL, error mapping, auth, config, enum serialization, or the local DB connection — covers the house patterns and the non-obvious gotchas.
---

# nano-bank API patterns

Rust / axum on :8081 over PostgreSQL. Handlers are thin:
`async fn(State<AppState>, Json<Req>) -> Result<(StatusCode, Json<Resp>), AppError>`.

## No ORM / no layers

There is no ORM, repository, or service layer — those modules exist as empty
placeholders. All SQL is inline in the handler:
`sqlx::query_as::<_, Model>(raw_sql).bind(...).fetch_one(&pool)`. Follow that;
don't introduce a layer.

## Postgres constraint code → HTTP status

Match SQLSTATE directly in handlers:

- `23505` (unique violation) → `AppError::Conflict` (409)
- `23503` (FK violation) → `AppError::BadRequest` (400)
- `23514` (CHECK violation) → `AppError::BadRequest` (400)

`AppError::into_response()` serialises as `{ "error": { code, message, details } }`.

## Auth planes

- **Customer plane:** `AuthenticatedCustomer` extractor (JWT). Cross-customer
  access returns **404**, not 403.
- **Service-token plane:** internal / network endpoints (card rails,
  `/interac/network/*`, `/interac/admin/*`).

## Gotchas

- **DB host is `::1` (IPv6 loopback), not 127.0.0.1.** `kubectl port-forward`
  leaves a dead `0.0.0.0:5432` IPv4 mapping that the Kind/Docker proxy resets.
  Set in `api/config/default.toml`.
- **Enum serialisation:** `KycStatus` and similar use
  `#[sqlx(rename_all = "snake_case")]` for the DB but have NO serde rename, so
  JSON output is PascalCase (`"Pending"`, not `"pending"`).
- **Config layering:** `default.toml` → `{RUN_MODE}.toml` → `local.toml` → env.
  Env override prefix `NANO_BANK__`, separator `__`
  (e.g. `NANO_BANK__DATABASE__HOST`).
- **Money is `rust_decimal::Decimal`** — never floats. CAD only
  (`chk_currency_cad`).

## Commands

From `api/`: `cargo check` (fast type-check), `cargo run` (starts :8081),
`cargo clippy`, `cargo fmt`. Requires
`kubectl port-forward -n nano-bank svc/postgres-service 5432:5432` first.
