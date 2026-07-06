use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use validator::Validate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "lynx_direction", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum LynxDirection {
    Outbound,
    Inbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "lynx_wire_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum LynxWireStatus {
    Sent,
    Settled,
    Rejected,
    Recalled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "lynx_recall_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum LynxRecallStatus {
    Requested,
    Accepted,
    Rejected,
}

// ---- customer plane ----

#[derive(Debug, Deserialize, Validate)]
pub struct InitiateWireRequest {
    pub from_account_id: Uuid,
    pub amount: Decimal,
    #[validate(length(min = 1, max = 140))]
    pub counterparty_name: String,
    #[validate(length(min = 3, max = 3))]
    pub counterparty_institution: String,
    #[validate(length(min = 1, max = 34))]
    pub counterparty_account: String,
    #[validate(length(max = 140))]
    pub remittance_info: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct WireResponse {
    pub wire_id: Uuid,
    pub uetr: Uuid,
    pub direction: String,
    pub status: String,
    pub amount: Decimal,
    pub currency: String,
    pub counterparty_name: String,
    pub counterparty_institution: String,
    pub message_type: String,
    pub reference_number: String,
    pub gl_entry: Option<String>,
}

#[derive(Debug, Deserialize, Validate)]
pub struct RecallRequest {
    #[validate(length(max = 140))]
    pub reason: Option<String>,
}

// ---- network plane ----

#[derive(Debug, Deserialize)]
pub struct NetworkInboundRequest {
    pub debtor_name: String,
    pub debtor_institution: String,
    pub debtor_account: String,
    pub beneficiary_institution: String,
    pub beneficiary_transit: String,
    pub beneficiary_account: String,
    pub amount: Decimal,
    pub remittance_info: Option<String>,
    pub message_type: Option<String>, // pacs.008 | pacs.009
    pub uetr: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct RecallResolveRequest {
    pub decision: String, // accept | reject
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InboundRecallRequest {
    pub wire_id: Uuid,
    pub decision: String, // accept | reject
    pub reason: Option<String>,
}
