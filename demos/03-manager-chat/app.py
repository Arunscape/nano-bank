"""Agentic manager console — per-action chats with a left-right conversation view.

Talks to the personal manager's Branch API (:8086). Seed a demo client, then use
the action boxes below (each pre-filled with a ready-to-send question and its own
conversation thread). Messages render left-right — you on the left, the manager on
the right — with the run's tool/model trace shown inline under each answer.

Config: DEMO_BRANCH_BASE (default http://localhost:8086) + DEMO_BRANCH_TOKEN
(the BRANCH_SERVICE_TOKEN). See demos/README.md.
"""
from __future__ import annotations
import os
import requests
import streamlit as st

API = os.environ.get("DEMO_BRANCH_BASE", "http://localhost:8086").rstrip("/")
TOKEN = os.environ.get("DEMO_BRANCH_TOKEN", "")
HDR = {"Authorization": f"Bearer {TOKEN}"}
TIMEOUT = 180

st.set_page_config(page_title="nano-bank · manager chat", layout="wide")
ss = st.session_state
ss.setdefault("clients", [])
ss.setdefault("cid", None)
ss.setdefault("cname", "")
ss.setdefault("threads", {})     # box_key -> thread_id
ss.setdefault("convo", {})       # box_key -> [ {role, text, trace?, pending?} ]

# The action boxes: (title, key, ready-to-send question)
BOXES = [
    ("① Account balance", "balance", "What is my current account balance?"),
    ("② Advantages of a savings account", "savings",
     "What are the advantages of opening a savings account?"),
    ("③ Open account", "open", "Open a savings account for me"),
    ("④ Register Interac payee", "payee", "Register sam@example.ca as a payee named Sam"),
    ("⑤ Perform transactions", "txn", "Deposit 100 into my chequing account"),
]


def _post(path, body=None):
    try:
        r = requests.post(f"{API}{path}", json=body, headers=HDR, timeout=TIMEOUT)
        return r.status_code, (r.json() if r.content else {})
    except requests.RequestException as e:
        return 0, {"error": str(e)}


def _get(path):
    try:
        r = requests.get(f"{API}{path}", headers=HDR, timeout=60)
        return r.status_code, (r.json() if r.content else {})
    except requests.RequestException as e:
        return 0, {"error": str(e)}


st.title("🤖 nano-bank — manager chat (you ↔ manager)")
st.caption(f"Branch API: `{API}` · seed a client, then chat by action — left-right conversation with inline traces")

# --- client (no dropdown: seed and use the first client) --------------------
top = st.columns([1, 2])
with top[0]:
    if st.button("🌱 Seed a demo client"):
        _, body = _post("/branch/seed")
        clients = body.get("customers", []) if isinstance(body, dict) else []
        ss["clients"] = clients
        ss["cid"] = clients[0]["customer_id"] if clients else None
        ss["threads"], ss["convo"] = {}, {}     # fresh conversations for the new client
        _, prof = (_get(f"/branch/clients/{ss['cid']}/profile") if ss["cid"] else (0, {}))
        ss["cname"] = (f"{prof.get('first_name','')} {prof.get('last_name','')}".strip()
                       if isinstance(prof, dict) else "")
        st.rerun()
with top[1]:
    if ss["cid"]:
        st.markdown(f"**Client:** {ss['cname'] or '(client)'} · `{ss['cid'][:8]}`")
        _, accts = _get(f"/branch/clients/{ss['cid']}/accounts")
        if isinstance(accts, list) and accts:
            st.table([{"type": a.get("account_type"), "balance": a.get("balance"),
                       "id": a.get("account_id", "")[:8]} for a in accts])

if not ss["cid"]:
    st.info("Seed a demo client to begin.")
    st.stop()


def _trace_strip(trace):
    if not trace:
        return None
    return "trace: " + "  ·  ".join(
        f"{'🔧' if e['kind'] == 'tool' else '🧠'}{'✅' if e.get('ok') else '❌'} "
        f"{e['name']} {e['elapsed_ms']}ms" for e in trace)


def _render_convo(turns):
    for t in turns:
        if t["role"] == "user":
            left, _ = st.columns([3, 2])
            with left, st.container(border=True):
                st.markdown("🧑 **You**")
                st.write(t["text"])
        else:
            _, right = st.columns([2, 3])
            with right, st.container(border=True):
                st.markdown("🤖 **Manager**")
                st.markdown(t["text"])
                strip = _trace_strip(t.get("trace"))
                if strip:
                    st.caption(strip)


def action_box(title, key, default):
    st.subheader(title)
    ss.setdefault(f"in_{key}", default)   # pre-filled, ready to Send (editable)
    ss["convo"].setdefault(key, [])
    row = st.columns([6, 1])
    msg = row[0].text_input("Message", key=f"in_{key}", label_visibility="collapsed")
    if row[1].button("Send", key=f"send_{key}") and msg:
        convo = ss["convo"][key]
        convo.append({"role": "user", "text": msg})
        body = {"message": msg}
        if ss["threads"].get(key):
            body["thread_id"] = ss["threads"][key]
        _, data = _post(f"/branch/clients/{ss['cid']}/message", body)
        if isinstance(data, dict):
            ss["threads"][key] = data.get("thread_id", ss["threads"].get(key))
            convo.append({"role": "manager", "text": data.get("answer", ""),
                          "trace": data.get("trace"), "pending": data.get("pending_action")})
        st.rerun()

    _render_convo(ss["convo"][key])

    # Confirm gate: act on the latest still-pending proposal in this box.
    pending = next((t["pending"] for t in reversed(ss["convo"][key])
                    if t["role"] == "manager" and t.get("pending")), None)
    if pending:
        st.warning(f"Proposed: {pending.get('summary', pending.get('id'))}")
        b = st.columns(2)
        if b[0].button("✅ Confirm", key=f"ok_{key}"):
            _, rr = _post(f"/branch/clients/{ss['cid']}/actions/{pending['id']}/confirm")
            for t in ss["convo"][key]:
                if t["role"] == "manager":
                    t["pending"] = None
            ss["convo"][key].append({"role": "manager", "text": "✅ Executed.", "trace": None})
            st.rerun()
        if b[1].button("✖ Cancel", key=f"no_{key}"):
            _post(f"/branch/clients/{ss['cid']}/actions/{pending['id']}/cancel")
            for t in ss["convo"][key]:
                if t["role"] == "manager":
                    t["pending"] = None
            ss["convo"][key].append({"role": "manager", "text": "✖ Cancelled.", "trace": None})
            st.rerun()


for i, (title, key, default) in enumerate(BOXES):
    if i:
        st.divider()
    action_box(title, key, default)
