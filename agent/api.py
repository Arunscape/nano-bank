from __future__ import annotations
import json
import time
from typing import Optional, Protocol

from fastapi import FastAPI, Header, HTTPException
from pydantic import BaseModel

from .config import Settings
from . import nano_manager


def _unwrap(result, *, one: bool = False):
    """Normalize an MCP tool result to plain JSON.

    Over HTTP, langchain-mcp-adapters returns MCP content blocks
    ([{"type": "text", "text": "<json>"}...]); parse those back to data.
    Plain values (e.g. from in-process test fakes) pass through unchanged.
    """
    if isinstance(result, list) and result and isinstance(result[0], dict) \
            and "text" in result[0]:
        items = []
        for block in result:
            try:
                value = json.loads(block["text"])
            except Exception:  # noqa: BLE001
                value = block.get("text")
            items.extend(value if isinstance(value, list) else [value])
        result = items
    if one:
        if isinstance(result, list):
            return result[0] if result else {}
        return result
    return result


class TokenResolver(Protocol):
    def resolve(self, customer_id: str) -> Optional[str]: ...


def _default_login(base, cred):
    from .bank import BankClient
    return BankClient(base).login(*cred)


class SeedTokenResolver:
    """Phase-1 resolver: logs into nano-bank with seeded creds (customer_id -> creds).

    Customer JWTs expire in 15 min; cache each token with a TTL (default 10 min,
    inside the 15-min window) and re-login on expiry so long-running demos don't
    start 401ing a quarter-hour after boot.
    """
    def __init__(self, settings: Settings, creds: dict, *, ttl_seconds: int = 600,
                 now=time.monotonic, login=_default_login):
        self.settings = settings
        self.creds = creds  # customer_id -> (email, password)
        self.ttl = ttl_seconds
        self._now = now
        self._login = login
        self._cache: dict = {}  # customer_id -> (token, expires_at)

    def resolve(self, customer_id: str) -> Optional[str]:
        cred = self.creds.get(customer_id)
        if not cred:
            return None
        hit = self._cache.get(customer_id)
        now = self._now()
        if hit and hit[1] > now:
            return hit[0]
        tok = self._login(self.settings.nano_bank_api, cred)
        self._cache[customer_id] = (tok, now + self.ttl)
        return tok


class MessageIn(BaseModel):
    message: str
    thread_id: Optional[str] = None


async def _default_confirm(settings, customer_id, token, action_id, cancel=False):
    """Reach execute_action/cancel_action directly over MCP — never through the LLM."""
    client = nano_manager._mcp_session(settings, customer_id, token)
    name = "cancel_action" if cancel else "execute_action"
    for t in await client.get_tools():
        if t.name == name:
            return await t.ainvoke({"action_id": action_id})
    raise HTTPException(500, "confirm tool unavailable")


