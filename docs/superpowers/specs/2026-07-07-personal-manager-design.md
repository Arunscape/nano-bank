# Nano-Bank Personal Manager — Design (Phase 1: read/advise + instructed act)

**Date:** 2026-07-07
**Status:** approved for spec review
**Scope:** Phase 1. Includes read/advise **and a minimal instructed-act path** (money
movement under simple guardrails). Full mandate-based governance (PR #19) and Proactive
monitoring are follow-on phases.

## 1. Purpose

An agentic **personal manager** for a nano-bank client. It knows everything about
one client, answers/advises about their banking, **and performs transactions on the
client's instruction** (transfer / deposit / withdrawal) under simple guardrails. It is
consumed as a **service endpoint** (the primary deliverable).

Two consumers, one core:
- **The Agentic Branch** — an agent-to-agent HTTP API that other autonomous agents call.
- **A test interface** — a first-class dev harness (§7.2) that (a) **seeds** customers,
  accounts and transactions directly against nano-bank, and (b) **chats** with the
  manager to ask information and instruct transactions. (The real human UI is a later,
  separate effort and is out of scope here.)

## 2. Requirements traceability

| Ask | Where it is satisfied |
|---|---|
| 1. Agentic personal manager | §5 agent core (ported `desktop_agent` harness) |
| 2. Knows everything about a client | §6 MCP server DB-read tools + server-side snapshot |
| 3. Accessed from UI and Agentic Branch | §7 FastAPI endpoint (primary) + test interface |
| 4. Harness similar to `desktop_agent.py` | §5 (managed ReAct agent, context hook, model factory) |
| 5. Local Qdrant RAG for interaction memories (not ragu) | §6 MCP RAG tools over a dedicated local Qdrant |
| 6. Ollama cloud backend, GLM5.2 → GLM4.7 fallback | §4 model factory + startup resolver |
| 7. Manager performs transactions on instruction | §6.5 act tools (nano-bank API) + §6.6 guardrails |
| 8. Test interface: seed customer/account/transactions + ask + instruct | §7.2 |

## 3. Layout

New directory in the nano-bank repo. The manager is a Python service (the repo's
Rust API stays untouched in Phase 1). **All components are containerized** and run as
a project-local stack (following the `testing/` harness's podman + Containerfile
pattern), including a **dedicated Qdrant container local to this project** (not ragu).

```
agent/
  mcp_server.py         # MCP server: the ONLY gateway to DB + RAG, customer-scoped IN CODE
  nano_manager.py       # agent core: model factory, LangGraph agent, MCP-client wiring, assist()
  api.py                # FastAPI — the PRIMARY deliverable (the manager endpoint)
  test_console.py       # Streamlit TEST interface: seed (customer/account/txns) + chat + instruct
  seed.py               # seeding helpers (create customer/account, fund, transfer) via nano-bank API
  Containerfile.api     # api.py (+ nano_manager) image
  Containerfile.mcp     # mcp_server.py image
  Containerfile.console # test_console.py image (test interface)
  compose.yaml          # api + mcp + qdrant (+ console); a project-local network
  requirements.txt
  .env.example
  README.md
  tests/                # unit + integration tests
```

Containers share a **private network**. Only `api` (and, in dev, `console`) publish ports
to the host; `mcp` and `qdrant` are reachable **only inside the network** (see §6.2).

## 4. Model factory (the one backend seam)

Ported from `desktop_agent._build_model` / `llm()`. Adds an `ollama_cloud` backend:

```python
from langchain_openai import ChatOpenAI
ChatOpenAI(base_url=OLLAMA_BASE_URL,   # https://ollama.com/v1
           api_key=OLLAMA_API_KEY, model=<resolved>, temperature=..., timeout=...)
```

**Startup model resolver.** A cheap 1-token probe of `MANAGER_MODEL` (default
`glm-5.2`). If it errors, fall back to `MANAGER_FALLBACK_MODEL` (default `glm-4.7`),
log which won, and cache the result. Every role (`reasoning` / `fast` / `summarizer`)
maps to the resolved id; both ids are env-overridable. If neither answers, startup
fails loudly (see §9).

The role-based `llm(role, reasoning=, temperature=, max_tokens=)` signature is kept
so the ported graph code reads identically to the source harness.

## 5. Agent core (`nano_manager.py`) — ported managed agent

`create_react_agent(llm, tools, prompt=MANAGER_PROMPT, pre_model_hook=context_hook,
checkpointer)`, following `desktop_agent.build_managed_agent`.

Deliberate divergences from `desktop_agent`:
- **No filesystem / bash / code tools.** They are unsafe and irrelevant for a banking
  agent. Phase-1 tools are exactly the customer-bound MCP tools from §6 (reads, RAG,
  and the act tools of §6.5).
- **Persona (`MANAGER_PROMPT`):** a careful personal banking manager that answers
  *only* from the client's real data, never fabricates balances or transactions, and
  says so plainly when it does not know. It **may move money only on an explicit client
  instruction**, only from the bound customer's own accounts, and only within the
  guardrails of §6.6 — it never initiates transfers on its own (proactive action is a
  later phase).

**Context hook** (ported `make_context_hook`): before the model call, it injects
(a) the client **snapshot** and (b) **recalled memories**, both obtained server-side
through the customer-bound MCP session (§6), and it bounds the message window. The
snapshot/recall are fetched by *code*, not by the LLM.

**Session identity.** `thread_id` is per (client, conversation). The LangGraph
checkpointer holds turn-by-turn state; long-term memory is the Qdrant RAG in §6.

**Entry point.** `assist(customer_id, message, thread_id) -> {answer, thread_id}`.
It opens a customer-bound MCP session (§6), loads snapshot + recall, runs the ReAct
loop, then stores the turn's memories through the same session. `customer_id` is a
parameter of this server-side function — it is **not** reachable by the LLM.

## 6. MCP server (`mcp_server.py`) — single customer-scoped gateway to DB + RAG + writes

Postgres reads, Qdrant memory, **and the nano-bank write API** all live behind **one MCP
server**. This is the security spine of the design.

### 6.1 Tools exposed to the LLM (no customer parameter)
- **DB-read (Postgres, read-only connection):**
  `get_profile()`, `get_accounts()`, `get_transactions(limit)`, `get_cards()`.
- **RAG (Qdrant):** `recall(query, k)`, `remember(fact, kind)`.
- **Act — propose only (nano-bank API, §6.5):** `propose_transfer(to_account, amount,
  memo?)`, `propose_deposit(to_account, amount)`, `propose_withdraw(from_account,
  amount)`. These **do not move money** — they record a pending action and return a
  confirmation id + human-readable summary for the user/caller to approve (§6.6).

None of these take a `customer_id` **or an auth token**. The agent therefore cannot
*express* access to — or action on — another customer. The scoping is absent from the
tool schema, not a prompt rule. For act tools the LLM supplies only the amount, the
destination, and *which of the bound customer's own accounts* to use; the owning
customer and the credential are bound server-side (§6.2, §6.5). Crucially, **no tool the
LLM can call executes a payment** — execution is a separate deterministic step gated on
explicit confirmation (§6.6), so the model can propose but never unilaterally act.

### 6.2 Customer binding — HTTP + trusted header (enforced in code)
- One long-running **streamable-HTTP** MCP server, **not published to the host** —
  reachable only by the `api` service over the private container network (§3).
- `api.py`, after authenticating the request (§7), opens a **per-request** MCP client
  session passing two **trusted headers** the LLM never sees or sets: `X-Nano-Customer`
  (the bound `customer_id`) and `X-Nano-Token` (that customer's nano-bank JWT, used only
  for act calls — §6.5).
- The MCP server reads those headers and stamps the bound `customer_id` into **every**
  SQL `WHERE customer_id = …` and **every** Qdrant payload filter, and uses the bound
  token for every act call. Any `customer_id`/token arriving in tool arguments is
  ignored — the headers are the sole source of truth.
- Trust boundary: **network isolation** — only the `api` container can reach the MCP
  container (the MCP port is unpublished). The LLM influences the server only through
  tool *arguments*, which carry no customer.
- **Hardening path (not Phase 1):** a stdio MCP server spawned per session with
  `NANO_CUSTOMER_ID` in its env gives per-process isolation; documented for later.

### 6.3 Data source: DB reads
A **read-only** psycopg2 connection reusing `testing/viewer`'s `DB_*` config
(`DB_HOST` default `::1`, etc.). Queries join a customer's rows across
`customers` / `customer_addresses` / `accounts` / `transactions` / `cards`(credit_card
accounts) / holds, always filtered by the bound `customer_id`.

`snapshot(customer_id)` composes profile + accounts/balances + recent transactions +
cards into a compact text digest, used by the context hook (§5). It is a server-side
call, not an LLM tool.

### 6.4 Data source: RAG memory (`QdrantMemory`, local, not ragu, bi-temporal)
Same interface as the harness's `BiTemporalMemory` (`store` / `invalidate` /
`query_valid` / `recall`), so the ported context hook is unchanged.

