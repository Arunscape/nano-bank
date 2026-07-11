"""Container entrypoint for the Agentic-Branch API.

Resolves the GLM model at startup, then serves the FastAPI app. A dev-only
SeedTokenResolver is wired so that seeding (POST /branch/seed) registers the
seeded customers' credentials in THIS process, letting the confirm path mint
each customer's nano-bank token (X-Nano-Token) — the LLM never sees it.
"""
from __future__ import annotations

import uvicorn

from .config import Settings
from . import model_factory as mf
from .api import create_app, SeedTokenResolver
from .bank import BankClient
from . import seed as seedmod


def build() -> "tuple":
    settings = Settings.from_env()
    mf.init_models(settings)
    resolver = SeedTokenResolver(settings, creds={})

    def seed_fn():
        out = seedmod.seed_demo(BankClient(settings.nano_bank_api))
        resolver.creds.update(out["creds"])
        return {"customers": out["customers"]}

    app = create_app(settings, token_resolver=resolver, seed_fn=seed_fn)
    return settings, app


if __name__ == "__main__":
    settings, app = build()
    uvicorn.run(app, host="0.0.0.0", port=settings.branch_port)
