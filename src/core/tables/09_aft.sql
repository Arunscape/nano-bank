-- Nano Bank Core Database Schema — Part 9: AFT / EFT batch rail

CREATE TYPE aft_entry_kind   AS ENUM ('credit', 'debit');
CREATE TYPE aft_direction    AS ENUM ('outbound', 'inbound');
CREATE TYPE aft_batch_status AS ENUM ('open', 'submitted', 'settled');
CREATE TYPE aft_entry_status AS ENUM ('pending', 'settled', 'returned', 'rejected');
CREATE TYPE mandate_status   AS ENUM ('active', 'revoked');

-- Pre-authorized debit mandates: a payer authorizes a biller to pull funds.
CREATE TABLE pad_mandates (
    mandate_id       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    payer_account_id UUID NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    biller_name      VARCHAR(200) NOT NULL,
    originator_id    VARCHAR(50) NOT NULL,          -- the biller's AFT originator id
    amount_cap       DECIMAL(15,2) NOT NULL,
    frequency        VARCHAR(20) NOT NULL DEFAULT 'monthly',
    status           mandate_status NOT NULL DEFAULT 'active',
    created_at       TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    revoked_at       TIMESTAMP WITH TIME ZONE,
    CONSTRAINT chk_mandate_cap_positive CHECK (amount_cap > 0)
);
CREATE INDEX idx_pad_mandates_payer ON pad_mandates (payer_account_id);

CREATE TABLE aft_batches (
    batch_id      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    direction     aft_direction NOT NULL DEFAULT 'outbound',
    status        aft_batch_status NOT NULL DEFAULT 'open',
    entry_count   INTEGER NOT NULL DEFAULT 0,
    total_credits DECIMAL(15,2) NOT NULL DEFAULT 0,
    total_debits  DECIMAL(15,2) NOT NULL DEFAULT 0,
    file_ref      TEXT,
    cutoff_at     TIMESTAMP WITH TIME ZONE,
    settled_at    TIMESTAMP WITH TIME ZONE,
    created_at    TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
);
CREATE INDEX idx_aft_batches_status ON aft_batches (status);

CREATE TABLE aft_entries (
    entry_id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    batch_id                 UUID NOT NULL REFERENCES aft_batches(batch_id) ON DELETE RESTRICT,
    kind                     aft_entry_kind NOT NULL,
    direction                aft_direction NOT NULL,
    originator_account_id    UUID REFERENCES accounts(account_id),   -- nano-bank side
    counterparty_account_id  UUID REFERENCES accounts(account_id),   -- set when internal
    counterparty_institution VARCHAR(3) REFERENCES rail_participants(institution_number),
    counterparty_transit     VARCHAR(5),
    counterparty_account     VARCHAR(12),
    payee_name               VARCHAR(200),
    amount                   DECIMAL(15,2) NOT NULL,
    mandate_id               UUID REFERENCES pad_mandates(mandate_id),
    status                   aft_entry_status NOT NULL DEFAULT 'pending',
    return_reason            VARCHAR(80),
    hold_transaction_id      UUID REFERENCES transactions(transaction_id),
    settle_transaction_id    UUID REFERENCES transactions(transaction_id),
    return_transaction_id    UUID REFERENCES transactions(transaction_id),
    created_at               TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT chk_aft_amount_positive CHECK (amount > 0),
    CONSTRAINT chk_aft_amount_precision CHECK (amount = ROUND(amount, 2))
);
CREATE INDEX idx_aft_entries_batch ON aft_entries (batch_id);
CREATE INDEX idx_aft_entries_status ON aft_entries (status);
CREATE INDEX idx_aft_entries_originator ON aft_entries (originator_account_id);
