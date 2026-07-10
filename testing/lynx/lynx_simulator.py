"""Mock Lynx RTGS network — plays "Bank of Canada + the beneficiary FI".

nano-bank sends high-value wires (held, `status='sent'`), then waits for the
RTGS system to settle them with finality; it also emits recall requests
(camt.056) and receives inbound wires. This simulator plays that network:

  1. Polls `lynx_wires` (directly via Postgres, like `viewer`/`cleanup.sh`) for
     `status='sent'` outbound wires and calls
     `POST /lynx/network/wires/{id}/settle` on each — settlement finality.
  2. Resolves open outbound recalls (`lynx_recalls status='requested'`) via
     `POST /lynx/network/recalls/{id}/resolve` — mostly `accept` (the
     beneficiary returns the funds), occasionally `reject`.
  3. Periodically **originates an inbound wire** to a randomly chosen nano-bank
     customer account: `POST /lynx/network/inbound`.
  4. Occasionally **requests an inbound recall** of a settled inbound wire
     (an external sender wanting its money back):
     `POST /lynx/network/inbound-recall` (nano-bank claws back if funded).

Like the Visa/Interac/AFT simulators, the network plane authenticates with a
service token (client-credentials), minted/cached/re-minted on expiry or 401.

Config via env vars:
  API_BASE_URL            issuer API base            (default http://localhost:8081)
  SERVICE_CLIENT_SECRET   secret to mint a service token (default matches dev config)
  INTERVAL_SECONDS        delay between poll cycles, s   (default 7.0)
  INBOUND_PROB            chance per cycle of originating an inbound wire (default 0.3)
  RECALL_PROB             chance per cycle of requesting an inbound recall (default 0.1)
  RECALL_ACCEPT_PROB      chance an outbound recall is accepted           (default 0.7)
  MIN_AMOUNT / MAX_AMOUNT inbound amount range            (default 10000 / 500000)
  REQUEST_TIMEOUT         per-request timeout, s          (default 10)
  DB_HOST / DB_PORT       Postgres to poll               (default ::1 / 5432)
  DB_NAME / DB_USER / DB_PASSWORD   (defaults match local dev)
"""
from __future__ import annotations

import os
import random
import sys
import time

import psycopg2
import requests

API_BASE_URL = os.getenv("API_BASE_URL", "http://localhost:8081").rstrip("/")
SERVICE_CLIENT_SECRET = os.getenv(
    "SERVICE_CLIENT_SECRET", "nano-bank-visa-network-secret-change-me"
)
INTERVAL_SECONDS = float(os.getenv("INTERVAL_SECONDS", "7.0"))
INBOUND_PROB = float(os.getenv("INBOUND_PROB", "0.3"))
RECALL_PROB = float(os.getenv("RECALL_PROB", "0.1"))
RECALL_ACCEPT_PROB = float(os.getenv("RECALL_ACCEPT_PROB", "0.7"))
MIN_AMOUNT = float(os.getenv("MIN_AMOUNT", "10000"))
MAX_AMOUNT = float(os.getenv("MAX_AMOUNT", "500000"))
REQUEST_TIMEOUT = float(os.getenv("REQUEST_TIMEOUT", "10"))

DB = dict(
    host=os.getenv("DB_HOST", "::1"),
    port=int(os.getenv("DB_PORT", "5432")),
    dbname=os.getenv("DB_NAME", "nano_bank_db"),
    user=os.getenv("DB_USER", "nanobank_user"),
    password=os.getenv("DB_PASSWORD", "secure_nano_password_2024!"),
)

SERVICE_TOKEN_URL = f"{API_BASE_URL}/api/v1/auth/service-token"
HEALTH_URL = f"{API_BASE_URL}/health"
SENDERS = ["Global Trade Ltd", "Meridian Capital", "Northwind Freight",
           "Atlas Holdings", "Riverstone LP"]
SENDER_INSTS = ["001", "002", "003", "004", "010"]

_service_token: str | None = None
_token_expiry: float = 0.0


def log(msg: str) -> None:
    print(f"{time.strftime('%H:%M:%S')}  {msg}", flush=True)


# ---- service token / auth (same pattern as the AFT/Interac simulators) ----

def get_service_token(session: requests.Session, force: bool = False) -> str | None:
    global _service_token, _token_expiry
    if not force and _service_token is not None and time.monotonic() < _token_expiry:
        return _service_token
    try:
        resp = session.post(
            SERVICE_TOKEN_URL,
            json={"client_secret": SERVICE_CLIENT_SECRET},
            timeout=REQUEST_TIMEOUT,
        )
    except requests.RequestException as e:
        log(f"✗ service-token request failed: {e}")
        return None
    if resp.status_code != 200:
        log(f"✗ service-token {resp.status_code}: {resp.text[:160]}")
        return None
    data = resp.json()
    _service_token = data["access_token"]
    _token_expiry = time.monotonic() + max(float(data.get("expires_in", 3600)) - 30, 30)
    log("🔑 minted network service token")
    return _service_token


def authed_post(session: requests.Session, url: str, json_body: dict | None) -> requests.Response | None:
    for attempt in (1, 2):
        token = get_service_token(session, force=(attempt == 2))
        if token is None:
            return None
        try:
            resp = session.post(
                url,
                json=json_body,
                headers={"Authorization": f"Bearer {token}"},
                timeout=REQUEST_TIMEOUT,
            )
        except requests.RequestException as e:
            log(f"✗ request to {url} failed: {e}")
            return None
        if resp.status_code == 401 and attempt == 1:
            log("· service token rejected (401) — re-minting")
            continue
        return resp
    return None


