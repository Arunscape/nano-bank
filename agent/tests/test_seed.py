from agent import seed


class FakeBank:
    def create_customer(self, payload):
        return {"customer_id": "c-" + payload["email"]}
    def login(self, email, password): return "jwt-" + email
    def create_account(self, token, payload):
        return {"account_id": "a-" + payload["customer_id"]}
    def deposit(self, token, account_id, amount, idempotency_key=None):
        return {"transaction_id": "d1", "amount": str(amount)}


def test_seed_customer_records_creds():
    bank, store = FakeBank(), seed.CredStore()
    out = seed.seed_customer(bank, store, first="Ada", last="L",
                             email="ada@x.ca", password="pw123456")
    assert out["customer_id"] == "c-ada@x.ca"
    assert store.get("c-ada@x.ca") == ("ada@x.ca", "pw123456")


def test_seed_demo_creates_two_customers_and_funds():
    bank = FakeBank()
    out = seed.seed_demo(bank)
    assert len(out["customers"]) == 2
    assert out["customers"][0]["account_id"].startswith("a-")
