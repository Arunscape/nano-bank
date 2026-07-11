# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "mcp[cli]>=1.2",
#   "httpx>=0.27",
# ]
# ///
"""nano-bank agent MCP server (stdio) — one agent, many mandates.

Exposes the nano-bank **agent plane** (`/api/v1/agent/*`) as MCP tools, so a
real AI assistant (e.g. Claude Code) can act on a customer's behalf under
their granted mandates — the scoped, limited, expiring, revocable consent
records.

One registration = one AGENT. The server holds only the agent's credentials;
it **discovers the agent's mandates live** (`POST /auth/agent-mandates`), so a
grant made in the consent UI appears on the next tool call and a revocation
disappears just as fast — no re-registration ever. Each mandate covers one
account with its own scopes/caps; tools take an `account` argument (e.g.
"chequing", "savings-1234") that selects WHICH mandate to act under, and the
server mints that mandate's 5-minute pointer token on demand. The bank still
pins every request to its mandate — this server only routes.

Policy failures (MANDATE_INACTIVE, POLICY_DENIED) are returned as tool
*results*, not exceptions, so the model can read and explain the decision.

Config (env):
  NANO_BANK_URL          default http://localhost:8081
  NANO_BANK_AGENT_ID     the registered agent's id
  NANO_BANK_AGENT_SECRET the secret returned once at registration

Run: uv run mcp/nano_bank_agent_mcp.py   (PEP 723 — uv resolves deps itself)
"""

import os
import time
import uuid
from typing import Any

import httpx
from mcp.server.fastmcp import FastMCP

BASE_URL = os.environ.get("NANO_BANK_URL", "http://localhost:8081").rstrip("/")
AGENT_ID = os.environ.get("NANO_BANK_AGENT_ID", "")
AGENT_SECRET = os.environ.get("NANO_BANK_AGENT_SECRET", "")

mcp = FastMCP("nano-bank-agent")

# Per-mandate token cache {mandate_id: {"value": str, "exp": float}} and a
# short-lived mandate-list cache (so a burst of tool calls shares one lookup
# while a fresh grant still shows up within seconds).
_tokens: dict[str, dict[str, Any]] = {}
_mandates_cache: dict[str, Any] = {"value": None, "exp": 0.0}
MANDATES_TTL_S = 15.0


def _error_payload(resp: httpx.Response) -> dict[str, Any]:
    """Surface nano-bank's { error: { code, message, details } } verbatim."""
    try:
        body = resp.json()
    except Exception:
        body = {"error": {"code": f"HTTP_{resp.status_code}", "message": resp.text[:500]}}
    body.setdefault("error", {})["http_status"] = resp.status_code
    return body


def _label(m: dict[str, Any]) -> str:
    return f"{m['account_type']}-{m['account_last4']}"


def _mandates(client: httpx.Client, fresh: bool = False) -> list[dict[str, Any]] | dict[str, Any]:
    """The agent's live mandate set (short cache); error payload on failure."""
    if not fresh and _mandates_cache["value"] is not None and time.time() < _mandates_cache["exp"]:
        return _mandates_cache["value"]
    resp = client.post(
        f"{BASE_URL}/api/v1/auth/agent-mandates",
        json={"agent_id": AGENT_ID, "agent_secret": AGENT_SECRET},
    )
    if resp.status_code != 200:
        return _error_payload(resp)
    mandates = resp.json()
    _mandates_cache["value"] = mandates
    _mandates_cache["exp"] = time.time() + MANDATES_TTL_S
    return mandates


