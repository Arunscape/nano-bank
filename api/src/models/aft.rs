use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use validator::Validate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "aft_entry_kind", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Credit,
    Debit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "aft_direction", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum AftDirection {
    Outbound,
    Inbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "aft_batch_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    Open,
    Submitted,
    Settled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "aft_entry_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum EntryStatus {
    Pending,
    Settled,
    Returned,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "mandate_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum MandateStatus {
    Active,
    Revoked,
}

// ---- mandates ----

#[derive(Debug, Deserialize, Validate)]
pub struct CreateMandateRequest {
    pub payer_account_id: Uuid,
    #[validate(length(min = 1, max = 200))]
    pub biller_name: String,
    #[validate(length(min = 1, max = 50))]
    pub originator_id: String,
    pub amount_cap: Decimal,
    pub frequency: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MandateResponse {
    pub mandate_id: Uuid,
    pub payer_account_id: Uuid,
    pub biller_name: String,
    pub amount_cap: Decimal,
    pub status: String,
}

// ---- originate ----

#[derive(Debug, Deserialize, Validate)]
pub struct CreateCreditRequest {
    pub originator_account_id: Uuid,
    pub amount: Decimal,
    #[validate(length(min = 3, max = 3))]
    pub counterparty_institution: String,
    #[validate(length(min = 5, max = 5))]
    pub counterparty_transit: String,
    #[validate(length(min = 1, max = 12))]
    pub counterparty_account: String,
    #[validate(length(min = 1, max = 200))]
    pub payee_name: String,
}

#[derive(Debug, Deserialize, Validate)]
pub struct CreateDebitRequest {
    /// The biller's own nano-bank account that collected funds land in.
    pub originator_account_id: Uuid,
    pub amount: Decimal,
    pub mandate_id: Uuid,
}

// ---- responses ----

#[derive(Debug, Serialize)]
pub struct BatchResponse {
    pub batch_id: Uuid,
    pub direction: String,
    pub status: String,
    pub entry_count: i32,
    pub total_credits: Decimal,
    pub total_debits: Decimal,
    pub file_ref: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EntryResponse {
    pub entry_id: Uuid,
    pub batch_id: Uuid,
    pub kind: String,
    pub direction: String,
    pub amount: Decimal,
    pub status: String,
    pub payee_name: Option<String>,
    pub return_reason: Option<String>,
}
