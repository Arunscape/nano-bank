-- Nano Bank Core Database Schema
-- Part 8: Interac e-Transfer

CREATE TYPE interac_direction   AS ENUM ('outbound', 'inbound');
CREATE TYPE interac_handle_type AS ENUM ('email', 'phone');
CREATE TYPE interac_status AS ENUM (
    'initiated', 'held', 'available', 'deposited',
    'declined', 'cancelled', 'expired', 'failed'
);
CREATE TYPE interac_notification_kind AS ENUM (
    'incoming_transfer', 'deposit_completed', 'declined', 'cancelled', 'expired'
);

-- Handle registrations. A row maps an email/phone to a customer for inbound
-- routing; a non-null autodeposit_account_id means autodeposit is enabled.
CREATE TABLE interac_handles (
    handle_id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    customer_id            UUID NOT NULL REFERENCES customers(customer_id) ON DELETE CASCADE,
    handle_type            interac_handle_type NOT NULL,
    handle_value           VARCHAR(255) NOT NULL,
    autodeposit_account_id UUID REFERENCES accounts(account_id) ON DELETE SET NULL,
    active                 BOOLEAN NOT NULL DEFAULT TRUE,
    created_at             TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT uq_interac_handle_value UNIQUE (handle_value)
);

CREATE TABLE interac_etransfers (
    etransfer_id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    direction                interac_direction NOT NULL,
    status                   interac_status NOT NULL DEFAULT 'initiated',
    amount                   DECIMAL(15,2) NOT NULL,
    currency                 VARCHAR(3) NOT NULL DEFAULT 'CAD',
    sender_customer_id       UUID REFERENCES customers(customer_id),
    sender_account_id        UUID REFERENCES accounts(account_id),
    sender_name              VARCHAR(200),
    recipient_handle_type    interac_handle_type NOT NULL,
    recipient_handle_value   VARCHAR(255) NOT NULL,
    recipient_customer_id    UUID REFERENCES customers(customer_id),
    recipient_account_id     UUID REFERENCES accounts(account_id),
    counterparty_institution VARCHAR(3) REFERENCES rail_participants(institution_number),
    security_question        TEXT,
    security_answer_hash     TEXT,
    claim_token              VARCHAR(40) NOT NULL,
    memo                     TEXT,
    hold_transaction_id      UUID REFERENCES transactions(transaction_id),
    wrong_answer_attempts    INTEGER NOT NULL DEFAULT 0,
    idempotency_key          VARCHAR(255),
    expires_at               TIMESTAMP WITH TIME ZONE,
    created_at               TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    notified_at              TIMESTAMP WITH TIME ZONE,
    resolved_at              TIMESTAMP WITH TIME ZONE,
    CONSTRAINT chk_interac_amount_positive  CHECK (amount > 0),
    CONSTRAINT chk_interac_amount_precision CHECK (amount = ROUND(amount, 2)),
    CONSTRAINT chk_interac_currency_cad     CHECK (currency = 'CAD'),
    -- NULLs are distinct in Postgres, so unregistered/inbound (null key) never collide.
    CONSTRAINT uq_interac_idempotency UNIQUE (sender_customer_id, idempotency_key)
);
CREATE INDEX idx_interac_recipient_handle ON interac_etransfers (recipient_handle_value);
CREATE INDEX idx_interac_status           ON interac_etransfers (status);
CREATE INDEX idx_interac_sender           ON interac_etransfers (sender_customer_id);

-- Notification outbox: the simulator + viewer read this (no real email/SMS).
CREATE TABLE interac_notifications (
    notification_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    etransfer_id    UUID NOT NULL REFERENCES interac_etransfers(etransfer_id) ON DELETE CASCADE,
    handle_value    VARCHAR(255) NOT NULL,
    kind            interac_notification_kind NOT NULL,
    message         TEXT NOT NULL,
    claim_token     VARCHAR(40),
    delivered       BOOLEAN NOT NULL DEFAULT FALSE,
    created_at      TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
);
CREATE INDEX idx_interac_notifications_undelivered
    ON interac_notifications (delivered) WHERE delivered = FALSE;
