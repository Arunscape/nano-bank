-- Nano Bank Core Database Schema
-- Part 7: Agentic banking — agents, mandates (user consent), and the agent action audit
--
-- An *agent* is a machine principal (an external AI assistant or the in-app
-- assistant) that acts on a customer's behalf. Access is never granted to the
-- agent directly: the customer creates a *mandate* — a scoped, limited,
-- expiring, revocable consent record binding (customer, agent, account).
-- Agent JWTs are pointers to a mandate; every request re-reads the row, so
-- revocation is immediate. Every policy decision (allow AND deny) is recorded
-- append-only in agent_actions.

-- Registered agents. Registration confers zero access — a mandate is the gate.
CREATE TABLE agents (
    agent_id     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    display_name VARCHAR(100) NOT NULL,
    description  TEXT,
    -- SHA-256 hex of the server-generated secret (high-entropy random, so a
    -- fast hash is fine — same rationale as user_sessions.session_token;
    -- argon2id remains for human passwords only).
    secret_hash  VARCHAR(64) NOT NULL,
    kind         VARCHAR(20) NOT NULL DEFAULT 'external'
                 CHECK (kind IN ('external', 'first_party')),
    -- Global kill switch for a compromised agent (checked on every request).
    status       VARCHAR(20) NOT NULL DEFAULT 'active'
                 CHECK (status IN ('active', 'disabled')),
    created_at   TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
);

-- The consent record — the single source of truth for what an agent may do.
-- Scopes/limits deliberately do NOT live in the agent JWT.
CREATE TABLE mandates (
    mandate_id      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    customer_id     UUID NOT NULL REFERENCES customers(customer_id) ON DELETE CASCADE,
    agent_id        UUID NOT NULL REFERENCES agents(agent_id),
    account_id      UUID NOT NULL REFERENCES accounts(account_id),
    -- 'read:balance' | 'read:transactions' | 'transfer:initiate'
    scopes          TEXT[] NOT NULL,
    -- Spend limits; required when 'transfer:initiate' is granted (API-enforced).
    max_per_tx      DECIMAL(15,2),
    daily_cap       DECIMAL(15,2),
    -- Velocity accounting, reset lazily on date rollover (account_limits pattern).
    daily_used      DECIMAL(15,2) NOT NULL DEFAULT 0,
    last_reset_date DATE NOT NULL DEFAULT CURRENT_DATE,
    status          VARCHAR(20) NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active', 'revoked', 'expired')),
    expires_at      TIMESTAMP WITH TIME ZONE NOT NULL,
    created_at      TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    revoked_at      TIMESTAMP WITH TIME ZONE,

    -- Constraints
    CONSTRAINT chk_mandate_expiry CHECK (expires_at > created_at),
    CONSTRAINT chk_mandate_revoked_logic CHECK (
        (status = 'revoked' AND revoked_at IS NOT NULL) OR
        (status <> 'revoked' AND revoked_at IS NULL)
    ),
    CONSTRAINT chk_mandate_daily_used CHECK (
        daily_used >= 0 AND (daily_cap IS NULL OR daily_used <= daily_cap)
    )
);

CREATE INDEX idx_mandates_customer_id ON mandates(customer_id);
CREATE INDEX idx_mandates_agent_id ON mandates(agent_id);
CREATE INDEX idx_mandates_active ON mandates(status, expires_at);

-- Append-only audit of every agent decision, including denials.
-- Never UPDATEd or DELETEd by the application.
CREATE TABLE agent_actions (
    action_id      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    mandate_id     UUID NOT NULL REFERENCES mandates(mandate_id),
    agent_id       UUID NOT NULL REFERENCES agents(agent_id),
    customer_id    UUID NOT NULL,
    account_id     UUID NOT NULL,
    -- 'token:issue' | 'read:balance' | 'read:transactions' | 'transfer'
    operation      VARCHAR(50) NOT NULL,
    amount         DECIMAL(15,2), -- NULL for reads
    decision       VARCHAR(20) NOT NULL
                   CHECK (decision IN ('allowed', 'denied', 'step_up_required')),
    reason         TEXT, -- machine-readable code (+ detail) on deny/step-up
    transaction_id UUID, -- resulting transaction when money moved
    created_at     TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE INDEX idx_agent_actions_mandate ON agent_actions(mandate_id, created_at);

-- Human consent events (grant/revoke) go to audit_logs with the user's session,
-- distinguishable from agent activity above.
ALTER TYPE audit_action ADD VALUE IF NOT EXISTS 'grant_mandate';
ALTER TYPE audit_action ADD VALUE IF NOT EXISTS 'revoke_mandate';
