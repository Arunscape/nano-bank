use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::net::IpAddr;
use uuid::Uuid;
use validator::Validate;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "audit_action", rename_all = "snake_case")]
pub enum AuditAction {
    Create,
    Update,
    Delete,
    Login,
    Logout,
    Transaction,
}

// Authentication Request
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct LoginRequest {
    #[validate(email)]
    pub email: String,

    #[validate(length(min = 1))]
    pub password: String,

    pub device_fingerprint: Option<String>,
}

// Authentication Response
#[derive(Debug, Serialize, Deserialize)]
pub struct LoginResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    pub customer: CustomerInfo,
    pub requires_mfa: bool,
}

// Customer info for auth response
#[derive(Debug, Serialize, Deserialize)]
pub struct CustomerInfo {
    pub customer_id: Uuid,
    pub email: String,
    pub first_name: String,
    pub last_name: String,
    pub kyc_status: String,
}

// Token Claims
#[derive(Debug, Serialize, Deserialize)]
pub struct TokenClaims {
    pub sub: Uuid, // Customer ID
    pub email: String,
    pub exp: i64, // Expiration time
    pub iat: i64, // Issued at
    pub session_id: Uuid,
    pub device_fingerprint: Option<String>,
}

// Refresh Token Request
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct RefreshTokenRequest {
    #[validate(length(min = 1))]
    pub refresh_token: String,
}

// User Session Entity
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct UserSession {
    pub session_id: Uuid,
    pub customer_id: Uuid,
    pub session_token: String,
    pub ip_address: IpAddr,
    pub user_agent: Option<String>,
    pub device_fingerprint: Option<String>,
    pub is_active: bool,
    pub last_activity_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub terminated_at: Option<DateTime<Utc>>,
    pub termination_reason: Option<String>,
}

// Failed Login Attempt Entity
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct FailedLoginAttempt {
    pub attempt_id: Uuid,
    pub email: Option<String>,
    pub ip_address: IpAddr,
    pub user_agent: Option<String>,
    pub failure_reason: String,
    pub created_at: DateTime<Utc>,
}

// Suspicious Activity Entity
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct SuspiciousActivity {
    pub activity_id: Uuid,
    pub customer_id: Option<Uuid>,
    pub account_id: Option<Uuid>,
    pub activity_type: String,
    pub risk_score: i32,
    pub description: String,
    pub metadata: Option<serde_json::Value>,
    pub status: String,
    pub assigned_to: Option<String>,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub resolution_notes: Option<String>,
}

// Known Device Entity
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct KnownDevice {
    pub device_id: Uuid,
    pub customer_id: Uuid,
    pub device_fingerprint: String,
    pub device_name: Option<String>,
    pub device_type: Option<String>,
    pub os_info: Option<String>,
    pub browser_info: Option<String>,
    pub is_trusted: bool,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub last_seen_ip: Option<IpAddr>,
    pub usage_count: i32,
}

// Audit Log Entity
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct AuditLog {
    pub log_id: Uuid,
    pub entity_type: String,
    pub entity_id: Uuid,
    pub action: AuditAction,
    pub old_values: Option<serde_json::Value>,
    pub new_values: Option<serde_json::Value>,
    pub user_id: Option<Uuid>,
    pub session_id: Option<String>,
    pub ip_address: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub created_at: DateTime<Utc>,
}

// Monitoring Rule Entity
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct MonitoringRule {
    pub rule_id: Uuid,
    pub rule_name: String,
    pub rule_type: String,
    pub conditions: serde_json::Value,
    pub risk_score: i32,
    pub is_active: bool,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// Rule Violation Entity
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct RuleViolation {
    pub violation_id: Uuid,
    pub rule_id: Uuid,
    pub customer_id: Option<Uuid>,
    pub account_id: Option<Uuid>,
    pub transaction_id: Option<Uuid>,
    pub violation_data: serde_json::Value,
    pub risk_score: i32,
    pub status: String,
    pub reviewed_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub reviewed_at: Option<DateTime<Utc>>,
}

// Device Trust Request
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct TrustDeviceRequest {
    #[validate(length(min = 1))]
    pub device_fingerprint: String,

    #[validate(length(min = 1, max = 100))]
    pub device_name: String,

    pub device_type: Option<String>,
}

// Session Response
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionResponse {
    pub session_id: Uuid,
    pub ip_address: IpAddr,
    pub device_info: Option<DeviceInfo>,
    pub is_current: bool,
    pub last_activity_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

// Device Info Response
#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub device_name: Option<String>,
    pub device_type: Option<String>,
    pub os_info: Option<String>,
    pub browser_info: Option<String>,
    pub is_trusted: bool,
}

// Security Alert Request
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct SecurityAlertRequest {
    #[validate(length(min = 1, max = 100))]
    pub activity_type: String,

    #[validate(range(min = 1, max = 100))]
    pub risk_score: i32,

    #[validate(length(min = 1))]
    pub description: String,

    pub metadata: Option<serde_json::Value>,
}

// Password Change Request
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct ChangePasswordRequest {
    #[validate(length(min = 1))]
    pub current_password: String,

    #[validate(length(min = 8))]
    pub new_password: String,
}

// MFA Setup Request
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct MfaSetupRequest {
    pub method: String, // "totp", "sms", etc.
    pub phone_number: Option<String>,
}

// MFA Verification Request
#[derive(Debug, Serialize, Deserialize, Validate)]
pub struct MfaVerificationRequest {
    #[validate(length(equal = 6))]
    pub code: String,

    pub session_id: Uuid,
}
