# Design: manager skill system + repo reorg

**Date:** 2026-07-09
**Status:** approved (pending spec review)
**Repo:** `nano-bank` (branch `manager-skills`, off `agent-k8s-e2e`)

Two independent workstreams, delivered together, **Phase 0 first** so the new
skill files land into a clean layout.

---

## Phase 0 — repo reorg (mechanical cleanup)

### Goal
Stop the sprawl of `.sh` files across four directories and the mixing of test
flavors, without fighting language conventions.

### Governing rules
- **`scripts/`** holds top-level lifecycle scripts.
- **`testing/<kind>/`** groups each test *flavor* in its own folder.
- **Respect tooling that mandates location:** Cargo integration tests stay in
  `api/tests/`; component deploy scripts stay beside their manifests
  (`k8s/deploy.sh`, `agent/k8s/deploy.sh`) because they `cd $(dirname $0)/..`
  and reference sibling manifests; the agent's Python unit tests stay in
  `agent/tests/` (idiomatic; keeps `pytest agent` + package imports working).

### Target structure
```
scripts/                     # lifecycle / orchestration
  deploy-all.sh  setup-k8s.sh  start-dev.sh  start-nano-bank.sh  stop-nano-bank.sh
testing/
  harness/                   # 3-container integration harness
    generator/  visa/  viewer/  run-testing.sh  stop-testing.sh  cleanup.sh  demo.sh
  smoke/                     # transactions_smoke.sh
  e2e/                       # e2e_test.sh (moved from agent/)
  api-collection/            # bruno/  +  nano-bank.http
k8s/deploy.sh                # unchanged (with manifests)
agent/k8s/deploy.sh          # unchanged (with manifests)
agent/tests/  api/tests/  api/test_db.rs   # unchanged (tooling-mandated)
```

### Moves + reference fixes (each move must keep the thing working)
- `deploy-all.sh` → `scripts/deploy-all.sh`; change `cd "$(dirname "$0")"` to
  `cd "$(dirname "$0")/.."` so `./k8s/deploy.sh` + `./agent/k8s/deploy.sh` resolve
  from repo root.
- `setup-k8s.sh`, `start-dev.sh`, `start-nano-bank.sh`, `stop-nano-bank.sh` →
  `scripts/`; re-point any relative paths to repo root.
- `agent/e2e_test.sh` → `testing/e2e/e2e_test.sh`; fix its `.env` lookup and
  port-forward paths to reference `agent/.env` from the new location.
- `testing/{generator,visa,viewer,run-testing.sh,stop-testing.sh,cleanup.sh,demo.sh}`
  → `testing/harness/…`; fix any cross-references (compose/Containerfile paths,
  the scripts that launch the three containers).
- `testing/transactions_smoke.sh` → `testing/smoke/`.
- `bruno/` + `nano-bank.http` → `testing/api-collection/`.
- Update references in `CLAUDE.md`, `README.md`, `agent/README.md`,
  `scripts/deploy-all.sh`, and any script that calls another moved script.

### Also
- Rename `agent/test_console.py` → `agent/console.py` (it is the Streamlit app,
  not a test — the `test_` prefix is misleading and makes pytest try to collect
  it). Update `agent/Dockerfile.console` (`streamlit run agent/console.py`) and
  `testing/e2e/e2e_test.sh` references.

### Verification
- `git mv` used for every move (preserve history).
- `agent/.venv/bin/python -m pytest agent -q` still green.
- `cargo test` in `api/` still compiles/collects.
- `scripts/deploy-all.sh --help`-style dry check: scripts resolve paths (bash `-n`
  syntax check + a path-existence assertion), and `testing/e2e/e2e_test.sh` runs
  green against the live stack.

---

## Phase 1 — manager skill system

### Goal
Give the manager a Claude-Code-style **skill** system: context-dependent
system-prompt guidance, dynamically loaded, with **one skill per bank product**
plus advisory skills for personal finance and investment.

### What a skill is
A markdown file with frontmatter + a guidance body. Skills are *knowledge*, not
tools — the action layer (`propose_transfer/deposit/withdraw`, all confirm-gated)
is unchanged.

```
agent/skills/<name>.md
---
name: credit_card
description: Servicing and recommending the bank's credit-card product.
kind: product            # product | advisory
product: credit_card     # (product skills only) maps to an account_type
---
<guidance body: how the manager should advise on / service / recommend this>
```

### Storage & registry *(chosen approach)*
Skills live in **`agent/skills/*.md`**, bundled into the agent images, loaded by a
small `SkillRegistry` (parses frontmatter + body). Rejected alternative: serving
skills through the MCP server — adds coupling for static, non-sensitive content
that needs no customer scoping. Holdings/data still come only through the MCP
gateway; **skills stay local guidance**.

### The hybrid menu (catalog + holdings annotation)
At session start, `assist()` reads the customer's held `account_type`s (via the
existing MCP `get_accounts`) and injects an **"Available skills"** section into the
system prompt: every skill as `name — description`, with product skills tagged
**[held]** or **[available — not held]**, advisory skills always listed. Only
names+descriptions go in; bodies are pulled on demand. This is the full product
**catalog** (so the manager can recommend products the customer lacks), with
holdings as annotation rather than a hard filter.

### Loading (`load_skill`)
A **local** agent tool `load_skill(name) -> str` returns the skill body, which
enters the conversation as guidance. Model-driven relevance over the code-surfaced
menu = the hybrid trigger. Local (static content), so it never touches the
customer-scoped gateway. Added to the manager's tool set alongside the MCP tools.

### Seed skills (this iteration)
- **Product:** `chequing`, `savings`, `credit_card` — servicing + recommendation
  guidance; `savings`/`credit_card` explicitly cover the "not held but worth
  considering" case.
- **Advisory (always listed):** `personal-finance` (budgeting/saving/debt, grounded
  in the customer's real cash flow via `get_transactions`/`get_accounts`) and
  `investment` (recommendations grounded in actual surplus, with an explicit
  "not licensed financial advice" disclaimer; concrete but disclaimed).

### Manager prompt
Add: "You have skills (listed below with descriptions). Before advising on a
product, or on personal finance / investing, call `load_skill` for the relevant
one and follow its guidance. Recommendations are advice only — money movement
still requires the confirm-gated propose_* tools."

### Guardrails
Skills are advice-only; recommendations produce guidance, not actions (e.g.
"consider opening a savings account" is text — there is no auto-open tool). All
money movement stays two-phase confirm-gated.

### Testing
- `SkillRegistry` unit tests (parse frontmatter/body, list, get-by-name).
- Catalog-injection test: given held account_types, the surfaced menu tags each
  product skill correctly and always lists advisory skills.
- `load_skill` tool test (returns body; unknown name → clear error).
- Live check: asking "should I be saving / investing?" loads the right skill and
  grounds the advice in the customer's real numbers.

### Scope / out of scope
- **In:** the framework + 5 seed skills.
- **Out (future):** *actionable* skills (an "open account" tool), an `e-transfer`
  skill when that rail merges, per-skill tool bundles.

## Relates to
- `docs/superpowers/specs/2026-07-07-personal-manager-design.md`
- `docs/superpowers/specs/2026-07-09-nano-bank-k8s-and-agent-e2e-design.md`
