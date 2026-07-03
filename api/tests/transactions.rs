//! Integration tests for the transaction endpoints.
//!
//! These drive the **real HTTP surface** of a running stack (the API on
//! `:8081` + the Kind Postgres + one ledger core), because the package is a
//! binary (its items aren't importable here) and deposit/withdrawal post to the
//! GL core.
//!
//! Every test probes `GET /health` first and **returns early (skips) when the
//! API is unreachable**, so `cargo test` still passes with nothing running.
//! Tests that need a working GL core (deposit/withdrawal/transfer) additionally
//! skip if a deposit comes back `503` (core down).
//!
//! The money-movement endpoints require a **customer access token** (see the
//! auth plane added in the auth PR): each test signs up a customer, logs in via
//! `POST /api/v1/auth/login`, and sends the returned bearer token on every
//! authenticated call. Identity is taken from the token, so accounts are always
//! created for — and operated by — the logged-in customer.
//!
//! Run against a live stack:
//! ```bash
//! # terminal 1: DB + core + API (see repo CLAUDE.md), then:
//! cd api && cargo test --test transactions -- --nocapture
//! ```
//! Override the base URL with `NANO_BANK_TEST_URL`.

use serde_json::{json, Value};
use uuid::Uuid;

const TEST_PASSWORD: &str = "securepass123";

fn base_url() -> String {
    std::env::var("NANO_BANK_TEST_URL").unwrap_or_else(|_| "http://localhost:8081".to_string())
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn stack_up(c: &reqwest::Client) -> bool {
    matches!(
        c.get(format!("{}/health", base_url())).send().await,
        Ok(r) if r.status().is_success()
    )
}

/// Skip the test (return) if the API isn't reachable.
macro_rules! require_stack {
    ($c:expr) => {
        if !stack_up($c).await {
            eprintln!("SKIP: nano-bank not reachable at {}", base_url());
            return;
        }
    };
}

/// rust_decimal may serialize as a JSON number or string depending on config;
/// accept either.
fn as_num(v: &Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("not a number: {v:?}"))
}

/// Create a customer (public signup) and return `(customer_id, email)`.
async fn create_customer(c: &reqwest::Client) -> (Uuid, String) {
    let n = Uuid::new_v4().as_u128();
    let email = format!("txntest_{}@example.com", n % 1_000_000_000);
    let body = json!({
        "email": email,
        "phone_number": format!("{:010}", (n % 10_000_000_000u128)),
        "first_name": "Txn",
        "last_name": "Test",
        "date_of_birth": "1990-01-01",
        "sin": format!("{:09}", n % 1_000_000_000),
        "password": TEST_PASSWORD
    });
    let resp = c
        .post(format!("{}/api/v1/customers", base_url()))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "create customer: {}",
        resp.status()
    );
    let v: Value = resp.json().await.unwrap();
    let id = Uuid::parse_str(v["customer_id"].as_str().unwrap()).unwrap();
    (id, email)
}

