# Design: Interac payees + confirm-gated e-Transfer

**Date:** 2026-07-10
**Status:** approved; **reworked 2026-07-11 during re-integration onto `main`**
**Repo:** `nano-bank` (branch `agent-k8s-e2e` / PR #22 — the levelling PR; no separate branch)

> **Re-integration note (2026-07-11):** `main` merged the **real** Interac
> e-Transfer rail (`api/src/handlers/interac.rs`, `/api/v1/interac/etransfers`,
> #18). So this feature was reworked: the **payee registry stays** (a sender-side
> address book — main has no such thing) but was renamed to
> `api/src/handlers/interac_payees.rs` + table `11_interac_recipients.sql` to
> avoid the collision; and the **send was moved off the payee-tagged withdrawal
> onto the real rail** — `propose_interac_transfer` now confirms into
> `POST /api/v1/interac/etransfers` (with a security question/answer unless the
> recipient has autodeposit), via `BankClient.send_etransfer`. The
> `07_interac.sql` / withdrawal-send described below are superseded.

## Goal

Let a customer (via the personal manager) **register Interac recipients by email**
and **send a confirm-gated e-Transfer** to a registered payee. Registering payees
is the foundation; sending proves it works.

## Principle & boundaries

- Payees are **real, persistent bank data** — a new table in the bank's Postgres,
  exposed through a REST resource on `bank-api`, read by the agent through its
  existing read-only DB view.
- The **send is a payee-tagged withdrawal** through the existing external-cash
  rail. Registration is **not** a ledger event, and the send **reuses the
  unchanged `/transactions/withdrawal` endpoint** — so `transactions.rs`, the
  `Ledger` port, and the modern core are **untouched**. Only `bank-api`
  (cluster A) and the agent change; the modern-core cluster is not rebuilt.
- Recipients are **external** (an email, not necessarily a nano-bank customer), so
  the send cannot be an internal transfer — modelling it as a withdrawal is the
  correct semantics (money leaves the bank).

## 1. Data model

New `src/core/tables/07_interac.sql` (DDL lives at repo-root `src/core/tables/`,
loaded by `k8s/init-db-job.yaml`, which lists each script explicitly — so a new
`psql -f /scripts/07_interac.sql` line is added there too):

```sql
CREATE TABLE IF NOT EXISTS interac_recipients (
  recipient_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  customer_id  UUID NOT NULL REFERENCES customers(customer_id) ON DELETE CASCADE,
  email        TEXT NOT NULL,
  display_name TEXT NOT NULL,
  status       TEXT NOT NULL DEFAULT 'active',   -- active | removed
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (customer_id, email)
);
CREATE INDEX IF NOT EXISTS idx_interac_recipients_customer
  ON interac_recipients(customer_id);
```

- Wired into the init-Job DDL load order (the ConfigMap built from
  `src/core/tables/` in `k8s/deploy.sh`).
- Applied to the **already-running** bank Postgres via `kubectl exec … psql`
  (the `IF NOT EXISTS` form is idempotent, so re-running init is safe too).
- Soft delete: `remove` sets `status='removed'`; the `UNIQUE (customer_id, email)`
  is enforced against all rows, so re-registering a removed email is out of scope
  (documented; user removes then uses a different email, or we reactivate — see
  Out of scope).

## 2. Bank REST resource

New `api/src/handlers/interac.rs` (self-contained: request/response structs
inline, like `transactions.rs`), with `recipient_routes()` merged into the
existing `/api/v1/customers` nest in `main.rs`. Follows existing handler idioms
(inline `sqlx::query_as`, `AppState`, `AppError`, Postgres constraint-code
mapping `23505`→Conflict, `23503`→BadRequest). Identity comes from the verified
JWT via the existing `AuthenticatedCustomer` extractor (`auth.customer_id`) — the
same pattern as `/customers/profile` and `/transactions/*` — **not** a path
param. No new auth mechanism.

- `POST   /api/v1/customers/interac-recipients`
  body `{ "email": "...", "display_name": "..." }` → `201 { recipient }`
- `GET    /api/v1/customers/interac-recipients`
  → `200 [ {active recipients} ]`
- `DELETE /api/v1/customers/interac-recipients/{recipient_id}`
  → `204` (soft-delete: `status='removed'`)

Enum/serialisation: plain `TEXT` columns, no new SQL enum, to avoid an enum
migration.

## 3. Agent wiring

Read via the read-only `ClientContext`; write via the bank REST client — the
existing boundary is preserved (the agent gets **no** new direct-write path to
the DB).

- `agent/db.py`:
  - `interac_recipients(customer_id) -> list[dict]` (status='active',
    newest first): `recipient_id, email, display_name, created_at`.
  - `recipient(customer_id, recipient_id) -> dict | None`.
- `agent/bank.py` (customer identity is carried by the token, so no `customer_id`
  arg — mirrors `withdraw(token, …)`):
  - `register_recipient(token, email, display_name) -> dict`
    (`POST /api/v1/customers/interac-recipients`).
  - `remove_recipient(token, recipient_id) -> None`
    (`DELETE /api/v1/customers/interac-recipients/{recipient_id}`).
- `agent/actions.py`:
  - `_KINDS` gains `"interac"`.
  - `PendingAction` gains `payee_email: Optional[str] = None` (dataclass; `asdict`
    keeps serialising for the API/console).
  - `propose` for kind `interac`: requires `from_account` owned by the customer
    **and** `payee_email` matching a registered **active** recipient → else
    `ActDenied`. Enforces the same `max_per_tx`, amount, and balance checks as a
    withdrawal.
  - `execute` for kind `interac`: calls
    `bank.withdraw(token, from_account, amount,
    description=f"Interac e-Transfer to {payee_email}" + (f" — {memo}" if memo else ""),
    idempotency_key=<action id>)` and stores the result.

## 4. Manager surface

New MCP tools in `agent/mcp_server.py`, added to `LLM_TOOL_NAMES`. Immediate
writes (register/remove) need a bank client at the MCP layer, so `Deps` gains a
`bank: BankClient` (constructed once in `build_deps`; the propose path keeps using
`deps.actions`). The tools use `current_customer()` / `current_token()` from the
bound `X-Nano-*` headers.

- `register_interac_recipient(email: str, name: str) -> dict` —
  `deps.bank.register_recipient(current_token(), email, name)`.
- `list_interac_recipients() -> list` — `deps.db.interac_recipients(current_customer())`.
- `remove_interac_recipient(recipient_id: str) -> str` —
  `deps.bank.remove_recipient(current_token(), recipient_id)`.
- `propose_interac_transfer(payee_email: str, amount: str, memo: str = "") -> dict`
  — returns a pending action (id + expires_at), confirm-gated; **not** executed.

The manager's pending-action → confirm flow in `nano_manager.py` is already generic
(detects any tool result carrying `id` + `expires_at`), so no change is needed
there beyond the tools being included. The proposal restates **amount / origin
account / target = recipient email** (the existing prompt rule about restating
amount/origin/target already covers this; the send's "target" is the payee email).

New always-listed advisory skill `agent/skills/e-transfer.md` (kind `advisory`):
guidance to register a payee first, then propose → confirm, restating the
recipient email, and to be explicit that the money **leaves the bank** (an
external send, not an internal transfer). Never send to an unregistered email.

## 5. Testing

- **Agent pytest** (`agent/tests/`):
  - `test_db.py`: `interac_recipients` / `recipient` SQL shape via the fake-rows
    override.
  - `test_bank.py`: `register_recipient` / `remove_recipient` hit the right
    path/verb/body (mock `httpx`).
  - `test_actions.py`: `interac` propose (no money moves), unknown-payee →
    `ActDenied`, execute calls `bank.withdraw` with the payee-tagged description;
    over-limit → `ActDenied`.
  - `test_mcp_binding.py` (or a new `test_interac.py`): the four new tools are
    registered and in `LLM_TOOL_NAMES`.
  - `test_skills.py`: `e-transfer` skill loads (advisory, always listed).
- **Live** (in-cluster, after deploy): register a payee → list shows it →
  `propose_interac_transfer` (balance unchanged) → confirm (balance drops by the
  amount, a `withdrawal` transaction posted with the payee-tagged description) →
  payee still listed.

## 6. Deploy / migration

- `docker build` bank-api (Rust) + agent-api; `kind load` both into cluster
  `nano-bank`; `kubectl rollout restart` both deployments. Modern-core cluster is
  not touched.
- Migrate the running Postgres: `kubectl exec … psql < 07_interac.sql`.
- Update `CLAUDE.md` (bank + agent sections) and `agent/README.md` with the new
  capability and endpoints.

## Out of scope

- The real Interac network / settlement, inbound (receiving) e-Transfers,
  security questions, auto-deposit registration.
- Editing a payee in place (remove + re-add a different email instead).
- Reactivating a soft-removed email (the `UNIQUE(customer_id, email)` blocks
  re-registering the same removed email; revisit if needed).
- Per-recipient send limits beyond the existing global `max_per_tx`.

## Relates to

- `docs/superpowers/specs/2026-07-09-manager-skills-and-repo-reorg-design.md`
  (the skill system this extends with `e-transfer`).
- `docs/superpowers/specs/2026-07-07-personal-manager-design.md`
  (the two-phase confirm-gated action model reused here).
- Branch `interac-rail-foundation` — the future *real* Interac rail; this slice is
  the manager-facing payee registry + send that a real rail can later back.
