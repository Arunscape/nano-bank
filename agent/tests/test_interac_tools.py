from agent.mcp_server import LLM_TOOL_NAMES


def test_interac_tools_are_in_llm_toolset():
    assert {"register_interac_recipient", "list_interac_recipients",
            "remove_interac_recipient", "propose_interac_transfer"} <= LLM_TOOL_NAMES


def test_deps_has_bank_field():
    import dataclasses
    from agent.mcp_server import Deps
    fields = {f.name for f in dataclasses.fields(Deps)}
    assert "bank" in fields
