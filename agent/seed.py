from __future__ import annotations
import uuid

from .mandate_gateway import MandateClient


class CredStore:
    def __init__(self):
        self._d: dict = {}

    def put(self, customer_id, email, password):
        self._d[customer_id] = (email, password)

    def get(self, customer_id):
        return self._d.get(customer_id)

    def as_dict(self):
        return dict(self._d)


def seed_customer(bank, store: CredStore, *, first, last, email, password,
                  dob="1990-01-01", phone=None) -> dict:
    # bank enforces phone uniqueness too; generate a unique one unless given.
    phone = phone or f"+1555{uuid.uuid4().int % 10_000_000:07d}"
    out = bank.create_customer({
        "first_name": first, "last_name": last, "email": email,
        "phone_number": phone, "date_of_birth": dob, "password": password})
    cid = out["customer_id"]
    store.put(cid, email, password)
    return {"customer_id": cid, "email": email, "password": password}


def open_account(bank, token, customer_id, account_type="chequing") -> dict:
    return bank.create_account(token, {"customer_id": customer_id,
                                       "account_type": account_type})


def fund(bank, token, account_id, amount) -> dict:
    return bank.deposit(token, account_id, str(amount))


def _seed_epcor_biller(bank) -> str:
    """A stable 'Epcor Utilities' biller: a synthetic customer + active chequing
    account, the destination for the agent's mandate-capped bill payment."""
    tag = uuid.uuid4().hex[:8]
    email, pw = f"epcor.{tag}@biller.nano", "Biller!" + tag
    bank.create_customer({
        "first_name": "Epcor", "last_name": "Utilities", "email": email,
        "phone_number": f"+1555{uuid.uuid4().int % 10_000_000:07d}",
        "date_of_birth": "1990-01-01", "password": pw})
    tok = bank.login(email, pw)
    return bank.create_account(tok, {"account_type": "chequing"})["account_id"]


def seed_agent_mandate(bank, customer_token, account_id) -> dict:
    """Register an external agent and grant it a mandate on `account_id`, whose
    ONLY allowed payee is a freshly-seeded Epcor biller — so an LLM-planned
    destination that isn't Epcor is denied by the bank (PAYEE_NOT_ALLOWED)."""
    from datetime import datetime, timedelta, timezone
    epcor_account_id = _seed_epcor_biller(bank)   # create biller BEFORE the mandate
    mc = MandateClient(bank.base, "", "")
    agent = mc.register_agent("Demo External Agent")
    mandate = mc.create_mandate(customer_token, {
        "agent_id": agent["agent_id"], "account_id": account_id,
        "scopes": ["read:balance", "read:transactions", "transfer:initiate",
                   "account:open", "payee:register"],
        "max_per_tx": "100", "daily_cap": "500",
        "allowed_payees": [epcor_account_id],
        "expires_at": (datetime.now(timezone.utc) + timedelta(hours=1)).isoformat()})
    return {"agent_id": agent["agent_id"], "agent_secret": agent["agent_secret"],
            "mandate_id": mandate["mandate_id"], "epcor_account_id": epcor_account_id}


def seed_demo(bank) -> dict:
    store = CredStore()
    customers = []
    # Unique per run so re-seeding never collides with existing customers
    # (the bank enforces email uniqueness). No destructive DB wipe needed.
    tag = uuid.uuid4().hex[:6]
    for i, (first, email) in enumerate([("Ada", f"ada+{tag}@x.ca"),
                                        ("Bo", f"bo+{tag}@x.ca")]):
        c = seed_customer(bank, store, first=first, last="Demo", email=email,
                          password="pw12345678")
        token = bank.login(email, "pw12345678")
        acc = open_account(bank, token, c["customer_id"])
        if i == 0:
            fund(bank, token, acc["account_id"], "1000")
        customers.append({**c, "account_id": acc["account_id"]})
    return {"customers": customers, "creds": store.as_dict()}
