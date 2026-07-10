//! Integration tests for the Lending Subsystem.
//!
//! Reuses the same offline-skip harness and local Kind DB / API infrastructure
//! established by `api/tests/transactions.rs`.

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

/// Lazily connect to the test Postgres.
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

fn as_num(v: &Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("not a number: {v:?}"))
}

async fn create_customer(c: &reqwest::Client) -> (Uuid, String) {
    let n = Uuid::new_v4().as_u128();
    let email = format!("loan_test_{}@example.com", n % 1_000_000_000);
    let body = json!({
        "email": email,
        "phone_number": format!("{:010}", (n % 10_000_000_000u128)),
        "first_name": "Loan",
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
    v["access_token"]
        .as_str()
        .expect("login response has an access_token")
        .to_string()
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

async fn post_json(c: &reqwest::Client, token: &str, path: &str, body: Value) -> reqwest::Response {
    c.post(format!("{}{}", base_url(), path))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Comprehensive Loan Lifecycle Integration Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_loan_lifecycle() {
    let c = client();
    require_stack!(&c);

    let Some(pool) = test_db().await else {
        println!("SKIP: direct DB connection unavailable");
        return;
    };

    // 1. Create a customer & session
    let (customer_id, email) = create_customer(&c).await;
    let token = login(&c, &email).await;

    // 2. Applying for a loan fails initially because KYC is not verified
    let resp = post_json(
        &c,
        &token,
        "/api/v1/loans",
        json!({
            "principal_amount": 10000.00,
            "interest_rate": 0.085,
            "amortization_months": 24
        }),
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let err_val: Value = resp.json().await.unwrap();
    assert!(
        err_val["error"].as_str().unwrap().contains("KYC"),
        "error should indicate KYC requirement"
    );

    // 3. Promote customer KYC status to verified via direct DB update
    sqlx::query("UPDATE customers SET kyc_status = 'verified' WHERE customer_id = $1")
        .bind(customer_id)
        .execute(&pool)
        .await
        .unwrap();

    // 4. Create an active chequing account to receive disbursement funds
    let chequing_id = create_account(&c, &token, "chequing").await;

    // 5. Applying for a loan succeeds now that KYC is verified
    let resp = post_json(
        &c,
        &token,
        "/api/v1/loans",
        json!({
            "principal_amount": 10000.00,
            "interest_rate": 0.085,
            "amortization_months": 24
        }),
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let loan_res: Value = resp.json().await.unwrap();
    let loan_id = Uuid::parse_str(loan_res["loan_id"].as_str().unwrap()).unwrap();
    let loan_account_id = Uuid::parse_str(loan_res["account_id"].as_str().unwrap()).unwrap();
    assert_eq!(loan_res["status"].as_str().unwrap(), "pending_disbursement");
    assert_eq!(as_num(&loan_res["monthly_payment"]), 454.56);

    // 6. Verify GET /loans list lookup
    let list_resp: Value = c
        .get(format!("{}/api/v1/loans", base_url()))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list_resp.as_array().unwrap().len(), 1);
    assert_eq!(Uuid::parse_str(list_resp[0]["loan_id"].as_str().unwrap()).unwrap(), loan_id);

    // 7. Verify GET /loans/{id} detailed lookup
    let detail_resp: Value = c
        .get(format!("{}/api/v1/loans/{}", base_url(), loan_id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail_resp["status"].as_str().unwrap(), "pending_disbursement");

    // 8. Disburse the loan
    let disburse_resp = c
        .post(format!("{}/api/v1/loans/{}/disburse", base_url(), loan_id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(disburse_resp.status(), reqwest::StatusCode::OK);
    let disburse_res: Value = disburse_resp.json().await.unwrap();
    assert_eq!(disburse_res["status"].as_str().unwrap(), "active");

    // 9. Verify balances: loan account is -10k (debt), chequing is +10k
    let chequing_bal: Value = c
        .get(format!("{}/api/v1/accounts/{}/balance", base_url(), chequing_id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(as_num(&chequing_bal["balance"]), 10000.00);

    let loan_bal: Value = c
        .get(format!("{}/api/v1/accounts/{}/balance", base_url(), loan_account_id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(as_num(&loan_bal["balance"]), -10000.00);

    // 10. Trigger admin daily interest accrual
    let accrue_resp = c
        .post(format!("{}/api/v1/loans/admin/accrue", base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(accrue_resp.status(), reqwest::StatusCode::OK);

    // Interest calculation: $10,000 * 8.5% / 365 = 2.3287 -> rounds to 2.33.
    // Negative debt balance increases (becomes more negative) to -10,002.33.
    let loan_bal_after: Value = c
        .get(format!("{}/api/v1/accounts/{}/balance", base_url(), loan_account_id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(as_num(&loan_bal_after["balance"]), -10002.33);

    // 11. Repay $1000 portion from chequing to loan account
    let repay_resp = post_json(
        &c,
        &token,
        &format!("/api/v1/loans/{}/repay", loan_id),
        json!({
            "funding_account_id": chequing_id,
            "amount": 1000.00
        }),
    )
    .await;
    assert_eq!(repay_resp.status(), reqwest::StatusCode::OK);
    let repay_res: Value = repay_resp.json().await.unwrap();
    assert_eq!(repay_res["status"].as_str().unwrap(), "active");

    // Verify balances after repayment: chequing = 9000, loan = -9002.33
    let chequing_bal_after_repay: Value = c
        .get(format!("{}/api/v1/accounts/{}/balance", base_url(), chequing_id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(as_num(&chequing_bal_after_repay["balance"]), 9000.00);

    let loan_bal_after_repay: Value = c
        .get(format!("{}/api/v1/accounts/{}/balance", base_url(), loan_account_id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(as_num(&loan_bal_after_repay["balance"]), -9002.33);

    // 12. Repay the entire remaining balance ($9002.33)
    let final_repay_resp = post_json(
        &c,
        &token,
        &format!("/api/v1/loans/{}/repay", loan_id),
        json!({
            "funding_account_id": chequing_id,
            "amount": 9002.33
        }),
    )
    .await;
    assert_eq!(final_repay_resp.status(), reqwest::StatusCode::OK);
    let final_repay_res: Value = final_repay_resp.json().await.unwrap();
    assert_eq!(final_repay_res["status"].as_str().unwrap(), "closed");

    // Verify that loan is closed and loan account has balance 0
    let loan_bal_closed: Value = c
        .get(format!("{}/api/v1/accounts/{}/balance", base_url(), loan_account_id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(as_num(&loan_bal_closed["balance"]), 0.00);
}