# ---- DB reads ----

def _query(sql: str) -> list[tuple]:
    try:
        conn = psycopg2.connect(connect_timeout=5, **DB)
        try:
            with conn.cursor() as cur:
                cur.execute(sql)
                return cur.fetchall()
        finally:
            conn.close()
    except psycopg2.Error as e:
        log(f"✗ DB error: {e}")
        return []


def sent_wires() -> list[str]:
    return [str(r[0]) for r in _query(
        "SELECT wire_id FROM lynx_wires WHERE status='sent' AND direction='outbound'")]


def open_outbound_recalls() -> list[str]:
    return [str(r[0]) for r in _query(
        "SELECT recall_id FROM lynx_recalls WHERE status='requested' AND direction='outbound'")]


def settled_inbound_wire() -> str | None:
    rows = _query(
        "SELECT wire_id FROM lynx_wires WHERE status='settled' AND direction='inbound' "
        "ORDER BY random() LIMIT 1")
    return str(rows[0][0]) if rows else None


def pick_customer_account() -> tuple[str, str, str] | None:
    """A random non-system nano-bank chequing account: (institution, transit, account)."""
    rows = _query(
        "SELECT a.institution_number, a.transit_number, a.account_number FROM accounts a "
        "JOIN customers c ON c.customer_id = a.customer_id "
        "WHERE a.account_type='chequing' AND c.email NOT LIKE '%@nano.bank' "
        "ORDER BY random() LIMIT 1")
    return (rows[0][0], rows[0][1], rows[0][2]) if rows else None


# ---- actions ----

def settle_wire(session: requests.Session, wire_id: str) -> None:
    resp = authed_post(session, f"{API_BASE_URL}/api/v1/lynx/network/wires/{wire_id}/settle", None)
    if resp is None:
        return
    if resp.status_code == 200:
        log(f"🏦 settled wire {wire_id[:8]} (final)")
    else:
        log(f"· settle {wire_id[:8]} {resp.status_code}: {resp.text[:120]}")


def resolve_recall(session: requests.Session, recall_id: str) -> None:
    accept = random.random() < RECALL_ACCEPT_PROB
    body = {"decision": "accept" if accept else "reject",
            "reason": "returned" if accept else "beyond recall window"}
    resp = authed_post(session, f"{API_BASE_URL}/api/v1/lynx/network/recalls/{recall_id}/resolve", body)
    if resp is None:
        return
    if resp.status_code == 200:
        d = resp.json()
        log(f"↩️  recall {recall_id[:8]} {d.get('status')} → wire {d.get('wire_status')}")
    else:
        log(f"· resolve {recall_id[:8]} {resp.status_code}: {resp.text[:120]}")


def originate_inbound(session: requests.Session, coords: tuple[str, str, str]) -> None:
    inst, transit, acct = coords
    amount = round(random.uniform(MIN_AMOUNT, MAX_AMOUNT), 2)
    body = {
        "debtor_name": random.choice(SENDERS),
        "debtor_institution": random.choice(SENDER_INSTS),
        "debtor_account": str(random.randint(10**9, 10**12)),
        "beneficiary_institution": inst,
        "beneficiary_transit": transit,
        "beneficiary_account": acct,
        "amount": amount,
        "remittance_info": "wire settlement",
    }
    resp = authed_post(session, f"{API_BASE_URL}/api/v1/lynx/network/inbound", body)
    if resp is None:
        return
    if resp.status_code == 201:
        log(f"📥 inbound wire ${amount:,.2f} → …{acct[-4:]}")
    else:
        log(f"✗ inbound {resp.status_code}: {resp.text[:160]}")


def request_inbound_recall(session: requests.Session, wire_id: str) -> None:
    body = {"wire_id": wire_id, "decision": "accept", "reason": "sender recall"}
    resp = authed_post(session, f"{API_BASE_URL}/api/v1/lynx/network/inbound-recall", body)
    if resp is None:
        return
    if resp.status_code == 200:
        d = resp.json()
        log(f"🔁 inbound recall {wire_id[:8]} → {d.get('recall_status')} ({d.get('resolution')})")
    else:
        log(f"· inbound-recall {wire_id[:8]} {resp.status_code}: {resp.text[:120]}")


def wait_for_api(retries: int = 30) -> None:
    for i in range(1, retries + 1):
        try:
            if requests.get(HEALTH_URL, timeout=REQUEST_TIMEOUT).ok:
                log(f"nano-bank API healthy at {API_BASE_URL}")
                return
        except requests.RequestException:
            pass
        log(f"waiting for API ({i}/{retries}) …")
        time.sleep(2)
    log(f"⚠️  API never became healthy at {API_BASE_URL}; trying anyway")


def main() -> int:
    log(f"lynx simulator starting → {API_BASE_URL}  interval={INTERVAL_SECONDS}s "
        f"inbound_prob={INBOUND_PROB} recall_prob={RECALL_PROB}")
    wait_for_api()
    session = requests.Session()
    try:
        while True:
            for wire_id in sent_wires():
                settle_wire(session, wire_id)
            for recall_id in open_outbound_recalls():
                resolve_recall(session, recall_id)
            if random.random() < INBOUND_PROB:
                coords = pick_customer_account()
                if coords:
                    originate_inbound(session, coords)
            if random.random() < RECALL_PROB:
                wire_id = settled_inbound_wire()
                if wire_id:
                    request_inbound_recall(session, wire_id)
            time.sleep(INTERVAL_SECONDS)
    except KeyboardInterrupt:
        log("interrupted")
    return 0


if __name__ == "__main__":
    sys.exit(main())
