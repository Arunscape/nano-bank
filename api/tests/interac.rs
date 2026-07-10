//! Integration tests for the Interac e-Transfer rail.
//!
//! Same offline-skip harness as `tests/transactions.rs`: each test probes
//! `GET /health` and **returns early (skips) when the API is unreachable**, so
//! `cargo test` still passes with nothing running. Money movement posts to the
//! GL core, so tests skip when a funding deposit returns `503` (core down).
//!
//! Run against a live stack:
//! ```bash
//! cd api && cargo test --test interac -- --nocapture
//! ```
//! Override the base URL with `NANO_BANK_TEST_URL`.
//!
//! Reachability note: `register_autodeposit` always sets an autodeposit account,
//! so a *registered* handle always autodeposits. The **inbound-held** path
//! (registered-without-autodeposit) is therefore not reachable through the public
//! API in v1 (see the repo `CLAUDE.md` "Known v1 gaps") and is exercised by the
//! Python network simulator instead — out of scope for these HTTP tests. The
//! "available" (held-for-claim) transfers used below are the **external** kind
//! (unregistered recipient handle), whose `recipient_customer_id` is NULL.

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

macro_rules! require_stack {
    ($c:expr) => {
        if !stack_up($c).await {
            eprintln!("SKIP: nano-bank not reachable at {}", base_url());
            return;
        }
    };
}

fn as_num(v: &Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("not a number: {v:?}"))
}

fn rnd() -> u128 {
    Uuid::new_v4().as_u128()
}

