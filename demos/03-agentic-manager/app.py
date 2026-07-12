"""Agentic manager console — per-action chat boxes with LangSmith-like traces.

Talks to the personal manager's Branch API (:8086). Seed a demo client, then use
three action boxes (open account / register Interac payee / transactions); each
box is its own conversation thread and shows the manager's run trace.

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

st.set_page_config(page_title="nano-bank · agentic manager", layout="wide")
ss = st.session_state
ss.setdefault("clients", [])
ss.setdefault("cid", None)
ss.setdefault("threads", {})     # box_key -> thread_id
ss.setdefault("last", {})        # box_key -> last response dict
ss.setdefault("pending", {})     # box_key -> pending_action


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


st.title("🤖 nano-bank — agentic manager console")
st.caption(f"Branch API: `{API}` · seed a client, then chat by action with live traces")

# --- client picker ----------------------------------------------------------
c1, c2 = st.columns([1, 2])
with c1:
    if st.button("🌱 Seed a demo client"):
        code, body = _post("/branch/seed")
        ss["clients"] = body.get("customers", []) if isinstance(body, dict) else []
        if ss["clients"]:
            ss["cid"] = ss["clients"][0]["customer_id"]
        st.rerun()
    if ss["clients"]:
        ss["cid"] = st.selectbox("Active client",
                                 [c["customer_id"] for c in ss["clients"]], index=0)
with c2:
    if ss["cid"]:
        _, prof = _get(f"/branch/clients/{ss['cid']}/profile")
        _, accts = _get(f"/branch/clients/{ss['cid']}/accounts")
        if isinstance(prof, dict):
            st.markdown(f"**Client:** {prof.get('first_name','?')} {prof.get('last_name','')} "
                        f"· `{ss['cid'][:8]}`")
        if isinstance(accts, list) and accts:
            st.table([{"type": a.get("account_type"), "balance": a.get("balance"),
                       "id": a.get("account_id", "")[:8]} for a in accts])

if not ss["cid"]:
    st.info("Seed a demo client to begin.")
    st.stop()


def _render_trace(trace):
    if not trace:
        st.caption("no trace")
        return
    for e in trace:
        icon = "🔧" if e["kind"] == "tool" else "🧠"
        mark = "✅" if e.get("ok") else "❌"
        head = f"{icon} {mark} **{e['name']}** · {e['elapsed_ms']}ms"
        with st.expander(head, expanded=False):
            if e.get("input"):
                st.markdown("input"); st.code(e["input"])
            if e.get("output"):
                st.markdown("output"); st.code(e["output"])
            if e.get("error"):
                st.error(e["error"])


def action_box(title, key, placeholder):
    st.subheader(title)
    msg = st.text_input("Message", key=f"in_{key}", placeholder=placeholder)
    if st.button("Send", key=f"send_{key}") and msg:
        body = {"message": msg}
        if ss["threads"].get(key):
            body["thread_id"] = ss["threads"][key]
        code, data = _post(f"/branch/clients/{ss['cid']}/message", body)
        if isinstance(data, dict):
            ss["threads"][key] = data.get("thread_id", ss["threads"].get(key))
            ss["last"][key] = data
            ss["pending"][key] = data.get("pending_action")
        st.rerun()
    data = ss["last"].get(key)
    if data:
        st.markdown(f"**Manager:** {data.get('answer','')}")
        pa = ss["pending"].get(key)
        if pa:
            st.warning(f"Proposed: {pa.get('summary', pa.get('id'))}")
            b1, b2 = st.columns(2)
            if b1.button("Confirm", key=f"ok_{key}"):
                _post(f"/branch/clients/{ss['cid']}/actions/{pa['id']}/confirm")
                ss["pending"][key] = None
                st.rerun()
            if b2.button("Cancel", key=f"no_{key}"):
                _post(f"/branch/clients/{ss['cid']}/actions/{pa['id']}/cancel")
                ss["pending"][key] = None
                st.rerun()
        with st.expander("🪵 Interaction trace (LangSmith-style)", expanded=True):
            _render_trace(data.get("trace"))


action_box("① Open account", "open", "e.g. open a savings account")
st.divider()
action_box("② Register Interac payee", "payee", "e.g. register sam@example.ca as Sam")
st.divider()
action_box("③ Perform transactions", "txn",
           "e.g. deposit 100 to chequing · transfer 25 to savings · send 30 interac to sam@example.ca")
