# nano-bank Full-k8s + Agent E2E Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run the entire nano-bank (bank API + modern GL core + agentic personal manager) from Kubernetes across two isolated Kind clusters, then prove the agent end-to-end with MCP + RAG (Qdrant) + a remote Ollama model + a Streamlit dashboard.

**Architecture:** Two dedicated, throwaway Kind clusters sharing nothing. Cluster A `nano-bank` (this repo) runs `bank-api` + the ledger Postgres + the agent stack (`agent-qdrant`/`agent-mcp`/`agent-api`/`agent-console`). Cluster B `modern-core` (the `nano-bank-modern-core` repo) runs the modern GL core + its Postgres. `bank-api` reaches the core cross-cluster over a stable host-port hop.

**Tech Stack:** Rust/axum (bank-api, modern-core), Python 3.12 (agent: LangGraph + MCP + FastAPI + Streamlit + qdrant-client/fastembed), PostgreSQL 16, Kind, Docker, remote Ollama (`ollama.com/v1`, `glm-5.2`→`glm-4.7`).

## Global Constraints

- **Total isolation.** No k3s, no shared ragu Qdrant, no reused DBs, no shared networks beyond Kind's own default `kind` docker network. Each subsystem gets its own dedicated Kind cluster.
- **No podman.** Build images with `docker build`; load with `kind load docker-image --name <cluster>`. Delete the `agent/compose.yaml` + `agent/Containerfile.*` podman artifacts.
- **Repo self-contained.** All manifests + scripts live in the owning repo. The only external input is the operator's `OLLAMA_API_KEY`, supplied via a gitignored `agent/.env` (documented by `agent/.env.example`). Never commit a live secret; base64 in a committed manifest is NOT acceptable for real secrets.
- **Namespaces:** cluster A uses ns `nano-bank`; cluster B uses ns `modern-core`.
- **Default GL backend:** `CORE_BACKEND=modern`. Legacy core is out of scope.
- **Podman env for shell:** none needed — this plan uses `docker`/`kubectl`/`kind` only. `kubectl` context for cluster A is `kind-nano-bank`; for cluster B `kind-modern-core`. Always pass `--context` explicitly.
- **Two repos:** Tasks 1 land in `/home/bmartins/dev/nano-bank-modern-core` (push directly; operator merges, no PR). Tasks 2–9 land in `/home/bmartins/dev/nano-bank` on branch `agent-k8s-e2e`.

## File Structure

**Cluster B — `nano-bank-modern-core` repo (new files):**
- `k8s/kind-cluster-config.yaml` — cluster `modern-core`, maps host `:8091` → nodePort `30091`.
- `k8s/namespace.yaml` — ns `modern-core`.
- `k8s/postgres.yaml` — modern_core Postgres Deployment + Service + PVC + Secret.
- `k8s/modern-core.yaml` — core Deployment + ClusterIP Service + NodePort (30091).
- `k8s/deploy.sh` — create cluster → build+load image → apply → wait → smoke.

**Cluster A — `nano-bank` repo:**
- `api/Dockerfile`, `api/.dockerignore` — containerize the Rust bank API (NEW).
- `k8s/modern-core-endpoints.yaml.tmpl` — cross-cluster Service+Endpoints template (host-gateway IP injected at deploy).
- `k8s/bank-api-deployment.yaml` — bank-api Deployment + Service.
- `agent/api.py` (modify) — add `/accounts` + `/transactions` read endpoints.
- `agent/tests/test_api.py` (modify) — tests for the two new endpoints.
- `agent/test_console.py` (rewrite) — customer/accounts/transactions dashboard + chat.
- `agent/Dockerfile.mcp`, `agent/Dockerfile.api`, `agent/Dockerfile.console` — replace `Containerfile.*`.
- `agent/k8s/qdrant.yaml`, `agent/k8s/mcp.yaml`, `agent/k8s/api.yaml`, `agent/k8s/console.yaml`.
- `agent/k8s/deploy.sh` — build+load 3 images → generate `nano-agent-secrets` → apply → wait.
- `agent/e2e_test.sh` — drive the full two-phase E2E and assert balances.
- `agent/.env.example` (modify), `k8s/deploy.sh` (modify — orchestrate), `CLAUDE.md` + `agent/README.md` (modify — docs).
- **Delete:** `agent/compose.yaml`, `agent/Containerfile.mcp`, `agent/Containerfile.api`, `agent/Containerfile.console`, `agent/run-agent.sh`.

---

## Task 1: Modern core → cluster B *(repo: nano-bank-modern-core)*

**Files:**
- Create: `k8s/kind-cluster-config.yaml`, `k8s/namespace.yaml`, `k8s/postgres.yaml`, `k8s/modern-core.yaml`, `k8s/deploy.sh`

**Interfaces:**
- Produces: a running modern core reachable from the **host** at `http://localhost:8091` (health, `/entries`, `/balances`) via a Kind `extraPortMapping` host `:8091` → nodePort `30091`. Cluster B nodes join Docker's default `kind` network (so cluster A can hop via the host gateway in Task 3).

- [ ] **Step 1: Write the Kind cluster config**

Create `k8s/kind-cluster-config.yaml`:

```yaml
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
name: modern-core
nodes:
- role: control-plane
  extraPortMappings:
  - containerPort: 30091   # modern-core NodePort
    hostPort: 8091
    protocol: TCP
```

- [ ] **Step 2: Write the namespace**

Create `k8s/namespace.yaml`:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: modern-core
  labels:
    name: modern-core
