"""Activity simulator — auto-generate bank activity and watch it stream.

Drives the real nano-bank REST API to create customers, open accounts of every
type, post transactions of every type (including deliberate failures), and
register + send Interac e-Transfers. Every API call — success and failure — is
recorded in a timestamped event log (the last tab), like a cloud activity stream.

Point it at the bank API with DEMO_API_BASE (default http://localhost:8081 —
port-forward svc/bank-api first; see demos/README.md).
"""
from __future__ import annotations
import os
import random
import string
import time
import uuid
from datetime import date, datetime

import requests
import streamlit as st

API = os.environ.get("DEMO_API_BASE", "http://localhost:8081").rstrip("/")
TIMEOUT = 30

st.set_page_config(page_title="nano-bank · activity simulator", layout="wide")
ss = st.session_state
ss.setdefault("events", [])       # cloud-log: list of event dicts
ss.setdefault("customers", [])    # [{customer_id, email, password, token, name}]
ss.setdefault("payees", [])       # [{customer_id, recipient_id, email}]


# --- event log + API layer --------------------------------------------------
def _log(action, method, path, code, ok, ms, detail):
    ss["events"].append({
        "ts": datetime.now(),
        "action": action, "method": method, "path": path,
        "code": code, "ok": ok, "ms": ms, "detail": detail,
    })


def _summarize(ok, body):
    if not ok:
        if isinstance(body, dict) and isinstance(body.get("error"), dict):
            return body["error"].get("message", str(body))[:140]
        return str(body)[:140]
    if isinstance(body, dict):
        if "customer_id" in body:
            return f"customer {body.get('first_name','')} {body.get('last_name','')} ({body['customer_id'][:8]})"
        if "access_token" in body:
            return "logged in"
        if "account_id" in body:
            return f"{body.get('account_type','account')} {body['account_id'][:8]}"
        if "transaction_id" in body:
            return f"{body.get('transaction_type','txn')} {body.get('amount','')}"
        if "etransfer_id" in body:
            return f"e-transfer {body.get('status','')} → {body.get('recipient_handle_value','')}"
        if "recipient_id" in body:
            return f"payee {body.get('email','')}"
    if isinstance(body, list):
        return f"{len(body)} item(s)"
    return "ok"


def api(method, path, *, token=None, json=None, action=""):
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    t0 = time.perf_counter()
    try:
        r = requests.request(method, f"{API}{path}", headers=headers, json=json, timeout=TIMEOUT)
        ms = int((time.perf_counter() - t0) * 1000)
        try:
            body = r.json()
        except ValueError:
            body = r.text
        ok = 200 <= r.status_code < 300
        _log(action, method, path, r.status_code, ok, ms, _summarize(ok, body))
        return r.status_code, body
    except requests.RequestException as e:
        _log(action, method, path, 0, False, int((time.perf_counter() - t0) * 1000),
             f"request error: {e}")
        return 0, {"error": {"message": str(e)}}


# --- random data ------------------------------------------------------------
def _rand_customer():
    n = random.randint(1000, 9_999_999)
    first = random.choice(["Ada", "Bo", "Cy", "Devi", "Ola", "Sam", "Mira", "Ravi", "Ines", "Tao"])
    last = f"Q{n}"
    return {
        "email": f"{first}.{last}.{n}@example.com".lower(),
        "phone_number": f"+1{random.randint(2000000000, 9999999999)}",
        "first_name": first, "last_name": last,
        "date_of_birth": date(random.randint(1960, 2004), random.randint(1, 12),
                              random.randint(1, 28)).isoformat(),
        "sin": "".join(random.choice(string.digits) for _ in range(9)),
        "password": "Demo!" + "".join(random.choice(string.ascii_letters) for _ in range(8)),
    }


def _accounts(cust):
    code, body = api("GET", "/api/v1/accounts", token=cust["token"], action="list accounts")
    return body if code == 200 and isinstance(body, list) else []


# --- header -----------------------------------------------------------------
st.title("🛰️ nano-bank — activity simulator")
st.caption(f"API: `{API}` · generate customers/accounts/transactions and watch the event log")

with st.sidebar:
    st.header("Controls")
    n_cust = st.slider("Customers to generate", 1, 8, 3)
    types = st.multiselect("Account types to open", ["chequing", "savings", "credit_card"],
                           default=["chequing", "savings"])
    if st.button("🧹 Reset session"):
        ss["events"], ss["customers"], ss["payees"] = [], [], []
        st.rerun()
    st.metric("Customers", len(ss["customers"]))
    st.metric("Events logged", len(ss["events"]))

