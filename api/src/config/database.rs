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

    // Self-heal the agentic-banking tables (canonical DDL: 11_agents.sql), so a
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
        // Phase 3: step-up pending approvals.
        r#"
        CREATE TABLE IF NOT EXISTS pending_approvals (
            approval_id     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            mandate_id      UUID NOT NULL REFERENCES mandates(mandate_id),
            agent_id        UUID NOT NULL REFERENCES agents(agent_id),
            customer_id     UUID NOT NULL REFERENCES customers(customer_id) ON DELETE CASCADE,
            account_id      UUID NOT NULL REFERENCES accounts(account_id),
            to_account_id   UUID NOT NULL,
            amount          DECIMAL(15,2) NOT NULL CHECK (amount > 0),
            description     TEXT NOT NULL,
            idempotency_key VARCHAR(128) NOT NULL,
            reason          TEXT NOT NULL,
            status          VARCHAR(20) NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending', 'executing', 'approved', 'declined', 'expired')),
            transaction_id  UUID,
            created_at      TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
            expires_at      TIMESTAMP WITH TIME ZONE NOT NULL,
            resolved_at     TIMESTAMP WITH TIME ZONE
        )
        "#,
        // The one-open-ask invariant covers pending AND executing (an ask being
        // executed must still swallow same-key retries). The old pending-only
        // index is dropped by its former name; the create is idempotent.
        "DROP INDEX IF EXISTS idx_pending_approvals_open_key",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_pending_approvals_open_ask \
         ON pending_approvals(mandate_id, idempotency_key) \
         WHERE status IN ('pending', 'executing')",
        "CREATE INDEX IF NOT EXISTS idx_pending_approvals_customer \
         ON pending_approvals(customer_id, created_at)",
        // Migrate DBs whose CHECK predates the transient 'executing' claim
        // state ('approved' is only ever written together with transaction_id).
        // DROP + re-ADD each boot: the pair is idempotent, and the ADD tolerates
        // a concurrent boot having re-added it first (duplicate_object).
        "ALTER TABLE pending_approvals \
         DROP CONSTRAINT IF EXISTS pending_approvals_status_check",
        r#"
        DO $$ BEGIN
            ALTER TABLE pending_approvals ADD CONSTRAINT pending_approvals_status_check
            CHECK (status IN ('pending', 'executing', 'approved', 'declined', 'expired'));
        EXCEPTION WHEN duplicate_object THEN NULL;
        END $$
        "#,
        // Lease marker for the 'executing' claim (timed reclaim of dead claims).
        "ALTER TABLE pending_approvals ADD COLUMN IF NOT EXISTS \
         claimed_at TIMESTAMP WITH TIME ZONE",
        // Saved Interac payees (address book). Self-heal for DBs predating the
        // 12_interac_recipients DDL, and migrate the old table-level UNIQUE to a
        // partial unique index so soft-deleted rows don't block re-registration.
        r#"
        CREATE TABLE IF NOT EXISTS interac_recipients (
            recipient_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            customer_id  UUID NOT NULL REFERENCES customers(customer_id) ON DELETE CASCADE,
            email        TEXT NOT NULL,
            display_name TEXT NOT NULL,
            status       TEXT NOT NULL DEFAULT 'active',
            created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
        )
        "#,
        "CREATE INDEX IF NOT EXISTS idx_interac_recipients_customer \
         ON interac_recipients(customer_id)",
        "ALTER TABLE interac_recipients \
         DROP CONSTRAINT IF EXISTS interac_recipients_customer_id_email_key",
        "CREATE UNIQUE INDEX IF NOT EXISTS uq_interac_recipients_active \
         ON interac_recipients(customer_id, email) WHERE status = 'active'",
    ] {
        sqlx::query(ddl).execute(pool).await?;
    }

    Ok(())
}

pub async fn health_check(pool: &DatabasePool) -> Result<(), SqlxError> {
    sqlx::query("SELECT 1").fetch_one(pool).await?;
    Ok(())
}