```

- [ ] **Step 3: Write the Postgres manifest**

Create `k8s/postgres.yaml` (schema self-bootstraps via `db::bootstrap` on core startup, so no init job):

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: modern-core-db-secret
  namespace: modern-core
type: Opaque
stringData:
  POSTGRES_DB: modern_core
  POSTGRES_USER: core
  POSTGRES_PASSWORD: core
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: modern-core-db-pvc
  namespace: modern-core
spec:
  accessModes: ["ReadWriteOnce"]
  resources:
    requests:
      storage: 1Gi
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: modern-core-db
  namespace: modern-core
  labels: { app: modern-core-db }
spec:
  replicas: 1
  strategy: { type: Recreate }
  selector:
    matchLabels: { app: modern-core-db }
  template:
    metadata:
      labels: { app: modern-core-db }
    spec:
      containers:
      - name: postgres
        image: postgres:16-alpine
        ports:
        - containerPort: 5432
        envFrom:
        - secretRef: { name: modern-core-db-secret }
        volumeMounts:
        - name: data
          mountPath: /var/lib/postgresql/data
          subPath: pgdata
        readinessProbe:
          exec: { command: ["pg_isready", "-U", "core", "-d", "modern_core"] }
          initialDelaySeconds: 5
          periodSeconds: 5
      volumes:
      - name: data
        persistentVolumeClaim: { claimName: modern-core-db-pvc }
---
apiVersion: v1
kind: Service
metadata:
  name: modern-core-db
  namespace: modern-core
spec:
  selector: { app: modern-core-db }
  ports:
  - port: 5432
    targetPort: 5432
```

- [ ] **Step 4: Write the modern-core app manifest**

Create `k8s/modern-core.yaml`:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: modern-core
  namespace: modern-core
  labels: { app: modern-core }
spec:
  replicas: 1
  selector:
    matchLabels: { app: modern-core }
  template:
    metadata:
      labels: { app: modern-core }
    spec:
      containers:
      - name: modern-core
        image: nano-bank-modern-core:dev
        imagePullPolicy: Never   # loaded via `kind load`
        ports:
        - containerPort: 8091
        env:
        - name: DATABASE_URL
          value: postgres://core:core@modern-core-db:5432/modern_core
        - name: PORT
          value: "8091"
        readinessProbe:
          httpGet: { path: /health, port: 8091 }
          initialDelaySeconds: 5
          periodSeconds: 5
---
apiVersion: v1
kind: Service
metadata:
  name: modern-core
  namespace: modern-core
spec:
  selector: { app: modern-core }
  ports:
  - port: 8091
    targetPort: 8091
---
apiVersion: v1
kind: Service
metadata:
  name: modern-core-nodeport
  namespace: modern-core
spec:
  type: NodePort
  selector: { app: modern-core }
  ports:
  - port: 8091
    targetPort: 8091
    nodePort: 30091
```

- [ ] **Step 5: Write the deploy script**

Create `k8s/deploy.sh` (`chmod +x` after):

```bash
#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
CLUSTER=modern-core
CTX=kind-$CLUSTER

if ! kind get clusters | grep -qx "$CLUSTER"; then
  echo "▶ creating Kind cluster $CLUSTER"
  kind create cluster --config k8s/kind-cluster-config.yaml
fi

echo "▶ building modern-core image"
docker build -t nano-bank-modern-core:dev .
echo "▶ loading image into $CLUSTER"
kind load docker-image nano-bank-modern-core:dev --name "$CLUSTER"

echo "▶ applying manifests"
kubectl --context "$CTX" apply -f k8s/namespace.yaml
kubectl --context "$CTX" apply -f k8s/postgres.yaml
kubectl --context "$CTX" -n modern-core rollout status deploy/modern-core-db --timeout=120s
kubectl --context "$CTX" apply -f k8s/modern-core.yaml
kubectl --context "$CTX" -n modern-core rollout status deploy/modern-core --timeout=120s

echo "▶ smoke: GET :8091/health"
curl -fsS -m 5 http://localhost:8091/health && echo " OK"
```

- [ ] **Step 6: Run the deploy and verify**

Run: `chmod +x k8s/deploy.sh && ./k8s/deploy.sh`
Expected (final lines): `... OK`, and:
Run: `curl -fsS http://localhost:8091/balances`
Expected: HTTP 200 with a JSON array (`[]` or seeded balances) — proves the core + its DB are up in cluster B and reachable from the host.

- [ ] **Step 7: Commit and push**

```bash
git add k8s/
git commit -m "feat(k8s): deploy modern core + Postgres to dedicated Kind cluster

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
git push
```

---

## Task 2: Containerize the bank API *(repo: nano-bank)*

**Files:**
- Create: `api/Dockerfile`, `api/.dockerignore`

**Interfaces:**
- Produces: image `nano-bank-api:dev` — a release binary that reads `config/default.toml` (WORKDIR `/app`) and honors env overrides `NANO_BANK__DATABASE__HOST`, `CORE_BACKEND`, `MODERN_CORE_URL`. Listens `0.0.0.0:8081`.

- [ ] **Step 1: Write `.dockerignore`**

Create `api/.dockerignore`:

```
target/
.git/
```

- [ ] **Step 2: Write the Dockerfile**

