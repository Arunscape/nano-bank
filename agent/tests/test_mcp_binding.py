import pytest
from agent import mcp_server as M


def test_current_customer_requires_binding():
    with pytest.raises(LookupError):
        M.current_customer()


def test_bind_sets_and_clears():
    with M.bind("cust-1", "tok-1"):
        assert M.current_customer() == "cust-1"
        assert M.current_token() == "tok-1"
    with pytest.raises(LookupError):
        M.current_customer()


def test_tool_partitions_are_disjoint_and_exclude_execute_from_llm():
    assert "execute_action" in M.CONFIRM_ONLY_TOOL_NAMES
    assert "execute_action" not in M.LLM_TOOL_NAMES
    assert "cancel_action" not in M.LLM_TOOL_NAMES
    assert M.LLM_TOOL_NAMES.isdisjoint(M.CONFIRM_ONLY_TOOL_NAMES)
    # no LLM tool name hints at a customer/token parameter
    assert all("customer" not in n for n in M.LLM_TOOL_NAMES)
