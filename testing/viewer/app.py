"""nano-bank activity viewer.

The *output* side of the test harness: a live dashboard that taps the Postgres
database directly and shows what the bank is doing — customer registrations,
account openings, and credit-card payment activity (authorizations, captures,
and settlement batches over the mock Visa rails).

Reads from Postgres (not the API) on purpose — it's an observability tool that
watches the source of truth, independent of which API endpoints exist yet. The
synthetic "system" customer and its internal GL accounts are filtered out.

Config via env vars (defaults match nano-bank's local dev port-forward):
  DB_HOST  (default ::1)         DB_PORT (default 5432)
  DB_NAME  (default nano_bank_db) DB_USER (default nanobank_user)
  DB_PASSWORD (default secure_nano_password_2024!)
  REFRESH_SECONDS (default 3)
"""
from __future__ import annotations

import os

import pandas as pd
import psycopg2
import streamlit as st
from streamlit_autorefresh import st_autorefresh

DB = dict(
    host=os.getenv("DB_HOST", "::1"),
    port=int(os.getenv("DB_PORT", "5432")),
    dbname=os.getenv("DB_NAME", "nano_bank_db"),
    user=os.getenv("DB_USER", "nanobank_user"),
    password=os.getenv("DB_PASSWORD", "secure_nano_password_2024!"),
)
REFRESH_SECONDS = int(os.getenv("REFRESH_SECONDS", "3"))

# The card rails use a synthetic system customer + internal GL accounts; hide
# them so the dashboard only shows real, customer-facing activity.
SYSTEM_EMAIL = "system@nano.bank"
NOT_SYSTEM_CUSTOMER = f"email <> '{SYSTEM_EMAIL}'"
NOT_SYSTEM_ACCOUNT = (
    f"customer_id NOT IN (SELECT customer_id FROM customers WHERE email = '{SYSTEM_EMAIL}')"
)


def query(sql: str) -> pd.DataFrame:
    """Run one query on a fresh short-lived connection (dashboard is low-traffic)."""
    conn = psycopg2.connect(connect_timeout=5, **DB)
    try:
        return pd.read_sql_query(sql, conn)
    finally:
        conn.close()


def render_customers() -> None:
    total = int(query(f"SELECT count(*) AS n FROM customers WHERE {NOT_SYSTEM_CUSTOMER}")["n"][0])
    last_hour = int(query(
        f"SELECT count(*) AS n FROM customers "
        f"WHERE {NOT_SYSTEM_CUSTOMER} AND created_at >= now() - interval '1 hour'")["n"][0])
    by_kyc = query(
        f"SELECT kyc_status::text AS kyc_status, count(*) AS n "
        f"FROM customers WHERE {NOT_SYSTEM_CUSTOMER} GROUP BY kyc_status ORDER BY n DESC")

    c1, c2, c3 = st.columns(3)
    c1.metric("Total customers", f"{total:,}")
    c2.metric("Created (last hour)", f"{last_hour:,}")
    c3.metric("KYC statuses", len(by_kyc))

    # Creation rate over the last hour, per minute.
    rate = query(
        f"SELECT date_trunc('minute', created_at) AS minute, count(*) AS customers "
        f"FROM customers WHERE {NOT_SYSTEM_CUSTOMER} AND created_at >= now() - interval '1 hour' "
        f"GROUP BY 1 ORDER BY 1")
    if not rate.empty:
        st.caption("Customers created per minute (last hour)")
        st.bar_chart(rate.set_index("minute")["customers"], height=180)

    st.subheader("🧾 Recent customer registrations")
    feed = query(
        f"SELECT created_at, first_name, last_name, email, phone_number, "
        f"kyc_status::text AS kyc_status, customer_id "
        f"FROM customers WHERE {NOT_SYSTEM_CUSTOMER} ORDER BY created_at DESC LIMIT 200")
    st.dataframe(feed, use_container_width=True, hide_index=True,
                 column_config={"created_at": st.column_config.DatetimeColumn(
                     "created", format="HH:mm:ss")})