Create `api/Dockerfile` (mirrors the modern core's working multi-stage pattern; `config/` must be in the runtime image because `Settings` layers `config/default.toml`):

```dockerfile
FROM rust:1-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /app/target/release/nano-bank-api /usr/local/bin/nano-bank-api
COPY config ./config
EXPOSE 8081
CMD ["nano-bank-api"]
```

- [ ] **Step 3: Build the image**

Run: `cd api && docker build -t nano-bank-api:dev .`
Expected: build completes; `docker images | grep nano-bank-api` shows `dev`.

- [ ] **Step 4: Smoke the binary boots (expect DB failure, not a crash-on-config)**

Run: `docker run --rm -e NANO_BANK__DATABASE__HOST=203.0.113.1 nano-bank-api:dev 2>&1 | head -5`
Expected: log lines showing it read config and *attempts* to connect to the DB (a connection error to the bogus host is fine) — confirms `config/` is present and the binary starts. No "config file not found" / panic-on-startup.

- [ ] **Step 5: Commit**

```bash
cd .. && git add api/Dockerfile api/.dockerignore
git commit -m "feat(api): add Dockerfile for the bank API

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Deploy bank-api to cluster A, wired cross-cluster to the core *(repo: nano-bank)*

**Files:**
- Create: `k8s/modern-core-endpoints.yaml.tmpl`, `k8s/bank-api-deployment.yaml`
- Modify: `k8s/deploy.sh`

**Interfaces:**
- Consumes: image `nano-bank-api:dev` (Task 2); the host-reachable core at `localhost:8091` (Task 1); `postgres-service.nano-bank:5432` (existing).
- Produces: Service `bank-api.nano-bank:8081`; a `modern-core.nano-bank` Service+Endpoints pointing at the Docker `kind`-network host gateway `:8091`, so `bank-api` uses `MODERN_CORE_URL=http://modern-core:8091`.

- [ ] **Step 1: Write the cross-cluster Endpoints template**

Create `k8s/modern-core-endpoints.yaml.tmpl` (`__GATEWAY_IP__` substituted at deploy time — this is the "stable host-port hop" from the spec):

```yaml
apiVersion: v1
kind: Service
metadata:
  name: modern-core
  namespace: nano-bank
spec:
  ports:
  - port: 8091
    targetPort: 8091
---
apiVersion: v1
kind: Endpoints
metadata:
  name: modern-core
  namespace: nano-bank
subsets:
- addresses:
  - ip: __GATEWAY_IP__
  ports:
  - port: 8091
```

- [ ] **Step 2: Write the bank-api Deployment + Service**

Create `k8s/bank-api-deployment.yaml`:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: bank-api
  namespace: nano-bank
  labels: { app: bank-api }
spec:
  replicas: 1
  selector:
    matchLabels: { app: bank-api }
  template:
    metadata:
      labels: { app: bank-api }
    spec:
      containers:
      - name: bank-api
        image: nano-bank-api:dev
        imagePullPolicy: Never
        ports:
        - containerPort: 8081
        env:
        - name: NANO_BANK__DATABASE__HOST
          value: postgres-service
        - name: NANO_BANK__DATABASE__PORT
          value: "5432"
        - name: CORE_BACKEND
          value: modern
        - name: MODERN_CORE_URL
          value: http://modern-core:8091
        readinessProbe:
          httpGet: { path: /health, port: 8081 }
          initialDelaySeconds: 5
          periodSeconds: 5
---
apiVersion: v1
kind: Service
metadata:
  name: bank-api
  namespace: nano-bank
spec:
  selector: { app: bank-api }
  ports:
  - port: 8081
    targetPort: 8081
```

- [ ] **Step 3: Add bank-api bring-up to `k8s/deploy.sh`**

Append to `k8s/deploy.sh` (after the existing Postgres/init-db steps), before its final echo:

```bash
# --- bank-api (in-cluster), wired cross-cluster to the modern core ---
echo "🐳 Building + loading bank-api image..."
docker build -t nano-bank-api:dev ../api
kind load docker-image nano-bank-api:dev --name nano-bank

echo "🌉 Wiring cross-cluster route to modern-core (host gateway hop)..."
GATEWAY_IP=$(docker network inspect kind -f '{{range .IPAM.Config}}{{if .Gateway}}{{.Gateway}}{{end}}{{end}}' | awk '{print $1}')
echo "   host gateway = ${GATEWAY_IP} (core published on host :8091 by cluster modern-core)"
sed "s/__GATEWAY_IP__/${GATEWAY_IP}/" k8s/modern-core-endpoints.yaml.tmpl | kubectl apply -f -

echo "🏦 Deploying bank-api..."
kubectl apply -f k8s/bank-api-deployment.yaml
kubectl -n nano-bank rollout status deploy/bank-api --timeout=180s
```

- [ ] **Step 4: Run the deploy (cluster B must already be up from Task 1)**

Run: `./k8s/deploy.sh`
Expected: `deploy/bank-api` rollout succeeds.

- [ ] **Step 5: Verify bank-api health + cross-cluster ledger post**

Run:
```bash
kubectl -n nano-bank port-forward svc/bank-api 8081:8081 >/tmp/bapf.log 2>&1 &
sleep 3
curl -fsS http://localhost:8081/health && echo " HEALTH-OK"
curl -fsS -X POST localhost:8081/api/v1/ledger/journal -H 'content-type: application/json' \
  -d '{"lines":[{"account":"bank","direction":"debit","amount":250.00},
                {"account":"revenue","direction":"credit","amount":250.00}]}'
curl -fsS localhost:8081/api/v1/ledger/balances
kill %1
```
Expected: `HEALTH-OK`; the journal POST returns a core document id; `/balances` reflects the 250.00 posting — proving `bank-api` (cluster A) posted through the port to `modern-core` (cluster B) across the host-gateway hop.

- [ ] **Step 6: Commit**

```bash
git add k8s/bank-api-deployment.yaml k8s/modern-core-endpoints.yaml.tmpl k8s/deploy.sh
git commit -m "feat(k8s): run bank-api in-cluster, wired cross-cluster to modern core

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Branch API read endpoints for the dashboard *(repo: nano-bank)*

**Files:**
- Modify: `agent/api.py`
- Test: `agent/tests/test_api.py`

