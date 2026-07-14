from agent.bank import BankClient


class _CapturingHTTP:
    def __init__(self): self.last = None
    def post(self, url, json=None, headers=None):
        self.last = {"url": url, "json": json, "headers": headers}
        class _R:
            status_code = 200
            def json(self): return {"transaction_id": "t1"}
        return _R()


def test_transfer_sends_idempotency_key_in_body():
    http = _CapturingHTTP()
    bank = BankClient("http://bank", http=http)
    bank.transfer("tok", "A1", "A2", "50", memo="rent", idempotency_key="idem-123")
    assert http.last["json"]["idempotency_key"] == "idem-123"
