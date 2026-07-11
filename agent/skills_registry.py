from __future__ import annotations
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


@dataclass
class Skill:
    name: str
    description: str
    kind: str                    # "product" | "advisory"
    product: Optional[str]       # account_type, for product skills
    body: str


def _parse(text: str) -> dict:
    meta, body = {}, text
    if text.startswith("---"):
        _, fm, body = text.split("---", 2)
        for line in fm.strip().splitlines():
            if ":" in line:
                k, v = line.split(":", 1)
                meta[k.strip()] = v.strip()
    return {"meta": meta, "body": body.strip()}


class SkillRegistry:
    def __init__(self, skills: list[Skill]):
        self._by_name = {s.name: s for s in skills}

    @classmethod
    def from_dir(cls, path) -> "SkillRegistry":
        skills = []
        for f in sorted(Path(path).glob("*.md")):
            p = _parse(f.read_text())
            m = p["meta"]
            skills.append(Skill(
                name=m.get("name", f.stem),
                description=m.get("description", ""),
                kind=m.get("kind", "advisory"),
                product=m.get("product") or None,
                body=p["body"]))
        return cls(skills)

    def all(self) -> list[Skill]:
        return list(self._by_name.values())

    def get(self, name: str) -> Optional[Skill]:
        return self._by_name.get(name)


def build_skill_menu(registry: "SkillRegistry", held_account_types: set) -> str:
    lines = []
    for s in registry.all():
        tag = ""
        if s.kind == "product":
            tag = " [held]" if s.product in held_account_types else " [available — not held]"
        lines.append(f"- {s.name} — {s.description}{tag}")
    return "\n".join(lines)


def make_load_skill_tool(registry: "SkillRegistry"):
    from langchain_core.tools import tool

    @tool
    def load_skill(name: str) -> str:
        """Load the full guidance for a named skill from the Available skills list."""
        s = registry.get(name)
        if s is None:
            return f"unknown skill '{name}'"
        return s.body

    return load_skill
