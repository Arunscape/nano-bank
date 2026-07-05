"""Mock Interac e-Transfer network — plays "the rest of the network".

nano-bank only owns its own customers' handles. Any e-Transfer sent to a handle
that isn't registered here is an *external* transfer: the money is held in
nano-bank's INTERAC_CLEARING account and a notification is queued for the far
bank to act on. This simulator plays that far bank:

  1. Polls the notification outbox (directly via Postgres, the way `viewer` and
     `cleanup.sh` read data — there's no admin read endpoint) for undelivered
     `incoming_transfer` notifications on **outbound, external** e-Transfers
     (`direction='outbound' AND recipient_customer_id IS NULL`). For each, it
     calls the network settle endpoint with a mostly-`claimed` (occasionally
     `declined`) outcome, then marks the notification delivered.
  2. Periodically **originates an inbound** transfer to a randomly chosen,
     currently-registered nano-bank handle (`interac_handles`), with a random
     amount from a made-up external sender. Whether it lands as an immediate
     autodeposit or a claim-required hold depends entirely on whether that
     handle has autodeposit registered — the simulator doesn't need to know.

Like the Visa rails simulator, the network plane authenticates with a service
token (`POST /api/v1/auth/service-token`, shared secret, client-credentials
style), not a cardholder JWT. We mint one, cache it, and re-mint on expiry or a
401.

Config via env vars:
  API_BASE_URL            issuer API base            (default http://localhost:8081)
  SERVICE_CLIENT_SECRET   secret to mint a service token (default matches dev config)
  INTERVAL_SECONDS        delay between poll cycles, s   (default 5.0)
  INBOUND_PROB            chance per cycle of originating an inbound transfer (default 0.3)
  DECLINE_PROB            chance a settle outcome is "declined" not "claimed" (default 0.15)
  SETTLE_INSTITUTION      institution code used when settling claims      (default 004)
  MIN_AMOUNT / MAX_AMOUNT inbound transfer amount range  (default 5 / 200)
  REQUEST_TIMEOUT         per-request timeout, s          (default 10)
  DB_HOST / DB_PORT       Postgres to poll notifications  (default ::1 / 5432)
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
INTERVAL_SECONDS = float(os.getenv("INTERVAL_SECONDS", "5.0"))
INBOUND_PROB = float(os.getenv("INBOUND_PROB", "0.3"))
DECLINE_PROB = float(os.getenv("DECLINE_PROB", "0.15"))
SETTLE_INSTITUTION = os.getenv("SETTLE_INSTITUTION", "004")
MIN_AMOUNT = float(os.getenv("MIN_AMOUNT", "5"))
MAX_AMOUNT = float(os.getenv("MAX_AMOUNT", "200"))
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

SENDER_NAMES = [
    "Alex Chen", "Priya Singh", "Jordan Lee", "Morgan Blake",
    "Sam Okafor", "Taylor Reyes", "Casey Nguyen", "Riley Dubois",
]
# Must match seeded rows in rail_participants (institution_number FK).
COUNTERPARTY_INSTITUTIONS = ["001", "002", "003", "004", "010"]

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


def fetch_undelivered_external() -> list[tuple[str, str]]:
    """Undelivered `incoming_transfer` notifications on outbound, external
    e-Transfers (no local recipient) — these are the ones the "far bank" (us)
    needs to act on."""
    try:
        conn = psycopg2.connect(connect_timeout=5, **DB)
        try:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    SELECT n.notification_id, e.etransfer_id
                    FROM interac_notifications n
                    JOIN interac_etransfers e ON e.etransfer_id = n.etransfer_id
                    WHERE n.delivered = FALSE AND n.kind = 'incoming_transfer'
                      AND e.direction = 'outbound' AND e.recipient_customer_id IS NULL
                    ORDER BY n.created_at
                    """
                )
                return [(str(nid), str(eid)) for nid, eid in cur.fetchall()]
        finally:
            conn.close()
    except psycopg2.Error as e:
        log(f"✗ DB error polling notifications: {e}")
        return []


