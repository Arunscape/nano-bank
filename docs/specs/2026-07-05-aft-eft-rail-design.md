# Design: AFT / EFT Batch Rail (Direct Deposit + Pre-Authorized Debit)

**Status:** Approved for planning
**Date:** 2026-07-05
**Scope:** Spec 2 of the Canadian payment-rail programme (Interac → **AFT** → Lynx)
**Branch:** `aft-rail` (stacked on `interac-rail-foundation`)

## 1. Context and goals

Spec 1 delivered the Interac e-Transfer rail plus the shared foundation: the
`Rail` port (`api/src/rails/`), the participant directory + Canadian routing
(`rail_participants`, `accounts.institution_number/transit_number`), and per-rail
clearing/settlement system accounts that dual-post through the `Ledger` port.

This spec adds **AFT/EFT** — Canada's batch rail (Automated Funds Transfer over
ACSS). Unlike Interac's per-message, near-real-time model, AFT is **file-based
and batched**: instructions accrue into a batch, are emitted as a CPA-005 file at
a cutoff, settle at a window, and — for debits — can **bounce back as returns**
days later. AFT also builds the **settlement sweep** deferred from Interac
(`*_SETTLEMENT → Bank` cash).

Two products, both in scope:
- **Direct deposit (credits)** — push money out (e.g. payroll) and receive
  inbound credits into customer accounts.
- **Pre-authorized debit (PAD, debits)** — pull money from a payer's account,
  gated by a stored **mandate**, with an **NSF/returns** cycle.

### Non-goals
- Lynx wires (spec 3).
- Byte-exact CPA-005 (1464-byte logical records). We model a CPA-005-*style*
  fixed-width format — authentic in shape, round-trippable — not the exact spec.
- Real-time settlement. AFT settles at simulated windows driven by the simulator.

## 2. Design decisions (from brainstorming)

