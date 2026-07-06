//! Integration tests for the agentic-banking plane (Phase 1: read-only).
//!
//! Same harness as `tests/transactions.rs`: every test probes `GET /health`
//! and **skips (still passes)** when the API isn't running; DB-level audit
//! assertions additionally skip if Postgres isn't reachable. Unlike the
//! transaction tests, most of these need **no GL core** — reads don't
//! dual-post — so they run against just the API + Postgres. Only the history
//! test seeds a deposit (and 503-skips when the core is down).
//!
//! Run against a live stack:
//! ```bash
//! cd api && cargo test --test agents -- --nocapture
//! ```
//! Override the base URL with `NANO_BANK_TEST_URL`, the DB with
//! `NANO_BANK_TEST_DB_URL`.

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

/// rust_decimal may serialize as a JSON number or string; accept either.
fn as_num(v: &Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("not a number: {v:?}"))
}

/// The `error.code` of a non-2xx response body.
async fn error_code(resp: reqwest::Response) -> String {
    let v: Value = resp.json().await.unwrap();
    v["error"]["code"].as_str().unwrap_or("").to_string()
}

async fn create_customer(c: &reqwest::Client) -> (Uuid, String) {
    let n = Uuid::new_v4().as_u128();
    let email = format!("agenttest_{}@example.com", n % 1_000_000_000);
    let body = json!({
        "email": email,
        "phone_number": format!("{:010}", (n % 10_000_000_000u128)),
        "first_name": "Agent",
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
    assert!(
        resp.status().is_success(),
        "create account: {}",
        resp.status()
    );
    let v: Value = resp.json().await.unwrap();
    Uuid::parse_str(v["account_id"].as_str().unwrap()).unwrap()
}

/// Customer-side balance view (`GET /accounts/{id}/balance`).
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

/// Register an agent (open endpoint), returning `(agent_id, agent_secret)`.
async fn register_agent(c: &reqwest::Client) -> (Uuid, String) {
    let resp = c
        .post(format!("{}/api/v1/agents", base_url()))
        .json(&json!({
            "display_name": "Test Assistant",
            "description": "integration-test agent"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "register agent");
    let v: Value = resp.json().await.unwrap();
    (
        Uuid::parse_str(v["agent_id"].as_str().unwrap()).unwrap(),
        v["agent_secret"].as_str().unwrap().to_string(),
    )
}

fn in_one_hour() -> String {
    (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()
}

/// Grant a mandate as the customer; asserts 201 and returns the mandate id.
async fn grant_mandate(
    c: &reqwest::Client,
    token: &str,
    agent_id: Uuid,
    account_id: Uuid,
    scopes: &[&str],
) -> Uuid {
    let resp = c
        .post(format!("{}/api/v1/mandates", base_url()))
        .bearer_auth(token)
        .json(&json!({
            "agent_id": agent_id,
            "account_id": account_id,
            "scopes": scopes,
            "expires_at": in_one_hour()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "grant mandate");
    let v: Value = resp.json().await.unwrap();
    Uuid::parse_str(v["mandate_id"].as_str().unwrap()).unwrap()
}

/// Exchange agent credentials + a mandate for an agent access token.
async fn agent_token_resp(
    c: &reqwest::Client,
    agent_id: Uuid,
    secret: &str,
    mandate_id: Uuid,
) -> reqwest::Response {
    c.post(format!("{}/api/v1/auth/agent-token", base_url()))
        .json(&json!({
            "agent_id": agent_id,
            "agent_secret": secret,
            "mandate_id": mandate_id
        }))
        .send()
        .await
        .unwrap()
}

async fn agent_token(
    c: &reqwest::Client,
    agent_id: Uuid,
    secret: &str,
    mandate_id: Uuid,
) -> String {
    let resp = agent_token_resp(c, agent_id, secret, mandate_id).await;
    assert!(resp.status().is_success(), "agent token: {}", resp.status());
    let v: Value = resp.json().await.unwrap();
    v["access_token"].as_str().unwrap().to_string()
}

async fn agent_get(c: &reqwest::Client, token: &str, path: &str) -> reqwest::Response {
    c.get(format!("{}{}", base_url(), path))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
}

/// Lazily connect to the test Postgres for audit assertions the HTTP surface
/// doesn't expose. `None` (with a SKIP note) if the DB is unreachable.
async fn test_db() -> Option<sqlx::PgPool> {
    let url = std::env::var("NANO_BANK_TEST_DB_URL").unwrap_or_else(|_| {
        "postgres://nanobank_user:secure_nano_password_2024!@[::1]:5432/nano_bank_db".to_string()
    });
    match sqlx::PgPool::connect(&url).await {
        Ok(pool) => Some(pool),
        Err(e) => {
            println!("SKIP: DB unreachable ({e})");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Registration + public metadata
// ---------------------------------------------------------------------------

#[tokio::test]
async fn register_and_inspect_agent() {
    let c = client();
    require_stack!(&c);

    let (agent_id, secret) = register_agent(&c).await;
    assert!(
        !secret.is_empty(),
        "secret is returned once at registration"
    );

    // Public metadata: anyone can inspect the agent before mandating it, but
    // the secret (or its hash) is never exposed.
    let resp = c
        .get(format!("{}/api/v1/agents/{}", base_url(), agent_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["display_name"], "Test Assistant");
    assert_eq!(v["kind"], "external");
    assert_eq!(v["status"], "active");
    assert!(v.get("agent_secret").is_none());
    assert!(v.get("secret_hash").is_none());

    // Unknown agent → 404.
    let resp = c
        .get(format!("{}/api/v1/agents/{}", base_url(), Uuid::new_v4()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

// ---------------------------------------------------------------------------
// Mandate lifecycle + grant-time validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mandate_grant_validation_and_listing() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let account = create_account(&c, &token, "chequing").await;
    let (agent_id, _secret) = register_agent(&c).await;

    // Happy path.
    let mandate = grant_mandate(&c, &token, agent_id, account, &["read:balance"]).await;

    // Listing shows it, active.
    let resp = c
        .get(format!("{}/api/v1/mandates", base_url()))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    let v: Value = resp.json().await.unwrap();
    let listed = v
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["mandate_id"] == mandate.to_string().as_str())
        .expect("granted mandate is listed");
    assert_eq!(listed["status"], "active");

    // Unknown scope → 400.
    let resp = c
        .post(format!("{}/api/v1/mandates", base_url()))
        .bearer_auth(&token)
        .json(&json!({
            "agent_id": agent_id, "account_id": account,
            "scopes": ["read:everything"], "expires_at": in_one_hour()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400, "unknown scope");

    // transfer:initiate without limits → 400 (money movement must be bounded).
    let resp = c
        .post(format!("{}/api/v1/mandates", base_url()))
        .bearer_auth(&token)
        .json(&json!({
            "agent_id": agent_id, "account_id": account,
            "scopes": ["transfer:initiate"], "expires_at": in_one_hour()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400, "unbounded transfer scope");

    // Past expiry → 400.
    let resp = c
        .post(format!("{}/api/v1/mandates", base_url()))
        .bearer_auth(&token)
        .json(&json!({
            "agent_id": agent_id, "account_id": account,
            "scopes": ["read:balance"],
            "expires_at": (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400, "past expiry");

    // Someone else's account → 404 (not 403): no existence leak.
    let (_other, other_token) = session(&c).await;
    let resp = c
        .post(format!("{}/api/v1/mandates", base_url()))
        .bearer_auth(&other_token)
        .json(&json!({
            "agent_id": agent_id, "account_id": account,
            "scopes": ["read:balance"], "expires_at": in_one_hour()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "cross-customer mandate");
}

// ---------------------------------------------------------------------------
// The happy path: token -> mandated balance read
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_reads_balance_under_mandate() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let account = create_account(&c, &token, "chequing").await;
    let (agent_id, secret) = register_agent(&c).await;
    let mandate = grant_mandate(&c, &token, agent_id, account, &["read:balance"]).await;

    let atoken = agent_token(&c, agent_id, &secret, mandate).await;
    let resp = agent_get(&c, &atoken, "/api/v1/agent/account").await;
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    // The mandate pins the account; a fresh account reads 0.00.
    assert_eq!(v["account_id"], account.to_string().as_str());
    assert_eq!(as_num(&v["balance"]), 0.0);
}

// ---------------------------------------------------------------------------
// Trust-plane matrix: wrong/missing/forged credentials
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_plane_matrix() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let account = create_account(&c, &token, "chequing").await;
    let (agent_id, secret) = register_agent(&c).await;
    let mandate = grant_mandate(&c, &token, agent_id, account, &["read:balance"]).await;

    // No token → 401.
    let resp = c
        .get(format!("{}/api/v1/agent/account", base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401, "no token");

    // A valid *customer* token on the agent plane → 403 (wrong plane).
    let resp = agent_get(&c, &token, "/api/v1/agent/account").await;
    assert_eq!(resp.status().as_u16(), 403, "customer token on agent plane");

    // A valid *agent* token on the customer plane → 401.
    let atoken = agent_token(&c, agent_id, &secret, mandate).await;
    let resp = agent_get(&c, &atoken, "/api/v1/accounts").await;
    assert_eq!(resp.status().as_u16(), 401, "agent token on customer plane");

    // Wrong secret → generic 401 (no enumeration).
    let resp = agent_token_resp(&c, agent_id, "not-the-secret", mandate).await;
    assert_eq!(resp.status().as_u16(), 401, "wrong secret");

    // A mandate belonging to a *different* agent → 401 (indistinguishable
    // from a missing mandate).
    let (other_agent, other_secret) = register_agent(&c).await;
    let resp = agent_token_resp(&c, other_agent, &other_secret, mandate).await;
    assert_eq!(resp.status().as_u16(), 401, "someone else's mandate");
}

// ---------------------------------------------------------------------------
// Scope enforcement + the audit-of-denials guarantee
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scope_denial_is_enforced_and_audited() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let account = create_account(&c, &token, "chequing").await;
    let (agent_id, secret) = register_agent(&c).await;
    // Only read:balance — history must be denied.
    let mandate = grant_mandate(&c, &token, agent_id, account, &["read:balance"]).await;
    let atoken = agent_token(&c, agent_id, &secret, mandate).await;

    // In-scope read succeeds…
    let resp = agent_get(&c, &atoken, "/api/v1/agent/account").await;
    assert_eq!(resp.status().as_u16(), 200);
    // …out-of-scope read is a 403 POLICY_DENIED.
    let resp = agent_get(&c, &atoken, "/api/v1/agent/transactions").await;
    assert_eq!(resp.status().as_u16(), 403);
    assert_eq!(error_code(resp).await, "POLICY_DENIED");

    // The audit trail must contain BOTH decisions (and the token issuance).
    let Some(db) = test_db().await else { return };
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT operation, decision, reason FROM agent_actions \
         WHERE mandate_id = $1 ORDER BY created_at",
    )
    .bind(mandate)
    .fetch_all(&db)
    .await
    .unwrap();
    assert!(
        rows.iter()
            .any(|(op, d, _)| op == "token:issue" && d == "allowed"),
        "token issuance audited: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|(op, d, _)| op == "read:balance" && d == "allowed"),
        "allowed read audited: {rows:?}"
    );
    assert!(
        rows.iter().any(|(op, d, r)| op == "read:transactions"
            && d == "denied"
            && r.as_deref() == Some("SCOPE_MISSING")),
        "denied read audited with reason: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// The revocation guarantee: a live, unexpired token dies with its mandate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revocation_kills_live_tokens() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let account = create_account(&c, &token, "chequing").await;
    let (agent_id, secret) = register_agent(&c).await;
    let mandate = grant_mandate(&c, &token, agent_id, account, &["read:balance"]).await;
    let atoken = agent_token(&c, agent_id, &secret, mandate).await;

    // Works now…
    let resp = agent_get(&c, &atoken, "/api/v1/agent/account").await;
    assert_eq!(resp.status().as_u16(), 200);

    // …the user revokes…
    let resp = c
        .delete(format!("{}/api/v1/mandates/{}", base_url(), mandate))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204, "revoke");

    // …and the SAME still-unexpired token is dead on the very next request.
    let resp = agent_get(&c, &atoken, "/api/v1/agent/account").await;
    assert_eq!(resp.status().as_u16(), 401, "revoked mandate");
    assert_eq!(error_code(resp).await, "MANDATE_INACTIVE");

    // Re-minting is refused too.
    let resp = agent_token_resp(&c, agent_id, &secret, mandate).await;
    assert_eq!(resp.status().as_u16(), 401, "re-mint after revoke");

    // A second revoke is a clean 409 (guarded flip), not a silent success.
    let resp = c
        .delete(format!("{}/api/v1/mandates/{}", base_url(), mandate))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409, "double revoke");

    // Someone else's revoke attempt on an unknown-to-them mandate → 404.
    let (_other, other_token) = session(&c).await;
    let resp = c
        .delete(format!("{}/api/v1/mandates/{}", base_url(), mandate))
        .bearer_auth(&other_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "cross-customer revoke");
}

// ---------------------------------------------------------------------------
// History is pinned to the mandate's account (needs the GL core to seed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn history_is_pinned_to_mandate_account() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;

    // Two accounts, only `a` mandated; fund both so each has history.
    let a = create_account(&c, &token, "chequing").await;
    let b = create_account(&c, &token, "chequing").await;
    for (acct, amount) in [(a, 100.0), (b, 200.0)] {
        let resp = c
            .post(format!("{}/api/v1/transactions/deposit", base_url()))
            .bearer_auth(&token)
            .json(&json!({ "account_id": acct, "amount": amount, "description": "seed" }))
            .send()
            .await
            .unwrap();
        if resp.status().as_u16() == 503 {
            eprintln!("SKIP: GL core unavailable (deposit returned 503)");
            return;
        }
        assert!(resp.status().is_success(), "deposit: {}", resp.status());
    }

    let (agent_id, secret) = register_agent(&c).await;
    let mandate = grant_mandate(&c, &token, agent_id, a, &["read:transactions"]).await;
    let atoken = agent_token(&c, agent_id, &secret, mandate).await;

    // The agent sees a's deposit — and even an explicit attempt to query the
    // OTHER account is ignored: the mandate pins the account.
    for path in [
        "/api/v1/agent/transactions".to_string(),
        format!("/api/v1/agent/transactions?account_id={b}"),
    ] {
        let resp = agent_get(&c, &atoken, &path).await;
        assert_eq!(resp.status().as_u16(), 200, "{path}");
        let v: Value = resp.json().await.unwrap();
        let txns = v["transactions"].as_array().unwrap();
        assert!(!txns.is_empty(), "mandated account has history");
        for t in txns {
            let entries = t["entries"].as_array().unwrap();
            assert!(
                entries
                    .iter()
                    .any(|e| e["account_id"] == a.to_string().as_str()),
                "every transaction touches the mandated account: {t}"
            );
            assert!(
                entries
                    .iter()
                    .all(|e| e["account_id"] != b.to_string().as_str()),
                "the other account never leaks: {t}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 2: bounded transfers
// ---------------------------------------------------------------------------

/// Grant a transfer-capable mandate (max_per_tx / daily_cap / optional payees).
async fn grant_transfer_mandate(
    c: &reqwest::Client,
    token: &str,
    agent_id: Uuid,
    account_id: Uuid,
    max_per_tx: f64,
    daily_cap: f64,
    allowed_payees: Option<Vec<Uuid>>,
) -> Uuid {
    let mut body = json!({
        "agent_id": agent_id,
        "account_id": account_id,
        "scopes": ["read:balance", "read:transactions", "transfer:initiate"],
        "max_per_tx": max_per_tx,
        "daily_cap": daily_cap,
        "expires_at": in_one_hour()
    });
    if let Some(payees) = allowed_payees {
        body["allowed_payees"] = json!(payees);
    }
    let resp = c
        .post(format!("{}/api/v1/mandates", base_url()))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "grant transfer mandate");
    let v: Value = resp.json().await.unwrap();
    Uuid::parse_str(v["mandate_id"].as_str().unwrap()).unwrap()
}

/// Fund a fresh chequing account via deposit; None (skip) if the core is down.
async fn funded_account(c: &reqwest::Client, token: &str, amount: f64) -> Option<Uuid> {
    let account = create_account(c, token, "chequing").await;
    let resp = c
        .post(format!("{}/api/v1/transactions/deposit", base_url()))
        .bearer_auth(token)
        .json(&json!({ "account_id": account, "amount": amount, "description": "seed funds" }))
        .send()
        .await
        .unwrap();
    if resp.status().as_u16() == 503 {
        eprintln!("SKIP: GL core unavailable (deposit returned 503)");
        return None;
    }
    assert!(resp.status().is_success(), "deposit: {}", resp.status());
    Some(account)
}

async fn agent_transfer(
    c: &reqwest::Client,
    atoken: &str,
    to: Uuid,
    amount: f64,
    key: &str,
) -> reqwest::Response {
    c.post(format!("{}/api/v1/agent/transfers", base_url()))
        .bearer_auth(atoken)
        .json(&json!({
            "to_account_id": to,
            "amount": amount,
            "description": "agent payment",
            "idempotency_key": key
        }))
        .send()
        .await
        .unwrap()
}

/// The mandate's daily_used as seen by its owner via GET /mandates.
async fn mandate_daily_used(c: &reqwest::Client, token: &str, mandate_id: Uuid) -> f64 {
    let v: Value = c
        .get(format!("{}/api/v1/mandates", base_url()))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let m = v
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["mandate_id"] == mandate_id.to_string().as_str())
        .expect("mandate listed");
    as_num(&m["daily_used"])
}

#[tokio::test]
async fn agent_transfer_happy_path_and_replay() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let Some(a) = funded_account(&c, &token, 1000.0).await else {
        return;
    };
    let b = create_account(&c, &token, "savings").await;
    let (agent_id, secret) = register_agent(&c).await;
    let mandate = grant_transfer_mandate(&c, &token, agent_id, a, 200.0, 500.0, None).await;
    let atoken = agent_token(&c, agent_id, &secret, mandate).await;

    let key = format!("agent-pay-{}", Uuid::new_v4());
    let resp = agent_transfer(&c, &atoken, b, 150.0, &key).await;
    assert_eq!(resp.status().as_u16(), 201, "agent transfer");
    let v: Value = resp.json().await.unwrap();
    let txn_id = v["transaction_id"].as_str().unwrap().to_string();

    // Balances: funding down amount + $1.50 fee; payee up amount.
    assert_eq!(balance(&c, &token, a).await, 848.5);
    assert_eq!(balance(&c, &token, b).await, 150.0);
    // The cap metered the amount only (not the fee).
    assert_eq!(mandate_daily_used(&c, &token, mandate).await, 150.0);

    // Replay the SAME key: 200, same transaction, no new spend or reservation.
    let resp = agent_transfer(&c, &atoken, b, 150.0, &key).await;
    assert_eq!(resp.status().as_u16(), 200, "idempotent replay");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["transaction_id"].as_str().unwrap(), txn_id);
    assert_eq!(balance(&c, &token, a).await, 848.5);
    assert_eq!(mandate_daily_used(&c, &token, mandate).await, 150.0);

    // Agency is on the money trail (metadata isn't exposed over HTTP).
    let Some(db) = test_db().await else { return };
    let (meta_agent, meta_mandate): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT metadata->>'agent_id', metadata->>'mandate_id' \
         FROM transactions WHERE transaction_id = $1::uuid",
    )
    .bind(&txn_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(meta_agent.as_deref(), Some(agent_id.to_string().as_str()));
    assert_eq!(meta_mandate.as_deref(), Some(mandate.to_string().as_str()));
}

#[tokio::test]
async fn transfer_caps_are_step_up_denials() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let Some(a) = funded_account(&c, &token, 1000.0).await else {
        return;
    };
    let b = create_account(&c, &token, "savings").await;
    let (agent_id, secret) = register_agent(&c).await;
    let mandate = grant_transfer_mandate(&c, &token, agent_id, a, 200.0, 500.0, None).await;
    let atoken = agent_token(&c, agent_id, &secret, mandate).await;

    // Over max_per_tx → 403 POLICY_DENIED, nothing moved.
    let resp = agent_transfer(&c, &atoken, b, 250.0, &Uuid::new_v4().to_string()).await;
    assert_eq!(resp.status().as_u16(), 403, "over max_per_tx");
    assert_eq!(error_code(resp).await, "POLICY_DENIED");
    assert_eq!(balance(&c, &token, b).await, 0.0);
    assert_eq!(mandate_daily_used(&c, &token, mandate).await, 0.0);

    // Two $180s fit the $500 cap; the third breaches it.
    for _ in 0..2 {
        let resp = agent_transfer(&c, &atoken, b, 180.0, &Uuid::new_v4().to_string()).await;
        assert_eq!(resp.status().as_u16(), 201);
    }
    let resp = agent_transfer(&c, &atoken, b, 180.0, &Uuid::new_v4().to_string()).await;
    assert_eq!(resp.status().as_u16(), 403, "daily cap breached");
    assert_eq!(mandate_daily_used(&c, &token, mandate).await, 360.0);

    // The audit distinguishes step-up candidates from hard denials.
    let Some(db) = test_db().await else { return };
    let rows: Vec<(String, Option<String>, Option<rust_decimal::Decimal>)> = sqlx::query_as(
        "SELECT decision, reason, amount FROM agent_actions \
         WHERE mandate_id = $1 AND operation = 'transfer' ORDER BY created_at",
    )
    .bind(mandate)
    .fetch_all(&db)
    .await
    .unwrap();
    assert!(
        rows.iter().any(|(d, r, amt)| d == "step_up_required"
            && r.as_deref() == Some("MAX_PER_TX_EXCEEDED")
            && amt.map(|a| a.to_string()) == Some("250.00".into())),
        "max_per_tx audit: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|(d, r, _)| d == "step_up_required" && r.as_deref() == Some("DAILY_CAP_EXCEEDED")),
        "daily cap audit: {rows:?}"
    );
    assert_eq!(
        rows.iter().filter(|(d, _, _)| d == "allowed").count(),
        2,
        "two allowed transfers: {rows:?}"
    );
}

#[tokio::test]
async fn concurrent_transfers_cannot_beat_the_cap() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let Some(a) = funded_account(&c, &token, 1000.0).await else {
        return;
    };
    let b = create_account(&c, &token, "savings").await;
    let (agent_id, secret) = register_agent(&c).await;
    // Cap $500; two concurrent $300s must not both pass.
    let mandate = grant_transfer_mandate(&c, &token, agent_id, a, 400.0, 500.0, None).await;
    let atoken = agent_token(&c, agent_id, &secret, mandate).await;

    let (k1, k2) = (Uuid::new_v4().to_string(), Uuid::new_v4().to_string());
    let (r1, r2) = tokio::join!(
        agent_transfer(&c, &atoken, b, 300.0, &k1),
        agent_transfer(&c, &atoken, b, 300.0, &k2),
    );
    let mut codes = [r1.status().as_u16(), r2.status().as_u16()];
    codes.sort_unstable();
    assert_eq!(codes, [201, 403], "exactly one wins the cap race");
    assert_eq!(mandate_daily_used(&c, &token, mandate).await, 300.0);
    assert_eq!(balance(&c, &token, b).await, 300.0);
}

#[tokio::test]
async fn payee_allowlist_pins_destinations() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let a = create_account(&c, &token, "chequing").await; // no funding needed: denied pre-locks
    let b = create_account(&c, &token, "savings").await;
    let stranger = create_account(&c, &token, "savings").await;
    let (agent_id, secret) = register_agent(&c).await;
    let mandate =
        grant_transfer_mandate(&c, &token, agent_id, a, 200.0, 500.0, Some(vec![b])).await;
    let atoken = agent_token(&c, agent_id, &secret, mandate).await;

    let resp = agent_transfer(&c, &atoken, stranger, 50.0, &Uuid::new_v4().to_string()).await;
    assert_eq!(resp.status().as_u16(), 403, "payee not on the allowlist");
    assert_eq!(error_code(resp).await, "POLICY_DENIED");
}

#[tokio::test]
async fn transfer_guards() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let a = create_account(&c, &token, "chequing").await;
    let b = create_account(&c, &token, "savings").await;
    let (agent_id, secret) = register_agent(&c).await;

    // A read-only mandate cannot transfer (hard deny, not step-up).
    let read_only = grant_mandate(&c, &token, agent_id, a, &["read:balance"]).await;
    let atoken = agent_token(&c, agent_id, &secret, read_only).await;
    let resp = agent_transfer(&c, &atoken, b, 10.0, &Uuid::new_v4().to_string()).await;
    assert_eq!(resp.status().as_u16(), 403, "scope missing");
    assert_eq!(error_code(resp).await, "POLICY_DENIED");

    // An empty idempotency key is rejected before anything happens.
    let resp = c
        .post(format!("{}/api/v1/agent/transfers", base_url()))
        .bearer_auth(&atoken)
        .json(&json!({
            "to_account_id": b, "amount": 10.0,
            "description": "x", "idempotency_key": ""
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400, "empty idempotency key");
    // A missing key never reaches the handler (axum body rejection).
    let resp = c
        .post(format!("{}/api/v1/agent/transfers", base_url()))
        .bearer_auth(&atoken)
        .json(&json!({ "to_account_id": b, "amount": 10.0, "description": "x" }))
        .send()
        .await
        .unwrap();
    assert!(
        [400, 422].contains(&resp.status().as_u16()),
        "missing idempotency key: {}",
        resp.status()
    );

    // Revoked transfer mandate → the token dies (401), not a policy 403.
    let mandate = grant_transfer_mandate(&c, &token, agent_id, a, 200.0, 500.0, None).await;
    let atoken = agent_token(&c, agent_id, &secret, mandate).await;
    let resp = c
        .delete(format!("{}/api/v1/mandates/{}", base_url(), mandate))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    let resp = agent_transfer(&c, &atoken, b, 10.0, &Uuid::new_v4().to_string()).await;
    assert_eq!(resp.status().as_u16(), 401, "revoked mandate");
    assert_eq!(error_code(resp).await, "MANDATE_INACTIVE");
}

// ---------------------------------------------------------------------------
// The owner's view of the audit trail (GET /mandates/{id}/actions)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mandate_activity_is_visible_to_its_owner() {
    let c = client();
    require_stack!(&c);
    let (_customer, token) = session(&c).await;
    let account = create_account(&c, &token, "chequing").await;
    let (agent_id, secret) = register_agent(&c).await;
    let mandate = grant_mandate(&c, &token, agent_id, account, &["read:balance"]).await;
    let atoken = agent_token(&c, agent_id, &secret, mandate).await;

    // One allowed read, one denied (out-of-scope) read.
    assert_eq!(
        agent_get(&c, &atoken, "/api/v1/agent/account")
            .await
            .status()
            .as_u16(),
        200
    );
    assert_eq!(
        agent_get(&c, &atoken, "/api/v1/agent/transactions")
            .await
            .status()
            .as_u16(),
        403
    );

    // The owner sees BOTH decisions over HTTP (newest first).
    let resp = c
        .get(format!(
            "{}/api/v1/mandates/{}/actions",
            base_url(),
            mandate
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let actions: Value = resp.json().await.unwrap();
    let actions = actions.as_array().unwrap();
    assert!(
        actions
            .iter()
            .any(|a| a["operation"] == "token:issue" && a["decision"] == "allowed"),
        "token issuance visible: {actions:?}"
    );
    assert!(
        actions
            .iter()
            .any(|a| a["operation"] == "read:balance" && a["decision"] == "allowed"),
        "allowed read visible: {actions:?}"
    );
    assert!(
        actions.iter().any(|a| a["operation"] == "read:transactions"
            && a["decision"] == "denied"
            && a["reason"] == "SCOPE_MISSING"),
        "denied read visible with reason: {actions:?}"
    );

    // Another customer gets 404 — the mandate's existence isn't leaked.
    let (_other, other_token) = session(&c).await;
    let resp = c
        .get(format!(
            "{}/api/v1/mandates/{}/actions",
            base_url(),
            mandate
        ))
        .bearer_auth(&other_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "cross-customer activity");
}
