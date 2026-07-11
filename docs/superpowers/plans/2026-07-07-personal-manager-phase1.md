# Nano-Bank Personal Manager — Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a containerized agentic personal manager for a nano-bank client that knows everything about one client, answers/advises, and performs transactions on explicit instruction behind a mandatory two-phase (propose → confirm) guardrail, exposed as an agent-to-agent HTTP endpoint plus a dev test console.

**Architecture:** A LangGraph ReAct agent (ported from `agentic_patterns/src/agent_desktop/desktop_agent.py`) on GLM via Ollama-cloud. All client data access — Postgres reads, Qdrant memory, and money-moving writes — is funnelled through **one MCP server** whose tools carry **no `customer_id` and no token**; the bound customer + JWT are injected server-side from trusted transport headers the LLM never sees. Money movement is two-phase: LLM-callable `propose_*` tools only record a pending action; a separate, non-LLM `execute_action` runs only via an explicit confirm request.

**Tech Stack:** Python 3.12, LangGraph + `langgraph.prebuilt.create_react_agent`, `langchain-openai` (Ollama-cloud OpenAI-compat), `mcp` / `FastMCP` + `langchain-mcp-adapters`, `qdrant-client` + `fastembed` (CPU, local Qdrant, **not** ragu), `psycopg2` (read-only), `httpx` (nano-bank API client), `fastapi` + `uvicorn`, `streamlit` (test console), `pytest`, podman + Containerfile.

## Global Constraints

- Python service in a **new `agent/` directory** in the nano-bank repo; the Rust API and DB schema are **not modified** in Phase 1.
- Backend is **Ollama-cloud OpenAI-compat** at `OLLAMA_BASE_URL` (`https://ollama.com/v1`); model is `MANAGER_MODEL` (`glm-5.2`) with startup fallback to `MANAGER_FALLBACK_MODEL` (`glm-4.7`).
- Memory is a **dedicated local Qdrant** — collection `nano_manager_memory`, **not** ragu's instance; host `http://localhost:6335`, in-network `http://qdrant:6333`. Embeddings via **fastembed / CPU**.
- The agent has **no filesystem / bash / code tools**. Its only tools are the customer-bound MCP tools.
- **No LLM-callable tool takes a `customer_id` or an auth token**, and **no LLM-callable tool executes a payment.** The bound customer id and nano-bank JWT come only from the MCP transport headers `X-Nano-Customer` / `X-Nano-Token`.
- **Confirmation is mandatory** for every money movement, identically for the console and A2A callers. Execution uses the *stored* pending-action parameters; the bank `idempotency_key` is the action id.
- Writes go through the **authenticated nano-bank API on `:8081`** (`POST /api/v1/transactions/{deposit,withdrawal,transfer}`) — never direct DB writes.
- DB host is `::1` when running on the host directly, `host.containers.internal` inside a container (Kind Postgres via host port-forward). Reuse `testing/viewer`'s `DB_*` defaults (`DB_NAME=nano_bank_db`, `DB_USER=nanobank_user`, `DB_PASSWORD=secure_nano_password_2024!`).
- Spec: `docs/superpowers/specs/2026-07-07-personal-manager-design.md`.
- All work on branch `personal-manager`. Commit after every task.

## File structure

```
agent/
  __init__.py
  config.py          # Settings.from_env() — all env config in one dataclass
  model_factory.py   # Ollama-cloud ChatOpenAI factory + glm-5.2→glm-4.7 resolver + healthcheck
  db.py              # ClientContext: read-only Postgres reads + snapshot + owns_account
  memory.py          # QdrantMemory (bi-temporal, per-customer) + AuditLog
  bank.py            # BankClient: nano-bank API (login, deposit/withdraw/transfer, create_*)
  actions.py         # ActionStore: two-phase propose/execute/cancel + guardrails (cap/TTL/ownership)
  mcp_server.py      # FastMCP server: read+rag+propose tools (LLM-safe) + execute/cancel (confirm-only); header→contextvar binding
  nano_manager.py    # model+graph+MCP-client wiring; assist(); LLM-safe tool subset; context hook
  api.py             # FastAPI: /message, /actions/{id}/confirm|cancel, /profile, /health; token resolution; service-token auth
  seed.py            # dev seeding: create customer/account/deposit/transfer + store creds
  test_console.py    # Streamlit test interface (seed + chat + confirm)
  requirements.txt
  .env.example
  README.md
  Containerfile.api
  Containerfile.mcp
  Containerfile.console
  compose.yaml
  run-agent.sh
  tests/
    __init__.py
    conftest.py
    test_config.py
    test_model_factory.py
    test_db.py
    test_memory.py
    test_bank.py
    test_actions.py
    test_mcp_binding.py
    test_nano_manager.py
    test_api.py
    test_seed.py
```

**Working-software midpoint:** after Task 8 the manager answers questions about a seeded client end-to-end (read/advise). Tasks 9–12 add instructed act + confirmation. Tasks 13–14 containerize and document.

---

### Task 1: Package scaffold + config

**Files:**
- Create: `agent/__init__.py`, `agent/config.py`, `agent/requirements.txt`, `agent/.env.example`, `agent/tests/__init__.py`, `agent/tests/conftest.py`
- Test: `agent/tests/test_config.py`

**Interfaces:**
- Produces: `config.Settings` (dataclass) with fields `ollama_api_key: str`, `ollama_base_url: str`, `manager_model: str`, `manager_fallback_model: str`, `qdrant_url: str`, `qdrant_collection: str`, `db: dict`, `nano_bank_api: str`, `branch_service_token: str`, `act_max_per_tx: Decimal`, `confirm_ttl_s: int`, `mcp_url: str`, `branch_port: int`, `console_port: int`; classmethod `Settings.from_env(env: Mapping[str,str] | None = None) -> Settings`.

- [ ] **Step 1: Write `agent/requirements.txt`**

```
langgraph>=0.2
langchain-core>=0.3
langchain-openai>=0.2
langchain-mcp-adapters>=0.1
mcp>=1.2
qdrant-client>=1.11
fastembed>=0.4
psycopg2-binary>=2.9
httpx>=0.27
fastapi>=0.115
uvicorn>=0.30
streamlit>=1.38
pytest>=8.0
```

- [ ] **Step 2: Write the failing test**

`agent/tests/test_config.py`:

```python
from decimal import Decimal
from agent.config import Settings


def test_defaults_when_env_empty():
    s = Settings.from_env({})
    assert s.ollama_base_url == "https://ollama.com/v1"
    assert s.manager_model == "glm-5.2"
    assert s.manager_fallback_model == "glm-4.7"
    assert s.qdrant_collection == "nano_manager_memory"
    assert s.confirm_ttl_s == 300
    assert s.act_max_per_tx == Decimal("1000")
    assert s.db["dbname"] == "nano_bank_db"


def test_env_overrides():
    s = Settings.from_env({
        "MANAGER_MODEL": "glm-9",
        "ACT_MAX_PER_TX": "50.5",
        "CONFIRM_TTL_S": "90",
        "DB_HOST": "host.containers.internal",
    })
    assert s.manager_model == "glm-9"
    assert s.act_max_per_tx == Decimal("50.5")
    assert s.confirm_ttl_s == 90
    assert s.db["host"] == "host.containers.internal"
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cd /home/bmartins/dev/nano-bank && python -m pytest agent/tests/test_config.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'agent.config'`

- [ ] **Step 4: Write `agent/config.py`**

```python
from __future__ import annotations
import os
from dataclasses import dataclass, field
from decimal import Decimal
from typing import Mapping, Optional


@dataclass
class Settings:
    ollama_api_key: str
    ollama_base_url: str
    manager_model: str
    manager_fallback_model: str
    qdrant_url: str
    qdrant_collection: str
    db: dict
    nano_bank_api: str
    branch_service_token: str
    act_max_per_tx: Decimal
    confirm_ttl_s: int
    mcp_url: str
    branch_port: int
    console_port: int

    @classmethod
    def from_env(cls, env: Optional[Mapping[str, str]] = None) -> "Settings":
        e = os.environ if env is None else env

        def g(k, d=""):
            return e.get(k, d)

        return cls(
            ollama_api_key=g("OLLAMA_API_KEY"),
            ollama_base_url=g("OLLAMA_BASE_URL", "https://ollama.com/v1"),
            manager_model=g("MANAGER_MODEL", "glm-5.2"),
            manager_fallback_model=g("MANAGER_FALLBACK_MODEL", "glm-4.7"),
            qdrant_url=g("QDRANT_URL", "http://localhost:6335"),
            qdrant_collection=g("QDRANT_COLLECTION", "nano_manager_memory"),
            db=dict(
                host=g("DB_HOST", "::1"),
                port=int(g("DB_PORT", "5432")),
                dbname=g("DB_NAME", "nano_bank_db"),
                user=g("DB_USER", "nanobank_user"),
                password=g("DB_PASSWORD", "secure_nano_password_2024!"),
            ),
            nano_bank_api=g("NANO_BANK_API", "http://localhost:8081"),
            branch_service_token=g("BRANCH_SERVICE_TOKEN"),
            act_max_per_tx=Decimal(g("ACT_MAX_PER_TX", "1000")),
            confirm_ttl_s=int(g("CONFIRM_TTL_S", "300")),
            mcp_url=g("MCP_URL", "http://localhost:8087/mcp"),
            branch_port=int(g("BRANCH_PORT", "8086")),
            console_port=int(g("CONSOLE_PORT", "8505")),
        )
```

Write `agent/__init__.py` (empty), `agent/tests/__init__.py` (empty), and `agent/tests/conftest.py`:

```python
import pytest


def pytest_configure(config):
    config.addinivalue_line("markers", "live: needs external services (DB/Ollama/bank); skipped by default")
```

Write `agent/.env.example` mirroring §11 of the spec (one `KEY=value` per line for every field above, with the defaults; `OLLAMA_API_KEY=` and `BRANCH_SERVICE_TOKEN=` left blank).

- [ ] **Step 5: Run test to verify it passes**

Run: `python -m pytest agent/tests/test_config.py -q`
Expected: PASS (2 passed)

- [ ] **Step 6: Commit**

```bash
git add agent/__init__.py agent/config.py agent/requirements.txt agent/.env.example agent/tests
git commit -m "feat(agent): package scaffold + Settings.from_env"
```

