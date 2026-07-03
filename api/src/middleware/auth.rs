//! Bearer-token extractors for the two trust planes.
//!
//! - [`AuthenticatedCustomer`] turns a customer (`role: customer`) token into the
//!   caller's `customer_id`, for the consumer-app endpoints (`/customers/*`,
//!   `/accounts/*`). It replaces the old `?customer_id=` query param so identity
//!   is derived from the verified token, not trusted from the client.
//! - [`AuthenticatedService`] accepts only a service (`role: service`) token, for
//!   the network-plane rails (`/cards/*`), where the caller is the card
//!   network/processor rather than a cardholder.
//!
//! Both read only request headers, so they implement `FromRequestParts` and can
//! sit before a `Json` body extractor in a handler signature.

use axum::async_trait;
use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use uuid::Uuid;

use crate::errors::AppError;
use crate::handlers::AppState;
use crate::utils::jwt::{decode_access_token, Claims, TokenRole};

/// Pull and verify the `Authorization: Bearer <jwt>` token, returning its claims.
fn bearer_claims(parts: &Parts, state: &AppState) -> Result<Claims, AppError> {
    let token = parts
        .headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            AppError::Authentication("Missing or malformed Authorization header".to_string())
        })?;

    decode_access_token(token, &state.settings.jwt)
}

/// A cardholder authenticated on the consumer-app plane.
pub struct AuthenticatedCustomer {
    pub customer_id: Uuid,
    /// The login session this token belongs to (`user_sessions.session_id`).
    /// `None` for tokens minted before sessions existed; logout no-ops on those.
    pub session_id: Option<Uuid>,
}

#[async_trait]
impl FromRequestParts<AppState> for AuthenticatedCustomer {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let claims = bearer_claims(parts, state)?;
        // A service token must never be accepted on a consumer endpoint.
        match (claims.role, claims.sub) {
            (TokenRole::Customer, Some(customer_id)) => Ok(AuthenticatedCustomer {
                customer_id,
                session_id: claims.sid,
            }),
            _ => Err(AppError::Authentication(
                "A customer access token is required".to_string(),
            )),
        }
    }
}

/// A machine principal (the card network/processor) authenticated on the
/// network plane. Carries no customer identity by design.
pub struct AuthenticatedService;

#[async_trait]
impl FromRequestParts<AppState> for AuthenticatedService {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let claims = bearer_claims(parts, state)?;
        // A valid customer token on a network endpoint is the wrong plane → 403.
        if claims.role == TokenRole::Service {
            Ok(AuthenticatedService)
        } else {
            Err(AppError::Authorization(
                "A service access token is required for the card rails".to_string(),
            ))
        }
    }
}