**Interfaces:**
- Consumes: the existing `_mcp_session` + tool-invocation pattern already used by the `/profile` route in `agent/api.py`.
- Produces: `GET /branch/clients/{cid}/accounts` (→ MCP `get_accounts`, returns a list) and `GET /branch/clients/{cid}/transactions` (→ MCP `get_transactions`, returns a list). Both require the `Bearer` service token; both inject `X-Nano-Customer` server-side (customer never chosen by caller).

- [ ] **Step 1: Write failing tests**

Read `agent/tests/test_api.py` first to match its existing app-construction/fake-MCP fixture style, then add (adapting names to that fixture — the fake MCP must expose `get_accounts`/`get_transactions` returning canned lists):

```python
def test_accounts_endpoint_returns_list(client, auth_header):
    r = client.get("/branch/clients/cust-1/accounts", headers=auth_header)
    assert r.status_code == 200
    assert isinstance(r.json(), list)
    assert r.json()[0]["account_id"]  # canned account from the fake MCP

def test_transactions_endpoint_returns_list(client, auth_header):
    r = client.get("/branch/clients/cust-1/transactions", headers=auth_header)
    assert r.status_code == 200
    assert isinstance(r.json(), list)

def test_accounts_endpoint_requires_token(client):
    r = client.get("/branch/clients/cust-1/accounts")
    assert r.status_code == 401
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_api.py -q -k "accounts or transactions"`
Expected: FAIL (404 for the new routes / fixture missing the tools).

- [ ] **Step 3: Implement the endpoints**

In `agent/api.py`, add after the existing `profile` route (mirror it exactly, swapping the tool name):

```python
    @app.get("/branch/clients/{cid}/accounts")
    async def accounts(cid: str, authorization: str = Header(None)):
        _auth(authorization)
        client = nano_manager._mcp_session(settings, cid, _token(cid))
        for t in await client.get_tools():
            if t.name == "get_accounts":
                return await t.ainvoke({})
        raise HTTPException(500, "accounts tool unavailable")

    @app.get("/branch/clients/{cid}/transactions")
    async def transactions(cid: str, limit: int = 20, authorization: str = Header(None)):
        _auth(authorization)
        client = nano_manager._mcp_session(settings, cid, _token(cid))
        for t in await client.get_tools():
            if t.name == "get_transactions":
                return await t.ainvoke({"limit": limit})
        raise HTTPException(500, "transactions tool unavailable")
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_api.py -q`
Expected: PASS (all, including the three new).

- [ ] **Step 5: Run the full offline suite (no regressions)**

Run: `agent/.venv/bin/python -m pytest agent -q`
Expected: previous 33 + 3 new pass (34 passed depending on count), 1 skipped.

- [ ] **Step 6: Commit**

```bash
git add agent/api.py agent/tests/test_api.py
git commit -m "feat(agent): add /accounts and /transactions Branch API read endpoints

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: Streamlit console → client dashboard *(repo: nano-bank)*

**Files:**
- Rewrite: `agent/test_console.py`

**Interfaces:**
- Consumes: Branch API `GET /profile`, `GET /accounts`, `GET /transactions` (Task 4), `POST /message`, `POST /actions/{id}/confirm|cancel`, `POST /seed`.
- Produces: a dashboard that shows Customer / Accounts / Recent transactions and refreshes them after a confirmed action.

- [ ] **Step 1: Rewrite the console**

Replace `agent/test_console.py` entirely:

```python
from __future__ import annotations
import os
import httpx
import streamlit as st

from agent.config import Settings

settings = Settings.from_env()
API = os.environ.get("MANAGER_API_URL", f"http://localhost:{settings.branch_port}")
HDR = {"Authorization": f"Bearer {settings.branch_service_token}"}

st.set_page_config(page_title="nano-bank manager — console", layout="wide")
st.title("nano-bank personal manager — console")


def _get(path):
    r = httpx.get(f"{API}{path}", headers=HDR, timeout=60)
    r.raise_for_status()
    return r.json()


left, right = st.columns([1, 2])

with left:
    st.subheader("Seed")
    if st.button("Seed demo (2 customers + funded account)"):
        out = httpx.post(f"{API}/branch/seed", headers=HDR, timeout=180).json()
        st.session_state["customers"] = out["customers"]
        st.success(f"seeded {len(out['customers'])} customers")
    customers = st.session_state.get("customers", [])
    cid = (st.selectbox("client", [c["customer_id"] for c in customers])
           if customers else st.text_input("client id"))

with right:
    st.subheader("Chat")
    msg = st.text_input("Ask or instruct (e.g. 'transfer 25 from <acc> to <acc>')")
    if st.button("Send") and cid and msg:
        data = httpx.post(f"{API}/branch/clients/{cid}/message",
                          json={"message": msg}, headers=HDR, timeout=180).json()
        st.markdown(f"**Manager:** {data.get('answer','')}")
        pa = data.get("pending_action")
        if pa:
            st.warning(f"Proposed: {pa.get('summary', pa)}")
            st.session_state["pending"] = pa
    pa = st.session_state.get("pending")
    if pa and cid:
        c1, c2 = st.columns(2)
        if c1.button("Confirm"):
            rr = httpx.post(f"{API}/branch/clients/{cid}/actions/{pa['id']}/confirm",
                            headers=HDR, timeout=180).json()
            st.success(rr)
            st.session_state.pop("pending", None)
        if c2.button("Cancel"):
            httpx.post(f"{API}/branch/clients/{cid}/actions/{pa['id']}/cancel",
                       headers=HDR, timeout=60)
            st.session_state.pop("pending", None)

