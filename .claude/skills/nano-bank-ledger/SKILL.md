---
name: nano-bank-ledger
description: Use when writing or reviewing any money-movement code in nano-bank (posting transactions, balances, GL, card or rail flows) — covers the double-entry invariants, the balance triggers, and the pluggable Ledger port.
---

# nano-bank double-entry ledger

The economic source of truth is the local double-entry ledger in Postgres.
Balances are trigger-maintained; the aggregate GL is posted through a pluggable
core (the Ledger port). Get these invariants wrong and the books drift or the
triggers reject the write.

## Core invariants

- **Post both legs in ONE multi-row INSERT** into `transaction_entries`.
  `trigger_validate_transaction_balance` (AFTER INSERT) checks
  `SUM(debits) = SUM(credits)` per transaction and raises if unbalanced —
  inserting legs one at a time trips it between rows. See `post_two_legged()` in
  `api/src/handlers/cards.rs`.
- **Never update `accounts.balance` directly.** `trigger_update_account_balance`
  (BEFORE INSERT on `transaction_entries`) mutates the balance and fills
  `balance_before` / `balance_after`. Debit → `balance_after = before − amount`;
  credit → `before + amount`.
- **GL / system accounts are keyed by `(customer_id, account_type)`, not
  `account_number`.** `trigger_generate_account_number` overwrites
  `account_number` with a random 12-digit value on insert, so system accounts
  cannot be looked up by number.

## System accounts

Internal accounts live under `system@nano.bank`, bootstrapped idempotently at
startup (`ensure_system_accounts()` in `cards.rs`) with a $1T overdraft so their
balances can float negative. Existing: `VISA_CLEARING`, `BANK_SETTLEMENT`.
Re-resolve their UUIDs per request (do not cache) — `testing/cleanup.sh` wipes
and rebuilds them.

## The Ledger port (kernel split)

Aggregate GL is posted through `api/src/ledger/` (trait `Ledger`) to one of two
interchangeable cores selected by `CORE_BACKEND`: `modern` (Rust, :8091) or
`legacy` (Java, :8090). The port speaks semantic accounts — `Bank`,
`Receivable`, `Payable`, `Revenue`, `Expense` — and each adapter maps them onto
its backend's numbering.

**Dual-post pattern:** money-movement code writes the local double-entry
subledger AND posts the aggregate GL through the port, inside one DB
transaction. If the core is down the whole operation returns 503 and rolls back
— the books never drift. Cards and the external rails both follow this; see the
`nano-bank-rails` skill.
