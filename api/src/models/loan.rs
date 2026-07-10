use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;
use validator::Validate;

// Core Loan Entity from DB
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Loan {
    pub loan_id: Uuid,
    pub customer_id: Uuid,
    pub account_id: Uuid,
    pub principal_amount: Decimal,
    pub interest_rate: Decimal,
    pub amortization_months: i32,
    pub monthly_payment: Decimal,
    pub status: String,
    pub next_payment_date: NaiveDate,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// Request to apply for a new loan
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ApplyLoanRequest {
    pub principal_amount: Decimal,
    pub interest_rate: Decimal, // e.g. 0.0850 for 8.5%
    pub amortization_months: i32,
}

// Request to repay a portion of a loan
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct RepayLoanRequest {
    pub funding_account_id: Uuid,
    pub amount: Decimal,
}

// Detailed response representing a loan
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoanResponse {
    pub loan_id: Uuid,
    pub customer_id: Uuid,
    pub account_id: Uuid,
    pub principal_amount: Decimal,
    pub interest_rate: Decimal,
    pub amortization_months: i32,
    pub monthly_payment: Decimal,
    pub status: String,
    pub next_payment_date: NaiveDate,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// Short summary of a loan for lists
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoanSummary {
    pub loan_id: Uuid,
    pub account_id: Uuid,
    pub principal_amount: Decimal,
    pub status: String,
    pub next_payment_date: NaiveDate,
    pub monthly_payment: Decimal,
}

impl From<Loan> for LoanResponse {
    fn from(l: Loan) -> Self {
        Self {
            loan_id: l.loan_id,
            customer_id: l.customer_id,
            account_id: l.account_id,
            principal_amount: l.principal_amount,
            interest_rate: l.interest_rate,
            amortization_months: l.amortization_months,
            monthly_payment: l.monthly_payment,
            status: l.status,
            next_payment_date: l.next_payment_date,
            created_at: l.created_at,
            updated_at: l.updated_at,
        }
    }
}

impl From<Loan> for LoanSummary {
    fn from(l: Loan) -> Self {
        Self {
            loan_id: l.loan_id,
            account_id: l.account_id,
            principal_amount: l.principal_amount,
            status: l.status,
            next_payment_date: l.next_payment_date,
            monthly_payment: l.monthly_payment,
        }
    }
}
