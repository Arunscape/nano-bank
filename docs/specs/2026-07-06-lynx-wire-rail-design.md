# Lynx Wire Rail — Design

**Status:** approved
**Date:** 2026-07-06
**Spec:** 3 of the Canadian payment-rails programme (Interac → AFT/EFT → **Lynx**)

## Purpose

Add **Lynx** — Canada's real-time gross settlement (RTGS) system for high-value
wire transfers — as the third external payment rail, built on the existing
`Rail` port. Unlike Interac (retail push) and AFT (deferred-net batch), Lynx
settles each payment **individually, in real time, with settlement finality**:
once settled a wire is irrevocable. Recovery of a settled wire is possible only
through a **recall request** the counterparty may accept or reject — never a
unilateral reversal.

This rail is a teaching/integration exercise, not a production payment system.
The goal is an authentic RTGS shape (per-wire settlement, ISO 20022 messaging,
finality + recall) wired end-to-end through nano-bank's double-entry ledger and
the swappable GL core.

## Scope

In scope:
- **Outbound** customer wires (nano-bank customer → external institution).
- **Inbound** wires (external institution → nano-bank customer).
- **Two-step settlement**: customer send reserves funds; a network/settlement
  call reaches finality.
- **Recall**, both directions: outbound (our customer requests a sent wire back)
  and inbound (an external sender requests a received wire back).
- **Real ISO 20022 messages** (pacs.008, pacs.009, camt.056, camt.029),
  round-trippable and unit-tested.
- A **high-value floor** (configurable), no ceiling.
- Network simulator, viewer tab, Bruno collection.

Out of scope (follow-ups): intraday liquidity / credit limits at the Bank of
Canada, message signing/auth beyond the existing service-token plane, batch
LVTS-style multilateral netting (Lynx is gross, not net), FX (CAD only),
schema-validating the ISO 20022 XML against the full XSD.

## Architecture

Lynx is a **peer rail** to Interac and AFT — fully decoupled, mirroring AFT's
layout and isolation. It reuses the `Rail` port verbs
(`hold`/`release`/`refund`/`accept_inbound`) and the `pub(crate)` helpers
promoted from `cards.rs` (`post_two_legged`, `post_gl_entry`,
`reference_number`). It does **not** touch `handlers/transactions.rs` or the
other rails.

```
customer  ─► POST /api/v1/lynx/wires ──────► hold  Dr customer / Cr LYNX_CLEARING   (status=sent, emit pacs.008)
network   ─► POST /api/v1/lynx/network/wires/:id/settle ─► release External: Dr CLEARING / Cr LYNX_SETTLEMENT (status=settled, FINAL)
network   ─► POST /api/v1/lynx/network/inbound ─────────► accept_inbound: Dr LYNX_SETTLEMENT / Cr customer (status=settled)
```

| File | Role |
|---|---|
| `src/core/tables/10_lynx.sql` | schema: enums + `lynx_wires`, `lynx_messages`, `lynx_recalls` |
| `api/src/rails/lynx.rs` | `LynxRail impl Rail`; `lynx@nano.bank` system customer + `ensure_lynx_accounts` |
| `api/src/lynx/iso20022.rs` | ISO 20022 codec (pacs.008/pacs.009/camt.056/camt.029) + unit tests |
| `api/src/handlers/lynx.rs` | wire lifecycle: customer / network / admin planes |
| `api/src/models/lynx.rs` | request/response DTOs + `sqlx::Type` enums |
| `testing/lynx/` | network simulator + Containerfile + requirements |
| `testing/viewer/app.py` | new Lynx tab (existing file, additive) |
| `bruno/8_Lynx/` | request collection |

### System accounts

A **separate** synthetic customer `lynx@nano.bank` (phone `+10000000004`, SIN
`000000004`) owns two GL accounts, keyed by `(customer, account_type)`:
- **`LYNX_CLEARING`** (chequing) — holds funds reserved for in-flight outbound
  wires.
