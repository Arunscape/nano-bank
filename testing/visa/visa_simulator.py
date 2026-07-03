"""Mock Visa payment rails — merchant + network simulator.

Drives nano-bank's issuer card endpoints to exercise the full credit-card flow:

    authorize → capture → settle (batch)

Each tick it picks a random active credit card (discovered straight from
Postgres, the way the network would already hold card references), authorizes a
random purchase at a Faker merchant, and usually captures it. Every
SETTLE_INTERVAL_SECONDS it triggers a settlement batch on the issuer.

The card rails are the issuer's *network plane*: they authenticate the card
network/processor (this simulator) with a service token, not a cardholder. We
mint one via the client-credentials endpoint (`POST /api/v1/auth/service-token`)
using a shared secret, cache it, and re-mint on expiry or a 401.

Config via env vars:
  API_BASE_URL            issuer API base        (default http://localhost:8081)
  SERVICE_CLIENT_SECRET   secret to mint a service token (default matches dev config)
  INTERVAL_SECONDS        delay between purchases (default 2.0)
  SETTLE_INTERVAL_SECONDS settlement batch cadence (default 30)
  CAPTURE_PROB            chance an approved auth is captured (default 0.9)
  MIN_AMOUNT / MAX_AMOUNT purchase amount range  (default 5 / 500)
  REQUEST_TIMEOUT         per-request timeout, s  (default 10)
  DB_HOST / DB_PORT       Postgres to discover cards (default ::1 / 5432)
  DB_NAME / DB_USER / DB_PASSWORD   (defaults match local dev)
"""
from __future__ import annotations

import os
import random
import sys
import time

import psycopg2
import requests
from faker import Faker

API_BASE_URL = os.getenv("API_BASE_URL", "http://localhost:8081").rstrip("/")
SERVICE_CLIENT_SECRET = os.getenv(
    "SERVICE_CLIENT_SECRET", "nano-bank-visa-network-secret-change-me"
)
INTERVAL_SECONDS = float(os.getenv("INTERVAL_SECONDS", "2.0"))
SETTLE_INTERVAL_SECONDS = float(os.getenv("SETTLE_INTERVAL_SECONDS", "30"))
CAPTURE_PROB = float(os.getenv("CAPTURE_PROB", "0.9"))
MIN_AMOUNT = float(os.getenv("MIN_AMOUNT", "5"))
MAX_AMOUNT = float(os.getenv("MAX_AMOUNT", "500"))
REQUEST_TIMEOUT = float(os.getenv("REQUEST_TIMEOUT", "10"))

DB = dict(
    host=os.getenv("DB_HOST", "::1"),
    port=int(os.getenv("DB_PORT", "5432")),
    dbname=os.getenv("DB_NAME", "nano_bank_db"),
    user=os.getenv("DB_USER", "nanobank_user"),
    password=os.getenv("DB_PASSWORD", "secure_nano_password_2024!"),
)

AUTHORIZE_URL = f"{API_BASE_URL}/api/v1/cards/authorize"
CAPTURE_URL = f"{API_BASE_URL}/api/v1/cards/capture"
SETTLE_URL = f"{API_BASE_URL}/api/v1/cards/settle"
SERVICE_TOKEN_URL = f"{API_BASE_URL}/api/v1/auth/service-token"
HEALTH_URL = f"{API_BASE_URL}/health"

fake = Faker("en_CA")

# Cached service token: (token, monotonic expiry deadline). Re-minted lazily.
_service_token: str | None = None
_token_expiry: float = 0.0


def log(msg: str) -> None:
    print(f"{time.strftime('%H:%M:%S')}  {msg}", flush=True)


def get_service_token(session: requests.Session, force: bool = False) -> str | None:
    """Return a valid network-plane service token, minting/refreshing as needed.

    Uses the client-credentials endpoint with SERVICE_CLIENT_SECRET. Tokens are
    cached until ~30s before expiry; `force=True` re-mints immediately (used
    after a 401)."""
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
    # Refresh a little before the real expiry to avoid racing the clock.
    _token_expiry = time.monotonic() + max(float(data.get("expires_in", 3600)) - 30, 30)
    log("🔑 minted network service token")
    return _service_token


def authed_post(session: requests.Session, url: str, json_body: dict | None) -> requests.Response | None:
    """POST with the service token; on a 401 re-mint once and retry. Returns the
    Response, or None on a connection-level failure."""
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


