use axum::{
    body::{Body, to_bytes},
    http::{Request, Method, StatusCode, header},
};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

use nano_bank_api::{
    config::Settings,
    config::database::create_connection_pool,
    create_router,
};

// Helper function to extract JSON from response
async fn extract_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn test_ledger_transactions() {
    // 1. Setup
    let settings = Settings::new().unwrap_or_default();
    let pool = create_connection_pool(&settings).await.expect("Failed to connect to test db");
    let system_accounts = nano_bank_api::handlers::cards::ensure_system_accounts(&pool).await.expect("Failed to bootstrap system accounts");
    let app = create_router(pool, &settings, system_accounts).await;

    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
    
    // 2. Create Alice
    let alice_payload = json!({
        "email": format!("alice_{}@example.com", timestamp),
        "phone_number": format!("+14165{}", timestamp % 1000000),
        "first_name": "Alice",
        "last_name": "Smith",
        "date_of_birth": "1990-01-01",
        "password": "Password123!",
        "sin": "123456789"
    });

    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/customers")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(alice_payload.to_string()))
        .unwrap();

    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    
    let alice = extract_json(res).await;
    let alice_id = alice["customer_id"].as_str().unwrap().to_string();

    // 3. Create Alice's Account
    let account_payload = json!({
        "customer_id": alice_id,
        "account_type": "chequing"
    });

    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/accounts")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(account_payload.to_string()))
        .unwrap();

    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    
    let alice_account = extract_json(res).await;
    let alice_account_id = alice_account["account_id"].as_str().unwrap().to_string();

    // 4. Create Bob
    let bob_payload = json!({
        "email": format!("bob_{}@example.com", timestamp),
        "phone_number": format!("+15165{}", timestamp % 1000000),
        "first_name": "Bob",
        "last_name": "Jones",
        "date_of_birth": "1992-05-05",
        "password": "Password123!"
    });

    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/customers")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bob_payload.to_string()))
        .unwrap();

    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    
    let bob = extract_json(res).await;
    let bob_id = bob["customer_id"].as_str().unwrap().to_string();

    // 5. Create Bob's Account
    let account_payload = json!({
        "customer_id": bob_id,
        "account_type": "chequing"
    });

    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/accounts")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(account_payload.to_string()))
        .unwrap();

    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    
    let bob_account = extract_json(res).await;
    let bob_account_id = bob_account["account_id"].as_str().unwrap().to_string();

    // 6. Deposit to Alice
    let deposit_payload = json!({
        "account_id": alice_account_id,
        "amount": "1000.00",
        "description": "Paycheck deposit"
    });

    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/transactions/deposit")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(deposit_payload.to_string()))
        .unwrap();

    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    // 7. Transfer Alice -> Bob
    let transfer_payload = json!({
        "from_account_id": alice_account_id,
        "to_account_id": bob_account_id,
        "amount": "250.50",
        "description": "Dinner payback"
    });

    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/transactions/transfer")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(transfer_payload.to_string()))
        .unwrap();

    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    // 8. Withdraw from Bob
    let withdraw_payload = json!({
        "account_id": bob_account_id,
        "amount": "40.00",
        "description": "ATM Withdrawal"
    });

    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/transactions/withdrawal")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(withdraw_payload.to_string()))
        .unwrap();

    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    // 9. Get Alice's Transactions
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/api/v1/transactions?account_id={}&limit=10", alice_account_id))
        .body(Body::empty())
        .unwrap();

    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    
    let history = extract_json(res).await;
    let transactions = history["transactions"].as_array().unwrap();
    assert_eq!(transactions.len(), 2); // Deposit + Transfer
    
    // Most recent is the transfer
    assert_eq!(transactions[0]["transaction_type"], "transfer");
    assert_eq!(transactions[0]["amount"], "250.50");
    
    // Older is the deposit
    assert_eq!(transactions[1]["transaction_type"], "deposit");
    assert_eq!(transactions[1]["amount"], "1000.00");
}