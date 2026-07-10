-- Nano Bank Core Database Schema
-- Part 11: Lending Subsystem Tables

-- 1. Extend the account_type enum to include 'loan'
ALTER TYPE account_type ADD VALUE 'loan';

-- 2. Create the loans table to track loan accounts details
CREATE TABLE loans (
    loan_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    customer_id UUID NOT NULL REFERENCES customers(customer_id) ON DELETE RESTRICT,
    account_id UUID NOT NULL REFERENCES accounts(account_id) ON DELETE RESTRICT,
    
    principal_amount DECIMAL(15,2) NOT NULL,
    interest_rate DECIMAL(5,4) NOT NULL,
    amortization_months INTEGER NOT NULL,
    monthly_payment DECIMAL(15,2) NOT NULL,
    
    status VARCHAR(50) DEFAULT 'pending_disbursement' NOT NULL,
    
    next_payment_date DATE NOT NULL,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    
    -- Constraints
    CONSTRAINT chk_principal_positive CHECK (principal_amount > 0),
    CONSTRAINT chk_interest_rate_range CHECK (interest_rate >= 0 AND interest_rate <= 1),
    CONSTRAINT chk_amortization_positive CHECK (amortization_months > 0),
    CONSTRAINT chk_monthly_payment_positive CHECK (monthly_payment > 0),
    CONSTRAINT chk_status CHECK (status IN ('pending_disbursement', 'active', 'closed', 'defaulted'))
);

-- Indexing for lookup performance
CREATE INDEX idx_loans_customer ON loans(customer_id);
CREATE INDEX idx_loans_account ON loans(account_id);
CREATE INDEX idx_loans_status ON loans(status);

-- Apply updated_at trigger
CREATE TRIGGER trigger_loans_updated_at
    BEFORE UPDATE ON loans
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();