def create_app(settings: Settings, *, assist_fn=nano_manager.assist,
               confirm_fn=_default_confirm, token_resolver: Optional[TokenResolver] = None,
               seed_fn=None, mandate_client=None, mandate_pep=None) -> FastAPI:
    app = FastAPI(title="nano-bank personal manager")

    def _auth(authorization: Optional[str]):
        expected = f"Bearer {settings.branch_service_token}"
        if not settings.branch_service_token or authorization != expected:
            raise HTTPException(401, "invalid service token")

    def _token(cid: str) -> Optional[str]:
        return token_resolver.resolve(cid) if token_resolver else None

    @app.get("/health")
    def health():
        return {"status": "ok"}

    async def _tool(cid: str, name: str, args: Optional[dict] = None, *, one: bool = False):
        client = nano_manager._mcp_session(settings, cid, _token(cid))
        for t in await client.get_tools():
            if t.name == name:
                return _unwrap(await t.ainvoke(args or {}), one=one)
        raise HTTPException(500, f"{name} tool unavailable")

    @app.get("/branch/clients/{cid}/profile")
    async def profile(cid: str, authorization: str = Header(None)):
        _auth(authorization)
        return await _tool(cid, "get_profile", one=True)

    @app.get("/branch/clients/{cid}/accounts")
    async def accounts(cid: str, authorization: str = Header(None)):
        _auth(authorization)
        return await _tool(cid, "get_accounts")

    @app.get("/branch/clients/{cid}/transactions")
    async def transactions(cid: str, limit: int = 20, authorization: str = Header(None)):
        _auth(authorization)
        return await _tool(cid, "get_transactions", {"limit": limit})

    @app.post("/branch/clients/{cid}/message")
    async def message(cid: str, body: MessageIn, authorization: str = Header(None)):
        _auth(authorization)
        return await assist_fn(settings, cid, _token(cid), body.message, body.thread_id)

    @app.post("/branch/clients/{cid}/actions/{aid}/confirm")
    async def confirm(cid: str, aid: str, authorization: str = Header(None)):
        _auth(authorization)
        return await confirm_fn(settings, cid, _token(cid), aid, cancel=False)

    @app.post("/branch/clients/{cid}/actions/{aid}/cancel")
    async def cancel(cid: str, aid: str, authorization: str = Header(None)):
        _auth(authorization)
        return await confirm_fn(settings, cid, _token(cid), aid, cancel=True)

    # --- external mandated-agent gateway: the ONLY door for the external agent ---
    from .mandate_gateway import MandateClient, MandatePEP
    # Mutable binding: starts from settings, rebindable at runtime by demo-seed
    # (so the demo works without redeploying the branch with new agent creds).
    _gw = {"agent_id": settings.nano_agent_id, "agent_secret": settings.nano_agent_secret,
           "mandate_id": settings.agent_mandate_id, "customer_id": settings.agent_customer_id,
           "biller": settings.agent_biller_account_id}

    def _mkclient():
        return mandate_client or MandateClient(
            settings.nano_bank_api, _gw["agent_id"], _gw["agent_secret"])

    def _mkpep(client):
        return mandate_pep or MandatePEP(client)

    def _gw_auth(authorization: Optional[str]):
        expected = f"Bearer {settings.agent_gateway_token}"
        if not settings.agent_gateway_token or authorization != expected:
            raise HTTPException(401, "invalid agent gateway token")

    _OP_SCOPE = {"transfer_out": "transfer:initiate", "open_account": "account:open",
                 "register_payee": "payee:register"}

    @app.get("/agent-gateway/mandate")
    def gw_mandate(authorization: str = Header(None)):
        _gw_auth(authorization)
        live = _mkclient().list_mandates()
        m = next((x for x in live if x.get("mandate_id") == _gw["mandate_id"]), None)
        if not m:
            raise HTTPException(404, "no active mandate")
        return m

    @app.post("/agent-gateway/act")
    def gw_act(body: dict, authorization: str = Header(None)):
        _gw_auth(authorization)
        op = (body or {}).get("operation")
        p = (body or {}).get("params") or {}
        scope = _OP_SCOPE.get(op)
        if scope is None:
            raise HTTPException(400, f"unknown operation {op!r}")
        client = _mkclient()
        amount = p.get("amount") if op == "transfer_out" else None
        d = _mkpep(client).check(_gw["mandate_id"], scope, amount=amount)
        if not d.allowed:
            return {"decision": "deny", "operation": op, "reason": d.reason}
        if op == "transfer_out":
            import uuid as _u
            to_acct = p.get("to_account_id") or _gw["biller"]
            idem = p.get("idempotency_key") or _u.uuid4().hex
            tok = client.mint_token(_gw["mandate_id"])
            code, res = client.agent_transfer(tok, to_acct, p["amount"],
                                              p.get("description", "Epcor utilities bill"),
                                              idem)
            if code == 202:
                return {"decision": "pending_approval", "operation": op, "http": code,
                        "approval_id": (res or {}).get("approval_id"), "result": res}
            if code >= 400:
                # The PEP cleared scope + max_per_tx, but the bank still rejected
                # at the transaction layer — most often allowed_payees (403
                # PAYEE_NOT_ALLOWED), which is enforced there, not in the PEP.
                err = (res or {}).get("error") or {}
                reason = err.get("message") or err.get("details") or f"bank rejected ({code})"
                return {"decision": "deny", "operation": op, "http": code,
                        "reason": reason, "result": res}
            return {"decision": "allow", "operation": op, "http": code, "result": res}
        from .bank import BankClient
        bank = BankClient(settings.nano_bank_api)
        ctok = _token(_gw["customer_id"])
        if op == "open_account":
            return {"decision": "allow", "operation": op,
                    "result": bank.create_account(ctok, {"account_type": p["account_type"]})}
        return {"decision": "allow", "operation": op,
                "result": bank.register_recipient(ctok, p["email"], p["name"])}

    @app.post("/agent-gateway/message")
    async def gw_message(body: dict, authorization: str = Header(None)):
        _gw_auth(authorization)
        d = _mkpep(_mkclient()).check(_gw["mandate_id"], "read:balance")
        if not d.allowed:
            return {"answer": f"(denied) {d.reason}", "trace": []}
        return await assist_fn(settings, _gw["customer_id"],
                               _token(_gw["customer_id"]), (body or {}).get("message", ""), None)

    @app.post("/agent-gateway/revoke")
    def gw_revoke(authorization: str = Header(None)):
        _gw_auth(authorization)
        _mkclient().revoke(_token(_gw["customer_id"]), _gw["mandate_id"])
        return {"revoked": True}

    if seed_fn is not None:
        @app.post("/branch/seed")
        def seed(authorization: str = Header(None)):
            """Dev-only: seed customers/accounts/transactions and register their creds
            in this process so the confirm path can mint their nano-bank token."""
            _auth(authorization)
            return seed_fn()

        @app.post("/agent-gateway/demo-seed")
        def gw_demo_seed(authorization: str = Header(None)):
            """Seed a funded customer, register an external agent + grant its mandate
            (and an Epcor biller), and rebind the gateway to it — one call, ready to run."""
            _gw_auth(authorization)
            from .bank import BankClient
            from .seed import seed_agent_mandate
            seeded = seed_fn()   # seeds Ada+Bo, registers their creds in the resolver
            ada = seeded["customers"][0]
            bank = BankClient(settings.nano_bank_api)
            binding = seed_agent_mandate(bank, _token(ada["customer_id"]), ada["account_id"])
            _gw.update(agent_id=binding["agent_id"], agent_secret=binding["agent_secret"],
                       mandate_id=binding["mandate_id"], customer_id=ada["customer_id"],
                       biller=binding["epcor_account_id"])
            return {"customer_id": ada["customer_id"], "account_id": ada["account_id"],
                    "mandate_id": binding["mandate_id"], "agent_id": binding["agent_id"],
                    "epcor_account_id": binding["epcor_account_id"]}

    return app