# --- Dashboard: customer / accounts / transactions ---
if cid:
    st.divider()
    st.subheader("Client dashboard")
    try:
        prof = _get(f"/branch/clients/{cid}/profile")
        st.markdown(f"**Customer:** {prof.get('first_name','?')} "
                    f"{prof.get('last_name','')} · {prof.get('email','')} "
                    f"· KYC {prof.get('kyc_status','?')}")
        st.markdown("**Accounts**")
        st.table(_get(f"/branch/clients/{cid}/accounts"))
        st.markdown("**Recent transactions**")
        st.table(_get(f"/branch/clients/{cid}/transactions"))
    except Exception as e:  # noqa: BLE001
        st.info(f"Dashboard unavailable: {e}")
```

- [ ] **Step 2: Verify it imports cleanly**

Run: `agent/.venv/bin/python -c "import ast; ast.parse(open('agent/test_console.py').read()); print('parse-ok')"`
Expected: `parse-ok`. (Full render is verified live in Task 8.)

- [ ] **Step 3: Commit**

```bash
git add agent/test_console.py
git commit -m "feat(agent): console shows customer/accounts/transactions dashboard

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: Agent Dockerfiles; retire podman artifacts *(repo: nano-bank)*

**Files:**
- Create: `agent/Dockerfile.mcp`, `agent/Dockerfile.api`, `agent/Dockerfile.console`
- Delete: `agent/compose.yaml`, `agent/Containerfile.mcp`, `agent/Containerfile.api`, `agent/Containerfile.console`, `agent/run-agent.sh`

**Interfaces:**
- Produces: images `nano-agent-mcp:dev`, `nano-agent-api:dev`, `nano-agent-console:dev` (each `COPY . /app/agent`, deps from `requirements.txt`).

- [ ] **Step 1: Create the three Dockerfiles**

`agent/Dockerfile.mcp`:
```dockerfile
FROM python:3.12-slim
WORKDIR /app
COPY requirements.txt /app/requirements.txt
RUN pip install --no-cache-dir -r requirements.txt
COPY . /app/agent
ENV PYTHONUNBUFFERED=1
CMD ["python", "-m", "agent.mcp_server"]
```

`agent/Dockerfile.api`:
```dockerfile
FROM python:3.12-slim
WORKDIR /app
COPY requirements.txt /app/requirements.txt
RUN pip install --no-cache-dir -r requirements.txt
COPY . /app/agent
ENV PYTHONUNBUFFERED=1
CMD ["python", "-m", "agent.api_main"]
```

`agent/Dockerfile.console`:
```dockerfile
FROM python:3.12-slim
WORKDIR /app
COPY requirements.txt /app/requirements.txt
RUN pip install --no-cache-dir -r requirements.txt
COPY . /app/agent
ENV PYTHONUNBUFFERED=1
CMD ["streamlit", "run", "agent/test_console.py", "--server.port=8505", "--server.address=0.0.0.0"]
```

- [ ] **Step 2: Ensure `.dockerignore` excludes the venv and secret**

Create `agent/.dockerignore` (docker uses this name; the old `.containerignore` goes away with podman):
```
.venv/
**/__pycache__/
.env
*.pyc
```

- [ ] **Step 3: Delete the podman artifacts**

Run:
```bash
git rm agent/compose.yaml agent/Containerfile.mcp agent/Containerfile.api \
       agent/Containerfile.console agent/run-agent.sh agent/.containerignore
```

- [ ] **Step 4: Build all three images**

Run:
```bash
cd agent
docker build -f Dockerfile.mcp     -t nano-agent-mcp:dev     .
docker build -f Dockerfile.api     -t nano-agent-api:dev     .
docker build -f Dockerfile.console -t nano-agent-console:dev .
cd ..
```
Expected: three successful builds; `docker images | grep nano-agent` shows all three.

- [ ] **Step 5: Commit**

```bash
git add agent/Dockerfile.mcp agent/Dockerfile.api agent/Dockerfile.console agent/.dockerignore
git commit -m "feat(agent): Docker images for mcp/api/console; retire podman compose

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: Agent stack → cluster A + secret-from-env *(repo: nano-bank)*

**Files:**
- Create: `agent/k8s/qdrant.yaml`, `agent/k8s/mcp.yaml`, `agent/k8s/api.yaml`, `agent/k8s/console.yaml`, `agent/k8s/deploy.sh`
- Modify: `agent/.env.example`

**Interfaces:**
- Consumes: images from Task 6; `postgres-service.nano-bank:5432`; `bank-api.nano-bank:8081` (Task 3); gitignored `agent/.env` for `OLLAMA_API_KEY` + `BRANCH_SERVICE_TOKEN`.
- Produces: in-ns services `agent-qdrant:6333`, `agent-mcp:8087`, `agent-api:8086`, `agent-console:8505`; Secret `nano-agent-secrets`.

- [ ] **Step 1: Qdrant manifest**

Create `agent/k8s/qdrant.yaml`:

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: agent-qdrant-pvc
  namespace: nano-bank
spec:
  accessModes: ["ReadWriteOnce"]
  resources: { requests: { storage: 1Gi } }
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: agent-qdrant
  namespace: nano-bank
  labels: { app: agent-qdrant }
spec:
  replicas: 1
  strategy: { type: Recreate }
  selector: { matchLabels: { app: agent-qdrant } }
  template:
    metadata: { labels: { app: agent-qdrant } }
    spec:
      containers:
      - name: qdrant
        image: qdrant/qdrant:latest
        ports: [ { containerPort: 6333 } ]
        volumeMounts:
        - name: data
          mountPath: /qdrant/storage
      volumes:
      - name: data
        persistentVolumeClaim: { claimName: agent-qdrant-pvc }
---
apiVersion: v1
kind: Service
metadata:
  name: agent-qdrant
  namespace: nano-bank
spec:
  selector: { app: agent-qdrant }
  ports: [ { port: 6333, targetPort: 6333 } ]
```

