# Design: external mandated agent — single-door through the agentic branch

**Date:** 2026-07-11
**Status:** approved
**Repo:** `nano-bank` (branch `agent-k8s-e2e` / PR #22 — the leveling PR)

An **autonomous external LLM agent** operates a customer's bank **only through the
agentic branch**, under a **mandate** the customer granted. The person keeps full
first-party access via the UI (banking + a manager chatbox); the agent has no UI
and no direct route to the bank. Both talk to the **same** personal manager.

## Actors, doors, invariants

```
PERSON  ──UI (customer JWT)──▶ normal banking + manager chatbox ─┐
                                                                 ├─▶ PERSONAL MANAGER (shared)
EXTERNAL AGENT ──mandate──▶  AGENTIC BRANCH (:8086)  ───────────┘        │
 (autonomous LLM, no UI,        the ONLY door: agent auth + live-mandate  ▼
  no bank creds/URL)            PEP, dispatch to the manager        bank REST / agent-plane
```

- **Person** = UI, first-party, full banking + chatbox. Unchanged.
- **External agent** = agentic branch **only**, mandate-gated; same operation set a
  person has (open account, transactions, register Interac payee) **plus** A2A chat
  with the manager.
- **One personal manager**, shared between the person's chatbox and the agent's A2A.
- **No other path for the agent:** it holds no bank URL and no bank/agent
  credentials — only the branch's address + a branch session. The branch holds the
  agent credentials and is the sole thing that reaches the bank.

## Consent — reuse #19 (the bank is the source of truth)

- The customer **registers the agent** (`POST /api/v1/agents` → agent_id + secret,
  shown once) and **grants a mandate** (`POST /api/v1/mandates`: account, scopes,
  caps, expiry). `DELETE /api/v1/mandates/:id` revokes; the bank re-reads the
  mandate on every agent-plane call. `GET /mandates/:id/actions` is the audit trail.
- A demo seed helper performs register + grant so demo 4 starts from a live mandate.

## The branch as the agent's door + policy enforcement point (PEP)

New surface on `agent-api` (`agent/api.py`), separate from the person's `/branch/*`:

- **`/agent-gateway/*`** endpoints. The **branch holds the agent's credentials**
  (`AGENT_ID`/`AGENT_SECRET` via env/secret) and, **per request**, re-reads the
  agent's live mandate from the bank (`POST /api/v1/auth/agent-mandates`) — so
  revocation takes effect on the very next call.
- The branch **enforces the mandate** (active + scope + per-transfer cap) *before*
  acting, then **dispatches to the shared personal manager** (customer-scoped by the
  mandate's customer). The manager already has the full toolset
  (`open_account`, `register_interac_recipient`, `propose_transfer/deposit/withdraw`,
  advisory skills).
- **The mandate replaces the human confirm-gate for the agent path.** The mandate
  *is* the consent; the branch enforces caps/scope, so agent-initiated money
  movement executes within the mandate (no interactive Confirm button — that's the
  person's UI flow). Every agent action is recorded (mandate actions audit).
- Endpoints (shape finalized in the plan):
  - `POST /agent-gateway/session` — agent authenticates (agent creds) → the branch
    validates the mandate, returns a branch session bound to (agent, mandate,
    customer). No bank token ever leaves the branch.
  - `POST /agent-gateway/act` — a structured operation (e.g. `transfer_out`,
    `open_account`, `register_payee`) run through the manager, mandate-checked.
  - `POST /agent-gateway/message` — A2A natural-language turn to the manager
    (advisory + actions), mandate-scoped; returns the manager answer + run trace.

## The external autonomous agent

A small **autonomous LLM agent** (glm-5.2, reusing `agent/model_factory`) that takes
a **high-level instruction** from the user, plans, and calls its tools — which are
**only** the branch `/agent-gateway/*` endpoints (act + message). It never sees the
bank. It loops: instruction → (optional) A2A questions to the manager → mandate-scoped
actions → summary. Lives under `agent/external_agent/` (importable + a thin CLI).

## NetworkPolicy lockdown — path (A)

- **Primary (app-level):** the agent is *structurally* unable to reach the bank — it
  is given no bank URL and no bank/agent credentials; the branch holds them. Single
  door by construction.
- **Defense-in-depth (manifest):** ship a k8s `NetworkPolicy` (`k8s/` +
  `agent/k8s/`) that restricts ingress to `bank-api` (and especially the agent-plane)
  to the `agent-api`/manager pods only. **Caveat documented:** Kind's default
  `kindnet` CNI does not enforce NetworkPolicy, so the manifest is enforced only
  where a policy-capable CNI (e.g. Calico) runs; the app-level guarantee stands
  regardless. (Recreating the cluster with Calico was considered and declined — path B.)

## Demo 4 (new)

A **new** `demos/04-external-agent/` — the **external-agent console** (the person's
manager chat is now demo 3, `demos/03-manager-chat/`, unchanged; it reuses that
chat styling):
- Shows the mandate (account, scopes, cap, expiry) the customer granted.
- A single **high-level instruction** box; the autonomous agent runs and streams its
  steps: every hop through the branch, the **live-mandate check**, the manager's A2A
  answers, and the resulting actions — you (left) ↔ agent/manager (right) with the
  run trace inline (keep the demo-4 chat styling).
- The two demonstrated operations: **a transfer-out** (mandate-capped) and
  **"what are the benefits of a savings account?"** (A2A advisory).
- A **Revoke** button (calls `DELETE /mandates/:id`) → the next agent action is
  denied at the branch — proving live revocation.

## Testing

- **Branch PEP unit tests:** mandate active/expired/over-cap/out-of-scope →
  allow/deny; revocation between calls → deny (fake bank mandate source).
- **Agent loop unit test:** given a scripted instruction + fake gateway, it issues
  the expected act/message calls and stops.
- **Live e2e:** register agent + grant mandate → instruction "move $50 out and tell
  me about savings" → transfer-out within cap + savings advice via A2A → revoke →
  next action denied. Confirm the agent has no working bank path.

## Out of scope
- A unified person "banking + chatbox" single-page UI (the person path already
  exists across demos 1–3 + the bank consent app). Enforcing NetworkPolicy at L3
  (needs a policy CNI — path B, declined). Multi-agent / multi-mandate arbitration
  beyond selecting one mandate.

## Phasing (all in the leveling PR)
1. Branch `/agent-gateway/*`: agent auth + live-mandate PEP + dispatch to the manager.
2. NetworkPolicy manifests + keep the agent free of bank creds/URL (path A).
3. The autonomous external LLM agent (instruction → tools).
4. Demo 4 console + a seed helper (register agent + grant mandate) + a Revoke button.

## Relates to
- PR #19 mandate/agent-plane (`api/src/handlers/{agents,mandates,agent_api}.rs`,
  `mcp/nano_bank_agent_mcp.py`) — the consent substrate reused here.
- `docs/superpowers/specs/2026-07-11-demo3-agentic-manager-design.md` (the manager +
  trace this builds on).
