from fastapi.testclient import TestClient
from agent.config import Settings
from agent.api import create_app
from agent import nano_manager


class _FakeTool:
    def __init__(self, name, result):
        self.name = name
        self._result = result

    async def ainvoke(self, kw):
        return self._result


class _FakeClient:
    def __init__(self, tools):
        self._tools = tools

    async def get_tools(self):
        return self._tools


_ACCOUNTS = [{"account_id": "acc-1", "account_type": "chequing",
              "balance": "1000.00", "status": "active"}]
_TXNS = [{"transaction_type": "deposit", "amount": "1000.00",
          "created_at": "2026-07-09"}]


def _app_with_mcp(monkeypatch, tools):
    settings = Settings.from_env({"BRANCH_SERVICE_TOKEN": "svc"})
    monkeypatch.setattr(nano_manager, "_mcp_session",
                        lambda settings, cid, token: _FakeClient(tools))

    class R:
        def resolve(self, cid):
            return "jwt-" + cid

    return TestClient(create_app(settings, token_resolver=R()))


def _app():
    settings = Settings.from_env({"BRANCH_SERVICE_TOKEN": "svc"})

    async def fake_assist(settings, cid, token, message, thread_id=None):
        return {"answer": f"hi {cid}", "thread_id": "th1",
                "pending_action": {"id": "act-1", "summary": "Transfer 50"}}

    async def fake_confirm(settings, cid, token, action_id, cancel=False):
        return {"status": "cancelled"} if cancel else {"transaction_id": "t1"}

    class R:
        def resolve(self, cid): return "jwt-" + cid

    return TestClient(create_app(settings, assist_fn=fake_assist,
                                 confirm_fn=fake_confirm, token_resolver=R()))


def test_message_requires_service_token():
    c = _app()
    r = c.post("/branch/clients/cust-1/message", json={"message": "hi"})
    assert r.status_code == 401


def test_message_returns_pending_action():
    c = _app()
    r = c.post("/branch/clients/cust-1/message", json={"message": "transfer 50"},
               headers={"Authorization": "Bearer svc"})
    assert r.status_code == 200
    assert r.json()["pending_action"]["id"] == "act-1"


def test_confirm_executes():
    c = _app()
    r = c.post("/branch/clients/cust-1/actions/act-1/confirm",
               headers={"Authorization": "Bearer svc"})
    assert r.status_code == 200 and r.json()["transaction_id"] == "t1"


def test_health_ok():
    assert _app().get("/health").status_code == 200


def test_seed_route_requires_auth_and_returns_customers():
    settings = Settings.from_env({"BRANCH_SERVICE_TOKEN": "svc"})
    app = create_app(settings, seed_fn=lambda: {"customers": [{"customer_id": "c1"}]})
    c = TestClient(app)
    assert c.post("/branch/seed").status_code == 401
    r = c.post("/branch/seed", headers={"Authorization": "Bearer svc"})
    assert r.status_code == 200 and r.json()["customers"][0]["customer_id"] == "c1"


def test_no_seed_route_when_seed_fn_absent():
    c = _app()
    assert c.post("/branch/seed", headers={"Authorization": "Bearer svc"}).status_code == 404


def test_accounts_endpoint_returns_list(monkeypatch):
    c = _app_with_mcp(monkeypatch, [_FakeTool("get_accounts", _ACCOUNTS)])
    r = c.get("/branch/clients/cust-1/accounts", headers={"Authorization": "Bearer svc"})
    assert r.status_code == 200
    assert isinstance(r.json(), list)
    assert r.json()[0]["account_id"] == "acc-1"


def test_transactions_endpoint_returns_list(monkeypatch):
    c = _app_with_mcp(monkeypatch, [_FakeTool("get_transactions", _TXNS)])
    r = c.get("/branch/clients/cust-1/transactions", headers={"Authorization": "Bearer svc"})
    assert r.status_code == 200
    assert isinstance(r.json(), list)
    assert r.json()[0]["transaction_type"] == "deposit"


def test_accounts_endpoint_requires_token(monkeypatch):
    c = _app_with_mcp(monkeypatch, [_FakeTool("get_accounts", _ACCOUNTS)])
    assert c.get("/branch/clients/cust-1/accounts").status_code == 401
