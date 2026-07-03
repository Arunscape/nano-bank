//! Password hashing and verification.
//!
//! Uses argon2id (the OWASP-recommended default) with a random per-password
//! salt. Hashes are stored as PHC strings (`$argon2id$v=19$...`) which embed the
//! algorithm, parameters, and salt, so verification needs only the stored string.

use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;

use crate::errors::AppError;

/// Hash a plaintext password with argon2id and a fresh random salt.
pub fn hash_password(plain: &str) -> Result<String, AppError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| AppError::Internal(format!("password hashing failed: {e}")))
}

/// Verify a plaintext password against a stored argon2 PHC hash.
///
/// Returns `Ok(false)` for a well-formed hash that simply doesn't match, and an
/// error only if the stored hash itself is malformed.
pub fn verify_password(plain: &str, hash: &str) -> Result<bool, AppError> {
    let parsed = PasswordHash::new(hash)
        .map_err(|e| AppError::Internal(format!("stored password hash is malformed: {e}")))?;
    match Argon2::default().verify_password(plain.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(e) => Err(AppError::Internal(format!(
            "password verification failed: {e}"
        ))),
    }
}
