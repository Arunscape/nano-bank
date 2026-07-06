# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "mcp[cli]>=1.2",
#   "httpx>=0.27",
# ]
# ///
"""nano-bank agent MCP server (stdio).

Exposes the nano-bank **agent plane** (`/api/v1/agent/*`) as MCP tools, so a
real AI assistant (e.g. Claude Code) can act on a customer's behalf under a
mandate — the scoped, limited, expiring, revocable consent record.

Trust model, deliberately mirrored here:
- This server holds ONLY the agent's own credentials + a mandate id. It never
  sees customer credentials.
- The agent token it mints is a 5-minute *pointer* to the mandate; nano-bank
  re-reads the mandate row on every request, so the moment the customer
  revokes, the very next tool call fails with MANDATE_INACTIVE — no local
  state to clean up.
- Policy failures (MANDATE_INACTIVE, POLICY_DENIED) are returned as tool
  *results*, not exceptions, so the model can read and explain the decision.

Config (env):
  NANO_BANK_URL          default http://localhost:8081
  NANO_BANK_AGENT_ID     the registered agent's id
  NANO_BANK_AGENT_SECRET the secret returned once at registration
  NANO_BANK_MANDATE_ID   the mandate to act under

Run: uv run mcp/nano_bank_agent_mcp.py   (PEP 723 — uv resolves deps itself)
"""

import os
import time
from typing import Any

import httpx
from mcp.server.fastmcp import FastMCP

BASE_URL = os.environ.get("NANO_BANK_URL", "http://localhost:8081").rstrip("/")
AGENT_ID = os.environ.get("NANO_BANK_AGENT_ID", "")
AGENT_SECRET = os.environ.get("NANO_BANK_AGENT_SECRET", "")
MANDATE_ID = os.environ.get("NANO_BANK_MANDATE_ID", "")

mcp = FastMCP("nano-bank-agent")

# Token cache: (access_token, unix_expiry). Re-minted ~30 s before expiry.
_token: dict[str, Any] = {"value": None, "exp": 0.0}


def _error_payload(resp: httpx.Response) -> dict[str, Any]:
    """Surface nano-bank's { error: { code, message, details } } verbatim."""
    try:
        body = resp.json()
    except Exception:
        body = {"error": {"code": f"HTTP_{resp.status_code}", "message": resp.text[:500]}}
    body.setdefault("error", {})["http_status"] = resp.status_code
    return body


def _mint_token(client: httpx.Client) -> dict[str, Any] | None:
    """Mint an agent token; returns an error payload on failure, None on success."""
    resp = client.post(
        f"{BASE_URL}/api/v1/auth/agent-token",
        json={
            "agent_id": AGENT_ID,
            "agent_secret": AGENT_SECRET,
            "mandate_id": MANDATE_ID,
        },
    )
    if resp.status_code != 200:
        return _error_payload(resp)
    body = resp.json()
    _token["value"] = body["access_token"]
    _token["exp"] = time.time() + float(body.get("expires_in", 300))
    return None


def _agent_get(path: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
    """GET an agent-plane endpoint with a fresh-enough token.

    Retries exactly once with a re-minted token on 401 (expired token); a 401
    that persists (revoked/expired mandate, disabled agent) is returned as-is.
    """
    with httpx.Client(timeout=10.0) as client:
        if _token["value"] is None or time.time() > _token["exp"] - 30:
            if err := _mint_token(client):
                return err
        for attempt in (1, 2):
            resp = client.get(
                f"{BASE_URL}{path}",
                params=params,
                headers={"Authorization": f"Bearer {_token['value']}"},
            )
            if resp.status_code == 200:
                return resp.json()
            if resp.status_code == 401 and attempt == 1:
                if err := _mint_token(client):
                    return err
                continue
            return _error_payload(resp)
    return {"error": {"code": "UNREACHABLE", "message": "unexpected fallthrough"}}


@mcp.tool()
def whoami() -> dict[str, Any]:
    """Who am I, banking-wise? The agent's public registration record and the
    mandate (consent grant) this server is configured to act under. Use this
    to explain the consent chain to the user."""
    with httpx.Client(timeout=10.0) as client:
        resp = client.get(f"{BASE_URL}/api/v1/agents/{AGENT_ID}")
        agent = resp.json() if resp.status_code == 200 else _error_payload(resp)
    return {
        "agent": agent,
        "acting_under_mandate": MANDATE_ID,
        "note": (
            "Access is scoped by the mandate, re-checked by the bank on every "
            "request, and revocable by the customer at any time."
        ),
    }


@mcp.tool()
def get_account_balance() -> dict[str, Any]:
    """Current balance of the mandated account (balance, available_balance,
    any active holds). Requires the read:balance scope; the mandate decides
    which account — there is no way to ask about any other account."""
    return _agent_get("/api/v1/agent/account")


@mcp.tool()
def get_recent_transactions(limit: int = 10) -> dict[str, Any]:
    """Recent transactions on the mandated account, newest first (double-entry
    legs included). Requires the read:transactions scope."""
    limit = max(1, min(int(limit), 100))
    return _agent_get("/api/v1/agent/transactions", params={"limit": limit})


if __name__ == "__main__":
    missing = [
        name
        for name, val in [
            ("NANO_BANK_AGENT_ID", AGENT_ID),
            ("NANO_BANK_AGENT_SECRET", AGENT_SECRET),
            ("NANO_BANK_MANDATE_ID", MANDATE_ID),
        ]
        if not val
    ]
    if missing:
        raise SystemExit(f"missing env: {', '.join(missing)} (run mcp/setup_demo.py first)")
    mcp.run()
