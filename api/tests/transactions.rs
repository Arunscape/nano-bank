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

use rust_decimal::Decimal;
use serde_json::{json, Value};
use uuid::Uuid;

mod common;

use common::*;

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
    // A transfer also charges a flat $1.50 fee (posted as a separate `fee` txn),
    // so the funding account loses amount + fee: 1000 - 400 - 1.50.
    assert_eq!(balance(&c, &token, a).await, 598.5);
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
    assert_eq!(balance(&c, &token, a).await, 498.5);

    // history for account a: deposit + transfer + fee + withdrawal = 4
    let v = history(&c, &token, Some(a)).await;
    assert_eq!(v["total_count"].as_u64().unwrap(), 4, "history: {v}");
    assert_eq!(v["transactions"].as_array().unwrap().len(), 4);
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
    // Only one transfer actually moved money (and the fee was charged once):
    // 500 - 200 - 1.50. The replay returns early, so it never double-charges.
    assert_eq!(balance(&c, &token, a).await, 298.5);
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

// ---------------------------------------------------------------------------
// Helpers for the single-fetch / reversal / fee tests
// ---------------------------------------------------------------------------

/// GET one transaction by id with the caller's token; returns `(status, body)`.
async fn get_txn(c: &reqwest::Client, token: &str, id: Uuid) -> (u16, Value) {
    let resp = c
        .get(format!("{}/api/v1/transactions/{}", base_url(), id))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// POST a reversal for a transaction id with the caller's token.
async fn reverse(c: &reqwest::Client, token: &str, id: Uuid) -> reqwest::Response {
    c.post(format!("{}/api/v1/transactions/{}/reverse", base_url(), id))
        .bearer_auth(token)
        .json(&json!({ "reason": "test" }))
        .send()
        .await
        .unwrap()
}

/// Extract `transaction_id` from a success response body.
async fn txn_id(resp: reqwest::Response) -> Uuid {
    let v: Value = resp.json().await.unwrap();
    Uuid::parse_str(v["transaction_id"].as_str().unwrap()).unwrap()
}

/// Deposit `amount` into a fresh chequing account; `(account, deposit_txn)` or
/// `None` if the GL core is down.
async fn seed_deposit(c: &reqwest::Client, token: &str, amount: f64) -> Option<(Uuid, Uuid)> {
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
    Some((account, txn_id(resp).await))
}

/// The Revenue GL balance as the configured core reports it, black-box via the
/// API's own (unauthenticated) `GET /api/v1/ledger/balances`. `None` means the
/// endpoint failed or no revenue account exists yet — the caller skips its GL
/// assertion in that case. The modern core names it `REVENUE`, the legacy core
/// `0000800000`; match either.
async fn revenue_gl_balance(c: &reqwest::Client) -> Option<f64> {
    let resp = c
        .get(format!("{}/api/v1/ledger/balances", base_url()))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let rows: Value = resp.json().await.ok()?;
    rows.as_array()?.iter().find_map(|r| {
        let name = r["account"].as_str()?;
        if name == "REVENUE" || name == "0000800000" {
            Some(as_num(&r["balance"]))
        } else {
            None
        }
    })
}

/// Lazily connect to the test Postgres so the fee test can assert rows the HTTP
/// surface doesn't expose. `None` (with a SKIP note) if the DB is unreachable.
/// Creds mirror `api/config/default.toml`; note the IPv6 `[::1]` host.
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
// Single fetch: scoped to the caller
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_fetch_scopes_to_the_caller() {
    let c = client();
    require_stack!(&c);
    let (_cust, token) = session(&c).await;
    let Some((_a, dep)) = seed_deposit(&c, &token, 100.0).await else {
        return;
    };

    // The owner can fetch it (hydrated with both legs).
    let (status, body) = get_txn(&c, &token, dep).await;
    assert_eq!(status, 200);
    assert_eq!(body["transaction_type"], "deposit");
    assert_eq!(body["entries"].as_array().unwrap().len(), 2);

    // A different customer cannot — 404, so existence isn't leaked.
    let (_other, other_token) = session(&c).await;
    let (status, _) = get_txn(&c, &other_token, dep).await;
    assert_eq!(status, 404, "cross-customer fetch must 404");

    // Unknown id → 404.
    let (status, _) = get_txn(&c, &token, Uuid::new_v4()).await;
    assert_eq!(status, 404);
}

// ---------------------------------------------------------------------------
// Transfer fee
// ---------------------------------------------------------------------------

#[tokio::test]
async fn transfer_charges_a_flat_fee() {
    let c = client();
    require_stack!(&c);
    let (_cust, token) = session(&c).await;
    let a = match funded_account(&c, &token, 1000.0).await {
        Some(a) => a,
        None => return,
    };
    let b = create_account(&c, &token, "savings").await;

    // Sample the Revenue GL before the transfer so we can assert the fee posted.
    let revenue_before = revenue_gl_balance(&c).await;

    let resp = post_json(
        &c,
        &token,
        "/api/v1/transactions/transfer",
        json!({ "from_account_id": a, "to_account_id": b, "amount": 400.0, "description": "rent" }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 201);
    let transfer_id = txn_id(resp).await;
    // Funding account loses amount + $1.50 fee; recipient gets the full amount.
    assert_eq!(balance(&c, &token, a).await, 598.5);
    assert_eq!(balance(&c, &token, b).await, 400.0);

    // The fee is a separate `fee` transaction in the funding account's history.
    let v = history(&c, &token, Some(a)).await;
    let types: Vec<&str> = v["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["transaction_type"].as_str().unwrap())
        .collect();
    assert!(
        types.contains(&"fee"),
        "history should include the fee txn: {types:?}"
    );

    // GL effect: the fee is recognised as Revenue at the core. Assert the
    // *magnitude* of the move (>= the $1.50 fee), which is agnostic to the core's
    // sign convention — the modern core reports Revenue credit-normal (a fee
    // makes it more negative), the legacy core may report it the other way.
    // Revenue is only ever credited (by fees, never debited/reversed), so it
    // moves one direction only and parallel fees can't cancel the delta.
    match (revenue_before, revenue_gl_balance(&c).await) {
        (before, Some(after)) => {
            let before = before.unwrap_or(0.0);
            assert!(
                (after - before).abs() >= 1.50 - 1e-6,
                "Revenue GL should move by at least the $1.50 fee: {before} -> {after}"
            );
        }
        _ => println!("SKIP: /ledger/balances unavailable or no revenue account yet"),
    }

    // DB effect: the `transaction_fees` row (linked to the transfer) and the
    // `gl_entry` recorded on the *fee* txn — neither is on the HTTP surface, so a
    // black-box test alone would still pass if either post were deleted.
    if let Some(pool) = test_db().await {
        let fee_rows: Vec<(String, Decimal)> = sqlx::query_as(
            "SELECT fee_type, fee_amount FROM transaction_fees WHERE transaction_id = $1",
        )
        .bind(transfer_id)
        .fetch_all(&pool)
        .await
        .expect("query transaction_fees");
        assert_eq!(fee_rows.len(), 1, "exactly one fee row for the transfer");
        assert_eq!(fee_rows[0].0, "transfer");
        assert_eq!(fee_rows[0].1, Decimal::new(150, 2));

        // The fee's own txn carries the GL document id in metadata.gl_entry.
        let fee_txn_id = v["transactions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["transaction_type"] == "fee")
            .and_then(|t| t["transaction_id"].as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            .expect("history exposes the fee txn id");
        let gl_entry: Option<String> = sqlx::query_scalar(
            "SELECT metadata->>'gl_entry' FROM transactions WHERE transaction_id = $1",
        )
        .bind(fee_txn_id)
        .fetch_one(&pool)
        .await
        .expect("query fee txn metadata");
        assert!(
            gl_entry.is_some_and(|s| !s.is_empty()),
            "the fee txn should record a non-empty gl_entry"
        );
    }
}

// ---------------------------------------------------------------------------
// Reversals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reverse_deposit_claws_back_and_rejects_re_reverse() {
    let c = client();
    require_stack!(&c);
    let (_cust, token) = session(&c).await;
    let Some((a, dep)) = seed_deposit(&c, &token, 500.0).await else {
        return;
    };
    assert_eq!(balance(&c, &token, a).await, 500.0);

    let resp = reverse(&c, &token, dep).await;
    assert_eq!(resp.status().as_u16(), 201);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["transaction_type"], "reversal");
    assert_eq!(balance(&c, &token, a).await, 0.0);

    // Reversing an already-reversed txn → 400 (it's no longer `completed`).
    let resp = reverse(&c, &token, dep).await;
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn reverse_withdrawal_refunds() {
    let c = client();
    require_stack!(&c);
    let (_cust, token) = session(&c).await;
    let a = match funded_account(&c, &token, 500.0).await {
        Some(a) => a,
        None => return,
    };
    let resp = post_json(
        &c,
        &token,
        "/api/v1/transactions/withdrawal",
        json!({ "account_id": a, "amount": 100.0, "description": "atm" }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 201);
    let wid = txn_id(resp).await;
    assert_eq!(balance(&c, &token, a).await, 400.0);

    let resp = reverse(&c, &token, wid).await;
    assert_eq!(resp.status().as_u16(), 201);
    assert_eq!(balance(&c, &token, a).await, 500.0);
}

#[tokio::test]
async fn reverse_transfer_claws_back_but_keeps_fee() {
    let c = client();
    require_stack!(&c);
    let (_cust, token) = session(&c).await;
    let a = match funded_account(&c, &token, 500.0).await {
        Some(a) => a,
        None => return,
    };
    let b = create_account(&c, &token, "savings").await;
    let resp = post_json(
        &c,
        &token,
        "/api/v1/transactions/transfer",
        json!({ "from_account_id": a, "to_account_id": b, "amount": 200.0, "description": "mv" }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 201);
    let tid = txn_id(resp).await;
    assert_eq!(balance(&c, &token, a).await, 298.5); // 500 - 200 - 1.50
    assert_eq!(balance(&c, &token, b).await, 200.0);

    let resp = reverse(&c, &token, tid).await;
    assert_eq!(resp.status().as_u16(), 201);
    // The 200 is clawed back from b and returned to a; the $1.50 fee is kept.
    assert_eq!(balance(&c, &token, a).await, 498.5);
    assert_eq!(balance(&c, &token, b).await, 0.0);
}

#[tokio::test]
async fn reverse_requires_the_initiator() {
    let c = client();
    require_stack!(&c);
    let (_cust, token) = session(&c).await;
    let Some((_a, dep)) = seed_deposit(&c, &token, 100.0).await else {
        return;
    };

    let (_other, other_token) = session(&c).await;
    let resp = reverse(&c, &other_token, dep).await;
    assert_eq!(
        resp.status().as_u16(),
        404,
        "a non-initiator must not reverse (404, no leak)"
    );
}

#[tokio::test]
async fn concurrent_double_reverse_yields_one_conflict() {
    let c = client();
    require_stack!(&c);
    let (_cust, token) = session(&c).await;
    let Some((a, dep)) = seed_deposit(&c, &token, 500.0).await else {
        return;
    };

    // Fire two reversals of the same deposit concurrently.
    let (r1, r2) = tokio::join!(reverse(&c, &token, dep), reverse(&c, &token, dep));
    let mut codes = [r1.status().as_u16(), r2.status().as_u16()];
    codes.sort_unstable();
    // Exactly one wins (201); the loser is 409 (lost the guarded UPDATE) or 400
    // (read the already-reversed status first) — never two successes.
    assert_eq!(codes[0], 201, "one reversal must succeed: {codes:?}");
    assert!(
        codes[1] == 409 || codes[1] == 400,
        "the loser is rejected (409/400): {codes:?}"
    );
    // A single reversal took effect: back to 0.
    assert_eq!(balance(&c, &token, a).await, 0.0);
}