tabs = st.tabs(["1 · Customers", "2 · Accounts", "3 · Transactions",
                "4 · Interac", "5 · Event log 🪵"])

# --- Tab 1: customers -------------------------------------------------------
with tabs[0]:
    st.subheader("Generate customers")
    if st.button(f"Create {n_cust} customer(s) + log in"):
        for _ in range(n_cust):
            draft = _rand_customer()
            code, body = api("POST", "/api/v1/customers", json=draft, action="create customer")
            if code == 201 and isinstance(body, dict):
                lcode, lbody = api("POST", "/api/v1/auth/login",
                                   json={"email": draft["email"], "password": draft["password"]},
                                   action="login")
                token = lbody.get("access_token") if isinstance(lbody, dict) else None
                ss["customers"].append({
                    "customer_id": body["customer_id"], "email": draft["email"],
                    "password": draft["password"], "token": token,
                    "name": f"{body['first_name']} {body['last_name']}"})
        st.rerun()
    if ss["customers"]:
        st.table([{"name": c["name"], "email": c["email"],
                   "customer_id": c["customer_id"], "logged_in": bool(c["token"])}
                  for c in ss["customers"]])
    else:
        st.info("No customers yet — click the button above.")

# --- Tab 2: accounts --------------------------------------------------------
with tabs[1]:
    st.subheader("Open accounts")
    if not ss["customers"]:
        st.info("Generate customers first (tab 1).")
    else:
        if st.button(f"Open {', '.join(types) or '(none)'} for every customer"):
            for c in ss["customers"]:
                if not c["token"]:
                    continue
                for t in types:
                    api("POST", "/api/v1/accounts", token=c["token"],
                        json={"account_type": t}, action=f"open {t}")
            st.rerun()
        rows = []
        for c in ss["customers"]:
            if c["token"]:
                for a in _accounts(c):
                    rows.append({"customer": c["name"], "type": a["account_type"],
                                 "account_id": a["account_id"],
                                 "balance": f"${float(a.get('balance', 0)):,.2f}",
                                 "status": a.get("status", "")})
        if rows:
            st.table(rows)

# --- Tab 3: transactions (all types + deliberate failures) ------------------
with tabs[2]:
    st.subheader("Generate transactions — including failures")
    st.caption("Deposits + withdrawals + transfers, plus intentional failures "
               "(overdraw, transfer to a non-owned account).")
    if not ss["customers"]:
        st.info("Generate customers and accounts first (tabs 1–2).")
    elif st.button("Run a transaction batch"):
        for c in ss["customers"]:
            if not c["token"]:
                continue
            accts = _accounts(c)
            chequing = next((a for a in accts if a["account_type"] == "chequing"), None)
            if not chequing:
                continue
            acc = chequing["account_id"]
            # success: fund it
            api("POST", "/api/v1/transactions/deposit", token=c["token"],
                json={"account_id": acc, "amount": f"{random.randint(200, 900)}.00",
                      "description": "Payroll"}, action="deposit ✓")
            # success: a modest withdrawal
            api("POST", "/api/v1/transactions/withdrawal", token=c["token"],
                json={"account_id": acc, "amount": "25.00", "description": "ATM"},
                action="withdraw ✓")
            # success: transfer chequing → savings if present
            savings = next((a for a in accts if a["account_type"] == "savings"), None)
            if savings:
                api("POST", "/api/v1/transactions/transfer", token=c["token"],
                    json={"from_account_id": acc, "to_account_id": savings["account_id"],
                          "amount": "50.00", "description": "To savings"}, action="transfer ✓")
            # FAILURE: overdraw far beyond balance
            api("POST", "/api/v1/transactions/withdrawal", token=c["token"],
                json={"account_id": acc, "amount": "999999.00", "description": "Overdraw"},
                action="withdraw ✗ (insufficient)")
            # FAILURE: transfer to a random account this customer does not own
            api("POST", "/api/v1/transactions/transfer", token=c["token"],
                json={"from_account_id": acc, "to_account_id": str(uuid.uuid4()),
                      "amount": "10.00", "description": "Bad target"},
                action="transfer ✗ (not owned)")
        st.rerun()
    # show this session's transaction outcomes from the log
    txn_events = [e for e in ss["events"] if e["path"].startswith("/api/v1/transactions")]
    if txn_events:
        st.table([{"time": e["ts"].strftime("%H:%M:%S"), "action": e["action"],
                   "code": e["code"], "result": "✅" if e["ok"] else "❌",
                   "detail": e["detail"]} for e in txn_events[-20:][::-1]])