- **`LYNX_SETTLEMENT`** (savings) — nano-bank's interbank/central-bank position.

Both carry a $1T overdraft so they can float negative; `available_balance` is
pinned at 0 for these system accounts (they never gate on funds). Distinct from
the card rails' `system@nano.bank`, Interac's `interac@nano.bank`, and AFT's
`aft@nano.bank`. `ensure_lynx_accounts` bootstraps them idempotently at startup,
re-resolved per request (a data wipe rebuilds them).

## Schema (`10_lynx.sql`)

**Enums**
- `lynx_direction`: `outbound`, `inbound`.
- `lynx_wire_status`: `sent`, `settled`, `rejected`, `recalled`.
- `lynx_recall_status`: `requested`, `accepted`, `rejected`.

**`lynx_wires`** — one row per wire.

| Column | Notes |
|---|---|
| `wire_id` UUID PK | |
| `uetr` UUID UNIQUE | ISO 20022 unique end-to-end transaction reference |
| `direction` `lynx_direction` | |
| `status` `lynx_wire_status` | |
| `local_account_id` UUID FK accounts | our customer account (debtor if outbound, creditor if inbound) |
| `counterparty_name` VARCHAR(140) | ISO 20022 name max |
| `counterparty_institution` VARCHAR(3) FK rail_participants | must have `supports_lynx = TRUE`, `active` |
| `counterparty_account` VARCHAR(34) | external account identifier |
| `amount` NUMERIC(19,4), `currency` VARCHAR(3) default `CAD` | |
| `remittance_info` VARCHAR(140) NULL | |
| `message_type` VARCHAR(12) | `pacs.008` (customer) or `pacs.009` (FI) |
| `settlement_transaction_id` UUID FK transactions NULL | the settling `transactions` row |
| `gl_entry` VARCHAR NULL | `backend:doc_id` from the Ledger core |
| `initiated_by` UUID FK customers NULL | null for inbound (network-originated) |
| `reference_number` VARCHAR UNIQUE | `LYNX…` reference |
| `created_at`, `sent_at`, `settled_at` TIMESTAMPTZ | |

