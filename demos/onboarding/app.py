"""Onboarding demo — create a customer, open accounts, post transactions.

A guided Streamlit walkthrough of the nano-bank consumer API:
  1. create a customer        POST /api/v1/customers
  2. log in                   POST /api/v1/auth/login   (→ bearer token)
  3. open account(s)          POST /api/v1/accounts     (owner from the token)
  4. post a transaction       POST /api/v1/transactions/{deposit,withdrawal,transfer}

Everything is driven over the real REST API; identity for account/transaction
calls comes from the customer's JWT, never a body/path param. Point it at the
bank API with DEMO_API_BASE (default http://localhost:8081 — port-forward
svc/bank-api first; see demos/README.md).
"""
from __future__ import annotations
import os
import random
import string
from datetime import date

import requests
import streamlit as st

API = os.environ.get("DEMO_API_BASE", "http://localhost:8081").rstrip("/")
TIMEOUT = 30

st.set_page_config(page_title="nano-bank · onboarding demo", layout="wide")
st.title("🏦 nano-bank — onboarding demo")
st.caption(f"API: `{API}` · create a customer → open accounts → post transactions")


# --- tiny API helpers -------------------------------------------------------
def _post(path: str, body: dict, token: str | None = None) -> tuple[int, dict | str]:
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    try:
        r = requests.post(f"{API}{path}", json=body, headers=headers, timeout=TIMEOUT)
    except requests.RequestException as e:
        return 0, f"request failed: {e}"
    try:
        return r.status_code, r.json()
    except ValueError:
        return r.status_code, r.text


def _get(path: str, token: str | None = None) -> tuple[int, dict | list | str]:
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    try:
        r = requests.get(f"{API}{path}", headers=headers, timeout=TIMEOUT)
    except requests.RequestException as e:
        return 0, f"request failed: {e}"
    try:
        return r.status_code, r.json()
    except ValueError:
        return r.status_code, r.text


def _rand_customer() -> dict:
    n = random.randint(1000, 9_999_999)
    first, last = random.choice(["Ada", "Bo", "Cy", "Devi", "Ola", "Sam"]), f"Q{n}"
    sin = "".join(random.choice(string.digits) for _ in range(9))
    return {
        "email": f"{first}.{last}.{n}@example.com".lower(),
        "phone_number": f"+1{random.randint(2000000000, 9999999999)}",
        "first_name": first,
        "last_name": last,
        "date_of_birth": date(random.randint(1960, 2004), random.randint(1, 12),
                              random.randint(1, 28)).isoformat(),
        "sin": sin,
        "password": "Demo!" + "".join(random.choice(string.ascii_letters) for _ in range(8)),
    }


ss = st.session_state
ss.setdefault("draft", _rand_customer())
ss.setdefault("customer", None)   # created customer record
ss.setdefault("password", None)
ss.setdefault("token", None)

left, right = st.columns([1, 1])

# --- Step 1 + 2: create customer and log in ---------------------------------
with left:
    st.subheader("1 · Create a customer")
    d = ss["draft"]
    if st.button("🎲 Randomize"):
        ss["draft"] = _rand_customer()
        st.rerun()
    with st.form("create_customer"):
        c1, c2 = st.columns(2)
        d["first_name"] = c1.text_input("First name", d["first_name"])
        d["last_name"] = c2.text_input("Last name", d["last_name"])
        d["email"] = st.text_input("Email", d["email"])
        c3, c4 = st.columns(2)
        d["phone_number"] = c3.text_input("Phone", d["phone_number"])
        d["date_of_birth"] = c4.text_input("Date of birth (YYYY-MM-DD)", d["date_of_birth"])
        c5, c6 = st.columns(2)
        d["sin"] = c5.text_input("SIN", d["sin"])
        d["password"] = c6.text_input("Password", d["password"])
        submitted = st.form_submit_button("Create customer + log in")
    if submitted:
        code, body = _post("/api/v1/customers", d)
        if code == 201 and isinstance(body, dict):
            ss["customer"], ss["password"] = body, d["password"]
            lcode, lbody = _post("/api/v1/auth/login",
                                 {"email": d["email"], "password": d["password"]})
            if lcode == 200 and isinstance(lbody, dict):
                ss["token"] = lbody.get("access_token")
                st.success(f"Created {body['first_name']} {body['last_name']} "
                           f"(KYC {body.get('kyc_status','?')}) and logged in.")
            else:
                st.warning(f"Customer created, but login failed [{lcode}]: {lbody}")
        elif code == 409:
            st.error("Email or phone already exists — click Randomize and retry.")
        else:
            st.error(f"Create failed [{code}]: {body}")

    if ss["customer"]:
        cust = ss["customer"]
        st.info(f"**Customer:** {cust['first_name']} {cust['last_name']} · "
                f"{cust['email']} · `{cust['customer_id'][:8]}…` · "
                f"{'🔓 logged in' if ss['token'] else '🔒 not logged in'}")

