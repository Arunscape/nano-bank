use axum::{
    extract::{ConnectInfo, State},
    http::{header::USER_AGENT, HeaderMap, StatusCode},
    response::Json,
    routing::post,
    Router,
};
use std::net::SocketAddr;
use uuid::Uuid;
use validator::Validate;

use crate::errors::AppError;
use crate::handlers::AppState;
use crate::middleware::auth::AuthenticatedCustomer;
use crate::models::auth::{
    AccessTokenResponse, LoginRequest, LoginResponse, RefreshRequest, ServiceTokenRequest,
};
use crate::utils::jwt::{encode_access_token, encode_service_token};
use crate::utils::password::verify_password;

pub fn auth_routes() -> Router<AppState> {
    Router::new()
        .route("/login", post(login))
        .route("/service-token", post(issue_service_token))
        .route("/refresh", post(refresh_token))
        .route("/logout", post(logout))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Constant-time byte comparison, to avoid leaking the secret via timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A fresh opaque refresh token (~244 bits from two v4 UUIDs). The plaintext is
/// returned to the client once; only its SHA-256 hash is stored (see callers).
fn generate_refresh_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Best-effort record of a failed login attempt (feeds the lockout window). A
/// failure to write the audit row must not change the login outcome.
async fn record_failed_login(
    pool: &sqlx::PgPool,
    email: &str,
    ip: &str,
    user_agent: Option<&str>,
    reason: &str,
) {
    if let Err(e) = sqlx::query(
        "INSERT INTO failed_login_attempts (email, ip_address, user_agent, failure_reason)
         VALUES ($1, $2::inet, $3, $4)",
    )
    .bind(email)
    .bind(ip)
    .bind(user_agent)
    .bind(reason)
    .execute(pool)
    .await
    {
        tracing::warn!("failed to record failed-login attempt: {e}");
    }
}

// ---------------------------------------------------------------------------
// service token (network plane)
// ---------------------------------------------------------------------------

/// Issue a network-plane service token (OAuth client-credentials style).
///
/// The card network/processor presents the shared `client_secret`; on a match we
/// mint a short-lived service JWT (`role: service`, no customer subject). The
/// signing key never leaves the server. Used by the rails (`/cards/*`).
async fn issue_service_token(
    State(state): State<AppState>,
    Json(payload): Json<ServiceTokenRequest>,
) -> Result<(StatusCode, Json<AccessTokenResponse>), AppError> {
    payload.validate()?;

    if !constant_time_eq(
        payload.client_secret.as_bytes(),
        state.settings.security.service_client_secret.as_bytes(),
    ) {
        return Err(AppError::Authentication(
            "Invalid client credentials".to_string(),
        ));
    }

    let access_token = encode_service_token(&state.settings.jwt)?;

    tracing::info!("🔑 service token issued (network plane)");

    Ok((
        StatusCode::OK,
        Json(AccessTokenResponse {
            access_token,
            token_type: "Bearer".to_string(),
            expires_in: state.settings.jwt.expires_in,
        }),
    ))
}

// ---------------------------------------------------------------------------
// login (customer plane)
// ---------------------------------------------------------------------------

/// Authenticate a customer: issue a short-lived access token plus a refresh
/// token backed by a revocable `user_sessions` row.
///
/// Every credential failure returns the same generic 401 (no account
/// enumeration) and is recorded; after `max_login_attempts` failures for an
/// email within `lockout_duration`, login is locked out with a 429.
async fn login(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<LoginRequest>,
) -> Result<(StatusCode, Json<LoginResponse>), AppError> {
    payload.validate()?;

    let ip = addr.ip().to_string();
    let ua = user_agent(&headers);

    // Lockout: too many recent failures for this email.
    let recent_failures: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM failed_login_attempts
         WHERE email = $1 AND created_at > now() - ($2 * interval '1 second')",
    )
    .bind(&payload.email)
    .bind(state.settings.security.lockout_duration as i64)
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::Database)?;

    if recent_failures >= state.settings.security.max_login_attempts as i64 {
        return Err(AppError::RateLimit(
            "Too many failed login attempts; try again later".to_string(),
        ));
    }

    let invalid = || AppError::Authentication("Invalid email or password".to_string());

    // (customer_id, password_hash) for the email, if both the customer and a
    // credential row exist.
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT c.customer_id, cr.password_hash
         FROM customers c
         JOIN customer_credentials cr ON cr.customer_id = c.customer_id
         WHERE c.email = $1",
    )
    .bind(&payload.email)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::Database)?;

    let (customer_id, password_hash) = match row {
        Some(r) => r,
        None => {
            record_failed_login(
                &state.pool,
                &payload.email,
                &ip,
                ua.as_deref(),
                "unknown_email",
            )
            .await;
            return Err(invalid());
        }
    };

    if !verify_password(&payload.password, &password_hash)? {
        record_failed_login(
            &state.pool,
            &payload.email,
            &ip,
            ua.as_deref(),
            "bad_password",
        )
        .await;
        return Err(invalid());
    }

    // Success: clear the failure counter, then open a revocable session whose
    // refresh token is stored hashed (pgcrypto digest), never in plaintext.
    let _ = sqlx::query("DELETE FROM failed_login_attempts WHERE email = $1")
        .bind(&payload.email)
        .execute(&state.pool)
        .await;

    let refresh_token = generate_refresh_token();
    let session_id: Uuid = sqlx::query_scalar(
        "INSERT INTO user_sessions (customer_id, session_token, ip_address, user_agent, expires_at)
         VALUES ($1, encode(digest($2, 'sha256'), 'hex'), $3::inet, $4,
                 now() + ($5 * interval '1 second'))
         RETURNING session_id",
    )
    .bind(customer_id)
    .bind(&refresh_token)
    .bind(&ip)
    .bind(ua.as_deref())
    .bind(state.settings.jwt.refresh_expires_in)
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::Database)?;

    let access_token = encode_access_token(customer_id, session_id, &state.settings.jwt)?;

    tracing::info!(%customer_id, %session_id, "🔑 customer logged in");

    Ok((
        StatusCode::OK,
        Json(LoginResponse {
            access_token,
            refresh_token,
            token_type: "Bearer".to_string(),
            expires_in: state.settings.jwt.expires_in,
        }),
    ))
}

