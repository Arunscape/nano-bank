from __future__ import annotations
import os
from dataclasses import dataclass
from decimal import Decimal
from typing import Mapping, Optional


@dataclass
class Settings:
    ollama_api_key: str
    ollama_base_url: str
    manager_model: str
    qdrant_url: str
    qdrant_collection: str
    db: dict
    nano_bank_api: str
    branch_service_token: str
    act_max_per_tx: Decimal
    confirm_ttl_s: int
    mcp_url: str
    branch_port: int
    console_port: int

    @classmethod
    def from_env(cls, env: Optional[Mapping[str, str]] = None) -> "Settings":
        e = os.environ if env is None else env

        def g(k, d=""):
            return e.get(k, d)

        return cls(
            ollama_api_key=g("OLLAMA_API_KEY"),
            ollama_base_url=g("OLLAMA_BASE_URL", "https://ollama.com/v1"),
            manager_model=g("MANAGER_MODEL", "glm-5.2"),
            qdrant_url=g("QDRANT_URL", "http://localhost:6335"),
            qdrant_collection=g("QDRANT_COLLECTION", "nano_manager_memory"),
            db=dict(
                host=g("DB_HOST", "::1"),
                port=int(g("DB_PORT", "5432")),
                dbname=g("DB_NAME", "nano_bank_db"),
                user=g("DB_USER", "nanobank_user"),
                password=g("DB_PASSWORD", "secure_nano_password_2024!"),
            ),
            nano_bank_api=g("NANO_BANK_API", "http://localhost:8081"),
            branch_service_token=g("BRANCH_SERVICE_TOKEN"),
            act_max_per_tx=Decimal(g("ACT_MAX_PER_TX", "1000")),
            confirm_ttl_s=int(g("CONFIRM_TTL_S", "300")),
            mcp_url=g("MCP_URL", "http://localhost:8087/mcp"),
            branch_port=int(g("BRANCH_PORT", "8086")),
            console_port=int(g("CONSOLE_PORT", "8505")),
        )
