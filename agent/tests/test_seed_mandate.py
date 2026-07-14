from agent.seed import seed_agent_mandate


class _FakeBank:
    base = "http://bank"
    def __init__(self): self.created_accounts = 0
    def create_customer(self, payload): return {"customer_id": "epcor-cust"}
    def login(self, email, pw): return "epcor-tok"
    def create_account(self, tok, payload):
        self.created_accounts += 1
        return {"account_id": "EPCOR-ACCT"}


class _FakeMC:
    def __init__(self, *a, **k): self.mandate_payload = None
    def register_agent(self, name): return {"agent_id": "ag1", "agent_secret": "sec"}
    def create_mandate(self, tok, payload):
        self.mandate_payload = payload
        return {"mandate_id": "M1"}


def test_seeded_mandate_pins_the_epcor_payee(monkeypatch):
    fake_mc = _FakeMC()
    monkeypatch.setattr("agent.seed.MandateClient", lambda *a, **k: fake_mc)
    out = seed_agent_mandate(_FakeBank(), "cust-tok", "A1")
    # the biller must exist and be the sole allowed payee on the mandate
    assert out["epcor_account_id"] == "EPCOR-ACCT"
    assert fake_mc.mandate_payload["allowed_payees"] == ["EPCOR-ACCT"]