// ---------------------------------------------------------------------------
// refresh
// ---------------------------------------------------------------------------

/// Exchange a refresh token for a new access token, rotating the refresh token
/// (single-use) and sliding the session expiry. A terminated or expired session
/// is rejected — this is what makes logout and expiry actually revoke access.
async fn refresh_token(
    State(state): State<AppState>,
    Json(payload): Json<RefreshRequest>,
) -> Result<(StatusCode, Json<LoginResponse>), AppError> {
    payload.validate()?;

    let session: Option<(Uuid, Uuid, chrono::DateTime<chrono::Utc>, bool)> = sqlx::query_as(
        "SELECT session_id, customer_id, expires_at, is_active
         FROM user_sessions
         WHERE session_token = encode(digest($1, 'sha256'), 'hex')",
    )
    .bind(&payload.refresh_token)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::Database)?;

    let (session_id, customer_id, expires_at, is_active) =
        session.ok_or_else(|| AppError::Authentication("Invalid refresh token".to_string()))?;

    if !is_active {
        return Err(AppError::Authentication(
            "Invalid refresh token".to_string(),
        ));
    }
    if expires_at <= chrono::Utc::now() {
        let _ = sqlx::query(
            "UPDATE user_sessions
             SET is_active = false, terminated_at = now(), termination_reason = 'expired'
             WHERE session_id = $1",
        )
        .bind(session_id)
        .execute(&state.pool)
        .await;
        return Err(AppError::SessionExpired);
    }

    let new_refresh = generate_refresh_token();
    sqlx::query(
        "UPDATE user_sessions
         SET session_token = encode(digest($1, 'sha256'), 'hex'),
             last_activity_at = now(),
             expires_at = now() + ($2 * interval '1 second')
         WHERE session_id = $3",
    )
    .bind(&new_refresh)
    .bind(state.settings.jwt.refresh_expires_in)
    .bind(session_id)
    .execute(&state.pool)
    .await
    .map_err(AppError::Database)?;

    let access_token = encode_access_token(customer_id, session_id, &state.settings.jwt)?;

    Ok((
        StatusCode::OK,
        Json(LoginResponse {
            access_token,
            refresh_token: new_refresh,
            token_type: "Bearer".to_string(),
            expires_in: state.settings.jwt.expires_in,
        }),
    ))
}

// ---------------------------------------------------------------------------
// logout
// ---------------------------------------------------------------------------

/// Terminate the caller's session (revokes its refresh token). Idempotent; the
/// current access token remains valid until it expires (short TTL by design).
async fn logout(
    State(state): State<AppState>,
    auth: AuthenticatedCustomer,
) -> Result<StatusCode, AppError> {
    if let Some(session_id) = auth.session_id {
        sqlx::query(
            "UPDATE user_sessions
             SET is_active = false, terminated_at = now(), termination_reason = 'logout'
             WHERE session_id = $1 AND is_active = true",
        )
        .bind(session_id)
        .execute(&state.pool)
        .await
        .map_err(AppError::Database)?;
        tracing::info!(%session_id, "👋 customer logged out");
    }
    Ok(StatusCode::NO_CONTENT)
}