async fn create_customer(c: &reqwest::Client) -> (Uuid, String) {
    let n = rnd();
    let email = format!("interac_{}@example.com", n % 1_000_000_000);
    let body = json!({
        "email": email,
        "phone_number": format!("{:010}", (n % 10_000_000_000u128)),
        "first_name": "Etr",
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
    assert!(resp.status().is_success(), "create customer: {}", resp.status());
    let v: Value = resp.json().await.unwrap();
    let id = Uuid::parse_str(v["customer_id"].as_str().unwrap()).unwrap();
    (id, email)
}

async fn login(c: &reqwest::Client, email: &str) -> String {
    let resp = c
        .post(format!("{}/api/v1/auth/login", base_url()))
        .json(&json!({ "email": email, "password": TEST_PASSWORD }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "login: {}", resp.status());
    let v: Value = resp.json().await.unwrap();
    v["access_token"].as_str().unwrap().to_string()
}

/// Sign up a fresh customer and log in, returning `(customer_id, token)`.
async fn session(c: &reqwest::Client) -> (Uuid, String) {
    let (id, email) = create_customer(c).await;
    let token = login(c, &email).await;
    (id, token)
}

async fn create_account(c: &reqwest::Client, token: &str, account_type: &str) -> Uuid {
    let resp = c
        .post(format!("{}/api/v1/accounts", base_url()))
        .bearer_auth(token)
        .json(&json!({ "account_type": account_type }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "create account: {}", resp.status());
    let v: Value = resp.json().await.unwrap();
    Uuid::parse_str(v["account_id"].as_str().unwrap()).unwrap()
}

async fn balance(c: &reqwest::Client, token: &str, account_id: Uuid) -> f64 {
    let v: Value = c
        .get(format!("{}/api/v1/accounts/{}/balance", base_url(), account_id))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    as_num(&v["balance"])
}

async fn post_json(c: &reqwest::Client, token: &str, path: &str, body: Value) -> reqwest::Response {
    c.post(format!("{}{}", base_url(), path))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// A funded chequing account, or `None` if the GL core isn't available (503).
async fn funded_account(c: &reqwest::Client, token: &str, amount: f64) -> Option<Uuid> {
    let account = create_account(c, token, "chequing").await;
    let resp = post_json(
        c,
        token,
        "/api/v1/transactions/deposit",
        json!({ "account_id": account, "amount": amount, "description": "seed" }),
    )
    .await;
    if resp.status().as_u16() == 503 {
        eprintln!("SKIP: GL core unavailable (deposit returned 503)");
        return None;
    }
    assert!(resp.status().is_success(), "deposit: {}", resp.status());
    Some(account)
}

async fn get_etransfer(c: &reqwest::Client, token: &str, id: Uuid) -> Value {
    c.get(format!("{}/api/v1/interac/etransfers/{}", base_url(), id))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Send an e-Transfer; returns the response so the caller can branch on 503/status.
async fn send_etransfer(c: &reqwest::Client, token: &str, body: Value) -> reqwest::Response {
    post_json(c, token, "/api/v1/interac/etransfers", body).await
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_requires_auth() {
    let c = client();
    require_stack!(&c);
    let resp = c
        .post(format!("{}/api/v1/interac/etransfers", base_url()))
        .json(&json!({
            "from_account_id": Uuid::new_v4(),
            "amount": 10.0,
            "recipient_handle_type": "email",
            "recipient_handle_value": "nobody@example.com"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401, "unauthenticated send must be 401");
}

// ---------------------------------------------------------------------------
// Autodeposit: send to a registered handle deposits immediately
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_to_autodeposit_handle_credits_recipient() {
    let c = client();
    require_stack!(&c);
    let (_a, a_token) = session(&c).await;
    let from = match funded_account(&c, &a_token, 500.0).await {
        Some(a) => a,
        None => return,
    };

    // Recipient B registers an autodeposit handle.
    let (_b, b_token) = session(&c).await;
    let b_acct = create_account(&c, &b_token, "chequing").await;
    let handle = format!("bob_{}@example.com", rnd() % 1_000_000_000);
    let reg = post_json(
        &c,
        &b_token,
        "/api/v1/interac/autodeposit",
        json!({ "handle_type": "email", "handle_value": handle, "deposit_account_id": b_acct }),
    )
    .await;
    assert!(reg.status().is_success(), "register autodeposit: {}", reg.status());

    let resp = send_etransfer(
        &c,
        &a_token,
        json!({ "from_account_id": from, "amount": 120.0,
                "recipient_handle_type": "email", "recipient_handle_value": handle }),
    )
    .await;
    assert!(resp.status().is_success(), "send: {}", resp.status());
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["status"], "deposited", "autodeposit should deposit: {v}");
    assert_eq!(balance(&c, &b_token, b_acct).await, 120.0);
}

// ---------------------------------------------------------------------------
// Held-for-claim (external recipient): send -> claim with Q&A
// ---------------------------------------------------------------------------

/// Send an "available" external transfer with a security Q&A; returns its id, or
/// `None` if the core is down.
async fn send_available(c: &reqwest::Client, a_token: &str, from: Uuid, amount: f64,
                        handle: &str, answer: &str) -> Option<Uuid> {
    let resp = send_etransfer(
        c,
        a_token,
        json!({ "from_account_id": from, "amount": amount,
                "recipient_handle_type": "email", "recipient_handle_value": handle,
                "security_question": "pet?", "security_answer": answer }),
    )
    .await;
    if resp.status().as_u16() == 503 {
        eprintln!("SKIP: GL core unavailable (send returned 503)");
        return None;
    }
    assert!(resp.status().is_success(), "send: {}", resp.status());
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["status"], "available", "held transfer should be available: {v}");
    Some(Uuid::parse_str(v["etransfer_id"].as_str().unwrap()).unwrap())
}

#[tokio::test]
async fn send_then_claim_with_correct_answer_deposits() {
    let c = client();
    require_stack!(&c);
    let (_a, a_token) = session(&c).await;
    let from = match funded_account(&c, &a_token, 500.0).await {
        Some(a) => a,
        None => return,
    };
    let handle = format!("ext_{}@other.test", rnd() % 1_000_000_000);
    let id = match send_available(&c, &a_token, from, 75.0, &handle, "rex").await {
        Some(id) => id,
        None => return,
    };

    // Recipient B claims by answering the question.
    let (_b, b_token) = session(&c).await;
    let b_acct = create_account(&c, &b_token, "chequing").await;
    let resp = post_json(
        &c,
        &b_token,
        &format!("/api/v1/interac/etransfers/{}/claim", id),
        json!({ "security_answer": "REX", "deposit_account_id": b_acct }),
    )
    .await;
    assert!(resp.status().is_success(), "claim: {}", resp.status());
    let claimed: Value = resp.json().await.unwrap();
    assert_eq!(claimed["status"], "deposited", "claim should deposit: {claimed}");
    assert_eq!(balance(&c, &b_token, b_acct).await, 75.0);

    // Claiming records B as the recipient, so B retains the receipt: a GET by B
    // now returns the transfer (before the fix this 404'd, because an external
    // transfer's recipient_customer_id was left NULL).
    let get = c
        .get(format!("{}/api/v1/interac/etransfers/{}", base_url(), id))
        .bearer_auth(&b_token)
        .send()
        .await
        .unwrap();
    assert_eq!(get.status().as_u16(), 200, "claimant should see the transfer after claiming");
    let seen: Value = get.json().await.unwrap();
    assert_eq!(seen["status"], "deposited", "claimant's GET should show deposited: {seen}");

    // ...and it appears in B's e-Transfer history.
    let list: Value = c
        .get(format!("{}/api/v1/interac/etransfers", base_url()))
        .bearer_auth(&b_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        list.as_array().unwrap().iter().any(|e| e["etransfer_id"] == claimed["etransfer_id"]),
        "claimed transfer should appear in B's list: {list}"
    );
}

#[tokio::test]
async fn claim_locks_after_three_wrong_answers() {
    let c = client();
    require_stack!(&c);
    let (_a, a_token) = session(&c).await;
    let from = match funded_account(&c, &a_token, 500.0).await {
        Some(a) => a,
        None => return,
    };
    let handle = format!("ext_{}@other.test", rnd() % 1_000_000_000);
    let id = match send_available(&c, &a_token, from, 40.0, &handle, "rex").await {
        Some(id) => id,
        None => return,
    };

    let (_b, b_token) = session(&c).await;
    let b_acct = create_account(&c, &b_token, "chequing").await;
    let wrong = json!({ "security_answer": "wrong", "deposit_account_id": b_acct });
    for attempt in 1..=3 {
        let resp = post_json(
            &c,
            &b_token,
            &format!("/api/v1/interac/etransfers/{}/claim", id),
            wrong.clone(),
        )
        .await;
        let code = resp.status().as_u16();
        if attempt < 3 {
            assert_eq!(code, 400, "wrong answer #{attempt} should be 400");
        } else {
            // The lock is an authorization failure (AppError::Authorization -> 403).
            assert_eq!(code, 403, "third wrong answer should lock (403)");
        }
    }
    // Check via the sender's token (an external recipient can't GET it).
    assert_eq!(get_etransfer(&c, &a_token, id).await["status"], "failed");
}

// ---------------------------------------------------------------------------
// Ownership: decline (recipient-only) and cancel (sender-only)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn decline_by_non_recipient_is_404() {
    let c = client();
    require_stack!(&c);
    let (_a, a_token) = session(&c).await;
    let from = match funded_account(&c, &a_token, 500.0).await {
        Some(a) => a,
        None => return,
    };
    // External "available" transfer: recipient_customer_id is NULL, so NO
    // authenticated customer may decline it (the fix). Pre-fix, any customer
    // could. A third, unrelated customer must get 404.
    let handle = format!("ext_{}@other.test", rnd() % 1_000_000_000);
    let id = match send_available(&c, &a_token, from, 20.0, &handle, "rex").await {
        Some(id) => id,
        None => return,
    };
    let (_c3, c3_token) = session(&c).await;
    let resp = post_json(
        &c,
        &c3_token,
        &format!("/api/v1/interac/etransfers/{}/decline", id),
        json!({}),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 404, "non-recipient decline must be 404");
    // The transfer is untouched (still available).
    assert_eq!(get_etransfer(&c, &a_token, id).await["status"], "available");
}

#[tokio::test]
async fn cancel_by_non_sender_is_404_but_sender_can_cancel() {
    let c = client();
    require_stack!(&c);
    let (_a, a_token) = session(&c).await;
    let from = match funded_account(&c, &a_token, 500.0).await {
        Some(a) => a,
        None => return,
    };
    let handle = format!("ext_{}@other.test", rnd() % 1_000_000_000);
    let id = match send_available(&c, &a_token, from, 30.0, &handle, "rex").await {
        Some(id) => id,
        None => return,
    };

    // A stranger cannot cancel A's transfer.
    let (_b, b_token) = session(&c).await;
    let resp = post_json(
        &c,
        &b_token,
        &format!("/api/v1/interac/etransfers/{}/cancel", id),
        json!({}),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 404, "non-sender cancel must be 404");

    // The sender can, and gets the funds back.
    let resp = post_json(
        &c,
        &a_token,
        &format!("/api/v1/interac/etransfers/{}/cancel", id),
        json!({}),
    )
    .await;
    assert!(resp.status().is_success(), "sender cancel: {}", resp.status());
    assert_eq!(get_etransfer(&c, &a_token, id).await["status"], "cancelled");
    // Funds returned: started 500, sent 30 (held), cancelled -> back to 500.
    assert_eq!(balance(&c, &a_token, from).await, 500.0);
}
