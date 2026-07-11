from __future__ import annotations
import os
import httpx
import streamlit as st

from agent.config import Settings

settings = Settings.from_env()
API = os.environ.get("MANAGER_API_URL", f"http://localhost:{settings.branch_port}")
HDR = {"Authorization": f"Bearer {settings.branch_service_token}"}

st.set_page_config(page_title="nano-bank manager — console", layout="wide")
st.title("nano-bank personal manager — console")


def _get(path):
    r = httpx.get(f"{API}{path}", headers=HDR, timeout=60)
    r.raise_for_status()
    return r.json()


left, right = st.columns([1, 2])

with left:
    st.subheader("Seed")
    if st.button("Seed demo (2 customers + funded account)"):
        out = httpx.post(f"{API}/branch/seed", headers=HDR, timeout=180).json()
        st.session_state["customers"] = out["customers"]
        st.success(f"seeded {len(out['customers'])} customers")
    customers = st.session_state.get("customers", [])
    cid = (st.selectbox("client", [c["customer_id"] for c in customers])
           if customers else st.text_input("client id"))

with right:
    st.subheader("Chat")
    msg = st.text_input("Ask or instruct (e.g. 'transfer 25 from <acc> to <acc>')")
    if st.button("Send") and cid and msg:
        data = httpx.post(f"{API}/branch/clients/{cid}/message",
                          json={"message": msg}, headers=HDR, timeout=180).json()
        st.markdown(f"**Manager:** {data.get('answer','')}")
        pa = data.get("pending_action")
        if pa:
            st.warning(f"Proposed: {pa.get('summary', pa)}")
            st.session_state["pending"] = pa
    pa = st.session_state.get("pending")
    if pa and cid:
        c1, c2 = st.columns(2)
        if c1.button("Confirm"):
            rr = httpx.post(f"{API}/branch/clients/{cid}/actions/{pa['id']}/confirm",
                            headers=HDR, timeout=180).json()
            st.success(rr)
            st.session_state.pop("pending", None)
        if c2.button("Cancel"):
            httpx.post(f"{API}/branch/clients/{cid}/actions/{pa['id']}/cancel",
                       headers=HDR, timeout=60)
            st.session_state.pop("pending", None)

# --- Dashboard: customer / accounts / transactions ---
# Each panel is isolated so one failing endpoint can't hide the others.
if cid:
    st.divider()
    st.subheader("Client dashboard")

    try:
        prof = _get(f"/branch/clients/{cid}/profile")
        st.markdown(f"**Customer:** {prof.get('first_name','?')} "
                    f"{prof.get('last_name','')} · {prof.get('email','')} "
                    f"· KYC {prof.get('kyc_status','?')}")
    except Exception as e:  # noqa: BLE001
        st.info(f"Profile unavailable: {e}")

    st.markdown("**Accounts**")
    try:
        st.table(_get(f"/branch/clients/{cid}/accounts"))
    except Exception as e:  # noqa: BLE001
        st.info(f"Accounts unavailable: {e}")

    st.markdown("**Recent transactions**")
    try:
        st.table(_get(f"/branch/clients/{cid}/transactions"))
    except Exception as e:  # noqa: BLE001
        st.info(f"Transactions unavailable: {e}")
