"""Mock AFT/EFT clearing system (ACSS) — plays "the batch network".

nano-bank customers accrue AFT credits/debits into the shared open batch; the
bank cuts (submits) that batch — emitting a CPA-005 file — and the clearing
system settles it and, later, returns some debits. Submitting the shared file is
a bank op (service token), so this simulator plays both the bank cutoff and the
clearing system:

  0. Polls `aft_batches` for `status='open'` outbound batches and calls
     `POST /aft/batches/{batch}/submit` (service token) to cut the file.
  1. Polls `aft_batches` (directly via Postgres, the way `viewer`/`cleanup.sh`
     read data) for `status='submitted'` batches and calls
     `POST /aft/network/settle/{batch}` on each.
  2. Periodically **originates an inbound batch** — builds a CPA-005 file
     crediting a randomly chosen nano-bank customer account and posts it to
     `POST /aft/network/inbound-batch`.
  3. For a fraction of settled outbound **credit** entries, builds a **returns**
     CPA-005 file (with an NSF reason) and posts it to
     `POST /aft/network/returns`, bouncing the entry back to the originator.

Like the Visa/Interac simulators, the network plane authenticates with a service
token (client-credentials), minted/cached/re-minted on expiry or 401.

Config via env vars:
  API_BASE_URL            issuer API base            (default http://localhost:8081)
  SERVICE_CLIENT_SECRET   secret to mint a service token (default matches dev config)
  INTERVAL_SECONDS        delay between poll cycles, s   (default 6.0)
  INBOUND_PROB            chance per cycle of originating an inbound batch (default 0.3)
  RETURN_PROB             chance per cycle of returning a settled credit   (default 0.2)
  MIN_AMOUNT / MAX_AMOUNT inbound amount range            (default 20 / 500)
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
INTERVAL_SECONDS = float(os.getenv("INTERVAL_SECONDS", "6.0"))
INBOUND_PROB = float(os.getenv("INBOUND_PROB", "0.3"))
RETURN_PROB = float(os.getenv("RETURN_PROB", "0.2"))
MIN_AMOUNT = float(os.getenv("MIN_AMOUNT", "20"))
MAX_AMOUNT = float(os.getenv("MAX_AMOUNT", "500"))
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
PAYERS = ["Payroll Inc", "Gov Benefits", "Acme Corp", "Hydro One", "Rent Co"]

_service_token: str | None = None
_token_expiry: float = 0.0


def log(msg: str) -> None:
    print(f"{time.strftime('%H:%M:%S')}  {msg}", flush=True)


# ---- CPA-005-style encoder (must match api/src/aft/cpa005.rs field widths) ----

def _field(s: object, w: int) -> str:
    s = str(s)[:w]
    return s + " " * (w - len(s))


def _cents(a: float) -> str:
    return f"{int(round(a * 100)):010d}"


def build_file(details: list[dict]) -> str:
    """details: list of {code,amount,inst,transit,acct,payee,reason?}."""
    lines = ["H" + _field("900", 10) + _field("2026186", 7) + f"{1:06d}"]
    tc = sum(d["amount"] for d in details if d["code"] == "C")
    td = sum(d["amount"] for d in details if d["code"] == "D")
    for d in details:
        lines.append(
            d["code"]
            + _cents(d["amount"])
            + _field(d["inst"], 3)
            + _field(d["transit"], 5)
            + _field(d["acct"], 12)
            + _field(d["payee"], 30)
            + _field("NANO", 4)
            + _field("2026186", 7)
            + _field(d.get("reason", ""), 4)
        )
    lines.append("T" + f"{len(details):06d}" + _cents(tc) + _cents(td))
    return "\n".join(lines)


# ---- service token / auth (same pattern as the interac simulator) ----

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


def open_batches() -> list[str]:
    return [
        str(r[0])
        for r in _query(
            "SELECT batch_id FROM aft_batches WHERE status='open' AND direction='outbound'"
        )
    ]


def submitted_batches() -> list[str]:
    return [str(r[0]) for r in _query("SELECT batch_id FROM aft_batches WHERE status='submitted'")]


def pick_customer_account() -> tuple[str, str] | None:
    """A random non-system nano-bank chequing account: (transit, account_number)."""
    rows = _query(
        "SELECT a.transit_number, a.account_number FROM accounts a "
        "JOIN customers c ON c.customer_id = a.customer_id "
        "WHERE a.account_type='chequing' AND c.email NOT LIKE '%@nano.bank' "
        "ORDER BY random() LIMIT 1"
    )
    return (rows[0][0], rows[0][1]) if rows else None


def settled_credit_to_return() -> tuple[float, str] | None:
    """A random settled outbound credit entry: (amount, counterparty_account)."""
    rows = _query(
        "SELECT amount, counterparty_account FROM aft_entries "
        "WHERE status='settled' AND kind='credit' AND direction='outbound' "
        "AND counterparty_account IS NOT NULL ORDER BY random() LIMIT 1"
    )
    return (float(rows[0][0]), rows[0][1]) if rows else None


# ---- actions ----

def settle_batch(session: requests.Session, batch_id: str) -> None:
    resp = authed_post(session, f"{API_BASE_URL}/api/v1/aft/network/settle/{batch_id}", None)
    if resp is None:
        return
    if resp.status_code == 200:
        d = resp.json()
        log(f"🏦 settled batch {batch_id[:8]}  settled={d.get('settled_entries')} "
            f"rejected={d.get('rejected')} swept={d.get('swept_credits')}")
    else:
        log(f"· settle {batch_id[:8]} {resp.status_code}: {resp.text[:120]}")


def submit_batch(session: requests.Session, batch_id: str) -> None:
    # Cutting the shared outbound file is a bank/network op (service token), not a
    # customer op — the ACSS sim plays the bank here.
    resp = authed_post(session, f"{API_BASE_URL}/api/v1/aft/batches/{batch_id}/submit", None)
    if resp is None:
        return
    if resp.status_code == 200:
        log(f"📄 submitted batch {batch_id[:8]}")
    else:
        log(f"· submit {batch_id[:8]} {resp.status_code}: {resp.text[:120]}")


def originate_inbound(session: requests.Session, coords: tuple[str, str]) -> None:
    transit, acct = coords
    amount = round(random.uniform(MIN_AMOUNT, MAX_AMOUNT), 2)
    file = build_file([{
        "code": "C", "amount": amount, "inst": "900", "transit": transit,
        "acct": acct, "payee": random.choice(PAYERS),
    }])
    resp = authed_post(session, f"{API_BASE_URL}/api/v1/aft/network/inbound-batch", {"file": file})
    if resp is None:
        return
    if resp.status_code == 201:
        d = resp.json()
        log(f"📥 inbound batch → credited={d.get('credited')} (${amount:,.2f} → …{acct[-4:]})")
    else:
        log(f"✗ inbound {resp.status_code}: {resp.text[:160]}")


def originate_return(session: requests.Session, credit: tuple[float, str]) -> None:
    amount, acct = credit
    file = build_file([{
        "code": "C", "amount": amount, "inst": "003", "transit": "00050",
        "acct": acct, "payee": "Returned", "reason": "NSF",
    }])
    resp = authed_post(session, f"{API_BASE_URL}/api/v1/aft/network/returns", {"file": file})
    if resp is None:
        return
    if resp.status_code == 200:
        log(f"↩️  returns → returned={resp.json().get('returned')} (${amount:,.2f})")
    else:
        log(f"✗ returns {resp.status_code}: {resp.text[:160]}")


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
    log(f"aft simulator starting → {API_BASE_URL}  interval={INTERVAL_SECONDS}s "
        f"inbound_prob={INBOUND_PROB} return_prob={RETURN_PROB}")
    wait_for_api()
    session = requests.Session()
    try:
        while True:
            for batch_id in open_batches():
                submit_batch(session, batch_id)
            for batch_id in submitted_batches():
                settle_batch(session, batch_id)
            if random.random() < INBOUND_PROB:
                coords = pick_customer_account()
                if coords:
                    originate_inbound(session, coords)
            if random.random() < RETURN_PROB:
                credit = settled_credit_to_return()
                if credit:
                    originate_return(session, credit)
            time.sleep(INTERVAL_SECONDS)
    except KeyboardInterrupt:
        log("interrupted")
    return 0


if __name__ == "__main__":
    sys.exit(main())