/// Log in and return a customer access token (for `Authorization: Bearer`).
async fn login(c: &reqwest::Client, email: &str) -> String {
    let resp = c
        .post(format!("{}/api/v1/auth/login", base_url()))
        .json(&json!({ "email": email, "password": TEST_PASSWORD }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "login: {}", resp.status());
    let v: Value = resp.json().await.unwrap();
    v["access_token"]
        .as_str()
        .expect("login response has an access_token")
        .to_string()
}

/// Sign up a fresh customer and log in, returning `(customer_id, token)`.
async fn session(c: &reqwest::Client) -> (Uuid, String) {
    let (id, email) = create_customer(c).await;
    let token = login(c, &email).await;
    (id, token)
}

/// Open an account for the logged-in customer (identity comes from the token).
async fn create_account(c: &reqwest::Client, token: &str, account_type: &str) -> Uuid {
    let resp = c
        .post(format!("{}/api/v1/accounts", base_url()))
        .bearer_auth(token)
        .json(&json!({ "account_type": account_type }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "create account: {}",
        resp.status()
    );
    let v: Value = resp.json().await.unwrap();
    Uuid::parse_str(v["account_id"].as_str().unwrap()).unwrap()
}

async fn balance(c: &reqwest::Client, token: &str, account_id: Uuid) -> f64 {
    let v: Value = c
        .get(format!(
            "{}/api/v1/accounts/{}/balance",
            base_url(),
            account_id
        ))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    as_num(&v["balance"])
}

/// POST a JSON body to an authenticated endpoint with the caller's token.
async fn post_json(c: &reqwest::Client, token: &str, path: &str, body: Value) -> reqwest::Response {
    c.post(format!("{}{}", base_url(), path))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// GET transaction history for the caller (optionally scoped to one account).
async fn history(c: &reqwest::Client, token: &str, account_id: Option<Uuid>) -> Value {
    let url = match account_id {
        Some(a) => format!("{}/api/v1/transactions?account_id={}", base_url(), a),
        None => format!("{}/api/v1/transactions", base_url()),
    };
    c.get(url)
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

// A funded chequing account, or `None` if the GL core isn't available (503).
async fn funded_account(c: &reqwest::Client, token: &str, amount: f64) -> Option<Uuid> {
    let account = create_account(c, token, "chequing").await;
    let resp = post_json(
        c,
        token,
        "/api/v1/transactions/deposit",
        json!({ "account_id": account, "amount": amount, "description": "seed funds" }),
    )
    .await;
    if resp.status().as_u16() == 503 {
        eprintln!("SKIP: GL core unavailable (deposit returned 503)");
        return None;
    }
    assert!(resp.status().is_success(), "deposit: {}", resp.status());
    Some(account)
}

// ---------------------------------------------------------------------------
// Happy path: deposit -> transfer -> withdrawal, with balance math
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deposit_transfer_withdraw_flow() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;

    let a = match funded_account(&c, &token, 1000.0).await {
        Some(a) => a,
        None => return,
    };
    assert_eq!(balance(&c, &token, a).await, 1000.0);

    let b = create_account(&c, &token, "savings").await;

    // transfer 400 a -> b
    let resp = post_json(
        &c,
        &token,
        "/api/v1/transactions/transfer",
        json!({ "from_account_id": a, "to_account_id": b, "amount": 400.0, "description": "rent" }),
    )
    .await;
    assert!(resp.status().is_success(), "transfer: {}", resp.status());
    assert_eq!(balance(&c, &token, a).await, 600.0);
    assert_eq!(balance(&c, &token, b).await, 400.0);

    // withdraw 100 from a
    let resp = post_json(
        &c,
        &token,
        "/api/v1/transactions/withdrawal",
        json!({ "account_id": a, "amount": 100.0, "description": "atm" }),
    )
    .await;
    assert!(resp.status().is_success(), "withdrawal: {}", resp.status());
    assert_eq!(balance(&c, &token, a).await, 500.0);

    // history for account a: deposit + transfer + withdrawal = 3
    let v = history(&c, &token, Some(a)).await;
    assert_eq!(v["total_count"].as_u64().unwrap(), 3, "history: {v}");
    assert_eq!(v["transactions"].as_array().unwrap().len(), 3);
    // newest first
    assert_eq!(v["transactions"][0]["transaction_type"], "withdrawal");
    // each transaction is hydrated with its balanced entries
    assert_eq!(v["transactions"][0]["entries"].as_array().unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// Auth: the money-movement endpoints reject an unauthenticated caller
// ---------------------------------------------------------------------------

#[tokio::test]
async fn transactions_require_auth() {
    let c = client();
    require_stack!(&c);

    // No bearer token → 401 on a write endpoint...
    let resp = c
        .post(format!("{}/api/v1/transactions/deposit", base_url()))
        .json(&json!({ "account_id": Uuid::new_v4(), "amount": 10.0, "description": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401, "deposit without a token");

    // ...and on history.
    let resp = c
        .get(format!("{}/api/v1/transactions", base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401, "history without a token");
}

// ---------------------------------------------------------------------------
// Ownership: you can't move money out of an account you don't own (404)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cannot_deposit_into_another_customers_account() {
    let c = client();
    require_stack!(&c);

    // Victim owns an account.
    let (_victim, victim_token) = session(&c).await;
    let victim_account = create_account(&c, &victim_token, "chequing").await;

    // Attacker, with a valid token of their own, targets the victim's account id.
    let (_attacker, attacker_token) = session(&c).await;
    let resp = post_json(
        &c,
        &attacker_token,
        "/api/v1/transactions/deposit",
        json!({ "account_id": victim_account, "amount": 10.0, "description": "not mine" }),
    )
    .await;
    // 404 (not 403) so we don't reveal the account exists.
    assert_eq!(
        resp.status().as_u16(),
        404,
        "depositing into another customer's account must 404"
    );
}

// ---------------------------------------------------------------------------
// Validation / rejection paths (no GL core needed — they fail before posting)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn withdrawal_insufficient_funds() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let account = create_account(&c, &token, "chequing").await; // balance 0

    let resp = post_json(
        &c,
        &token,
        "/api/v1/transactions/withdrawal",
        json!({ "account_id": account, "amount": 10.0, "description": "atm" }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "INSUFFICIENT_FUNDS");
}

#[tokio::test]
async fn transfer_same_account_rejected() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let account = create_account(&c, &token, "chequing").await;

    let resp = post_json(
        &c,
        &token,
        "/api/v1/transactions/transfer",
        json!({ "from_account_id": account, "to_account_id": account, "amount": 5.0, "description": "self" }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn deposit_to_credit_card_rejected() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let card = create_account(&c, &token, "credit_card").await;

    let resp = post_json(
        &c,
        &token,
        "/api/v1/transactions/deposit",
        json!({ "account_id": card, "amount": 10.0, "description": "nope" }),
    )
    .await;
    assert_eq!(
        resp.status().as_u16(),
        400,
        "credit-card deposit should be rejected"
    );
}

#[tokio::test]
async fn deposit_negative_amount_rejected() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let account = create_account(&c, &token, "chequing").await;

    let resp = post_json(
        &c,
        &token,
        "/api/v1/transactions/deposit",
        json!({ "account_id": account, "amount": -5.0, "description": "bad" }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
}

// ---------------------------------------------------------------------------
// Idempotent transfer replay (needs GL core to seed funds)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn transfer_is_idempotent() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let a = match funded_account(&c, &token, 500.0).await {
        Some(a) => a,
        None => return,
    };
    let b = create_account(&c, &token, "savings").await;
    let key = format!("idem-{}", Uuid::new_v4());

    let body = json!({
        "from_account_id": a, "to_account_id": b, "amount": 200.0,
        "description": "dup", "idempotency_key": key
    });

    let r1 = post_json(&c, &token, "/api/v1/transactions/transfer", body.clone()).await;
    assert_eq!(r1.status().as_u16(), 201);
    let v1: Value = r1.json().await.unwrap();

    let r2 = post_json(&c, &token, "/api/v1/transactions/transfer", body).await;
    assert_eq!(
        r2.status().as_u16(),
        200,
        "replay should be 200, not a new post"
    );
    let v2: Value = r2.json().await.unwrap();

    assert_eq!(
        v1["transaction_id"], v2["transaction_id"],
        "same txn returned"
    );
    // Only one transfer actually moved money.
    assert_eq!(balance(&c, &token, a).await, 300.0);
    assert_eq!(balance(&c, &token, b).await, 200.0);
}

// ---------------------------------------------------------------------------
// Daily withdrawal limit (default 1000/day)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn daily_withdrawal_limit_enforced() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let a = match funded_account(&c, &token, 2000.0).await {
        Some(a) => a,
        None => return,
    };

    // First 600 ok (used=600 <= 1000).
    let r1 = post_json(
        &c,
        &token,
        "/api/v1/transactions/withdrawal",
        json!({ "account_id": a, "amount": 600.0, "description": "w1" }),
    )
    .await;
    assert_eq!(r1.status().as_u16(), 201);

    // Second 600 would push used to 1200 > 1000 daily limit -> rejected.
    let r2 = post_json(
        &c,
        &token,
        "/api/v1/transactions/withdrawal",
        json!({ "account_id": a, "amount": 600.0, "description": "w2" }),
    )
    .await;
    assert_eq!(r2.status().as_u16(), 400);
    let v: Value = r2.json().await.unwrap();
    assert_eq!(v["error"]["code"], "TRANSACTION_LIMIT_EXCEEDED");

    // The rejected withdrawal must not have moved money.
    assert_eq!(balance(&c, &token, a).await, 1400.0);
}