# --- Tab 4: Interac payees + real-rail send ---------------------------------
with tabs[3]:
    st.subheader("Interac — register payees and send e-Transfers")
    st.caption("Register a saved payee, then send over the real Interac rail "
               "(security Q&A required). Includes a deliberate failure.")
    if not ss["customers"]:
        st.info("Generate customers and fund an account first (tabs 1–3).")
    else:
        if st.button("Register a payee + send (with a failure case)"):
            for c in ss["customers"]:
                if not c["token"]:
                    continue
                payee_email = f"payee.{random.randint(1000,9_999_999)}@example.com"
                code, body = api("POST", "/api/v1/customers/interac-recipients",
                                 token=c["token"],
                                 json={"email": payee_email, "display_name": "Payee"},
                                 action="register payee")
                if code == 201 and isinstance(body, dict):
                    ss["payees"].append({"customer_id": c["customer_id"],
                                         "recipient_id": body["recipient_id"],
                                         "email": payee_email})
                accts = _accounts(c)
                chequing = next((a for a in accts if a["account_type"] == "chequing"), None)
                if not chequing:
                    continue
                acc = chequing["account_id"]
                # success: send over the real rail with a security Q&A
                api("POST", "/api/v1/interac/etransfers", token=c["token"],
                    json={"from_account_id": acc, "amount": "20.00",
                          "recipient_handle_type": "email", "recipient_handle_value": payee_email,
                          "security_question": "colour?", "security_answer": "blue",
                          "memo": "hello"}, action="e-transfer ✓")
                # FAILURE: send with no security question (non-autodeposit recipient)
                api("POST", "/api/v1/interac/etransfers", token=c["token"],
                    json={"from_account_id": acc, "amount": "20.00",
                          "recipient_handle_type": "email", "recipient_handle_value": payee_email,
                          "memo": "no security"}, action="e-transfer ✗ (no security)")
            st.rerun()
        if ss["payees"]:
            st.markdown("**Registered payees**")
            st.table([{"customer_id": p["customer_id"][:8], "email": p["email"],
                       "recipient_id": p["recipient_id"][:8]} for p in ss["payees"]])
        it_events = [e for e in ss["events"] if "interac" in e["path"]]
        if it_events:
            st.markdown("**Interac events**")
            st.table([{"time": e["ts"].strftime("%H:%M:%S"), "action": e["action"],
                       "code": e["code"], "result": "✅" if e["ok"] else "❌",
                       "detail": e["detail"]} for e in it_events[-20:][::-1]])

# --- Tab 5: the cloud log ---------------------------------------------------
with tabs[4]:
    st.subheader("Event log")
    evs = ss["events"]
    c1, c2, c3 = st.columns(3)
    c1.metric("Total", len(evs))
    c2.metric("OK", sum(1 for e in evs if e["ok"]))
    c3.metric("Failed", sum(1 for e in evs if not e["ok"]))
    show = st.radio("Show", ["All", "Failures only"], horizontal=True)
    stream = [e for e in evs if (show == "All" or not e["ok"])]
    if not stream:
        st.info("No events yet — generate some activity in tabs 1–4.")
    else:
        lines = []
        for e in reversed(stream):   # newest first
            colour = "#1a7f37" if e["ok"] else "#cf222e"
            icon = "✅" if e["ok"] else "❌"
            ts = e["ts"].strftime("%H:%M:%S.%f")[:-3]
            lines.append(
                f"<div style='font-family:monospace;font-size:0.86rem;white-space:pre-wrap'>"
                f"<span style='color:#8b949e'>{ts}</span> {icon} "
                f"<span style='color:{colour}'>{e['code'] or '—'}</span> "
                f"<b>{e['method']}</b> {e['path']} "
                f"<span style='color:#8b949e'>({e['ms']}ms)</span> · "
                f"{e['action']} — {e['detail']}</div>")
        st.markdown("\n".join(lines), unsafe_allow_html=True)
