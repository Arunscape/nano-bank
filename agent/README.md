# nano-bank personal manager (Phase 1)

An agentic **personal manager** for one nano-bank client. It knows everything about the
client (profile, accounts, balances, transactions, cards), answers and advises, and
**performs transactions on explicit instruction** behind a mandatory two-phase guardrail.
It runs on **GLM via Ollama-cloud** and is consumed as an **agent-to-agent HTTP endpoint**
(the "Agentic Branch"), with a Streamlit **test console** for driving it by hand.

## Key guarantees

- **The agent cannot pick the customer.** Every data/RAG/act tool is served by one MCP
  server; none of its tools take a `customer_id` or a token. The bound customer + the
  customer's nano-bank JWT come only from trusted transport headers
  (`X-Nano-Customer` / `X-Nano-Token`) that the LLM never sees.
- **Confirmation is mandatory, and it's a protocol property.** Money movement is
  two-phase: LLM-callable `propose_*` tools only record a *pending* action (no money
  moves); a separate, non-LLM `execute_action` runs only via an explicit `…/confirm`
  request — identically for the console and A2A callers. The model can propose but never
  self-confirm.
- **Writes go through the authenticated nano-bank API** (`:8081`), never direct DB — so
  ledger invariants hold. Reads come straight from Postgres (read-only).
- **Interac e-Transfers to saved payees.** The manager can register / list /
  remove saved Interac recipients by email (`register_interac_recipient`,
  `list_interac_recipients`, `remove_interac_recipient`) and send a confirm-gated
  `propose_interac_transfer(payee_email, amount, from_account, security_question,
  security_answer)` — which, on confirm, sends over the **real Interac rail**
  (`POST /api/v1/interac/etransfers`; security Q&A required unless the recipient
  has autodeposit). A payee must be saved first.
- **Memory is a dedicated local Qdrant** (collection `nano_manager_memory`), per-customer
  and bi-temporal (superseded facts are invalidated, not deleted). It is **not** ragu.

## Prerequisites

- The two Kind clusters up (see the repo-root `CLAUDE.md` + `deploy-all.sh`):
  cluster `nano-bank` (bank-api + Postgres + this agent stack) and cluster
  `modern-core` (the GL core).
- An `OLLAMA_API_KEY` for `https://ollama.com/v1` (models `glm-5.2` → `glm-4.7` fallback).
- `docker`, `kind`, `kubectl` (no podman).

## Run (in Kubernetes)

```bash
cd agent
cp .env.example .env          # fill OLLAMA_API_KEY and BRANCH_SERVICE_TOKEN
./k8s/deploy.sh               # builds+loads mcp/api/console, mints nano-agent-secrets
                              # from .env, applies qdrant/mcp/api/console to ns nano-bank
```

The whole stack (both clusters) comes up with the repo-root `../scripts/deploy-all.sh`.
The Secret is generated on apply from the **gitignored** `.env` — nothing sensitive
is committed. `agent-qdrant` and `agent-mcp` are ClusterIP-only (never published);
reach the published surfaces with `kubectl port-forward`:

```bash
kubectl -n nano-bank port-forward svc/agent-console 8505:8505   # Streamlit dashboard
kubectl -n nano-bank port-forward svc/agent-api     8086:8086   # Agentic Branch API
```

- Console <http://localhost:8505> — **Seed demo**, pick a client, then ask
  ("what's my balance?") or instruct ("transfer 25 from <acc> to <acc>") and **Confirm**;
  the Customer / Accounts / Recent-transactions panels refresh after a confirmed action.
- Agentic Branch API `http://localhost:8086` (all guarded by `Authorization: Bearer $BRANCH_SERVICE_TOKEN`):
  - `POST /branch/clients/{customer_id}/message` `{ "message": "..." }`
    → `{ answer, thread_id, pending_action? }`
  - `POST /branch/clients/{customer_id}/actions/{action_id}/confirm` → executes
  - `POST /branch/clients/{customer_id}/actions/{action_id}/cancel`
  - `GET  /branch/clients/{customer_id}/{profile,accounts,transactions}`, `GET /health`

## Tests

```bash
python -m venv .venv && .venv/bin/pip install -r requirements.txt
.venv/bin/python -m pytest agent -q            # offline suite (mocks + in-memory Qdrant)
../testing/e2e/e2e_test.sh                      # full in-cluster E2E (seed→ask→propose→confirm)
```

## Design & plan

- Spec: `../docs/superpowers/specs/2026-07-07-personal-manager-design.md`
- Plan: `../docs/superpowers/plans/2026-07-07-personal-manager-phase1.md`

Phase 2 (deferred) swaps the simple service token + seeded customer token for PR #19's
mandate + agent-token auth and moves reads/act onto the bank's mandate-pinned
`/api/v1/agent/*` surface. Phase 3 adds proactive monitoring.
