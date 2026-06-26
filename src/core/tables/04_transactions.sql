-- Nano Bank Core Database Schema
-- Part 4: Transaction Processing Tables

-- Core transaction records
CREATE TABLE transactions (
    transaction_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    reference_number VARCHAR(20) UNIQUE NOT NULL, -- External reference for tracking
    transaction_type VARCHAR(50) NOT NULL, -- 'transfer', 'deposit', 'withdrawal', 'fee', etc.
    amount DECIMAL(15,2) NOT NULL,
    currency VARCHAR(3) DEFAULT 'CAD' NOT NULL,
    description TEXT,
    status transaction_status DEFAULT 'pending' NOT NULL,
    initiated_by UUID REFERENCES customers(customer_id),
    external_reference VARCHAR(100), -- For external system references
    metadata JSONB, -- Flexible field for additional transaction data
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    processed_at TIMESTAMP WITH TIME ZONE,
    completed_at TIMESTAMP WITH TIME ZONE,
    failed_at TIMESTAMP WITH TIME ZONE,
    failure_reason TEXT,

    -- Constraints
    CONSTRAINT chk_amount_positive CHECK (amount > 0),
    CONSTRAINT chk_amount_precision CHECK (amount = ROUND(amount, 2)),
    CONSTRAINT chk_currency_cad CHECK (currency = 'CAD'),
    CONSTRAINT chk_reference_format CHECK (reference_number ~ '^[A-Z0-9]{10,20}$'),
    CONSTRAINT chk_status_timestamps CHECK (
        (status = 'completed' AND completed_at IS NOT NULL) OR
        (status = 'failed' AND failed_at IS NOT NULL) OR
        (status IN ('pending', 'cancelled', 'reversed'))
    ),
    UNIQUE(initiated_by, external_reference)
);

-- Double-entry bookkeeping entries
CREATE TABLE transaction_entries (
    entry_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    transaction_id UUID NOT NULL REFERENCES transactions(transaction_id) ON DELETE RESTRICT,
    account_id UUID NOT NULL REFERENCES accounts(account_id) ON DELETE RESTRICT,
    entry_type entry_type NOT NULL,
    amount DECIMAL(15,2) NOT NULL,
    balance_before DECIMAL(15,2) NOT NULL,
    balance_after DECIMAL(15,2) NOT NULL,
    entry_order INTEGER NOT NULL, -- Order of entries within transaction
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,

    -- Constraints
    CONSTRAINT chk_entry_amount_positive CHECK (amount > 0),
    CONSTRAINT chk_entry_amount_precision CHECK (amount = ROUND(amount, 2)),
    CONSTRAINT chk_balance_precision CHECK (
        balance_before = ROUND(balance_before, 2) AND
        balance_after = ROUND(balance_after, 2)
    ),
    CONSTRAINT chk_balance_calculation CHECK (
        (entry_type = 'credit' AND balance_after = balance_before + amount) OR
        (entry_type = 'debit' AND balance_after = balance_before - amount)
    ),
    CONSTRAINT chk_entry_order_positive CHECK (entry_order > 0)
);

-- Transaction reversals
CREATE TABLE transaction_reversals (
    reversal_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    original_transaction_id UUID NOT NULL REFERENCES transactions(transaction_id) ON DELETE RESTRICT,
    reversal_transaction_id UUID NOT NULL REFERENCES transactions(transaction_id) ON DELETE RESTRICT,
    reason TEXT NOT NULL,
    authorized_by UUID REFERENCES customers(customer_id),
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,

    -- Constraints
    CONSTRAINT chk_different_transactions CHECK (original_transaction_id != reversal_transaction_id)
);

-- Transaction fees
CREATE TABLE transaction_fees (
    fee_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    transaction_id UUID NOT NULL REFERENCES transactions(transaction_id) ON DELETE RESTRICT,
    fee_type VARCHAR(50) NOT NULL, -- 'overdraft', 'transfer', 'foreign_exchange', etc.
    fee_amount DECIMAL(15,2) NOT NULL,
    fee_percentage DECIMAL(5,4), -- If fee is percentage-based
    waived BOOLEAN DEFAULT FALSE NOT NULL,
    waived_reason TEXT,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,

    -- Constraints
    CONSTRAINT chk_fee_amount_positive CHECK (fee_amount >= 0),
    CONSTRAINT chk_fee_amount_precision CHECK (fee_amount = ROUND(fee_amount, 2)),
    CONSTRAINT chk_fee_percentage CHECK (fee_percentage IS NULL OR (fee_percentage >= 0 AND fee_percentage <= 1))
);

-- Daily transaction summaries (for limits and reporting)
CREATE TABLE daily_transaction_summaries (
    summary_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    summary_date DATE NOT NULL,
    total_debits DECIMAL(15,2) DEFAULT 0.00 NOT NULL,
    total_credits DECIMAL(15,2) DEFAULT 0.00 NOT NULL,
    transaction_count INTEGER DEFAULT 0 NOT NULL,
    largest_debit DECIMAL(15,2) DEFAULT 0.00,
    largest_credit DECIMAL(15,2) DEFAULT 0.00,
    end_of_day_balance DECIMAL(15,2) NOT NULL,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,

    -- Constraints
    CONSTRAINT chk_summary_amounts_precision CHECK (
        total_debits = ROUND(total_debits, 2) AND
        total_credits = ROUND(total_credits, 2) AND
        end_of_day_balance = ROUND(end_of_day_balance, 2)
    ),
    CONSTRAINT chk_transaction_count_positive CHECK (transaction_count >= 0),
    UNIQUE(account_id, summary_date)
);

-- Indexes for transactions table
CREATE INDEX idx_transactions_reference ON transactions(reference_number);
CREATE INDEX idx_transactions_status ON transactions(status);
CREATE INDEX idx_transactions_type ON transactions(transaction_type);
CREATE INDEX idx_transactions_initiated_by ON transactions(initiated_by);
CREATE INDEX idx_transactions_created_at ON transactions(created_at);
CREATE INDEX idx_transactions_external_ref ON transactions(external_reference);

-- Indexes for transaction_entries table
CREATE INDEX idx_transaction_entries_transaction_id ON transaction_entries(transaction_id);
CREATE INDEX idx_transaction_entries_account_id ON transaction_entries(account_id);
CREATE INDEX idx_transaction_entries_created_at ON transaction_entries(created_at);
CREATE UNIQUE INDEX idx_transaction_entries_order ON transaction_entries(transaction_id, entry_order);

-- Indexes for transaction_reversals table
CREATE INDEX idx_transaction_reversals_original ON transaction_reversals(original_transaction_id);
CREATE INDEX idx_transaction_reversals_reversal ON transaction_reversals(reversal_transaction_id);

-- Indexes for transaction_fees table
CREATE INDEX idx_transaction_fees_transaction_id ON transaction_fees(transaction_id);
CREATE INDEX idx_transaction_fees_type ON transaction_fees(fee_type);

-- Indexes for daily_transaction_summaries table
CREATE INDEX idx_daily_summaries_account_date ON daily_transaction_summaries(account_id, summary_date);
CREATE INDEX idx_daily_summaries_date ON daily_transaction_summaries(summary_date);