---

### Task 2: Model factory + glm-5.2→glm-4.7 resolver

**Files:**
- Create: `agent/model_factory.py`
- Test: `agent/tests/test_model_factory.py`

**Interfaces:**
- Consumes: `config.Settings`.
- Produces:
  - `resolve_model(settings, probe: Callable[[str, Settings], bool] | None = None) -> str` — returns `manager_model` if its probe succeeds else `manager_fallback_model`; raises `RuntimeError` if neither probes true.
  - `init_models(settings, probe=None) -> str` — resolves once and caches the id in module state.
  - `llm(role: str = "fast", *, temperature: float = 0.2, max_tokens: int | None = None) -> ChatOpenAI` — builds a client for the cached resolved id (raises if `init_models` not called).
  - `build_model(model, settings, *, temperature=0.2, max_tokens=None) -> ChatOpenAI`.
  - `backend_healthcheck(settings) -> bool`.

- [ ] **Step 1: Write the failing test**

`agent/tests/test_model_factory.py`:

```python
import pytest
from agent.config import Settings
from agent import model_factory as mf


def _settings():
    return Settings.from_env({"OLLAMA_API_KEY": "x"})


def test_resolver_picks_primary_when_it_probes():
    s = _settings()
    assert mf.resolve_model(s, probe=lambda model, st: model == "glm-5.2") == "glm-5.2"


def test_resolver_falls_back_when_primary_fails():
    s = _settings()
    assert mf.resolve_model(s, probe=lambda model, st: model == "glm-4.7") == "glm-4.7"


def test_resolver_raises_when_both_fail():
    s = _settings()
    with pytest.raises(RuntimeError):
        mf.resolve_model(s, probe=lambda model, st: False)


def test_llm_requires_init(monkeypatch):
    monkeypatch.setattr(mf, "_RESOLVED", None, raising=False)
    with pytest.raises(RuntimeError):
        mf.llm("fast")
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest agent/tests/test_model_factory.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'agent.model_factory'`

- [ ] **Step 3: Write `agent/model_factory.py`**

```python
from __future__ import annotations
import logging
from functools import lru_cache
from typing import Callable, Optional

from langchain_core.messages import HumanMessage
from langchain_openai import ChatOpenAI

from .config import Settings

log = logging.getLogger("nano_manager.llm")

_RESOLVED: Optional[str] = None
_SETTINGS: Optional[Settings] = None


def build_model(model: str, settings: Settings, *, temperature: float = 0.2,
                max_tokens: Optional[int] = None) -> ChatOpenAI:
    kw = dict(model=model, temperature=temperature, base_url=settings.ollama_base_url,
              api_key=settings.ollama_api_key or "ollama", timeout=600)
    if max_tokens:
        kw["max_tokens"] = max_tokens
    return ChatOpenAI(**kw)


def _default_probe(model: str, settings: Settings) -> bool:
    try:
        m = build_model(model, settings, temperature=0.0, max_tokens=8)
        m.invoke([HumanMessage("reply with the single word: ok")])
        return True
    except Exception as e:  # noqa: BLE001 - probe must not raise
        log.warning("probe failed for %s: %s", model, e)
        return False


def resolve_model(settings: Settings, probe: Optional[Callable[[str, Settings], bool]] = None) -> str:
    probe = probe or _default_probe
    for model in (settings.manager_model, settings.manager_fallback_model):
        if probe(model, settings):
            log.info("resolved model: %s", model)
            return model
    raise RuntimeError(
        f"neither {settings.manager_model} nor {settings.manager_fallback_model} answered at "
        f"{settings.ollama_base_url}")


def init_models(settings: Settings, probe: Optional[Callable[[str, Settings], bool]] = None) -> str:
    global _RESOLVED, _SETTINGS
    _SETTINGS = settings
    _RESOLVED = resolve_model(settings, probe)
    return _RESOLVED


@lru_cache(maxsize=8)
def _client(model: str, temperature: float, max_tokens: Optional[int]) -> ChatOpenAI:
    return build_model(model, _SETTINGS, temperature=temperature, max_tokens=max_tokens)


def llm(role: str = "fast", *, temperature: float = 0.2, max_tokens: Optional[int] = None) -> ChatOpenAI:
    if _RESOLVED is None or _SETTINGS is None:
        raise RuntimeError("call init_models(settings) before llm()")
    return _client(_RESOLVED, temperature, max_tokens)


def backend_healthcheck(settings: Settings) -> bool:
    try:
        return _default_probe(settings.manager_model, settings) or \
               _default_probe(settings.manager_fallback_model, settings)
    except Exception:  # noqa: BLE001
        return False
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest agent/tests/test_model_factory.py -q`
Expected: PASS (4 passed)

- [ ] **Step 5: Commit**

```bash
git add agent/model_factory.py agent/tests/test_model_factory.py
git commit -m "feat(agent): Ollama-cloud model factory + glm-5.2->glm-4.7 resolver"
```

---

### Task 3: DB reads (`ClientContext`)

**Files:**
- Create: `agent/db.py`
- Test: `agent/tests/test_db.py`

**Interfaces:**
- Consumes: `config.Settings.db`.
- Produces: `ClientContext(db_params: dict)` with methods (all filter by `customer_id`):
  - `profile(customer_id: str) -> dict | None`
  - `accounts(customer_id: str) -> list[dict]`
  - `transactions(customer_id: str, limit: int = 20) -> list[dict]`
  - `cards(customer_id: str) -> list[dict]`
  - `owns_account(customer_id: str, account_id: str) -> bool`
  - `snapshot(customer_id: str) -> str`
  - internal `_rows(sql: str, params: tuple) -> list[dict]` (monkeypatched in tests; real impl opens a **read-only** psycopg2 connection).

- [ ] **Step 1: Write the failing test** (offline — `_rows` is stubbed, so no DB needed)

`agent/tests/test_db.py`:

```python
from agent.db import ClientContext


class FakeCtx(ClientContext):
    def __init__(self, tables):
        self._tables = tables  # dict: name -> list[dict]

    def _rows(self, sql, params):
        # crude router: pick table by a marker in the SQL comment
        if "-- accounts" in sql:
            return self._tables.get("accounts", [])
        if "-- transactions" in sql:
            return self._tables.get("transactions", [])
        if "-- profile" in sql:
            return self._tables.get("profile", [])
        if "-- owns" in sql:
            return self._tables.get("owns", [])
        return []


def test_snapshot_includes_name_and_balance():
    ctx = FakeCtx({
        "profile": [{"first_name": "Ada", "last_name": "L", "email": "a@x.ca",
                     "kyc_status": "Verified"}],
        "accounts": [{"account_id": "acc-1", "account_type": "chequing",
                      "balance": "1200.00", "status": "active"}],
        "transactions": [{"transaction_type": "deposit", "amount": "1200.00",
                          "created_at": "2026-07-01"}],
    })
    snap = ctx.snapshot("cust-1")
    assert "Ada" in snap and "1200.00" in snap and "chequing" in snap


def test_owns_account_true_false():
    ctx = FakeCtx({"owns": [{"n": 1}]})
    assert ctx.owns_account("cust-1", "acc-1") is True
    ctx2 = FakeCtx({"owns": []})
    assert ctx2.owns_account("cust-1", "acc-9") is False
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest agent/tests/test_db.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'agent.db'`

- [ ] **Step 3: Write `agent/db.py`**

```python
from __future__ import annotations
from typing import Optional


class ClientContext:
    """Read-only Postgres access, always scoped to a customer_id."""

    def __init__(self, db_params: Optional[dict] = None):
        self._db = db_params

    # -- real connection (overridden in tests) --------------------------------
    def _rows(self, sql: str, params: tuple) -> list[dict]:
        import psycopg2
        import psycopg2.extras
        conn = psycopg2.connect(**self._db)
        try:
            conn.set_session(readonly=True, autocommit=True)
            with conn.cursor(cursor_factory=psycopg2.extras.RealDictCursor) as cur:
                cur.execute(sql, params)
                return [dict(r) for r in cur.fetchall()]
        finally:
            conn.close()

    def profile(self, customer_id: str) -> Optional[dict]:
        rows = self._rows(
            "-- profile\nSELECT first_name, last_name, email, kyc_status "
            "FROM customers WHERE customer_id = %s", (customer_id,))
        return rows[0] if rows else None

    def accounts(self, customer_id: str) -> list[dict]:
        return self._rows(
            "-- accounts\nSELECT account_id, account_type, balance, status "
            "FROM accounts WHERE customer_id = %s ORDER BY account_type", (customer_id,))

    def transactions(self, customer_id: str, limit: int = 20) -> list[dict]:
        return self._rows(
            "-- transactions\nSELECT te.transaction_type, te.amount, t.created_at "
            "FROM transaction_entries te JOIN transactions t ON t.transaction_id = te.transaction_id "
            "JOIN accounts a ON a.account_id = te.account_id "
            "WHERE a.customer_id = %s ORDER BY t.created_at DESC LIMIT %s",
            (customer_id, limit))

    def cards(self, customer_id: str) -> list[dict]:
        return self._rows(
            "-- accounts\nSELECT account_id, account_type, balance, overdraft_limit, status "
            "FROM accounts WHERE customer_id = %s AND account_type = 'credit_card'",
            (customer_id,))

    def owns_account(self, customer_id: str, account_id: str) -> bool:
        rows = self._rows(
            "-- owns\nSELECT 1 AS n FROM accounts WHERE customer_id = %s AND account_id = %s",
            (customer_id, account_id))
        return len(rows) > 0

    def snapshot(self, customer_id: str) -> str:
        p = self.profile(customer_id) or {}
        accts = self.accounts(customer_id)
        txns = self.transactions(customer_id, limit=8)
        lines = [
            f"CLIENT: {p.get('first_name','?')} {p.get('last_name','')} "
            f"<{p.get('email','?')}> KYC={p.get('kyc_status','?')}",
            "ACCOUNTS:",
        ]
        for a in accts:
            lines.append(f"  - {a['account_type']} {a['account_id']}: "
                         f"balance {a['balance']} ({a['status']})")
        lines.append("RECENT TRANSACTIONS:")
        for t in txns:
            lines.append(f"  - {t['created_at']}: {t['transaction_type']} {t['amount']}")
        return "\n".join(lines)
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest agent/tests/test_db.py -q`
Expected: PASS (2 passed)

