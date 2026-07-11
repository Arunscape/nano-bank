from __future__ import annotations
import time
import uuid
from dataclasses import dataclass, asdict
from decimal import Decimal, InvalidOperation
from typing import Callable, Optional


class ActDenied(Exception):
    """Refused at propose time (policy)."""


class ActError(Exception):
    """Refused/failed at execute time."""


@dataclass
class PendingAction:
    id: str
    customer_id: str
    kind: str
    amount: str
    from_account: Optional[str]
    to_account: Optional[str]
    memo: Optional[str]
    payee_email: Optional[str]
    security_question: Optional[str]
    security_answer: Optional[str]
    created_at: float
    expires_at: float
    status: str = "pending"   # pending | executed | cancelled
    result: Optional[dict] = None


_KINDS = {"transfer", "deposit", "withdraw", "interac"}


class ActionStore:
    def __init__(self, db, bank, audit, max_per_tx: Decimal, ttl_s: int,
                 now: Callable[[], float] = time.time):
        self.db = db
        self.bank = bank
        self.audit = audit
        self.max = max_per_tx
        self.ttl = ttl_s
        self.now = now
        self._pending: dict[str, PendingAction] = {}

    def _amount(self, amount) -> Decimal:
        try:
            a = Decimal(str(amount))
        except (InvalidOperation, ValueError):
            raise ActDenied(f"invalid amount: {amount!r}")
        if a <= 0:
            raise ActDenied("amount must be positive")
        return a

    def propose(self, customer_id, token, kind, *, amount,
                from_account=None, to_account=None, memo=None, payee_email=None,
                security_question=None, security_answer=None) -> dict:
        if kind not in _KINDS:
            raise ActDenied(f"unknown kind: {kind}")
        a = self._amount(amount)
        if a > self.max:
            self._audit(customer_id, kind, a, "denied", "over cap")
            raise ActDenied(f"amount {a} exceeds per-transaction cap {self.max}")
        # ownership: any account the customer names as *theirs* must belong to them.
        for acct in ((from_account,) if kind in ("transfer", "withdraw") else (to_account,)):
            if acct and not self.db.owns_account(customer_id, acct):
                self._audit(customer_id, kind, a, "denied", f"account {acct} not owned")
                raise ActDenied(f"account {acct} is not yours")
        if kind == "transfer" and not (from_account and to_account):
            raise ActDenied("transfer needs from_account and to_account")
        if kind == "interac":
            if not from_account:
                raise ActDenied("interac needs a from_account")
            if not self.db.owns_account(customer_id, from_account):
                self._audit(customer_id, kind, a, "denied", f"account {from_account} not owned")
                raise ActDenied(f"account {from_account} is not yours")
            emails = {r.get("email") for r in self.db.interac_recipients(customer_id)}
            if not payee_email or payee_email not in emails:
                self._audit(customer_id, kind, a, "denied", "unregistered payee")
                raise ActDenied(f"'{payee_email}' is not a registered recipient")
        pid = uuid.uuid4().hex
        now = self.now()
        pa = PendingAction(id=pid, customer_id=customer_id, kind=kind, amount=str(a),
                           from_account=from_account, to_account=to_account, memo=memo,
                           payee_email=payee_email,
                           security_question=security_question,
                           security_answer=security_answer,
                           created_at=now, expires_at=now + self.ttl)
        self._pending[pid] = pa
        self._audit(customer_id, kind, a, "proposed", "", action_id=pid)
        return {"id": pid, "kind": kind, "amount": str(a), "from": from_account,
                "to": to_account, "expires_at": pa.expires_at, "summary": self._summary(pa)}

    def execute(self, action_id, customer_id, token) -> dict:
        pa = self._pending.get(action_id)
        if pa is None or pa.customer_id != customer_id:
            raise ActError("unknown action")
        if pa.status == "executed":
            return pa.result                      # idempotent replay
        if pa.status == "cancelled":
            raise ActError("action cancelled")
        if self.now() > pa.expires_at:
            self._audit(customer_id, pa.kind, Decimal(pa.amount), "expired", "")
            raise ActError("action expired")
        if Decimal(pa.amount) > self.max:
            raise ActError("over cap")
        try:
            if pa.kind == "transfer":
                res = self.bank.transfer(token, pa.from_account, pa.to_account, pa.amount,
                                         memo=pa.memo, idempotency_key=pa.id)
            elif pa.kind == "deposit":
                res = self.bank.deposit(token, pa.to_account, pa.amount, idempotency_key=pa.id)
            elif pa.kind == "interac":
                res = self.bank.send_etransfer(
                    token, pa.from_account, pa.amount,
                    recipient_handle_value=pa.payee_email,
                    security_question=pa.security_question,
                    security_answer=pa.security_answer,
                    memo=pa.memo, idempotency_key=pa.id)
            else:
                res = self.bank.withdraw(token, pa.from_account, pa.amount, idempotency_key=pa.id)
        except Exception as e:  # noqa: BLE001
            self._audit(customer_id, pa.kind, Decimal(pa.amount), "failed", str(e), action_id=pa.id)
            raise ActError(f"bank rejected: {e}") from e
        pa.status = "executed"
        pa.result = res
        self._audit(customer_id, pa.kind, Decimal(pa.amount), "executed", "", action_id=pa.id)
        return res

    def cancel(self, action_id, customer_id) -> dict:
        pa = self._pending.get(action_id)
        if pa is None or pa.customer_id != customer_id:
            raise ActError("unknown action")
        pa.status = "cancelled"
        self._audit(customer_id, pa.kind, Decimal(pa.amount), "cancelled", "", action_id=pa.id)
        return {"id": action_id, "status": "cancelled"}

    def get(self, action_id, customer_id):
        pa = self._pending.get(action_id)
        return asdict(pa) if pa and pa.customer_id == customer_id else None

    def _summary(self, pa: PendingAction) -> str:
        if pa.kind == "transfer":
            return f"Transfer {pa.amount} from {pa.from_account} to {pa.to_account}" + \
                   (f" ({pa.memo})" if pa.memo else "")
        if pa.kind == "deposit":
            return f"Deposit {pa.amount} into {pa.to_account}"
        if pa.kind == "interac":
            return f"Interac e-Transfer {pa.amount} from {pa.from_account} to {pa.payee_email}" + \
                   (f" ({pa.memo})" if pa.memo else "")
        return f"Withdraw {pa.amount} from {pa.from_account}"

    def _audit(self, customer_id, kind, amount, outcome, reason, action_id=None):
        self.audit.record({"customer_id": customer_id, "kind": kind, "amount": str(amount),
                           "outcome": outcome, "reason": reason, "action_id": action_id})
