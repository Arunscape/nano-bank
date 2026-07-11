from __future__ import annotations
import json
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


class SeedTokenResolver:
    """Phase-1 resolver: logs into nano-bank with seeded creds (customer_id -> creds)."""
    def __init__(self, settings: Settings, creds: dict):
        self.settings = settings
        self.creds = creds  # customer_id -> (email, password)
        self._cache: dict = {}

    def resolve(self, customer_id: str) -> Optional[str]:
        if customer_id in self._cache:
            return self._cache[customer_id]
        cred = self.creds.get(customer_id)
        if not cred:
            return None
        from .bank import BankClient
        tok = BankClient(self.settings.nano_bank_api).login(*cred)
        self._cache[customer_id] = tok
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
               seed_fn=None) -> FastAPI:
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

    if seed_fn is not None:
        @app.post("/branch/seed")
        def seed(authorization: str = Header(None)):
            """Dev-only: seed customers/accounts/transactions and register their creds
            in this process so the confirm path can mint their nano-bank token."""
            _auth(authorization)
            return seed_fn()

    return app
