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
    -- Optional payee allowlist for transfers; NULL = any destination account.
    allowed_payees  UUID[],
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

-- Phase 3: step-up approvals. An over-cap agent transfer parks here instead of
-- hard-failing; the granting customer approves (the transfer executes — the
-- explicit consent overrides the amount caps for that one transfer; every
-- other check re-runs) or declines. Unresolved asks expire.
CREATE TABLE pending_approvals (
    approval_id     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    mandate_id      UUID NOT NULL REFERENCES mandates(mandate_id),
    agent_id        UUID NOT NULL REFERENCES agents(agent_id),
    customer_id     UUID NOT NULL REFERENCES customers(customer_id) ON DELETE CASCADE,
    account_id      UUID NOT NULL REFERENCES accounts(account_id),
    to_account_id   UUID NOT NULL,
    amount          DECIMAL(15,2) NOT NULL CHECK (amount > 0),
    description     TEXT NOT NULL,
    idempotency_key VARCHAR(128) NOT NULL,
    -- Which cap tripped: 'MAX_PER_TX_EXCEEDED' | 'DAILY_CAP_EXCEEDED'
    reason          TEXT NOT NULL,
    -- 'executing' is the transient claim state while the approved transfer
    -- posts: 'approved' is only ever written together with transaction_id, so
    -- an agent polling can treat approved as final (never approved-with-no-txn).
    status          VARCHAR(20) NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'executing', 'approved', 'declined', 'expired')),
    transaction_id  UUID, -- the executed transfer (approved only)
    -- Lease marker for the 'executing' claim: a crash mid-execution leaves the
    -- row claimable again once claimed_at ages past the reclaim window (the
    -- approve path finalizes by idempotency key, so re-approve can't double-pay).
    claimed_at      TIMESTAMP WITH TIME ZONE,
    created_at      TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    expires_at      TIMESTAMP WITH TIME ZONE NOT NULL,
    resolved_at     TIMESTAMP WITH TIME ZONE
);

-- An agent retry of the same request (mandate + idempotency key) maps onto the
-- same OPEN ask instead of stacking duplicates. "Open" = pending OR executing:
-- an ask being executed right now must still swallow retries, or a duplicate
-- row parked during the executing window could be approved concurrently and
-- double-pay.
CREATE UNIQUE INDEX idx_pending_approvals_open_ask
    ON pending_approvals(mandate_id, idempotency_key)
    WHERE status IN ('pending', 'executing');
CREATE INDEX idx_pending_approvals_customer ON pending_approvals(customer_id, created_at);