> Note: `transaction_entries`/`transactions` join reflects the double-entry schema (`src/core/tables/04_transactions.sql`). Verify column names against that DDL when wiring the live path; adjust the SQL string only (tests stub `_rows`).

- [ ] **Step 5: Commit**

```bash
git add agent/db.py agent/tests/test_db.py
git commit -m "feat(agent): ClientContext read-only DB reads + snapshot"
```

---

### Task 4: Qdrant memory + audit log

**Files:**
- Create: `agent/memory.py`
- Test: `agent/tests/test_memory.py`

**Interfaces:**
- Consumes: `config.Settings`.
- Produces:
  - `QdrantMemory(client, collection: str, embed)` + classmethod `in_memory(collection="test") -> QdrantMemory` (uses `QdrantClient(":memory:")` + fastembed).
  - `store(fact, *, customer_id, kind="observation", source="agent", thread_id=None) -> str`
  - `invalidate(fact_id, reason)`
  - `query_valid(customer_id, kind=None, thread_id=None) -> list[dict]`
  - `recall(query, customer_id, k=3, thread_id=None) -> list[str]`
  - `AuditLog(client, collection="nano_manager_audit")` + `in_memory()`; `record(event: dict) -> str`; `for_customer(customer_id) -> list[dict]`.

- [ ] **Step 1: Write the failing test** (offline — in-memory Qdrant + fastembed CPU)

`agent/tests/test_memory.py`:

```python
import pytest
from agent.memory import QdrantMemory, AuditLog


@pytest.fixture
def mem():
    return QdrantMemory.in_memory()


def test_store_and_recall(mem):
    mem.store("client prefers e-transfer over cheque", customer_id="A")
    hits = mem.recall("how does the client like to send money", customer_id="A", k=3)
    assert any("e-transfer" in h for h in hits)


def test_recall_is_customer_scoped(mem):
    mem.store("A's secret goal is a boat", customer_id="A")
    assert mem.recall("boat", customer_id="B", k=5) == []


def test_invalidate_hides_fact(mem):
    fid = mem.store("old address is 1 Main St", customer_id="A")
    mem.invalidate(fid, reason="moved")
    assert all("1 Main St" not in h for h in mem.recall("address", customer_id="A", k=5))


def test_audit_append_and_read():
    a = AuditLog.in_memory()
    a.record({"customer_id": "A", "kind": "transfer", "amount": "50", "outcome": "proposed"})
    rows = a.for_customer("A")
    assert len(rows) == 1 and rows[0]["outcome"] == "proposed"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest agent/tests/test_memory.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'agent.memory'`

- [ ] **Step 3: Write `agent/memory.py`**

```python
from __future__ import annotations
import time
import uuid
from typing import Optional

from qdrant_client import QdrantClient, models


def _embedder():
    from fastembed import TextEmbedding
    return TextEmbedding()  # small default CPU model


class QdrantMemory:
    def __init__(self, client: QdrantClient, collection: str, embed):
        self.client = client
        self.collection = collection
        self._embed = embed
        self._dim = len(next(iter(embed.embed(["dim probe"]))))
        if not client.collection_exists(collection):
            client.create_collection(
                collection,
                vectors_config=models.VectorParams(size=self._dim, distance=models.Distance.COSINE))

    @classmethod
    def in_memory(cls, collection: str = "test_mem") -> "QdrantMemory":
        return cls(QdrantClient(":memory:"), collection, _embedder())

    @classmethod
    def from_settings(cls, settings) -> "QdrantMemory":
        return cls(QdrantClient(url=settings.qdrant_url), settings.qdrant_collection, _embedder())

    def _vec(self, text: str):
        return list(next(iter(self._embed.embed([text]))))

    def store(self, fact: str, *, customer_id: str, kind: str = "observation",
              source: str = "agent", thread_id: Optional[str] = None) -> str:
        pid = uuid.uuid4().hex
        self.client.upsert(self.collection, points=[models.PointStruct(
            id=pid, vector=self._vec(fact),
            payload={"customer_id": customer_id, "kind": kind, "source": source,
                     "fact": fact, "thread_id": thread_id,
                     "valid_from": time.time(), "valid_to": None})])
        return pid

    def invalidate(self, fact_id: str, reason: str) -> None:
        self.client.set_payload(self.collection, payload={"valid_to": time.time(),
                                "invalidated_reason": reason}, points=[fact_id])

    def _valid_filter(self, customer_id: str, kind: Optional[str], thread_id: Optional[str]):
        must = [models.FieldCondition(key="customer_id", match=models.MatchValue(value=customer_id)),
                models.IsNullCondition(is_null=models.PayloadField(key="valid_to"))]
        if kind:
            must.append(models.FieldCondition(key="kind", match=models.MatchValue(value=kind)))
        if thread_id:
            must.append(models.FieldCondition(key="thread_id", match=models.MatchValue(value=thread_id)))
        return models.Filter(must=must)

    def query_valid(self, customer_id: str, kind=None, thread_id=None) -> list[dict]:
        pts, _ = self.client.scroll(self.collection, limit=200,
                                    scroll_filter=self._valid_filter(customer_id, kind, thread_id))
        return [p.payload for p in pts]

    def recall(self, query: str, customer_id: str, k: int = 3, thread_id=None) -> list[str]:
        hits = self.client.query_points(
            self.collection, query=self._vec(query), limit=k,
            query_filter=self._valid_filter(customer_id, None, thread_id)).points
        return [h.payload["fact"] for h in hits]


class AuditLog:
    def __init__(self, client: QdrantClient, collection: str = "nano_manager_audit"):
        self.client = client
        self.collection = collection
        if not client.collection_exists(collection):
            client.create_collection(
                collection, vectors_config=models.VectorParams(size=1, distance=models.Distance.DOT))

    @classmethod
    def in_memory(cls) -> "AuditLog":
        return cls(QdrantClient(":memory:"))

    @classmethod
    def from_settings(cls, settings) -> "AuditLog":
        return cls(QdrantClient(url=settings.qdrant_url))

    def record(self, event: dict) -> str:
        pid = uuid.uuid4().hex
        event = {**event, "ts": time.time()}
        self.client.upsert(self.collection,
                           points=[models.PointStruct(id=pid, vector=[0.0], payload=event)])
        return pid

    def for_customer(self, customer_id: str) -> list[dict]:
        pts, _ = self.client.scroll(self.collection, limit=500,
            scroll_filter=models.Filter(must=[models.FieldCondition(
                key="customer_id", match=models.MatchValue(value=customer_id))]))
        return sorted((p.payload for p in pts), key=lambda e: e.get("ts", 0))
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest agent/tests/test_memory.py -q`
Expected: PASS (4 passed). First run downloads the fastembed model (network once); if the CI box is fully offline, mark the file `@pytest.mark.live` and run locally.

- [ ] **Step 5: Commit**

```bash
git add agent/memory.py agent/tests/test_memory.py
git commit -m "feat(agent): per-customer bi-temporal Qdrant memory + audit log"
```

---

### Task 5: nano-bank API client (`BankClient`)

**Files:**
- Create: `agent/bank.py`
- Test: `agent/tests/test_bank.py`

**Interfaces:**
- Consumes: `config.Settings.nano_bank_api`.
- Produces: `BankClient(base_url: str, http=None)` with:
  - `login(email, password) -> str` (JWT)
  - `deposit(token, account_id, amount, idempotency_key=None) -> dict`
  - `withdraw(token, account_id, amount, idempotency_key=None) -> dict`
  - `transfer(token, from_account, to_account, amount, memo=None, idempotency_key=None) -> dict`
  - `create_customer(payload: dict) -> dict`
  - `create_account(token, payload: dict) -> dict`
  - Each maps non-2xx to `BankError(status, body)`.

- [ ] **Step 1: Write the failing test** (offline — inject a fake httpx transport)

`agent/tests/test_bank.py`:

```python
import json
import httpx
import pytest
from agent.bank import BankClient, BankError


def _client(handler):
    transport = httpx.MockTransport(handler)
    return BankClient("http://bank.test", http=httpx.Client(transport=transport))


def test_transfer_sends_token_amount_and_idempotency():
    seen = {}

    def handler(req: httpx.Request) -> httpx.Response:
        seen["url"] = str(req.url)
        seen["auth"] = req.headers.get("authorization")
        seen["idem"] = req.headers.get("idempotency-key")
        seen["body"] = json.loads(req.content)
        return httpx.Response(201, json={"transaction_id": "t1"})

    bank = _client(handler)
    out = bank.transfer("jwt-abc", "acc-from", "acc-to", "50.00",
                        memo="rent", idempotency_key="act-1")
    assert out["transaction_id"] == "t1"
    assert seen["url"].endswith("/api/v1/transactions/transfer")
    assert seen["auth"] == "Bearer jwt-abc"
    assert seen["idem"] == "act-1"
    assert seen["body"]["amount"] == "50.00"


def test_non_2xx_raises_bankerror():
    bank = _client(lambda req: httpx.Response(422, json={"error": {"message": "insufficient"}}))
    with pytest.raises(BankError) as ei:
        bank.deposit("jwt", "acc", "10")
    assert ei.value.status == 422
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest agent/tests/test_bank.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'agent.bank'`

- [ ] **Step 3: Write `agent/bank.py`**

