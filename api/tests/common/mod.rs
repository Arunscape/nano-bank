use serde_json::Value;
use uuid::Uuid;

pub const TEST_PASSWORD: &str = "securepass123";

pub fn base_url() -> String {
    std::env::var("NANO_BANK_TEST_URL").unwrap_or_else(|_| "http://localhost:8081".to_string())
}

pub fn client() -> reqwest::Client {
    reqwest::Client::new()
}

pub fn rnd() -> u128 {
    Uuid::new_v4().as_u128()
}

pub async fn stack_up(c: &reqwest::Client) -> bool {
    matches!(
        c.get(format!("{}/health", base_url())).send().await,
        Ok(r) if r.status().is_success()
    )
}

pub fn as_num(v: &Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("not a number: {v:?}"))
}

pub async fn test_db() -> Option<sqlx::PgPool> {
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

pub async fn create_customer(c: &reqwest::Client) -> (Uuid, String) {
    let n = Uuid::new_v4().as_u128();
    let email = format!("test_{}@example.com", n % 1_000_000_000);
    let body = serde_json::json!({
        "email": email,
        "phone_number": format!("{:010}", (n % 10_000_000_000u128)),
        "first_name": "Test",
        "last_name": "User",
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

pub async fn login(c: &reqwest::Client, email: &str) -> String {
    let resp = c
        .post(format!("{}/api/v1/auth/login", base_url()))
        .json(&serde_json::json!({ "email": email, "password": TEST_PASSWORD }))
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

pub async fn session(c: &reqwest::Client) -> (Uuid, String) {
    let (id, email) = create_customer(c).await;
    let token = login(c, &email).await;
    (id, token)
}

pub async fn create_account(c: &reqwest::Client, token: &str, account_type: &str) -> Uuid {
    let resp = c
        .post(format!("{}/api/v1/accounts", base_url()))
        .bearer_auth(token)
        .json(&serde_json::json!({ "account_type": account_type }))
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

pub async fn balance(c: &reqwest::Client, token: &str, account_id: Uuid) -> f64 {
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

pub async fn post_json(c: &reqwest::Client, token: &str, path: &str, body: Value) -> reqwest::Response {
    c.post(format!("{}{}", base_url(), path))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

pub async fn history(c: &reqwest::Client, token: &str, account_id: Option<Uuid>) -> Value {
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

#[macro_export]
macro_rules! require_stack {
    ($c:expr) => {
        if !common::stack_up($c).await {
            eprintln!("SKIP: nano-bank not reachable at {}", common::base_url());
            return;
        }
    };
}
