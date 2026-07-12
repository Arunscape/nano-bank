from __future__ import annotations
import logging
from functools import lru_cache
from typing import Callable, Optional

from langchain_core.messages import HumanMessage
from langchain_openai import ChatOpenAI

from .config import Settings

log = logging.getLogger("nano_manager.llm")

_RESOLVED: Optional[str] = None
_SETTINGS: Optional[Settings] = None


def build_model(model: str, settings: Settings, *, temperature: float = 0.2,
                max_tokens: Optional[int] = None) -> ChatOpenAI:
    kw = dict(model=model, temperature=temperature, base_url=settings.ollama_base_url,
              api_key=settings.ollama_api_key or "ollama", timeout=600)
    if max_tokens:
        kw["max_tokens"] = max_tokens
    return ChatOpenAI(**kw)


def _default_probe(model: str, settings: Settings) -> bool:
    try:
        m = build_model(model, settings, temperature=0.0, max_tokens=8)
        m.invoke([HumanMessage("reply with the single word: ok")])
        return True
    except Exception as e:  # noqa: BLE001 - probe must not raise
        log.warning("probe failed for %s: %s", model, e)
        return False


def resolve_model(settings: Settings, probe: Optional[Callable[[str, Settings], bool]] = None) -> str:
    probe = probe or _default_probe
    model = settings.manager_model
    if probe(model, settings):
        log.info("resolved model: %s", model)
        return model
    raise RuntimeError(f"{model} did not answer at {settings.ollama_base_url}")


def init_models(settings: Settings, probe: Optional[Callable[[str, Settings], bool]] = None) -> str:
    global _RESOLVED, _SETTINGS
    _SETTINGS = settings
    _RESOLVED = resolve_model(settings, probe)
    return _RESOLVED


@lru_cache(maxsize=8)
def _client(model: str, temperature: float, max_tokens: Optional[int]) -> ChatOpenAI:
    return build_model(model, _SETTINGS, temperature=temperature, max_tokens=max_tokens)


def llm(role: str = "fast", *, temperature: float = 0.2, max_tokens: Optional[int] = None) -> ChatOpenAI:
    if _RESOLVED is None or _SETTINGS is None:
        raise RuntimeError("call init_models(settings) before llm()")
    return _client(_RESOLVED, temperature, max_tokens)


def backend_healthcheck(settings: Settings) -> bool:
    try:
        return _default_probe(settings.manager_model, settings)
    except Exception:  # noqa: BLE001
        return False