def mark_delivered(notification_id: str) -> None:
    try:
        conn = psycopg2.connect(connect_timeout=5, **DB)
        try:
            with conn.cursor() as cur:
                cur.execute(
                    "UPDATE interac_notifications SET delivered = TRUE WHERE notification_id = %s",
                    (notification_id,),
                )
            conn.commit()
        finally:
            conn.close()
    except psycopg2.Error as e:
        log(f"✗ DB error marking notification {notification_id[:8]} delivered: {e}")


def pick_handle() -> tuple[str, str] | None:
    """Pick one random active nano-bank handle straight from Postgres, the way
    the network would already know which handles are registered here."""
    try:
        conn = psycopg2.connect(connect_timeout=5, **DB)
        try:
            with conn.cursor() as cur:
                cur.execute(
                    "SELECT handle_type::text, handle_value FROM interac_handles "
                    "WHERE active = TRUE ORDER BY random() LIMIT 1"
                )
                row = cur.fetchone()
                return (row[0], row[1]) if row else None
        finally:
            conn.close()
    except psycopg2.Error as e:
        log(f"✗ DB error picking handle: {e}")
        return None


def settle(session: requests.Session, etransfer_id: str, outcome: str) -> requests.Response | None:
    return authed_post(
        session,
        f"{API_BASE_URL}/api/v1/interac/network/etransfers/{etransfer_id}/settle",
        {"outcome": outcome, "institution": SETTLE_INSTITUTION},
    )


def originate_inbound(session: requests.Session, handle: tuple[str, str]) -> None:
    handle_type, handle_value = handle
    amount = round(random.uniform(MIN_AMOUNT, MAX_AMOUNT), 2)
    payload = {
        "amount": amount,
        "sender_name": random.choice(SENDER_NAMES),
        "counterparty_institution": random.choice(COUNTERPARTY_INSTITUTIONS),
        "recipient_handle_type": handle_type,
        "recipient_handle_value": handle_value,
        # Only used if the handle isn't autodeposit-registered; harmless otherwise.
        "security_question": "What is the secret word?",
        "security_answer": "banana",
        "memo": "Simulated inbound e-Transfer",
    }
    resp = authed_post(session, f"{API_BASE_URL}/api/v1/interac/network/inbound", payload)
    if resp is None:
        return
    if resp.status_code == 201:
        d = resp.json()
        log(f"📥 ${amount:,.2f} inbound → {handle_value}  status={d.get('status')}  "
            f"id={d['etransfer_id'][:8]}")
    else:
        log(f"✗ inbound {resp.status_code}: {resp.text[:160]}")


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
    log(f"interac simulator starting → {API_BASE_URL}  interval={INTERVAL_SECONDS}s  "
        f"inbound_prob={INBOUND_PROB}  decline_prob={DECLINE_PROB}")
    wait_for_api()

    session = requests.Session()
    waiting_logged = False
    try:
        while True:
            for nid, eid in fetch_undelivered_external():
                outcome = "declined" if random.random() < DECLINE_PROB else "claimed"
                resp = settle(session, eid, outcome)
                if resp is None:
                    # Connection-level failure — leave undelivered, retry next cycle.
                    continue
                # The network considers the notification delivered once it has acted
                # on it, whether the local API accepted the settlement (200) or
                # rejected it because the e-Transfer already moved on (409, e.g.
                # claimed/expired/declined by another path) — either way there's
                # nothing more for this notification to do.
                mark_delivered(nid)
                if resp.status_code == 200:
                    log(f"✓ settled e-Transfer {eid[:8]} → {outcome}  (status={resp.json().get('status')})")
                else:
                    log(f"· settle {eid[:8]} {resp.status_code} (already resolved?): {resp.text[:120]}")

            if random.random() < INBOUND_PROB:
                handle = pick_handle()
                if handle is None:
                    if not waiting_logged:
                        log("· no registered nano-bank handles yet — waiting for autodeposit registrations …")
                        waiting_logged = True
                else:
                    waiting_logged = False
                    originate_inbound(session, handle)

            time.sleep(INTERVAL_SECONDS)
    except KeyboardInterrupt:
        log("interrupted")
    return 0


if __name__ == "__main__":
    sys.exit(main())
