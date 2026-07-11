import pytest
from agent.config import Settings
from agent import model_factory as mf


def _settings():
    return Settings.from_env({"OLLAMA_API_KEY": "x"})


def test_resolver_picks_primary_when_it_probes():
    s = _settings()
    assert mf.resolve_model(s, probe=lambda model, st: model == "glm-5.2") == "glm-5.2"


def test_resolver_falls_back_when_primary_fails():
    s = _settings()
    assert mf.resolve_model(s, probe=lambda model, st: model == "glm-4.7") == "glm-4.7"


def test_resolver_raises_when_both_fail():
    s = _settings()
    with pytest.raises(RuntimeError):
        mf.resolve_model(s, probe=lambda model, st: False)


def test_llm_requires_init(monkeypatch):
    monkeypatch.setattr(mf, "_RESOLVED", None, raising=False)
    with pytest.raises(RuntimeError):
        mf.llm("fast")
