# External Mandated Agent — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An autonomous external LLM agent operates a customer's bank **only through the agentic branch**, under a customer-granted **mandate** (scoped, capped, revocable) — performing a transfer-out and asking the manager about savings, with no direct path to the bank.

**Architecture:** Reuse #19's mandate system (bank = consent record + revocation source). The **branch** (`agent-api`) becomes the agent's single door and policy-enforcement point: it holds the bank agent credentials, re-reads the live mandate per request, enforces scope/cap, and dispatches — transfers to the bank agent-plane (bank-enforced cap), open-account/payee to the customer REST (branch-enforced new scopes), and A2A messages to the shared personal manager. The autonomous agent holds only a branch gateway token — no bank URL, no bank creds.

**Tech Stack:** Rust (small scope-const change), Python 3.12 (branch + agent), FastAPI, LangGraph/glm-5.2, pytest, Streamlit, Docker/kind/kubectl.

## Global Constraints

- **The agent has no bank route or creds** — only the branch URL + `AGENT_GATEWAY_TOKEN`. The branch holds `NANO_AGENT_ID`/`NANO_AGENT_SECRET`. (Path A single-door.)
- **Mandate is the consent for the agent path** — no interactive confirm-gate; the branch enforces caps/scope and re-reads the live mandate each call (revocation immediate).
- **Bank = source of truth** for mandate existence/status/scopes/cap; the branch is the PEP for manager-routed ops; the bank agent-plane is the PEP for transfers.
- **Money movement stays real:** transfer-out goes through the bank agent-plane `POST /api/v1/agent/transfers` (cap-checked under the mandate row lock).
- **Branch:** `agent-k8s-e2e` (PR #22, leveling). Agent venv `agent/.venv`. kubectl context `kind-nano-bank`.

## Reference — #19 mandate API (exact shapes)

- `POST /api/v1/agents` `{display_name, description?}` → `{agent_id, agent_secret, ...}` (secret once).
- `POST /api/v1/mandates` (customer JWT) `{agent_id, account_id, scopes[], max_per_tx?, daily_cap?, allowed_payees?, expires_at}` → MandateResponse.
- `DELETE /api/v1/mandates/{id}` (customer JWT) → revoke.
- `POST /api/v1/auth/agent-mandates` `{agent_id, agent_secret}` → `[{mandate_id, account_id, account_type, account_last4, scopes[], max_per_tx, daily_cap, daily_used, expires_at}]` (live; a revoked mandate drops out).
- `POST /api/v1/auth/agent-token` `{agent_id, agent_secret, mandate_id}` → `{access_token}` (5-min pointer).
- `POST /api/v1/agent/transfers` (agent token) `{to_account_id, amount, description, idempotency_key}` → transfer (202 + approval on over-cap).
- Scopes today: `read:balance`, `read:transactions`, `transfer:initiate` (validated against `KNOWN_SCOPES`).

## File Structure

- Modify `api/src/models/agent.rs` — add `account:open`, `payee:register` to `KNOWN_SCOPES`.
- Create `agent/mandate_gateway.py` — `MandateClient` + `MandatePEP`.
- Modify `agent/config.py` — agent creds + gateway token.
- Modify `agent/api.py` — `/agent-gateway/*` endpoints.
- Create `agent/external_agent/__init__.py`, `agent/external_agent/agent.py` — the autonomous agent.
- Modify `agent/seed.py` (or add helper) — register agent + grant mandate.
- Create `k8s/networkpolicy.yaml` + `agent/k8s/networkpolicy.yaml`.
- Create `demos/04-external-agent/{app.py,requirements.txt}`; update `demos/README.md`.
- Tests: `agent/tests/test_mandate_gateway.py`, `test_agent_gateway_api.py`, `test_external_agent.py`.

---

## Task 1: Add branch-enforced scopes to the bank (Rust)

**Files:** Modify `api/src/models/agent.rs`.

**Interfaces:** Produces two new accepted mandate scopes `account:open`, `payee:register` (stored by the bank, enforced by the branch; the bank has no agent-plane endpoint for them).

- [ ] **Step 1: Extend the scope constants + KNOWN_SCOPES**

In `api/src/models/agent.rs`:
```rust
pub const SCOPE_READ_BALANCE: &str = "read:balance";
pub const SCOPE_READ_TRANSACTIONS: &str = "read:transactions";
pub const SCOPE_TRANSFER_INITIATE: &str = "transfer:initiate";
// Branch-enforced scopes: the bank stores them on the mandate but exposes no
// agent-plane endpoint; the agentic branch checks them before routing the
// operation to the customer REST via the personal manager.
pub const SCOPE_ACCOUNT_OPEN: &str = "account:open";
pub const SCOPE_PAYEE_REGISTER: &str = "payee:register";
pub const KNOWN_SCOPES: [&str; 5] = [
    SCOPE_READ_BALANCE,
    SCOPE_READ_TRANSACTIONS,
    SCOPE_TRANSFER_INITIATE,
    SCOPE_ACCOUNT_OPEN,
    SCOPE_PAYEE_REGISTER,
];
```
(Update the array length `[&str; 3]` → `[&str; 5]`.)

- [ ] **Step 2: Compile**

```bash
cd /home/bmartins/dev/nano-bank/api && cargo build 2>&1 | tail -2
```
Expected: builds (pre-existing warnings only).

- [ ] **Step 3: Commit**

```bash
cd /home/bmartins/dev/nano-bank
git add api/src/models/agent.rs
git commit -m "feat(api): add branch-enforced mandate scopes account:open, payee:register

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: `MandateClient` + `MandatePEP` (branch → bank)

**Files:** Create `agent/mandate_gateway.py`; Test `agent/tests/test_mandate_gateway.py`.

**Interfaces:**
- `MandateClient(base_url, agent_id, agent_secret, http?)`: `list_mandates() -> list[dict]`, `mint_token(mandate_id) -> str`, `agent_transfer(token, to_account_id, amount, description, idempotency_key) -> (status, dict)`, `register_agent(display_name) -> dict`, `create_mandate(customer_token, payload) -> dict`, `revoke(customer_token, mandate_id) -> None`.
- `MandatePEP(client)`: `check(mandate_id, scope, amount=None) -> Decision` where `Decision(allowed: bool, mandate: dict|None, reason: str)`. Re-reads live mandates; denies if the mandate is absent (revoked/expired), the scope is missing, or `amount > max_per_tx`.

- [ ] **Step 1: Write failing tests**

```python
# agent/tests/test_mandate_gateway.py
from decimal import Decimal
from agent.mandate_gateway import MandateClient, MandatePEP


class FakeClient:
    def __init__(self, mandates): self._m = mandates
    def list_mandates(self): return self._m


def _m(scopes, cap="100"):
    return {"mandate_id": "M1", "account_id": "A1", "scopes": scopes,
            "max_per_tx": cap, "daily_cap": None, "daily_used": "0",
            "account_last4": "1234", "account_type": "chequing"}


def test_allows_in_scope_under_cap():
    pep = MandatePEP(FakeClient([_m(["transfer:initiate"], cap="100")]))
    d = pep.check("M1", "transfer:initiate", amount=Decimal("50"))
    assert d.allowed and d.mandate["account_id"] == "A1"


def test_denies_missing_scope():
    pep = MandatePEP(FakeClient([_m(["read:balance"])]))
    assert not pep.check("M1", "account:open").allowed


def test_denies_over_cap():
    pep = MandatePEP(FakeClient([_m(["transfer:initiate"], cap="40")]))
    d = pep.check("M1", "transfer:initiate", amount=Decimal("50"))
    assert not d.allowed and "cap" in d.reason.lower()


def test_denies_revoked_absent_mandate():
    pep = MandatePEP(FakeClient([]))   # revoked → not in the live list
    d = pep.check("M1", "read:balance")
    assert not d.allowed and ("revoked" in d.reason.lower() or "not" in d.reason.lower())
```

- [ ] **Step 2: Run to verify fail**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_mandate_gateway.py -q`
Expected: FAIL (`ModuleNotFoundError`).

- [ ] **Step 3: Implement**

```python
# agent/mandate_gateway.py
from __future__ import annotations
from dataclasses import dataclass
from decimal import Decimal
from typing import Optional
import httpx


class GatewayError(Exception):
    def __init__(self, status: int, body): super().__init__(f"{status}: {body}"); self.status = status


@dataclass
class Decision:
    allowed: bool
    mandate: Optional[dict]
    reason: str


class MandateClient:
    """Branch-side client for the bank's mandate + agent plane. Holds the agent
    credentials; the external agent never sees these."""

    def __init__(self, base_url: str, agent_id: str, agent_secret: str,
                 http: Optional[httpx.Client] = None):
        self.base = base_url.rstrip("/")
        self.agent_id = agent_id
        self.agent_secret = agent_secret
        self.http = http or httpx.Client(timeout=30)

    def _json(self, r):
        if r.status_code // 100 != 2:
            raise GatewayError(r.status_code, _safe(r))
        return _safe(r)

    def list_mandates(self) -> list:
        r = self.http.post(f"{self.base}/api/v1/auth/agent-mandates",
                           json={"agent_id": self.agent_id, "agent_secret": self.agent_secret})
        return self._json(r)

    def mint_token(self, mandate_id: str) -> str:
        r = self.http.post(f"{self.base}/api/v1/auth/agent-token",
                           json={"agent_id": self.agent_id, "agent_secret": self.agent_secret,
                                 "mandate_id": mandate_id})
        return self._json(r)["access_token"]

    def agent_transfer(self, token, to_account_id, amount, description, idempotency_key):
        r = self.http.post(f"{self.base}/api/v1/agent/transfers",
                           headers={"Authorization": f"Bearer {token}"},
                           json={"to_account_id": to_account_id, "amount": str(amount),
                                 "description": description, "idempotency_key": idempotency_key})
        return r.status_code, _safe(r)

    def register_agent(self, display_name, description="external demo agent") -> dict:
        r = self.http.post(f"{self.base}/api/v1/agents",
                           json={"display_name": display_name, "description": description})
        return self._json(r)

    def create_mandate(self, customer_token, payload: dict) -> dict:
        r = self.http.post(f"{self.base}/api/v1/mandates",
                           headers={"Authorization": f"Bearer {customer_token}"}, json=payload)
        return self._json(r)

    def revoke(self, customer_token, mandate_id) -> None:
        r = self.http.request("DELETE", f"{self.base}/api/v1/mandates/{mandate_id}",
                              headers={"Authorization": f"Bearer {customer_token}"})
        if r.status_code // 100 != 2:
            raise GatewayError(r.status_code, _safe(r))


class MandatePEP:
    """Re-reads the live mandate every check → immediate revocation."""

    def __init__(self, client):
        self.client = client

    def check(self, mandate_id: str, scope: str, amount: Optional[Decimal] = None) -> Decision:
        try:
            live = self.client.list_mandates()
        except Exception as e:  # noqa: BLE001
            return Decision(False, None, f"mandate lookup failed: {e}")
        m = next((x for x in live if x.get("mandate_id") == mandate_id), None)
        if m is None:
            return Decision(False, None, "mandate revoked or expired (not in live set)")
        if scope not in (m.get("scopes") or []):
            return Decision(False, m, f"scope '{scope}' not granted")
        if amount is not None and m.get("max_per_tx") is not None:
            if Decimal(str(amount)) > Decimal(str(m["max_per_tx"])):
                return Decision(False, m, f"amount exceeds per-tx cap {m['max_per_tx']}")
        return Decision(True, m, "ok")


def _safe(r):
    try:
        return r.json()
    except Exception:  # noqa: BLE001
        return {"raw": r.text}
```

- [ ] **Step 4: Run to verify pass**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_mandate_gateway.py -q`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add agent/mandate_gateway.py agent/tests/test_mandate_gateway.py
git commit -m "feat(agent): MandateClient + MandatePEP — live-mandate policy enforcement

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Branch `/agent-gateway/*` endpoints

**Files:** Modify `agent/config.py`, `agent/api.py`; Test `agent/tests/test_agent_gateway_api.py`.

**Interfaces:**
- Config gains `nano_agent_id`, `nano_agent_secret`, `agent_gateway_token`, plus the demo mandate binding (`agent_mandate_id`, `agent_customer_id`), all from env.
- Endpoints (bearer `AGENT_GATEWAY_TOKEN`):
  - `GET  /agent-gateway/mandate` → the live mandate summary (or 404 if none).
  - `POST /agent-gateway/act` `{operation, params}` → `{decision, result?, reason?}`;
    operations `transfer_out{to_account_id,amount}` (→ bank agent-plane),
    `open_account{account_type}` (→ customer REST), `register_payee{email,name}` (→ customer REST).
  - `POST /agent-gateway/message` `{message}` → `{answer, trace}` (manager A2A, mandate must be active).
  - `POST /agent-gateway/revoke` → `{revoked:true}` (customer revokes; demo button).

- [ ] **Step 1: Config**

Add to `agent/config.py` fields + `from_env` (mirror existing `g(...)`):
```python
    nano_agent_id: str
    nano_agent_secret: str
    agent_gateway_token: str
    agent_mandate_id: str
    agent_customer_id: str
```
```python
            nano_agent_id=g("NANO_AGENT_ID"),
            nano_agent_secret=g("NANO_AGENT_SECRET"),
            agent_gateway_token=g("AGENT_GATEWAY_TOKEN"),
            agent_mandate_id=g("AGENT_MANDATE_ID"),
            agent_customer_id=g("AGENT_CUSTOMER_ID"),
```

- [ ] **Step 2: Write the failing test**

```python
# agent/tests/test_agent_gateway_api.py
from fastapi.testclient import TestClient
from agent.config import Settings
from agent import api as apimod


def _app(monkeypatch, pep_allowed=True):
    s = Settings.from_env({"OLLAMA_API_KEY": "x", "AGENT_GATEWAY_TOKEN": "gw",
                           "AGENT_MANDATE_ID": "M1", "AGENT_CUSTOMER_ID": "C1",
                           "BRANCH_SERVICE_TOKEN": "svc"})
    # patch the PEP + client the app builds, plus the manager + bank calls
    return s


def test_act_denied_when_pep_denies(monkeypatch):
    # Build the app with a PEP stub that denies; assert /agent-gateway/act returns decision=deny
    ...
```
(Follow `agent/tests/test_api.py`'s existing app-construction pattern — inject stubs for the PEP/client/assist through `create_app` params added in Step 3; assert `deny` blocks the action and `allow` calls the right downstream.)

- [ ] **Step 3: Implement the endpoints**

In `agent/api.py`, extend `create_app(...)` to accept injectable `mandate_pep`, `mandate_client`, and reuse `assist_fn`/`token_resolver`. Add:
```python
    from .mandate_gateway import MandateClient, MandatePEP
    _mc = mandate_client or MandateClient(settings.nano_bank_api,
                                          settings.nano_agent_id, settings.nano_agent_secret)
    _pep = mandate_pep or MandatePEP(_mc)

    def _gw_auth(authorization):
        if authorization != f"Bearer {settings.agent_gateway_token}":
            raise HTTPException(401, "invalid agent gateway token")

    @app.get("/agent-gateway/mandate")
    def gw_mandate(authorization: str = Header(None)):
        _gw_auth(authorization)
        live = _mc.list_mandates()
        m = next((x for x in live if x["mandate_id"] == settings.agent_mandate_id), None)
        if not m:
            raise HTTPException(404, "no active mandate")
        return m

    @app.post("/agent-gateway/act")
    async def gw_act(body: dict, authorization: str = Header(None)):
        _gw_auth(authorization)
        op = body.get("operation"); p = body.get("params") or {}
        cid = settings.agent_customer_id
        scope = {"transfer_out": "transfer:initiate", "open_account": "account:open",
                 "register_payee": "payee:register"}.get(op)
        if scope is None:
            raise HTTPException(400, f"unknown operation {op}")
        amount = p.get("amount") if op == "transfer_out" else None
        d = _pep.check(settings.agent_mandate_id, scope, amount=amount)
        if not d.allowed:
            return {"decision": "deny", "reason": d.reason}
        if op == "transfer_out":
            tok = _mc.mint_token(settings.agent_mandate_id)
            import uuid as _u
            code, res = _mc.agent_transfer(tok, p["to_account_id"], p["amount"],
                                           p.get("description", "agent transfer"), _u.uuid4().hex)
            return {"decision": "allow", "result": res, "http": code}
        # open_account / register_payee → customer REST via the mandate's customer token
        from .bank import BankClient
        bank = BankClient(settings.nano_bank_api)
        ctok = _token(cid)
        if op == "open_account":
            return {"decision": "allow",
                    "result": bank.create_account(ctok, {"account_type": p["account_type"]})}
        return {"decision": "allow",
                "result": bank.register_recipient(ctok, p["email"], p["name"])}

    @app.post("/agent-gateway/message")
    async def gw_message(body: dict, authorization: str = Header(None)):
        _gw_auth(authorization)
        d = _pep.check(settings.agent_mandate_id, "read:balance")
        if not d.allowed:
            return {"answer": f"(denied) {d.reason}", "trace": []}
        return await assist_fn(settings, settings.agent_customer_id,
                               _token(settings.agent_customer_id), body["message"], None)

    @app.post("/agent-gateway/revoke")
    def gw_revoke(authorization: str = Header(None)):
        _gw_auth(authorization)
        _mc.revoke(_token(settings.agent_customer_id), settings.agent_mandate_id)
        return {"revoked": True}
```

- [ ] **Step 4: Run tests + full suite**

```bash
agent/.venv/bin/python -m pytest agent/tests/test_agent_gateway_api.py agent -q 2>&1 | tail -2
```
Expected: new gateway tests + prior suite green.

- [ ] **Step 5: Commit**

```bash
git add agent/api.py agent/config.py agent/tests/test_agent_gateway_api.py
git commit -m "feat(agent): /agent-gateway/* — mandate-gated single door (act/message/revoke)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: The autonomous external LLM agent

**Files:** Create `agent/external_agent/__init__.py`, `agent/external_agent/agent.py`; Test `agent/tests/test_external_agent.py`.

**Interfaces:**
- `ExternalAgent(gateway_base, gateway_token, llm=None)`: `run(instruction: str) -> list[dict]` — an event list `{kind: "plan"|"act"|"message"|"result", ...}`. Tools call ONLY the branch gateway. No bank creds.

- [ ] **Step 1: Write the failing test (scripted, no live LLM)**

```python
# agent/tests/test_external_agent.py
from agent.external_agent.agent import ExternalAgent


class FakeGW:
    def __init__(self): self.calls = []
    def act(self, op, params): self.calls.append((op, params)); return {"decision": "allow", "result": {}}
    def message(self, msg): self.calls.append(("message", msg)); return {"answer": "savings are good", "trace": []}


def test_planned_steps_call_the_gateway():
    gw = FakeGW()
    a = ExternalAgent.from_plan([("act", "transfer_out", {"to_account_id": "x", "amount": "50"}),
                                 ("message", "benefits of a savings account?")], gateway=gw)
    events = a.run("move 50 out and tell me about savings")
    ops = [c[0] for c in gw.calls]
    assert "transfer_out" in ops and "message" in ops
    assert any(e["kind"] == "result" for e in events)
```
(The `from_plan` constructor lets tests drive a deterministic plan; the live LLM path is exercised in Task 8.)

- [ ] **Step 2: Run to verify fail** — `pytest agent/tests/test_external_agent.py -q` → FAIL.

- [ ] **Step 3: Implement**

```python
# agent/external_agent/__init__.py
from .agent import ExternalAgent  # noqa: F401
```
```python
# agent/external_agent/agent.py
"""Autonomous external agent — talks ONLY to the branch gateway (no bank creds)."""
from __future__ import annotations
import json
from typing import Optional
import httpx


class GatewayHTTP:
    def __init__(self, base, token, http=None):
        self.base = base.rstrip("/"); self.h = {"Authorization": f"Bearer {token}"}
        self.http = http or httpx.Client(timeout=180)
    def mandate(self): return self.http.get(f"{self.base}/agent-gateway/mandate", headers=self.h).json()
    def act(self, op, params):
        return self.http.post(f"{self.base}/agent-gateway/act", headers=self.h,
                              json={"operation": op, "params": params}).json()
    def message(self, msg):
        return self.http.post(f"{self.base}/agent-gateway/message", headers=self.h,
                             json={"message": msg}).json()


PLANNER_SYS = (
    "You are an autonomous banking agent operating under a mandate through a gateway. "
    "Given the user's high-level instruction, output a JSON list of steps. Each step is "
    '{"kind":"act","operation":"transfer_out|open_account|register_payee","params":{...}} '
    'or {"kind":"message","text":"..."} to ask the manager. Only use granted capabilities.'
)


class ExternalAgent:
    def __init__(self, gateway, llm=None, plan=None):
        self.gw = gateway
        self.llm = llm
        self._plan = plan

    @classmethod
    def from_plan(cls, plan, gateway):
        return cls(gateway=gateway, plan=plan)

    @classmethod
    def http(cls, gateway_base, gateway_token, llm=None):
        return cls(gateway=GatewayHTTP(gateway_base, gateway_token), llm=llm)

    def _make_plan(self, instruction):
        if self._plan is not None:
            return self._plan
        from langchain_core.messages import SystemMessage, HumanMessage
        out = self.llm.invoke([SystemMessage(PLANNER_SYS), HumanMessage(instruction)])
        steps = json.loads(out.content)
        norm = []
        for s in steps:
            if s.get("kind") == "act":
                norm.append(("act", s["operation"], s.get("params", {})))
            else:
                norm.append(("message", s.get("text", "")))
        return norm

    def run(self, instruction: str) -> list[dict]:
        events = [{"kind": "plan", "instruction": instruction}]
        for step in self._make_plan(instruction):
            if step[0] == "act":
                _, op, params = step
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

- [ ] **Step 4: Run to verify pass** — `pytest agent/tests/test_external_agent.py -q` → PASS.

- [ ] **Step 5: Commit**

```bash
git add agent/external_agent/ agent/tests/test_external_agent.py
git commit -m "feat(agent): autonomous external agent — plans + acts only via the branch gateway

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: NetworkPolicy manifests (defense-in-depth)

**Files:** Create `agent/k8s/networkpolicy.yaml`.

**Interfaces:** Restrict ingress to `bank-api` so only the `agent-api`/manager pods (and existing bank clients) may reach it; document the kindnet caveat.

- [ ] **Step 1: Write the manifest**

`agent/k8s/networkpolicy.yaml`:
```yaml
# Defense-in-depth: only agent-api (the branch) may reach bank-api's agent plane.
# NOTE: Kind's default CNI (kindnet) does NOT enforce NetworkPolicy — this is
# enforced only under a policy-capable CNI (e.g. Calico). The primary guarantee
# is app-level: the external agent holds no bank URL/creds (path A).
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: bank-api-ingress-lock
  namespace: nano-bank
spec:
  podSelector:
    matchLabels: { app: bank-api }
  policyTypes: [Ingress]
  ingress:
    - from:
        - podSelector: { matchLabels: { app: agent-api } }
        - podSelector: { matchLabels: { app: agent-mcp } }
      ports:
        - { protocol: TCP, port: 8081 }
```

- [ ] **Step 2: Apply (best-effort) + verify it's accepted**

```bash
kubectl --context kind-nano-bank -n nano-bank apply -f agent/k8s/networkpolicy.yaml
kubectl --context kind-nano-bank -n nano-bank get networkpolicy bank-api-ingress-lock
```
Expected: `networkpolicy.../bank-api-ingress-lock created`, and it lists (enforcement depends on the CNI — documented).

- [ ] **Step 3: Commit**

```bash
git add agent/k8s/networkpolicy.yaml
git commit -m "feat(k8s): NetworkPolicy locking bank-api ingress to the branch (kindnet caveat noted)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: Seed helper — register agent + grant mandate

**Files:** Modify `agent/seed.py` (add `seed_agent_mandate`) and expose via `agent/api.py` `POST /agent-gateway/demo-seed`.

**Interfaces:** `seed_agent_mandate(bank, customer_id, customer_token, account_id) -> {agent_id, agent_secret, mandate_id}` — registers an agent and grants a mandate (scopes: all 5; `max_per_tx` e.g. 100; expiry +1h). Returns the binding so the branch can be configured. Also seeds an **"Epcor Utilities" biller** (a synthetic customer + active chequing account) and returns its `epcor_account_id` — the destination for the agent's bill-payment transfer-out. Configured on the branch as `AGENT_BILLER_ACCOUNT_ID`.

- [ ] **Step 1: Implement the helper**

In `agent/seed.py`:
```python
def _seed_epcor_biller(client):
    """A stable 'Epcor Utilities' biller: a synthetic customer + active chequing
    account, the destination for the agent's mandate-capped bill payment."""
    import uuid
    tag = uuid.uuid4().hex[:8]
    cust = client.create_customer({
        "email": f"epcor.{tag}@biller.nano", "phone_number": f"+1555{uuid.uuid4().int % 10_000_000:07d}",
        "first_name": "Epcor", "last_name": "Utilities",
        "date_of_birth": "1990-01-01", "sin": f"{uuid.uuid4().int % 1_000_000_000:09d}",
        "password": "Biller!" + tag})
    tok = client.login(cust["email"], "Biller!" + tag)
    acct = client.create_account(tok, {"account_type": "chequing"})
    return acct["account_id"]


def seed_agent_mandate(client, customer_token, account_id):
    from datetime import datetime, timedelta, timezone
    from .mandate_gateway import MandateClient
    mc = MandateClient(client.base, "", "")
    agent = mc.register_agent("Demo External Agent")
    mandate = mc.create_mandate(customer_token, {
        "agent_id": agent["agent_id"], "account_id": account_id,
        "scopes": ["read:balance", "read:transactions", "transfer:initiate",
                   "account:open", "payee:register"],
        "max_per_tx": "100", "daily_cap": "500",
        "expires_at": (datetime.now(timezone.utc) + timedelta(hours=1)).isoformat()})
    epcor = _seed_epcor_biller(client)
    return {"agent_id": agent["agent_id"], "agent_secret": agent["agent_secret"],
            "mandate_id": mandate["mandate_id"], "epcor_account_id": epcor}
```

- [ ] **Step 2: Expose a seed endpoint** in `agent/api.py` (behind the gateway token) that seeds a customer+account (reuse `seed_fn`), calls `seed_agent_mandate`, and returns `{customer_id, account_id, agent_id, agent_secret, mandate_id}` so the operator can set the branch env. (Concrete wiring mirrors the existing `/branch/seed`.)

- [ ] **Step 3: Verify (offline import) + commit**

```bash
agent/.venv/bin/python -c "import agent.seed; print('seed import ok')"
git add agent/seed.py agent/api.py
git commit -m "feat(agent): seed helper — register external agent + grant its mandate

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: Demo 4 — external-agent console

**Files:** Create `demos/04-external-agent/{app.py,requirements.txt}`; update `demos/README.md`.

**Interfaces:** Streamlit → branch `/agent-gateway/*` (bearer `AGENT_GATEWAY_TOKEN`). Shows the mandate, a high-level instruction box, runs the autonomous agent, streams steps + the mandate gate, and a Revoke button.

- [ ] **Step 1: Write the app** (reuse the demo-3 left-right styling)

Key structure of `demos/04-external-agent/app.py`:
```python
import os, requests, streamlit as st
from agent.external_agent.agent import ExternalAgent, GatewayHTTP

BASE = os.environ.get("DEMO_BRANCH_BASE", "http://localhost:8086").rstrip("/")
TOKEN = os.environ.get("AGENT_GATEWAY_TOKEN", "")
gw = GatewayHTTP(BASE, TOKEN)

st.title("🛰️ nano-bank — external mandated agent")
# 1) show the mandate (account, scopes, cap, expiry) via GET /agent-gateway/mandate
# 2) high-level instruction box (pre-filled): "Move 50 out of my chequing and tell me
#    whether I should open a savings account."
# 3) on Run: ExternalAgent(gateway=gw, llm=<glm-5.2>).run(instruction) → stream events
#    left-right: agent step (left) / gateway+manager result (right); show allow/deny + trace.
# 4) Revoke button → POST /agent-gateway/revoke, then re-run shows the next act denied.
```
(Full app: render `mandate`; on Run build the LLM via `agent.model_factory` and call `ExternalAgent.http(BASE, TOKEN, llm=...).run(...)`; render each event with the demo-3 bubble helper; a `Revoke` button calls `requests.post(f"{BASE}/agent-gateway/revoke", headers=...)`.)

`requirements.txt`: `streamlit>=1.36`, `requests>=2.31`.

- [ ] **Step 2: Parse + boot check**

```bash
agent/.venv/bin/python -c "import ast; ast.parse(open('demos/04-external-agent/app.py').read()); print('parse-ok')"
```

- [ ] **Step 3: README + commit**

Add a demo-4 row (external mandated agent) and its run command (gateway token) to `demos/README.md`.
```bash
git add demos/ && git commit -m "feat(demos): demo 4 — external mandated agent console

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: Deploy, live e2e, run demo 4

**Files:** none (deploy + verify).

- [ ] **Step 1: Rebuild bank-api (scopes) + agent-api (gateway) + agent-mcp (unchanged unless needed)**

```bash
cd /home/bmartins/dev/nano-bank
docker build -t nano-bank-api:dev ./api -q && kind load docker-image nano-bank-api:dev --name nano-bank
docker build -f agent/Dockerfile.api -t nano-agent-api:dev ./agent -q && kind load docker-image nano-agent-api:dev --name nano-bank
kubectl --context kind-nano-bank -n nano-bank rollout restart deploy/bank-api deploy/agent-api
kubectl --context kind-nano-bank -n nano-bank rollout status deploy/bank-api --timeout=180s
kubectl --context kind-nano-bank -n nano-bank rollout status deploy/agent-api --timeout=180s
```

- [ ] **Step 2: Seed the agent+mandate and configure the branch env**

Port-forward agent-api; call the demo-seed endpoint to register the agent + grant the mandate; set `NANO_AGENT_ID/SECRET`, `AGENT_MANDATE_ID`, `AGENT_CUSTOMER_ID`, `AGENT_GATEWAY_TOKEN` on the agent-api deployment (env or the secret) and roll out. (Exact `kubectl set env` commands finalized here once the seed output shape is confirmed live.)

- [ ] **Step 3: Live e2e — the single door + mandate gate + revoke**

```bash
# with the agent-api port-forward on :8086 and AGENT_GATEWAY_TOKEN=$GT
curl -fsS localhost:8086/agent-gateway/mandate -H "Authorization: Bearer $GT" | python3 -m json.tool | head
# transfer within cap → allow; over cap → deny
curl -fsS -X POST localhost:8086/agent-gateway/act -H "Authorization: Bearer $GT" \
  -H 'content-type: application/json' -d '{"operation":"transfer_out","params":{"to_account_id":"'$TO'","amount":"50"}}'
curl -fsS -X POST localhost:8086/agent-gateway/act -H "Authorization: Bearer $GT" \
  -H 'content-type: application/json' -d '{"operation":"transfer_out","params":{"to_account_id":"'$TO'","amount":"9999"}}'
# advisory via the manager
curl -fsS -X POST localhost:8086/agent-gateway/message -H "Authorization: Bearer $GT" \
  -H 'content-type: application/json' -d '{"message":"What are the benefits of a savings account?"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["answer"][:120])'
# revoke → next act denied
curl -fsS -X POST localhost:8086/agent-gateway/revoke -H "Authorization: Bearer $GT"
curl -fsS -X POST localhost:8086/agent-gateway/act -H "Authorization: Bearer $GT" \
  -H 'content-type: application/json' -d '{"operation":"transfer_out","params":{"to_account_id":"'$TO'","amount":"50"}}'
```
Expected: mandate shown; $50 allow; $9999 deny (cap); advisory answer; after revoke the transfer is denied ("revoked or expired"). Confirms the single door, mandate cap, live revocation.

- [ ] **Step 4: Run demo 4 on the LAN (:8513)**

```bash
setsid bash -c "DEMO_BRANCH_BASE=http://localhost:8086 AGENT_GATEWAY_TOKEN=$GT exec agent/.venv/bin/python -m streamlit run demos/04-external-agent/app.py --server.address 0.0.0.0 --server.port 8513 --server.headless true" >/tmp/demo4.log 2>&1 </dev/null & disown
sleep 9; curl -fsS -o /dev/null -w 'demo4 :8513 HTTP %{http_code}\n' http://localhost:8513/
```

- [ ] **Step 5: Commit + push**

```bash
git add -A && git commit -m "chore: external mandated agent live-verified (single door + revoke)" || true
git push origin agent-k8s-e2e
```

---

## Self-Review notes

- **Spec coverage:** §consent reuse→refs #19; §branch PEP/door→T2+T3; §autonomous agent→T4; §NetworkPolicy path A→T5 (+ app-level: agent holds only the gateway token); §demo 4 + revoke→T7; §seed→T6; §scopes for open-account/payee→T1. Transfer-out + savings-advisory are the demonstrated ops (T8 e2e).
- **Placeholder scan:** concrete code for T1–T6; T7 gives the demo's structure + key calls (full render reuses the demo-3 bubble helper); T8 has exact curl checks. The two "finalized live" notes (seed env wiring, exact `set env`) depend on the live seed output and are resolved during execution, not guesses baked into code.
- **Type/name consistency:** `MandatePEP.check(mandate_id, scope, amount) -> Decision(allowed,mandate,reason)` is produced in T2 and consumed in T3; gateway ops→scope map (`transfer_out→transfer:initiate`, `open_account→account:open`, `register_payee→payee:register`) matches T1's new scopes; `ExternalAgent.run` event shape (T4) is what demo 4 renders (T7).
- **Watch-outs:** the branch holds the agent creds (never sent to the demo/agent); the PEP re-reads live mandates each call (revocation immediate); Kind kindnet won't enforce the NetworkPolicy (app-level is primary); `transfer_out` needs a real `to_account_id` (a second account — the seed opens chequing+savings).
