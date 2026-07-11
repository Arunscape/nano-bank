-- Sender-side Interac saved payees (address book) registered per customer.
-- Distinct from the Interac rail's `interac_handles` (recipient-side autodeposit
-- registrations): this is a convenience list of recipients a customer sends to.
-- Sending money still goes through the rail (POST /api/v1/interac/etransfers).
CREATE TABLE IF NOT EXISTS interac_recipients (
    recipient_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    customer_id  UUID NOT NULL REFERENCES customers(customer_id) ON DELETE CASCADE,
    email        TEXT NOT NULL,
    display_name TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'active',   -- active | removed
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (customer_id, email)
);

CREATE INDEX IF NOT EXISTS idx_interac_recipients_customer
    ON interac_recipients(customer_id);
