# Design: Payment Rail Foundation + Interac e-Transfer

**Status:** Approved for planning
**Date:** 2026-07-04
**Author:** nano-bank team
**Scope:** Spec 1 of a multi-rail programme (Interac → AFT → Lynx)

## 1. Context and goals

nano-bank currently moves money over two internal mechanisms: the card payment
rails (`handlers/cards.rs`) and basic transactions (deposit / withdrawal /
transfer). It has no **external payment rails** — the systems a Canadian bank
uses to move money to and from *other* institutions.

This programme adds the three rails a Canadian challenger bank actually touches:

- **Interac e-Transfer** — consumer person-to-person transfers addressed by
  email or phone.
- **AFT / EFT** — the batch, ACH-like rail (direct deposit, pre-authorised
  debit).
- **Lynx** — real-time gross settlement for high-value, irrevocable wires.

Each rail is a distinct subsystem and gets its own design → plan →
implementation cycle. **This document covers spec 1 only:** a shared rail
foundation plus Interac e-Transfer built on top of it, delivered together so the
foundation is validated by a real consumer rather than designed in a vacuum.
AFT and Lynx are explicitly out of scope here and reuse this foundation.

### Non-goals

- AFT and Lynx rails (later specs).
- A real ACSS clearing/settlement cycle that sweeps the interbank position to
  cash — the position simply stands as an inspectable balance in this spec; the
  sweep lands with AFT, which needs the same machinery.
- Real email / SMS delivery. Notifications are persisted to an outbox table and
  read by the test simulator and the Streamlit viewer.
- Shared `account_limits` integration for e-Transfers (deferred; see §8).

## 2. Design decisions

Two decisions were made deliberately during design review:

1. **A `Rail` port is introduced now**, parallel to the existing `Ledger` port,
   rather than deferring the abstraction until a second rail exists. The trait is
   scoped to the *clearing / settlement plumbing* that is genuinely common across
   Interac, AFT and Lynx; each rail's product lifecycle stays in its own module
   so the trait does not become Interac-shaped. See §4.

2. **Two-account settlement model.** Held funds sit in an `INTERAC_CLEARING`
   account; the interbank position moves through a separate `INTERAC_SETTLEMENT`
   account (the analogue of the card rails' `BANK_SETTLEMENT`). This produces a
   real, inspectable "owed to / from the network" position and establishes the
   settlement concept AFT and Lynx will both reuse. See §5.

## 3. Architecture overview

```
api/src/rails/                 NEW foundation
  mod.rs        Rail trait, neutral types (RailId, Hold, RailPosting, RailError),
                Destination, system-account helper
  interac.rs    InteracRail: impl Rail  (clearing/settlement plumbing only)

api/src/handlers/
  interac.rs    NEW  e-Transfer product lifecycle + HTTP handlers
  cards.rs      post_two_legged / ensure_system_accounts / reference-number gen
                promoted to pub(crate) and reused (no behavioural change)
  ledger.rs     unchanged (Rail adapters post aggregate GL through the Ledger port)

api/src/models/interac.rs   NEW  request/response/entity types

src/core/tables/
  07_rails.sql   NEW  routing foundation + system accounts
  08_interac.sql NEW  Interac tables + enums
```

A `Rail` sits **beside** the `Ledger` port, not on top of it. A rail owns the
**local double-entry** (customer account ↔ its clearing / settlement system
accounts, kept correct by the existing balance triggers) **and** fires the
**aggregate GL** post through the `Ledger` port — the same dual-post pattern the
card rails already use. Both legs and the GL post run inside one database
transaction: if the accounting core is unavailable the whole operation returns
`503` and rolls back, so the books never drift.

## 4. The `Rail` port

The common vocabulary across all three rails is "move money through this rail's
clearing and settlement accounts, dual-posted." Product lifecycle (claim /
decline / expiry for Interac; batch cutoffs for AFT; settlement finality for
Lynx) stays out of the trait, in the handler.

```rust
#[async_trait]
pub trait Rail: Send + Sync {
    fn id(&self) -> RailId;                       // Interac | Aft | Lynx
    fn clearing_account(&self) -> &'static str;   // "INTERAC_CLEARING"
    fn settlement_account(&self) -> &'static str; // "INTERAC_SETTLEMENT"

    /// Outbound: reserve funds from a customer account into clearing (a "hold").
    async fn hold(&self, tx: &mut PgTx, from: Uuid, amount: Decimal, r#ref: &str)
        -> Result<Hold, RailError>;

    /// Release a hold to its destination: an internal customer account, or
    /// external settlement.
    async fn release(&self, tx: &mut PgTx, hold: &Hold, dest: Destination)
        -> Result<RailPosting, RailError>;

    /// Return a hold to its origin (decline / cancel / expire).
    async fn refund(&self, tx: &mut PgTx, hold: &Hold)
        -> Result<RailPosting, RailError>;

    /// Inbound: accept an incoming credit from the network into a customer account.
    async fn accept_inbound(&self, tx: &mut PgTx, to: Uuid, amount: Decimal, r#ref: &str)
        -> Result<RailPosting, RailError>;
}

pub enum Destination {
    Internal(Uuid),        // recipient is a nano-bank customer account
    External(String),      // settles through settlement acct vs a directory participant
}
```

