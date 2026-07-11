from __future__ import annotations
import contextlib
import contextvars
from dataclasses import dataclass

from mcp.server.fastmcp import FastMCP
from mcp.server.transport_security import TransportSecuritySettings

from .config import Settings
from .db import ClientContext
from .memory import QdrantMemory, AuditLog
from .actions import ActionStore, ActDenied, ActError

_CUSTOMER: contextvars.ContextVar[str] = contextvars.ContextVar("nano_customer")
_TOKEN: contextvars.ContextVar[str] = contextvars.ContextVar("nano_token")

LLM_TOOL_NAMES = frozenset({
    "get_profile", "get_accounts", "get_transactions", "get_cards",
    "recall", "remember", "propose_transfer", "propose_deposit", "propose_withdraw",
    "register_interac_recipient", "list_interac_recipients",
    "remove_interac_recipient", "propose_interac_transfer"})
CONFIRM_ONLY_TOOL_NAMES = frozenset({"execute_action", "cancel_action"})


def current_customer() -> str:
    try:
        return _CUSTOMER.get()
    except LookupError:
        raise LookupError("no customer bound to this MCP request")


def current_token():
    return _TOKEN.get(None)


@contextlib.contextmanager
def bind(customer_id: str, token=None):
    t1 = _CUSTOMER.set(customer_id)
    t2 = _TOKEN.set(token)
    try:
        yield
    finally:
        _CUSTOMER.reset(t1)
        _TOKEN.reset(t2)


class BindMiddleware:
    """ASGI middleware: copy trusted headers into the context vars per request."""
    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope["type"] == "http":
            headers = {k.decode().lower(): v.decode() for k, v in scope.get("headers", [])}
            cust = headers.get("x-nano-customer")
            tok = headers.get("x-nano-token")
            if cust:
                c1 = _CUSTOMER.set(cust)
                c2 = _TOKEN.set(tok)
                try:
                    await self.app(scope, receive, send)
                finally:
                    _CUSTOMER.reset(c1)
                    _TOKEN.reset(c2)
                return
        await self.app(scope, receive, send)


@dataclass
class Deps:
    db: ClientContext
    memory: QdrantMemory
    audit: AuditLog
    actions: ActionStore
    bank: "BankClient"


def build_mcp(deps: Deps) -> FastMCP:
    # This MCP server is an internal-only ClusterIP service (never published to
    # the host); customer scoping is enforced by trusted X-Nano-* headers +
    # network isolation. Disable DNS-rebinding protection so in-cluster clients
    # reaching it by service name (Host: agent-mcp:8087) aren't rejected (421).
    mcp = FastMCP("nano-manager",
                  transport_security=TransportSecuritySettings(
                      enable_dns_rebinding_protection=False))

    @mcp.tool()
    def get_profile() -> dict:
        """The bound client's profile."""
        return deps.db.profile(current_customer()) or {}

    @mcp.tool()
    def get_accounts() -> list:
        """The bound client's accounts and balances."""
        return deps.db.accounts(current_customer())

    @mcp.tool()
    def get_transactions(limit: int = 20) -> list:
        """The bound client's recent transactions."""
        return deps.db.transactions(current_customer(), limit=limit)

    @mcp.tool()
    def get_cards() -> list:
        """The bound client's credit-card accounts."""
        return deps.db.cards(current_customer())

    @mcp.tool()
    def recall(query: str, k: int = 3) -> list:
        """Recall durable memories about the bound client."""
        return deps.memory.recall(query, current_customer(), k=k)

    @mcp.tool()
    def remember(fact: str, kind: str = "observation") -> str:
        """Store a durable memory about the bound client."""
        return deps.memory.store(fact, customer_id=current_customer(), kind=kind)

    def _propose(kind, **kw):
        try:
            return deps.actions.propose(current_customer(), current_token(), kind, **kw)
        except ActDenied as e:
            return {"denied": True, "reason": str(e)}

    @mcp.tool()
    def propose_transfer(to_account: str, amount: str, from_account: str, memo: str = "") -> dict:
        """Propose a transfer from one of the client's accounts. Requires confirmation."""
        return _propose("transfer", amount=amount, from_account=from_account,
                        to_account=to_account, memo=memo or None)

    @mcp.tool()
    def propose_deposit(to_account: str, amount: str) -> dict:
        """Propose a deposit into one of the client's accounts. Requires confirmation."""
        return _propose("deposit", amount=amount, to_account=to_account)

    @mcp.tool()
    def propose_withdraw(from_account: str, amount: str) -> dict:
        """Propose a withdrawal from one of the client's accounts. Requires confirmation."""
        return _propose("withdraw", amount=amount, from_account=from_account)

    @mcp.tool()
    def register_interac_recipient(email: str, name: str) -> dict:
        """Register an Interac e-Transfer recipient (payee) for the bound client."""
        return deps.bank.register_recipient(current_token(), email, name)

    @mcp.tool()
    def list_interac_recipients() -> list:
        """List the bound client's registered Interac recipients (payees)."""
        return deps.db.interac_recipients(current_customer())

    @mcp.tool()
    def remove_interac_recipient(recipient_id: str) -> str:
        """Remove a registered Interac recipient by id."""
        deps.bank.remove_recipient(current_token(), recipient_id)
        return f"removed {recipient_id}"

    @mcp.tool()
    def propose_interac_transfer(payee_email: str, amount: str, from_account: str,
                                 security_question: str = "", security_answer: str = "",
                                 memo: str = "") -> dict:
        """Propose an Interac e-Transfer from one of the client's accounts to a
        REGISTERED payee email, sent over the real Interac rail. Unless the
        recipient has autodeposit, a security_question and security_answer are
        required. Requires confirmation."""
        return _propose("interac", amount=amount, from_account=from_account,
                        payee_email=payee_email,
                        security_question=security_question or None,
                        security_answer=security_answer or None,
                        memo=memo or None)

    # --- confirm-only (never bound to the agent's toolset) -------------------
    @mcp.tool()
    def execute_action(action_id: str) -> dict:
        """Execute a previously proposed action. Confirm-path only."""
        try:
            return deps.actions.execute(action_id, current_customer(), current_token())
        except ActError as e:
            return {"error": str(e)}

    @mcp.tool()
    def cancel_action(action_id: str) -> dict:
        """Cancel a pending action. Confirm-path only."""
        try:
            return deps.actions.cancel(action_id, current_customer())
        except ActError as e:
            return {"error": str(e)}

    return mcp


def build_deps(settings: Settings) -> Deps:
    db = ClientContext(settings.db)
    memory = QdrantMemory.from_settings(settings)
    audit = AuditLog.from_settings(settings)
    from .bank import BankClient
    bank = BankClient(settings.nano_bank_api)
    actions = ActionStore(db, bank, audit,
                          max_per_tx=settings.act_max_per_tx, ttl_s=settings.confirm_ttl_s)
    return Deps(db=db, memory=memory, audit=audit, actions=actions, bank=bank)


def main():
    settings = Settings.from_env()
    mcp = build_mcp(build_deps(settings))
    app = BindMiddleware(mcp.streamable_http_app())
    import uvicorn
    uvicorn.run(app, host="0.0.0.0", port=8087)


if __name__ == "__main__":
    main()