# --- Step 3: open accounts --------------------------------------------------
with right:
    st.subheader("2 · Open an account")
    if not ss["token"]:
        st.caption("Create a customer first — accounts are owned by the logged-in customer.")
    else:
        atype = st.selectbox("Account type", ["chequing", "savings", "credit_card"])
        if st.button("Open account"):
            code, body = _post("/api/v1/accounts", {"account_type": atype}, token=ss["token"])
            if code == 201 and isinstance(body, dict):
                st.success(f"Opened {body['account_type']} #{body.get('account_number','?')} "
                           f"(`{body['account_id'][:8]}…`, {body.get('status','?')})")
            else:
                st.error(f"Open failed [{code}]: {body}")

# --- Accounts snapshot ------------------------------------------------------
accounts: list[dict] = []
if ss["token"]:
    code, body = _get("/api/v1/accounts", token=ss["token"])
    if code == 200 and isinstance(body, list):
        accounts = body

st.divider()
st.subheader("3 · Post a transaction")
if not accounts:
    st.caption("Open at least one account to post a transaction.")
else:
    label = {a["account_id"]: f"{a['account_type']} · {a['account_id'][:8]}… "
                              f"(${float(a.get('balance', 0)):,.2f})" for a in accounts}
    ids = list(label)
    ttype = st.radio("Type", ["deposit", "withdrawal", "transfer"], horizontal=True)
    t1, t2, t3 = st.columns(3)
    src = t1.selectbox("Account" if ttype != "transfer" else "From account",
                       ids, format_func=lambda i: label[i])
    dst = None
    if ttype == "transfer":
        dst = t2.selectbox("To account", ids, format_func=lambda i: label[i])
    amount = t3.text_input("Amount", "100.00")
    memo = st.text_input("Description", ttype.capitalize())
    if st.button(f"Post {ttype}"):
        if ttype == "deposit":
            code, body = _post("/api/v1/transactions/deposit",
                               {"account_id": src, "amount": amount, "description": memo},
                               token=ss["token"])
        elif ttype == "withdrawal":
            code, body = _post("/api/v1/transactions/withdrawal",
                               {"account_id": src, "amount": amount, "description": memo},
                               token=ss["token"])
        else:
            code, body = _post("/api/v1/transactions/transfer",
                               {"from_account_id": src, "to_account_id": dst,
                                "amount": amount, "description": memo}, token=ss["token"])
        if code in (200, 201):
            st.success(f"{ttype.capitalize()} posted.")
            st.json(body)
            st.rerun()
        else:
            st.error(f"{ttype.capitalize()} failed [{code}]: {body}")

# --- Live snapshot ----------------------------------------------------------
if accounts:
    st.divider()
    st.subheader("Accounts")
    st.table([{"type": a["account_type"], "account_id": a["account_id"],
               "number": a.get("account_number", ""), "status": a.get("status", ""),
               "balance": f"${float(a.get('balance', 0)):,.2f}"} for a in accounts])
    code, txns = _get("/api/v1/transactions", token=ss["token"])
    # GET /transactions returns an envelope: {"transactions": [...], "total_count": ...}
    rows = txns.get("transactions") if isinstance(txns, dict) else txns
    if code == 200 and isinstance(rows, list) and rows:
        st.subheader("Recent transactions")
        st.table([{"type": t.get("transaction_type", ""),
                   "amount": t.get("amount", ""),
                   "status": t.get("status", ""),
                   "created_at": t.get("created_at", "")} for t in rows[:15]])
