from __future__ import annotations
from typing import Optional
import httpx


class BankError(Exception):
    def __init__(self, status: int, body):
        super().__init__(f"nano-bank {status}: {body}")
        self.status = status
        self.body = body


class BankClient:
    def __init__(self, base_url: str, http: Optional[httpx.Client] = None):
        self.base = base_url.rstrip("/")
        self.http = http or httpx.Client(timeout=30)

    def _post(self, path: str, json: dict, token: Optional[str] = None,
              idempotency_key: Optional[str] = None) -> dict:
        headers = {}
        if token:
            headers["Authorization"] = f"Bearer {token}"
        if idempotency_key:
            headers["Idempotency-Key"] = idempotency_key
        r = self.http.post(self.base + path, json=json, headers=headers)
        if r.status_code // 100 != 2:
            raise BankError(r.status_code, _safe_json(r))
        return _safe_json(r)

    def login(self, email: str, password: str) -> str:
        out = self._post("/api/v1/auth/login", {"email": email, "password": password})
        return out.get("access_token") or out["token"]

    def deposit(self, token, account_id, amount, description="Deposit",
                idempotency_key=None) -> dict:
        return self._post("/api/v1/transactions/deposit",
                          {"account_id": account_id, "amount": str(amount),
                           "description": description},
                          token=token, idempotency_key=idempotency_key)

    def withdraw(self, token, account_id, amount, description="Withdrawal",
                 idempotency_key=None) -> dict:
        return self._post("/api/v1/transactions/withdrawal",
                          {"account_id": account_id, "amount": str(amount),
                           "description": description},
                          token=token, idempotency_key=idempotency_key)

    def transfer(self, token, from_account, to_account, amount, memo=None,
                 idempotency_key=None) -> dict:
        # bank-api MoneyTransferRequest requires `description`; the human memo maps to it.
        body = {"from_account_id": from_account, "to_account_id": to_account,
                "amount": str(amount), "description": memo or "Transfer"}
        return self._post("/api/v1/transactions/transfer", body,
                          token=token, idempotency_key=idempotency_key)

    def send_etransfer(self, token, from_account_id, amount, recipient_handle_value,
                       recipient_handle_type="email", security_question=None,
                       security_answer=None, memo=None, idempotency_key=None) -> dict:
        # Real Interac e-Transfer rail (held → claim/autodeposit). Non-autodeposit
        # recipients require a security question + answer.
        body = {"from_account_id": from_account_id, "amount": str(amount),
                "recipient_handle_type": recipient_handle_type,
                "recipient_handle_value": recipient_handle_value}
        if security_question:
            body["security_question"] = security_question
        if security_answer:
            body["security_answer"] = security_answer
        if memo:
            body["memo"] = memo
        if idempotency_key:
            body["idempotency_key"] = idempotency_key
        return self._post("/api/v1/interac/etransfers", body, token=token)

    def register_recipient(self, token, email, display_name) -> dict:
        return self._post("/api/v1/customers/interac-recipients",
                          {"email": email, "display_name": display_name},
                          token=token)

    def remove_recipient(self, token, recipient_id) -> None:
        headers = {"Authorization": f"Bearer {token}"} if token else {}
        r = self.http.request(
            "DELETE",
            self.base + f"/api/v1/customers/interac-recipients/{recipient_id}",
            headers=headers)
        if r.status_code // 100 != 2:
            raise BankError(r.status_code, _safe_json(r))

    def create_customer(self, payload: dict) -> dict:
        return self._post("/api/v1/customers", payload)

    def create_account(self, token, payload: dict) -> dict:
        return self._post("/api/v1/accounts", payload, token=token)


def _safe_json(r: httpx.Response):
    try:
        return r.json()
    except Exception:  # noqa: BLE001
        return {"raw": r.text}
