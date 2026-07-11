import pytest


def pytest_configure(config):
    config.addinivalue_line("markers", "live: needs external services (DB/Ollama/bank); skipped by default")


def pytest_addoption(parser):
    parser.addoption("--run-live", action="store_true", default=False,
                     help="run @live tests that need external services")


def pytest_collection_modifyitems(config, items):
    if config.getoption("--run-live"):
        return
    skip = pytest.mark.skip(reason="needs --run-live")
    for item in items:
        if "live" in item.keywords:
            item.add_marker(skip)
