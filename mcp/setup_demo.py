# /// script
# requires-python = ">=3.11"
# dependencies = ["httpx>=0.27"]
# ///
"""One-shot seeding for the agentic-banking MCP demo.

Creates (against a running nano-bank stack):
  1. a demo customer + login (the "account owner"),
  2. a chequing account, funded with a $750 deposit and a $50 withdrawal so
     history is interesting (skipped with a warning if the GL core is down),
     plus a savings account as the approved payee,
  3. a registered agent named "Claude" (secret captured — shown once!),
  4. TWO differently-scoped mandates for that ONE agent (the real-life shape):
     - chequing: read + transfer:initiate (max_per_tx $200, daily_cap $500,
       payee pinned to savings),
     - savings: READ-ONLY.
     Both 7-day expiry; the MCP server discovers them live.

Writes mcp/.env.demo (gitignored — contains live secrets) and prints the
`claude mcp add` command, demo prompts, and the ready-made revoke curl.

Run: uv run mcp/setup_demo.py
"""

import datetime as dt
import pathlib
import shlex
import sys
import time

import httpx

BASE = "http://localhost:8081"
PASSWORD = "securepass123"
HERE = pathlib.Path(__file__).resolve().parent


def die(msg: str) -> None:
    sys.exit(f"✗ {msg}")