- **Store:** a **dedicated local Qdrant** — its own container on its own port
  (default `:6335`, **not** ragu's), collection `nano_manager_memory`. Embeddings via
  **fastembed / CPU**.
- **Point payload:** `{customer_id, kind, source, fact, valid_from, valid_to, thread_id}`.
- **Scoping:** `recall` / `query_valid` filter by `customer_id` **AND** `valid_to IS
  NULL`. Superseded facts are **invalidated (stamped `valid_to`), not deleted** — a
  guarantee the old Vertex RAG-Engine path could not keep.
- **What gets written each turn:** the user request, the assistant answer, and any
  salient facts — all through the customer-bound session.

### 6.5 Acting (writes) — two-phase, customer-token-bound
Money movement goes through the **existing authenticated nano-bank API** on `:8081`
(`POST /api/v1/transactions/{deposit,withdrawal,transfer}`), so all ledger triggers,
balance/limit checks and double-entry invariants hold — the manager never writes to
Postgres directly. It is **two-phase**, and the two phases have different callers:

1. **Propose (LLM-callable).** A `propose_*` tool (§6.1) validates the request against
   the guardrails (§6.6), records a **pending action** — `{id, customer_id, kind, from,
   to, amount, memo, created_at, expires_at, status=pending}` — and returns the id + a
   summary. **No bank call happens here.**
2. **Execute (NOT LLM-callable).** A separate server-side `execute_action(id)` — reached
   only via the deterministic confirm path (§7.1 endpoint / console button) — re-checks
   the pending action (unexpired, still within guardrails, still owned) and *then* calls
   nano-bank with the **stored** parameters. The MCP server exposes `execute_action` only
   to the confirm route, never as an LLM tool, so the model cannot self-confirm.

- **Auth is bound, not passed by the LLM.** The execute call uses the `X-Nano-Token`
  bound to the session (§6.2) — the seeded customer's nano-bank JWT, which nano-bank
  requires for money movement (PR #16 gated transfers behind customer auth). Because the
  token *is* the customer, the bank guarantees writes only touch that customer's
  accounts. The LLM can neither supply nor swap the token.
- **Source-account guard.** At both propose and execute, the MCP server verifies the
  `from`/`to` account belongs to the bound customer (via the §6.3 read path); a mismatch
  is refused and audited (§6.6) — defense-in-depth on top of the bank's own auth.
- **Idempotency.** Execution derives the bank `idempotency_key` from the pending-action
  `id`, so a double-confirm (or a retried confirm) cannot double-spend; an already-
  executed action returns its original result.

### 6.6 Guardrails (Phase 1) — confirmation is mandatory
Money movement by an AI needs a floor of governance even before the full mandate system
(§12). Phase 1:
- **Mandatory confirmation (both surfaces).** *Every* act requires an explicit
  confirmation of the exact proposed action before execution — for the human test
  console **and** for A2A callers alike. There is no "auto-execute" mode; the LLM's
  proposal is never sufficient on its own. Confirmation approves a specific pending-action
  `id`, so the amount/destination cannot change between propose and execute.
- **Pending-action TTL.** A proposal expires after `CONFIRM_TTL_S` (env); a stale or
  already-consumed id cannot be confirmed.
- **Amount cap.** `ACT_MAX_PER_TX` (env) refuses any single transaction above the cap at
  propose time (and re-checks at execute).
- **Append-only audit.** Every step — **propose, confirm/execute, and every deny** — is
  recorded (customer, kind, amount, destination, outcome, reason, timestamp) to an
  append-only store (a dedicated Qdrant collection / table). This is the vocabulary
  Phase 2's mandate `agent_actions` audit will subsume.
- **Explicit-instruction only.** `propose_*` tools fire only in response to a client
  instruction in the conversation; the manager does not act proactively (Phase 3).

## 7. Surfaces

### 7.1 Agentic Branch API (`api.py`, FastAPI) — primary
- `POST /branch/clients/{customer_id}/message` — body `{message, thread_id?}` →
  `{answer, thread_id, pending_action?}`. When the manager proposes a transaction, the
  response carries `pending_action = {id, kind, from, to, amount, memo, expires_at,
  summary}` and money has **not** moved yet.
- `POST /branch/clients/{customer_id}/actions/{action_id}/confirm` → executes the pending
  action (§6.5 step 2) and returns the bank result, or an error if expired/consumed/over
  guardrail. This is the **only** way an act completes — same for humans and agents.
- `POST /branch/clients/{customer_id}/actions/{action_id}/cancel` → discards a pending
  action (audited).
- `GET  /branch/clients/{customer_id}/profile` → the snapshot.
- `GET  /health` → resolver + Qdrant + Postgres status.

**Confirmation is symmetric across surfaces.** An A2A caller must make the explicit
`…/confirm` call (a deliberate second request), exactly as the human clicks Confirm in
the console — the two-phase design (§6.5) makes "confirm before acting" a property of the
protocol, not a UI nicety, so it cannot be skipped by any caller.
- **Auth (Phase 1):** deliberately simple — a shared `BRANCH_SERVICE_TOKEN` bearer for
  calling agents. The `customer_id` from the (authenticated) request binds the MCP
  session (§6.2).
- **Binding the customer's nano-bank token.** To let the manager *act*, `api.py` needs
  that customer's nano-bank JWT for `X-Nano-Token`. In Phase 1 (dev/test) it obtains one
  by logging into nano-bank with the **seeded credentials** (kept server-side, keyed by
  `customer_id`); it is never taken from the LLM or the request body. Callers that only
  need read/advise can omit it (act tools then refuse). This whole mechanism is what
  Phase 2 replaces with the agent-token→mandate flow.
- Default port `:8086`.

**Caller vs agent scoping.** The external caller still names `customer_id` in the URL;
proving a *caller* may act for that client is **Phase-2 caller-authorization**. Phase-1's
guarantee is narrower and enforced: the **LLM/agent** cannot deviate from the bound
customer.

**The real Agentic-Branch auth (PR #19, `agentic-banking`).** The mature auth model for
this surface is the mandate system already in flight: a customer-granted, scoped,
expiring, **revocable mandate** is the single source of truth; an agent presents a
5-minute **agent-token** that is a *pointer* to a mandate, and the bank re-reads the
mandate row on every call (immediate revocation, no blocklist). Notably the bank's own
agent read surface — `GET /api/v1/agent/account`, `GET /api/v1/agent/transactions` — is
**mandate-pinned with no account parameter**, which is the same "scoping outside the
agent" invariant as §6, enforced a layer deeper at the bank. Phase 2 replaces the shared
`BRANCH_SERVICE_TOKEN` with agent-token→mandate and shifts reads onto that mandate-pinned
surface (see §12).

### 7.2 Test interface (`test_console.py`, Streamlit) — first-class dev harness
A control panel to drive and demo the whole system end-to-end. It is a **client of
`api.py`** (chat) plus a thin **seeder** against nano-bank (`seed.py`), not a second
core. Default port `:8505`. Two panes:

1. **Seed** (direct nano-bank API calls via `seed.py`):
   - **Create customer** (fake Canadian identity + credentials, à la `testing/generator`).
   - **Open account(s)** for that customer.
   - **Create transactions** — fund via `deposit`, and optionally seed transfers, so
     there is real history and balances to talk about. Destination customers/accounts for
     later transfers can be seeded here too.
   - Shows the created `customer_id`(s) to pick for the chat pane.
2. **Chat with the manager** (via `POST /branch/clients/{id}/message`):
   - **Ask information** — "what's my balance?", "list my recent transactions".
   - **Instruct transactions** — "transfer $50 from my chequing to Alice", "deposit $200".
     The manager **proposes** (§6.5); the console renders the proposed action with
     **Confirm / Cancel** buttons that call the confirm/cancel endpoints. Money moves only
     after Confirm; the change is visible on refresh.
   - Sidebar shows the live snapshot and the act-audit trail (§6.6).

This is the surface that satisfies asks 7 and 8. It is a dev/test tool, not the eventual
production UI.

## 8. Data flow

```
POST /branch/clients/{id}/message   (BRANCH_SERVICE_TOKEN)
  → api.py authenticates, derives customer_id, resolves that customer's nano-bank JWT
  → assist(customer_id, message, thread_id):
       open MCP session bound to customer_id + token (X-Nano-Customer, X-Nano-Token)
       snapshot = MCP get_profile/accounts/transactions/cards   (server-side)
       memories = MCP recall(...)                                (server-side)
       context hook injects snapshot + memories, bounds window
       ReAct loop on GLM (ollama.com/v1):
         read tools     → Postgres (customer-filtered) via MCP
         propose_* tools → guardrails (cap/audit) → pending action recorded (NO bank call)
       MCP remember(request), remember(answer), remember(salient facts)
  → {answer, thread_id, pending_action?}      # if the manager proposed a transaction

# execution is a SEPARATE, deterministic request (human clicks Confirm / agent calls confirm):
POST /branch/clients/{id}/actions/{action_id}/confirm
  → api.py (customer+token bound) → MCP execute_action(id):
       re-check unexpired + guardrails + ownership
       → nano-bank API :8081 (bound token, idempotency_key = action id)
       → audit execute
  → {result}                                  # money has now moved
```

## 9. Error handling

- **Backend:** resolver picks glm-5.2/glm-4.7; if neither answers, startup fails with a
  clear message. Per-request model errors return a graceful reply, not a 500 stacktrace.
- **DB:** read-only connection; a tool failure returns an error string so the agent
  degrades (answers from memory/known context) rather than crashing.
- **Memory:** writes never raise (mirrors `desktop_agent.remember`).
- **Unknown customer_id:** `404` from the API / a notice in the test console.
- **MCP:** if the customer-bound session cannot open, the request fails closed (no
  unscoped access is ever attempted).
- **Act:** over-cap or an account not owned by the bound customer is refused at propose
  time and audited; a nano-bank rejection (insufficient funds, etc.) surfaces as a clear
  message, is audited, and never leaves the local view inconsistent (the bank is the
  source of truth).
- **Confirm:** an expired, cancelled, unknown, or already-executed `action_id` returns a
  clear error and moves no money (idempotent; the original result is returned for a
  duplicate confirm of an executed action).

## 10. Testing

- **Unit**
  - `QdrantMemory`: store → recall → invalidate; and **cross-customer isolation**
    (a fact stored for customer A is never recalled for customer B).
  - Model resolver: primary-ok, primary-fails-fallback-ok, both-fail (mocked probe).
  - MCP scoping: a tool call is answered only for the header-bound customer; a
    `customer_id` injected into tool args is ignored.
  - **Act guardrails:** over-cap refused; act tool naming an account not owned by the
    bound customer refused; both audited; token never sourced from tool args.
  - **Confirmation is mandatory:** a `propose_*` call moves no money and no `execute_action`
    tool is reachable by the LLM; only the confirm route executes; an expired/cancelled id
    will not execute.
  - The DB-read layer's SQL (§6.3) against a seeded test DB (or fixture).
- **Integration**
  - `seed.py` creates a customer + account + a funding deposit; `POST` to the a2a
    `/message` "what's my balance?" cites the real balance; memory persists across two
    calls (turn 2 recalls a turn-1 fact).
  - **Act end-to-end (two-phase):** seed two customers; instruct "transfer $X to <other>";
    assert the `/message` response is a `pending_action` and **balances are unchanged**;
    then `…/confirm` and assert balances moved by exactly $X and an audit row exists; a
    second `…/confirm` of the same id does **not** double-spend (idempotent).
- **Health:** `GET /health` (and a `--health` CLI) probe ollama-cloud + Qdrant + Postgres
  + nano-bank API.

## 11. Config (`.env.example`)

| Var | Default | Purpose |
|---|---|---|
| `OLLAMA_API_KEY` | — | ollama.com auth |
| `OLLAMA_BASE_URL` | `https://ollama.com/v1` | OpenAI-compat endpoint |
| `MANAGER_MODEL` | `glm-5.2` | primary model |
| `MANAGER_FALLBACK_MODEL` | `glm-4.7` | fallback model |
| `QDRANT_URL` | `http://qdrant:6333` (in-network); `http://localhost:6335` from host | project-local Qdrant container (not ragu) |
| `QDRANT_COLLECTION` | `nano_manager_memory` | memory collection |
| `DB_HOST/DB_PORT/DB_NAME/DB_USER/DB_PASSWORD` | viewer defaults, but see note | read-only DB access |
| `NANO_BANK_API` | `http://localhost:8081` (host) | act calls (§6.5) + seed login |
| `BRANCH_SERVICE_TOKEN` | — | a2a bearer for calling agents |
| `ACT_MAX_PER_TX` | e.g. `1000` | per-transaction amount cap (§6.6) |
| `CONFIRM_TTL_S` | `300` | how long a proposed action can wait for confirmation (§6.6) |
| `MCP_URL` | `http://mcp:8087/mcp` (in-network only) | unpublished MCP server |
| `BRANCH_PORT` / `CONSOLE_PORT` | `8086` / `8505` | API / test-console ports (published) |

The seeded customers' nano-bank credentials are held server-side by `api.py`/`seed.py`
(keyed by `customer_id`) to mint the `X-Nano-Token`; they are never exposed to the LLM.

**Container-networking note.** Inside the stack, services address each other by name
(`qdrant`, `mcp`) over the private network; only `api`/`console` publish to the host. The
nano-bank Postgres runs in Kind and is reached via a **host** `kubectl port-forward`, so
from inside a container `DB_HOST` is **`host.containers.internal`** (podman), *not* the
viewer's `::1` — the `::1` default only applies when running the manager directly on the
host. Same for `NANO_BANK_API`.

## 12. Out of scope (later phases)

Phase 1 already *acts* (§6.5) under simple guardrails and a seeded customer token. What
is deferred:

- **Phase 2 — real Agentic-Branch auth + governance hardening (builds on PR #19
  `agentic-banking`):**
  - Replace the shared `BRANCH_SERVICE_TOKEN` **and** the Phase-1 seeded-customer-token
    with the **mandate + agent-token** flow: the manager authenticates as a registered
    agent, exchanges its secret for a 5-minute pointer JWT, and operates strictly within
    a customer-granted, real-time-**revocable** mandate.
  - **Shift reads** from Phase-1's direct DB access onto the bank's **mandate-pinned
    agent surface** (`GET /api/v1/agent/account|transactions`, no account parameter) for
    defense-in-depth + append-only `agent_actions` audit.
  - **Move the act path** from the customer-authed `POST /api/v1/transactions/transfer`
    onto the mandate-scoped `POST /api/v1/agent/transfers`, whose caps/allowlist/
    idempotency are enforced by the bank's `policy.rs` — replacing our simple §6.6 cap.
  - Reconcile with PR #19's existing `mcp/` server (a thin MCP wrapper over the mandate
    agent API): our `mcp_server.py` adds the RAG memory + aggregated client snapshot;
    the two should converge rather than duplicate.
- **Phase 3 — Proactive:** a monitor scanning the client picture for signals
  (low balance, unusual activity) and surfacing alerts, and acting on its own.
- The real human-facing UI (this phase ships only the dev test console).
- stdio-per-session MCP isolation (§6.2 hardening path).
