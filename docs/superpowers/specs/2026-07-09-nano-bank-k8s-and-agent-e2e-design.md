# Design: nano-bank fully in k8s (two isolated clusters) + agent E2E

**Date:** 2026-07-09
**Status:** approved (pending spec review)
**Repos touched:** `nano-bank` (this repo) and `nano-bank-modern-core` (peer repo)

## Goal

Run the **entire** nano-bank — the bank API, its GL core, and the agentic
personal manager — from Kubernetes, then prove the agent works end-to-end with
**MCP + RAG (Qdrant) + a remote Ollama model + the Streamlit console** all
in-cluster. Today only the bank's Postgres runs in Kind; the bank API and both
GL cores run as host processes / loose Docker containers. This migrates the bank
into the cluster, decouples the core into its own cluster, and replaces the
agent's podman-compose deployment with k8s manifests.

## Governing principles

1. **Total isolation.** This project uses *none* of the machine's existing
   infrastructure — no k3s, no shared ragu Qdrant, no reused databases, no
   shared Docker networks beyond Kind's own. Each subsystem gets its own
   dedicated, throwaway Kind cluster. Teardown is `kind delete cluster`.
2. **Repo self-contained.** Everything needed to stand a cluster up lives in its
   owning repo (manifests + scripts). The *only* external input is the operator's
   own secret, supplied through a gitignored `.env` documented by `.env.example`.
3. **No podman.** Podman leaves the project entirely. Images are built with
   `docker build` and loaded with `kind load docker-image`.

## Current topology (before)

| Component | Runs as | Where | Containerized? |
|---|---|---|---|
| Bank API `:8081` | `./target/debug/nano-bank-api` (bare cargo binary) | host process | ❌ no Dockerfile |
| Bank ledger DB `nano_bank_db` | Postgres pod | Kind cluster `nano-bank` | ✅ already k8s |
| Modern core `:8091` | `./target/debug/nano-bank-modern-core` (bare binary) | host process | ✅ has Dockerfile |
| Modern core DB `:5435` | `modern-core-db` | host Docker | docker only |
| Legacy core `:8090` | `legacy-core-app` | host Docker | ✅ has Dockerfile |
| Legacy core DB `:5434` | `legacy-core-db` | host Docker | docker only |

## Target architecture (after)

Two dedicated Kind clusters, provisioned by their own repos, sharing nothing:

```
┌─ Cluster A: "nano-bank"  (this repo owns) ──────────────┐     ┌─ Cluster B: "modern-core" (core repo owns) ─┐
│  ns: nano-bank                                          │     │  ns: modern-core                            │
│    bank-api        (NEW Dockerfile + Deploy/Svc)        │     │    modern-core   (Dockerfile ✓ → Deploy/Svc)│
│    nano_bank_db    (Postgres — already here)            │ ──► │    modern_core_db (Postgres + PVC)          │
│    agent-qdrant    (own Qdrant, NOT ragu)               │  ┆  │                                             │
│    agent-mcp       (MCP gateway :8087)                  │  ┆  └─────────────────────────────────────────────┘
│    agent-api       (Agentic Branch :8086)               │  ┆   cross-cluster: bank-api → modern-core
│    agent-console   (Streamlit :8505)                    │  ┆   over a stable host-port hop
└─────────────────────────────────────────────────────────┘
```

## Component design

### Cluster B — modern core *(work lives in `nano-bank-modern-core` repo)*

- New `k8s/` in that repo: `kind-cluster-config.yaml` (cluster `modern-core`),
  Postgres Deployment/Service/PVC/Secret (its own `modern_core` DB), modern-core
  Deployment/Service, and a `deploy.sh` (create cluster → build image →
  `kind load` → apply → wait ready).
- The core is exposed on a **stable host port** via Kind `extraPortMapping`
  (host `:8091`) so cluster A reaches it without depending on ephemeral container
  IPs.
- Default GL backend for the whole system. Legacy core is **out of scope** here
  and added later only for specific tests behind the same `CORE_BACKEND` swap.

### Cluster A — the bank, in-cluster *(this repo)*

- **New `api/Dockerfile`**: multi-stage Rust *release* build → slim runtime,
  listens `0.0.0.0:8081`. (This is the one missing piece of containerization.)
- **`k8s/bank-api-deployment.yaml` + Service `bank-api:8081`**, env-configured:
  - DB → `postgres-service.nano-bank:5432` (in-cluster; the host `::1`
    IPv4-reset gotcha disappears entirely inside the cluster).
  - `CORE_BACKEND=modern`, `MODERN_CORE_URL=http://<cross-cluster hop>:8091`.
- `bank-api` Service is what the agent's mcp/api pods call for seeds + writes.

### Cluster A — the agent stack *(this repo; replaces `agent/compose.yaml`)*

New `agent/k8s/` manifests, all in ns `nano-bank`:

- **agent-qdrant** — Deployment + Service (`:6333`) + small PVC. Dedicated RAG
  memory, collection `nano_manager_memory`. Never ragu.
- **agent-mcp** — Deployment + Service (`:8087`), not exposed to host. Env:
  `DB_HOST=postgres-service`, `QDRANT_URL=http://agent-qdrant:6333`,
  `NANO_BANK_API=http://bank-api:8081`.