Each method runs inside the caller's DB transaction so the local legs and the GL
post commit or roll back together. The four verbs map onto the later rails: an
AFT batch is many `hold` / `accept_inbound` calls settled at a window; a Lynx
wire is one `hold` + `release` with no `refund` (finality). This is why the seam
is not Interac-only.

## 5. Data model

Two new migration files, loaded in order by the init Job after `06_triggers.sql`.

### 5.1 Routing foundation (`07_rails.sql`)

- `ALTER TABLE accounts ADD institution_number VARCHAR(3) DEFAULT '900',
  ADD transit_number VARCHAR(5) DEFAULT '00001'` — nano-bank's own Canadian
  routing coordinates (fake institution `900`). External accounts live at other
  banks, so this stamps only nano-bank's accounts.
- `rail_participants` — external participant directory: `institution_number` PK,
  `name`, `is_self BOOL`, `supports_interac / supports_aft / supports_lynx BOOL`,
  `active`. Seeded with the real big-five institution numbers (RBC `003`,
  BMO `001`, Scotia `002`, TD `004`, CIBC `010`) plus nano-bank
  (`900`, `is_self = true`). Interac routes by handle; this records which external
  institution a transfer settled against.
- Two system accounts under `system@nano.bank`, bootstrapped idempotently at
  startup by a generalised `ensure_system_accounts` (promoted from `cards.rs`),
  each with the $1T overdraft so their balances can float:
  - `INTERAC_CLEARING` (`chequing`) — holds in-flight e-Transfer funds.
  - `INTERAC_SETTLEMENT` (`savings`) — the interbank position vs the network.

### 5.2 Interac tables (`08_interac.sql`)

New enums: `interac_direction` (outbound | inbound), `interac_status`
(initiated | held | available | deposited | declined | cancelled | expired |
failed), `interac_handle_type` (email | phone), `interac_notification_kind`
(incoming_transfer | deposit_completed | declined | cancelled | expired).

- `interac_handles` — `customer_id`, `handle_type`, `handle_value` (normalised,
  UNIQUE), `autodeposit_account_id` (nullable; set means autodeposit is on),
  `active`. Maps a handle to a customer for inbound routing; presence of
  `autodeposit_account_id` decides autodeposit vs claim.
- `interac_etransfers` — the core record: `direction`, `status`, sender columns
  (`sender_customer_id`, `sender_account_id`), recipient columns
  (`recipient_handle_type`, `recipient_handle_value`, resolved
  `recipient_customer_id` / `recipient_account_id`), `counterparty_institution`
  (when external), `amount`, `currency`, `security_question`,
  `security_answer_hash` (argon2; null when autodeposit), `claim_token`,
  `reference` memo, `hold_transaction_id`, `expires_at`, `wrong_answer_attempts`
  (locks after 3, matching real Interac), `idempotency_key`
  (`UNIQUE(sender_customer_id, idempotency_key)`), timestamps
  (`created_at`, `notified_at`, `resolved_at`).
- `interac_notifications` — the outbox: `etransfer_id`, `handle_value`, `kind`,
  `message`, `claim_token`, `delivered BOOL`, `created_at`. Read by the simulator
  and the viewer.

## 6. Money and GL flows

The local double-entry (customer account ↔ system accounts, via the existing
balance triggers) is the economic source of truth. Each transition **also** posts
a balanced aggregate GL entry through the `Ledger` port (the core-of-record trial
balance), mirroring the card rails and the `Payable`/`Payable` transfer audit
convention.

| Lifecycle event | Local double-entry (subledger) | Aggregate GL (Ledger port) |
|---|---|---|
| **Send** (hold) | Dr sender acct · Cr `INTERAC_CLEARING` | Dr Payable · Cr Payable (owed-to-customer → owed-to-clearing) |
| **Autodeposit / Claim** → internal recipient | Dr `INTERAC_CLEARING` · Cr recipient acct | Dr Payable · Cr Payable |
| **Release** → external recipient | Dr `INTERAC_CLEARING` · Cr `INTERAC_SETTLEMENT` | Dr Payable · Cr Payable (interbank liability) |
| **Decline / Cancel / Expire** (refund) | Dr `INTERAC_CLEARING` · Cr sender acct | Dr Payable · Cr Payable |
| **Inbound receive** (hold) | Dr `INTERAC_SETTLEMENT` · Cr `INTERAC_CLEARING` | Dr Receivable · Cr Payable (network owes us) |
| **Inbound autodeposit / claim** | Dr `INTERAC_CLEARING` · Cr recipient acct | Dr Payable · Cr Payable |

Because the neutral GL account set is coarse (`Bank`, `Receivable`, `Payable`,
`Revenue`, `Expense`), most transitions post a balanced `Payable`/`Payable`
reclassification — a net-zero audit marker, consistent with how PR #15 posts
transfers. The real economic positions live in the `INTERAC_CLEARING` and
`INTERAC_SETTLEMENT` system accounts, which are fully inspectable. The interbank
position stands in `INTERAC_SETTLEMENT`; sweeping it to `Bank` cash in a clearing
cycle is deferred to the AFT spec.

