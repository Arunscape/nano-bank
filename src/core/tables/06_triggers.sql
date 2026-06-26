-- Nano Bank Core Database Schema
-- Part 6: Triggers and Functions

-- Function to update updated_at timestamp
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = CURRENT_TIMESTAMP;
    RETURN NEW;
END;
$$ language 'plpgsql';

-- Function to generate account numbers
CREATE OR REPLACE FUNCTION generate_account_number()
RETURNS TRIGGER AS $$
DECLARE
    new_account_number VARCHAR(12);
    account_exists BOOLEAN;
BEGIN
    LOOP
        -- Generate a 12-digit account number
        new_account_number := LPAD(FLOOR(RANDOM() * 1000000000000)::TEXT, 12, '0');

        -- Check if account number already exists
        SELECT EXISTS(SELECT 1 FROM accounts WHERE account_number = new_account_number) INTO account_exists;

        -- Exit loop if unique
        IF NOT account_exists THEN
            EXIT;
        END IF;
    END LOOP;

    NEW.account_number := new_account_number;
    RETURN NEW;
END;
$$ language 'plpgsql';

-- Function to generate transaction reference numbers
CREATE OR REPLACE FUNCTION generate_transaction_reference()
RETURNS TRIGGER AS $$
DECLARE
    new_reference VARCHAR(20);
    reference_exists BOOLEAN;
BEGIN
    LOOP
        -- Generate a 16-character alphanumeric reference
        new_reference := 'TXN' || UPPER(SUBSTRING(MD5(RANDOM()::TEXT) FROM 1 FOR 13));

        -- Check if reference already exists
        SELECT EXISTS(SELECT 1 FROM transactions WHERE reference_number = new_reference) INTO reference_exists;

        -- Exit loop if unique
        IF NOT reference_exists THEN
            EXIT;
        END IF;
    END LOOP;

    NEW.reference_number := new_reference;
    RETURN NEW;
END;
$$ language 'plpgsql';

-- Function to validate double-entry bookkeeping
CREATE OR REPLACE FUNCTION validate_transaction_balance()
RETURNS TRIGGER AS $$
DECLARE
    total_debits DECIMAL(15,2);
    total_credits DECIMAL(15,2);
BEGIN
    -- Calculate total debits and credits for the transaction
    SELECT
        COALESCE(SUM(CASE WHEN entry_type = 'debit' THEN amount ELSE 0 END), 0),
        COALESCE(SUM(CASE WHEN entry_type = 'credit' THEN amount ELSE 0 END), 0)
    INTO total_debits, total_credits
    FROM transaction_entries
    WHERE transaction_id = COALESCE(NEW.transaction_id, OLD.transaction_id);

    -- Ensure debits equal credits
    IF total_debits != total_credits THEN
        RAISE EXCEPTION 'Transaction entries must balance: debits=%, credits=%', total_debits, total_credits;
    END IF;

    RETURN COALESCE(NEW, OLD);
END;
$$ language 'plpgsql';

-- Function to update account balance after transaction entries
CREATE OR REPLACE FUNCTION update_account_balance()
RETURNS TRIGGER AS $$
DECLARE
    current_balance DECIMAL(15,2);
    new_balance DECIMAL(15,2);
BEGIN
    -- Get current account balance
    SELECT balance INTO current_balance FROM accounts WHERE account_id = NEW.account_id;

    -- Calculate new balance
    IF NEW.entry_type = 'credit' THEN
        new_balance := current_balance + NEW.amount;
    ELSE
        new_balance := current_balance - NEW.amount;
    END IF;

    -- Update account balance
    UPDATE accounts
    SET balance = new_balance, 
        available_balance = new_balance + overdraft_limit - COALESCE((SELECT sum(amount) FROM account_holds WHERE account_id = NEW.account_id AND released_at IS NULL), 0),
        updated_at = CURRENT_TIMESTAMP
    WHERE account_id = NEW.account_id;

    -- Update the balance_after in the entry
    NEW.balance_before := current_balance;
    NEW.balance_after := new_balance;

    RETURN NEW;
END;
$$ language 'plpgsql';

-- Function to log audit entries
CREATE OR REPLACE FUNCTION log_audit_entry()
RETURNS TRIGGER AS $$
DECLARE
    table_name TEXT;
    primary_key_column TEXT;
    primary_key_value UUID;
    action_type audit_action;
