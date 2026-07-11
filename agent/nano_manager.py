from __future__ import annotations
import json as _json
import uuid
from pathlib import Path
from typing import Optional

from langchain_core.messages import AIMessage, HumanMessage, SystemMessage
from langgraph.prebuilt import create_react_agent
from langgraph.checkpoint.memory import InMemorySaver

from .config import Settings
from . import model_factory as mf
from .mcp_server import LLM_TOOL_NAMES
from .skills_registry import SkillRegistry, build_skill_menu, make_load_skill_tool

MANAGER_PROMPT = (
    "You are a careful personal banking manager for ONE client. Answer only from the "
    "client's real data (use your tools to look it up); never fabricate balances or "
    "transactions, and say plainly when you do not know. You may move money only when the "
    "client explicitly instructs it, and only via the propose_* tools — proposing does NOT "
    "move money; the client must CONFIRM the exact proposed action before it executes. "
    "Whenever you propose a money movement, clearly restate its exact details before asking "
    "to confirm — the amount, the origin (source) account, and the target (destination) "
    "account — and ask the client (or the client's agent) to confirm; do nothing until they "
    "do. Never claim a transfer is done from a proposal alone. Do not act proactively."
    " You have skills listed under 'Available skills'; before advising on a "
    "product or on personal finance/investing, call load_skill(name) for the "
    "relevant one and follow its guidance. Recommendations are advice only — "
    "money still moves only via the confirm-gated propose_* tools."
    " For Interac e-Transfers: a recipient must be a registered payee first "
    "(register_interac_recipient); then propose_interac_transfer proposes a "
    "confirm-gated send to that payee's email — never send to an unregistered "
    "recipient."
)

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


def agent_tools(all_tools):
    return [t for t in all_tools if getattr(t, "name", None) in LLM_TOOL_NAMES]


def _mcp_session(settings: Settings, customer_id: str, token: Optional[str]):
    """Per-request MCP client bound to a customer via trusted headers."""
    from langchain_mcp_adapters.client import MultiServerMCPClient
    return MultiServerMCPClient({
        "nano": {
            "url": settings.mcp_url,
            "transport": "streamable_http",
            "headers": {"X-Nano-Customer": customer_id, **({"X-Nano-Token": token} if token else {})},
        }
    })


async def assist(settings: Settings, customer_id: str, token: Optional[str],
                 message: str, thread_id: Optional[str] = None) -> dict:
    thread_id = thread_id or f"{customer_id}-{uuid.uuid4().hex[:6]}"
    client = _mcp_session(settings, customer_id, token)
    all_tools = await client.get_tools()
    tools = agent_tools(all_tools)

    # server-side snapshot + recall (code, not the LLM) -> a context system message
    async def _call(name, **kw):
        for t in all_tools:
            if t.name == name:
                return await t.ainvoke(kw)
        return None
    snapshot = await _call("get_accounts")
    recalled = await _call("recall", query=message, k=4)
    held = _held_account_types(snapshot)
    context = SystemMessage(f"<client_snapshot>\n{snapshot}\n</client_snapshot>\n"
                            f"<durable_memory>\n{recalled}\n</durable_memory>\n"
                            f"{_skills_section(held)}")
    tools = tools + [make_load_skill_tool(_SKILLS)]

    agent = create_react_agent(mf.llm("fast"), tools, prompt=MANAGER_PROMPT,
                               checkpointer=InMemorySaver())
    out = await agent.ainvoke(
        {"messages": [context, HumanMessage(message)]},
        config={"configurable": {"thread_id": thread_id}, "recursion_limit": 40})

    answer, pending = "(no answer)", None
    for m in reversed(out["messages"]):
        if isinstance(m, AIMessage) and (m.content or "").strip():
            answer = m.content
            break
    import json

    def _texts(content):
        # Tool-message content may be a plain string or MCP content blocks
        # ([{"type": "text", "text": "<json>"}...]); yield candidate strings.
        if isinstance(content, str):
            return [content]
        if isinstance(content, list):
            return [b["text"] if isinstance(b, dict) and "text" in b else b
                    for b in content if isinstance(b, (dict, str))]
        return []

    for m in out["messages"]:
        for tc in _texts(getattr(m, "content", None)):
            if not (isinstance(tc, str) and '"id"' in tc and "expires_at" in tc):
                continue
            try:
                obj = json.loads(tc)
            except Exception:  # noqa: BLE001
                continue
            if isinstance(obj, dict) and obj.get("id") and not obj.get("denied"):
                pending = obj

    await _call("remember", fact=f"User asked: {message}", kind="user")
    await _call("remember", fact=f"Manager answered: {answer[:400]}", kind="assistant")
    res = {"answer": answer, "thread_id": thread_id}
    if pending:
        res["pending_action"] = pending
    return res
