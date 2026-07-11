from agent import nano_manager as NM


class _T:
    def __init__(self, name): self.name = name


def test_agent_tools_excludes_execute_and_cancel():
    tools = [_T("get_accounts"), _T("propose_transfer"), _T("execute_action"),
             _T("cancel_action"), _T("recall")]
    kept = {t.name for t in NM.agent_tools(tools)}
    assert "execute_action" not in kept and "cancel_action" not in kept
    assert {"get_accounts", "propose_transfer", "recall"} <= kept


def test_manager_prompt_mentions_read_and_confirm():
    p = NM.MANAGER_PROMPT.lower()
    assert "confirm" in p and ("never fabricate" in p or "do not fabricate" in p)


def test_manager_prompt_requires_stated_transaction_details():
    p = NM.MANAGER_PROMPT.lower()
    # proposals must restate the transaction details before asking to confirm
    assert "amount" in p
    assert "origin" in p or "source" in p
    assert "target" in p or "destination" in p


def test_prompt_mentions_registered_payee():
    p = NM.MANAGER_PROMPT.lower()
    assert "register" in p and ("payee" in p or "recipient" in p)


def test_skill_menu_reflects_holdings():
    from agent.nano_manager import _held_account_types, _skills_section
    snapshot = [{"account_id": "a", "account_type": "chequing", "balance": "100", "status": "active"}]
    held = _held_account_types(snapshot)
    assert held == {"chequing"}
    section = _skills_section(held)
    assert "Available skills" in section
    assert "chequing" in section and "[held]" in section
    assert "savings" in section and "available — not held" in section


def test_held_account_types_handles_mcp_blocks():
    import json
    from agent.nano_manager import _held_account_types
    blocks = [{"type": "text",
               "text": json.dumps([{"account_type": "savings"}, {"account_type": "credit_card"}])}]
    assert _held_account_types(blocks) == {"savings", "credit_card"}
