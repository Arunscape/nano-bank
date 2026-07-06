# /// script
# requires-python = ">=3.11"
# dependencies = ["httpx>=0.27"]
# ///
"""One-shot seeding for the agentic-banking MCP demo.

Creates (against a running nano-bank stack):
  1. a demo customer + login (the "account owner"),
  2. a chequing account, funded with a $750 deposit and a $50 withdrawal so
     history is interesting (skipped with a warning if the GL core is down),
  3. a registered agent named "Claude" (secret captured — shown once!),
  4. a mandate: read:balance + read:transactions, 7-day expiry.

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
        die(f"nano-bank not reachable at {BASE} — bring the stack up first (HANDOFF.md §6)")

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

    # 2. Funded chequing account
    r = c.post("/api/v1/accounts", headers=auth, json={"account_type": "chequing"})
    r.status_code in (200, 201) or die(f"create account: {r.status_code} {r.text}")
    account_id = r.json()["account_id"]
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

    # 4. The consent act: grant the mandate
    expires = (dt.datetime.now(dt.timezone.utc) + dt.timedelta(days=7)).isoformat()
    r = c.post(
        "/api/v1/mandates",
        headers=auth,
        json={
            "agent_id": agent_id,
            "account_id": account_id,
            "scopes": ["read:balance", "read:transactions"],
            "expires_at": expires,
        },
    )
    r.status_code == 201 or die(f"grant mandate: {r.status_code} {r.text}")
    mandate_id = r.json()["mandate_id"]
    print(f"✓ mandate granted: {mandate_id} (read:balance, read:transactions, 7 days)")

    # 5. Persist demo state (contains live secrets — gitignored)
    env_file = HERE / ".env.demo"
    env_file.write_text(
        f"""# agentic-banking MCP demo state — generated {dt.datetime.now().isoformat(timespec='seconds')}
# CONTAINS LIVE SECRETS — do not commit.
NANO_BANK_URL={BASE}
NANO_BANK_AGENT_ID={agent_id}
NANO_BANK_AGENT_SECRET={agent_secret}
NANO_BANK_MANDATE_ID={mandate_id}
# owner-side (for the revoke step of the demo):
DEMO_CUSTOMER_EMAIL={email}
DEMO_CUSTOMER_TOKEN={token}
DEMO_ACCOUNT_ID={account_id}
"""
    )
    print(f"✓ wrote {env_file}")

    server = HERE / "nano_bank_agent_mcp.py"
    add_cmd = (
        "claude mcp add nano-bank-agent"
        f" --env NANO_BANK_URL={BASE}"
        f" --env NANO_BANK_AGENT_ID={agent_id}"
        f" --env NANO_BANK_AGENT_SECRET={agent_secret}"
        f" --env NANO_BANK_MANDATE_ID={mandate_id}"
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

2. Start a Claude Code session and try:
   • "Who are you to my bank? Use the nano-bank tools to introduce yourself."
   • "What's my account balance?"
   • "Summarize my recent transactions."

3. THE REVOKE MOMENT — as the account owner, pull consent (customer token,
   not the agent's):

   {revoke_cmd}

   …then ask Claude for the balance again: the very next tool call returns
   MANDATE_INACTIVE. No token blocklist — the mandate row IS the truth.

4. Audit trail (every decision, allow AND deny):
   kubectl exec -n nano-bank deployment/postgres -- psql -U nanobank_user -d nano_bank_db \\
     -c "SELECT operation, decision, reason, created_at FROM agent_actions \\
         WHERE mandate_id = '{mandate_id}' ORDER BY created_at;"

Cleanup: claude mcp remove nano-bank-agent
──────────────────────────────────────────────────────────────────────────"""
    )


if __name__ == "__main__":
    main()
