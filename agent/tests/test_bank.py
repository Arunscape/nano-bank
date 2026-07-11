import json
import httpx
import pytest
from agent.bank import BankClient, BankError


def _client(handler):
    transport = httpx.MockTransport(handler)
    return BankClient("http://bank.test", http=httpx.Client(transport=transport))


def test_transfer_sends_token_amount_and_idempotency():
    seen = {}

    def handler(req: httpx.Request) -> httpx.Response:
        seen["url"] = str(req.url)
        seen["auth"] = req.headers.get("authorization")
        seen["idem"] = req.headers.get("idempotency-key")
        seen["body"] = json.loads(req.content)
        return httpx.Response(201, json={"transaction_id": "t1"})

    bank = _client(handler)
    out = bank.transfer("jwt-abc", "acc-from", "acc-to", "50.00",
                        memo="rent", idempotency_key="act-1")
    assert out["transaction_id"] == "t1"
    assert seen["url"].endswith("/api/v1/transactions/transfer")
    assert seen["auth"] == "Bearer jwt-abc"
    assert seen["idem"] == "act-1"
    assert seen["body"]["amount"] == "50.00"


def _capture_body():
    seen = {}

    def handler(req: httpx.Request) -> httpx.Response:
        seen["body"] = json.loads(req.content)
        return httpx.Response(201, json={"transaction_id": "t1"})

    return seen, handler


def test_deposit_includes_required_description():
    seen, handler = _capture_body()
    _client(handler).deposit("jwt", "acc-1", "1000")
    assert seen["body"]["account_id"] == "acc-1"
    assert seen["body"]["amount"] == "1000"
    assert seen["body"].get("description"), "bank-api DepositRequest requires description"


def test_withdraw_includes_required_description():
    seen, handler = _capture_body()
    _client(handler).withdraw("jwt", "acc-1", "10")
    assert seen["body"].get("description"), "bank-api WithdrawalRequest requires description"


def test_transfer_includes_required_description():
    seen, handler = _capture_body()
    _client(handler).transfer("jwt", "a", "b", "5", memo="rent")
    assert seen["body"].get("description"), "bank-api MoneyTransferRequest requires description"


def test_non_2xx_raises_bankerror():
    bank = _client(lambda req: httpx.Response(422, json={"error": {"message": "insufficient"}}))
    with pytest.raises(BankError) as ei:
        bank.deposit("jwt", "acc", "10")
    assert ei.value.status == 422


def test_register_recipient_posts_email_and_name():
    seen = {}

    def handler(req: httpx.Request) -> httpx.Response:
        seen["url"] = str(req.url)
        seen["auth"] = req.headers.get("authorization")
        seen["body"] = json.loads(req.content)
        return httpx.Response(201, json={"recipient_id": "r1", "email": "a@b.ca"})

    out = _client(handler).register_recipient("jwt", "a@b.ca", "Ada")
    assert out["recipient_id"] == "r1"
    assert seen["url"].endswith("/api/v1/customers/interac-recipients")
    assert seen["auth"] == "Bearer jwt"
    assert seen["body"] == {"email": "a@b.ca", "display_name": "Ada"}


def test_remove_recipient_deletes_by_id():
    seen = {}

    def handler(req: httpx.Request) -> httpx.Response:
        seen["method"] = req.method
        seen["url"] = str(req.url)
        seen["auth"] = req.headers.get("authorization")
        return httpx.Response(204)

    _client(handler).remove_recipient("jwt", "r1")
    assert seen["method"] == "DELETE"
    assert seen["url"].endswith("/api/v1/customers/interac-recipients/r1")
    assert seen["auth"] == "Bearer jwt"


def test_send_etransfer_posts_to_the_interac_rail():
    seen = {}

    def handler(req: httpx.Request) -> httpx.Response:
        seen["url"] = str(req.url)
        seen["auth"] = req.headers.get("authorization")
        seen["body"] = json.loads(req.content)
        return httpx.Response(201, json={"etransfer_id": "e1", "status": "held"})

    out = _client(handler).send_etransfer(
        "jwt", "acc-1", "30", "sam@example.ca",
        security_question="pet?", security_answer="rex", memo="rent", idempotency_key="act-1")
    assert out["etransfer_id"] == "e1"
    assert seen["url"].endswith("/api/v1/interac/etransfers")
    assert seen["auth"] == "Bearer jwt"
    b = seen["body"]
    assert b["from_account_id"] == "acc-1" and b["amount"] == "30"
    assert b["recipient_handle_type"] == "email" and b["recipient_handle_value"] == "sam@example.ca"
    assert b["security_question"] == "pet?" and b["security_answer"] == "rex"
    assert b["idempotency_key"] == "act-1"