```python
from __future__ import annotations
from typing import Optional
import httpx


class BankError(Exception):
    def __init__(self, status: int, body):
        super().__init__(f"nano-bank {status}: {body}")
        self.status = status
        self.body = body


class BankClient:
    def __init__(self, base_url: str, http: Optional[httpx.Client] = None):
        self.base = base_url.rstrip("/")
        self.http = http or httpx.Client(timeout=30)

    def _post(self, path: str, json: dict, token: Optional[str] = None,
              idempotency_key: Optional[str] = None) -> dict:
        headers = {}
        if token:
            headers["Authorization"] = f"Bearer {token}"
        if idempotency_key:
            headers["Idempotency-Key"] = idempotency_key
        r = self.http.post(self.base + path, json=json, headers=headers)
        if r.status_code // 100 != 2:
            raise BankError(r.status_code, _safe_json(r))
        return _safe_json(r)

    def login(self, email: str, password: str) -> str:
        out = self._post("/api/v1/auth/login", {"email": email, "password": password})
        return out.get("access_token") or out["token"]

    def deposit(self, token, account_id, amount, idempotency_key=None) -> dict:
        return self._post("/api/v1/transactions/deposit",
                          {"account_id": account_id, "amount": str(amount)},
                          token=token, idempotency_key=idempotency_key)

    def withdraw(self, token, account_id, amount, idempotency_key=None) -> dict:
        return self._post("/api/v1/transactions/withdrawal",
                          {"account_id": account_id, "amount": str(amount)},
                          token=token, idempotency_key=idempotency_key)

    def transfer(self, token, from_account, to_account, amount, memo=None,
                 idempotency_key=None) -> dict:
        body = {"from_account_id": from_account, "to_account_id": to_account,
                "amount": str(amount)}
        if memo:
            body["memo"] = memo
        return self._post("/api/v1/transactions/transfer", body,
                          token=token, idempotency_key=idempotency_key)

    def create_customer(self, payload: dict) -> dict:
        return self._post("/api/v1/customers", payload)

    def create_account(self, token, payload: dict) -> dict:
        return self._post("/api/v1/accounts", payload, token=token)


def _safe_json(r: httpx.Response):
    try:
        return r.json()
    except Exception:  # noqa: BLE001
        return {"raw": r.text}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest agent/tests/test_bank.py -q`
Expected: PASS (2 passed)

> Note: the exact request/response field names (`from_account_id`, `access_token`, login path) must be checked against `api/src/handlers/transactions.rs` and `auth.rs` when wiring live. Tests pin the shape; adjust the strings if the Rust handlers differ.

- [ ] **Step 5: Commit**

```bash
git add agent/bank.py agent/tests/test_bank.py
git commit -m "feat(agent): nano-bank API client (login + deposit/withdraw/transfer)"
```

---

### Task 6: Two-phase act engine + guardrails (`ActionStore`)

**Files:**
- Create: `agent/actions.py`
- Test: `agent/tests/test_actions.py`

**Interfaces:**
- Consumes: `db.ClientContext`, `bank.BankClient` (+ `BankError`), `memory.AuditLog`, `Settings.act_max_per_tx`, `Settings.confirm_ttl_s`.
- Produces:
  - `ActionStore(db, bank, audit, max_per_tx: Decimal, ttl_s: int, now: Callable[[], float] = time.time)`
  - `propose(customer_id, token, kind, *, amount, from_account=None, to_account=None, memo=None) -> dict` → `{"id","summary","expires_at","kind","amount","from","to"}`; raises `ActDenied(reason)` on cap/ownership/validation failure (audited).
  - `execute(action_id, customer_id, token) -> dict` → bank result; idempotent; raises `ActError` on expired/unknown/foreign/over-cap.
  - `cancel(action_id, customer_id) -> dict`
  - `get(action_id, customer_id) -> dict | None`
  - `kind ∈ {"transfer","deposit","withdraw"}`.

- [ ] **Step 1: Write the failing test** (offline — fakes for db/bank/audit)

`agent/tests/test_actions.py`:

```python
from decimal import Decimal
import pytest
from agent.actions import ActionStore, ActDenied, ActError


class FakeDB:
    def __init__(self, owned): self.owned = set(owned)
    def owns_account(self, customer_id, account_id): return account_id in self.owned


class FakeBank:
    def __init__(self): self.calls = []
    def transfer(self, token, from_account, to_account, amount, memo=None, idempotency_key=None):
        self.calls.append(("transfer", idempotency_key, str(amount)))
        return {"transaction_id": "t-" + idempotency_key}


class FakeAudit:
    def __init__(self): self.events = []
    def record(self, e): self.events.append(e); return "a"


def _store(**kw):
    clock = {"t": 1000.0}
    db = kw.get("db", FakeDB(["acc-from", "acc-to"]))
    bank = kw.get("bank", FakeBank())
    audit = kw.get("audit", FakeAudit())
    s = ActionStore(db, bank, audit, max_per_tx=Decimal("1000"), ttl_s=300,
                    now=lambda: clock["t"])
    return s, db, bank, audit, clock


def test_propose_over_cap_denied_and_audited():
    s, *_ , audit, _ = _store()
    with pytest.raises(ActDenied):
        s.propose("C", "tok", "transfer", amount="5000", from_account="acc-from",
                  to_account="acc-to")
    assert audit.events[-1]["outcome"] == "denied"


def test_propose_foreign_source_denied():
    s, *_ = _store()
    with pytest.raises(ActDenied):
        s.propose("C", "tok", "transfer", amount="10", from_account="acc-STRANGER",
                  to_account="acc-to")


def test_propose_does_not_move_money():
    s, db, bank, *_ = _store()
    out = s.propose("C", "tok", "transfer", amount="50", from_account="acc-from",
                    to_account="acc-to")
    assert "id" in out and bank.calls == []


def test_execute_moves_money_once_idempotent():
    s, db, bank, *_ = _store()
    pid = s.propose("C", "tok", "transfer", amount="50", from_account="acc-from",
                    to_account="acc-to")["id"]
    r1 = s.execute(pid, "C", "tok")
    r2 = s.execute(pid, "C", "tok")           # duplicate confirm
    assert r1 == r2
    assert len(bank.calls) == 1               # only one bank call
    assert bank.calls[0][1] == pid            # idempotency key == action id


def test_execute_expired_refused():
    s, db, bank, audit, clock = _store()
    pid = s.propose("C", "tok", "transfer", amount="50", from_account="acc-from",
                    to_account="acc-to")["id"]
    clock["t"] += 301
    with pytest.raises(ActError):
        s.execute(pid, "C", "tok")
    assert bank.calls == []


def test_execute_foreign_customer_refused():
    s, db, bank, *_ = _store()
    pid = s.propose("C", "tok", "transfer", amount="50", from_account="acc-from",
                    to_account="acc-to")["id"]
    with pytest.raises(ActError):
        s.execute(pid, "OTHER", "tok")
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest agent/tests/test_actions.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'agent.actions'`

- [ ] **Step 3: Write `agent/actions.py`**

```python
from __future__ import annotations
import time
import uuid
from dataclasses import dataclass, asdict
from decimal import Decimal, InvalidOperation
from typing import Callable, Optional


class ActDenied(Exception):
    """Refused at propose time (policy)."""


class ActError(Exception):
    """Refused/failed at execute time."""


@dataclass
class PendingAction:
    id: str
    customer_id: str
    kind: str
    amount: str
    from_account: Optional[str]
    to_account: Optional[str]
    memo: Optional[str]
    created_at: float
    expires_at: float
    status: str = "pending"   # pending | executed | cancelled
    result: Optional[dict] = None


_KINDS = {"transfer", "deposit", "withdraw"}


class ActionStore:
    def __init__(self, db, bank, audit, max_per_tx: Decimal, ttl_s: int,
                 now: Callable[[], float] = time.time):
        self.db = db
        self.bank = bank
        self.audit = audit
        self.max = max_per_tx
        self.ttl = ttl_s
        self.now = now
        self._pending: dict[str, PendingAction] = {}

    def _amount(self, amount) -> Decimal:
        try:
            a = Decimal(str(amount))
        except (InvalidOperation, ValueError):
            raise ActDenied(f"invalid amount: {amount!r}")
        if a <= 0:
            raise ActDenied("amount must be positive")
        return a

    def propose(self, customer_id, token, kind, *, amount,
                from_account=None, to_account=None, memo=None) -> dict:
        if kind not in _KINDS:
            raise ActDenied(f"unknown kind: {kind}")
        a = self._amount(amount)
        if a > self.max:
            self._audit(customer_id, kind, a, "denied", "over cap")
            raise ActDenied(f"amount {a} exceeds per-transaction cap {self.max}")
        # ownership: any account the customer names as *theirs* must belong to them.
        for acct in ((from_account,) if kind in ("transfer", "withdraw") else (to_account,)):
            if acct and not self.db.owns_account(customer_id, acct):
                self._audit(customer_id, kind, a, "denied", f"account {acct} not owned")
                raise ActDenied(f"account {acct} is not yours")
        if kind == "transfer" and not (from_account and to_account):
            raise ActDenied("transfer needs from_account and to_account")
        pid = uuid.uuid4().hex
        now = self.now()
        pa = PendingAction(id=pid, customer_id=customer_id, kind=kind, amount=str(a),
                           from_account=from_account, to_account=to_account, memo=memo,
                           created_at=now, expires_at=now + self.ttl)
        self._pending[pid] = pa
        self._audit(customer_id, kind, a, "proposed", "", action_id=pid)
        return {"id": pid, "kind": kind, "amount": str(a), "from": from_account,
                "to": to_account, "expires_at": pa.expires_at, "summary": self._summary(pa)}

    def execute(self, action_id, customer_id, token) -> dict:
        pa = self._pending.get(action_id)
        if pa is None or pa.customer_id != customer_id:
            raise ActError("unknown action")
        if pa.status == "executed":
            return pa.result                      # idempotent replay
        if pa.status == "cancelled":
            raise ActError("action cancelled")
        if self.now() > pa.expires_at:
            self._audit(customer_id, pa.kind, Decimal(pa.amount), "expired", "")
            raise ActError("action expired")
        if Decimal(pa.amount) > self.max:
            raise ActError("over cap")
        try:
            if pa.kind == "transfer":
                res = self.bank.transfer(token, pa.from_account, pa.to_account, pa.amount,
                                         memo=pa.memo, idempotency_key=pa.id)
            elif pa.kind == "deposit":
                res = self.bank.deposit(token, pa.to_account, pa.amount, idempotency_key=pa.id)
            else:
                res = self.bank.withdraw(token, pa.from_account, pa.amount, idempotency_key=pa.id)
        except Exception as e:  # noqa: BLE001
            self._audit(customer_id, pa.kind, Decimal(pa.amount), "failed", str(e), action_id=pa.id)
            raise ActError(f"bank rejected: {e}") from e
        pa.status = "executed"
        pa.result = res
        self._audit(customer_id, pa.kind, Decimal(pa.amount), "executed", "", action_id=pa.id)
        return res

    def cancel(self, action_id, customer_id) -> dict:
        pa = self._pending.get(action_id)
        if pa is None or pa.customer_id != customer_id:
            raise ActError("unknown action")
        pa.status = "cancelled"
        self._audit(customer_id, pa.kind, Decimal(pa.amount), "cancelled", "", action_id=pa.id)
        return {"id": action_id, "status": "cancelled"}

    def get(self, action_id, customer_id):
        pa = self._pending.get(action_id)
        return asdict(pa) if pa and pa.customer_id == customer_id else None

    def _summary(self, pa: PendingAction) -> str:
        if pa.kind == "transfer":
            return f"Transfer {pa.amount} from {pa.from_account} to {pa.to_account}" + \
                   (f" ({pa.memo})" if pa.memo else "")
        if pa.kind == "deposit":
            return f"Deposit {pa.amount} into {pa.to_account}"
        return f"Withdraw {pa.amount} from {pa.from_account}"

    def _audit(self, customer_id, kind, amount, outcome, reason, action_id=None):
        self.audit.record({"customer_id": customer_id, "kind": kind, "amount": str(amount),
                           "outcome": outcome, "reason": reason, "action_id": action_id})
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest agent/tests/test_actions.py -q`
Expected: PASS (6 passed)

