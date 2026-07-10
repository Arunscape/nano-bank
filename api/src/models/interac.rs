use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use validator::Validate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "interac_handle_type", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum HandleType {
    Email,
    Phone,
}

#[derive(Debug, Deserialize, Validate)]
pub struct RegisterAutodepositRequest {
    pub handle_type: HandleType,
    #[validate(length(min = 3, max = 255))]
    pub handle_value: String,
    pub deposit_account_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct HandleResponse {
    pub handle_id: Uuid,
    pub handle_type: HandleType,
    pub handle_value: String,
    pub autodeposit_account_id: Option<Uuid>,
    pub active: bool,
}

#[derive(Debug, Deserialize, Validate)]
pub struct SendEtransferRequest {
    pub from_account_id: Uuid,
    pub amount: Decimal,
    pub recipient_handle_type: HandleType,
    #[validate(length(min = 3, max = 255))]
    pub recipient_handle_value: String,
    /// Required unless the recipient handle has autodeposit enabled.
    pub security_question: Option<String>,
    pub security_answer: Option<String>,
    pub memo: Option<String>,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ClaimEtransferRequest {
    pub security_answer: String,
    pub deposit_account_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct InboundEtransferRequest {
    pub amount: Decimal,
    pub sender_name: String,
    pub counterparty_institution: String,
    pub recipient_handle_type: HandleType,
    pub recipient_handle_value: String,
    pub security_question: Option<String>,
    pub security_answer: Option<String>,
    pub memo: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SettleEtransferRequest {
    /// "claimed" | "declined"
    pub outcome: String,
    pub institution: String,
}

#[derive(Debug, Serialize)]
pub struct EtransferResponse {
    pub etransfer_id: Uuid,
    pub direction: String,
    pub status: String,
    pub amount: Decimal,
    pub recipient_handle_value: String,
    pub security_question: Option<String>,
    pub memo: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}
