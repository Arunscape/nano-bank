pub mod database;

use config::{Config, ConfigError, Environment, File};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::env;

#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub database: DatabaseSettings,
    pub server: ServerSettings,
    pub jwt: JwtSettings,
    pub security: SecuritySettings,
    pub logging: LoggingSettings,
    #[serde(default)]
    pub interac: InteracSettings,
    #[serde(default)]
    pub lynx: LynxSettings,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DatabaseSettings {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub database_name: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub workers: Option<usize>,
    pub keep_alive: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct JwtSettings {
    pub secret: String,
    pub expires_in: i64,
    pub refresh_expires_in: i64,
    pub issuer: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SecuritySettings {
    pub password_min_length: usize,
    pub max_login_attempts: u32,
    pub lockout_duration: u64,
    pub session_timeout: i64,
    pub require_mfa: bool,
    /// Shared secret presented by the card network/processor to mint a service
    /// token at `POST /auth/service-token` (OAuth client-credentials style).
    pub service_client_secret: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LoggingSettings {
    pub level: String,
    pub format: String,
}

/// Interac e-Transfer rail tunables. Overridable via `config/*.toml` or the
/// layered env vars `NANO_BANK__INTERAC__EXPIRY_DAYS` /
/// `NANO_BANK__INTERAC__MAX_ETRANSFER_AMOUNT`.
#[derive(Debug, Deserialize, Clone)]
pub struct InteracSettings {
    /// Hold lifetime before auto-expiry (real Interac: 30 days).
    #[serde(default = "default_expiry_days")]
    pub expiry_days: i64,
    /// Max amount per e-Transfer (funds check aside). Real Interac default $3,000.
    #[serde(with = "rust_decimal::serde::str", default = "default_max_etransfer")]
    pub max_etransfer_amount: Decimal,
}

fn default_expiry_days() -> i64 {
    30
}

fn default_max_etransfer() -> Decimal {
    Decimal::new(3000, 0)
}

impl Default for InteracSettings {
    fn default() -> Self {
        Self {
            expiry_days: default_expiry_days(),
            max_etransfer_amount: default_max_etransfer(),
        }
    }
}

/// Lynx RTGS wire rail tunables. Overridable via `config/*.toml` or the layered
/// env vars `NANO_BANK__LYNX__MIN_AMOUNT` / `NANO_BANK__LYNX__STALE_MINUTES`.
#[derive(Debug, Deserialize, Clone)]
pub struct LynxSettings {
    /// High-value floor: the minimum wire amount (real Lynx has no retail cap;
    /// this floor keeps low-value payments on the retail rails). Default $10,000.
    #[serde(with = "rust_decimal::serde::str", default = "default_min_amount")]
    pub min_amount: Decimal,
    /// How old (minutes) a `sent` wire must be before the admin sweep rejects it.
    #[serde(default = "default_stale_minutes")]
    pub stale_minutes: i32,
}

fn default_min_amount() -> Decimal {
    Decimal::new(1000000, 2)
}

fn default_stale_minutes() -> i32 {
    60
}

impl Default for LynxSettings {
    fn default() -> Self {
        Self {
            min_amount: default_min_amount(),
            stale_minutes: default_stale_minutes(),
        }
    }
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let run_mode = env::var("RUN_MODE").unwrap_or_else(|_| "development".into());

        let s = Config::builder()
            // Start with default configuration
            .add_source(File::with_name("config/default").required(false))
            // Add environment-specific configuration
            .add_source(File::with_name(&format!("config/{}", run_mode)).required(false))
            // Add local configuration (gitignored)
            .add_source(File::with_name("config/local").required(false))
            // Add environment variables with prefix "NANO_BANK"
            .add_source(Environment::with_prefix("NANO_BANK").separator("__"))
            .build()?;

        s.try_deserialize()
    }

    pub fn database_url(&self) -> String {
        let host = if self.database.host.contains(':') {
            format!("[{}]", self.database.host)
        } else {
            self.database.host.clone()
        };
        format!(
            "postgresql://{}:{}@{}:{}/{}?sslmode=disable",
            self.database.username,
            self.database.password,
            host,
            self.database.port,
            self.database.database_name
        )
    }

    pub fn server_address(&self) -> String {
        format!("{}:{}", self.server.host, self.server.port)
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            database: DatabaseSettings {
                host: "localhost".to_string(),
                port: 30432,
                username: "nanobank_user".to_string(),
                password: "secure_nano_password_2024!".to_string(),
                database_name: "nano_bank_db".to_string(),
                max_connections: 10,
                min_connections: 1,
                acquire_timeout: 30,
            },
            server: ServerSettings {
                host: "0.0.0.0".to_string(),
                port: 8081,
                workers: None,
                keep_alive: 60,
            },
            jwt: JwtSettings {
                secret: "your-super-secret-jwt-key-change-this-in-production".to_string(),
                expires_in: 900,            // 15 min (short-lived access token)
                refresh_expires_in: 604800, // 1 week
                issuer: "nano-bank".to_string(),
            },
            security: SecuritySettings {
                password_min_length: 8,
                max_login_attempts: 5,
                lockout_duration: 900,  // 15 minutes
                session_timeout: 86400, // 24 hours
                require_mfa: false,
                service_client_secret: "nano-bank-visa-network-secret-change-me".to_string(),
            },
            logging: LoggingSettings {
                level: "info".to_string(),
                format: "json".to_string(),
            },
            interac: InteracSettings::default(),
            lynx: LynxSettings::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_interac_settings_from_default_toml() {
        // Runs with cwd = crate root (api/), so config/default.toml is found.
        let s = Settings::new().expect("config should load");
        assert_eq!(s.interac.expiry_days, 30);
        assert_eq!(s.interac.max_etransfer_amount, Decimal::new(3000, 0));
    }

    #[test]
    fn loads_lynx_settings_from_default_toml() {
        let s = Settings::new().expect("config should load");
        assert_eq!(s.lynx.min_amount, Decimal::new(10000, 0));
        assert_eq!(s.lynx.stale_minutes, 60);
    }
}