BEGIN
    -- Determine table name
    table_name := TG_TABLE_NAME;

    -- Determine action type
    IF TG_OP = 'INSERT' THEN
        action_type := 'create';
    ELSIF TG_OP = 'UPDATE' THEN
        action_type := 'update';
    ELSIF TG_OP = 'DELETE' THEN
        action_type := 'delete';
    END IF;

    -- Get primary key value
    CASE table_name
        WHEN 'customers' THEN
            primary_key_value := COALESCE(NEW.customer_id, OLD.customer_id);
        WHEN 'accounts' THEN
            primary_key_value := COALESCE(NEW.account_id, OLD.account_id);
        WHEN 'transactions' THEN
            primary_key_value := COALESCE(NEW.transaction_id, OLD.transaction_id);
        WHEN 'transaction_entries' THEN
            primary_key_value := COALESCE(NEW.entry_id, OLD.entry_id);
        ELSE
            primary_key_value := NULL;
    END CASE;

    -- Insert audit log entry
    INSERT INTO audit_logs (
        entity_type,
        entity_id,
        action,
        old_values,
        new_values,
        created_at
    ) VALUES (
        table_name,
        primary_key_value,
        action_type,
        CASE WHEN TG_OP != 'INSERT' THEN row_to_json(OLD) ELSE NULL END,
        CASE WHEN TG_OP != 'DELETE' THEN row_to_json(NEW) ELSE NULL END,
        CURRENT_TIMESTAMP
    );

    RETURN COALESCE(NEW, OLD);
END;
$$ language 'plpgsql';

-- Function to enforce primary address constraint
CREATE OR REPLACE FUNCTION enforce_single_primary_address()
RETURNS TRIGGER AS $$
BEGIN
    -- If setting an address as primary, unset others for the same customer and type
    IF NEW.is_primary = TRUE THEN
        UPDATE customer_addresses
        SET is_primary = FALSE, updated_at = CURRENT_TIMESTAMP
        WHERE customer_id = NEW.customer_id
          AND address_type = NEW.address_type
          AND address_id != NEW.address_id;
    END IF;

    RETURN NEW;
END;
$$ language 'plpgsql';

-- Apply updated_at triggers to relevant tables
CREATE TRIGGER trigger_customers_updated_at
    BEFORE UPDATE ON customers
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER trigger_customer_addresses_updated_at
    BEFORE UPDATE ON customer_addresses
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER trigger_accounts_updated_at
    BEFORE UPDATE ON accounts
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER trigger_account_limits_updated_at
    BEFORE UPDATE ON account_limits
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- Apply account number generation trigger
CREATE TRIGGER trigger_generate_account_number
    BEFORE INSERT ON accounts
    FOR EACH ROW EXECUTE FUNCTION generate_account_number();

-- Apply transaction reference generation trigger
CREATE TRIGGER trigger_generate_transaction_reference
    BEFORE INSERT ON transactions
    FOR EACH ROW
    WHEN (NEW.reference_number IS NULL)
    EXECUTE FUNCTION generate_transaction_reference();

-- Apply balance update trigger
CREATE TRIGGER trigger_update_account_balance
    BEFORE INSERT ON transaction_entries
    FOR EACH ROW EXECUTE FUNCTION update_account_balance();

-- Apply double-entry validation trigger (after insert/update/delete)
CREATE TRIGGER trigger_validate_transaction_balance
    AFTER INSERT OR UPDATE OR DELETE ON transaction_entries
    FOR EACH ROW EXECUTE FUNCTION validate_transaction_balance();

-- Apply audit logging triggers
CREATE TRIGGER trigger_customers_audit
    AFTER INSERT OR UPDATE OR DELETE ON customers
    FOR EACH ROW EXECUTE FUNCTION log_audit_entry();

CREATE TRIGGER trigger_accounts_audit
    AFTER INSERT OR UPDATE OR DELETE ON accounts
    FOR EACH ROW EXECUTE FUNCTION log_audit_entry();

CREATE TRIGGER trigger_transactions_audit
    AFTER INSERT OR UPDATE OR DELETE ON transactions
    FOR EACH ROW EXECUTE FUNCTION log_audit_entry();

CREATE TRIGGER trigger_transaction_entries_audit
    AFTER INSERT OR UPDATE OR DELETE ON transaction_entries
    FOR EACH ROW EXECUTE FUNCTION log_audit_entry();

-- Apply primary address enforcement trigger
CREATE TRIGGER trigger_enforce_single_primary_address
    BEFORE INSERT OR UPDATE ON customer_addresses
    FOR EACH ROW EXECUTE FUNCTION enforce_single_primary_address();