def render_accounts() -> None:
    total = int(query(f"SELECT count(*) AS n FROM accounts WHERE {NOT_SYSTEM_ACCOUNT}")["n"][0])
    last_hour = int(query(
        f"SELECT count(*) AS n FROM accounts "
        f"WHERE {NOT_SYSTEM_ACCOUNT} AND created_at >= now() - interval '1 hour'")["n"][0])
    by_type = query(
        f"SELECT account_type::text AS account_type, count(*) AS n, "
        f"       avg(interest_rate) AS avg_rate "
        f"FROM accounts WHERE {NOT_SYSTEM_ACCOUNT} GROUP BY account_type ORDER BY n DESC")

    c1, c2, c3 = st.columns(3)
    c1.metric("Total accounts", f"{total:,}")
    c2.metric("Opened (last hour)", f"{last_hour:,}")
    chequing_rate = next(
        (float(r.avg_rate) for r in by_type.itertuples() if r.account_type == "chequing"),
        None)
    c3.metric("Chequing rate", f"{chequing_rate:.2%}" if chequing_rate is not None else "—")

    if not by_type.empty:
        st.caption("Accounts by type")
        st.bar_chart(by_type.set_index("account_type")["n"], height=180)

    # Opening rate over the last hour, per minute.
    rate = query(
        f"SELECT date_trunc('minute', created_at) AS minute, count(*) AS accounts "
        f"FROM accounts WHERE {NOT_SYSTEM_ACCOUNT} AND created_at >= now() - interval '1 hour' "
        f"GROUP BY 1 ORDER BY 1")
    if not rate.empty:
        st.caption("Accounts opened per minute (last hour)")
        st.bar_chart(rate.set_index("minute")["accounts"], height=180)

    st.subheader("🧾 Recent accounts")
    # overdraft_limit doubles as the credit limit for credit-card accounts.
    feed = query(
        "SELECT a.created_at, a.account_number, a.account_type::text AS account_type, "
        "       a.status::text AS status, (a.interest_rate * 100) AS rate_pct, "
        "       a.balance, "
        "       CASE WHEN a.account_type = 'credit_card' THEN a.overdraft_limit END AS credit_limit, "
        "       a.currency, "
        "       c.first_name || ' ' || c.last_name AS customer "
        "FROM accounts a JOIN customers c USING (customer_id) "
        f"WHERE c.{NOT_SYSTEM_CUSTOMER} "
        "ORDER BY a.created_at DESC LIMIT 200")
    st.dataframe(feed, use_container_width=True, hide_index=True,
                 column_config={
                     "created_at": st.column_config.DatetimeColumn("opened", format="HH:mm:ss"),
                     "rate_pct": st.column_config.NumberColumn("rate", format="%.2f%%"),
                     "credit_limit": st.column_config.NumberColumn("credit limit", format="$%.0f"),
                 })


def render_transactions() -> None:
    """Credit-card payment rails: authorizations (open holds), captured
    purchases, and settlement batches."""
    purchases = int(query(
        "SELECT count(*) AS n FROM transactions WHERE transaction_type = 'card_purchase'")["n"][0])
    vol_hour = float(query(
        "SELECT COALESCE(sum(amount), 0) AS v FROM transactions "
        "WHERE transaction_type = 'card_purchase' AND created_at >= now() - interval '1 hour'")["v"][0])
    unsettled = float(query(
        "SELECT COALESCE(sum(amount), 0) AS v FROM transactions "
        "WHERE transaction_type = 'card_purchase' "
        "AND (metadata->>'settled') IS DISTINCT FROM 'true'")["v"][0])
    open_auths = int(query(
        "SELECT count(*) AS n FROM account_holds WHERE released_at IS NULL "
        "AND reason LIKE 'visa_auth:%'")["n"][0])

    c1, c2, c3, c4 = st.columns(4)
    c1.metric("Captured purchases", f"{purchases:,}")
    c2.metric("Volume (last hour)", f"${vol_hour:,.2f}")
    c3.metric("Unsettled", f"${unsettled:,.2f}")
    c4.metric("Open authorizations", f"{open_auths:,}")

    # Purchase volume per minute over the last hour.
    rate = query(
        "SELECT date_trunc('minute', created_at) AS minute, sum(amount) AS volume "
        "FROM transactions WHERE transaction_type = 'card_purchase' "
        "AND created_at >= now() - interval '1 hour' GROUP BY 1 ORDER BY 1")
    if not rate.empty:
        st.caption("Card purchase volume per minute (last hour)")
        st.bar_chart(rate.set_index("minute")["volume"], height=180)

    st.subheader("🧾 Recent card activity")
    feed = query(
        "SELECT t.created_at, t.transaction_type::text AS type, t.amount, "
        "       t.metadata->>'merchant' AS merchant, "
        "       (t.metadata->>'settled') = 'true' AS settled, "
        "       t.reference_number, "
        "       c.first_name || ' ' || c.last_name AS cardholder "
        "FROM transactions t LEFT JOIN customers c ON c.customer_id = t.initiated_by "
        "WHERE t.transaction_type IN ('card_purchase', 'card_settlement') "
        "ORDER BY t.created_at DESC LIMIT 200")
    st.dataframe(feed, use_container_width=True, hide_index=True,
                 column_config={
                     "created_at": st.column_config.DatetimeColumn("when", format="HH:mm:ss"),
                     "amount": st.column_config.NumberColumn("amount", format="$%.2f"),
                     "settled": st.column_config.CheckboxColumn("settled"),
                 })


