import textwrap
from pathlib import Path
from agent.skills_registry import SkillRegistry


def _write(dirpath, name, text):
    (dirpath / f"{name}.md").write_text(textwrap.dedent(text))


def test_registry_parses_frontmatter_and_body(tmp_path):
    _write(tmp_path, "savings", """\
        ---
        name: savings
        description: Servicing and recommending savings accounts.
        kind: product
        product: savings
        ---
        Guidance body line one.
        """)
    reg = SkillRegistry.from_dir(tmp_path)
    s = reg.get("savings")
    assert s is not None
    assert s.description.startswith("Servicing")
    assert s.kind == "product" and s.product == "savings"
    assert "Guidance body line one." in s.body


def test_registry_lists_all_and_missing_is_none(tmp_path):
    _write(tmp_path, "investment", """\
        ---
        name: investment
        description: Investment recommendations (not licensed advice).
        kind: advisory
        ---
        Body.
        """)
    reg = SkillRegistry.from_dir(tmp_path)
    assert [s.name for s in reg.all()] == ["investment"]
    assert reg.get("nope") is None


def test_seed_skills_load_from_repo():
    from pathlib import Path
    reg = SkillRegistry.from_dir(Path(__file__).resolve().parents[1] / "skills")
    names = {s.name for s in reg.all()}
    assert {"chequing", "savings", "credit_card", "personal-finance", "investment"} <= names
    prod = {s.name: s.product for s in reg.all() if s.kind == "product"}
    assert prod == {"chequing": "chequing", "savings": "savings", "credit_card": "credit_card"}
    assert reg.get("investment").kind == "advisory"


def test_build_menu_tags_by_holdings(tmp_path):
    from agent.skills_registry import build_skill_menu
    # two product skills + one advisory
    (tmp_path / "chequing.md").write_text("---\nname: chequing\ndescription: d1\nkind: product\nproduct: chequing\n---\nb")
    (tmp_path / "savings.md").write_text("---\nname: savings\ndescription: d2\nkind: product\nproduct: savings\n---\nb")
    (tmp_path / "investment.md").write_text("---\nname: investment\ndescription: d3\nkind: advisory\n---\nb")
    reg = SkillRegistry.from_dir(tmp_path)
    menu = build_skill_menu(reg, held_account_types={"chequing"})
    assert "chequing — d1 [held]" in menu
    assert "savings — d2 [available — not held]" in menu
    assert "investment — d3" in menu and "investment — d3 [" not in menu  # advisory untagged


def test_load_skill_tool_returns_body_or_error(tmp_path):
    from agent.skills_registry import make_load_skill_tool
    (tmp_path / "savings.md").write_text("---\nname: savings\ndescription: d\nkind: product\nproduct: savings\n---\nBODY-XYZ")
    tool = make_load_skill_tool(SkillRegistry.from_dir(tmp_path))
    assert "BODY-XYZ" in tool.invoke({"name": "savings"})
    assert "unknown" in tool.invoke({"name": "nope"}).lower()


def test_etransfer_skill_present_and_advisory():
    from pathlib import Path
    reg = SkillRegistry.from_dir(Path(__file__).resolve().parents[1] / "skills")
    s = reg.get("e-transfer")
    assert s is not None and s.kind == "advisory"
