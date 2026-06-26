use rust_decimal::Decimal;
use uuid::Uuid;
use crate::models::account::Account;
use crate::errors::AppError;

pub type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

const ACCOUNT_COLUMNS: &str = "account_id, customer_id, account_number, account_type, currency, \
    balance, available_balance, status, interest_rate, overdraft_limit, minimum_balance, \
    created_at, updated_at, activated_at, closed_at";

pub async fn fetch_account_for_update(tx: &mut Tx<'_>, account_id: Uuid) -> Result<Option<Account>, sqlx::Error> {
    sqlx::query_as::<_, Account>(&format!(
        "SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE account_id = $1 FOR UPDATE"
    ))
    .bind(account_id)
    .fetch_optional(&mut **tx)
    .await
}

/// Insert both legs of a transaction in one statement, so the balance triggers
/// fire with a balanced set. The BEFORE-INSERT trigger fills in balance_before/
/// after and updates each account's balance; we pass 0 placeholders.
pub async fn post_two_legged(
    tx: &mut Tx<'_>,
    transaction_id: Uuid,
    account_a: Uuid,
    type_a: &str,
    account_b: Uuid,
    type_b: &str,
    amount: Decimal,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO transaction_entries
            (transaction_id, account_id, entry_type, amount, balance_before, balance_after, entry_order)
        VALUES
            ($1, $2, $3::entry_type, $6, 0, 0, 1),
            ($1, $4, $5::entry_type, $6, 0, 0, 2)
        "#,
    )
    .bind(transaction_id)
    .bind(account_a)
    .bind(type_a)
    .bind(account_b)
    .bind(type_b)
    .bind(amount)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A reference number matching `^[A-Z0-9]{10,20}$`: `prefix` + 12 digits.
pub fn reference_number(prefix: &str) -> String {
    let n = (Uuid::new_v4().as_u128() % 1_000_000_000_000) as u64;
    format!("{}{:012}", prefix, n)
}

/// Round to 2 dp (the schema rejects anything else) and reject non-positive.
pub fn normalize_amount(amount: Decimal) -> Result<Decimal, AppError> {
    let amount = amount.round_dp(2);
    if amount <= Decimal::ZERO {
        return Err(AppError::BadRequest("amount must be positive".to_string()));
    }
    Ok(amount)
}
