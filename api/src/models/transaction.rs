use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;
use validator::Validate;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "transaction_status", rename_all = "snake_case")]
pub enum TransactionStatus {
    Pending,
    Completed,
    Failed,
    Reversed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "entry_type", rename_all = "snake_case")]
pub enum EntryType {
    Debit,
    Credit,
}

// Core Transaction Entity
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Transaction {
    pub transaction_id: Uuid,
    pub reference_number: String,
    pub transaction_type: String,
    pub amount: Decimal,
    pub currency: String,
    pub description: Option<String>,
    pub status: TransactionStatus,
    pub initiated_by: Option<Uuid>,
    pub external_reference: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub processed_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub failure_reason: Option<String>,
}

// Transaction Entry Entity (Double-Entry Bookkeeping)
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct TransactionEntry {
    pub entry_id: Uuid,
    pub transaction_id: Uuid,
    pub account_id: Uuid,
    pub entry_type: EntryType,
    pub amount: Decimal,
    pub balance_before: Decimal,
    pub balance_after: Decimal,
    pub entry_order: i32,
    pub created_at: DateTime<Utc>,
}

// Money Transfer Request
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct MoneyTransferRequest {
    pub from_account_id: Uuid,
    pub to_account_id: Uuid,

    pub amount: Decimal,

    #[validate(length(min = 1, max = 255))]
    pub description: String,

    pub reference: Option<String>,

    // Idempotency key to prevent duplicate transfers
    pub idempotency_key: Option<String>,
}

// Internal Transfer Request (between own accounts)
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct InternalTransferRequest {
    pub from_account_id: Uuid,
    pub to_account_id: Uuid,

    pub amount: Decimal,

    #[validate(length(min = 1, max = 255))]
    pub description: String,
}

// Deposit Request
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct DepositRequest {
    pub account_id: Uuid,

    pub amount: Decimal,

    #[validate(length(min = 1, max = 255))]
    pub description: String,

    pub external_reference: Option<String>,
}

// Withdrawal Request
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct WithdrawalRequest {
    pub account_id: Uuid,

    pub amount: Decimal,

    #[validate(length(min = 1, max = 255))]
    pub description: String,

    pub external_reference: Option<String>,
}

// Transaction Reversal Entity
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct TransactionReversal {
    pub reversal_id: Uuid,
    pub original_transaction_id: Uuid,
    pub reversal_transaction_id: Uuid,
    pub reason: String,
    pub authorized_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

// Transaction Fee Entity
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct TransactionFee {
    pub fee_id: Uuid,
    pub transaction_id: Uuid,
    pub fee_type: String,
    pub fee_amount: Decimal,
    pub fee_percentage: Option<Decimal>,
    pub waived: bool,
    pub waived_reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

// Daily Transaction Summary
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct DailyTransactionSummary {
    pub summary_id: Uuid,
    pub account_id: Uuid,
    pub summary_date: chrono::NaiveDate,
    pub total_debits: Decimal,
    pub total_credits: Decimal,
    pub transaction_count: i32,
    pub largest_debit: Option<Decimal>,
    pub largest_credit: Option<Decimal>,
    pub end_of_day_balance: Decimal,
    pub created_at: DateTime<Utc>,
}

// Transaction Response
#[derive(Debug, Serialize, Deserialize)]
pub struct TransactionResponse {
    pub transaction_id: Uuid,
    pub reference_number: String,
    pub transaction_type: String,
    pub amount: Decimal,
    pub currency: String,
    pub description: Option<String>,
    pub status: TransactionStatus,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub entries: Vec<TransactionEntryResponse>,
}

// Transaction Entry Response
#[derive(Debug, Serialize, Deserialize)]
pub struct TransactionEntryResponse {
    pub entry_id: Uuid,
    pub account_id: Uuid,
    pub entry_type: EntryType,
    pub amount: Decimal,
    pub balance_after: Decimal,
}

// Transaction History Query Parameters
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct TransactionHistoryQuery {
    pub account_id: Option<Uuid>,

    pub start_date: Option<DateTime<Utc>>,
    pub end_date: Option<DateTime<Utc>>,

    pub transaction_type: Option<String>,
    pub status: Option<TransactionStatus>,

    #[validate(range(min = 1, max = 100))]
    pub limit: Option<u32>,

    pub offset: Option<u32>,
}

// Paginated Transaction Response
#[derive(Debug, Serialize, Deserialize)]
pub struct TransactionHistoryResponse {
    pub transactions: Vec<TransactionResponse>,
    pub total_count: u64,
    pub has_more: bool,
    pub next_offset: Option<u32>,
}

impl From<Transaction> for TransactionResponse {
    fn from(transaction: Transaction) -> Self {
        Self {
            transaction_id: transaction.transaction_id,
            reference_number: transaction.reference_number,
            transaction_type: transaction.transaction_type,
            amount: transaction.amount,
            currency: transaction.currency,
            description: transaction.description,
            status: transaction.status,
            created_at: transaction.created_at,
            completed_at: transaction.completed_at,
            entries: vec![], // Will be populated separately
        }
    }
}

impl From<TransactionEntry> for TransactionEntryResponse {
    fn from(entry: TransactionEntry) -> Self {
        Self {
            entry_id: entry.entry_id,
            account_id: entry.account_id,
            entry_type: entry.entry_type,
            amount: entry.amount,
            balance_after: entry.balance_after,
        }
    }
}
