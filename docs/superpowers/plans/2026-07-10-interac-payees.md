# Interac Payees + Confirm-Gated e-Transfer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the personal manager register Interac recipients by email and send a confirm-gated e-Transfer (a payee-tagged withdrawal) to a registered payee.

**Architecture:** Payees persist in the bank's Postgres via a new REST resource on `bank-api` (identity from the JWT `AuthenticatedCustomer`, not a path param). The agent reads them through its read-only DB view and registers via REST. The "send" reuses the **unchanged** `/transactions/withdrawal` endpoint through the existing `ActionStore` two-phase gate — no GL/`transactions.rs`/modern-core changes.

**Tech Stack:** Rust (axum 0.7, sqlx), PostgreSQL 16, Python 3.12 (agent), pytest, Docker/kind/kubectl.

## Global Constraints

- **Do NOT touch** `api/src/handlers/transactions.rs`, the `Ledger` port, or the modern core. The send reuses the existing withdrawal endpoint as-is.
- **DDL lives at repo-root `src/core/tables/`** and is loaded by `k8s/init-db-job.yaml`, which lists each script explicitly (a new script needs a new `psql -f` line there).
- **Bank identity comes from the JWT** via `AuthenticatedCustomer` (`auth.customer_id`), never a path/query param — match `/customers/profile`.
- **Agent boundary:** reads via the read-only `ClientContext` (`agent/db.py`); writes via `BankClient` (`agent/bank.py`). No new direct-write path to the DB from the agent.
- **Skills are guidance, not tools.** Money movement stays two-phase confirm-gated. All new money movement goes through `ActionStore.propose`/`execute`.
- **Branch:** `agent-k8s-e2e` (PR #22 — the levelling PR). kubectl context `kind-nano-bank`. Agent venv `agent/.venv`.
- **Column types are plain `TEXT`** (no new SQL enum), to avoid an enum migration.

## File Structure

- Create `src/core/tables/07_interac.sql` — the `interac_recipients` table.
- Modify `k8s/init-db-job.yaml` — run the new script.
- Create `api/src/handlers/interac.rs` — recipient REST resource (inline structs).
- Modify `api/src/handlers/mod.rs` — `pub mod interac;`.
- Modify `api/src/main.rs` — merge `recipient_routes()` into the `/api/v1/customers` nest.
- Modify `agent/db.py` — `interac_recipients` / `recipient` reads.
- Modify `agent/bank.py` — `register_recipient` / `remove_recipient` writes.
- Modify `agent/actions.py` — `interac` kind (payee-tagged withdrawal).
- Modify `agent/mcp_server.py` — 4 new tools, `Deps.bank`, `LLM_TOOL_NAMES`.
- Create `agent/skills/e-transfer.md` — advisory skill.
- Modify `agent/nano_manager.py` — one prompt sentence about e-Transfers.
- Tests: `agent/tests/test_db.py`, `test_bank.py`, `test_actions.py`, new `test_interac_tools.py`, `test_skills.py`.
- Docs: `CLAUDE.md`, `agent/README.md`.

---

## Task 1: `interac_recipients` table + init wiring

**Files:**
- Create: `src/core/tables/07_interac.sql`
- Modify: `k8s/init-db-job.yaml` (after the `06_triggers.sql` line)

**Interfaces:**
- Produces: table `interac_recipients(recipient_id, customer_id, email, display_name, status, created_at)` with `UNIQUE(customer_id, email)`.

- [ ] **Step 1: Write the DDL file**

Create `src/core/tables/07_interac.sql`:
```sql
-- Interac e-Transfer recipients (payees) registered per customer.
-- Registration is not a ledger event; sending reuses the withdrawal rail.
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

- [ ] **Step 2: Add the init-job line**

In `k8s/init-db-job.yaml`, right after the `06_triggers.sql` line, add:
```yaml
    psql -h postgres-service -U nanobank_user -d nano_bank_db -f /scripts/07_interac.sql
```

- [ ] **Step 3: Apply to the running DB and verify**

```bash
cd /home/bmartins/dev/nano-bank
kubectl --context kind-nano-bank -n nano-bank exec -i deployment/postgres -- \
  psql -U nanobank_user -d nano_bank_db < src/core/tables/07_interac.sql
kubectl --context kind-nano-bank -n nano-bank exec deployment/postgres -- \
  psql -U nanobank_user -d nano_bank_db -c '\d interac_recipients'
```
Expected: `CREATE TABLE` / `CREATE INDEX` (or no error if re-run), and `\d` lists the columns `recipient_id, customer_id, email, display_name, status, created_at`.

- [ ] **Step 4: Commit**

```bash
git add src/core/tables/07_interac.sql k8s/init-db-job.yaml
git commit -m "feat(db): interac_recipients table + init-job wiring

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Bank REST resource for recipients

**Files:**
- Create: `api/src/handlers/interac.rs`
- Modify: `api/src/handlers/mod.rs`, `api/src/main.rs`

**Interfaces:**
- Consumes: Task 1's table; `AppState`, `AppError`, `AuthenticatedCustomer`.
- Produces:
  - `POST   /api/v1/customers/interac-recipients` `{email, display_name}` → `201 Json<Recipient>`
  - `GET    /api/v1/customers/interac-recipients` → `200 Json<Vec<Recipient>>` (active)
  - `DELETE /api/v1/customers/interac-recipients/{recipient_id}` → `204`
  - `pub fn recipient_routes() -> Router<AppState>` in `handlers::interac`.

- [ ] **Step 1: Write the handler module**

Create `api/src/handlers/interac.rs`:
```rust
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::errors::AppError;
use crate::handlers::AppState;
use crate::middleware::auth::AuthenticatedCustomer;

/// Routes mounted under `/api/v1/customers` (merged with `customer_routes`).
pub fn recipient_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/interac-recipients",
            post(register_recipient).get(list_recipients),
        )
        .route("/interac-recipients/{recipient_id}", axum::routing::delete(remove_recipient))
}

#[derive(Debug, Deserialize)]
struct CreateRecipientRequest {
    email: String,
    display_name: String,
}

#[derive(Debug, Serialize, FromRow)]
struct Recipient {
    recipient_id: Uuid,
    customer_id: Uuid,
    email: String,
    display_name: String,
    status: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

async fn register_recipient(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Json(payload): Json<CreateRecipientRequest>,
) -> Result<(StatusCode, Json<Recipient>), AppError> {
    if payload.email.trim().is_empty() || payload.display_name.trim().is_empty() {
        return Err(AppError::BadRequest(
            "email and display_name are required".to_string(),
        ));
    }
    let rec = sqlx::query_as::<_, Recipient>(
        r#"
        INSERT INTO interac_recipients (customer_id, email, display_name)
        VALUES ($1, $2, $3)
        RETURNING recipient_id, customer_id, email, display_name, status, created_at
        "#,
    )
    .bind(auth.customer_id)
    .bind(payload.email.trim())
    .bind(payload.display_name.trim())
    .fetch_one(&state.pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) => match db.code().as_deref() {
            Some("23505") => AppError::Conflict(
                "This recipient email is already registered".to_string(),
            ),
            Some("23503") => AppError::BadRequest("Unknown customer".to_string()),
            _ => AppError::Database(e),
        },
        _ => AppError::Database(e),
    })?;
    Ok((StatusCode::CREATED, Json(rec)))
}

async fn list_recipients(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
) -> Result<Json<Vec<Recipient>>, AppError> {
    let rows = sqlx::query_as::<_, Recipient>(
        r#"
        SELECT recipient_id, customer_id, email, display_name, status, created_at
        FROM interac_recipients
        WHERE customer_id = $1 AND status = 'active'
        ORDER BY created_at DESC
        "#,
    )
    .bind(auth.customer_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows))
}

async fn remove_recipient(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
    Path(recipient_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let res = sqlx::query(
        "UPDATE interac_recipients SET status = 'removed' \
         WHERE recipient_id = $1 AND customer_id = $2 AND status = 'active'",
    )
    .bind(recipient_id)
    .bind(auth.customer_id)
    .execute(&state.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound("Recipient not found".to_string()));
    }
    Ok(StatusCode::NO_CONTENT)
}
```

- [ ] **Step 2: Register the module**

In `api/src/handlers/mod.rs`, add (keep alphabetical-ish with the others):
```rust
pub mod interac;
```

- [ ] **Step 3: Merge the routes into the customers nest**

In `api/src/main.rs`, change the customers nest line:
```rust
// was:
//     .nest("/api/v1/customers", handlers::customers::customer_routes())
        .nest(
            "/api/v1/customers",
            handlers::customers::customer_routes()
                .merge(handlers::interac::recipient_routes()),
        )
```

- [ ] **Step 4: Compile + lint**

```bash
cd /home/bmartins/dev/nano-bank/api
cargo build 2>&1 | tail -5
cargo clippy 2>&1 | tail -5
```
Expected: builds successfully (pre-existing dead-code warnings are fine); no new errors. (The repo has no cargo tests; the live HTTP check runs in Task 8 after deploy.)

- [ ] **Step 5: Commit**

```bash
cd /home/bmartins/dev/nano-bank
git add api/src/handlers/interac.rs api/src/handlers/mod.rs api/src/main.rs
git commit -m "feat(api): interac-recipients REST resource (register/list/remove)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Agent reads — `db.py`

**Files:**
- Modify: `agent/db.py`
- Test: `agent/tests/test_db.py`

**Interfaces:**
- Produces:
  - `ClientContext.interac_recipients(customer_id) -> list[dict]` (active, newest first): `recipient_id, email, display_name, created_at`.
  - `ClientContext.recipient(customer_id, recipient_id) -> dict | None`.

- [ ] **Step 1: Write the failing test**

Add to `agent/tests/test_db.py` (follow the file's existing `_rows`-override pattern — a subclass/monkeypatch that returns canned rows):
```python
def test_interac_recipients_query_shape():
    from agent.db import ClientContext
    captured = {}

    class Fake(ClientContext):
        def _rows(self, sql, params):
            captured["sql"] = sql
            captured["params"] = params
            return [{"recipient_id": "r1", "email": "a@b.ca",
                     "display_name": "Ada", "created_at": "t"}]

    ctx = Fake()
    out = ctx.interac_recipients("cust-1")
    assert out[0]["email"] == "a@b.ca"
    assert "interac_recipients" in captured["sql"]
    assert "status = 'active'" in captured["sql"]
    assert captured["params"] == ("cust-1",)


def test_recipient_by_id_returns_none_when_absent():
    from agent.db import ClientContext

    class Fake(ClientContext):
        def _rows(self, sql, params):
            return []

    assert Fake().recipient("cust-1", "nope") is None
```

- [ ] **Step 2: Run to verify it fails**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_db.py -q -k "interac or recipient"`
Expected: FAIL (`AttributeError: 'Fake' object has no attribute 'interac_recipients'`).

- [ ] **Step 3: Implement**

Add to `agent/db.py` (inside `ClientContext`, next to `accounts`/`cards`):
```python
    def interac_recipients(self, customer_id: str) -> list[dict]:
        return self._rows(
            "-- interac_recipients\nSELECT recipient_id, email, display_name, created_at "
            "FROM interac_recipients WHERE customer_id = %s AND status = 'active' "
            "ORDER BY created_at DESC", (customer_id,))

    def recipient(self, customer_id: str, recipient_id: str) -> Optional[dict]:
        rows = self._rows(
            "-- recipient\nSELECT recipient_id, email, display_name, created_at "
            "FROM interac_recipients WHERE customer_id = %s AND recipient_id = %s "
            "AND status = 'active'", (customer_id, recipient_id))
        return rows[0] if rows else None
```

- [ ] **Step 4: Run to verify it passes**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_db.py -q`
Expected: PASS (existing + 2 new).

- [ ] **Step 5: Commit**

```bash
git add agent/db.py agent/tests/test_db.py
git commit -m "feat(agent): read interac recipients from the bank DB

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Agent writes — `bank.py`

**Files:**
- Modify: `agent/bank.py`
- Test: `agent/tests/test_bank.py`

**Interfaces:**
- Consumes: the Task 2 REST endpoints.
- Produces:
  - `BankClient.register_recipient(token, email, display_name) -> dict` → `POST /api/v1/customers/interac-recipients`.
  - `BankClient.remove_recipient(token, recipient_id) -> None` → `DELETE /api/v1/customers/interac-recipients/{recipient_id}`.

- [ ] **Step 1: Write the failing test**

Add to `agent/tests/test_bank.py` using the file's existing `_client(handler)` helper (real `httpx.Client` over `httpx.MockTransport`; `json` and `httpx` are already imported):
```python
def test_register_recipient_posts_email_and_name():
    seen = {}

    def handler(req: httpx.Request) -> httpx.Response:
        seen["url"] = str(req.url)
        seen["auth"] = req.headers.get("authorization")
        seen["body"] = json.loads(req.content)
        return httpx.Response(201, json={"recipient_id": "r1", "email": "a@b.ca"})

    out = _client(handler).register_recipient("jwt", "a@b.ca", "Ada")
    assert out["recipient_id"] == "r1"
    assert seen["url"].endswith("/api/v1/customers/interac-recipients")
    assert seen["auth"] == "Bearer jwt"
    assert seen["body"] == {"email": "a@b.ca", "display_name": "Ada"}


def test_remove_recipient_deletes_by_id():
    seen = {}

    def handler(req: httpx.Request) -> httpx.Response:
        seen["method"] = req.method
        seen["url"] = str(req.url)
        seen["auth"] = req.headers.get("authorization")
        return httpx.Response(204)

    _client(handler).remove_recipient("jwt", "r1")
    assert seen["method"] == "DELETE"
    assert seen["url"].endswith("/api/v1/customers/interac-recipients/r1")
    assert seen["auth"] == "Bearer jwt"
```

- [ ] **Step 2: Run to verify it fails**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_bank.py -q -k recipient`
Expected: FAIL (`AttributeError: 'BankClient' object has no attribute 'register_recipient'`).

- [ ] **Step 3: Implement**

Add to `agent/bank.py` `BankClient` (after `withdraw`). Note `_post` already sends the Bearer header; add a small `_delete` for the DELETE verb:
```python
    def register_recipient(self, token, email, display_name) -> dict:
        return self._post("/api/v1/customers/interac-recipients",
                          {"email": email, "display_name": display_name},
                          token=token)

    def remove_recipient(self, token, recipient_id) -> None:
        headers = {"Authorization": f"Bearer {token}"} if token else {}
        r = self.http.request("DELETE",
                              self.base + f"/api/v1/customers/interac-recipients/{recipient_id}",
                              headers=headers)
        if r.status_code // 100 != 2:
            raise BankError(r.status_code, _safe_json(r))
```

- [ ] **Step 4: Run to verify it passes**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_bank.py -q`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add agent/bank.py agent/tests/test_bank.py
git commit -m "feat(agent): register/remove interac recipients via bank REST

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: `interac` action kind — `actions.py`

**Files:**
- Modify: `agent/actions.py`
- Test: `agent/tests/test_actions.py`

**Interfaces:**
- Consumes: `ClientContext.interac_recipients` (Task 3); `BankClient.withdraw` (existing).
- Produces: `ActionStore.propose(..., kind="interac", amount=..., from_account=..., payee_email=..., memo=...)` → pending action; `execute` maps it to a payee-tagged `bank.withdraw`. Unknown/invalid payee → `ActDenied`.

- [ ] **Step 1: Write the failing test**

First extend the module-level fakes in `agent/tests/test_actions.py` so `FakeDB` answers `interac_recipients` and `FakeBank` records `withdraw` calls:
```python
# in FakeDB: add (recipients defaults to empty via getattr)
    def interac_recipients(self, customer_id):
        return getattr(self, "recipients", [])

# in FakeBank.__init__: add a second list
        self.withdraw_calls = []
# in FakeBank: add the method
    def withdraw(self, token, account_id, amount, description="Withdrawal", idempotency_key=None):
        self.withdraw_calls.append((idempotency_key, str(amount), description))
        return {"transaction_id": "w-" + (idempotency_key or "x")}
```

Then add the tests (the `_store(**kw)` helper returns `(s, db, bank, audit, clock)`):
```python
def test_interac_propose_requires_registered_payee():
    db = FakeDB(["acc-1"])
    db.recipients = []  # no payees registered
    s, _db, _bank, _audit, _clock = _store(db=db)
    with pytest.raises(ActDenied):
        s.propose("cust-1", "tok", "interac", amount="10",
                  from_account="acc-1", payee_email="x@y.ca")


def test_interac_execute_calls_withdraw_with_payee_tag():
    db = FakeDB(["acc-1"])
    db.recipients = [{"email": "x@y.ca", "display_name": "X"}]
    bank = FakeBank()
    s, _db, _bank, _audit, _clock = _store(db=db, bank=bank)
    prop = s.propose("cust-1", "tok", "interac", amount="10",
                     from_account="acc-1", payee_email="x@y.ca", memo="rent")
    assert s.get(prop["id"], "cust-1")["payee_email"] == "x@y.ca"
    s.execute(prop["id"], "cust-1", "tok")
    idem, amt, desc = bank.withdraw_calls[-1]
    assert "x@y.ca" in desc and "rent" in desc and amt == "10"
```

- [ ] **Step 2: Run to verify it fails**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_actions.py -q -k interac`
Expected: FAIL (`ActDenied("unknown kind: interac")` or missing `payee_email`).

- [ ] **Step 3: Implement**

In `agent/actions.py`:

Add `interac` to the kinds set:
```python
_KINDS = {"transfer", "deposit", "withdraw", "interac"}
```

Add the field to `PendingAction` (after `memo`):
```python
    payee_email: Optional[str] = None
```

Extend `propose` signature and add interac validation. Change the signature:
```python
    def propose(self, customer_id, token, kind, *, amount,
                from_account=None, to_account=None, memo=None, payee_email=None) -> dict:
```
After the existing ownership loop (before the `transfer` needs both check), add:
```python
        if kind == "interac":
            if not from_account:
                raise ActDenied("interac needs a from_account")
            if not from_account or not self.db.owns_account(customer_id, from_account):
                raise ActDenied(f"account {from_account} is not yours")
            emails = {r.get("email") for r in self.db.interac_recipients(customer_id)}
            if not payee_email or payee_email not in emails:
                self._audit(customer_id, kind, a, "denied", "unregistered payee")
                raise ActDenied(f"'{payee_email}' is not a registered recipient")
```
Note: the generic ownership loop already covers `("transfer","withdraw")`; `interac` is validated explicitly above, so leave that loop's tuple unchanged.

Thread `payee_email` into the `PendingAction(...)` construction:
```python
        pa = PendingAction(id=pid, customer_id=customer_id, kind=kind, amount=str(a),
                           from_account=from_account, to_account=to_account, memo=memo,
                           payee_email=payee_email,
                           created_at=now, expires_at=now + self.ttl)
```

In `execute`, add an `interac` branch before the final `else`:
```python
            elif pa.kind == "interac":
                desc = f"Interac e-Transfer to {pa.payee_email}"
                if pa.memo:
                    desc += f" — {pa.memo}"
                res = self.bank.withdraw(token, pa.from_account, pa.amount,
                                         description=desc, idempotency_key=pa.id)
```

In `_summary`, add before the final withdraw return:
```python
        if pa.kind == "interac":
            return f"Interac e-Transfer {pa.amount} from {pa.from_account} to {pa.payee_email}" + \
                   (f" ({pa.memo})" if pa.memo else "")
```

- [ ] **Step 4: Run to verify it passes**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_actions.py -q`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add agent/actions.py agent/tests/test_actions.py
git commit -m "feat(agent): interac action kind — payee-tagged, confirm-gated withdrawal

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: MCP tools + manager wiring

**Files:**
- Modify: `agent/mcp_server.py`
- Test: `agent/tests/test_interac_tools.py` (new)

**Interfaces:**
- Consumes: `deps.db.interac_recipients` (T3), `BankClient.register_recipient/remove_recipient` (T4), `deps.actions.propose(kind="interac", ...)` (T5).
- Produces: MCP tools `register_interac_recipient`, `list_interac_recipients`, `remove_interac_recipient`, `propose_interac_transfer`; all four in `LLM_TOOL_NAMES`; `Deps.bank: BankClient`.

- [ ] **Step 1: Write the failing test**

Create `agent/tests/test_interac_tools.py`:
```python
from agent.mcp_server import LLM_TOOL_NAMES


def test_interac_tools_are_in_llm_toolset():
    assert {"register_interac_recipient", "list_interac_recipients",
            "remove_interac_recipient", "propose_interac_transfer"} <= LLM_TOOL_NAMES


def test_deps_has_bank_field():
    import dataclasses
    from agent.mcp_server import Deps
    fields = {f.name for f in dataclasses.fields(Deps)}
    assert "bank" in fields
```

- [ ] **Step 2: Run to verify it fails**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_interac_tools.py -q`
Expected: FAIL (names not in `LLM_TOOL_NAMES`; no `bank` field).

- [ ] **Step 3: Implement**

In `agent/mcp_server.py`:

Extend `LLM_TOOL_NAMES`:
```python
LLM_TOOL_NAMES = frozenset({
    "get_profile", "get_accounts", "get_transactions", "get_cards",
    "recall", "remember", "propose_transfer", "propose_deposit", "propose_withdraw",
    "register_interac_recipient", "list_interac_recipients",
    "remove_interac_recipient", "propose_interac_transfer"})
```

Add `bank` to `Deps`:
```python
@dataclass
class Deps:
    db: ClientContext
    memory: QdrantMemory
    audit: AuditLog
    actions: ActionStore
    bank: BankClient
```

In `build_deps`, reuse the same `BankClient` for both actions and Deps:
```python
    bank = BankClient(settings.nano_bank_api)
    actions = ActionStore(db, bank, audit,
                          ...)  # keep the existing remaining args
    return Deps(db=db, memory=memory, audit=audit, actions=actions, bank=bank)
```
(Ensure `from .bank import BankClient` is imported at the top of `mcp_server.py`; add it if missing.)

Register the four tools inside `build_mcp` (next to the other `@mcp.tool()`s):
```python
    @mcp.tool()
    def register_interac_recipient(email: str, name: str) -> dict:
        """Register an Interac e-Transfer recipient (payee) for the bound client."""
        return deps.bank.register_recipient(current_token(), email, name)

    @mcp.tool()
    def list_interac_recipients() -> list:
        """List the bound client's registered Interac recipients (payees)."""
        return deps.db.interac_recipients(current_customer())

    @mcp.tool()
    def remove_interac_recipient(recipient_id: str) -> str:
        """Remove a registered Interac recipient by id."""
        deps.bank.remove_recipient(current_token(), recipient_id)
        return f"removed {recipient_id}"

    @mcp.tool()
    def propose_interac_transfer(payee_email: str, amount: str, memo: str = "") -> dict:
        """Propose an Interac e-Transfer to a REGISTERED payee. Requires confirmation."""
        return _propose("interac", amount=amount, from_account=from_account_for(payee_email),
                        payee_email=payee_email, memo=memo or None)
```
The manager must pass the sending account. Simpler and explicit — give the tool a `from_account` param instead of inferring it:
```python
    @mcp.tool()
    def propose_interac_transfer(payee_email: str, amount: str,
                                 from_account: str, memo: str = "") -> dict:
        """Propose an Interac e-Transfer from one of the client's accounts to a
        REGISTERED payee email. Requires confirmation."""
        return _propose("interac", amount=amount, from_account=from_account,
                        payee_email=payee_email, memo=memo or None)
```
(Use this second form; delete the `from_account_for` variant.)

- [ ] **Step 4: Run to verify it passes + full suite**

```bash
agent/.venv/bin/python -m pytest agent/tests/test_interac_tools.py -q
agent/.venv/bin/python -m pytest agent -q 2>&1 | tail -2
```
Expected: new tests PASS; full suite green (previous + all new).

- [ ] **Step 5: Commit**

```bash
git add agent/mcp_server.py agent/tests/test_interac_tools.py
git commit -m "feat(agent): MCP tools for interac payees + confirm-gated e-Transfer

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: `e-transfer` skill + manager prompt

**Files:**
- Create: `agent/skills/e-transfer.md`
- Modify: `agent/nano_manager.py`
- Test: `agent/tests/test_skills.py`, `agent/tests/test_nano_manager.py`

**Interfaces:**
- Consumes: the `SkillRegistry` (already loads `agent/skills/*.md`).
- Produces: an advisory skill `e-transfer` (always listed); a prompt sentence naming the register→propose→confirm flow.

- [ ] **Step 1: Write the failing tests**

Add to `agent/tests/test_skills.py`:
```python
def test_etransfer_skill_present_and_advisory():
    from pathlib import Path
    reg = SkillRegistry.from_dir(Path(__file__).resolve().parents[1] / "skills")
    s = reg.get("e-transfer")
    assert s is not None and s.kind == "advisory"
```
Add to `agent/tests/test_nano_manager.py`:
```python
def test_prompt_mentions_registered_payee():
    p = NM.MANAGER_PROMPT.lower()
    assert "register" in p and ("payee" in p or "recipient" in p)
```

- [ ] **Step 2: Run to verify they fail**

```bash
agent/.venv/bin/python -m pytest agent/tests/test_skills.py -q -k etransfer
agent/.venv/bin/python -m pytest agent/tests/test_nano_manager.py -q -k registered_payee
```
Expected: FAIL (skill missing; prompt words absent).

- [ ] **Step 3: Implement**

Create `agent/skills/e-transfer.md`:
```markdown
---
name: e-transfer
description: Sending Interac e-Transfers to registered payees, and registering new payees by email.
kind: advisory
---
Interac e-Transfers send money OUT of the bank to a recipient's email — it is not
an internal transfer, so the funds leave the client's account for good once sent.
Before you can send, the recipient must be a REGISTERED payee: use
register_interac_recipient(email, name) to add one, and list_interac_recipients()
to see who is already registered. To send, call propose_interac_transfer with the
payee's email, the amount, and the client's source account — this only PROPOSES;
restate the amount, the source account, and the recipient email, and the client
must CONFIRM before any money moves. Never send to an unregistered email, and make
clear that an e-Transfer leaves the bank (unlike moving money between the client's
own accounts).
```

In `agent/nano_manager.py`, append to `MANAGER_PROMPT` (after the skills sentence):
```python
    " For Interac e-Transfers: a recipient must be a registered payee first "
    "(register_interac_recipient); then propose_interac_transfer proposes a "
    "confirm-gated send to that payee's email — never send to an unregistered "
    "recipient."
```

- [ ] **Step 4: Run to verify pass + full suite**

```bash
agent/.venv/bin/python -m pytest agent -q 2>&1 | tail -2
```
Expected: green (all previous + new).

- [ ] **Step 5: Commit**

```bash
git add agent/skills/e-transfer.md agent/nano_manager.py agent/tests/test_skills.py agent/tests/test_nano_manager.py
git commit -m "feat(agent): e-transfer skill + prompt guidance for registered payees

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: Deploy, migrate, live verification + docs

**Files:**
- Modify: `CLAUDE.md`, `agent/README.md`
- Deploy: bank-api + agent-api images

**Interfaces:**
- Consumes: the running two-cluster stack (cluster `nano-bank`).

- [ ] **Step 1: Ensure the table exists on the running DB**

(Idempotent — safe even if Task 1 already applied it.)
```bash
cd /home/bmartins/dev/nano-bank
kubectl --context kind-nano-bank -n nano-bank exec -i deployment/postgres -- \
  psql -U nanobank_user -d nano_bank_db < src/core/tables/07_interac.sql
```

- [ ] **Step 2: Rebuild + reload bank-api (Rust) and agent-api (Python)**

**IMPORTANT:** `mcp_server.py` changed (the new tools live there), and it runs in
the **`agent-mcp`** deployment — NOT `agent-api`. Rebuild all three: `bank-api`
(Rust handler), `agent-mcp` (the tools), and `agent-api` (nano_manager prompt).

```bash
cd /home/bmartins/dev/nano-bank/api
docker build -t nano-bank-api:dev . -q && kind load docker-image nano-bank-api:dev --name nano-bank
cd /home/bmartins/dev/nano-bank/agent
docker build -f Dockerfile.api -t nano-agent-api:dev . -q && kind load docker-image nano-agent-api:dev --name nano-bank
docker build -f Dockerfile.mcp -t nano-agent-mcp:dev . -q && kind load docker-image nano-agent-mcp:dev --name nano-bank
kubectl --context kind-nano-bank -n nano-bank rollout restart deploy/bank-api deploy/agent-mcp deploy/agent-api
kubectl --context kind-nano-bank -n nano-bank rollout status deploy/bank-api --timeout=180s
kubectl --context kind-nano-bank -n nano-bank rollout status deploy/agent-mcp --timeout=180s
kubectl --context kind-nano-bank -n nano-bank rollout status deploy/agent-api --timeout=180s
```
(Confirm the bank-api deployment name and image tag match `k8s/bank-api-deployment.yaml`; adjust the `docker build -t` tag to whatever that manifest references.)

- [ ] **Step 2b: Direct REST smoke (bank resource works with a real JWT)**

```bash
cd /home/bmartins/dev/nano-bank/agent
kubectl --context kind-nano-bank -n nano-bank port-forward svc/agent-api 8086:8086 >/tmp/pf.log 2>&1 &
PF=$!; sleep 5
TOKEN=$(grep -E '^BRANCH_SERVICE_TOKEN=' .env | cut -d= -f2-); H="Authorization: Bearer $TOKEN"
SEED=$(curl -fsS -m120 -X POST localhost:8086/branch/seed -H "$H")
ADA=$(echo "$SEED" | python3 -c 'import sys,json;print(json.load(sys.stdin)["customers"][0]["customer_id"])')
echo "ADA=$ADA"   # (REST is validated end-to-end via the manager in Step 3)
kill $PF 2>/dev/null
```

- [ ] **Step 3: Live end-to-end through the manager**

```bash
cd /home/bmartins/dev/nano-bank/agent
kubectl --context kind-nano-bank -n nano-bank port-forward svc/agent-api 8086:8086 >/tmp/pf.log 2>&1 &
PF=$!; sleep 5
TOKEN=$(grep -E '^BRANCH_SERVICE_TOKEN=' .env | cut -d= -f2-); H="Authorization: Bearer $TOKEN"
j(){ python3 -c "import sys,json;print($1)"; }
say(){ curl -fsS -m150 -X POST "localhost:8086/branch/clients/$1/message" -H "$H" -H 'content-type: application/json' -d "$2"; }
bal(){ curl -fsS -m30 "localhost:8086/branch/clients/$1/accounts" -H "$H" | python3 -c 'import sys,json;print(sum(float(a["balance"]) for a in json.load(sys.stdin)))'; }

SEED=$(curl -fsS -m120 -X POST localhost:8086/branch/seed -H "$H")
ADA=$(echo "$SEED" | j 'json.load(sys.stdin)["customers"][0]["customer_id"]')
ADA_ACC=$(echo "$SEED" | j 'json.load(sys.stdin)["customers"][0]["account_id"]')

echo "== register payee =="
say "$ADA" '{"message":"Register an Interac payee: email sam@example.ca, name Sam"}' | j 'json.load(sys.stdin)["answer"]'
echo "== list payees =="
say "$ADA" '{"message":"Who are my Interac payees?"}' | j 'json.load(sys.stdin)["answer"]'
echo "== propose send (no money moves) =="
PROP=$(say "$ADA" "{\"message\":\"Send a 30 dollar Interac e-transfer to sam@example.ca from $ADA_ACC\"}")
echo "$PROP" | j 'json.load(sys.stdin)["answer"]'
AID=$(echo "$PROP" | j '(json.load(sys.stdin).get("pending_action") or {}).get("id","")')
echo "pending=$AID  balance_after_propose=$(bal "$ADA")  (expect 1000.0)"
echo "== confirm (money leaves) =="
curl -fsS -m60 -X POST "localhost:8086/branch/clients/$ADA/actions/$AID/confirm" -H "$H" >/dev/null
echo "balance_after_confirm=$(bal "$ADA")  (expect 970.0)"
kill $PF 2>/dev/null
```
Expected: payee registered + listed; propose leaves balance at `1000.0`; after confirm the balance is `970.0` (a withdrawal posted with an "Interac e-Transfer to sam@example.ca" description).

- [ ] **Step 4: Update docs**

- `CLAUDE.md` (agent section): note the new manager capability — register/list/remove Interac payees + confirm-gated `propose_interac_transfer` (a payee-tagged withdrawal; real Interac network is out of scope).
- `agent/README.md`: add the four tools + the register→propose→confirm flow to the tool list.

- [ ] **Step 5: Commit + push**

```bash
cd /home/bmartins/dev/nano-bank
git add CLAUDE.md agent/README.md
git commit -m "docs: interac payees + e-transfer manager capability"
git push origin agent-k8s-e2e
```

---

## Self-Review notes

- **Spec coverage:** §1 data model → T1; §2 REST resource → T2; §3 agent reads → T3, writes → T4, actions → T5; §4 manager surface (tools + Deps.bank + skill + prompt) → T6+T7; §5 testing → per-task pytest + T8 live; §6 deploy/migration/docs → T8. Out-of-scope items (real network, inbound, reactivation) are not implemented.
- **Placeholder scan:** every step has concrete code/commands; the one design choice inside T6 (from_account param vs inference) is resolved explicitly to the `from_account` param form.
- **Type/name consistency:** `interac_recipients`/`recipient` (db), `register_recipient`/`remove_recipient` (bank), kind `"interac"` + `payee_email` (actions), the four MCP tool names, and `Deps.bank` are used identically across T3–T8. Endpoints derive `customer_id` from the JWT in both the Rust handler (T2) and the agent client (T4).
- **Watch-outs:** `interac` is validated explicitly in `propose` (not via the generic ownership loop); the bank-api image tag/deployment name in T8 must match `k8s/bank-api-deployment.yaml`; the DELETE uses `http.request("DELETE", ...)` since `_post` is POST-only.