1. **Full AFT**: credits + PAD debits + settlement + returns, together (they are
   tightly coupled — batches, settlement, and returns don't stand apart).
2. **Real CPA-005 files, DB-driven core**: a real fixed-width file is emitted on
   submit and a returns file is ingested; the core logic is DB state, and a
   simulator plays ACSS.
3. **PAD mandates modeled**: a debit must cite a valid active mandate.
4. **Reuse the `Rail` port for money moves; keep AFT orchestration out of the
   trait.** Batching, the CPA-005 file, the settlement sweep, and returns live in
   the AFT module. The sweep stays AFT-local for now; promote it to the `Rail`
   trait only when Lynx also needs it.

## 3. Architecture

```
api/src/rails/aft.rs        AftRail: impl Rail + ensure_aft_accounts (aft@nano.bank)
api/src/aft/cpa005.rs       CPA-005-style fixed-width encode/decode (round-trippable)
api/src/handlers/aft.rs     batch lifecycle, mandates, settlement, returns, file emit/ingest
api/src/models/aft.rs       DTOs + entities
src/core/tables/09_aft.sql  schema
```

`AftRail` implements the existing `Rail` trait (`hold` / `release` / `refund` /
`accept_inbound`) for the per-transaction money moves. Everything AFT-specific —
accruing a batch, emitting/ingesting the CPA-005 file, the settlement-window
sweep, and post-settlement returns — is orchestration in `handlers/aft.rs`, built
*on top of* those verbs. The `Rail` trait is not extended.

**System accounts (AFT-owned, decoupled from cards):** a new synthetic customer
`aft@nano.bank` owns `AFT_CLEARING` (chequing, in-flight originated funds) and
`AFT_SETTLEMENT` (savings), $1T overdraft, bootstrapped at startup — same pattern
as Interac (its own customer because GL accounts key on `(customer, account_type)`).
AFT does **not** reuse the card rails' `BANK_SETTLEMENT` or any card system
account.

**Settlement/funding:** `AFT_SETTLEMENT` doubles as AFT's own settlement/funding
account (the bank's AFT interbank + cash position). The settlement sweep moves
the in-flight net from `AFT_CLEARING` to `AFT_SETTLEMENT` locally, and posts the
net cash effect through the `Ledger` port's `Bank`/`Payable` roles — the aggregate
GL is the record of cash actually settling with the network. This keeps AFT
entirely self-contained.

## 4. Data model (`09_aft.sql`)

New enums: `aft_entry_kind` (credit | debit), `aft_batch_status`
(open | submitted | settled), `aft_entry_status`
(pending | settled | returned | rejected), `mandate_status` (active | revoked),
`aft_direction` (outbound | inbound).

- `pad_mandates` — `mandate_id`, `payer_account_id` (nano-bank account) or
  external payer coords, `biller_name`, `originator_id`, `amount_cap`,
  `frequency`, `status`, timestamps. A PAD debit must reference a valid `active`
  mandate; the payer can revoke.
- `aft_batches` — `batch_id`, `direction`, `status`, `entry_count`,
  `total_credits`, `total_debits`, `cutoff_at`, `settled_at`, `file_ref` (path of
  the emitted CPA-005 file), timestamps.
- `aft_entries` — `entry_id`, `batch_id`, `kind`, `direction`, `originator_account_id`
  (nano-bank account), counterparty coords (`institution_number` + `transit` +
  `account`, or an internal `counterparty_account_id`), `payee_name`, `amount`,
  `mandate_id` (debits), `status`, `return_reason`, and links to the
  hold/settle/return `transaction_id`s.

## 5. Money & GL flows

The local double-entry (customer/originator ↔ `AFT_CLEARING`/`AFT_SETTLEMENT`, via
the balance triggers) is the source of truth; each transition also posts the
aggregate `Ledger` effect (the `Payable`/`Receivable`/`Bank` roles), like Interac.
Customer-account `available_balance` is hand-recomputed around posts (system
accounts stay at 0), per the Interac convention.

| Flow | Originate (batch open) | Settle (window) | Return |
|---|---|---|---|
| **Credit out** (pay external) | `hold`: Dr originator / Cr `AFT_CLEARING` | `release` External: Dr `AFT_CLEARING` / Cr `AFT_SETTLEMENT`; **sweep** (net `Bank` GL) | credit returned → refund originator |
| **Credit in** (payroll → customer) | — (inbound file) | `accept_inbound`: Dr `AFT_SETTLEMENT` / Cr customer; sweep | rare |
| **Debit out / PAD** (biller pulls external) | originate (cites mandate) | Dr `AFT_SETTLEMENT` / Cr biller; sweep | NSF → reverse: Dr biller / Cr `AFT_SETTLEMENT` |
| **Debit in / PAD** (external biller pulls customer) | `hold`: Dr customer / Cr `AFT_CLEARING` (active mandate + funds) | `release`: Dr `AFT_CLEARING` / Cr `AFT_SETTLEMENT`; sweep | insufficient funds at originate → `reject` (return, funds stay) |

**Settlement sweep** (per window, the deferred piece): moves the in-flight net
from `AFT_CLEARING` into AFT's own `AFT_SETTLEMENT` in one transaction and posts
the net `Bank`/`Payable` GL through the `Ledger` port — the debit/credit direction
follows the sign of the net (cash out for net credits, cash in for net
collections). Same shape as the card rails' `settle`, but on AFT's own accounts —
no shared card system account.

**Returns** are a *post-settlement* reversal (distinct from `refund`, which
returns a pre-settlement hold): post mirror legs of the original settlement,
reverse the aggregate GL, recompute the affected customer's `available_balance`,
and mark the entry `returned`.

## 6. CPA-005-style file (`aft/cpa005.rs`)

A fixed-width, line-oriented format that round-trips (encode→decode is identity —
the core unit test):
- **Header record**: record type, originator id, file creation date, file
  sequence number.
- **Detail record** (one per entry): transaction code (`C`/`D`), amount,
  destination institution + transit + account, payee/payor name, originator
  short name, due date.
- **Trailer record**: entry count, total credit amount, total debit amount.

The module exposes `encode(batch, entries) -> String` and
`decode(&str) -> (Header, Vec<Detail>, Trailer)`; the returns file reuses the
same detail layout plus a return-reason code.

## 7. API surface

**Customer plane** (`AuthenticatedCustomer`):
- `POST /aft/mandates` · `GET /aft/mandates` · `DELETE /aft/mandates/{id}` — a
  payer authorizes/revokes a biller.
- `POST /aft/credits` — queue a direct-deposit credit into the open outbound
  batch. `POST /aft/debits` — queue a PAD debit (must cite an active mandate).
- `POST /aft/batches/{id}/submit` — close + emit the CPA-005 file (originator or
  admin).
- `GET /aft/batches` · `GET /aft/entries` — ownership-scoped.

**Service plane** (`AuthenticatedService`, driven by the simulator = ACSS):
- `POST /aft/network/settle/{batch}` — settle a submitted batch (apply legs +
  sweep).
- `POST /aft/network/inbound-batch` — ingest an inbound CPA-005 file (external
  credits/debits targeting nano-bank customers).
- `POST /aft/network/returns` — ingest a returns file; reverse the cited entries.

## 8. Cross-cutting

- **Auth planes / ownership**: customer endpoints scope to the caller (cross-
  customer → 404); network/admin use the service token — same as Interac.
- **Concurrency**: batch/entry state transitions guarded with `FOR UPDATE` +
  status re-check (a batch settles once; an entry returns once), mirroring the
  Interac claim/settle guards.
- **available_balance**: recompute on customer accounts around rail posts; system
  accounts stay at 0 (the lesson learned in the Interac build — never recompute
  system accounts).
- **PR #15 coexistence**: do NOT touch `handlers/transactions.rs`. Reuse the
  `pub(crate)` `cards.rs` helpers and the `Rail` port. New files only.
- **Config**: `NANO_BANK__AFT__*` (settlement window, return window, file output
  dir), read via env like the Interac config.

## 9. Testing

- `testing/aft/aft_simulator.py` — plays **ACSS**: polls submitted batches, reads
  their CPA-005 files, calls `/network/settle`; originates inbound batches; emits
  returns files for a fraction of debits and posts them to `/network/returns`.
- `api/src/aft/cpa005.rs` — `#[cfg(test)]` round-trip tests (encode→decode
  identity; header/trailer totals).
- Extend `testing/viewer/app.py` with an **AFT tab**: batches by status,
  `AFT_CLEARING`/`AFT_SETTLEMENT`/`BANK_SETTLEMENT` balances, entries + returns.
- Bruno `7_AFT` collection; curl smoke end-to-end (originate → submit → settle →
  return), the repo's established verification path.

## 10. Follow-ups (out of scope)
- Lynx wires (spec 3), which will promote the settlement sweep to the `Rail`
  trait if it fits.
- Retroactively giving Interac its own settlement sweep (same AFT-local pattern).
- Byte-exact CPA-005; real SFTP file exchange.
- Shared `account_limits` integration (still pending PR #15).
