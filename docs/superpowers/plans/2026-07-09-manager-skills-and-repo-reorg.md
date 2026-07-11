# Manager Skill System + Repo Reorg Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Tidy the repo (Phase 0: `scripts/` + `testing/<kind>/`) then give the manager a Claude-Code-style skill system (Phase 1: one skill per product + advisory finance/investment, hybrid loading).

**Architecture:** Phase 0 is mechanical `git mv` + reference fixes, respecting tooling that mandates location (cargo `api/tests/`, python `agent/tests/`, manifest-local deploy scripts). Phase 1 adds `agent/skills/*.md` (frontmatter + body), a `SkillRegistry`, a code-built "Available skills" menu tagged by the customer's holdings, and a local `load_skill` tool the manager calls to pull a skill body — over the existing MCP/LangGraph agent.

**Tech Stack:** bash, Python 3.12 (agent), pytest, Docker/kind/kubectl.

## Global Constraints

- **Preserve history:** every move uses `git mv`.
- **Respect tooling location:** do NOT move `api/tests/`, `api/test_db.rs` (cargo), or `agent/tests/` (python package); do NOT move `k8s/deploy.sh` / `agent/k8s/deploy.sh` (they `cd $(dirname $0)/..` and reference sibling manifests).
- **The `testing/` harness stays podman-based** (legacy; de-podman'ing it is out of scope — this reorg only moves it).
- **Skills are guidance, not tools.** Money movement stays two-phase confirm-gated via `propose_*`. `load_skill` is a **local** agent tool (static content; never touches the customer-scoped MCP gateway).
- **kubectl context:** `kind-nano-bank`. Agent venv: `agent/.venv`. Branch: `manager-skills`.

## File Structure

**Phase 0 moves:**
- `scripts/{deploy-all,setup-k8s,start-dev,start-nano-bank,stop-nano-bank}.sh`
- `testing/harness/{generator,visa,viewer,run-testing.sh,stop-testing.sh,cleanup.sh,demo.sh}`
- `testing/smoke/transactions_smoke.sh`
- `testing/e2e/e2e_test.sh` (from `agent/`)
- `testing/api-collection/{bruno,nano-bank.http}`
- `agent/console.py` (renamed from `agent/test_console.py`)

**Phase 1 new files:**
- `agent/skills/{chequing,savings,credit_card,personal-finance,investment}.md`
- `agent/skills_registry.py` — `SkillRegistry`, `build_skill_menu`, `load_skill_tool`
- `agent/tests/test_skills.py`
- Modify: `agent/nano_manager.py` (inject menu, add tool, prompt), `agent/Dockerfile.console`.

---

## Task 1: `scripts/` for lifecycle scripts

**Files:**
- Move: `deploy-all.sh`, `setup-k8s.sh`, `start-dev.sh`, `start-nano-bank.sh`, `stop-nano-bank.sh` → `scripts/`

**Interfaces:**
- Produces: lifecycle scripts under `scripts/`, each resolving paths from repo root.

- [ ] **Step 1: Move the scripts**

```bash
cd /home/bmartins/dev/nano-bank
mkdir -p scripts
git mv deploy-all.sh setup-k8s.sh start-dev.sh start-nano-bank.sh stop-nano-bank.sh scripts/
```

- [ ] **Step 2: Fix repo-root resolution in each moved script**

`scripts/deploy-all.sh` — change the top `cd` line:
```bash
# was: cd "$(dirname "$0")"
cd "$(dirname "$0")/.."
```
`scripts/start-dev.sh` and `scripts/start-nano-bank.sh` — change:
```bash
# was: project_path=$(dirname "$0")
project_path=$(cd "$(dirname "$0")/.." && pwd)
```
`scripts/setup-k8s.sh` — add as the first line after the shebang/`set`:
```bash
cd "$(dirname "$0")/.."
```
`scripts/start-nano-bank.sh` — fix the final hint: `./stop-nano-bank.sh` → `scripts/stop-nano-bank.sh`. (`stop-nano-bank.sh` has no relative-path deps; no body change.)

- [ ] **Step 3: Verify syntax + path resolution**

```bash
for s in scripts/*.sh; do bash -n "$s" && echo "ok $s"; done
# deploy-all references must resolve from repo root:
cd /home/bmartins/dev/nano-bank && test -x k8s/deploy.sh && test -x agent/k8s/deploy.sh && echo "deploy targets present"
grep -q 'dirname "$0")/..' scripts/deploy-all.sh && echo "deploy-all cd fixed"
```
Expected: every `ok scripts/*.sh`, `deploy targets present`, `deploy-all cd fixed`.

- [ ] **Step 4: Commit**

```bash
git add -A scripts/ && git commit -m "refactor: move lifecycle scripts to scripts/

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: `testing/<kind>/` for each test flavor

**Files:**
- Move within `testing/`: harness bits → `harness/`, `transactions_smoke.sh` → `smoke/`
- Move in: `agent/e2e_test.sh` → `testing/e2e/e2e_test.sh`; `bruno/` + `nano-bank.http` → `testing/api-collection/`

**Interfaces:**
- Consumes: `agent/.env` (the e2e script reads `BRANCH_SERVICE_TOKEN`).
- Produces: grouped test folders; the harness scripts still build/run their sibling service dirs (they `cd $(dirname $0)` and reference `generator/`/`visa/`/`viewer/`, which move with them).

- [ ] **Step 1: Restructure `testing/`**

```bash
cd /home/bmartins/dev/nano-bank
mkdir -p testing/harness testing/smoke testing/e2e testing/api-collection
git mv testing/generator testing/visa testing/viewer testing/harness/
git mv testing/run-testing.sh testing/stop-testing.sh testing/cleanup.sh testing/demo.sh testing/harness/
git mv testing/transactions_smoke.sh testing/smoke/
git mv agent/e2e_test.sh testing/e2e/e2e_test.sh
git mv bruno testing/api-collection/bruno
git mv nano-bank.http testing/api-collection/nano-bank.http
```
(The harness scripts keep working: `run-testing.sh`/`stop-testing.sh` `cd $(dirname $0)` and build `viewer`/`generator`/`visa` which are now siblings in `harness/`.)

- [ ] **Step 2: Fix the e2e script's env-file lookup for its new location**

In `testing/e2e/e2e_test.sh`, replace the top two working lines:
```bash
# was:
# cd "$(dirname "$0")"
# TOKEN=$(grep -E '^BRANCH_SERVICE_TOKEN=' .env | cut -d= -f2-)
# now (resolve repo root, read agent/.env):
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TOKEN=$(grep -E '^BRANCH_SERVICE_TOKEN=' "$ROOT/agent/.env" | cut -d= -f2-)
```
(Everything else in the script uses absolute kubectl/curl and needs no change.)

- [ ] **Step 3: Verify**

```bash
for s in testing/harness/*.sh testing/smoke/*.sh testing/e2e/*.sh; do bash -n "$s" && echo "ok $s"; done
grep -q 'agent/.env' testing/e2e/e2e_test.sh && echo "e2e env path fixed"
# pytest is unaffected (agent/tests/ untouched):
agent/.venv/bin/python -m pytest agent -q 2>&1 | tail -1
```
Expected: all `ok`, `e2e env path fixed`, `39 passed, 1 skipped`.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "refactor: group tests under testing/{harness,smoke,e2e,api-collection}

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Rename the mis-prefixed console module

**Files:**
- Rename: `agent/test_console.py` → `agent/console.py`
- Modify: `agent/Dockerfile.console`, `testing/e2e/e2e_test.sh` (comment only)

**Interfaces:**
- Produces: `agent/console.py` (the Streamlit app); pytest no longer tries to collect it.

- [ ] **Step 1: Rename**

```bash
cd /home/bmartins/dev/nano-bank
git mv agent/test_console.py agent/console.py
```

- [ ] **Step 2: Point the console image at the new name**

In `agent/Dockerfile.console`:
```dockerfile
# was: CMD ["streamlit", "run", "agent/test_console.py", ...]
CMD ["streamlit", "run", "agent/console.py", "--server.port=8505", "--server.address=0.0.0.0"]
```

- [ ] **Step 3: Verify**

```bash
agent/.venv/bin/python -c "import ast; ast.parse(open('agent/console.py').read()); print('parse-ok')"
agent/.venv/bin/python -m pytest agent -q 2>&1 | tail -1   # still 39 passed / 1 skipped
cd agent && docker build -f Dockerfile.console -t nano-agent-console:dev . -q && echo "console image builds" && cd ..
```
Expected: `parse-ok`, `39 passed, 1 skipped`, `console image builds`.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "refactor: rename agent/test_console.py -> agent/console.py (it's the app, not a test)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Update docs for the new paths

**Files:**
- Modify: `CLAUDE.md`, `README.md`, `agent/README.md`

**Interfaces:**
- Produces: docs that reference the new locations (`scripts/deploy-all.sh`, `testing/e2e/e2e_test.sh`, `agent/console.py`).

- [ ] **Step 1: Update references**

Replace stale paths repo-wide in the three docs:
- `./deploy-all.sh` → `./scripts/deploy-all.sh`
- `./agent/e2e_test.sh` → `./testing/e2e/e2e_test.sh`
- `agent/test_console.py` → `agent/console.py`
- Any `start-nano-bank.sh` / `stop-nano-bank.sh` / `setup-k8s.sh` / `start-dev.sh` → prefix `scripts/`
- In `CLAUDE.md`'s testing section, note the new `testing/{harness,smoke,e2e,api-collection}` layout.

- [ ] **Step 2: Verify no stale references remain**

```bash
cd /home/bmartins/dev/nano-bank
! grep -rnE '\./deploy-all\.sh|agent/e2e_test\.sh|agent/test_console\.py|\./(start|stop)-nano-bank\.sh|\./setup-k8s\.sh|\./start-dev\.sh' CLAUDE.md README.md agent/README.md \
  && echo "no stale paths in docs"
```
Expected: `no stale paths in docs`.

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md README.md agent/README.md
git commit -m "docs: update paths for scripts/ + testing/ reorg

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: `SkillRegistry` — parse `agent/skills/*.md`

**Files:**
- Create: `agent/skills_registry.py`, `agent/skills/.gitkeep` (placeholder until Task 6)
- Test: `agent/tests/test_skills.py`

**Interfaces:**
- Produces:
  - `class Skill(name: str, description: str, kind: str, product: Optional[str], body: str)`
  - `class SkillRegistry` with `from_dir(path) -> SkillRegistry`, `.all() -> list[Skill]`, `.get(name) -> Skill | None`
  - Frontmatter is a simple `key: value` block between `---` fences; body is the rest.

- [ ] **Step 1: Write the failing test**

```python
# agent/tests/test_skills.py
import textwrap
from pathlib import Path
from agent.skills_registry import SkillRegistry


def _write(dirpath, name, text):
    (dirpath / f"{name}.md").write_text(textwrap.dedent(text))


def test_registry_parses_frontmatter_and_body(tmp_path):
    _write(tmp_path, "savings", """\
        ---
        name: savings
        description: Servicing and recommending savings accounts.
        kind: product
        product: savings
        ---
        Guidance body line one.
        """)
    reg = SkillRegistry.from_dir(tmp_path)
    s = reg.get("savings")
    assert s is not None
    assert s.description.startswith("Servicing")
    assert s.kind == "product" and s.product == "savings"
    assert "Guidance body line one." in s.body


def test_registry_lists_all_and_missing_is_none(tmp_path):
    _write(tmp_path, "investment", """\
        ---
        name: investment
        description: Investment recommendations (not licensed advice).
        kind: advisory
        ---
        Body.
        """)
    reg = SkillRegistry.from_dir(tmp_path)
    assert [s.name for s in reg.all()] == ["investment"]
    assert reg.get("nope") is None
```

- [ ] **Step 2: Run to verify it fails**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_skills.py -q`
Expected: FAIL (`ModuleNotFoundError: agent.skills_registry`).

- [ ] **Step 3: Implement the registry**

```python
# agent/skills_registry.py
from __future__ import annotations
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


@dataclass
class Skill:
    name: str
    description: str
    kind: str                    # "product" | "advisory"
    product: Optional[str]       # account_type, for product skills
    body: str


def _parse(text: str) -> dict:
    meta, body = {}, text
    if text.startswith("---"):
        _, fm, body = text.split("---", 2)
        for line in fm.strip().splitlines():
            if ":" in line:
                k, v = line.split(":", 1)
                meta[k.strip()] = v.strip()
    return {"meta": meta, "body": body.strip()}


class SkillRegistry:
    def __init__(self, skills: list[Skill]):
        self._by_name = {s.name: s for s in skills}

    @classmethod
    def from_dir(cls, path) -> "SkillRegistry":
        skills = []
        for f in sorted(Path(path).glob("*.md")):
            p = _parse(f.read_text())
            m = p["meta"]
            skills.append(Skill(
                name=m.get("name", f.stem),
                description=m.get("description", ""),
                kind=m.get("kind", "advisory"),
                product=m.get("product") or None,
                body=p["body"]))
        return cls(skills)

    def all(self) -> list[Skill]:
        return list(self._by_name.values())

    def get(self, name: str) -> Optional[Skill]:
        return self._by_name.get(name)
```

- [ ] **Step 4: Run to verify it passes**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_skills.py -q`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add agent/skills_registry.py agent/skills/.gitkeep agent/tests/test_skills.py
git commit -m "feat(agent): SkillRegistry — parse skills/*.md (frontmatter + body)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: Author the five seed skills

**Files:**
- Create: `agent/skills/{chequing,savings,credit_card,personal-finance,investment}.md`
- Delete: `agent/skills/.gitkeep`
- Test: extend `agent/tests/test_skills.py`

**Interfaces:**
- Consumes: `SkillRegistry.from_dir` (Task 5).
- Produces: 5 real skill files; product skills map to account_types `chequing`/`savings`/`credit_card`.

- [ ] **Step 1: Write the five skill files**

`agent/skills/chequing.md`:
```markdown
---
name: chequing
description: Servicing everyday chequing accounts — balances, transfers, day-to-day money movement.
kind: product
product: chequing
---
The chequing account is the client's everyday transactional account. Help them
understand balances and recent activity, and move money via the confirm-gated
propose_* tools when they explicitly instruct it. Flag persistently high idle
balances as an opportunity to move surplus into savings (load the `savings`
skill). Never move money without an explicit instruction and confirmation.
```

`agent/skills/savings.md`:
```markdown
---
name: savings
description: Servicing and recommending savings accounts — growing idle cash, goals.
kind: product
product: savings
---
The savings account holds money the client doesn't need day-to-day. If the client
HOLDS one, help them fund it from chequing (propose_transfer, confirm-gated) and
track progress toward goals. If they do NOT hold one but carry a consistently high
chequing balance, recommend opening a savings account and explain the benefit in
concrete terms (e.g. "≈$X currently idle could be earning interest"). Opening an
account is not something you can do directly — advise the client to open it.
```

`agent/skills/credit_card.md`:
```markdown
---
name: credit_card
description: Servicing and recommending the credit-card product — usage, payments, cashback.
kind: product
product: credit_card
---
For clients who HOLD a card: help them understand balance, available credit, and
recent card activity; encourage paying the balance to avoid interest (a payment is
a confirm-gated transfer/propose_* the client must instruct). For clients WITHOUT a
card who have steady spend and reliable income, you may recommend one and explain
the benefit (cashback/building history), with a caution about paying in full to
avoid interest. Never encourage debt the client can't service.
```

`agent/skills/personal-finance.md`:
```markdown
---
name: personal-finance
description: Personal-finance coaching — budgeting, saving rate, debt paydown — grounded in real data.
kind: advisory
---
Coach the client using their ACTUAL data: pull recent transactions and balances
(get_transactions/get_accounts) and reason from real inflows/outflows — never
invent numbers. Offer a simple, concrete picture: what's coming in, what's going
out, and one or two specific next steps (build an emergency buffer, raise the
saving rate by a realistic amount, prioritise high-interest debt). Keep advice
proportional to what the numbers actually show, and be honest when the data is
too thin to conclude.
```

`agent/skills/investment.md`:
```markdown
---
name: investment
description: Investment recommendations grounded in real surplus. Not licensed financial advice.
kind: advisory
---
Begin by making clear you are not a licensed financial advisor and this is general
education, not personalized investment advice. Ground everything in the client's
real surplus (from get_accounts/get_transactions): only discuss investing money
beyond an emergency buffer and after high-interest debt. Explain general,
product-appropriate ideas (e.g. tax-advantaged registered accounts, low-cost
diversified index funds, matching horizon to risk) in concrete but non-prescriptive
terms. Never guarantee returns; always note that investments can lose value and
suggest consulting a licensed advisor before acting.
```

- [ ] **Step 2: Remove the placeholder and add a coverage test**

```bash
git rm agent/skills/.gitkeep
```
Append to `agent/tests/test_skills.py`:
```python
def test_seed_skills_load_from_repo():
    from pathlib import Path
    reg = SkillRegistry.from_dir(Path(__file__).resolve().parents[1] / "skills")
    names = {s.name for s in reg.all()}
    assert {"chequing", "savings", "credit_card", "personal-finance", "investment"} <= names
    prod = {s.name: s.product for s in reg.all() if s.kind == "product"}
    assert prod == {"chequing": "chequing", "savings": "savings", "credit_card": "credit_card"}
    assert reg.get("investment").kind == "advisory"
```

- [ ] **Step 3: Run**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_skills.py -q`
Expected: PASS (3 tests).

- [ ] **Step 4: Commit**

```bash
git add agent/skills/ agent/tests/test_skills.py
git commit -m "feat(agent): seed skills — chequing/savings/credit_card + personal-finance/investment

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: `build_skill_menu` + `load_skill` tool

**Files:**
- Modify: `agent/skills_registry.py`
- Test: extend `agent/tests/test_skills.py`

**Interfaces:**
- Consumes: `SkillRegistry`, `Skill` (Task 5).
- Produces:
  - `build_skill_menu(registry, held_account_types: set[str]) -> str` — the "Available skills" text: one `- name — description [held]/[available — not held]` line per skill; advisory lines carry no tag.
  - `make_load_skill_tool(registry)` -> a LangChain `@tool`-style callable named `load_skill(name: str) -> str` returning the body or a clear "unknown skill" message.

- [ ] **Step 1: Write failing tests**

```python
def test_build_menu_tags_by_holdings(tmp_path):
    from agent.skills_registry import build_skill_menu
    # two product skills + one advisory
    (tmp_path / "chequing.md").write_text("---\nname: chequing\ndescription: d1\nkind: product\nproduct: chequing\n---\nb")
    (tmp_path / "savings.md").write_text("---\nname: savings\ndescription: d2\nkind: product\nproduct: savings\n---\nb")
    (tmp_path / "investment.md").write_text("---\nname: investment\ndescription: d3\nkind: advisory\n---\nb")
    reg = SkillRegistry.from_dir(tmp_path)
    menu = build_skill_menu(reg, held_account_types={"chequing"})
    assert "chequing — d1 [held]" in menu
    assert "savings — d2 [available — not held]" in menu
    assert "investment — d3" in menu and "investment — d3 [" not in menu  # advisory untagged


def test_load_skill_tool_returns_body_or_error(tmp_path):
    from agent.skills_registry import make_load_skill_tool
    (tmp_path / "savings.md").write_text("---\nname: savings\ndescription: d\nkind: product\nproduct: savings\n---\nBODY-XYZ")
    tool = make_load_skill_tool(SkillRegistry.from_dir(tmp_path))
    assert "BODY-XYZ" in tool.invoke({"name": "savings"})
    assert "unknown" in tool.invoke({"name": "nope"}).lower()
```

- [ ] **Step 2: Run to verify fail**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_skills.py -q -k "menu or load_skill_tool"`
Expected: FAIL (functions missing).

- [ ] **Step 3: Implement**

Append to `agent/skills_registry.py`:
```python
def build_skill_menu(registry: "SkillRegistry", held_account_types: set) -> str:
    lines = []
    for s in registry.all():
        tag = ""
        if s.kind == "product":
            tag = " [held]" if s.product in held_account_types else " [available — not held]"
        lines.append(f"- {s.name} — {s.description}{tag}")
    return "\n".join(lines)


def make_load_skill_tool(registry: "SkillRegistry"):
    from langchain_core.tools import tool

    @tool
    def load_skill(name: str) -> str:
        """Load the full guidance for a named skill from the Available skills list."""
        s = registry.get(name)
        if s is None:
            return f"unknown skill '{name}'"
        return s.body

    return load_skill
```

- [ ] **Step 4: Run to verify pass**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_skills.py -q`
Expected: PASS (all skill tests).

- [ ] **Step 5: Commit**

```bash
git add agent/skills_registry.py agent/tests/test_skills.py
git commit -m "feat(agent): build_skill_menu (holdings-tagged) + load_skill tool

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: Wire skills into `assist()`

**Files:**
- Modify: `agent/nano_manager.py`
- Test: `agent/tests/test_nano_manager.py` (extend)

**Interfaces:**
- Consumes: `SkillRegistry.from_dir`, `build_skill_menu`, `make_load_skill_tool` (Tasks 5–7); the existing `snapshot = await _call("get_accounts")`.
- Produces: `assist()` injects the "Available skills" menu into the context system message, adds `load_skill` to the agent's tools, and the prompt tells the manager to use it.

- [ ] **Step 1: Write a failing test (menu + tool wired)**

Add to `agent/tests/test_nano_manager.py` a test that patches the MCP session with a fake that returns two accounts (chequing held) and asserts the built context contains the skills menu with a `[held]` chequing line and that `load_skill` is among the bound tools. (Follow the file's existing fake-session pattern; assert on the helper `_skill_menu_for(snapshot)` extracted below rather than the whole graph.)

```python
def test_skill_menu_reflects_holdings():
    from agent.nano_manager import _held_account_types, _skills_section
    snapshot = [{"account_id": "a", "account_type": "chequing", "balance": "100", "status": "active"}]
    held = _held_account_types(snapshot)
    assert held == {"chequing"}
    section = _skills_section(held)
    assert "Available skills" in section
    assert "chequing" in section and "[held]" in section
    assert "savings" in section and "available — not held" in section
```

- [ ] **Step 2: Run to verify fail**

Run: `agent/.venv/bin/python -m pytest agent/tests/test_nano_manager.py -q -k skill`
Expected: FAIL (`_held_account_types`/`_skills_section` missing).

- [ ] **Step 3: Implement the wiring**

In `agent/nano_manager.py`, add near the top (after imports):
```python
import json as _json
from pathlib import Path
from .skills_registry import SkillRegistry, build_skill_menu, make_load_skill_tool

_SKILLS = SkillRegistry.from_dir(Path(__file__).resolve().parent / "skills")


def _held_account_types(snapshot) -> set:
    """Extract account_type values from a get_accounts result (list or MCP blocks)."""
    items = snapshot
    if isinstance(snapshot, list) and snapshot and isinstance(snapshot[0], dict) \
            and "text" in snapshot[0]:
        items = []
        for b in snapshot:
            try:
                v = _json.loads(b["text"])
            except Exception:  # noqa: BLE001
                v = None
            if isinstance(v, list):
                items.extend(v)
            elif isinstance(v, dict):
                items.append(v)
    out = set()
    for a in items or []:
        if isinstance(a, dict) and a.get("account_type"):
            out.add(a["account_type"])
    return out


def _skills_section(held_account_types: set) -> str:
    return ("## Available skills (load the relevant one before advising)\n"
            + build_skill_menu(_SKILLS, held_account_types))
```

Update `MANAGER_PROMPT` — append:
```python
    " You have skills listed under 'Available skills'; before advising on a "
    "product or on personal finance/investing, call load_skill(name) for the "
    "relevant one and follow its guidance. Recommendations are advice only — "
    "money still moves only via the confirm-gated propose_* tools."
```

In `assist()`, after `snapshot = await _call("get_accounts")` and building `context`, fold the skills section into the context message and add the tool:
```python
    held = _held_account_types(snapshot)
    context = SystemMessage(f"<client_snapshot>\n{snapshot}\n</client_snapshot>\n"
                            f"<durable_memory>\n{recalled}\n</durable_memory>\n"
                            f"{_skills_section(held)}")
    tools = tools + [make_load_skill_tool(_SKILLS)]
```
(Keep the existing `create_react_agent(mf.llm("fast"), tools, ...)` call — it now receives `load_skill` too.)

- [ ] **Step 4: Run to verify pass + full suite**

Run: `agent/.venv/bin/python -m pytest agent -q`
Expected: PASS — previous 39 + new skill/menu tests, 1 skipped.

- [ ] **Step 5: Commit**

```bash
git add agent/nano_manager.py agent/tests/test_nano_manager.py
git commit -m "feat(agent): inject holdings-tagged skill menu + load_skill into the manager

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 9: Rebuild, redeploy, live verification

**Files:** none (deploy + verify)

**Interfaces:**
- Consumes: the running cluster (agent-api/mcp) from the k8s work.

- [ ] **Step 1: Rebuild + reload the agent-api image (carries nano_manager + skills)**

```bash
cd /home/bmartins/dev/nano-bank/agent
docker build -f Dockerfile.api -t nano-agent-api:dev . -q
kind load docker-image nano-agent-api:dev --name nano-bank
kubectl --context kind-nano-bank -n nano-bank rollout restart deploy/agent-api
kubectl --context kind-nano-bank -n nano-bank rollout status deploy/agent-api --timeout=180s
```

- [ ] **Step 2: Live check — advisory skill loads and grounds in real data**

```bash
kubectl --context kind-nano-bank -n nano-bank port-forward svc/agent-api 8086:8086 >/tmp/pf.log 2>&1 &
PF=$!; sleep 4
TOKEN=$(grep -E '^BRANCH_SERVICE_TOKEN=' .env | cut -d= -f2-); H="Authorization: Bearer $TOKEN"
SEED=$(curl -fsS -m120 -X POST localhost:8086/branch/seed -H "$H")
ADA=$(echo "$SEED" | python3 -c 'import sys,json;print(json.load(sys.stdin)["customers"][0]["customer_id"])')
curl -fsS -m120 -X POST "localhost:8086/branch/clients/$ADA/message" -H "$H" \
  -H 'content-type: application/json' \
  -d '{"message":"I have money sitting in chequing — should I be saving or investing it?"}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["answer"][:400])'
kill $PF 2>/dev/null
```
Expected: the answer references the client's real ~$1000 balance and gives
savings/investment guidance consistent with the `savings`/`personal-finance`/
`investment` skills (grounded, disclaimered) — evidence the menu + `load_skill`
work end-to-end.

- [ ] **Step 3: Commit (if any tweak was needed) and finish**

If Step 2 required a prompt/skill wording tweak, commit it:
```bash
git add -A && git commit -m "chore(agent): tune skill wording after live check

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-Review notes

- **Spec coverage:** Phase 0 §scripts→T1, §testing→T2, §console rename→T3, §docs→T4. Phase 1 §registry→T5, §seed skills→T6, §menu+load_skill→T7, §assist wiring + prompt→T8, §live testing→T9. Guardrails (advice-only, confirm-gated) preserved — no new action tools added.
- **Placeholders:** none — all skill bodies, code, and move commands are concrete.
- **Type/name consistency:** `SkillRegistry.from_dir/all/get`, `Skill(name,description,kind,product,body)`, `build_skill_menu(registry, held_account_types)`, `make_load_skill_tool(registry)`, `_held_account_types`, `_skills_section` are used identically across T5–T8. `held_account_types` is a `set[str]` throughout.
- **Watch-outs:** the reorg's harness scripts stay podman-based (moved, not rewritten); `_held_account_types` handles both plain-list and MCP content-block snapshots (matching what `get_accounts` returns in-cluster).
