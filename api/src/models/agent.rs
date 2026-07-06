//! Agentic-banking models: agents (machine principals) and mandates (the
//! scoped, limited, expiring, revocable consent records that gate them).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use validator::Validate;

/// The mandate scope vocabulary. Unknown scopes are rejected at grant time.
pub const SCOPE_READ_BALANCE: &str = "read:balance";
pub const SCOPE_READ_TRANSACTIONS: &str = "read:transactions";
pub const SCOPE_TRANSFER_INITIATE: &str = "transfer:initiate";
pub const KNOWN_SCOPES: [&str; 3] = [
    SCOPE_READ_BALANCE,
    SCOPE_READ_TRANSACTIONS,
    SCOPE_TRANSFER_INITIATE,
];

/// Self-registration request for an agent (`POST /api/v1/agents`).
/// Registration confers zero access — a customer mandate is the gate.
#[derive(Debug, Deserialize, Validate)]
pub struct RegisterAgentRequest {
    #[validate(length(min = 1, max = 100))]
    pub display_name: String,

    #[validate(length(max = 500))]
    pub description: Option<String>,
}

/// Registration response. `agent_secret` is returned exactly once; only its
/// SHA-256 hash is stored (the refresh-token pattern).
#[derive(Debug, Serialize)]
pub struct RegisterAgentResponse {
    pub agent_id: Uuid,
    pub agent_secret: String,
    pub display_name: String,
    pub kind: String,
    pub status: String,
}

/// Public agent metadata (`GET /api/v1/agents/{id}`) — never the secret.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AgentPublic {
    pub agent_id: Uuid,
    pub display_name: String,
    pub description: Option<String>,
    pub kind: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

/// Mandate creation (`POST /api/v1/mandates`) — **the consent act**, always
/// authenticated as the granting customer.
#[derive(Debug, Deserialize, Validate)]
pub struct CreateMandateRequest {
    pub agent_id: Uuid,
    pub account_id: Uuid,
    /// Any of `read:balance`, `read:transactions`, `transfer:initiate`.
    #[validate(length(min = 1))]
    pub scopes: Vec<String>,
    /// Required (together with `daily_cap`) when `transfer:initiate` is granted.
    pub max_per_tx: Option<Decimal>,
    pub daily_cap: Option<Decimal>,
    /// Optional payee allowlist for transfers; omitted/None = any destination.
    #[validate(length(min = 1))]
    pub allowed_payees: Option<Vec<Uuid>>,
    /// Must be in the future.
    pub expires_at: DateTime<Utc>,
}

/// A mandate as seen by its granting customer.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct MandateResponse {
    pub mandate_id: Uuid,
    pub agent_id: Uuid,
    pub account_id: Uuid,
    pub scopes: Vec<String>,
    pub max_per_tx: Option<Decimal>,
    pub daily_cap: Option<Decimal>,
    pub allowed_payees: Option<Vec<Uuid>>,
    pub daily_used: Decimal,
    pub status: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Agent-initiated transfer (`POST /api/v1/agent/transfers`). The funding
/// account is always the mandate's account — there is no `from` field by
/// design. `idempotency_key` is REQUIRED: agents retry on timeouts far more
/// than humans, and a replayed key must not double-spend.
#[derive(Debug, Deserialize, Validate)]
pub struct AgentTransferRequest {
    pub to_account_id: Uuid,
    pub amount: Decimal,
    #[validate(length(min = 1, max = 500))]
    pub description: String,
    #[validate(length(min = 1, max = 128))]
    pub idempotency_key: String,
}

/// Client-credentials request for an agent token
/// (`POST /api/v1/auth/agent-token`): the agent authenticates itself and names
/// the mandate it wants to act under.
#[derive(Debug, Deserialize, Validate)]
pub struct AgentTokenRequest {
    pub agent_id: Uuid,
    #[validate(length(min = 1))]
    pub agent_secret: String,
    pub mandate_id: Uuid,
}
