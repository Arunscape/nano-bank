-- Nano Bank Core Database Schema
-- Initialization Script

-- Enable required PostgreSQL extensions
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";
CREATE EXTENSION IF NOT EXISTS "pgcrypto";

-- Set timezone for the database
SET timezone = 'America/Toronto';

-- Create a schema for the banking system (optional, can use public)
-- CREATE SCHEMA IF NOT EXISTS banking;
-- SET search_path TO banking, public;

-- Script execution order:
-- 1. 01_enums.sql - Create all enum types
-- 2. 02_customers.sql - Customer and identity tables
-- 3. 03_accounts.sql - Account management tables
-- 4. 04_transactions.sql - Transaction processing tables
-- 5. 05_security.sql - Security and compliance tables
-- 6. 06_triggers.sql - Database triggers and functions
-- 7. 07_rails.sql - Payment-rail foundation (routing, participants, rail system accounts)
-- 8. 08_interac.sql - Interac e-Transfer tables (handles, etransfers, notifications)
-- 9. 09_aft.sql - AFT/EFT batch-rail tables (mandates, batches, entries)
-- 10. 10_lynx.sql - Lynx RTGS wire-rail tables (wires, messages, recalls)
-- 11. 11_agents.sql - Agentic banking: agents, mandates, agent action audit
-- 12. 11_lending.sql - Lending subsystem tables (loans details, status, triggers)

-- To execute all scripts in order:
-- \i 01_enums.sql
-- \i 02_customers.sql
-- \i 03_accounts.sql
-- \i 04_transactions.sql
-- \i 05_security.sql
-- \i 06_triggers.sql
-- \i 07_rails.sql
-- \i 08_interac.sql
-- \i 09_aft.sql
-- \i 10_lynx.sql
-- \i 11_agents.sql
-- \i 11_lending.sql