def pick_card() -> str | None:
    """Pick one random active credit-card account id straight from Postgres."""
    try:
        conn = psycopg2.connect(connect_timeout=5, **DB)
        try:
            with conn.cursor() as cur:
                cur.execute(
                    "SELECT account_id FROM accounts "
                    "WHERE account_type = 'credit_card' AND status = 'active' "
                    "ORDER BY random() LIMIT 1"
                )
                row = cur.fetchone()
                return str(row[0]) if row else None
        finally:
            conn.close()
    except psycopg2.Error as e:
        log(f"✗ DB error picking card: {e}")
        return None


def authorize(session: requests.Session, card_id: str, amount: float, merchant: str) -> dict | None:
    """Return the auth response dict, or None on request failure."""
    payload = {"account_id": card_id, "amount": round(amount, 2), "merchant": merchant}
    # Future: enrich with fraud signals (device fingerprint, geo, MCC) here so
    # the issuer's risk engine has more to score on.
    resp = authed_post(session, AUTHORIZE_URL, payload)
    if resp is None:
        return None
    if resp.status_code in (200, 201):
        return resp.json()
    log(f"✗ authorize {resp.status_code}: {resp.text[:160]}")
    return None


def capture(session: requests.Session, auth_id: str) -> bool:
    resp = authed_post(session, CAPTURE_URL, {"auth_id": auth_id})
    if resp is None:
        return False
    if resp.status_code == 201:
        return True
    log(f"  ✗ capture {resp.status_code}: {resp.text[:160]}")
    return False


def settle(session: requests.Session) -> None:
    resp = authed_post(session, SETTLE_URL, None)
    if resp is None:
        return
    if resp.status_code in (200, 201):
        d = resp.json()
        if d.get("status") == "settled":
            log(f"⚙ settlement batch: net=${float(d['net_amount']):,.2f} "
                f"over {d['settled_transactions']} purchase(s)  ref={d.get('reference_number')}")
        else:
            log("⚙ settlement batch: nothing to settle")
    else:
        log(f"⚙ settlement {resp.status_code}: {resp.text[:160]}")


def wait_for_api(retries: int = 30) -> None:
    for i in range(1, retries + 1):
        try:
            if requests.get(HEALTH_URL, timeout=REQUEST_TIMEOUT).ok:
                log(f"issuer API healthy at {API_BASE_URL}")
                return
        except requests.RequestException:
            pass
        log(f"waiting for API ({i}/{retries}) …")
        time.sleep(2)
    log(f"⚠️  API never became healthy at {API_BASE_URL}; trying anyway")


def main() -> int:
    log(f"visa simulator starting → {API_BASE_URL}  interval={INTERVAL_SECONDS}s  "
        f"settle_every={SETTLE_INTERVAL_SECONDS}s  capture_prob={CAPTURE_PROB}")
    wait_for_api()

    session = requests.Session()
    last_settle = time.monotonic()
    waiting_logged = False
    try:
        while True:
            card_id = pick_card()
            if not card_id:
                if not waiting_logged:
                    log("· no active credit cards yet — waiting for the generator …")
                    waiting_logged = True
                time.sleep(INTERVAL_SECONDS)
                continue
            waiting_logged = False

            amount = random.uniform(MIN_AMOUNT, MAX_AMOUNT)
            merchant = fake.company()
            auth = authorize(session, card_id, amount, merchant)
            if auth and auth.get("status") == "approved":
                amt = float(auth["amount"])
                if random.random() < CAPTURE_PROB and capture(session, auth["auth_id"]):
                    log(f"✓ ${amt:,.2f} @ {merchant}  card={card_id[:8]}  → captured")
                else:
                    log(f"~ ${amt:,.2f} @ {merchant}  card={card_id[:8]}  → authorized only "
                        f"(hold {auth['auth_id'][:8]})")
            elif auth and auth.get("status") == "declined":
                log(f"✗ ${round(amount, 2):,.2f} @ {merchant}  card={card_id[:8]}  → DECLINED "
                    f"({auth.get('reason')})")

            if time.monotonic() - last_settle >= SETTLE_INTERVAL_SECONDS:
                settle(session)
                last_settle = time.monotonic()

            time.sleep(INTERVAL_SECONDS)
    except KeyboardInterrupt:
        log("interrupted")
    return 0


if __name__ == "__main__":
    sys.exit(main())
