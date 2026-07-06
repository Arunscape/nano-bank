use crate::config::Settings;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Error as SqlxError;
use std::time::Duration;
use tracing::{info, warn};

pub type DatabasePool = PgPool;

pub async fn create_connection_pool(settings: &Settings) -> Result<DatabasePool, SqlxError> {
    info!("Creating database connection pool...");

    let database_url = settings.database_url();

    let pool = PgPoolOptions::new()
        .max_connections(settings.database.max_connections)
        .min_connections(settings.database.min_connections)
        .acquire_timeout(Duration::from_secs(settings.database.acquire_timeout))
        .idle_timeout(Duration::from_secs(600)) // 10 minutes
        .max_lifetime(Duration::from_secs(1800)) // 30 minutes
        .connect(&database_url)
        .await?;

    // Test the connection
    match sqlx::query("SELECT 1").fetch_one(&pool).await {
        Ok(_) => {
            info!("Database connection pool created successfully");
            info!("Connected to database: {}", settings.database.database_name);
        }
        Err(e) => {
            warn!("Failed to test database connection: {}", e);
            return Err(e);
        }
    }

    Ok(pool)
}

pub async fn run_migrations(pool: &DatabasePool) -> Result<(), sqlx::Error> {
    info!("Running database migrations...");

    // Note: In a real application, you would run actual migrations here
    // For now, we'll just verify that the tables exist
    let table_check = sqlx::query("SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'customers'")
        .fetch_optional(pool)
        .await?;

    match table_check {
        Some(_) => {
            info!("Database tables found - migrations appear to be complete");
        }
        None => {
            warn!("Database tables not found - please run the SQL scripts manually");
            warn!("Run the scripts in ~/dev/nano-bank/src/core/tables/ in order");
        }
    }

    // Self-heal the auth credentials table. The canonical DDL lives in
    // src/core/tables/02_customers.sql for fresh deploys, but issuing it here
    // (idempotently) means a DB initialised before auth existed picks up the
    // table on the next `cargo run` without a redeploy — same pattern as
    // handlers::cards::ensure_system_accounts.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS customer_credentials (
            customer_id UUID PRIMARY KEY REFERENCES customers(customer_id) ON DELETE CASCADE,
            password_hash VARCHAR(255) NOT NULL,
            password_changed_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
            created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Self-heal the agentic-banking tables (canonical DDL: 07_agents.sql), so a
    // DB initialised before the agent plane existed picks them up on next boot.
    // Statements run one at a time: ALTER TYPE ... ADD VALUE can't share a
    // transaction with other statements.
    for ddl in [
        r#"
        CREATE TABLE IF NOT EXISTS agents (
            agent_id     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            display_name VARCHAR(100) NOT NULL,
            description  TEXT,
            secret_hash  VARCHAR(64) NOT NULL,
            kind         VARCHAR(20) NOT NULL DEFAULT 'external'
                         CHECK (kind IN ('external', 'first_party')),
            status       VARCHAR(20) NOT NULL DEFAULT 'active'
                         CHECK (status IN ('active', 'disabled')),
            created_at   TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
        )
        "#,
        r#"
        CREATE TABLE IF NOT EXISTS mandates (
            mandate_id      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            customer_id     UUID NOT NULL REFERENCES customers(customer_id) ON DELETE CASCADE,
            agent_id        UUID NOT NULL REFERENCES agents(agent_id),
            account_id      UUID NOT NULL REFERENCES accounts(account_id),
            scopes          TEXT[] NOT NULL,
            max_per_tx      DECIMAL(15,2),
            daily_cap       DECIMAL(15,2),
            allowed_payees  UUID[],
            daily_used      DECIMAL(15,2) NOT NULL DEFAULT 0,
            last_reset_date DATE NOT NULL DEFAULT CURRENT_DATE,
            status          VARCHAR(20) NOT NULL DEFAULT 'active'
                            CHECK (status IN ('active', 'revoked', 'expired')),
            expires_at      TIMESTAMP WITH TIME ZONE NOT NULL,
            created_at      TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
            revoked_at      TIMESTAMP WITH TIME ZONE,
            CONSTRAINT chk_mandate_expiry CHECK (expires_at > created_at),
            CONSTRAINT chk_mandate_revoked_logic CHECK (
                (status = 'revoked' AND revoked_at IS NOT NULL) OR
                (status <> 'revoked' AND revoked_at IS NULL)
            ),
            CONSTRAINT chk_mandate_daily_used CHECK (
                daily_used >= 0 AND (daily_cap IS NULL OR daily_used <= daily_cap)
            )
        )
        "#,
        "CREATE INDEX IF NOT EXISTS idx_mandates_customer_id ON mandates(customer_id)",
        "CREATE INDEX IF NOT EXISTS idx_mandates_agent_id ON mandates(agent_id)",
        "CREATE INDEX IF NOT EXISTS idx_mandates_active ON mandates(status, expires_at)",
        r#"
        CREATE TABLE IF NOT EXISTS agent_actions (
            action_id      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            mandate_id     UUID NOT NULL REFERENCES mandates(mandate_id),
            agent_id       UUID NOT NULL REFERENCES agents(agent_id),
            customer_id    UUID NOT NULL,
            account_id     UUID NOT NULL,
            operation      VARCHAR(50) NOT NULL,
            amount         DECIMAL(15,2),
            decision       VARCHAR(20) NOT NULL
                           CHECK (decision IN ('allowed', 'denied', 'step_up_required')),
            reason         TEXT,
            transaction_id UUID,
            created_at     TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
        )
        "#,
        "CREATE INDEX IF NOT EXISTS idx_agent_actions_mandate \
         ON agent_actions(mandate_id, created_at)",
        "ALTER TYPE audit_action ADD VALUE IF NOT EXISTS 'grant_mandate'",
        "ALTER TYPE audit_action ADD VALUE IF NOT EXISTS 'revoke_mandate'",
        // Additive: DBs whose mandates table predates the Phase-2 payee allowlist.
        "ALTER TABLE mandates ADD COLUMN IF NOT EXISTS allowed_payees UUID[]",
    ] {
        sqlx::query(ddl).execute(pool).await?;
    }

    Ok(())
}

pub async fn health_check(pool: &DatabasePool) -> Result<(), SqlxError> {
    sqlx::query("SELECT 1").fetch_one(pool).await?;
    Ok(())
}
