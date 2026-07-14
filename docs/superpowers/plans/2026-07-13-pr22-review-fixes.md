# PR #22 Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the six verified findings from blashkar's review of PR #22 (external mandated agent) — the money-correctness, security, and data-model gaps in the branch gateway, the bank client, the seed, and the interac_recipients table.

**Architecture:** Fixes span three layers. Python branch/agent (`agent/`) — most fixes: pin the mandate payee (#2), move idempotency into the transfer body (#1), stabilize the gateway idempotency key end-to-end (#3), surface the bank's 202 parked-approval as `pending_approval` (#4), TTL the customer-token cache (#6). Rust/SQL bank (`api/`, `src/core/tables/`) — the interac_recipients partial-unique-index + migration self-heal + file renumber (#5). Each Python task is TDD against the existing `pytest agent` suite; the Rust/SQL task is verified by `cargo build` + a live register→remove→register loop.

**Tech Stack:** Python 3.11 / FastAPI / httpx / pytest (branch + agent); Rust/axum + sqlx + PostgreSQL (bank); Kind/k8s (deploy); glm-5.2 via Ollama-cloud (planner, live only).

## Global Constraints

- **glm-5.2 only** — no model fallback anywhere (already the state of `model_factory`); do not reintroduce 4.7.
- **Deploy reality: unit-green ≠ deployed.** The agent's tools live in `agent-mcp`; `nano_manager`/gateway live in `agent-api`; Rust handlers live in `bank-api`. A change to `agent/api.py`/`seed.py`/`bank.py`/`mandate_gateway.py`/`external_agent/` rebuilds **`nano-agent-api:dev`**; a change to `api/`/`src/core/tables/` rebuilds **`nano-bank-api:dev`** (and re-runs the DDL init Job for a fresh table). Live-verify every change; do not claim done on unit tests alone.
- **kubectl needs the snap env** in this shell: `export XDG_RUNTIME_DIR=/run/user/1000 XDG_DATA_HOME=/home/bmartins/.local/share`; context `kind-nano-bank`, namespace `nano-bank`.
- **Branch:** `agent-k8s-e2e` (the levelling PR #22). All commits land here.
- Do **not** touch `api/src/handlers/transactions.rs` posting internals (PR #15 territory) beyond what #5 requires (nothing — #5 is table/migration only).
- Finding **#7 (A2A scope granularity) is deliberately out of scope** — a design decision, not a bug; left as-is.

---

### Task 1: Pin the mandate payee (#2, security)

The seeded mandate grants `max_per_tx/daily_cap` but no `allowed_payees`, and `gw_act` forwards the caller's `to_account_id` — so a prompt-injected step can send anywhere up to the daily cap. The bank already enforces `allowed_payees` (`policy.rs` → `PAYEE_NOT_ALLOWED`); we just have to populate it. Requires creating the Epcor biller **before** the mandate so its account id can go into `allowed_payees`.

**Files:**
- Modify: `agent/seed.py` (`seed_agent_mandate`, ~lines 53-67)
- Test: `agent/tests/test_seed_mandate.py` (create)

**Interfaces:**
- Consumes: `MandateClient.create_mandate(customer_token, payload)`, `_seed_epcor_biller(bank) -> str`.
- Produces: `seed_agent_mandate` unchanged signature, still returns `{agent_id, agent_secret, mandate_id, epcor_account_id}`; the created mandate now carries `allowed_payees: [epcor_account_id]`.

- [ ] **Step 1: Write the failing test**

```python
# agent/tests/test_seed_mandate.py
from agent.seed import seed_agent_mandate


class _FakeBank:
    base = "http://bank"
    def __init__(self): self.created_accounts = 0
    def create_customer(self, payload): return {"customer_id": "epcor-cust"}
    def login(self, email, pw): return "epcor-tok"
    def create_account(self, tok, payload):
        self.created_accounts += 1
        return {"account_id": "EPCOR-ACCT"}


class _FakeMC:
    def __init__(self, *a, **k): self.mandate_payload = None
    def register_agent(self, name): return {"agent_id": "ag1", "agent_secret": "sec"}
    def create_mandate(self, tok, payload):
        self.mandate_payload = payload
        return {"mandate_id": "M1"}


def test_seeded_mandate_pins_the_epcor_payee(monkeypatch):
    fake_mc = _FakeMC()
    monkeypatch.setattr("agent.seed.MandateClient", lambda *a, **k: fake_mc)
    out = seed_agent_mandate(_FakeBank(), "cust-tok", "A1")
    # the biller must exist and be the sole allowed payee on the mandate
    assert out["epcor_account_id"] == "EPCOR-ACCT"
    assert fake_mc.mandate_payload["allowed_payees"] == ["EPCOR-ACCT"]
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /home/bmartins/dev/nano-bank && python -m pytest agent/tests/test_seed_mandate.py -q`
Expected: FAIL — `KeyError: 'allowed_payees'` (payload has no such key).

- [ ] **Step 3: Write minimal implementation**

Rewrite `seed_agent_mandate` so the biller is created first and pinned:

```python
def seed_agent_mandate(bank, customer_token, account_id) -> dict:
    """Register an external agent and grant it a mandate on `account_id`, whose
    ONLY allowed payee is a freshly-seeded Epcor biller — so an LLM-planned
    destination that isn't Epcor is denied by the bank (PAYEE_NOT_ALLOWED)."""
    from datetime import datetime, timedelta, timezone
    from .mandate_gateway import MandateClient
    epcor_account_id = _seed_epcor_biller(bank)   # create biller BEFORE the mandate
    mc = MandateClient(bank.base, "", "")
    agent = mc.register_agent("Demo External Agent")
    mandate = mc.create_mandate(customer_token, {
        "agent_id": agent["agent_id"], "account_id": account_id,
        "scopes": ["read:balance", "read:transactions", "transfer:initiate",
                   "account:open", "payee:register"],
        "max_per_tx": "100", "daily_cap": "500",
        "allowed_payees": [epcor_account_id],
        "expires_at": (datetime.now(timezone.utc) + timedelta(hours=1)).isoformat()})
    return {"agent_id": agent["agent_id"], "agent_secret": agent["agent_secret"],
            "mandate_id": mandate["mandate_id"], "epcor_account_id": epcor_account_id}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest agent/tests/test_seed_mandate.py -q`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add agent/seed.py agent/tests/test_seed_mandate.py
git commit -m "fix(agent): pin seeded mandate to the Epcor payee (allowed_payees)

Review #2: without allowed_payees a prompt-injected transfer_out step could
send anywhere up to the daily cap. The bank enforces allowed_payees already;
seed the biller before the mandate and pin it.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Move the confirm-path idempotency key into the transfer body (#1, money)

`bank.py::_post` sends `idempotency_key` as an HTTP `Idempotency-Key` header, but the bank reads it only from the JSON body (`MoneyTransferRequest.idempotency_key`) — no header is read anywhere. So `actions.py::execute`'s `idempotency_key=pa.id` on the manager confirm path is silently dropped, and a retry after an execute timeout double-pays. Fix `transfer` to put the key in the body, matching `send_etransfer`.

**Files:**
- Modify: `agent/bank.py` (`transfer`, ~lines 48-54)
- Test: `agent/tests/test_bank_idempotency.py` (create)

**Interfaces:**
- Consumes: nothing new.
- Produces: `BankClient.transfer(...)` unchanged signature; the POST body for `/transactions/transfer` now includes `idempotency_key` when provided.

- [ ] **Step 1: Write the failing test**

```python
# agent/tests/test_bank_idempotency.py
from agent.bank import BankClient


class _CapturingHTTP:
    def __init__(self): self.last = None
    def post(self, url, json=None, headers=None):
        self.last = {"url": url, "json": json, "headers": headers}
        class _R:
            status_code = 200
            def json(self): return {"transaction_id": "t1"}
        return _R()


def test_transfer_sends_idempotency_key_in_body():
    http = _CapturingHTTP()
    bank = BankClient("http://bank", http=http)
    bank.transfer("tok", "A1", "A2", "50", memo="rent", idempotency_key="idem-123")
    assert http.last["json"]["idempotency_key"] == "idem-123"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest agent/tests/test_bank_idempotency.py -q`
Expected: FAIL — `KeyError: 'idempotency_key'` (currently only in the header).

- [ ] **Step 3: Write minimal implementation**

```python
    def transfer(self, token, from_account, to_account, amount, memo=None,
                 idempotency_key=None) -> dict:
        # bank-api MoneyTransferRequest requires `description`; the human memo maps to it.
        # The bank reads idempotency_key from the BODY (never a header), so put it there.
        body = {"from_account_id": from_account, "to_account_id": to_account,
                "amount": str(amount), "description": memo or "Transfer"}
        if idempotency_key:
            body["idempotency_key"] = idempotency_key
        return self._post("/api/v1/transactions/transfer", body, token=token)
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest agent/tests/test_bank_idempotency.py -q`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add agent/bank.py agent/tests/test_bank_idempotency.py
git commit -m "fix(agent): send transfer idempotency_key in body, not header

Review #1: the bank reads idempotency_key from the JSON body only; bank.py
sent it as an Idempotency-Key header, so the confirm path's key was dropped
and a retried confirm double-paid. (deposit/withdraw have no bank-side key
support — out of scope.)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Stabilize the gateway idempotency key end-to-end (#3, money)

`gw_act` mints a fresh `uuid4` per invocation, so re-running the same instruction (or a retried act) is a *different* payment to the bank — the agent plane's mandatory key can't dedupe it. Make the external agent derive a stable key per act step (deterministic from op+params) and thread it through `GatewayHTTP.act` → `gw_act`, which uses the supplied key instead of minting one.

**Files:**
- Modify: `agent/external_agent/agent.py` (`GatewayHTTP.act` ~23-25; `ExternalAgent.run` ~72-85; add `_idem_key` helper)
- Modify: `agent/api.py` (`gw_act` transfer branch, ~173-180)
- Test: `agent/tests/test_external_agent.py` (create) and extend `agent/tests/test_agent_gateway_api.py`

**Interfaces:**
- Consumes: `GatewayHTTP.act(op, params)`.
- Produces: `_idem_key(op, params) -> str` (stable sha1-based hex); `GatewayHTTP.act` now includes `params["idempotency_key"]` if the caller set one; `gw_act` uses `p.get("idempotency_key") or uuid4().hex`.

- [ ] **Step 1: Write the failing tests**

```python
# agent/tests/test_external_agent.py
from agent.external_agent.agent import ExternalAgent, _idem_key


class _RecordingGW:
    def __init__(self): self.acts = []
    def act(self, op, params): self.acts.append((op, dict(params))); return {"decision": "allow"}
    def message(self, msg): return {"answer": "ok", "trace": []}


def test_idem_key_is_stable_for_same_op_and_params():
    a = _idem_key("transfer_out", {"amount": "50"})
    b = _idem_key("transfer_out", {"amount": "50"})
    c = _idem_key("transfer_out", {"amount": "60"})
    assert a == b and a != c


def test_run_attaches_stable_idempotency_key_to_act_steps():
    gw = _RecordingGW()
    agent = ExternalAgent.from_plan([("act", "transfer_out", {"amount": "50"})], gw)
    agent.run("pay the bill")
    assert gw.acts[0][1]["idempotency_key"] == _idem_key("transfer_out", {"amount": "50"})
```

Extend the gateway API test so the supplied key reaches the bank client:

```python
# append to agent/tests/test_agent_gateway_api.py
def test_act_transfer_uses_supplied_idempotency_key():
    fc = FakeClient()
    # capture the idem key the gateway hands the bank client
    fc.keys = []
    orig = fc.agent_transfer
    def _cap(token, to, amount, desc, idem):
        fc.keys.append(idem); return orig(token, to, amount, desc, idem)
    fc.agent_transfer = _cap
    s = Settings.from_env(_ENV)
    c = TestClient(create_app(s, mandate_client=fc, mandate_pep=FakePEP(True)))
    c.post("/agent-gateway/act", headers={"Authorization": "Bearer gw"},
           json={"operation": "transfer_out", "params": {"amount": "50", "idempotency_key": "K1"}})
    assert fc.keys[-1] == "K1"
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `python -m pytest agent/tests/test_external_agent.py agent/tests/test_agent_gateway_api.py::test_act_transfer_uses_supplied_idempotency_key -q`
Expected: FAIL — `ImportError: cannot import name '_idem_key'`; and the gateway test fails because `gw_act` ignores the supplied key.

- [ ] **Step 3a: Implement the external-agent side**

In `agent/external_agent/agent.py`, add the helper and thread the key. Change `GatewayHTTP.act` to pass through a caller-set key, and `ExternalAgent.run` to stamp one per act step:

```python
import hashlib  # add to imports


def _idem_key(op: str, params: dict) -> str:
    """Stable per-(op,params) key so a re-run of the same instruction dedupes
    at the bank instead of double-paying."""
    payload = json.dumps({"op": op, "params": params}, sort_keys=True, default=str)
    return hashlib.sha1(payload.encode()).hexdigest()
```

```python
    def act(self, op, params):
        return self.http.post(f"{self.base}/agent-gateway/act", headers=self.h,
                              json={"operation": op, "params": params}).json()
```

(unchanged — `act` already forwards whatever is in `params`; the key rides inside `params`.)

In `ExternalAgent.run`, stamp the key before calling `act`:

```python
    def run(self, instruction: str) -> list[dict]:
        events = [{"kind": "plan", "instruction": instruction}]
        for step in self._make_plan(instruction):
            if step[0] == "act":
                _, op, params = step
                params = {**params, "idempotency_key": _idem_key(op, params)}
                res = self.gw.act(op, params)
                events.append({"kind": "act", "operation": op, "params": params, "result": res})
            else:
                _, msg = step
                res = self.gw.message(msg)
                events.append({"kind": "message", "text": msg, "answer": res.get("answer"),
                               "trace": res.get("trace")})
        events.append({"kind": "result", "steps": len(events) - 1})
        return events
```

- [ ] **Step 3b: Implement the gateway side**

In `agent/api.py` `gw_act`, use the supplied key (fall back to a mint for direct callers):

```python
        if op == "transfer_out":
            import uuid as _u
            to_acct = p.get("to_account_id") or _gw["biller"]
            idem = p.get("idempotency_key") or _u.uuid4().hex
            tok = client.mint_token(_gw["mandate_id"])
            code, res = client.agent_transfer(tok, to_acct, p["amount"],
                                              p.get("description", "Epcor utilities bill"),
                                              idem)
            return {"decision": "allow", "operation": op, "http": code, "result": res}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `python -m pytest agent/tests/test_external_agent.py agent/tests/test_agent_gateway_api.py -q`
Expected: PASS (all gateway tests including the new one).

- [ ] **Step 5: Commit**

```bash
git add agent/external_agent/agent.py agent/api.py agent/tests/test_external_agent.py agent/tests/test_agent_gateway_api.py
git commit -m "fix(agent): stable gateway idempotency key per act step

Review #3: gw_act minted a fresh uuid4 per call, so a re-run/retry was a new
payment the bank couldn't dedupe. Derive a deterministic key from op+params in
the external agent, thread it through the gateway, and honor it in gw_act.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: Surface the bank's 202 parked approval as `pending_approval` (#4)

Over the `daily_cap`, the agent plane returns **202** with a pending approval (the `MandatePEP` pre-checks `max_per_tx` but not `daily_cap`, so the breach reaches the bank). `gw_act` currently collapses 201/202 into `decision:"allow"`, so a parked payment reads as done and `ExternalAgent.run` records it as complete. Branch on `code == 202` → `decision:"pending_approval"` with the `approval_id`; render it distinctly in the demo.

**Files:**
- Modify: `agent/api.py` (`gw_act` transfer branch return, ~180)
- Modify: `demos/04-external-agent/app.py` (act rendering, ~107-113)
- Test: extend `agent/tests/test_agent_gateway_api.py`

**Interfaces:**
- Consumes: `client.agent_transfer(...) -> (status_code, body)`.
- Produces: `gw_act` returns `{"decision": "pending_approval", "operation", "approval_id", "http": 202, "result": body}` when the bank parks; still `{"decision":"allow", ...}` on 201.

- [ ] **Step 1: Write the failing test**

```python
# append to agent/tests/test_agent_gateway_api.py
class _ParkingClient(FakeClient):
    def agent_transfer(self, token, to, amount, desc, idem):
        return 202, {"approval_id": "AP1", "status": "pending"}


def test_act_transfer_over_cap_is_pending_approval():
    s = Settings.from_env(_ENV)
    c = TestClient(create_app(s, mandate_client=_ParkingClient(), mandate_pep=FakePEP(True)))
    r = c.post("/agent-gateway/act", headers={"Authorization": "Bearer gw"},
               json={"operation": "transfer_out", "params": {"amount": "100"}})
    body = r.json()
    assert body["decision"] == "pending_approval" and body["approval_id"] == "AP1"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest agent/tests/test_agent_gateway_api.py::test_act_transfer_over_cap_is_pending_approval -q`
Expected: FAIL — `decision` is `"allow"`, no `approval_id`.

- [ ] **Step 3: Implement the gateway branch**

Replace the `transfer_out` return in `gw_act`:

```python
        if op == "transfer_out":
            import uuid as _u
            to_acct = p.get("to_account_id") or _gw["biller"]
            idem = p.get("idempotency_key") or _u.uuid4().hex
            tok = client.mint_token(_gw["mandate_id"])
            code, res = client.agent_transfer(tok, to_acct, p["amount"],
                                              p.get("description", "Epcor utilities bill"),
                                              idem)
            if code == 202:
                return {"decision": "pending_approval", "operation": op, "http": code,
                        "approval_id": (res or {}).get("approval_id"), "result": res}
            return {"decision": "allow", "operation": op, "http": code, "result": res}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest agent/tests/test_agent_gateway_api.py -q`
Expected: PASS (all gateway tests).

- [ ] **Step 5: Render `pending_approval` in the demo**

In `demos/04-external-agent/app.py`, update the act branch so a parked payment shows amber, not green:

```python
        elif e["kind"] == "act":
            res = e.get("result", {})
            dec = res.get("decision", "?")
            _bubble("left", f"🤖 **Agent → act**", f"`{e['operation']}` {e.get('params', {})}")
            tone = {"allow": "allow", "deny": "deny"}.get(dec, "neutral")
            if dec == "pending_approval":
                detail = (f"⏸ over the daily cap — parked for the customer to approve "
                          f"(approval `{str(res.get('approval_id'))[:8]}`). Not paid yet.")
            else:
                detail = res.get("reason") or (res.get("result") if dec == "allow" else res)
            label = {"allow": "allow", "deny": "deny"}.get(dec, "pending")
            _bubble("right", f"🏦 **Gateway** · mandate check → **{label}**", f"{detail}", tone=tone)
```

- [ ] **Step 6: Commit**

```bash
git add agent/api.py demos/04-external-agent/app.py agent/tests/test_agent_gateway_api.py
git commit -m "fix(agent): surface 202 parked approval as pending_approval

Review #4: an over-daily-cap transfer parks as a 202 pending approval, but
gw_act reported it as allow and the demo showed success. Branch on 202 →
pending_approval with the approval_id; render it as 'parked, not paid'.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: TTL the customer-token cache (#6, robustness)

`SeedTokenResolver` caches a customer's login token forever, but the bank's customer JWT expires in 15 min (`expires_in = 900`). Every token-using path (manager tools, confirm, gateway `open_account`/`register_payee`/`message`) 401s a quarter-hour after boot until pod restart. Add a TTL with a safety margin and re-login on expiry; add seams so it's testable without a live bank.

**Files:**
- Modify: `agent/api.py` (`SeedTokenResolver`, ~lines 40-56)
- Test: `agent/tests/test_token_resolver.py` (create)

**Interfaces:**
- Consumes: `Settings.nano_bank_api`.
- Produces: `SeedTokenResolver(settings, creds, *, ttl_seconds=600, now=<callable>, login=<callable>)`; `resolve(customer_id)` re-logins once the cached token is older than `ttl_seconds`.

- [ ] **Step 1: Write the failing test**

```python
# agent/tests/test_token_resolver.py
from agent.api import SeedTokenResolver


class _S:
    nano_bank_api = "http://bank"


def test_resolver_relogins_after_ttl():
    calls = []
    clock = {"t": 1000.0}
    def fake_login(base, cred): calls.append(cred); return f"tok{len(calls)}"
    r = SeedTokenResolver(_S(), {"C1": ("e@x.ca", "pw")}, ttl_seconds=600,
                          now=lambda: clock["t"], login=fake_login)
    assert r.resolve("C1") == "tok1"      # first login
    clock["t"] += 300
    assert r.resolve("C1") == "tok1"      # still fresh → cached
    clock["t"] += 400                     # now 700s > ttl 600
    assert r.resolve("C1") == "tok2"      # re-login
    assert len(calls) == 2


def test_resolver_unknown_customer_is_none():
    r = SeedTokenResolver(_S(), {}, login=lambda *a: "x")
    assert r.resolve("nope") is None
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest agent/tests/test_token_resolver.py -q`
Expected: FAIL — current `__init__` takes no `ttl_seconds/now/login`; TypeError.

- [ ] **Step 3: Implement TTL + seams**

Replace `SeedTokenResolver` in `agent/api.py`:

```python
import time


def _default_login(base, cred):
    from .bank import BankClient
    return BankClient(base).login(*cred)


class SeedTokenResolver:
    """Phase-1 resolver: logs into nano-bank with seeded creds (customer_id -> creds).

    Customer JWTs expire in 15 min; cache each token with a TTL (default 10 min,
    inside the 15-min window) and re-login on expiry so long-running demos don't
    start 401ing a quarter-hour after boot.
    """
    def __init__(self, settings: Settings, creds: dict, *, ttl_seconds: int = 600,
                 now=time.monotonic, login=_default_login):
        self.settings = settings
        self.creds = creds  # customer_id -> (email, password)
        self.ttl = ttl_seconds
        self._now = now
        self._login = login
        self._cache: dict = {}  # customer_id -> (token, expires_at)

    def resolve(self, customer_id: str):
        cred = self.creds.get(customer_id)
        if not cred:
            return None
        hit = self._cache.get(customer_id)
        now = self._now()
        if hit and hit[1] > now:
            return hit[0]
        tok = self._login(self.settings.nano_bank_api, cred)
        self._cache[customer_id] = (tok, now + self.ttl)
        return tok
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest agent/tests/test_token_resolver.py -q`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add agent/api.py agent/tests/test_token_resolver.py
git commit -m "fix(agent): TTL the seed token cache, re-login on expiry

Review #6: customer JWTs expire in 15 min but the resolver cached forever, so
token-using paths 401ed until pod restart. Cache with a 10-min TTL and
re-login; add login/clock seams for testing.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 6: interac_recipients — partial unique index, migration self-heal, file renumber (#5)

Three sub-issues on the saved-payees table: (a) table-level `UNIQUE(customer_id, email)` + soft-delete (`status='removed'`) means register→remove→register the same email is a permanent 409 on the dead row; (b) the table isn't in the `database.rs` migration self-heal, so DBs that predate this table never get it; (c) the file shares the `11_` prefix with `11_agents.sql`. Fix: a **partial** unique index on active rows only, add self-heal (including dropping the old constraint on existing DBs), and renumber to `12_`.

**Files:**
- Modify: `src/core/tables/11_interac_recipients.sql` → rename to `src/core/tables/12_interac_recipients.sql`; change the constraint
- Modify: `api/src/config/database.rs` (`run_migrations` statement list, after the `pending_approvals` block ~line 160+)
- Verify: `cargo build` + live register→remove→register

**Interfaces:**
- Consumes: existing `interac_recipients` handlers (`api/src/handlers/interac_payees.rs`) — unchanged; a plain INSERT now succeeds after a soft-delete because only active rows are unique.
- Produces: partial unique index `uq_interac_recipients_active`; migration statements that create the table + index and drop the legacy constraint idempotently.

- [ ] **Step 1: Renumber and fix the DDL file**

```bash
git mv src/core/tables/11_interac_recipients.sql src/core/tables/12_interac_recipients.sql
```

Edit `src/core/tables/12_interac_recipients.sql`: drop the inline `UNIQUE (customer_id, email)` and add a partial unique index (unique among **active** rows only):

```sql
-- Sender-side Interac saved payees (address book) registered per customer.
-- Distinct from the Interac rail's `interac_handles` (recipient-side autodeposit
-- registrations): this is a convenience list of recipients a customer sends to.
-- Sending money still goes through the rail (POST /api/v1/interac/etransfers).
CREATE TABLE IF NOT EXISTS interac_recipients (
    recipient_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    customer_id  UUID NOT NULL REFERENCES customers(customer_id) ON DELETE CASCADE,
    email        TEXT NOT NULL,
    display_name TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'active',   -- active | removed
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_interac_recipients_customer
    ON interac_recipients(customer_id);

-- Unique only among ACTIVE rows, so remove (soft-delete) → re-register the same
-- email is allowed; a stale 'removed' row no longer blocks re-registration.
CREATE UNIQUE INDEX IF NOT EXISTS uq_interac_recipients_active
    ON interac_recipients(customer_id, email) WHERE status = 'active';
```

- [ ] **Step 2: Add the migration self-heal**

In `api/src/config/database.rs`, in the same `run_migrations` statement list, after the `pending_approvals` block, add these statements (idempotent — safe on fresh and existing DBs):

```rust
        // Saved Interac payees (address book). Self-heal for DBs predating the
        // 12_interac_recipients DDL, and migrate the old table-level UNIQUE to a
        // partial unique index so soft-deleted rows don't block re-registration.
        r#"
        CREATE TABLE IF NOT EXISTS interac_recipients (
            recipient_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            customer_id  UUID NOT NULL REFERENCES customers(customer_id) ON DELETE CASCADE,
            email        TEXT NOT NULL,
            display_name TEXT NOT NULL,
            status       TEXT NOT NULL DEFAULT 'active',
            created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
        )
        "#,
        "CREATE INDEX IF NOT EXISTS idx_interac_recipients_customer \
         ON interac_recipients(customer_id)",
        "ALTER TABLE interac_recipients \
         DROP CONSTRAINT IF EXISTS interac_recipients_customer_id_email_key",
        "CREATE UNIQUE INDEX IF NOT EXISTS uq_interac_recipients_active \
         ON interac_recipients(customer_id, email) WHERE status = 'active'",
```

- [ ] **Step 3: Build**

Run: `cd /home/bmartins/dev/nano-bank/api && cargo build 2>&1 | tail -5`
Expected: `Finished` (pre-existing dead-code warnings are fine; no errors).

- [ ] **Step 4: Commit**

```bash
cd /home/bmartins/dev/nano-bank
git add src/core/tables/12_interac_recipients.sql api/src/config/database.rs
git commit -m "fix(bank): interac_recipients partial unique index + self-heal + renumber

Review #5: table-level UNIQUE(customer_id,email) + soft-delete made
register->remove->register a permanent 409; the table wasn't in the migration
self-heal; and it shared the 11_ prefix with 11_agents.sql. Partial unique
index on active rows, add self-heal (drop legacy constraint), renumber to 12_.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 7: Deploy, live-verify, and final suite

Rebuild the two affected images, redeploy, and prove the fixes on the live cluster (unit-green ≠ deployed). The `bank-api` change needs the DDL init Job re-run (fresh table) OR relies on the self-heal on the existing DB — verify the register→remove→register loop either way.

**Files:** none (build/deploy/verify only).

- [ ] **Step 1: Full offline suite**

Run: `cd /home/bmartins/dev/nano-bank && python -m pytest agent -q`
Expected: all pass (≥ 74 now, was 70 + the new tests), 1 skipped.

- [ ] **Step 2: Rebuild + load + roll the affected images**

```bash
export XDG_RUNTIME_DIR=/run/user/1000 XDG_DATA_HOME=/home/bmartins/.local/share
cd /home/bmartins/dev/nano-bank
# bank-api (Rust/SQL — Task 6)
docker build -t nano-bank-api:dev -f api/Dockerfile api
kind load docker-image nano-bank-api:dev --name nano-bank
# agent-api (Python — Tasks 1-5)
docker build -t nano-agent-api:dev -f agent/Dockerfile.api agent
kind load docker-image nano-agent-api:dev --name nano-bank
kubectl --context kind-nano-bank -n nano-bank rollout restart deploy/bank-api deploy/agent-api
kubectl --context kind-nano-bank -n nano-bank rollout status deploy/bank-api deploy/agent-api --timeout=120s
```

Expected: both deployments `successfully rolled out`.

- [ ] **Step 3: Port-forward the branch + bank**

```bash
setsid bash -c 'exec kubectl --context kind-nano-bank -n nano-bank port-forward svc/agent-api 8086:8086' >/tmp/pf-8086.log 2>&1 </dev/null & disown
setsid bash -c 'exec kubectl --context kind-nano-bank -n nano-bank port-forward svc/bank-api 8081:8081' >/tmp/pf-8081.log 2>&1 </dev/null & disown
sleep 3; curl -sf http://localhost:8086/health && curl -sf http://localhost:8081/health
```

Expected: both return healthy JSON.

- [ ] **Step 4: Live-verify the mandate-payee pin (#2) + pending approval (#4)**

```bash
GW=http://localhost:8086/agent-gateway; H='Authorization: Bearer '"$(grep -E '^AGENT_GATEWAY_TOKEN=' agent/.env | cut -d= -f2-)"
curl -s -X POST "$GW/demo-seed" -H "$H" | tee /tmp/seed.json          # seeds Ada + pinned Epcor mandate
# in-cap Epcor bill-pay → allow
curl -s -X POST "$GW/act" -H "$H" -d '{"operation":"transfer_out","params":{"amount":"50"}}'   # decision:allow, http:201
# arbitrary payee (prompt-injection) → bank denies PAYEE_NOT_ALLOWED
curl -s -X POST "$GW/act" -H "$H" -d '{"operation":"transfer_out","params":{"amount":"50","to_account_id":"00000000-0000-0000-0000-000000000000"}}'  # decision:deny (PAYEE_NOT_ALLOWED)
# push over the $500 daily cap with repeated in-cap sends → eventually 202 pending_approval
for i in 1 2 3 4 5; do curl -s -X POST "$GW/act" -H "$H" -d '{"operation":"transfer_out","params":{"amount":"100"}}' | python -c "import sys,json;print(json.load(sys.stdin)['decision'])"; done
```

Expected: first `allow`; arbitrary payee `deny`; the run of $100 sends flips to `pending_approval` once the daily cap is crossed.

- [ ] **Step 5: Live-verify the interac_recipients re-register loop (#5)**

```bash
# log in as the seeded Ada (creds are in-process; use the manager path or seed output)
CID=$(python -c "import json;print(json.load(open('/tmp/seed.json'))['customer_id'])")
# register -> remove -> register the SAME email via the branch's customer path
# (uses the resolver's cached customer token)
curl -s -X POST http://localhost:8086/branch/clients/$CID/message -H "Authorization: Bearer $(grep -E '^BRANCH_SERVICE_TOKEN=' agent/.env | cut -d= -f2-)" \
  -d '{"message":"add interac payee bob@x.ca named Bob, then remove him, then add bob@x.ca named Bob again"}'
```

Expected: the final add succeeds (no permanent 409) — the partial index allows re-registration after soft-delete. (If the manager phrasing is unreliable, verify directly against the bank REST with Ada's token: `POST /api/v1/customers/interac-recipients` → `DELETE .../{id}` → `POST` same email → 201.)

- [ ] **Step 6: Final suite + commit the verification note**

Run: `python -m pytest agent -q`
Then append a one-line live-verified note to the plan and commit:

```bash
git add docs/superpowers/plans/2026-07-13-pr22-review-fixes.md
git commit -m "docs: mark PR #22 review fixes live-verified (allow/deny/pending/re-register)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
git push
```

- [ ] **Step 7: Reply to the reviewer**

Post a PR comment summarizing what changed per finding (#1-#6 fixed, #7 left as a deliberate design decision), referencing the commits. Reply in the review thread, not a fresh top-level comment where a thread exists.

---

## Self-Review

**Spec coverage:** Findings #1 (Task 2), #2 (Task 1), #3 (Task 3), #4 (Task 4), #5 (Task 6), #6 (Task 5) each have a task; #7 explicitly out of scope per the user. Deploy/verify is Task 7. ✅

**Placeholder scan:** All steps carry concrete code/commands and expected output. The one soft spot is Task 7 Step 5's reliance on manager phrasing — mitigated with a direct-REST fallback in the same step. ✅

**Type consistency:** `_idem_key(op, params)` defined in Task 3 and used only there; `gw_act` returns `decision` ∈ {allow, deny, pending_approval}; `SeedTokenResolver(...)` seam names (`ttl_seconds`, `now`, `login`) match between Task 5 test and impl; `agent_transfer -> (code, body)` tuple consistent across Tasks 3/4 and the existing FakeClient. ✅

---

## Live-verify record (2026-07-14)

Rebuilt & kind-loaded both images (`nano-bank-api:dev`, `nano-agent-api:dev`),
`rollout restart deploy/bank-api deploy/agent-api`, port-forwarded `svc/agent-api
:8086` + `svc/bank-api :8081`. Against the deployed pods:

- **#1** `$50` `transfer_out` → `decision:allow http:201` (idempotency in body; transfer posts).
- **#2** `transfer_out` to a foreign `to_account_id` → `decision:deny http:403 reason:PAYEE_NOT_ALLOWED`
  (allowed_payees pinned to the seeded Epcor biller; enforced at the bank).
- **#4** `$50` + five `$100` (cap `$500`) → 4 allow, 5th → `decision:pending_approval http:202`
  with `approval_id` (parked, money not moved).
- **#5** customer register → remove (soft-delete) → register **same** email → 2nd register 201, no 409
  (partial unique index on `status='active'`).
- **Follow-up (found in live-verify):** `gw_act` collapsed every non-202 into `decision:allow`, so the
  #2 deny read as `allow http:403`. Fixed to map `code>=400` → `decision:deny` with the bank's reason
  (commit `04a1dff`, test `test_act_transfer_bank_403_is_deny_not_allow`).

#3 (stable idempotency key) and #6 (10-min token TTL) are unit-verified — internal / time-based,
impractical to exercise live. `agent/.venv/bin/python -m pytest agent -q` → **79 passed, 1 skipped**.

**#7 (A2A scope granularity) remains out of scope** — a deliberate design decision, flagged to the reviewer.
