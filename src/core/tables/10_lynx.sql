-- Nano Bank Core Database Schema — Part 10: Lynx RTGS high-value wire rail

CREATE TYPE lynx_direction     AS ENUM ('outbound', 'inbound');
CREATE TYPE lynx_wire_status   AS ENUM ('sent', 'settled', 'rejected', 'recalled');
CREATE TYPE lynx_recall_status AS ENUM ('requested', 'accepted', 'rejected');

CREATE TABLE lynx_wires (
    wire_id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    uetr                      UUID NOT NULL UNIQUE,            -- ISO 20022 end-to-end ref
    direction                 lynx_direction NOT NULL,
    status                    lynx_wire_status NOT NULL DEFAULT 'sent',
    local_account_id          UUID NOT NULL REFERENCES accounts(account_id),
    counterparty_name         VARCHAR(140) NOT NULL,
    counterparty_institution  VARCHAR(3) NOT NULL REFERENCES rail_participants(institution_number),
    counterparty_account      VARCHAR(34) NOT NULL,
    amount                    DECIMAL(19,4) NOT NULL,
    currency                  VARCHAR(3) NOT NULL DEFAULT 'CAD',
    remittance_info           VARCHAR(140),
    message_type              VARCHAR(12) NOT NULL DEFAULT 'pacs.008',
    settlement_transaction_id UUID REFERENCES transactions(transaction_id),
    gl_entry                  VARCHAR(120),
    initiated_by              UUID REFERENCES customers(customer_id),
    reference_number          VARCHAR(50) NOT NULL UNIQUE,
    created_at                TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    sent_at                   TIMESTAMP WITH TIME ZONE,
    settled_at                TIMESTAMP WITH TIME ZONE,
    CONSTRAINT chk_lynx_amount_positive  CHECK (amount > 0),
    CONSTRAINT chk_lynx_amount_precision CHECK (amount = ROUND(amount, 4)),
    CONSTRAINT chk_lynx_currency_cad     CHECK (currency = 'CAD')
);
CREATE INDEX idx_lynx_wires_status ON lynx_wires (status);
CREATE INDEX idx_lynx_wires_local  ON lynx_wires (local_account_id);
CREATE INDEX idx_lynx_wires_initiator ON lynx_wires (initiated_by);

CREATE TABLE lynx_messages (
    message_id   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    wire_id      UUID NOT NULL REFERENCES lynx_wires(wire_id) ON DELETE CASCADE,
    message_type VARCHAR(12) NOT NULL,     -- pacs.008 | pacs.009 | camt.056 | camt.029
    flow         VARCHAR(8)  NOT NULL,     -- emitted | received
    payload      TEXT NOT NULL,            -- the ISO 20022 XML
    created_at   TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT chk_lynx_msg_flow CHECK (flow IN ('emitted','received'))
);
CREATE INDEX idx_lynx_messages_wire ON lynx_messages (wire_id);

CREATE TABLE lynx_recalls (
    recall_id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    wire_id            UUID NOT NULL REFERENCES lynx_wires(wire_id) ON DELETE CASCADE,
    direction          lynx_direction NOT NULL,   -- who initiated the recall
    requested_by       UUID REFERENCES customers(customer_id),
    reason             VARCHAR(140),
    status             lynx_recall_status NOT NULL DEFAULT 'requested',
    resolution_reason  VARCHAR(140),
    camt056_message_id UUID REFERENCES lynx_messages(message_id),
    camt029_message_id UUID REFERENCES lynx_messages(message_id),
    created_at         TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    resolved_at        TIMESTAMP WITH TIME ZONE
);
CREATE INDEX idx_lynx_recalls_wire ON lynx_recalls (wire_id);
CREATE INDEX idx_lynx_recalls_status ON lynx_recalls (status);