def render_interac() -> None:
    """Interac e-Transfer rail: in-flight/recent transfers, the clearing and
    settlement GL balances, and the notification outbox (no real email/SMS —
    just an outbox table the simulator and this viewer read)."""
    total = int(query("SELECT count(*) AS n FROM interac_etransfers")["n"][0])
    in_flight = int(query(
        "SELECT count(*) AS n FROM interac_etransfers "
        "WHERE status IN ('initiated', 'held', 'available')")["n"][0])
    last_hour = int(query(
        "SELECT count(*) AS n FROM interac_etransfers "
        "WHERE created_at >= now() - interval '1 hour'")["n"][0])
    by_status = query(
        "SELECT status::text AS status, count(*) AS n "
        "FROM interac_etransfers GROUP BY status ORDER BY n DESC")

    c1, c2, c3 = st.columns(3)
    c1.metric("Total e-Transfers", f"{total:,}")
    c2.metric("In-flight", f"{in_flight:,}")
    c3.metric("Created (last hour)", f"{last_hour:,}")

    if not by_status.empty:
        st.caption("e-Transfers by status")
        st.bar_chart(by_status.set_index("status")["n"], height=180)

    st.subheader("🧾 Recent e-Transfers")
    feed = query(
        "SELECT created_at, direction::text AS direction, status::text AS status, "
        "       amount, recipient_handle_value "
        "FROM interac_etransfers ORDER BY created_at DESC LIMIT 200")
    st.dataframe(feed, use_container_width=True, hide_index=True,
                 column_config={
                     "created_at": st.column_config.DatetimeColumn("when", format="HH:mm:ss"),
                     "amount": st.column_config.NumberColumn("amount", format="$%.2f"),
                 })

    st.subheader("💰 Clearing & settlement balances")
    balances = query(
        "SELECT a.account_type::text AS account_type, a.balance, a.currency "
        "FROM accounts a JOIN customers c USING (customer_id) "
        "WHERE c.email = 'interac@nano.bank'")
    clearing = next(
        (float(r.balance) for r in balances.itertuples() if r.account_type == "chequing"), None)
    settlement = next(
        (float(r.balance) for r in balances.itertuples() if r.account_type == "savings"), None)
    b1, b2 = st.columns(2)
    b1.metric("INTERAC_CLEARING", f"${clearing:,.2f}" if clearing is not None else "—")
    b2.metric("INTERAC_SETTLEMENT", f"${settlement:,.2f}" if settlement is not None else "—")

    st.subheader("📬 Notification outbox")
    notifications = query(
        "SELECT created_at, kind::text AS kind, delivered, message "
        "FROM interac_notifications ORDER BY created_at DESC LIMIT 200")
    st.dataframe(notifications, use_container_width=True, hide_index=True,
                 column_config={
                     "created_at": st.column_config.DatetimeColumn("when", format="HH:mm:ss"),
                     "delivered": st.column_config.CheckboxColumn("delivered"),
                 })


