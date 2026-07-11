import asyncio
import pytest
from agent.config import Settings
from agent.bank import BankClient
from agent import seed, model_factory as mf, nano_manager

pytestmark = pytest.mark.live


def test_two_phase_transfer_end_to_end():
    settings = Settings.from_env()
    mf.init_models(settings)
    bank = BankClient(settings.nano_bank_api)
    demo = seed.seed_demo(bank)
    ada, bo = demo["customers"]

    # ask -> cites a balance
    r1 = asyncio.run(nano_manager.assist(settings, ada["customer_id"],
        bank.login(ada["email"], ada["password"]), "what is my balance?"))
    assert "answer" in r1

    # instruct transfer -> pending_action, money NOT moved yet
    tok = bank.login(ada["email"], ada["password"])
    r2 = asyncio.run(nano_manager.assist(settings, ada["customer_id"], tok,
        f"transfer 25 from {ada['account_id']} to {bo['account_id']}"))
    assert r2.get("pending_action"), "manager must propose, not auto-execute"