- **agent-api** — Deployment + Service (`:8086`). Env:
  `MCP_URL=http://agent-mcp:8087/mcp`, model config, secrets from
  `nano-agent-secrets`, `NANO_BANK_API=http://bank-api:8081`. Startup probes
  `glm-5.2 → glm-4.7` against `ollama.com/v1` (pods have egress); free-tier
  fallback is automatic.
- **agent-console** — Deployment + Service (`:8505`, Streamlit). Env:
  `MANAGER_API_URL=http://agent-api:8086`.
- `agent/compose.yaml` + the three `Containerfile.*` are **retired**; three
  `Dockerfile`s replace them.

### Console dashboard + new read endpoints

To let the console *show* customer/account/transaction data (not just chat), the
Branch API gains two read endpoints that mirror the existing `/profile` (each
proxies the matching MCP tool; customer-scoping invariant preserved — the server
injects `X-Nano-Customer`, the client never picks it):

- `GET /branch/clients/{id}/accounts`   → MCP `get_accounts`
- `GET /branch/clients/{id}/transactions` → MCP `get_transactions`

The console renders a **client dashboard**: a *Customer* panel (name, email,
KYC), an *Accounts* table (id · type · balance · status), and a *Recent
transactions* table (type · amount · date), auto-refreshed after a confirmed
action so the transfer is visibly seen to land. Chat + propose/confirm remains.

## Secrets & config

- `nano-agent-secrets` (ns nano-bank) is minted **generate-on-apply** from the
  gitignored `agent/.env`, mirroring how `deploy.sh` already generates the
  `sql-scripts` configmap — nothing sensitive is committed:
  ```bash
  kubectl create secret generic nano-agent-secrets -n nano-bank \
    --from-literal=OLLAMA_API_KEY="$(grep -E '^OLLAMA_API_KEY=' agent/.env | cut -d= -f2-)" \
    --from-literal=BRANCH_SERVICE_TOKEN="$(grep -E '^BRANCH_SERVICE_TOKEN=' agent/.env | cut -d= -f2-)" \
    --dry-run=client -o yaml | kubectl apply -f -
  ```
- The committed `k8s/postgres-secret.yaml` (base64 dev password) is **not** the
  template for real secrets — base64 is encoding, not encryption. Live keys only
  ever come from `.env`.
- `agent/.env.example` is updated to the in-cluster shape; real `.env` stays
  gitignored (verified: `.gitignore` lines 20–21 cover `.env` / `.env.*`).

## Cross-cluster networking (bank-api → modern-core)

Both clusters' nodes sit on Docker's shared `kind` network. For a stable path,
cluster B exposes the core on a fixed host port (`extraPortMapping`, `:8091`),
and in cluster A `bank-api` gets `MODERN_CORE_URL` pointing at the Docker
host-gateway on that port — via a headless `Service`+`Endpoints` (or the gateway
IP injected at deploy time). The design commitment is "stable host-port hop, no
ephemeral IPs"; the exact mechanism is pinned in the implementation plan.

## Deploy orchestration & UI access

- Order: **(1)** core repo `deploy.sh` brings up cluster B; **(2)** this repo's
  `deploy.sh` (extended) brings up cluster A: Postgres → build+load+apply
  bank-api → build+load agent images → generate `nano-agent-secrets` → apply
  agent manifests.
- Images: `docker build` + `kind load docker-image --name <cluster>`.
- Host access to **Streamlit `:8505`** and **Branch API `:8086`** via
  `kubectl port-forward` (the existing cluster maps only 80/443/5432; adding
  NodePorts would need a cluster recreate). Ingress is noted as a later option
  since the cluster is `ingress-ready`.

## E2E test (the deliverable)

An `agent/e2e_test.sh` (and/or the existing gated `test_integration_live.py`,
retargeted via port-forward) drives the real path end-to-end:

1. `POST /branch/seed` → creates Ada + Bo, funds Ada 1000 (deposit → **exercises
   the in-cluster modern core** through the ledger port).
2. `POST /branch/clients/{ada}/message` "what's my balance?" → answer cites
   ~$1000 (**MCP DB read + remote GLM**).
3. "transfer 25 …" → returns `pending_action`; **money NOT moved** (verify Ada
   still 1000).
4. `POST …/actions/{id}/confirm` → executes; verify **Ada 975 / Bo 25** in
   Postgres.
5. **Streamlit console** shows customer + accounts + transactions before, and
   re-renders after confirm with Ada 975 / Bo 25 and the new transfer row.

This validates MCP + RAG (Qdrant) + remote Ollama + Streamlit, all in-cluster.

## Phasing (each lands independently)

- **P1** *(core repo `nano-bank-modern-core`; push directly, operator merges — no
  PR review)*: modern core → cluster B.
- **P2** *(this repo)*: bank-api containerized → cluster A, wired to cluster B.
- **P3** *(this repo)*: agent stack → cluster A + secret; console dashboard +
  read endpoints; run the E2E.

## Out of scope / deferred

- Legacy core (only for specific tests, added later behind `CORE_BACKEND`).
- Ingress / TLS; CI.
- `transactions.rs` and other bank handlers unchanged.

## Relates to

- `docs/superpowers/specs/2026-07-07-personal-manager-design.md` (the agent).
- The kernel-split (`api/src/ledger/`): this design realizes the core as a true
  peer service in its own cluster.
