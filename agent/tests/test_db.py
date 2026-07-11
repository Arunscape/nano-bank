from agent.db import ClientContext


class FakeCtx(ClientContext):
    def __init__(self, tables):
        self._tables = tables  # dict: name -> list[dict]

    def _rows(self, sql, params):
        # crude router: pick table by a marker in the SQL comment
        if "-- accounts" in sql:
            return self._tables.get("accounts", [])
        if "-- transactions" in sql:
            return self._tables.get("transactions", [])
        if "-- profile" in sql:
            return self._tables.get("profile", [])
        if "-- owns" in sql:
            return self._tables.get("owns", [])
        if "-- interac_recipients" in sql:
            self.last = (sql, params)
            return self._tables.get("recipients", [])
        if "-- recipient" in sql:
            self.last = (sql, params)
            return self._tables.get("recipient", [])
        return []


def test_snapshot_includes_name_and_balance():
    ctx = FakeCtx({
        "profile": [{"first_name": "Ada", "last_name": "L", "email": "a@x.ca",
                     "kyc_status": "Verified"}],
        "accounts": [{"account_id": "acc-1", "account_type": "chequing",
                      "balance": "1200.00", "status": "active"}],
        "transactions": [{"transaction_type": "deposit", "amount": "1200.00",
                          "created_at": "2026-07-01"}],
    })
    snap = ctx.snapshot("cust-1")
    assert "Ada" in snap and "1200.00" in snap and "chequing" in snap


def test_owns_account_true_false():
    ctx = FakeCtx({"owns": [{"n": 1}]})
    assert ctx.owns_account("cust-1", "acc-1") is True
    ctx2 = FakeCtx({"owns": []})
    assert ctx2.owns_account("cust-1", "acc-9") is False


def test_interac_recipients_query_shape():
    ctx = FakeCtx({"recipients": [{"recipient_id": "r1", "email": "a@b.ca",
                                   "display_name": "Ada", "created_at": "t"}]})
    out = ctx.interac_recipients("cust-1")
    assert out[0]["email"] == "a@b.ca"
    sql, params = ctx.last
    assert "interac_recipients" in sql and "status = 'active'" in sql
    assert params == ("cust-1",)


def test_recipient_by_id_returns_none_when_absent():
    assert FakeCtx({"recipient": []}).recipient("cust-1", "nope") is None
    ctx = FakeCtx({"recipient": [{"recipient_id": "r1", "email": "a@b.ca"}]})
    assert ctx.recipient("cust-1", "r1")["email"] == "a@b.ca"