def _resolve(client: httpx.Client, account: str) -> dict[str, Any]:
    """Map an `account` argument to one mandate.

    Accepts a label ("chequing-1234"), a bare account type ("chequing", if
    unambiguous), a last-4, an account id, or a mandate id. With exactly one
    mandate, an empty argument selects it. Returns {"mandate": …} or an
    {"error": …} payload listing the valid choices.
    """
    mandates = _mandates(client)
    if isinstance(mandates, dict):  # error payload
        return mandates
    if not mandates:
        return {"error": {"code": "NO_MANDATES",
                          "message": "This agent holds no active mandates — ask the "
                                     "customer to grant one in the consent UI (/app)."}}
    account = (account or "").strip().lower()
    if not account:
        if len(mandates) == 1:
            return {"mandate": mandates[0]}
        return {"error": {"code": "ACCOUNT_REQUIRED",
                          "message": "Several accounts are mandated — say which one.",
                          "choices": [_label(m) for m in mandates]}}
    hits = [
        m for m in mandates
        if account in (_label(m).lower(), m["account_type"].lower(),
                       m["account_last4"], m["account_id"].lower(),
                       m["mandate_id"].lower())
    ]
    if len(hits) == 1:
        return {"mandate": hits[0]}
    code = "ACCOUNT_AMBIGUOUS" if hits else "ACCOUNT_NOT_MANDATED"
    return {"error": {"code": code,
                      "message": f"'{account}' matches {len(hits)} of the mandated accounts.",
                      "choices": [_label(m) for m in mandates]}}


def _mint_token(client: httpx.Client, mandate_id: str) -> dict[str, Any] | None:
    """Mint this mandate's pointer token; error payload on failure, None on success."""
    resp = client.post(
        f"{BASE_URL}/api/v1/auth/agent-token",
        json={"agent_id": AGENT_ID, "agent_secret": AGENT_SECRET, "mandate_id": mandate_id},
    )
    if resp.status_code != 200:
        return _error_payload(resp)
    body = resp.json()
    _tokens[mandate_id] = {"value": body["access_token"],
                           "exp": time.time() + float(body.get("expires_in", 300))}
    return None


