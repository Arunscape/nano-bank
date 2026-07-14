from fastapi.testclient import TestClient
from agent.config import Settings
from agent.api import create_app
from agent.mandate_gateway import Decision


_ENV = {"AGENT_GATEWAY_TOKEN": "gw", "AGENT_MANDATE_ID": "M1", "AGENT_CUSTOMER_ID": "C1",
        "AGENT_BILLER_ACCOUNT_ID": "EPCOR", "BRANCH_SERVICE_TOKEN": "svc"}


class FakePEP:
    def __init__(self, allowed, reason="ok"):
        self._allowed, self._reason = allowed, reason
    def check(self, mandate_id, scope, amount=None):
        return Decision(self._allowed, {"mandate_id": mandate_id}, self._reason)


class FakeClient:
    def __init__(self): self.transfers = []
    def list_mandates(self): return [{"mandate_id": "M1", "scopes": ["transfer:initiate"],
                                      "account_id": "A1", "max_per_tx": "100"}]
    def mint_token(self, mid): return "tok-" + mid
    def agent_transfer(self, token, to, amount, desc, idem):
        self.transfers.append({"to": to, "amount": str(amount), "desc": desc}); return 201, {"transaction_id": "t1"}


def _app(pep_allowed=True):
    s = Settings.from_env(_ENV)
    return TestClient(create_app(s, mandate_client=FakeClient(),
                                 mandate_pep=FakePEP(pep_allowed, "policy"))), s


def test_gateway_requires_its_token():
    c, _ = _app()
    assert c.post("/agent-gateway/act", json={"operation": "transfer_out", "params": {}}).status_code == 401


def test_act_transfer_denied_when_pep_denies():
    c, _ = _app(pep_allowed=False)
    r = c.post("/agent-gateway/act", headers={"Authorization": "Bearer gw"},
               json={"operation": "transfer_out", "params": {"amount": "50"}})
    assert r.status_code == 200 and r.json()["decision"] == "deny"


def test_act_transfer_allow_defaults_to_biller():
    fc = FakeClient()
    s = Settings.from_env(_ENV)
    c = TestClient(create_app(s, mandate_client=fc, mandate_pep=FakePEP(True)))
    r = c.post("/agent-gateway/act", headers={"Authorization": "Bearer gw"},
               json={"operation": "transfer_out", "params": {"amount": "50"}})
    assert r.status_code == 200 and r.json()["decision"] == "allow"
    assert fc.transfers[-1]["to"] == "EPCOR" and fc.transfers[-1]["amount"] == "50"


def test_message_denied_when_pep_denies():
    c, _ = _app(pep_allowed=False)
    r = c.post("/agent-gateway/message", headers={"Authorization": "Bearer gw"},
               json={"message": "hi"})
    assert r.status_code == 200 and r.json()["answer"].startswith("(denied)")


def test_unknown_operation_is_400():
    c, _ = _app()
    r = c.post("/agent-gateway/act", headers={"Authorization": "Bearer gw"},
               json={"operation": "nope", "params": {}})
    assert r.status_code == 400


class _ParkingClient(FakeClient):
    def agent_transfer(self, token, to, amount, desc, idem):
        return 202, {"approval_id": "AP1", "status": "pending"}


def test_act_transfer_over_cap_is_pending_approval():
    s = Settings.from_env(_ENV)
    c = TestClient(create_app(s, mandate_client=_ParkingClient(), mandate_pep=FakePEP(True)))
    r = c.post("/agent-gateway/act", headers={"Authorization": "Bearer gw"},
               json={"operation": "transfer_out", "params": {"amount": "100"}})
    body = r.json()
    assert body["decision"] == "pending_approval" and body["approval_id"] == "AP1"


class _DenyingClient(FakeClient):
    # The PEP passes (scope + max_per_tx ok) but the bank rejects the payee at
    # the transaction layer — allowed_payees is enforced there, returning 403.
    def agent_transfer(self, token, to, amount, desc, idem):
        return 403, {"error": {"code": "POLICY_DENIED", "message": "PAYEE_NOT_ALLOWED"}}


def test_act_transfer_bank_403_is_deny_not_allow():
    s = Settings.from_env(_ENV)
    c = TestClient(create_app(s, mandate_client=_DenyingClient(), mandate_pep=FakePEP(True)))
    r = c.post("/agent-gateway/act", headers={"Authorization": "Bearer gw"},
               json={"operation": "transfer_out",
                     "params": {"amount": "25", "to_account_id": "FOREIGN"}})
    body = r.json()
    assert body["decision"] == "deny", body
    assert body["http"] == 403
    assert "PAYEE_NOT_ALLOWED" in (body.get("reason") or "")


def test_act_transfer_uses_supplied_idempotency_key():
    fc = FakeClient()
    fc.keys = []
    orig = fc.agent_transfer
    def _cap(token, to, amount, desc, idem):
        fc.keys.append(idem); return orig(token, to, amount, desc, idem)
    fc.agent_transfer = _cap
    s = Settings.from_env(_ENV)
    c = TestClient(create_app(s, mandate_client=fc, mandate_pep=FakePEP(True)))
    c.post("/agent-gateway/act", headers={"Authorization": "Bearer gw"},
           json={"operation": "transfer_out", "params": {"amount": "50", "idempotency_key": "K1"}})
    assert fc.keys[-1] == "K1"
