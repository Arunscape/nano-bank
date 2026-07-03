//! JWT access-token encoding/decoding.
//!
//! Two token kinds share one signing key and claim shape, distinguished by
//! `role`:
//!   - **customer** tokens (`sub` = customer id) authenticate the consumer-app
//!     plane (`/customers/*`, `/accounts/*`). Issued by `POST /auth/login`.
//!   - **service** tokens (no `sub`) authenticate the network plane
//!     (`/cards/*`), where the caller is the card network/processor, not a
//!     cardholder. Issued by `POST /auth/service-token`.
//!
//! Access tokens are short-lived (TTL = `jwt.expires_in`) and stateless.

use chrono::Utc;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::JwtSettings;
use crate::errors::AppError;

/// Which trust plane a token belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenRole {
    /// A cardholder using the consumer app.
    Customer,
    /// A machine principal (the card network/processor) driving the rails.
    Service,
}

/// Registered + custom claims carried by an access token.
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    /// Subject — the customer's id for customer tokens; absent for service tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub: Option<Uuid>,
    /// Session id (the revocable `user_sessions` row) for customer tokens; absent
    /// for service tokens. Lets logout terminate exactly this session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sid: Option<Uuid>,
    /// Trust plane this token authenticates.
    pub role: TokenRole,
    /// Issuer — must match `jwt.issuer`.
    pub iss: String,
    /// Issued-at (unix seconds).
    pub iat: i64,
    /// Expiry (unix seconds).
    pub exp: i64,
}

fn encode_claims(
    sub: Option<Uuid>,
    sid: Option<Uuid>,
    role: TokenRole,
    jwt: &JwtSettings,
) -> Result<String, AppError> {
    let now = Utc::now().timestamp();
    let claims = Claims {
        sub,
        sid,
        role,
        iss: jwt.issuer.clone(),
        iat: now,
        exp: now + jwt.expires_in,
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(jwt.secret.as_bytes()),
    )
    .map_err(AppError::from)
}

/// Mint a customer (consumer-app) access token bound to a login session.
pub fn encode_access_token(
    customer_id: Uuid,
    session_id: Uuid,
    jwt: &JwtSettings,
) -> Result<String, AppError> {
    encode_claims(
        Some(customer_id),
        Some(session_id),
        TokenRole::Customer,
        jwt,
    )
}

/// Mint a service (network-plane) access token. No customer subject or session.
pub fn encode_service_token(jwt: &JwtSettings) -> Result<String, AppError> {
    encode_claims(None, None, TokenRole::Service, jwt)
}

/// Verify a token's signature, expiry, and issuer, returning its claims.
pub fn decode_access_token(token: &str, jwt: &JwtSettings) -> Result<Claims, AppError> {
    let mut validation = Validation::default();
    validation.set_issuer(&[&jwt.issuer]);
    // `exp` is validated by default.

    decode::<Claims>(
        token,
        &DecodingKey::from_secret(jwt.secret.as_bytes()),
        &validation,
    )
    .map(|data| data.claims)
    .map_err(AppError::from)
}