def render_aft() -> None:
    """AFT/EFT batch rail: batches by status, the clearing/settlement GL
    balances, recent entries (with returns), and PAD mandates."""
    total_batches = int(query("SELECT count(*) AS n FROM aft_batches")["n"][0])
    open_or_submitted = int(query(
        "SELECT count(*) AS n FROM aft_batches WHERE status IN ('open','submitted')")["n"][0])
    total_entries = int(query("SELECT count(*) AS n FROM aft_entries")["n"][0])
    by_status = query(
        "SELECT status::text AS status, count(*) AS n "
        "FROM aft_batches GROUP BY status ORDER BY n DESC")

    c1, c2, c3 = st.columns(3)
    c1.metric("Total batches", f"{total_batches:,}")
    c2.metric("Open / submitted", f"{open_or_submitted:,}")
    c3.metric("Entries", f"{total_entries:,}")

    if not by_status.empty:
        st.caption("batches by status")
        st.bar_chart(by_status.set_index("status")["n"], height=180)

    st.subheader("💰 Clearing & settlement balances")
    balances = query(
        "SELECT a.account_type::text AS account_type, a.balance "
        "FROM accounts a JOIN customers c USING (customer_id) "
        "WHERE c.email = 'aft@nano.bank'")
    clearing = next(
        (float(r.balance) for r in balances.itertuples() if r.account_type == "chequing"), None)
    settlement = next(
        (float(r.balance) for r in balances.itertuples() if r.account_type == "savings"), None)
    b1, b2 = st.columns(2)
    b1.metric("AFT_CLEARING", f"${clearing:,.2f}" if clearing is not None else "—")
    b2.metric("AFT_SETTLEMENT", f"${settlement:,.2f}" if settlement is not None else "—")

    st.subheader("📦 Recent batches")
    batches = query(
        "SELECT created_at, direction::text AS direction, status::text AS status, "
        "       entry_count, total_credits, total_debits FROM aft_batches "
        "ORDER BY created_at DESC LIMIT 100")
    st.dataframe(batches, use_container_width=True, hide_index=True,
                 column_config={
                     "created_at": st.column_config.DatetimeColumn("when", format="HH:mm:ss"),
                     "total_credits": st.column_config.NumberColumn("credits", format="$%.2f"),
                     "total_debits": st.column_config.NumberColumn("debits", format="$%.2f"),
                 })

    st.subheader("🧾 Recent entries")
    entries = query(
        "SELECT created_at, kind::text AS kind, direction::text AS direction, "
        "       status::text AS status, amount, payee_name, return_reason "
        "FROM aft_entries ORDER BY created_at DESC LIMIT 200")
    st.dataframe(entries, use_container_width=True, hide_index=True,
                 column_config={
                     "created_at": st.column_config.DatetimeColumn("when", format="HH:mm:ss"),
                     "amount": st.column_config.NumberColumn("amount", format="$%.2f"),
                 })

    st.subheader("📝 PAD mandates")
    mandates = query(
        "SELECT created_at, biller_name, amount_cap, status::text AS status "
        "FROM pad_mandates ORDER BY created_at DESC LIMIT 100")
    st.dataframe(mandates, use_container_width=True, hide_index=True,
                 column_config={
                     "created_at": st.column_config.DatetimeColumn("when", format="HH:mm:ss"),
                     "amount_cap": st.column_config.NumberColumn("cap", format="$%.2f"),
                 })