- [ ] **Step 5: Commit**

```bash
git add agent/actions.py agent/tests/test_actions.py
git commit -m "feat(agent): two-phase act engine (propose/execute/cancel) + guardrails"
```

---

### Task 7: MCP server + customer/token binding

**Files:**
- Create: `agent/mcp_server.py`
- Test: `agent/tests/test_mcp_binding.py`

**Interfaces:**
- Consumes: `db.ClientContext`, `memory.QdrantMemory`, `memory.AuditLog`, `actions.ActionStore`, `Settings`.
- Produces:
  - Context vars + helpers: `current_customer() -> str`, `current_token() -> str | None`, and a `bind(customer_id, token)` contextmanager (used by tools + tests).
  - `BindMiddleware` (ASGI) that copies `X-Nano-Customer`/`X-Nano-Token` headers into the context vars per request.
  - `build_mcp(deps) -> FastMCP` registering tools: **LLM-safe** `get_profile`, `get_accounts`, `get_transactions`, `get_cards`, `recall`, `remember`, `propose_transfer`, `propose_deposit`, `propose_withdraw`; **confirm-only** `execute_action`, `cancel_action`. `deps` is a small struct `Deps(db, memory, audit, actions)`.
  - `LLM_TOOL_NAMES: frozenset[str]` and `CONFIRM_ONLY_TOOL_NAMES: frozenset[str]` (consumed by Task 8 to filter the agent's toolset).
  - `main()` entry that runs streamable-HTTP on port 8087.

- [ ] **Step 1: Write the failing test** (offline — exercises binding + a tool fn directly, no HTTP)

`agent/tests/test_mcp_binding.py`:

```python
import pytest
from agent import mcp_server as M


def test_current_customer_requires_binding():
    with pytest.raises(LookupError):
        M.current_customer()


def test_bind_sets_and_clears():
    with M.bind("cust-1", "tok-1"):
        assert M.current_customer() == "cust-1"
        assert M.current_token() == "tok-1"
    with pytest.raises(LookupError):
        M.current_customer()


def test_tool_partitions_are_disjoint_and_exclude_execute_from_llm():
    assert "execute_action" in M.CONFIRM_ONLY_TOOL_NAMES
    assert "execute_action" not in M.LLM_TOOL_NAMES
    assert "cancel_action" not in M.LLM_TOOL_NAMES
    assert M.LLM_TOOL_NAMES.isdisjoint(M.CONFIRM_ONLY_TOOL_NAMES)
    # no LLM tool name hints at a customer/token parameter
    assert all("customer" not in n for n in M.LLM_TOOL_NAMES)
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest agent/tests/test_mcp_binding.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'agent.mcp_server'`

- [ ] **Step 3: Write `agent/mcp_server.py`**

```python
from __future__ import annotations
import contextlib
import contextvars
from dataclasses import dataclass

from mcp.server.fastmcp import FastMCP

from .config import Settings
from .db import ClientContext
from .memory import QdrantMemory, AuditLog
from .actions import ActionStore, ActDenied, ActError

_CUSTOMER: contextvars.ContextVar[str] = contextvars.ContextVar("nano_customer")
_TOKEN: contextvars.ContextVar[str] = contextvars.ContextVar("nano_token")

LLM_TOOL_NAMES = frozenset({
    "get_profile", "get_accounts", "get_transactions", "get_cards",
    "recall", "remember", "propose_transfer", "propose_deposit", "propose_withdraw"})
CONFIRM_ONLY_TOOL_NAMES = frozenset({"execute_action", "cancel_action"})


def current_customer() -> str:
    try:
        return _CUSTOMER.get()
    except LookupError:
        raise LookupError("no customer bound to this MCP request")


def current_token():
    return _TOKEN.get(None)


@contextlib.contextmanager
def bind(customer_id: str, token=None):
    t1 = _CUSTOMER.set(customer_id)
    t2 = _TOKEN.set(token)
    try:
        yield
    finally:
        _CUSTOMER.reset(t1)
        _TOKEN.reset(t2)


class BindMiddleware:
    """ASGI middleware: copy trusted headers into the context vars per request."""
    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope["type"] == "http":
            headers = {k.decode().lower(): v.decode() for k, v in scope.get("headers", [])}
            cust = headers.get("x-nano-customer")
            tok = headers.get("x-nano-token")
            if cust:
                c1 = _CUSTOMER.set(cust)
                c2 = _TOKEN.set(tok)
                try:
                    await self.app(scope, receive, send)
                finally:
                    _CUSTOMER.reset(c1)
                    _TOKEN.reset(c2)
                return
        await self.app(scope, receive, send)


@dataclass
class Deps:
    db: ClientContext
    memory: QdrantMemory
    audit: AuditLog
    actions: ActionStore


def build_mcp(deps: Deps) -> FastMCP:
    mcp = FastMCP("nano-manager")

    @mcp.tool()
    def get_profile() -> dict:
        """The bound client's profile."""
        return deps.db.profile(current_customer()) or {}

    @mcp.tool()
    def get_accounts() -> list:
        """The bound client's accounts and balances."""
        return deps.db.accounts(current_customer())

    @mcp.tool()
    def get_transactions(limit: int = 20) -> list:
        """The bound client's recent transactions."""
        return deps.db.transactions(current_customer(), limit=limit)

    @mcp.tool()
    def get_cards() -> list:
        """The bound client's credit-card accounts."""
        return deps.db.cards(current_customer())

    @mcp.tool()
    def recall(query: str, k: int = 3) -> list:
        """Recall durable memories about the bound client."""
        return deps.memory.recall(query, current_customer(), k=k)

    @mcp.tool()
    def remember(fact: str, kind: str = "observation") -> str:
        """Store a durable memory about the bound client."""
        return deps.memory.store(fact, customer_id=current_customer(), kind=kind)

    def _propose(kind, **kw):
        try:
            return deps.actions.propose(current_customer(), current_token(), kind, **kw)
        except ActDenied as e:
            return {"denied": True, "reason": str(e)}

    @mcp.tool()
    def propose_transfer(to_account: str, amount: str, from_account: str, memo: str = "") -> dict:
        """Propose a transfer from one of the client's accounts. Requires confirmation."""
        return _propose("transfer", amount=amount, from_account=from_account,
                        to_account=to_account, memo=memo or None)

    @mcp.tool()
    def propose_deposit(to_account: str, amount: str) -> dict:
        """Propose a deposit into one of the client's accounts. Requires confirmation."""
        return _propose("deposit", amount=amount, to_account=to_account)

    @mcp.tool()
    def propose_withdraw(from_account: str, amount: str) -> dict:
        """Propose a withdrawal from one of the client's accounts. Requires confirmation."""
        return _propose("withdraw", amount=amount, from_account=from_account)

    # --- confirm-only (never bound to the agent's toolset) -------------------
    @mcp.tool()
    def execute_action(action_id: str) -> dict:
        """Execute a previously proposed action. Confirm-path only."""
        try:
            return deps.actions.execute(action_id, current_customer(), current_token())
        except ActError as e:
            return {"error": str(e)}

    @mcp.tool()
    def cancel_action(action_id: str) -> dict:
        """Cancel a pending action. Confirm-path only."""
        try:
            return deps.actions.cancel(action_id, current_customer())
        except ActError as e:
            return {"error": str(e)}

    return mcp


def build_deps(settings: Settings) -> Deps:
    from decimal import Decimal
    db = ClientContext(settings.db)
    memory = QdrantMemory.from_settings(settings)
    audit = AuditLog.from_settings(settings)
    from .bank import BankClient
    actions = ActionStore(db, BankClient(settings.nano_bank_api), audit,
                          max_per_tx=settings.act_max_per_tx, ttl_s=settings.confirm_ttl_s)
    return Deps(db=db, memory=memory, audit=audit, actions=actions)


def main():
    settings = Settings.from_env()
    mcp = build_mcp(build_deps(settings))
    app = BindMiddleware(mcp.streamable_http_app())
    import uvicorn
    uvicorn.run(app, host="0.0.0.0", port=8087)


if __name__ == "__main__":
    main()
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest agent/tests/test_mcp_binding.py -q`
Expected: PASS (3 passed)

> Note: `FastMCP.streamable_http_app()` / `mcp.tool()` names come from the installed `mcp` SDK; if `streamable_http_app` differs, consult `python -c "import mcp.server.fastmcp as f; help(f.FastMCP)"`. The header-binding contract (context vars set from `X-Nano-Customer`/`X-Nano-Token`) is what the tests pin and must not change.

- [ ] **Step 5: Commit**

```bash
git add agent/mcp_server.py agent/tests/test_mcp_binding.py
git commit -m "feat(agent): MCP server (read+rag+propose LLM-safe, execute confirm-only) + header binding"
```

---

### Task 8: Manager core (`nano_manager.py`) — graph + MCP client + assist()

**Files:**
- Create: `agent/nano_manager.py`
- Test: `agent/tests/test_nano_manager.py`

**Interfaces:**
- Consumes: `model_factory.llm/init_models`, `mcp_server.LLM_TOOL_NAMES`, `langchain-mcp-adapters`.
- Produces:
  - `MANAGER_PROMPT: str`.
  - `agent_tools(all_tools) -> list` — filters MCP tools to `LLM_TOOL_NAMES` (defensive: guarantees `execute_action`/`cancel_action` are never given to the LLM).
  - `async assist(settings, customer_id, token, message, thread_id=None) -> dict` → `{"answer", "thread_id", "pending_action"?}`. Opens a per-request MCP client session with headers `X-Nano-Customer`/`X-Nano-Token`, injects snapshot+recall via a context hook, runs the ReAct agent, stores turn memories, and surfaces any `pending_action` returned by a `propose_*` tool call in the trace.

- [ ] **Step 1: Write the failing test** (offline — only the pure `agent_tools` filter is unit-tested; the full run is `@live`)

`agent/tests/test_nano_manager.py`:

```python
from agent import nano_manager as NM


class _T:
    def __init__(self, name): self.name = name


def test_agent_tools_excludes_execute_and_cancel():
    tools = [_T("get_accounts"), _T("propose_transfer"), _T("execute_action"),
             _T("cancel_action"), _T("recall")]
    kept = {t.name for t in NM.agent_tools(tools)}
    assert "execute_action" not in kept and "cancel_action" not in kept
    assert {"get_accounts", "propose_transfer", "recall"} <= kept


def test_manager_prompt_mentions_read_and_confirm():
    p = NM.MANAGER_PROMPT.lower()
    assert "confirm" in p and ("never fabricate" in p or "do not fabricate" in p)
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest agent/tests/test_nano_manager.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'agent.nano_manager'`

- [ ] **Step 3: Write `agent/nano_manager.py`**

```python
from __future__ import annotations
import uuid
from typing import Optional

from langchain_core.messages import AIMessage, HumanMessage, SystemMessage
from langgraph.prebuilt import create_react_agent
from langgraph.checkpoint.memory import InMemorySaver

from .config import Settings
from . import model_factory as mf
from .mcp_server import LLM_TOOL_NAMES

MANAGER_PROMPT = (
    "You are a careful personal banking manager for ONE client. Answer only from the "
    "client's real data (use your tools to look it up); never fabricate balances or "
    "transactions, and say plainly when you do not know. You may move money only when the "
    "client explicitly instructs it, and only via the propose_* tools — proposing does NOT "
    "move money; the client must CONFIRM the exact proposed action before it executes. Never "
    "claim a transfer is done from a proposal alone. Do not act proactively."
)


def agent_tools(all_tools):
    return [t for t in all_tools if getattr(t, "name", None) in LLM_TOOL_NAMES]


def _mcp_session(settings: Settings, customer_id: str, token: Optional[str]):
    """Per-request MCP client bound to a customer via trusted headers."""
    from langchain_mcp_adapters.client import MultiServerMCPClient
    return MultiServerMCPClient({
        "nano": {
            "url": settings.mcp_url,
            "transport": "streamable_http",
            "headers": {"X-Nano-Customer": customer_id, **({"X-Nano-Token": token} if token else {})},
        }
    })


async def assist(settings: Settings, customer_id: str, token: Optional[str],
                 message: str, thread_id: Optional[str] = None) -> dict:
    thread_id = thread_id or f"{customer_id}-{uuid.uuid4().hex[:6]}"
    client = _mcp_session(settings, customer_id, token)
    tools = agent_tools(await client.get_tools())

    # server-side snapshot + recall (code, not the LLM) -> a context system message
    async def _call(name, **kw):
        for t in await client.get_tools():
            if t.name == name:
                return await t.ainvoke(kw)
        return None
    snapshot = await _call("get_accounts")
    recalled = await _call("recall", query=message, k=4)
    context = SystemMessage(f"<client_snapshot>\n{snapshot}\n</client_snapshot>\n"
                            f"<durable_memory>\n{recalled}\n</durable_memory>")

    agent = create_react_agent(mf.llm("fast"), tools, prompt=MANAGER_PROMPT,
                               checkpointer=InMemorySaver())
    out = await agent.ainvoke(
        {"messages": [context, HumanMessage(message)]},
        config={"configurable": {"thread_id": thread_id}, "recursion_limit": 40})

    answer, pending = "(no answer)", None
    for m in reversed(out["messages"]):
        if isinstance(m, AIMessage) and (m.content or "").strip():
            answer = m.content
            break
    for m in out["messages"]:
        tc = getattr(m, "content", None)
        if isinstance(tc, str) and '"id"' in tc and "expires_at" in tc:
            import json
            try:
                obj = json.loads(tc)
                if isinstance(obj, dict) and obj.get("id") and not obj.get("denied"):
                    pending = obj
            except Exception:  # noqa: BLE001
                pass

    await _call("remember", fact=f"User asked: {message}", kind="user")
    await _call("remember", fact=f"Manager answered: {answer[:400]}", kind="assistant")
    res = {"answer": answer, "thread_id": thread_id}
    if pending:
        res["pending_action"] = pending
    return res
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest agent/tests/test_nano_manager.py -q`
Expected: PASS (2 passed)

> Note: `MultiServerMCPClient.get_tools()` and tool `.ainvoke` are from `langchain-mcp-adapters`; verify method names against the installed version (`python -c "import langchain_mcp_adapters.client as c; help(c.MultiServerMCPClient)"`). The `pending_action` scraping from the tool trace is a Phase-1 convenience; the authoritative pending state lives in the MCP `ActionStore`.

- [ ] **Step 5: Commit**

```bash
git add agent/nano_manager.py agent/tests/test_nano_manager.py
git commit -m "feat(agent): manager core — MCP-client wiring, context hook, assist()"
```

---

### Task 9: FastAPI Agentic-Branch endpoint

**Files:**
- Create: `agent/api.py`
- Test: `agent/tests/test_api.py`

**Interfaces:**
- Consumes: `Settings`, `nano_manager.assist`, an MCP client for the confirm path (`execute_action`/`cancel_action`), a `TokenResolver`.
- Produces:
  - `TokenResolver` protocol: `resolve(customer_id) -> str | None` (Phase 1: from seeded creds; injectable for tests).
  - `create_app(settings, assist_fn=..., confirm_fn=..., token_resolver=...) -> FastAPI` with:
    - `POST /branch/clients/{cid}/message` (Bearer `BRANCH_SERVICE_TOKEN`) → `{answer, thread_id, pending_action?}`
    - `POST /branch/clients/{cid}/actions/{aid}/confirm` → bank result / error
    - `POST /branch/clients/{cid}/actions/{aid}/cancel`
    - `GET  /branch/clients/{cid}/profile`
    - `GET  /health`
  - Dependency-injected `assist_fn`/`confirm_fn`/`token_resolver` so tests run without MCP/LLM.

- [ ] **Step 1: Write the failing test** (offline — FastAPI TestClient + injected fakes)

`agent/tests/test_api.py`:

```python
from fastapi.testclient import TestClient
from agent.config import Settings
from agent.api import create_app


def _app():
    settings = Settings.from_env({"BRANCH_SERVICE_TOKEN": "svc"})

    async def fake_assist(settings, cid, token, message, thread_id=None):
        return {"answer": f"hi {cid}", "thread_id": "th1",
                "pending_action": {"id": "act-1", "summary": "Transfer 50"}}

    async def fake_confirm(settings, cid, token, action_id, cancel=False):
        return {"status": "cancelled"} if cancel else {"transaction_id": "t1"}

    class R:
        def resolve(self, cid): return "jwt-" + cid

    return TestClient(create_app(settings, assist_fn=fake_assist,
                                 confirm_fn=fake_confirm, token_resolver=R()))


def test_message_requires_service_token():
    c = _app()
    r = c.post("/branch/clients/cust-1/message", json={"message": "hi"})
    assert r.status_code == 401


def test_message_returns_pending_action():
    c = _app()
    r = c.post("/branch/clients/cust-1/message", json={"message": "transfer 50"},
               headers={"Authorization": "Bearer svc"})
    assert r.status_code == 200
    assert r.json()["pending_action"]["id"] == "act-1"


def test_confirm_executes():
    c = _app()
    r = c.post("/branch/clients/cust-1/actions/act-1/confirm",
               headers={"Authorization": "Bearer svc"})
    assert r.status_code == 200 and r.json()["transaction_id"] == "t1"


def test_health_ok():
    assert _app().get("/health").status_code == 200
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest agent/tests/test_api.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'agent.api'`

- [ ] **Step 3: Write `agent/api.py`**

```python
from __future__ import annotations
from typing import Optional, Protocol

from fastapi import FastAPI, Header, HTTPException
from pydantic import BaseModel

from .config import Settings
from . import nano_manager


class TokenResolver(Protocol):
    def resolve(self, customer_id: str) -> Optional[str]: ...


class SeedTokenResolver:
    """Phase-1 resolver: logs into nano-bank with seeded creds (customer_id -> creds)."""
    def __init__(self, settings: Settings, creds: dict):
        self.settings = settings
        self.creds = creds  # customer_id -> (email, password)
        self._cache: dict = {}

    def resolve(self, customer_id: str) -> Optional[str]:
        if customer_id in self._cache:
            return self._cache[customer_id]
        cred = self.creds.get(customer_id)
        if not cred:
            return None
        from .bank import BankClient
        tok = BankClient(self.settings.nano_bank_api).login(*cred)
        self._cache[customer_id] = tok
        return tok


class MessageIn(BaseModel):
    message: str
    thread_id: Optional[str] = None


async def _default_confirm(settings, customer_id, token, action_id, cancel=False):
    """Reach execute_action/cancel_action directly over MCP — never through the LLM."""
    client = nano_manager._mcp_session(settings, customer_id, token)
    name = "cancel_action" if cancel else "execute_action"
    for t in await client.get_tools():
        if t.name == name:
            return await t.ainvoke({"action_id": action_id})
    raise HTTPException(500, "confirm tool unavailable")


def create_app(settings: Settings, *, assist_fn=nano_manager.assist,
               confirm_fn=_default_confirm, token_resolver: Optional[TokenResolver] = None) -> FastAPI:
    app = FastAPI(title="nano-bank personal manager")

    def _auth(authorization: Optional[str]):
        expected = f"Bearer {settings.branch_service_token}"
        if not settings.branch_service_token or authorization != expected:
            raise HTTPException(401, "invalid service token")

    def _token(cid: str) -> Optional[str]:
        return token_resolver.resolve(cid) if token_resolver else None

    @app.get("/health")
    def health():
        return {"status": "ok"}

    @app.get("/branch/clients/{cid}/profile")
    async def profile(cid: str, authorization: str = Header(None)):
        _auth(authorization)
        client = nano_manager._mcp_session(settings, cid, _token(cid))
        for t in await client.get_tools():
            if t.name == "get_profile":
                return await t.ainvoke({})
        raise HTTPException(500, "profile tool unavailable")

    @app.post("/branch/clients/{cid}/message")
    async def message(cid: str, body: MessageIn, authorization: str = Header(None)):
        _auth(authorization)
        return await assist_fn(settings, cid, _token(cid), body.message, body.thread_id)

    @app.post("/branch/clients/{cid}/actions/{aid}/confirm")
    async def confirm(cid: str, aid: str, authorization: str = Header(None)):
        _auth(authorization)
        return await confirm_fn(settings, cid, _token(cid), aid, cancel=False)

    @app.post("/branch/clients/{cid}/actions/{aid}/cancel")
    async def cancel(cid: str, aid: str, authorization: str = Header(None)):
        _auth(authorization)
        return await confirm_fn(settings, cid, _token(cid), aid, cancel=True)

    return app
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest agent/tests/test_api.py -q`
Expected: PASS (4 passed)

- [ ] **Step 5: Commit**

```bash
git add agent/api.py agent/tests/test_api.py
git commit -m "feat(agent): FastAPI Agentic-Branch endpoint (message/confirm/cancel/profile/health)"
```

---

### Task 10: Dev seeding (`seed.py`)

**Files:**
- Create: `agent/seed.py`
- Test: `agent/tests/test_seed.py`

**Interfaces:**
- Consumes: `bank.BankClient`.
- Produces:
  - `seed_customer(bank, *, first, last, email, password, ...) -> dict` → `{"customer_id","email","password"}` (registers customer + credentials).
  - `open_account(bank, token, customer_id, account_type="chequing") -> dict` → `{"account_id",...}`.
  - `fund(bank, token, account_id, amount) -> dict` (deposit).
  - `CredStore` — an in-memory `dict[customer_id] -> (email, password)` consumed by `api.SeedTokenResolver`.
  - `seed_demo(bank) -> dict` composing the above into two customers + accounts + a funding deposit (used by the console/integration).

- [ ] **Step 1: Write the failing test** (offline — fake bank)

`agent/tests/test_seed.py`:

```python
from agent import seed


class FakeBank:
    def create_customer(self, payload):
        return {"customer_id": "c-" + payload["email"]}
    def login(self, email, password): return "jwt-" + email
    def create_account(self, token, payload):
        return {"account_id": "a-" + payload["customer_id"]}
    def deposit(self, token, account_id, amount, idempotency_key=None):
        return {"transaction_id": "d1", "amount": str(amount)}


def test_seed_customer_records_creds():
    bank, store = FakeBank(), seed.CredStore()
    out = seed.seed_customer(bank, store, first="Ada", last="L",
                             email="ada@x.ca", password="pw123456")
    assert out["customer_id"] == "c-ada@x.ca"
    assert store.get("c-ada@x.ca") == ("ada@x.ca", "pw123456")


def test_seed_demo_creates_two_customers_and_funds():
    bank = FakeBank()
    out = seed.seed_demo(bank)
    assert len(out["customers"]) == 2
    assert out["customers"][0]["account_id"].startswith("a-")
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest agent/tests/test_seed.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'agent.seed'`

- [ ] **Step 3: Write `agent/seed.py`**

```python
from __future__ import annotations
from datetime import date


class CredStore:
    def __init__(self):
        self._d: dict = {}
    def put(self, customer_id, email, password):
        self._d[customer_id] = (email, password)
    def get(self, customer_id):
        return self._d.get(customer_id)
    def as_dict(self):
        return dict(self._d)


def seed_customer(bank, store: CredStore, *, first, last, email, password,
                  dob="1990-01-01") -> dict:
    out = bank.create_customer({
        "first_name": first, "last_name": last, "email": email,
        "phone_number": "+15550100000", "date_of_birth": dob, "password": password})
    cid = out["customer_id"]
    store.put(cid, email, password)
    return {"customer_id": cid, "email": email, "password": password}


def open_account(bank, token, customer_id, account_type="chequing") -> dict:
    return bank.create_account(token, {"customer_id": customer_id,
                                       "account_type": account_type})


def fund(bank, token, account_id, amount) -> dict:
    return bank.deposit(token, account_id, str(amount))


def seed_demo(bank) -> dict:
    store = CredStore()
    customers = []
    for i, (first, email) in enumerate([("Ada", "ada@x.ca"), ("Bo", "bo@x.ca")]):
        c = seed_customer(bank, store, first=first, last="Demo", email=email,
                          password="pw12345678")
        token = bank.login(email, "pw12345678")
        acc = open_account(bank, token, c["customer_id"])
        if i == 0:
            fund(bank, token, acc["account_id"], "1000")
        customers.append({**c, "account_id": acc["account_id"]})
    return {"customers": customers, "creds": store.as_dict()}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest agent/tests/test_seed.py -q`
Expected: PASS (2 passed)

> Note: customer/account payload fields must match `api/src/handlers/customers.rs` / `accounts.rs`; adjust the dict keys after checking those handlers. The tests pin the seed *flow*, not the exact bank schema.

- [ ] **Step 5: Commit**

```bash
git add agent/seed.py agent/tests/test_seed.py
git commit -m "feat(agent): dev seeding (customer/account/fund) + CredStore"
```

---

### Task 11: Streamlit test console

**Files:**
- Create: `agent/test_console.py`
- Test: (manual) — no unit test; verified by launch in Task 14 / smoke.

**Interfaces:**
- Consumes: `Settings`, the running `api` (`/message`, `/actions/*`, `/profile`) via `httpx`, and `seed.seed_demo` via the running `api` host or a direct `BankClient`.

- [ ] **Step 1: Write `agent/test_console.py`**

```python
from __future__ import annotations
import httpx
import streamlit as st

from agent.config import Settings

settings = Settings.from_env()
API = f"http://localhost:{settings.branch_port}"
HDR = {"Authorization": f"Bearer {settings.branch_service_token}"}

st.set_page_config(page_title="nano-bank manager — test console", layout="wide")
st.title("nano-bank personal manager — test console")

seed_col, chat_col = st.columns([1, 2])

with seed_col:
    st.subheader("Seed")
    if st.button("Seed demo (2 customers + funded account)"):
        from agent.bank import BankClient
        from agent import seed
        out = seed.seed_demo(BankClient(settings.nano_bank_api))
        st.session_state["customers"] = out["customers"]
        st.success(f"seeded {len(out['customers'])} customers")
    customers = st.session_state.get("customers", [])
    cid = st.selectbox("client", [c["customer_id"] for c in customers]) if customers else \
        st.text_input("client id")
    if cid and st.button("Load snapshot"):
        r = httpx.get(f"{API}/branch/clients/{cid}/profile", headers=HDR)
        st.json(r.json())

with chat_col:
    st.subheader("Chat")
    msg = st.text_input("Ask or instruct (e.g. 'transfer 50 from <acc> to <acc>')")
    if st.button("Send") and cid and msg:
        r = httpx.post(f"{API}/branch/clients/{cid}/message",
                       json={"message": msg}, headers=HDR, timeout=120)
        data = r.json()
        st.markdown(f"**Manager:** {data.get('answer','')}")
        pa = data.get("pending_action")
        if pa:
            st.warning(f"Proposed: {pa.get('summary', pa)}")
            c1, c2 = st.columns(2)
            if c1.button("Confirm"):
                rr = httpx.post(f"{API}/branch/clients/{cid}/actions/{pa['id']}/confirm",
                                headers=HDR, timeout=120)
                st.success(rr.json())
            if c2.button("Cancel"):
                rr = httpx.post(f"{API}/branch/clients/{cid}/actions/{pa['id']}/cancel",
                                headers=HDR, timeout=120)
                st.info(rr.json())
```

- [ ] **Step 2: Syntax-check (no server needed)**

Run: `python -m py_compile agent/test_console.py`
Expected: no output (exit 0)

- [ ] **Step 3: Commit**

```bash
git add agent/test_console.py
git commit -m "feat(agent): Streamlit test console (seed + chat + confirm/cancel)"
```

---

### Task 12: Full-suite green + live integration harness (opt-in)

**Files:**
- Create: `agent/tests/test_integration_live.py`
- Modify: `agent/tests/conftest.py` (add a `--run-live` opt-in)

**Interfaces:**
- Consumes: everything above. These tests are marked `@pytest.mark.live` and skipped unless `--run-live` is passed (they need Postgres + nano-bank + Ollama + Qdrant running).

- [ ] **Step 1: Add the opt-in to `conftest.py`**

Append to `agent/tests/conftest.py`:

```python
def pytest_addoption(parser):
    parser.addoption("--run-live", action="store_true", default=False,
                     help="run @live tests that need external services")


def pytest_collection_modifyitems(config, items):
    if config.getoption("--run-live"):
        return
    skip = pytest.mark.skip(reason="needs --run-live")
    for item in items:
        if "live" in item.keywords:
            item.add_marker(skip)
```

- [ ] **Step 2: Write the live integration test**

`agent/tests/test_integration_live.py`:

```python
import asyncio
import pytest
from agent.config import Settings
from agent.bank import BankClient
from agent import seed, model_factory as mf, nano_manager

pytestmark = pytest.mark.live


def test_two_phase_transfer_end_to_end():
    settings = Settings.from_env()
    mf.init_models(settings)
    bank = BankClient(settings.nano_bank_api)
    demo = seed.seed_demo(bank)
    ada, bo = demo["customers"]

    # ask -> cites a balance
    r1 = asyncio.run(nano_manager.assist(settings, ada["customer_id"],
        bank.login(ada["email"], ada["password"]), "what is my balance?"))
    assert "answer" in r1

    # instruct transfer -> pending_action, money NOT moved yet
    tok = bank.login(ada["email"], ada["password"])
    r2 = asyncio.run(nano_manager.assist(settings, ada["customer_id"], tok,
        f"transfer 25 from {ada['account_id']} to {bo['account_id']}"))
    assert r2.get("pending_action"), "manager must propose, not auto-execute"
```

- [ ] **Step 3: Run the full offline suite (live skipped)**

Run: `python -m pytest agent -q`
Expected: PASS — all offline tests green; live tests reported as skipped.

- [ ] **Step 4: Commit**

```bash
git add agent/tests/test_integration_live.py agent/tests/conftest.py
git commit -m "test(agent): opt-in live two-phase integration harness + suite green"
```

---

### Task 13: Containerization (compose + Containerfiles + run script)

**Files:**
- Create: `agent/Containerfile.api`, `agent/Containerfile.mcp`, `agent/Containerfile.console`, `agent/compose.yaml`, `agent/run-agent.sh`

**Interfaces:**
- Produces a project-local stack: `qdrant` (unpublished), `mcp` (unpublished), `api` (published `:8086`), `console` (published `:8505`), on a private network. Only `api`/`console` map ports.

- [ ] **Step 1: Write `agent/Containerfile.mcp`**

```dockerfile
FROM python:3.12-slim
WORKDIR /app
COPY agent/requirements.txt /app/requirements.txt
RUN pip install --no-cache-dir -r requirements.txt
COPY agent /app/agent
ENV PYTHONUNBUFFERED=1
CMD ["python", "-m", "agent.mcp_server"]
```

- [ ] **Step 2: Write `agent/Containerfile.api`**

```dockerfile
FROM python:3.12-slim
WORKDIR /app
COPY agent/requirements.txt /app/requirements.txt
RUN pip install --no-cache-dir -r requirements.txt
COPY agent /app/agent
ENV PYTHONUNBUFFERED=1
CMD ["python", "-c", "import uvicorn,os; from agent.config import Settings; from agent.api import create_app; from agent.api import SeedTokenResolver; s=Settings.from_env(); uvicorn.run(create_app(s), host='0.0.0.0', port=s.branch_port)"]
```

> Note: the API container must call `model_factory.init_models(settings)` at startup; add a small `agent/api_main.py` if the inline `-c` grows — for now `assist` calls `mf.llm` which requires init, so set `CMD` to run a module that inits models then serves. Replace the `-c` with `python -m agent.api_main` and create `agent/api_main.py`:

```python
# agent/api_main.py
import uvicorn
from agent.config import Settings
from agent import model_factory as mf
from agent.api import create_app

if __name__ == "__main__":
    s = Settings.from_env()
    mf.init_models(s)
    uvicorn.run(create_app(s), host="0.0.0.0", port=s.branch_port)
```

Set `Containerfile.api` `CMD ["python", "-m", "agent.api_main"]`.

- [ ] **Step 3: Write `agent/Containerfile.console`**

```dockerfile
FROM python:3.12-slim
WORKDIR /app
COPY agent/requirements.txt /app/requirements.txt
RUN pip install --no-cache-dir -r requirements.txt
COPY agent /app/agent
ENV PYTHONUNBUFFERED=1
CMD ["streamlit", "run", "agent/test_console.py", "--server.port=8505", "--server.address=0.0.0.0"]
```

- [ ] **Step 4: Write `agent/compose.yaml`**

```yaml
services:
  qdrant:
    image: qdrant/qdrant:latest
    networks: [nano]
    # no published ports — reachable only in-network
  mcp:
    build: { context: .., dockerfile: agent/Containerfile.mcp }
    environment:
      - QDRANT_URL=http://qdrant:6333
      - DB_HOST=host.containers.internal
      - NANO_BANK_API=http://host.containers.internal:8081
      - ACT_MAX_PER_TX=${ACT_MAX_PER_TX:-1000}
      - CONFIRM_TTL_S=${CONFIRM_TTL_S:-300}
    networks: [nano]
    depends_on: [qdrant]
    # no published ports
  api:
    build: { context: .., dockerfile: agent/Containerfile.api }
    environment:
      - MCP_URL=http://mcp:8087/mcp
      - OLLAMA_API_KEY=${OLLAMA_API_KEY}
      - OLLAMA_BASE_URL=${OLLAMA_BASE_URL:-https://ollama.com/v1}
      - MANAGER_MODEL=${MANAGER_MODEL:-glm-5.2}
      - MANAGER_FALLBACK_MODEL=${MANAGER_FALLBACK_MODEL:-glm-4.7}
      - BRANCH_SERVICE_TOKEN=${BRANCH_SERVICE_TOKEN}
      - NANO_BANK_API=http://host.containers.internal:8081
      - DB_HOST=host.containers.internal
    ports: ["8086:8086"]
    networks: [nano]
    depends_on: [mcp]
  console:
    build: { context: .., dockerfile: agent/Containerfile.console }
    environment:
      - BRANCH_PORT=8086
      - BRANCH_SERVICE_TOKEN=${BRANCH_SERVICE_TOKEN}
      - NANO_BANK_API=http://host.containers.internal:8081
    ports: ["8505:8505"]
    networks: [nano]
    depends_on: [api]

networks:
  nano: {}
```

- [ ] **Step 5: Write `agent/run-agent.sh`**

```bash
#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
# Requires: nano-bank API on :8081 and its Kind Postgres port-forward on the host.
podman compose -f compose.yaml up --build "$@"
```

Make it executable: `chmod +x agent/run-agent.sh`.

- [ ] **Step 6: Validate compose config (no build needed)**

Run: `cd agent && podman compose -f compose.yaml config >/dev/null && echo OK`
Expected: `OK` (compose file parses). If `podman compose` is unavailable, `docker compose -f compose.yaml config`.

- [ ] **Step 7: Commit**

```bash
git add agent/Containerfile.* agent/compose.yaml agent/run-agent.sh agent/api_main.py
git commit -m "feat(agent): containerize stack (qdrant+mcp unpublished, api+console published)"
```

---

### Task 14: README + docs + final verification

**Files:**
- Create: `agent/README.md`
- Modify: `CLAUDE.md` (add an `agent/` pointer under "Where things live")

**Interfaces:** none (docs).

- [ ] **Step 1: Write `agent/README.md`**

Cover: what it is (one-paragraph), the two-phase confirm guarantee, the customer-binding invariant, prerequisites (nano-bank on :8081 + Kind Postgres port-forward + `OLLAMA_API_KEY`), `cp .env.example .env` then `./run-agent.sh`, the console URL `http://localhost:8505`, the a2a endpoints (`/branch/clients/{id}/message`, `/actions/{id}/confirm`), and `python -m pytest agent -q` (add `--run-live` for the full path). Point to the spec and this plan.

- [ ] **Step 2: Add a pointer to `CLAUDE.md`**

Under "## Where things live", add:

```
- `agent/` — the Python **personal manager** (Phase 1): a GLM/Ollama-cloud LangGraph
  agent behind a customer-scoped MCP gateway (DB reads + Qdrant memory + two-phase,
  confirm-gated money movement), exposed as an agent-to-agent FastAPI endpoint (:8086)
  with a Streamlit test console (:8505). See `agent/README.md` and
  `docs/superpowers/specs/2026-07-07-personal-manager-design.md`.
```

- [ ] **Step 3: Run the whole offline suite once more**

Run: `python -m pytest agent -q`
Expected: PASS (all offline tests green; live skipped).

- [ ] **Step 4: Commit**

```bash
git add agent/README.md CLAUDE.md
git commit -m "docs(agent): README + CLAUDE.md pointer for the personal manager"
```

---

## Self-Review

**Spec coverage (spec §→task):**
- §3 layout / containerization → Tasks 1, 13.
- §4 model factory + resolver → Task 2.
- §5 agent core (persona, context hook, no fs/bash tools, thread_id) → Task 8.
- §6.1 tool schema (no customer/token param) → Tasks 7 (names) + 8 (filter test).
- §6.2 header binding, network isolation → Task 7 (`BindMiddleware`) + 13 (unpublished ports).
- §6.3 DB reads + snapshot + owns_account → Task 3.
- §6.4 Qdrant bi-temporal per-customer memory (not ragu) → Task 4.
- §6.5 two-phase act, token-bound, source guard, idempotency → Tasks 5, 6, 7, 9.
- §6.6 guardrails (mandatory confirm, cap, TTL, audit, explicit-only) → Tasks 4 (audit), 6, 8 (persona).
- §7.1 A2A endpoints incl. confirm/cancel → Task 9.
- §7.2 test console (seed + chat + confirm) → Tasks 10, 11.
- §8 data flow (propose in /message, execute in confirm) → Tasks 8, 9.
- §9 error handling (resolver fail, expired/consumed confirm, act refusals) → Tasks 2, 6, 9.
- §10 testing (isolation, resolver, scoping, act guardrails, two-phase e2e) → Tasks 2,4,6,7,8,12.
- §11 config → Task 1 + compose env (Task 13).
- Requirement 5 (Qdrant not ragu) → Task 4 dedicated collection/instance; Task 13 dedicated container.

**Placeholder scan:** no "TBD"/"handle edge cases"/"similar to Task N" — each step carries full code. The few `> Note:` blocks flag *live-wiring field names to verify against the Rust handlers*, not missing plan content; tests pin the contracts so a field-name fix is localized.

**Type consistency:** `ClientContext.owns_account/snapshot/accounts` (Task 3) are consumed unchanged by `ActionStore` (Task 6) and MCP tools (Task 7). `ActionStore.propose/execute/cancel` signatures (Task 6) match the MCP tool bodies (Task 7) and the confirm path (Task 9). `LLM_TOOL_NAMES`/`CONFIRM_ONLY_TOOL_NAMES` (Task 7) are consumed by `agent_tools` (Task 8). `assist(...) -> {answer, thread_id, pending_action?}` (Task 8) matches the API response and tests (Task 9). `CredStore` (Task 10) matches `SeedTokenResolver` (Task 9).

**Known live-wiring checks (localized, test-pinned):** nano-bank field names for login/transfer/customer/account (Tasks 5, 10); `mcp` SDK `streamable_http_app`/`tool` and `langchain-mcp-adapters` `get_tools`/`ainvoke` (Tasks 7, 8). Each has a `> Note:` with the exact `help(...)` probe.