def main() -> None:
    c = httpx.Client(base_url=BASE, timeout=15.0)

    # 0. Stack check
    try:
        assert c.get("/health").status_code == 200
    except Exception:
        die(f"nano-bank not reachable at {BASE} — bring the stack up first (see root CLAUDE.md)")

    stamp = int(time.time())
    email = f"demo.owner.{stamp}@example.com"

    # 1. Customer + login
    r = c.post(
        "/api/v1/customers",
        json={
            "email": email,
            "phone_number": f"{stamp % 10_000_000_000:010}",
            "first_name": "Demo",
            "last_name": "Owner",
            "date_of_birth": "1990-05-15",
            "sin": f"{stamp % 1_000_000_000:09}",
            "password": PASSWORD,
        },
    )
    r.status_code in (200, 201) or die(f"create customer: {r.status_code} {r.text}")
    r = c.post("/api/v1/auth/login", json={"email": email, "password": PASSWORD})
    r.status_code == 200 or die(f"login: {r.status_code} {r.text}")
    token = r.json()["access_token"]
    auth = {"Authorization": f"Bearer {token}"}
    print(f"✓ customer {email}")

    # 2. Funded chequing account + a savings account as the approved payee
    r = c.post("/api/v1/accounts", headers=auth, json={"account_type": "chequing"})
    r.status_code in (200, 201) or die(f"create account: {r.status_code} {r.text}")
    account_id = r.json()["account_id"]
    r = c.post("/api/v1/accounts", headers=auth, json={"account_type": "savings"})
    r.status_code in (200, 201) or die(f"create payee account: {r.status_code} {r.text}")
    payee_id = r.json()["account_id"]
    funded = True
    for path, body in [
        ("deposit", {"account_id": account_id, "amount": 750.00, "description": "Payday deposit"}),
        ("withdrawal", {"account_id": account_id, "amount": 50.00, "description": "Coffee money"}),
    ]:
        r = c.post(f"/api/v1/transactions/{path}", headers=auth, json=body)
        if r.status_code == 503:
            print("⚠ GL core unavailable — continuing with an unfunded ($0) account")
            funded = False
            break
        r.status_code in (200, 201) or die(f"{path}: {r.status_code} {r.text}")
    print(f"✓ chequing account {account_id}" + (" — balance $700.00" if funded else ""))

    # 3. Register the agent (the secret is returned exactly once — captured here)
    r = c.post(
        "/api/v1/agents",
        json={
            "display_name": "Claude",
            "description": "Anthropic's Claude, acting as the account owner's assistant (MCP demo)",
        },
    )
    r.status_code == 201 or die(f"register agent: {r.status_code} {r.text}")
    agent = r.json()
    agent_id, agent_secret = agent["agent_id"], agent["agent_secret"]
    print(f"✓ agent 'Claude' registered: {agent_id}")

    # 4. The consent acts: TWO differently-scoped mandates for ONE agent.
    expires = (dt.datetime.now(dt.timezone.utc) + dt.timedelta(days=7)).isoformat()
    r = c.post(
        "/api/v1/mandates",
        headers=auth,
        json={
            "agent_id": agent_id,
            "account_id": account_id,
            "scopes": ["read:balance", "read:transactions", "transfer:initiate"],
            "max_per_tx": 200.00,
            "daily_cap": 500.00,
            "allowed_payees": [payee_id],
            "expires_at": expires,
        },
    )
    r.status_code == 201 or die(f"grant chequing mandate: {r.status_code} {r.text}")
    mandate_id = r.json()["mandate_id"]
    print(
        f"✓ chequing mandate: {mandate_id} (reads + transfers ≤ $200/tx, $500/day, "
        "payee = savings)"
    )
    r = c.post(
        "/api/v1/mandates",
        headers=auth,
        json={
            "agent_id": agent_id,
            "account_id": payee_id,
            "scopes": ["read:balance", "read:transactions"],
            "expires_at": expires,
        },
    )
    r.status_code == 201 or die(f"grant savings mandate: {r.status_code} {r.text}")
    savings_mandate_id = r.json()["mandate_id"]
    print(f"✓ savings mandate:  {savings_mandate_id} (READ-ONLY)")

    # 5. Persist demo state (contains live secrets — gitignored)
    env_file = HERE / ".env.demo"
    env_file.write_text(
        f"""# agentic-banking MCP demo state — generated {dt.datetime.now().isoformat(timespec='seconds')}
# CONTAINS LIVE SECRETS — do not commit.
NANO_BANK_URL={BASE}
NANO_BANK_AGENT_ID={agent_id}
NANO_BANK_AGENT_SECRET={agent_secret}
# owner-side (for the revoke step of the demo):
DEMO_CHEQUING_MANDATE_ID={mandate_id}
DEMO_SAVINGS_MANDATE_ID={savings_mandate_id}
DEMO_CUSTOMER_EMAIL={email}
DEMO_CUSTOMER_TOKEN={token}
DEMO_ACCOUNT_ID={account_id}
DEMO_PAYEE_ACCOUNT_ID={payee_id}
"""
    )
    print(f"✓ wrote {env_file}")

    server = HERE / "nano_bank_agent_mcp.py"
    add_cmd = (
        "claude mcp add nano-bank-agent"
        f" --env NANO_BANK_URL={BASE}"
        f" --env NANO_BANK_AGENT_ID={agent_id}"
        f" --env NANO_BANK_AGENT_SECRET={agent_secret}"
        f" -- uv run {shlex.quote(str(server))}"
    )
    revoke_cmd = (
        f"curl -s -X DELETE {BASE}/api/v1/mandates/{mandate_id}"
        f' -H "Authorization: Bearer {token}" -w "%{{http_code}}\\n"'
    )

    print(
        f"""
──────────────────────────────────────────────────────────────────────────
NEXT STEPS

1. Wire Claude Code up as the agent:

   {add_cmd}

2. Start a Claude Code session and try (ONE registration, BOTH accounts):
   • "What access do you have to my bank?"       → lists both mandates + scopes
   • "What's my chequing balance? And savings?"  → reads under each mandate
   • "Move $150 from chequing into my savings ({payee_id})."
   • "Now move $250 more." → POLICY_DENIED (over the $200 per-transaction cap)
   • "Transfer $20 FROM my savings." → POLICY_DENIED SCOPE_MISSING (read-only!)
   • Grant/revoke more mandates in the UI at {BASE}/app — Claude picks up the
     change on its very next call, no re-registration.

3. THE REVOKE MOMENT — as the account owner, pull consent (customer token,
   not the agent's):

   {revoke_cmd}

   …then ask Claude for the CHEQUING balance again: MANDATE_INACTIVE — while
   the savings mandate keeps working. Per-grant revocation, live.

4. Audit trail (every decision, allow AND deny):
   kubectl exec -n nano-bank deployment/postgres -- psql -U nanobank_user -d nano_bank_db \\
     -c "SELECT operation, decision, reason, created_at FROM agent_actions \\
         WHERE mandate_id = '{mandate_id}' ORDER BY created_at;"

Cleanup: claude mcp remove nano-bank-agent
──────────────────────────────────────────────────────────────────────────"""
    )


if __name__ == "__main__":
    main()