def render_lynx() -> None:
    """Lynx RTGS wire rail: wires by direction/status, the clearing/settlement
    GL balances, recent wires, the ISO 20022 message log, and recalls."""
    total_wires = int(query("SELECT count(*) AS n FROM lynx_wires")["n"][0])
    settled = int(query("SELECT count(*) AS n FROM lynx_wires WHERE status='settled'")["n"][0])
    recalls = int(query("SELECT count(*) AS n FROM lynx_recalls")["n"][0])
    by_status = query(
        "SELECT status::text AS status, count(*) AS n "
        "FROM lynx_wires GROUP BY status ORDER BY n DESC")

    c1, c2, c3 = st.columns(3)
    c1.metric("Total wires", f"{total_wires:,}")
    c2.metric("Settled", f"{settled:,}")
    c3.metric("Recalls", f"{recalls:,}")

    if not by_status.empty:
        st.caption("wires by status")
        st.bar_chart(by_status.set_index("status")["n"], height=180)

    st.subheader("💰 Clearing & settlement balances")
    balances = query(
        "SELECT a.account_type::text AS account_type, a.balance "
        "FROM accounts a JOIN customers c USING (customer_id) "
        "WHERE c.email = 'lynx@nano.bank'")
    clearing = next(
        (float(r.balance) for r in balances.itertuples() if r.account_type == "chequing"), None)
    settlement = next(
        (float(r.balance) for r in balances.itertuples() if r.account_type == "savings"), None)
    b1, b2 = st.columns(2)
    b1.metric("LYNX_CLEARING", f"${clearing:,.2f}" if clearing is not None else "—")
    b2.metric("LYNX_SETTLEMENT", f"${settlement:,.2f}" if settlement is not None else "—")

    st.subheader("🌐 Recent wires")
    wires = query(
        "SELECT created_at, direction::text AS direction, status::text AS status, amount, "
        "       counterparty_name, counterparty_institution AS inst, message_type "
        "FROM lynx_wires ORDER BY created_at DESC LIMIT 200")
    st.dataframe(wires, use_container_width=True, hide_index=True,
                 column_config={
                     "created_at": st.column_config.DatetimeColumn("when", format="HH:mm:ss"),
                     "amount": st.column_config.NumberColumn("amount", format="$%.2f"),
                 })

    st.subheader("✉️ ISO 20022 message log")
    messages = query(
        "SELECT created_at, message_type, flow FROM lynx_messages "
        "ORDER BY created_at DESC LIMIT 200")
    st.dataframe(messages, use_container_width=True, hide_index=True,
                 column_config={
                     "created_at": st.column_config.DatetimeColumn("when", format="HH:mm:ss"),
                 })

    st.subheader("↩️ Recalls")
    recall_rows = query(
        "SELECT created_at, direction::text AS direction, status::text AS status, "
        "       reason, resolution_reason FROM lynx_recalls ORDER BY created_at DESC LIMIT 100")
    st.dataframe(recall_rows, use_container_width=True, hide_index=True,
                 column_config={
                     "created_at": st.column_config.DatetimeColumn("when", format="HH:mm:ss"),
                 })


def main() -> None:
    st.set_page_config(page_title="nano-bank viewer", page_icon="🏦", layout="wide")
    st.title("🏦 nano-bank · live activity")
    st.caption(f"Postgres `{DB['host']}:{DB['port']}/{DB['dbname']}` · "
               f"refresh every {REFRESH_SECONDS}s")
    st_autorefresh(interval=REFRESH_SECONDS * 1000, key="refresh")

    tab_customers, tab_accounts, tab_tx, tab_interac, tab_aft, tab_lynx = st.tabs(
        ["👤 Customers", "💳 Accounts", "💸 Card transactions", "📨 Interac", "🏦 AFT", "🌐 Lynx"])
    with tab_customers:
        try:
            render_customers()
        except Exception as e:  # show the failure instead of a blank screen
            st.error(f"Database error: {e}")
            st.info("Is the nano-bank Postgres port-forward up? "
                    "`kubectl port-forward -n nano-bank svc/postgres-service 5432:5432`. "
                    "Note the port-forward binds IPv6 loopback `::1` here (the IPv4 "
                    "`0.0.0.0:5432` is a dead kind/docker-proxy mapping), so DB_HOST "
                    "defaults to `::1`.")
    with tab_accounts:
        try:
            render_accounts()
        except Exception as e:
            st.error(f"Database error: {e}")
    with tab_tx:
        try:
            render_transactions()
        except Exception as e:
            st.error(f"Database error: {e}")
    with tab_interac:
        try:
            render_interac()
        except Exception as e:
            st.error(f"Database error: {e}")
    with tab_aft:
        try:
            render_aft()
        except Exception as e:
            st.error(f"Database error: {e}")
    with tab_lynx:
        try:
            render_lynx()
        except Exception as e:
            st.error(f"Database error: {e}")


if __name__ == "__main__":
    main()