## 7. HTTP API surface

Three auth planes. The customer plane uses the existing `AuthenticatedCustomer`
extractor; the network and admin planes use the same service-token mechanism the
card rails use.

### Customer plane

- `POST /api/v1/interac/etransfers` — send. Body: `from_account_id`, `amount`,
  recipient `handle` (type + value), `security_question` + `security_answer`
  (omitted only when the target handle has autodeposit), `reference`,
  `idempotency_key`. Resolves the handle: registered + autodeposit → internal
  auto-credit; registered without autodeposit → internal claim; unregistered →
  external (simulator).
- `POST /interac/etransfers/{id}/cancel` — sender cancels before claim → refund.
- `POST /interac/etransfers/{id}/claim` — internal recipient answers the security
  question, picks a deposit account → release. Autodeposit skips this.
- `POST /interac/etransfers/{id}/decline` — recipient declines → refund to sender.
- `GET /interac/etransfers` — caller's transfers (sent + received), filtered.
- `GET /interac/etransfers/{id}` — single, ownership-scoped.
- `POST /interac/autodeposit` · `GET /interac/autodeposit`
  · `DELETE /interac/autodeposit/{id}` — autodeposit registration.

### Network plane (service-token — the simulator plays the rest of the Interac network)

- `POST /interac/network/inbound` — an external bank sends an e-Transfer into
  nano-bank addressed to a handle (autodeposit → credit; else held +
  notification).
- `POST /interac/network/etransfers/{id}/settle` — the far side ACKs an
  outbound-to-external transfer: `claimed` → release to `INTERAC_SETTLEMENT`;
  `declined` → refund sender.

### Admin plane (service-token)

- `POST /interac/admin/sweep-expired` — expire overdue holds, refund senders,
  post expiry notifications; returns a count. Primary mechanism (testable). An
  optional tokio interval task at startup calls the same internal function; a
  cron job would drive it in production.

## 8. Cross-cutting concerns

**Idempotency, limits, concurrency** — kept self-contained to avoid coupling to
the unmerged PR #15:

- Idempotency: `idempotency_key UNIQUE(sender, key)` on send; a replay returns
  the original transfer.
- Limits: funds check plus a configurable per-transfer cap now. Shared
  `account_limits` (daily / monthly transfer) integration is deferred to a
  follow-up once PR #15 merges, to avoid conflicting edits to that logic.
- Concurrency: canonical sorted-UUID `lock_all` ordering (no ABBA deadlock) and
  guarded status transitions (`UPDATE … WHERE status = 'available'` row count →
  one winner, others `409`) for claim / cancel / expire races.

**Security** — security-question answers are argon2-hashed (reusing
`utils/password.rs`) and verified on claim; three wrong answers lock the transfer.

**Configuration** — `NANO_BANK__INTERAC__EXPIRY_DAYS` (default 30),
`NANO_BANK__INTERAC__MAX_ETRANSFER_AMOUNT`; the service token is reused from the
card rails.

## 9. Coexistence with PR #15 (money-movement)

PR #15 is an open draft that heavily rewrites `handlers/transactions.rs`
(+1000 lines). This spec is designed to avoid that file entirely:

- `transactions.rs` is **not touched**. Interac writes its own `transactions` /
  `transaction_entries` rows using helpers promoted to `pub(crate)` from
  `cards.rs` (`post_two_legged`, `ensure_system_accounts`, reference-number gen)
  — a small, mergeable change with no behavioural impact.
- Everything else is new files: `rails/`, `handlers/interac.rs`,
  `models/interac.rs`, `07_rails.sql`, `08_interac.sql`, `bruno/6_Interac/`,
  `testing/interac/`.
- `handlers/mod.rs`, `main.rs` (route registration) and config receive additive
  edits only.

Rebasing against PR #15 stays trivial.

## 10. Testing

- `testing/interac/interac_simulator.py` — a container that plays the network:
  polls the notification outbox for outbound-to-external transfers and calls
  `/network/etransfers/{id}/settle` (claim with the shared security answer, or a
  random decline); originates inbound e-Transfers via `/network/inbound` into
  seeded handles; exercises autodeposit, claim and expiry paths.
- Extend `testing/viewer/app.py` (Streamlit, port 8504) with an Interac tab:
  in-flight transfers, clearing / settlement balances, and the notification
  outbox timeline.
- Rust integration tests for the lifecycle transitions (send → claim / decline /
  expire, autodeposit, inbound), following the test scaffolding PR #15
  introduces or standing up a minimal harness.
- A Bruno `6_Interac/` collection mirroring the flows for manual exercise.

## 11. Follow-ups (out of scope for this spec)

- Shared `account_limits` integration once PR #15 merges.
- The ACSS-style clearing/settlement sweep (`INTERAC_SETTLEMENT` → `Bank`),
  built with the AFT rail.
- Request Money (request e-Transfer) flow.
- AFT / EFT rail (spec 2) and Lynx rail (spec 3).