- [ ] **Step 2: MCP manifest**

Create `agent/k8s/mcp.yaml` (reads Postgres in-cluster — no `::1` gotcha; DB creds match `k8s/postgres-secret.yaml`):

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: agent-mcp
  namespace: nano-bank
  labels: { app: agent-mcp }
spec:
  replicas: 1
  selector: { matchLabels: { app: agent-mcp } }
  template:
    metadata: { labels: { app: agent-mcp } }
    spec:
      containers:
      - name: mcp
        image: nano-agent-mcp:dev
        imagePullPolicy: Never
        ports: [ { containerPort: 8087 } ]
        env:
        - { name: DB_HOST,        value: postgres-service }
        - { name: DB_PORT,        value: "5432" }
        - { name: DB_NAME,        value: nano_bank_db }
        - { name: DB_USER,        value: nanobank_user }
        - { name: DB_PASSWORD,    value: "secure_nano_password_2024!" }
        - { name: QDRANT_URL,     value: http://agent-qdrant:6333 }
        - { name: QDRANT_COLLECTION, value: nano_manager_memory }
        - { name: NANO_BANK_API,  value: http://bank-api:8081 }
        - { name: ACT_MAX_PER_TX, value: "1000" }
        - { name: CONFIRM_TTL_S,  value: "300" }
---
apiVersion: v1
kind: Service
metadata:
  name: agent-mcp
  namespace: nano-bank
spec:
  selector: { app: agent-mcp }
  ports: [ { port: 8087, targetPort: 8087 } ]
```

- [ ] **Step 3: Branch API manifest**

Create `agent/k8s/api.yaml` (`envFrom` pulls the generated Secret):

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: agent-api
  namespace: nano-bank
  labels: { app: agent-api }
spec:
  replicas: 1
  selector: { matchLabels: { app: agent-api } }
  template:
    metadata: { labels: { app: agent-api } }
    spec:
      containers:
      - name: api
        image: nano-agent-api:dev
        imagePullPolicy: Never
        ports: [ { containerPort: 8086 } ]
        envFrom:
        - secretRef: { name: nano-agent-secrets }
        env:
        - { name: OLLAMA_BASE_URL,       value: https://ollama.com/v1 }
        - { name: MANAGER_MODEL,         value: glm-5.2 }
        - { name: MANAGER_FALLBACK_MODEL, value: glm-4.7 }
        - { name: MCP_URL,               value: http://agent-mcp:8087/mcp }
        - { name: NANO_BANK_API,         value: http://bank-api:8081 }
        - { name: BRANCH_PORT,           value: "8086" }
        readinessProbe:
          httpGet: { path: /health, port: 8086 }
          initialDelaySeconds: 10
          periodSeconds: 5
---
apiVersion: v1
kind: Service
metadata:
  name: agent-api
  namespace: nano-bank
spec:
  selector: { app: agent-api }
  ports: [ { port: 8086, targetPort: 8086 } ]
```

- [ ] **Step 4: Console manifest**

Create `agent/k8s/console.yaml`:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: agent-console
  namespace: nano-bank
  labels: { app: agent-console }
spec:
  replicas: 1
  selector: { matchLabels: { app: agent-console } }
  template:
    metadata: { labels: { app: agent-console } }
    spec:
      containers:
      - name: console
        image: nano-agent-console:dev
        imagePullPolicy: Never
        ports: [ { containerPort: 8505 } ]
        envFrom:
        - secretRef: { name: nano-agent-secrets }
        env:
        - { name: MANAGER_API_URL, value: http://agent-api:8086 }
        - { name: NANO_BANK_API,   value: http://bank-api:8081 }
---
apiVersion: v1
kind: Service
metadata:
  name: agent-console
  namespace: nano-bank
spec:
  selector: { app: agent-console }
  ports: [ { port: 8505, targetPort: 8505 } ]
```

- [ ] **Step 5: Agent deploy script (secret generated from .env)**

Create `agent/k8s/deploy.sh` (`chmod +x`):

```bash
#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."          # -> agent/
CTX=kind-nano-bank

[ -f .env ] || { echo "❌ agent/.env missing (copy .env.example, fill OLLAMA_API_KEY + BRANCH_SERVICE_TOKEN)"; exit 1; }
OLLAMA_API_KEY=$(grep -E '^OLLAMA_API_KEY=' .env | cut -d= -f2-)
BRANCH_SERVICE_TOKEN=$(grep -E '^BRANCH_SERVICE_TOKEN=' .env | cut -d= -f2-)
[ -n "$OLLAMA_API_KEY" ] || { echo "❌ OLLAMA_API_KEY empty in .env"; exit 1; }
[ -n "$BRANCH_SERVICE_TOKEN" ] || { echo "❌ BRANCH_SERVICE_TOKEN empty in .env"; exit 1; }

echo "🐳 Building + loading agent images..."
docker build -f Dockerfile.mcp     -t nano-agent-mcp:dev     .
docker build -f Dockerfile.api     -t nano-agent-api:dev     .
docker build -f Dockerfile.console -t nano-agent-console:dev .
kind load docker-image nano-agent-mcp:dev nano-agent-api:dev nano-agent-console:dev --name nano-bank

echo "🔐 Minting nano-agent-secrets from .env (generate-on-apply; nothing committed)..."
kubectl --context "$CTX" create secret generic nano-agent-secrets -n nano-bank \
  --from-literal=OLLAMA_API_KEY="$OLLAMA_API_KEY" \
  --from-literal=BRANCH_SERVICE_TOKEN="$BRANCH_SERVICE_TOKEN" \
  --dry-run=client -o yaml | kubectl --context "$CTX" apply -f -

echo "📦 Applying agent manifests..."
kubectl --context "$CTX" apply -f k8s/qdrant.yaml
kubectl --context "$CTX" -n nano-bank rollout status deploy/agent-qdrant --timeout=180s
kubectl --context "$CTX" apply -f k8s/mcp.yaml
kubectl --context "$CTX" apply -f k8s/api.yaml
kubectl --context "$CTX" apply -f k8s/console.yaml
kubectl --context "$CTX" -n nano-bank rollout status deploy/agent-mcp     --timeout=180s
kubectl --context "$CTX" -n nano-bank rollout status deploy/agent-api     --timeout=240s
kubectl --context "$CTX" -n nano-bank rollout status deploy/agent-console --timeout=180s
echo "✅ agent stack up"
```

- [ ] **Step 6: Update `.env.example` to the in-cluster shape**

Replace `agent/.env.example` body so host/URL defaults reflect in-cluster service names and note only the two secrets are required:

```
# --- Secrets (REQUIRED; minted into nano-agent-secrets by agent/k8s/deploy.sh) ---
OLLAMA_API_KEY=
BRANCH_SERVICE_TOKEN=

# --- Remote model (Ollama cloud, OpenAI-compatible) ---
OLLAMA_BASE_URL=https://ollama.com/v1
MANAGER_MODEL=glm-5.2
MANAGER_FALLBACK_MODEL=glm-4.7

# --- In-cluster service wiring (set by manifests; listed here for local runs) ---
QDRANT_URL=http://agent-qdrant:6333
QDRANT_COLLECTION=nano_manager_memory
DB_HOST=postgres-service
DB_PORT=5432
DB_NAME=nano_bank_db
DB_USER=nanobank_user
DB_PASSWORD=secure_nano_password_2024!
NANO_BANK_API=http://bank-api:8081
MCP_URL=http://agent-mcp:8087/mcp
ACT_MAX_PER_TX=1000
CONFIRM_TTL_S=300
BRANCH_PORT=8086
CONSOLE_PORT=8505
```

- [ ] **Step 7: Deploy and verify pods are Running**

Run: `chmod +x agent/k8s/deploy.sh && agent/k8s/deploy.sh`
Then: `kubectl --context kind-nano-bank -n nano-bank get pods`
Expected: `agent-qdrant`, `agent-mcp`, `agent-api`, `agent-console` all `Running`/`Ready`. (agent-api readiness may take up to a minute — it probes the remote Ollama model at startup.)

- [ ] **Step 8: Commit**

```bash
git add agent/k8s/ agent/.env.example
git commit -m "feat(agent): k8s manifests + secret-from-.env; agent stack in cluster

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: End-to-end test *(repo: nano-bank)*

**Files:**
- Create: `agent/e2e_test.sh`

**Interfaces:**
- Consumes: the running agent stack (Task 7) + bank-api (Task 3) + core (Task 1). Uses `BRANCH_SERVICE_TOKEN` from `agent/.env`.
- Produces: a pass/fail E2E that seeds, asks balance, proposes (asserts no money moved), confirms (asserts money moved), and checks the console is reachable.

- [ ] **Step 1: Write the E2E script**

Create `agent/e2e_test.sh` (`chmod +x`). It port-forwards the Branch API + console, drives the two-phase loop, and asserts balances via the accounts endpoint:

```bash
#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
CTX=kind-nano-bank
TOKEN=$(grep -E '^BRANCH_SERVICE_TOKEN=' .env | cut -d= -f2-)
H="Authorization: Bearer $TOKEN"

kubectl --context "$CTX" -n nano-bank port-forward svc/agent-api 8086:8086 >/tmp/agent-api-pf.log 2>&1 &
API_PF=$!
kubectl --context "$CTX" -n nano-bank port-forward svc/agent-console 8505:8505 >/tmp/agent-console-pf.log 2>&1 &
CON_PF=$!
trap 'kill $API_PF $CON_PF 2>/dev/null || true' EXIT
sleep 4

echo "1) seed"
SEED=$(curl -fsS -X POST localhost:8086/branch/seed -H "$H")
ADA=$(echo "$SEED" | python3 -c 'import sys,json;print(json.load(sys.stdin)["customers"][0]["customer_id"])')
ADA_ACC=$(echo "$SEED" | python3 -c 'import sys,json;print(json.load(sys.stdin)["customers"][0]["account_id"])')
BO_ACC=$(echo "$SEED" | python3 -c 'import sys,json;print(json.load(sys.stdin)["customers"][1]["account_id"])')
echo "   ada=$ADA acc=$ADA_ACC bo_acc=$BO_ACC"

bal () { curl -fsS "localhost:8086/branch/clients/$1/accounts" -H "$H" \
  | python3 -c 'import sys,json;print(sum(float(a["balance"]) for a in json.load(sys.stdin)))'; }

echo "2) ask balance"
curl -fsS -X POST "localhost:8086/branch/clients/$ADA/message" -H "$H" \
  -H 'content-type: application/json' -d '{"message":"what is my balance?"}' \
  | python3 -c 'import sys,json;print("   answer:",json.load(sys.stdin)["answer"][:160])'

echo "3) propose transfer 25 (must NOT move money)"
PROP=$(curl -fsS -X POST "localhost:8086/branch/clients/$ADA/message" -H "$H" \
  -H 'content-type: application/json' \
  -d "{\"message\":\"transfer 25 from $ADA_ACC to $BO_ACC\"}")
AID=$(echo "$PROP" | python3 -c 'import sys,json;d=json.load(sys.stdin);print((d.get("pending_action") or {}).get("id",""))')
[ -n "$AID" ] || { echo "❌ manager did not propose a pending action"; exit 1; }
BEFORE=$(bal "$ADA"); echo "   pending=$AID  ada_balance_after_propose=$BEFORE"
[ "$BEFORE" = "1000.0" ] || { echo "❌ money moved on propose (expected 1000.0, got $BEFORE)"; exit 1; }

echo "4) confirm (money moves)"
curl -fsS -X POST "localhost:8086/branch/clients/$ADA/actions/$AID/confirm" -H "$H" >/dev/null
AFTER=$(bal "$ADA"); echo "   ada_balance_after_confirm=$AFTER"
[ "$AFTER" = "975.0" ] || { echo "❌ expected 975.0 after transfer, got $AFTER"; exit 1; }

echo "5) console reachable"
curl -fsS -o /dev/null -w '   console HTTP %{http_code}\n' localhost:8505/ || { echo "❌ console unreachable"; exit 1; }

echo "✅ E2E PASSED"
```

- [ ] **Step 2: Run the E2E**

Run: `chmod +x agent/e2e_test.sh && agent/e2e_test.sh`
Expected: prints steps 1–5 and ends `✅ E2E PASSED`. Ada's balance is `1000.0` after propose and `975.0` after confirm; console returns HTTP 200.

- [ ] **Step 3: Manual console spot-check (visual confirmation of the dashboard)**

Run: `kubectl --context kind-nano-bank -n nano-bank port-forward svc/agent-console 8505:8505 &`
Open `http://localhost:8505`, click **Seed demo**, pick Ada, confirm the **Customer / Accounts / Recent transactions** panels render, run a transfer + **Confirm**, and confirm the transactions table shows the new transfer and the balance updates. Kill the port-forward after.

- [ ] **Step 4: Commit**

```bash
git add agent/e2e_test.sh
git commit -m "test(agent): end-to-end two-phase + dashboard E2E over the k8s stack

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 9: Orchestration + docs *(repo: nano-bank)*

**Files:**
- Modify: `k8s/deploy.sh` (call the agent deploy), `CLAUDE.md`, `agent/README.md`
- Create: `deploy-all.sh` (repo root)

**Interfaces:**
- Produces: one command that stands the whole thing up, and docs describing the two-cluster topology + `.env`-to-Secret flow.

- [ ] **Step 1: Root orchestration script**

Create `deploy-all.sh` (`chmod +x`) documenting the required order (cluster B first, then cluster A + agent):

```bash
#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
echo "== cluster B: modern core =="
( cd ../nano-bank-modern-core && ./k8s/deploy.sh )
echo "== cluster A: bank + agent =="
./k8s/deploy.sh          # postgres + bank-api + cross-cluster wiring
./agent/k8s/deploy.sh    # qdrant + mcp + api + console + secret
echo "✅ full stack up — run: ./agent/e2e_test.sh"
```

- [ ] **Step 2: Have `k8s/deploy.sh` NOT auto-run the agent (keep concerns separable)**

Confirm `k8s/deploy.sh` ends after bank-api (agent bring-up stays in `agent/k8s/deploy.sh`, invoked by `deploy-all.sh`). No code change if Task 3 left it that way; otherwise remove any agent calls from `k8s/deploy.sh`.

- [ ] **Step 3: Update docs**

In `CLAUDE.md`, under "Running the stack", add a subsection describing: two Kind clusters (`nano-bank`, `modern-core`), the host-gateway `:8091` hop, `deploy-all.sh`, and that agent secrets come from gitignored `agent/.env` via `agent/k8s/deploy.sh` (never committed). In `agent/README.md`, replace the podman-compose "Run" section with the k8s flow (`deploy-all.sh` → `agent/e2e_test.sh` → `kubectl port-forward svc/agent-console 8505`).

- [ ] **Step 4: Verify a clean bring-up from zero (optional but recommended)**

Run: `kind delete cluster --name modern-core && kind delete cluster --name nano-bank` then recreate cluster A per repo CLAUDE.md and run `./deploy-all.sh && ./agent/e2e_test.sh`.
Expected: `✅ E2E PASSED`. (Skip if you don't want to tear down the running clusters.)

- [ ] **Step 5: Commit**

```bash
git add deploy-all.sh k8s/deploy.sh CLAUDE.md agent/README.md
git commit -m "docs+build: deploy-all orchestration and two-cluster k8s docs

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-Review notes

- **Spec coverage:** §Cluster B→T1; §bank-api container+DB wiring→T2,T3; §cross-cluster hop→T3; §agent qdrant/mcp/api/console→T6,T7; §secret-from-.env→T7; §console dashboard + read endpoints→T4,T5; §E2E (all 5 steps)→T8; §deploy orchestration/UI access→T7,T8,T9; §retire podman→T6; §isolation→Global Constraints + dedicated clusters. Legacy core correctly absent (deferred).
- **External dependency to expect at runtime:** `fastembed` downloads its embedding model from the internet on first use inside `agent-mcp`; the isolated cluster still has egress, so this works but adds first-request latency. Not a blocker.
- **Type/name consistency:** MCP tool names (`get_accounts`, `get_transactions`) match `agent/mcp_server.py` `LLM_TOOL_NAMES`; seed response shape (`customers[].customer_id/account_id`, `creds`) matches `agent/seed.py`; balances asserted from `/accounts` (list of `{account_id,account_type,balance,status}`) matches `db.accounts`.
- **Assertion assumption (verify in T8):** deposit funds Ada's single account to 1000 and transfer 25 leaves 975; if seed opens >1 account for Ada, adjust the `bal()` sum expectation — the script sums all her accounts, which still nets 975 post-transfer.
