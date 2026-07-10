//! Integration tests for the Lending Subsystem.
//!
//! Reuses the same offline-skip harness and local Kind DB / API infrastructure
//! established by `api/tests/transactions.rs`.

mod common;

use common::*;
use serde_json::{json, Value};
use uuid::Uuid;

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
