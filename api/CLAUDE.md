# CLAUDE.md — nano-bank API (Rust)

Internals of the `nano-bank-api` crate. Read the repo-root `CLAUDE.md` first for
the big picture (the kernel split and the two cores).

## Layout

- `src/main.rs` — builds the router and the `AppState`. `build_ledger()` selects
  the core backend from env and stores an `Arc<dyn Ledger>` in state.
- `src/ledger/` — the **Ledger port** (backend-agnostic accounting):
  - `mod.rs` — `trait Ledger { post_entry, balances, backend }` and the neutral
    types: `Account` (semantic role enum), `Direction`, `EntryLine`, `NewEntry`,
    `PostedEntry { id, backend }`, `AccountBalance`, `LedgerError`.
  - `modern.rs` — `ModernLedger`: HTTP client for `nano-bank-modern-core`.
  - `legacy.rs` — `LegacyLedger`: HTTP client for `nano-bank-legacy-core`
    (`/api/v1/documents`, `/api/v1/gl-balances`); maps the semantic `Account` to
    the legacy `0000xxxxxx` numbers and `Direction` to `S/H`, tags everything
    with company code `1000`, and **truncates `bktxt` to 25 / `xblnr` to 16**
    chars to respect the legacy field widths.
- `src/handlers/` — axum handlers. `ledger.rs` is the wired journal flow;
  `cards.rs` is the card rails; the rest are mostly stubs.
- `src/errors/mod.rs` — `AppError` → HTTP. Includes `Upstream { status, message }`
  used to **preserve a core's status** when proxying (see below).
- `src/config/`, `src/models/`, `src/middleware/`, `src/repositories/`,
  `src/services/`, `src/utils/` — app plumbing; repositories/services are thin.

## The Ledger port

`Account` is a small enum of roles (`Bank`, `Receivable`, `Payable`, `Revenue`,
`Expense`) with `modern_code()` / `legacy_account()` mappings; `Direction` has
`modern()` / `legacy()`. Adding a backend = a new `impl Ledger`; adding an
operation (reverse/clearing/dunning) = a new trait method implemented in both
adapters. Keep backend-specific quirks (field widths, account numbers, company
code) **inside the adapter**, never in the port or handlers.

`AppState.ledger: Arc<dyn Ledger>` is set once at startup; handlers call
`state.ledger.post_entry(...)` / `.balances()`.

## Error mapping (important)

When proxying a core call, surface the upstream status faithfully:
- `LedgerError::Backend { status, body }` → `AppError::Upstream { status, .. }`
  (so an unbalanced entry stays a `422`, a bad line a `400`).
- `LedgerError::Transport(..)` → `503` (the core is unreachable).

`handlers/ledger.rs::ledger_error()` does this. `cards.rs` treats any GL-post
failure as `503` (the card op fails so the GL can't drift).

## Cards

`capture`/`settle` keep their local per-card subledger work (the double-entry on
`transaction_entries`, hold release, available recompute) **and** post the
aggregate GL to the core via `post_gl_entry()` before committing the local tx.
Note: `cards.rs` imports the ledger `Account` aliased as `GlAccount` because the
data model already has its own `Account` type.

## Build / run

```bash
cargo build
# run (reads ../config? no — config/ is resolved relative to the cwd):
cd api && CORE_BACKEND=modern MODERN_CORE_URL=http://localhost:8091 cargo run
```

- Config: `config/default.toml` + env vars (prefix `NANO_BANK`, separator `__`).
  To run a second instance beside one on `:8081`: `NANO_BANK__SERVER__PORT=8082`.
- Backend selection env: `CORE_BACKEND` (`modern`|`legacy`), `MODERN_CORE_URL`,
  `LEGACY_CORE_URL`.
- The binary needs the Kind Postgres up (it health-checks at startup and exits
  if it can't connect). DB host is **`::1`** (see root `CLAUDE.md`).

## Dev gotchas

- **Don't** stop the server with `pkill -f 'target/debug/nano-bank-api'` — the
  pattern matches the launching shell's own command line and kills it. Kill by
  PID instead.
- A `cargo build` here emits some pre-existing dead-code warnings (stubbed
  handlers/models); they're not from the ledger work.