def _agent_call(
    mandate_id: str,
    method: str,
    path: str,
    params: dict[str, Any] | None = None,
    body: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Call an agent-plane endpoint under one mandate's token.

    Retries exactly once with a re-minted token on 401 (expired token); a 401
    that persists (revoked/expired mandate, disabled agent) is returned as-is.
    Safe to retry POSTs here because the API is idempotency-keyed.
    """
    with httpx.Client(timeout=10.0) as client:
        tok = _tokens.get(mandate_id)
        if tok is None or time.time() > tok["exp"] - 30:
            if err := _mint_token(client, mandate_id):
                return err
        for attempt in (1, 2):
            resp = client.request(
                method,
                f"{BASE_URL}{path}",
                params=params,
                json=body,
                headers={"Authorization": f"Bearer {_tokens[mandate_id]['value']}"},
            )
            if resp.status_code in (200, 201):
                return resp.json()
            if resp.status_code == 401 and attempt == 1:
                if err := _mint_token(client, mandate_id):
                    return err
                continue
            return _error_payload(resp)
    return {"error": {"code": "UNREACHABLE", "message": "unexpected fallthrough"}}


def _with_resolved(account: str):
    """Resolve `account` → mandate, or return the error payload."""
    with httpx.Client(timeout=10.0) as client:
        return _resolve(client, account)


@mcp.tool()
def list_my_access() -> dict[str, Any]:
    """What access do I currently hold? Lists every ACTIVE mandate the
    customer has granted this agent: which account (type + last-4), which
    scopes (read:balance / read:transactions / transfer:initiate), the
    per-transaction and daily caps with today's usage, and the expiry.
    Call this first when unsure which accounts you may touch — grants and
    revocations show up here live."""
    with httpx.Client(timeout=10.0) as client:
        mandates = _mandates(client, fresh=True)
    if isinstance(mandates, dict):
        return mandates
    return {
        "agent_id": AGENT_ID,
        "mandates": [
            {
                "account": _label(m),
                "account_id": m["account_id"],
                "scopes": m["scopes"],
                "max_per_tx": m["max_per_tx"],
                "daily_cap": m["daily_cap"],
                "spent_today": m["daily_used"],
                "expires_at": m["expires_at"],
                "mandate_id": m["mandate_id"],
            }
            for m in mandates
        ],
        "note": "Each mandate is separately scoped, capped, audited, and revocable "
                "by the customer at any time.",
    }


@mcp.tool()
def whoami() -> dict[str, Any]:
    """Who am I to this bank? The agent's public registration record plus how
    many mandates it currently holds. Use list_my_access for the detail."""
    with httpx.Client(timeout=10.0) as client:
        resp = client.get(f"{BASE_URL}/api/v1/agents/{AGENT_ID}")
        agent = resp.json() if resp.status_code == 200 else _error_payload(resp)
        mandates = _mandates(client)
    count = len(mandates) if isinstance(mandates, list) else None
    return {
        "agent": agent,
        "active_mandates": count,
        "note": "Access is scoped per mandate, re-checked by the bank on every "
                "request, and revocable by the customer at any time.",
    }


@mcp.tool()
def get_account_balance(account: str = "") -> dict[str, Any]:
    """Balance of one mandated account (balance, available_balance, holds).
    `account` picks which mandate to read under — a label from list_my_access
    like "chequing-1234", or just the type if unambiguous; leave empty when
    only one account is mandated. Requires that mandate's read:balance scope."""
    r = _with_resolved(account)
    if "error" in r:
        return r
    m = r["mandate"]
    out = _agent_call(m["mandate_id"], "GET", "/api/v1/agent/account")
    if "error" not in out:
        out["account"] = _label(m)
    return out


@mcp.tool()
def get_recent_transactions(account: str = "", limit: int = 10) -> dict[str, Any]:
    """Recent transactions on one mandated account, newest first (double-entry
    legs included). `account` as in get_account_balance. Requires that
    mandate's read:transactions scope."""
    r = _with_resolved(account)
    if "error" in r:
        return r
    m = r["mandate"]
    limit = max(1, min(int(limit), 100))
    out = _agent_call(m["mandate_id"], "GET", "/api/v1/agent/transactions",
                      params={"limit": limit})
    if "error" not in out:
        out["account"] = _label(m)
    return out


@mcp.tool()
def transfer(
    from_account: str,
    to_account: str,
    amount: float,
    description: str,
    idempotency_key: str,
) -> dict[str, Any]:
    """Transfer money OUT of one mandated account to another nano-bank account.

    from_account selects which mandate funds it (label / type / last-4 from
    list_my_access) — that mandate needs the transfer:initiate scope, and the
    bank enforces ITS limits atomically: max_per_tx, the daily cap, and (if
    set) the payee allowlist. A breach returns POLICY_DENIED with the reason
    (MAX_PER_TX_EXCEEDED / DAILY_CAP_EXCEEDED / PAYEE_NOT_ALLOWED); explain it
    to the user rather than retrying. A flat $1.50 fee applies.

    to_account: either another MANDATED account by label/type/last-4 (e.g.
    "savings" — no id needed) or a full account UUID for any other
    destination. Never guess a UUID; ask the user for it if the destination
    isn't one of the mandated accounts.

    idempotency_key: REQUIRED. Invent a fresh unique string for each new
    payment, and REUSE the exact same key if you retry the same payment after
    an error/timeout — a sequentially replayed key returns the original
    transfer instead of paying twice (do NOT fire the same payment in
    parallel). Never reuse a key for a different payment.
    """
    r = _with_resolved(from_account)
    if "error" in r:
        return r
    m = r["mandate"]

    # Destination: a raw UUID passes straight through; otherwise resolve it as
    # one of the mandated accounts (client-side sugar only — the bank still
    # enforces payees/caps/scopes on the resolved id).
    to_id = (to_account or "").strip()
    try:
        uuid.UUID(to_id)
    except ValueError:
        dest = _with_resolved(to_id)
        if "error" in dest:
            err = dest["error"]
            err["message"] = (
                f"'{to_account}' is not a full account id and doesn't match a "
                "mandated account. Use a mandated label or ask the user for the "
                "destination's full account id."
            )
            return dest
        to_id = dest["mandate"]["account_id"]

    out = _agent_call(
        m["mandate_id"],
        "POST",
        "/api/v1/agent/transfers",
        body={
            "to_account_id": to_id,
            "amount": round(float(amount), 2),
            "description": description,
            "idempotency_key": idempotency_key,
        },
    )
    if "error" not in out:
        out["from_account"] = _label(m)
    return out


if __name__ == "__main__":
    missing = [
        name
        for name, val in [
            ("NANO_BANK_AGENT_ID", AGENT_ID),
            ("NANO_BANK_AGENT_SECRET", AGENT_SECRET),
        ]
        if not val
    ]
    if missing:
        raise SystemExit(f"missing env: {', '.join(missing)} (run mcp/setup_demo.py first)")
    mcp.run()
