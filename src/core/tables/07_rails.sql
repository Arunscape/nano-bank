-- Nano Bank Core Database Schema
-- Part 7: Payment rail foundation (Canadian routing + external participants)

ALTER TABLE accounts
    ADD COLUMN institution_number VARCHAR(3) NOT NULL DEFAULT '900',
    ADD COLUMN transit_number     VARCHAR(5) NOT NULL DEFAULT '00001';

ALTER TABLE accounts
    ADD CONSTRAINT chk_institution_number_format CHECK (institution_number ~ '^[0-9]{3}$'),
    ADD CONSTRAINT chk_transit_number_format     CHECK (transit_number ~ '^[0-9]{5}$');

-- External institutions nano-bank can settle against. Interac routes by handle,
-- but a claimed external transfer records which participant it settled with.
CREATE TABLE rail_participants (
    institution_number VARCHAR(3) PRIMARY KEY,
    name               VARCHAR(100) NOT NULL,
    is_self            BOOLEAN NOT NULL DEFAULT FALSE,
    supports_interac   BOOLEAN NOT NULL DEFAULT TRUE,
    supports_aft       BOOLEAN NOT NULL DEFAULT TRUE,
    supports_lynx      BOOLEAN NOT NULL DEFAULT FALSE,
    active             BOOLEAN NOT NULL DEFAULT TRUE,
    created_at         TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT chk_participant_institution_format CHECK (institution_number ~ '^[0-9]{3}$')
);

INSERT INTO rail_participants (institution_number, name, is_self, supports_lynx) VALUES
    ('900', 'nano-bank',                            TRUE,  TRUE),
    ('001', 'Bank of Montreal',                     FALSE, TRUE),
    ('002', 'Scotiabank',                           FALSE, TRUE),
    ('003', 'Royal Bank of Canada',                 FALSE, TRUE),
    ('004', 'Toronto-Dominion Bank',                FALSE, TRUE),
    ('010', 'Canadian Imperial Bank of Commerce',   FALSE, TRUE)
ON CONFLICT (institution_number) DO NOTHING;
