from __future__ import annotations
from typing import Optional


class ClientContext:
    """Read-only Postgres access, always scoped to a customer_id."""

    def __init__(self, db_params: Optional[dict] = None):
        self._db = db_params

    # -- real connection (overridden in tests) --------------------------------
    def _rows(self, sql: str, params: tuple) -> list[dict]:
        import psycopg2
        import psycopg2.extras
        conn = psycopg2.connect(**self._db)
        try:
            conn.set_session(readonly=True, autocommit=True)
            with conn.cursor(cursor_factory=psycopg2.extras.RealDictCursor) as cur:
                cur.execute(sql, params)
                return [dict(r) for r in cur.fetchall()]
        finally:
            conn.close()

    def profile(self, customer_id: str) -> Optional[dict]:
        rows = self._rows(
            "-- profile\nSELECT first_name, last_name, email, kyc_status "
            "FROM customers WHERE customer_id = %s", (customer_id,))
        return rows[0] if rows else None

    def accounts(self, customer_id: str) -> list[dict]:
        return self._rows(
            "-- accounts\nSELECT account_id, account_type, balance, status "
            "FROM accounts WHERE customer_id = %s ORDER BY account_type", (customer_id,))

    def transactions(self, customer_id: str, limit: int = 20) -> list[dict]:
        return self._rows(
            "-- transactions\nSELECT t.transaction_type, te.entry_type, te.amount, t.created_at "
            "FROM transaction_entries te JOIN transactions t ON t.transaction_id = te.transaction_id "
            "JOIN accounts a ON a.account_id = te.account_id "
            "WHERE a.customer_id = %s ORDER BY t.created_at DESC LIMIT %s",
            (customer_id, limit))

    def cards(self, customer_id: str) -> list[dict]:
        return self._rows(
            "-- accounts\nSELECT account_id, account_type, balance, overdraft_limit, status "
            "FROM accounts WHERE customer_id = %s AND account_type = 'credit_card'",
            (customer_id,))

    def interac_recipients(self, customer_id: str) -> list[dict]:
        return self._rows(
            "-- interac_recipients\nSELECT recipient_id, email, display_name, created_at "
            "FROM interac_recipients WHERE customer_id = %s AND status = 'active' "
            "ORDER BY created_at DESC", (customer_id,))

    def recipient(self, customer_id: str, recipient_id: str) -> Optional[dict]:
        rows = self._rows(
            "-- recipient\nSELECT recipient_id, email, display_name, created_at "
            "FROM interac_recipients WHERE customer_id = %s AND recipient_id = %s "
            "AND status = 'active'", (customer_id, recipient_id))
        return rows[0] if rows else None

    def owns_account(self, customer_id: str, account_id: str) -> bool:
        rows = self._rows(
            "-- owns\nSELECT 1 AS n FROM accounts WHERE customer_id = %s AND account_id = %s",
            (customer_id, account_id))
        return len(rows) > 0

    def snapshot(self, customer_id: str) -> str:
        p = self.profile(customer_id) or {}
        accts = self.accounts(customer_id)
        txns = self.transactions(customer_id, limit=8)
        lines = [
            f"CLIENT: {p.get('first_name','?')} {p.get('last_name','')} "
            f"<{p.get('email','?')}> KYC={p.get('kyc_status','?')}",
            "ACCOUNTS:",
        ]
        for a in accts:
            lines.append(f"  - {a['account_type']} {a['account_id']}: "
                         f"balance {a['balance']} ({a['status']})")
        lines.append("RECENT TRANSACTIONS:")
        for t in txns:
            lines.append(f"  - {t['created_at']}: {t['transaction_type']} {t['amount']}")
        return "\n".join(lines)
