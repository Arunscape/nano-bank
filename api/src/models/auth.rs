use serde::{Deserialize, Serialize};
use validator::Validate;

/// Login credentials submitted to `POST /api/v1/auth/login`.
#[derive(Debug, Deserialize, Validate)]
pub struct LoginRequest {
    #[validate(email)]
    pub email: String,

    #[validate(length(min = 1))]
    pub password: String,
}

/// Client-credentials request for a network-plane service token, submitted to
/// `POST /api/v1/auth/service-token` by the card network/processor.
#[derive(Debug, Deserialize, Validate)]
pub struct ServiceTokenRequest {
    #[validate(length(min = 1))]
    pub client_secret: String,
}

/// Refresh request: exchange a (rotating) refresh token for a fresh access token.
#[derive(Debug, Deserialize, Validate)]
pub struct RefreshRequest {
    #[validate(length(min = 1))]
    pub refresh_token: String,
}

/// Customer login / refresh response: a short-lived `access_token` plus a
/// `refresh_token` (rotated on each refresh) to mint the next access token.
/// `token_type` is always `"Bearer"`; `expires_in` is the access-token lifetime
/// in seconds (mirrors `jwt.expires_in`).
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: i64,
}

/// Service-token response (network plane). No refresh token — the network
/// re-mints via client-credentials when its token expires.
#[derive(Debug, Serialize)]
pub struct AccessTokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: i64,
}
