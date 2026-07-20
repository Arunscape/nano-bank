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
// Branch-enforced scopes: the bank stores them on the mandate but exposes no
// agent-plane endpoint; the agentic branch checks them before routing the
// operation to the customer REST via the personal manager.
pub const SCOPE_ACCOUNT_OPEN: &str = "account:open";
pub const SCOPE_PAYEE_REGISTER: &str = "payee:register";
pub const KNOWN_SCOPES: [&str; 5] = [
    SCOPE_READ_BALANCE,
    SCOPE_READ_TRANSACTIONS,
    SCOPE_TRANSFER_INITIATE,
    SCOPE_ACCOUNT_OPEN,
    SCOPE_PAYEE_REGISTER,
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

/// One row of a mandate's audit trail, as shown to its granting customer
/// (`GET /api/v1/mandates/{id}/actions`). Denials are included by design.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AgentActionResponse {
    pub action_id: Uuid,
    pub operation: String,
    pub amount: Option<Decimal>,
    pub decision: String,
    pub reason: Option<String>,
    pub transaction_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Agent-initiated transfer (`POST /api/v1/agent/transfers`). The funding
/// account is always the mandate's account — there is no `from` field by
/// design. `idempotency_key` is REQUIRED: agents retry on timeouts far more
/// than humans. Keys are namespaced to the mandate; a *sequential* replay
/// returns the original transfer (best-effort like the customer path — no
/// unique index, so tightly-concurrent duplicates could still both post).
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

/// Bare agent credentials (`POST /api/v1/auth/agent-mandates`): used to
/// discover the agent's own active mandates, so one agent can hold several
/// grants (different accounts, different scopes) behind a single registration.
#[derive(Debug, Deserialize, Validate)]
pub struct AgentCredentialsRequest {
    pub agent_id: Uuid,
    #[validate(length(min = 1))]
    pub agent_secret: String,
}

/// A step-up approval as seen by the agent that raised it: returned with the
/// **202** on an over-cap transfer, and by the poll surface
/// (`GET /api/v1/agent/approvals/{id}`). Status contract: `approved` ALWAYS
/// carries `transaction_id` (they are written atomically) — treat it as final;
/// the transient `executing` means the owner approved and the transfer is
/// posting — poll again shortly.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AgentApprovalStatus {
    pub approval_id: Uuid,
    pub status: String,
    /// Which cap tripped: `MAX_PER_TX_EXCEEDED` / `DAILY_CAP_EXCEEDED`.
    pub reason: String,
    pub amount: Decimal,
    pub to_account_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub transaction_id: Option<Uuid>,
}

/// A step-up approval as seen by its granting customer
/// (`GET /api/v1/approvals`): everything needed to decide — which agent asked,
/// out of which account, to where, how much, and which cap it breached.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PendingApprovalResponse {
    pub approval_id: Uuid,
    pub mandate_id: Uuid,
    pub agent_display_name: String,
    pub account_id: Uuid,
    pub account_last4: String,
    pub to_account_id: Uuid,
    pub amount: Decimal,
    pub description: String,
    pub reason: String,
    pub status: String,
    pub transaction_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

/// One of the agent's own active grants, as returned by mandate discovery.
/// Carries just enough account identity to *address* the account in
/// conversation (type + last-4) — full account detail stays behind the
/// mandate-pinned read surface.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AgentMandateSummary {
    pub mandate_id: Uuid,
    pub account_id: Uuid,
    pub account_type: String,
    pub account_last4: String,
    pub scopes: Vec<String>,
    pub max_per_tx: Option<Decimal>,
    pub daily_cap: Option<Decimal>,
    pub daily_used: Decimal,
    pub expires_at: DateTime<Utc>,
}