**`lynx_messages`** — audit/outbox of the ISO 20022 payloads exchanged (like
Interac's notification outbox). `message_id` UUID PK, `wire_id` FK,
`message_type` (pacs.008/pacs.009/camt.056/camt.029), `flow`
(`emitted`/`received`), `payload` TEXT (the XML), `created_at`. The simulator
reads emitted messages and posts received ones via the network endpoints.

**`lynx_recalls`** — `recall_id` UUID PK, `wire_id` FK, `direction`
(`lynx_direction` — who initiated), `requested_by` UUID NULL, `reason`
VARCHAR(140), `status` `lynx_recall_status`, `resolution_reason` VARCHAR(140)
NULL, `camt056_message_id` UUID FK lynx_messages NULL, `camt029_message_id` UUID
FK lynx_messages NULL, `created_at`, `resolved_at` TIMESTAMPTZ NULL.

## Lifecycle & endpoints

Three auth planes, consistent with Interac/AFT: **customer**, service-token
**network**, service-token **admin**.

### Customer plane

- **`POST /api/v1/lynx/wires`** — initiate an outbound wire. Validates:
  `amount >= min_amount` (floor), `currency = CAD`, from-account owned +
  operable + `available_balance >= amount`, counterparty institution exists and
  is lynx-capable + active. Effect: `hold` (Dr customer / Cr `LYNX_CLEARING`),
  mint a `uetr`, emit **pacs.008** to `lynx_messages`, insert `lynx_wires`
  (`status=sent`). Returns the wire + UETR. **201**.
- **`GET /api/v1/lynx/wires`** — list the caller's wires (both directions).
- **`GET /api/v1/lynx/wires/:id`** — single wire; ownership-scoped — **404** if
  the caller is not a party (no existence leak).
- **`POST /api/v1/lynx/wires/:id/recall`** — request recall of a **settled
  outbound** wire the caller initiated (initiator-only, else 404). Inserts
  `lynx_recalls` (`direction=outbound`, `status=requested`), emits **camt.056**.
  Rejects (409) if a recall is already open or the wire isn't settled-outbound.
  **201**.

### Network plane (service token — driven by the simulator)

- **`POST /api/v1/lynx/network/wires/:id/settle`** — Bank-of-Canada settlement
  of a `sent` wire. Guarded transition `sent → settled` (concurrent double-settle
  → **409**). Effect: `release` with `Destination::External(institution)` →
  Dr `LYNX_CLEARING` / Cr `LYNX_SETTLEMENT`; GL **Dr Payable / Cr Bank**;
  `status=settled` (**final**), set `settled_at`. **200**.
- **`POST /api/v1/lynx/network/inbound`** — an arriving wire (pacs.008/pacs.009).
  Body carries the message fields + target local account (resolved by
  institution `900` + transit + account number, or `local_account_id`). Effect:
  `accept_inbound` → Dr `LYNX_SETTLEMENT` / Cr customer; GL **Dr Bank / Cr
  Payable** (real central-bank money arrived immediately — deliberately `Bank`,
  not the `Receivable` Interac uses for its unsettled inbound). Insert
  `lynx_wires` (`direction=inbound`, `status=settled`), store the received
  message. **201**.
- **`POST /api/v1/lynx/network/recalls/:id/resolve`** — the beneficiary FI's
  **camt.029** answer to our outbound recall. `accept` → refund the customer
  (Dr `LYNX_SETTLEMENT` / Cr customer), wire `status=recalled`, recall
  `status=accepted`; `reject` → wire stays `settled`, recall `status=rejected`.
  Guarded so a resolved recall can't be resolved twice (409). Stores the camt.029.
  **200**.
- **`POST /api/v1/lynx/network/inbound-recall`** — an external sender's
  **camt.056** for a wire we received. We respond with a **camt.029**: `accept`
  → claw back from the beneficiary customer (Dr customer / Cr `LYNX_SETTLEMENT`),
  wire `status=recalled`; `reject` if the customer has insufficient funds (v1
  simplification) or on policy. Records both messages. **200**.

### Admin plane (service token)

- **`POST /api/v1/lynx/admin/reject-stale`** — sweep `sent` wires the network
  never settled (older than a configurable age). Per row: `refund` (Dr
  `LYNX_CLEARING` / Cr customer), `status=rejected`. Mirrors Interac's
  sweep-expired; handles the in-flight-never-settled failure case. **200** with a
  count.

## ISO 20022 codec (`iso20022.rs`)

A struct per message with `encode()` → XML string and `decode(&str)` → struct,
**round-trippable** (authentic shape; not validated against the full XSD — the
same philosophy as AFT's CPA-005 codec). Messages:

- **pacs.008** — FIToFICustomerCreditTransfer (outbound customer wire).
- **pacs.009** — FinancialInstitutionCreditTransfer (FI-to-FI; inbound may be
  either type).
- **camt.056** — FIToFIPaymentCancellationRequest (recall request).
- **camt.029** — ResolutionOfInvestigation (recall accept/reject).

Each carries the fields that matter for the flow: `uetr`, debtor/creditor name +
agent (institution number) + account, `amount`/`currency`, `remittance_info`,
and (for camt) the original UETR + a reason/status code. Unit tests assert
`decode(encode(x)) == x` for representative values of all four. No DB, no
network — pure functions.

## Settlement / GL mechanics

Every rail movement posts local double-entry **and** the aggregate GL effect
through the `Ledger` port, inside one DB transaction — if the core can't record
the GL the operation fails (**503**) and rolls back, so the subledger and GL
never drift.

| Step | Local legs | Aggregate GL | Rationale |
|---|---|---|---|
| Send (hold) | Dr customer / Cr `LYNX_CLEARING` | Dr Payable / Cr Payable (net 0) | funds reserved, not yet gone |
| Settle | Dr `LYNX_CLEARING` / Cr `LYNX_SETTLEMENT` | **Dr Payable / Cr Bank** | money leaves the bank (finality) |
| Inbound | Dr `LYNX_SETTLEMENT` / Cr customer | **Dr Bank / Cr Payable** | central-bank money arrived |
| Outbound recall accept | Dr `LYNX_SETTLEMENT` / Cr customer | Dr Bank / Cr Payable | funds returned to us |
| Inbound recall accept | Dr customer / Cr `LYNX_SETTLEMENT` | Dr Payable / Cr Bank | funds clawed back and returned |
| Stale reject | Dr `LYNX_CLEARING` / Cr customer | Dr Payable / Cr Payable (net 0) | reservation released, never settled |

**`available_balance` rule** (hard-won from Interac/AFT): recompute
`available_balance` **only on customer accounts** — zero it before a customer
debit, recompute after — and **never** on the system `LYNX_CLEARING` /
`LYNX_SETTLEMENT` accounts, which stay at 0 and float on the $1T overdraft.

## Error handling & auth

- Cross-customer access → **404** (no existence leak), matching the house pattern.
- Amount below floor, non-CAD, unknown/non-lynx counterparty institution → **400**.
- Core down during a GL post → **503** + rollback.
- Double-settle / double-recall-resolve → **409** via guarded status transitions
  (`UPDATE … WHERE status = <expected>`, require one row).
- Insufficient funds on send or inbound-recall clawback → `InsufficientFunds`.
- Network and admin endpoints require a valid **service token**
  (`AuthenticatedService`); customer endpoints require `AuthenticatedCustomer`.

## Testing

- **Unit** — codec round-trips for pacs.008, pacs.009, camt.056, camt.029.
- **Integration** (cargo tests against a live stack; skip cleanly offline — same
  `require_stack!` harness as aft/interac): send → network-settle (final) →
  inbound credit → outbound recall (request + accept-refund + reject) → inbound
  recall (accept-clawback + reject) → stale-sweep → high-value-floor rejection →
  non-lynx-institution rejection → ownership scoping (404) → double-settle 409.
- **Simulator** (`testing/lynx/lynx_simulator.py`) — plays Bank of Canada /
  beneficiary FI: settles `sent` wires, originates inbound wires, resolves our
  recalls (camt.029), and requests inbound recalls (camt.056). A small Python
  ISO 20022 encoder matching the Rust codec's field shape. Containerized like
  `testing/aft/` and `testing/interac/`.
- **Viewer** — a Lynx tab: wires list with status, `LYNX_CLEARING` /
  `LYNX_SETTLEMENT` balances, and the message log.
- **Bruno** — `8_Lynx/` covering the full flow.

## v1 simplifications

- Inbound-recall clawback **rejects** rather than forcing a negative customer
  balance if the beneficiary has already spent the funds (same stance as AFT
  returns and transaction reversal).
- ISO 20022 messages are authentic-shape and round-trippable, **not** validated
  against the official XSD.
- No intraday liquidity/credit ceiling at the Bank of Canada; the settlement
  account floats on the $1T overdraft.
- CAD only; no FX.
- `pacs.009` is supported by the codec and accepted inbound, but the customer
  send path always emits `pacs.008` (customer credit transfer).

## Branch / delivery

Built inline (no background agents) in the `lynx-rail` worktree, stacked on
`aft-rail` per the programme's stacked-PR strategy (Interac → EFT → **Wires**).
The `aft-rail → lynx-rail` stack rebases onto `main` after the Interac PR (#18)
merges. Does not modify `handlers/transactions.rs`.
