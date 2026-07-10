---
name: nano-bank-rails
description: Use when working on nano-bank external payment rails — Interac e-Transfer, AFT/EFT, or Lynx — including the Rail port, clearing/settlement accounts, and the e-Transfer lifecycle.
---

# nano-bank payment rails

External payment rails (Interac, AFT, Lynx) move money to and from other
institutions. They sit BESIDE the Ledger port (see the `nano-bank-ledger`
skill), not on top of it: a rail owns the local double-entry (customer account ↔
its clearing / settlement system accounts) AND fires the aggregate GL post
through the Ledger port.

Design spec: `docs/specs/2026-07-04-interac-rail-foundation-design.md`.

## The Rail port

`api/src/rails/` defines a `Rail` trait scoped to the clearing/settlement
plumbing common to every rail; the product lifecycle stays in the handler. Verbs:

- `hold` — reserve funds from a customer account into the rail's clearing
  account (outbound).
- `release` — release a hold to a `Destination`: `Internal(account)` (nano-bank
  recipient) or `External(institution)` (settles through the settlement account).
- `refund` — return a hold to its origin (decline / cancel / expire).
- `accept_inbound` — credit an incoming payment from the network into a customer
  account.

Each method takes `&mut PgTx` so the local legs and the GL post commit or roll
back together (503 + rollback if the core is down).

## Two-account settlement

Each rail has two system accounts (mirroring the card rails'
`VISA_CLEARING` / `BANK_SETTLEMENT`): a CLEARING account holds in-flight funds; a
SETTLEMENT account carries the interbank position vs the network. Interac:
`INTERAC_CLEARING`, `INTERAC_SETTLEMENT`. The settlement position stands as an
inspectable balance; the ACSS-style sweep to `Bank` cash lands with the AFT rail.

## Interac e-Transfer

Handle-addressed (email / phone). Lifecycle: send → held in clearing →
autodeposit (registered handle) OR claim (security Q&A, argon2-hashed, 3-strike
lock) OR decline / cancel / expire (refund to sender). Counterparty resolution: a
registered nano-bank handle → internal deposit; otherwise external via the
simulator. Notifications are written to an outbox table read by the simulator and
the viewer. Three auth planes: customer, service-token `/interac/network/*` (the
simulator plays the network), and service-token `/interac/admin/*`.

## PR #15 coexistence

Do NOT edit `handlers/transactions.rs` (heavily rewritten by open PR #15). Rails
write their own `transactions` / `transaction_entries` rows via `pub(crate)`
helpers promoted from `cards.rs` (`post_two_legged`, `ensure_system_accounts`).
