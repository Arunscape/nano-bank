pub mod database;

use config::{Config, ConfigError, Environment, File};
use serde::Deserialize;
use std::env;

#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub database: DatabaseSettings,
    pub server: ServerSettings,
    pub jwt: JwtSettings,
    pub security: SecuritySettings,
    pub logging: LoggingSettings,
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
        }
    }
}
