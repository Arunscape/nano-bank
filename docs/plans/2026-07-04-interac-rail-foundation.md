# Payment Rail Foundation + Interac e-Transfer — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Canadian external payment-rail foundation (routing, participant directory, per-rail clearing/settlement GL accounts, a `Rail` port) and the full Interac e-Transfer product on top of it.

**Architecture:** A `Rail` port (`api/src/rails/`) sits *beside* the existing `Ledger` port. A rail owns the local double-entry (customer account ↔ its clearing/settlement system accounts, via the DB balance triggers) AND posts the aggregate GL effect through `Ledger`, both inside one DB transaction (503 + rollback if the core is down). Interac is the first `Rail` implementation; its handlers (`api/src/handlers/interac.rs`) carry the product lifecycle (send/autodeposit/claim/decline/cancel/expire + inbound + settle).

**Tech Stack:** Rust, axum 0.7, sqlx 0.7 (Postgres 16), rust_decimal, argon2 (security-answer hashing), tokio; Python (simulator + Streamlit viewer) for end-to-end verification.

## Global Constraints

- **CAD only.** All money is `rust_decimal::Decimal`, never floats. Amounts rounded to 2 dp; schema rejects otherwise.
- **Double-entry invariant:** both legs of every transaction inserted in ONE multi-row INSERT (use `handlers::cards::post_two_legged`). Never update `accounts.balance` directly — triggers own it.
- **System/GL accounts are keyed by `(customer_id, account_type)`**; `account_number` is trigger-overwritten. Re-resolve their UUIDs per request (a data wipe rebuilds them).
- **Dual-post:** local subledger + aggregate GL through `state.ledger` (`handlers::cards::post_gl_entry`), GL post before commit; a GL failure fails the whole op (503).
- **Do NOT edit `handlers/transactions.rs`** (rewritten by open draft PR #15). Reuse the already-`pub(crate)` helpers in `cards.rs` (`post_two_legged`, `post_gl_entry`, `reference_number`, `normalize_amount`, `fetch_account_for_update`, `Tx<'a>`). New files only elsewhere; `main.rs` / `handlers/mod.rs` get additive edits.
- **Auth planes:** customer endpoints use `AuthenticatedCustomer` (`customer_id`); network/admin endpoints use `AuthenticatedService`. Cross-customer access returns **404**, not 403.
- **DB host is `::1`** (not 127.0.0.1). Run needs Kind Postgres up + `kubectl port-forward -n nano-bank svc/postgres-service 5432:5432`, and a core (modern `:8091` default) for GL posts.
- **Spec:** `docs/specs/2026-07-04-interac-rail-foundation-design.md`. One deliberate simplification vs the spec: the `Rail` trait methods return `Result<_, AppError>` (not a separate `RailError`) since every caller is a handler — matches the `cards.rs` helpers.

---

## Verification note

The repo has **zero Rust tests**; features are verified through the Python container harness + Bruno + curl smoke scripts (see how the card rails were verified). This plan follows that convention: **pure functions** get `#[cfg(test)]` unit tests (real TDD), and **HTTP lifecycle** is verified by concrete curl scripts with expected output plus the `interac_simulator`. A full Rust integration harness is a follow-up, not this spec.

Throughout, `$TOK` is a customer access token and `$STOK` a service token, obtained via:
```bash
# customer: log in a seeded customer (see testing/generator); service: mint via auth
# store as: TOK=<jwt>  STOK=<service jwt>
```

---

## Task 1: Routing foundation schema (`07_rails.sql`)

**Files:**
- Create: `src/core/tables/07_rails.sql`

**Interfaces:**
- Produces: `accounts.institution_number` / `accounts.transit_number` columns; table `rail_participants(institution_number PK, name, is_self, supports_interac, supports_aft, supports_lynx, active, created_at)` seeded with nano-bank (`900`) + big-five.

- [ ] **Step 1: Write the DDL**

```sql
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
```

- [ ] **Step 2: Apply to the running DB**

The init Job only loads DDL on a fresh cluster, so apply manually to the live DB:
```bash
kubectl exec -i -n nano-bank deployment/postgres -- \
  psql -U nanobank_user -d nano_bank_db < src/core/tables/07_rails.sql
```
Expected: `ALTER TABLE` ×2, `CREATE TABLE`, `INSERT 0 6`.

- [ ] **Step 3: Verify**

```bash
kubectl exec -i -n nano-bank deployment/postgres -- \
  psql -U nanobank_user -d nano_bank_db -c \
  "SELECT institution_number, name, is_self FROM rail_participants ORDER BY institution_number;"
```
Expected: 6 rows; `900 | nano-bank | t`.

- [ ] **Step 4: Commit**

```bash
git add src/core/tables/07_rails.sql
git commit -m "feat(rails): routing foundation — account routing cols + participant directory"
```

---

## Task 2: Interac schema (`08_interac.sql`)

**Files:**
- Create: `src/core/tables/08_interac.sql`

**Interfaces:**
- Produces: enums `interac_direction`, `interac_handle_type`, `interac_status`, `interac_notification_kind`; tables `interac_handles`, `interac_etransfers`, `interac_notifications`.

- [ ] **Step 1: Write the DDL**

```sql
-- Nano Bank Core Database Schema
-- Part 8: Interac e-Transfer

CREATE TYPE interac_direction   AS ENUM ('outbound', 'inbound');
CREATE TYPE interac_handle_type AS ENUM ('email', 'phone');
CREATE TYPE interac_status AS ENUM (
    'initiated', 'held', 'available', 'deposited',
    'declined', 'cancelled', 'expired', 'failed'
);
CREATE TYPE interac_notification_kind AS ENUM (
    'incoming_transfer', 'deposit_completed', 'declined', 'cancelled', 'expired'
);

-- Handle registrations. A row maps an email/phone to a customer for inbound
-- routing; a non-null autodeposit_account_id means autodeposit is enabled.
CREATE TABLE interac_handles (
    handle_id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    customer_id            UUID NOT NULL REFERENCES customers(customer_id) ON DELETE CASCADE,
    handle_type            interac_handle_type NOT NULL,
    handle_value           VARCHAR(255) NOT NULL,
    autodeposit_account_id UUID REFERENCES accounts(account_id) ON DELETE SET NULL,
    active                 BOOLEAN NOT NULL DEFAULT TRUE,
    created_at             TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT uq_interac_handle_value UNIQUE (handle_value)
);

CREATE TABLE interac_etransfers (
    etransfer_id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    direction                interac_direction NOT NULL,
    status                   interac_status NOT NULL DEFAULT 'initiated',
    amount                   DECIMAL(15,2) NOT NULL,
    currency                 VARCHAR(3) NOT NULL DEFAULT 'CAD',
    sender_customer_id       UUID REFERENCES customers(customer_id),
    sender_account_id        UUID REFERENCES accounts(account_id),
    sender_name              VARCHAR(200),
    recipient_handle_type    interac_handle_type NOT NULL,
    recipient_handle_value   VARCHAR(255) NOT NULL,
    recipient_customer_id    UUID REFERENCES customers(customer_id),
    recipient_account_id     UUID REFERENCES accounts(account_id),
    counterparty_institution VARCHAR(3) REFERENCES rail_participants(institution_number),
    security_question        TEXT,
    security_answer_hash     TEXT,
    claim_token              VARCHAR(40) NOT NULL,
    memo                     TEXT,
    hold_transaction_id      UUID REFERENCES transactions(transaction_id),
    wrong_answer_attempts    INTEGER NOT NULL DEFAULT 0,
    idempotency_key          VARCHAR(255),
    expires_at               TIMESTAMP WITH TIME ZONE,
    created_at               TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    notified_at              TIMESTAMP WITH TIME ZONE,
    resolved_at              TIMESTAMP WITH TIME ZONE,
    CONSTRAINT chk_interac_amount_positive  CHECK (amount > 0),
    CONSTRAINT chk_interac_amount_precision CHECK (amount = ROUND(amount, 2)),
    CONSTRAINT chk_interac_currency_cad     CHECK (currency = 'CAD'),
    -- NULLs are distinct in Postgres, so unregistered/inbound (null key) never collide.
    CONSTRAINT uq_interac_idempotency UNIQUE (sender_customer_id, idempotency_key)
);
CREATE INDEX idx_interac_recipient_handle ON interac_etransfers (recipient_handle_value);
CREATE INDEX idx_interac_status           ON interac_etransfers (status);
CREATE INDEX idx_interac_sender           ON interac_etransfers (sender_customer_id);

-- Notification outbox: the simulator + viewer read this (no real email/SMS).
CREATE TABLE interac_notifications (
    notification_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    etransfer_id    UUID NOT NULL REFERENCES interac_etransfers(etransfer_id) ON DELETE CASCADE,
    handle_value    VARCHAR(255) NOT NULL,
    kind            interac_notification_kind NOT NULL,
    message         TEXT NOT NULL,
    claim_token     VARCHAR(40),
    delivered       BOOLEAN NOT NULL DEFAULT FALSE,
    created_at      TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
);
CREATE INDEX idx_interac_notifications_undelivered
    ON interac_notifications (delivered) WHERE delivered = FALSE;
```

- [ ] **Step 2: Apply & verify**

```bash
kubectl exec -i -n nano-bank deployment/postgres -- \
  psql -U nanobank_user -d nano_bank_db < src/core/tables/08_interac.sql
kubectl exec -i -n nano-bank deployment/postgres -- \
  psql -U nanobank_user -d nano_bank_db -c "\dt interac_*"
```
Expected: `CREATE TYPE` ×4, `CREATE TABLE` ×3, `CREATE INDEX` ×4; `\dt` lists `interac_etransfers`, `interac_handles`, `interac_notifications`.

- [ ] **Step 3: Commit**

```bash
git add src/core/tables/08_interac.sql
git commit -m "feat(interac): e-Transfer schema — handles, etransfers, notification outbox"
```

---

## Task 3: `Rail` port + Interac system accounts (`rails/` module)

**Files:**
- Create: `api/src/rails/mod.rs`
- Create: `api/src/rails/interac.rs`
- Modify: `api/src/main.rs` (add `mod rails;`, bootstrap Interac accounts at startup)

**Interfaces:**
- Produces:
  - `rails::PgTx<'a>` = `sqlx::Transaction<'a, sqlx::Postgres>`
  - `rails::RailId { Interac, Aft, Lynx }` with `as_str()`
  - `rails::Hold { from_account: Uuid, amount: Decimal, reference: String, transaction_id: Uuid }`
  - `rails::Destination { Internal(Uuid), External(String) }`
  - `rails::RailPosting { transaction_id: Uuid, gl_entry: Option<String> }`
  - `rails::Rail` trait (see Task 4)
  - `rails::interac::InteracAccounts { clearing_id: Uuid, settlement_id: Uuid }`
  - `rails::interac::ensure_interac_accounts(pool: &DatabasePool) -> Result<InteracAccounts, sqlx::Error>`
  - `rails::interac::normalize_handle(handle_type, raw: &str) -> String`

- [ ] **Step 1: Write the failing unit test for handle normalization**

In `api/src/rails/interac.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_handles_are_lowercased_and_trimmed() {
        assert_eq!(normalize_handle(HandleType::Email, "  Alice@Example.COM "), "alice@example.com");
    }

    #[test]
    fn phone_handles_keep_only_digits_and_plus() {
        assert_eq!(normalize_handle(HandleType::Phone, "+1 (416) 555-0199"), "+14165550199");
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd api && cargo test rails::interac::tests -- --nocapture`
Expected: FAIL — `cannot find function normalize_handle` / module `rails` not declared.

- [ ] **Step 3: Write `rails/mod.rs`**

```rust
//! The **Rail port**: nano-bank's interface to an external payment rail
//! (Interac, AFT, Lynx). A rail sits BESIDE the Ledger port — it owns the local
//! double-entry (customer account ↔ its clearing/settlement system accounts) AND
//! posts the aggregate GL effect through `Ledger`, in one DB transaction.
//!
//! The trait's verbs are the clearing/settlement plumbing common to every rail;
//! product lifecycle (Interac's claim/decline/expiry) lives in the handler.

pub mod interac;

use async_trait::async_trait;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::errors::AppError;
use crate::handlers::AppState;

pub type PgTx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailId {
    Interac,
    Aft,
    Lynx,
}

impl RailId {
    pub fn as_str(self) -> &'static str {
        match self {
            RailId::Interac => "interac",
            RailId::Aft => "aft",
            RailId::Lynx => "lynx",
        }
    }
}

/// A reserved amount sitting in a rail's clearing account.
#[derive(Debug, Clone)]
pub struct Hold {
    /// The account funds were reserved from. For an inbound hold this is the
    /// rail's SETTLEMENT account (money arriving from the network).
    pub from_account: Uuid,
    pub amount: Decimal,
    pub reference: String,
    pub transaction_id: Uuid,
}

/// Where a released hold lands.
#[derive(Debug, Clone)]
pub enum Destination {
    /// A nano-bank customer account.
    Internal(Uuid),
    /// An external participant (institution number); settles through SETTLEMENT.
    External(String),
}

/// The result of a rail posting.
#[derive(Debug, Clone)]
pub struct RailPosting {
    pub transaction_id: Uuid,
    /// "backend:doc_id" from the Ledger core, when a GL post was made.
    pub gl_entry: Option<String>,
}

/// A payment rail. All methods run inside the caller's DB transaction so the
/// local legs and the GL post commit or roll back together.
#[async_trait]
pub trait Rail: Send + Sync {
    fn id(&self) -> RailId;

    /// Reserve `amount` from `from` into the rail's clearing account.
    /// Local: Dr `from` / Cr CLEARING. GL: Dr Payable / Cr Payable.
    async fn hold(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        from: Uuid,
        amount: Decimal,
        description: &str,
    ) -> Result<Hold, AppError>;

    /// Release a hold to its destination.
    /// Internal: Dr CLEARING / Cr account. External: Dr CLEARING / Cr SETTLEMENT.
    async fn release(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        hold: &Hold,
        dest: Destination,
        description: &str,
    ) -> Result<RailPosting, AppError>;

    /// Return a hold to its origin (Dr CLEARING / Cr `hold.from_account`).
    async fn refund(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        hold: &Hold,
        description: &str,
    ) -> Result<RailPosting, AppError>;

    /// Credit an incoming payment straight to a customer account (autodeposit
    /// fast path). Local: Dr SETTLEMENT / Cr `to`. GL: Dr Receivable / Cr Payable.
    async fn accept_inbound(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        to: Uuid,
        amount: Decimal,
        description: &str,
    ) -> Result<RailPosting, AppError>;
}
```

- [ ] **Step 4: Write `rails/interac.rs` — accounts, normalization, struct (impl in Task 4)**

```rust
//! Interac e-Transfer rail: the clearing/settlement plumbing. The product
//! lifecycle lives in `handlers/interac.rs`.

use rust_decimal::Decimal;
use uuid::Uuid;

use crate::config::database::DatabasePool;
use crate::models::interac::HandleType;

use super::{Hold, RailId};

/// Interac's own synthetic system customer — SEPARATE from the card rails'
/// `system@nano.bank`, because GL accounts are keyed by (customer, account_type)
/// and that customer already uses its chequing/savings for VISA_CLEARING /
/// BANK_SETTLEMENT.
const INTERAC_CUSTOMER_EMAIL: &str = "interac@nano.bank";
const CLEARING_TYPE: &str = "chequing"; // INTERAC_CLEARING
const SETTLEMENT_TYPE: &str = "savings"; // INTERAC_SETTLEMENT

#[derive(Clone, Copy, Debug)]
pub struct InteracAccounts {
    pub clearing_id: Uuid,
    pub settlement_id: Uuid,
}

/// The Interac rail. Carries the resolved clearing/settlement ids (re-resolved
/// per request by the handler, because a data wipe rebuilds them).
#[derive(Clone, Copy, Debug)]
pub struct InteracRail {
    pub accounts: InteracAccounts,
}

impl InteracRail {
    pub fn new(accounts: InteracAccounts) -> Self {
        Self { accounts }
    }
    pub fn id(&self) -> RailId {
        RailId::Interac
    }
}

/// Normalise a handle for storage/lookup: emails lowercased+trimmed; phones
/// reduced to a leading '+' (if present) and digits.
pub fn normalize_handle(handle_type: HandleType, raw: &str) -> String {
    match handle_type {
        HandleType::Email => raw.trim().to_lowercase(),
        HandleType::Phone => {
            let mut out = String::new();
            for (i, c) in raw.trim().chars().enumerate() {
                if c == '+' && i == 0 {
                    out.push('+');
                } else if c.is_ascii_digit() {
                    out.push(c);
                }
            }
            out
        }
    }
}

/// Create Interac's system customer + two GL accounts if absent; return ids.
/// Idempotent — mirrors `handlers::cards::ensure_system_accounts`.
pub async fn ensure_interac_accounts(
    pool: &DatabasePool,
) -> Result<InteracAccounts, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO customers (email, phone_number, first_name, last_name, date_of_birth, sin)
        VALUES ($1, '+10000000001', 'Nano', 'Interac', '1970-01-01', '000000001')
        ON CONFLICT (email) DO NOTHING
        "#,
    )
    .bind(INTERAC_CUSTOMER_EMAIL)
    .execute(pool)
    .await?;

    let customer_id: Uuid =
        sqlx::query_scalar("SELECT customer_id FROM customers WHERE email = $1")
            .bind(INTERAC_CUSTOMER_EMAIL)
            .fetch_one(pool)
            .await?;

    let clearing_id = ensure_gl_account(pool, customer_id, CLEARING_TYPE).await?;
    let settlement_id = ensure_gl_account(pool, customer_id, SETTLEMENT_TYPE).await?;
    tracing::info!(%clearing_id, %settlement_id, "✅ Interac GL accounts ready");
    Ok(InteracAccounts { clearing_id, settlement_id })
}

async fn ensure_gl_account(
    pool: &DatabasePool,
    customer_id: Uuid,
    account_type: &str,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO accounts
            (customer_id, account_number, account_type, status, overdraft_limit, activated_at)
        SELECT $1, '000000000000', $2::account_type, 'active', 1000000000000, CURRENT_TIMESTAMP
        WHERE NOT EXISTS (
            SELECT 1 FROM accounts WHERE customer_id = $1 AND account_type = $2::account_type
        )
        "#,
    )
    .bind(customer_id)
    .bind(account_type)
    .execute(pool)
    .await?;

    sqlx::query_scalar(
        "SELECT account_id FROM accounts WHERE customer_id = $1 AND account_type = $2::account_type \
         ORDER BY created_at LIMIT 1",
    )
    .bind(customer_id)
    .bind(account_type)
    .fetch_one(pool)
    .await
}

// keep the Hold type referenced so imports don't rot before Task 4
#[allow(unused_imports)]
use super::Destination as _Destination;
#[allow(dead_code)]
fn _hold_marker(_: &Hold) {}
```

> Note: `models::interac::HandleType` is created in Task 5. To compile Task 3 standalone, temporarily define `HandleType` at the top of `rails/interac.rs`; Task 5 moves it to `models/interac.rs` and this file imports it. If executing sequentially, add this stub now and delete it in Task 5:
> ```rust
> // TEMP until Task 5 — delete when models::interac::HandleType exists.
> #[derive(Debug, Clone, Copy, PartialEq, Eq)]
> pub enum HandleType { Email, Phone }
> ```
> and change the `use crate::models::interac::HandleType;` line to `use self::HandleType;`-free (drop the import). Task 5 restores the import.

- [ ] **Step 5: Wire the module + startup bootstrap in `main.rs`**

Add after `mod ledger;` (line 4):
```rust
mod rails;
```
Add after the `ensure_system_accounts` block (around line 80):
```rust
    // Bootstrap the Interac rail's clearing/settlement GL accounts (idempotent;
    // also re-resolved per request, so a mid-run wipe self-heals).
    if let Err(e) = rails::interac::ensure_interac_accounts(&pool).await {
        warn!("❌ Failed to bootstrap Interac GL accounts: {}", e);
        std::process::exit(1);
    }
```

- [ ] **Step 6: Run the unit test — verify it passes**

Run: `cd api && cargo test rails::interac::tests`
Expected: PASS (2 tests).

- [ ] **Step 7: Verify the accounts bootstrap end-to-end**

```bash
cd api && CORE_BACKEND=modern cargo run  # in another shell: modern core on :8091
# then:
kubectl exec -i -n nano-bank deployment/postgres -- psql -U nanobank_user -d nano_bank_db -c \
 "SELECT c.email, a.account_type FROM accounts a JOIN customers c ON c.customer_id=a.customer_id
  WHERE c.email='interac@nano.bank' ORDER BY a.account_type;"
```
Expected: two rows — `interac@nano.bank | chequing` and `interac@nano.bank | savings`.

- [ ] **Step 8: Commit**

```bash
git add api/src/rails/ api/src/main.rs
git commit -m "feat(rails): Rail port + Interac system accounts + handle normalization"
```

---

## Task 4: Implement `Rail` for `InteracRail`

**Files:**
- Modify: `api/src/rails/interac.rs` (add `impl Rail for InteracRail`)

**Interfaces:**
- Consumes: `handlers::cards::{post_two_legged, post_gl_entry, reference_number, Tx}`; `ledger::Account as GlAccount`.
- Produces: `InteracRail: Rail` — the four verbs, each creating a `transactions` row (types `interac_hold` / `interac_release` / `interac_refund` / `interac_inbound`), posting the local double-entry, and posting aggregate GL.

- [ ] **Step 1: Add the impl**

Append to `rails/interac.rs`:
```rust
use async_trait::async_trait;
use serde_json::json;

use crate::errors::AppError;
use crate::handlers::AppState;
use crate::handlers::cards::{post_gl_entry, post_two_legged, reference_number};
use crate::ledger::Account as GlAccount;
use super::{Destination, Hold, PgTx, Rail, RailId, RailPosting};

/// Create a completed `transactions` row for one rail movement; return its id.
async fn new_txn(
    tx: &mut PgTx<'_>,
    reference: &str,
    txn_type: &str,
    amount: Decimal,
    description: &str,
    initiated_by: Option<Uuid>,
) -> Result<Uuid, AppError> {
    let id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO transactions
            (reference_number, transaction_type, amount, description, status,
             initiated_by, completed_at, metadata)
        VALUES ($1, $2, $3, $4, 'completed', $5, CURRENT_TIMESTAMP, $6)
        RETURNING transaction_id
        "#,
    )
    .bind(reference)
    .bind(txn_type)
    .bind(amount)
    .bind(description)
    .bind(initiated_by)
    .bind(json!({ "rail": "interac" }))
    .fetch_one(&mut **tx)
    .await?;
    Ok(id)
}

async fn tag_gl(tx: &mut PgTx<'_>, txn_id: Uuid, gl: &str) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE transactions SET metadata = jsonb_set(COALESCE(metadata,'{}'::jsonb), \
         '{gl_entry}', to_jsonb($2::text)) WHERE transaction_id = $1",
    )
    .bind(txn_id)
    .bind(gl)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[async_trait]
impl Rail for InteracRail {
    fn id(&self) -> RailId {
        RailId::Interac
    }

    async fn hold(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        from: Uuid,
        amount: Decimal,
        description: &str,
    ) -> Result<Hold, AppError> {
        let reference = reference_number("ETRH");
        let txn_id = new_txn(tx, &reference, "interac_hold", amount, description, None).await?;
        // Dr from / Cr INTERAC_CLEARING (holds the funds).
        post_two_legged(tx, txn_id, from, "debit", self.accounts.clearing_id, "credit", amount).await?;
        // Aggregate GL: owed-to-customer → owed-to-clearing (Payable/Payable).
        let gl = post_gl_entry(state, &reference, description, GlAccount::Payable, GlAccount::Payable, amount).await?;
        tag_gl(tx, txn_id, &format!("{}:{}", gl.backend, gl.id)).await?;
        Ok(Hold { from_account: from, amount, reference, transaction_id: txn_id })
    }

    async fn release(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        hold: &Hold,
        dest: Destination,
        description: &str,
    ) -> Result<RailPosting, AppError> {
        let reference = reference_number("ETRR");
        let txn_id = new_txn(tx, &reference, "interac_release", hold.amount, description, None).await?;
        let credit_account = match dest {
            Destination::Internal(acct) => acct,
            Destination::External(_) => self.accounts.settlement_id,
        };
        // Dr INTERAC_CLEARING / Cr destination (recipient or settlement).
        post_two_legged(tx, txn_id, self.accounts.clearing_id, "debit", credit_account, "credit", hold.amount).await?;
        let gl = post_gl_entry(state, &reference, description, GlAccount::Payable, GlAccount::Payable, hold.amount).await?;
        tag_gl(tx, txn_id, &format!("{}:{}", gl.backend, gl.id)).await?;
        Ok(RailPosting { transaction_id: txn_id, gl_entry: Some(format!("{}:{}", gl.backend, gl.id)) })
    }

    async fn refund(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        hold: &Hold,
        description: &str,
    ) -> Result<RailPosting, AppError> {
        let reference = reference_number("ETRX");
        let txn_id = new_txn(tx, &reference, "interac_refund", hold.amount, description, None).await?;
        // Dr INTERAC_CLEARING / Cr origin (sender for outbound; settlement for inbound).
        post_two_legged(tx, txn_id, self.accounts.clearing_id, "debit", hold.from_account, "credit", hold.amount).await?;
        let gl = post_gl_entry(state, &reference, description, GlAccount::Payable, GlAccount::Payable, hold.amount).await?;
        tag_gl(tx, txn_id, &format!("{}:{}", gl.backend, gl.id)).await?;
        Ok(RailPosting { transaction_id: txn_id, gl_entry: Some(format!("{}:{}", gl.backend, gl.id)) })
    }

    async fn accept_inbound(
        &self,
        state: &AppState,
        tx: &mut PgTx<'_>,
        to: Uuid,
        amount: Decimal,
        description: &str,
    ) -> Result<RailPosting, AppError> {
        let reference = reference_number("ETRI");
        let txn_id = new_txn(tx, &reference, "interac_inbound", amount, description, None).await?;
        // Dr INTERAC_SETTLEMENT / Cr recipient (network → customer).
        post_two_legged(tx, txn_id, self.accounts.settlement_id, "debit", to, "credit", amount).await?;
        // GL: network owes us (Receivable) → customer payable.
        let gl = post_gl_entry(state, &reference, description, GlAccount::Receivable, GlAccount::Payable, amount).await?;
        tag_gl(tx, txn_id, &format!("{}:{}", gl.backend, gl.id)).await?;
        Ok(RailPosting { transaction_id: txn_id, gl_entry: Some(format!("{}:{}", gl.backend, gl.id)) })
    }
}
```

- [ ] **Step 2: Delete the `_hold_marker` / `_Destination` placeholders from Task 3 Step 4** (now that the types are used).

- [ ] **Step 3: Type-check**

Run: `cd api && cargo check`
Expected: compiles (pre-existing dead-code warnings from stub handlers are fine).

- [ ] **Step 4: Commit**

```bash
git add api/src/rails/interac.rs
git commit -m "feat(interac): implement Rail (hold/release/refund/accept_inbound) with dual-post"
```

---

## Task 5: Interac models (`models/interac.rs`)

**Files:**
- Create: `api/src/models/interac.rs`
- Modify: `api/src/models/mod.rs` (add `pub mod interac;`)
- Modify: `api/src/rails/interac.rs` (remove the TEMP `HandleType`, import from models)

**Interfaces:**
- Produces: `HandleType` (email|phone, `sqlx::Type`), request/response DTOs used by the handlers in Tasks 7–14. Exact names below — later tasks depend on them.

- [ ] **Step 1: Write the models**

```rust
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use validator::Validate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "interac_handle_type", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum HandleType {
    Email,
    Phone,
}

#[derive(Debug, Deserialize, Validate)]
pub struct RegisterAutodepositRequest {
    pub handle_type: HandleType,
    #[validate(length(min = 3, max = 255))]
    pub handle_value: String,
    pub deposit_account_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct HandleResponse {
    pub handle_id: Uuid,
    pub handle_type: HandleType,
    pub handle_value: String,
    pub autodeposit_account_id: Option<Uuid>,
    pub active: bool,
}

#[derive(Debug, Deserialize, Validate)]
pub struct SendEtransferRequest {
    pub from_account_id: Uuid,
    pub amount: Decimal,
    pub recipient_handle_type: HandleType,
    #[validate(length(min = 3, max = 255))]
    pub recipient_handle_value: String,
    /// Required unless the recipient handle has autodeposit enabled.
    pub security_question: Option<String>,
    pub security_answer: Option<String>,
    pub memo: Option<String>,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ClaimEtransferRequest {
    pub security_answer: String,
    pub deposit_account_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct InboundEtransferRequest {
    pub amount: Decimal,
    pub sender_name: String,
    pub counterparty_institution: String,
    pub recipient_handle_type: HandleType,
    pub recipient_handle_value: String,
    pub security_question: Option<String>,
    pub security_answer: Option<String>,
    pub memo: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SettleEtransferRequest {
    /// "claimed" | "declined"
    pub outcome: String,
    pub institution: String,
}

#[derive(Debug, Serialize)]
pub struct EtransferResponse {
    pub etransfer_id: Uuid,
    pub direction: String,
    pub status: String,
    pub amount: Decimal,
    pub recipient_handle_value: String,
    pub security_question: Option<String>,
    pub memo: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}
```

- [ ] **Step 2: Register the module & fix the rail import**

`models/mod.rs`: add `pub mod interac;`.
In `rails/interac.rs`: delete the TEMP `HandleType` enum and restore `use crate::models::interac::HandleType;`.

- [ ] **Step 3: Type-check + re-run unit tests**

Run: `cd api && cargo test rails::interac::tests && cargo check`
Expected: 2 tests PASS; compiles.

- [ ] **Step 4: Commit**

```bash
git add api/src/models/interac.rs api/src/models/mod.rs api/src/rails/interac.rs
git commit -m "feat(interac): request/response models + HandleType"
```

---

## Task 6: Handler scaffold, routes & wiring

**Files:**
- Create: `api/src/handlers/interac.rs`
- Modify: `api/src/handlers/mod.rs` (add `pub mod interac;`)
- Modify: `api/src/main.rs` (nest `/api/v1/interac`)

**Interfaces:**
- Produces: `handlers::interac::interac_routes() -> Router<AppState>` with all endpoints registered (bodies filled in Tasks 7–14). A small shared helper `resolve_interac(&AppState) -> InteracRail`.

- [ ] **Step 1: Write the scaffold with the router and shared helpers**

```rust
//! Interac e-Transfer product lifecycle. Money movement goes through the Rail
//! port (`rails::interac::InteracRail`); this module owns handle resolution,
//! the claim/decline/cancel/expiry state machine, and notifications.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::{delete, get, post},
    Router,
};
use uuid::Uuid;

use crate::errors::AppError;
use crate::handlers::AppState;
use crate::middleware::auth::{AuthenticatedCustomer, AuthenticatedService};
use crate::rails::interac::{ensure_interac_accounts, InteracRail};

pub fn interac_routes() -> Router<AppState> {
    Router::new()
        // customer plane
        .route("/etransfers", post(send_etransfer).get(list_etransfers))
        .route("/etransfers/:id", get(get_etransfer))
        .route("/etransfers/:id/claim", post(claim_etransfer))
        .route("/etransfers/:id/decline", post(decline_etransfer))
        .route("/etransfers/:id/cancel", post(cancel_etransfer))
        .route("/autodeposit", post(register_autodeposit).get(list_autodeposit))
        .route("/autodeposit/:id", delete(deregister_autodeposit))
        // network plane (service token)
        .route("/network/inbound", post(network_inbound))
        .route("/network/etransfers/:id/settle", post(network_settle))
        // admin plane (service token)
        .route("/admin/sweep-expired", post(sweep_expired))
}

/// Resolve Interac's clearing/settlement accounts (re-resolved per request) and
/// build the rail.
async fn resolve_interac(state: &AppState) -> Result<InteracRail, AppError> {
    let accts = ensure_interac_accounts(&state.pool).await?;
    Ok(InteracRail::new(accts))
}

/// Interac's default hold lifetime before auto-expiry (real Interac: 30 days).
fn expiry_days() -> i64 {
    std::env::var("NANO_BANK__INTERAC__EXPIRY_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
}

/// Max amount per e-Transfer (funds check aside). Default $3,000 like real Interac.
fn max_amount() -> rust_decimal::Decimal {
    std::env::var("NANO_BANK__INTERAC__MAX_ETRANSFER_AMOUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| rust_decimal::Decimal::new(3000, 0))
}
```

Add empty handler stubs so it compiles (each returns `AppError::Internal("todo")` — replaced in later tasks):
```rust
async fn send_etransfer() -> Result<StatusCode, AppError> { Err(AppError::Internal("todo".into())) }
// ...repeat one stub per route: list_etransfers, get_etransfer, claim_etransfer,
// decline_etransfer, cancel_etransfer, register_autodeposit, list_autodeposit,
// deregister_autodeposit, network_inbound, network_settle, sweep_expired
```

> Each stub is replaced wholesale in its task; they exist only so `cargo check` passes now.

- [ ] **Step 2: Wire module + route**

`handlers/mod.rs`: add `pub mod interac;`.
`main.rs`: add after the cards nest (line 145):
```rust
        .nest("/api/v1/interac", handlers::interac::interac_routes())
```

- [ ] **Step 3: Type-check**

Run: `cd api && cargo check`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add api/src/handlers/interac.rs api/src/handlers/mod.rs api/src/main.rs
git commit -m "feat(interac): handler scaffold, routes, and router wiring"
```

---

## Task 7: Autodeposit registration endpoints

**Files:**
- Modify: `api/src/handlers/interac.rs`

**Interfaces:**
- Consumes: `models::interac::{RegisterAutodepositRequest, HandleResponse}`, `normalize_handle`, `AuthenticatedCustomer`.
- Produces: `POST /autodeposit` (201), `GET /autodeposit` (200 list), `DELETE /autodeposit/:id` (204). A handle is owned by the caller; ownership enforced.

- [ ] **Step 1: Implement the three handlers**

```rust
use axum::extract::rejection::JsonRejection;
use axum::Json as AxumJson;
use validator::Validate;
use crate::models::interac::{HandleResponse, RegisterAutodepositRequest};
use crate::rails::interac::normalize_handle;

async fn register_autodeposit(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    AxumJson(req): AxumJson<RegisterAutodepositRequest>,
) -> Result<(StatusCode, Json<HandleResponse>), AppError> {
    req.validate()?;
    let handle = normalize_handle(req.handle_type, &req.handle_value);

    // The deposit account must belong to the caller.
    let owns: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE account_id=$1 AND customer_id=$2)",
    )
    .bind(req.deposit_account_id)
    .bind(caller.customer_id)
    .fetch_one(&state.pool)
    .await?;
    if !owns {
        return Err(AppError::NotFound("deposit account not found".into()));
    }

    let row = sqlx::query_as::<_, (Uuid, Option<Uuid>, bool)>(
        r#"
        INSERT INTO interac_handles (customer_id, handle_type, handle_value, autodeposit_account_id)
        VALUES ($1, $2, $3, $4)
        RETURNING handle_id, autodeposit_account_id, active
        "#,
    )
    .bind(caller.customer_id)
    .bind(req.handle_type)
    .bind(&handle)
    .bind(req.deposit_account_id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            AppError::Conflict("handle already registered".into())
        }
        _ => AppError::from(e),
    })?;

    Ok((
        StatusCode::CREATED,
        Json(HandleResponse {
            handle_id: row.0,
            handle_type: req.handle_type,
            handle_value: handle,
            autodeposit_account_id: row.1,
            active: row.2,
        }),
    ))
}

async fn list_autodeposit(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
) -> Result<Json<Vec<HandleResponse>>, AppError> {
    let rows = sqlx::query_as::<_, (Uuid, crate::models::interac::HandleType, String, Option<Uuid>, bool)>(
        "SELECT handle_id, handle_type, handle_value, autodeposit_account_id, active \
         FROM interac_handles WHERE customer_id=$1 ORDER BY created_at",
    )
    .bind(caller.customer_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, ht, hv, ad, active)| HandleResponse {
                handle_id: id, handle_type: ht, handle_value: hv,
                autodeposit_account_id: ad, active,
            })
            .collect(),
    ))
}

async fn deregister_autodeposit(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Path(handle_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let n = sqlx::query("DELETE FROM interac_handles WHERE handle_id=$1 AND customer_id=$2")
        .bind(handle_id)
        .bind(caller.customer_id)
        .execute(&state.pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound("handle not found".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}
```
Delete the corresponding stubs.

- [ ] **Step 2: Type-check**

Run: `cd api && cargo check`
Expected: compiles.

- [ ] **Step 3: Smoke-test (stack + core running)**

```bash
curl -sS -X POST localhost:8081/api/v1/interac/autodeposit -H "Authorization: Bearer $TOK" \
  -H 'content-type: application/json' \
  -d '{"handle_type":"email","handle_value":"Alice@Example.com","deposit_account_id":"'$ACC'"}' | jq
```
Expected: 201, JSON with `"handle_value":"alice@example.com"` and the `autodeposit_account_id` set.
Re-POST the same handle → `409 CONFLICT`. `GET` → array of 1. `DELETE /autodeposit/<id>` → 204.

- [ ] **Step 4: Commit**

```bash
git add api/src/handlers/interac.rs
git commit -m "feat(interac): autodeposit registration endpoints"
```

---

## Task 8: Send e-Transfer (`POST /etransfers`)

**Files:**
- Modify: `api/src/handlers/interac.rs`

**Interfaces:**
- Consumes: `SendEtransferRequest`, `EtransferResponse`, `InteracRail::hold` / `accept_inbound`... (outbound only here), `hash_password`, `resolve_interac`, `normalize_handle`, `expiry_days`, `max_amount`.
- Produces: `POST /etransfers` (201). Resolution: recipient handle **registered + autodeposit** → hold then immediately release Internal → status `deposited`; registered **without** autodeposit → hold, status `available`, notification; **unregistered** → hold + release External(settlement) is deferred until the far side settles (Task 13), so status `available` + external notification with `counterparty_institution=null` until settle.

- [ ] **Step 1: Implement `send_etransfer`**

```rust
use rust_decimal::Decimal;
use crate::handlers::cards::{fetch_account_for_update, normalize_amount};
use crate::models::interac::{EtransferResponse, HandleType, SendEtransferRequest};
use crate::rails::{Destination};
use crate::utils::password::hash_password;

async fn send_etransfer(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    AxumJson(req): AxumJson<SendEtransferRequest>,
) -> Result<(StatusCode, Json<EtransferResponse>), AppError> {
    req.validate()?;
    let amount = normalize_amount(req.amount)?;
    if amount > max_amount() {
        return Err(AppError::BadRequest(format!("amount exceeds per-transfer max {}", max_amount())));
    }
    let recipient_handle = normalize_handle(req.recipient_handle_type, &req.recipient_handle_value);
    let rail = resolve_interac(&state).await?;

    // Idempotency replay: same (sender, key) returns the original.
    if let Some(key) = &req.idempotency_key {
        if let Some(existing) = load_etransfer_by_key(&state, caller.customer_id, key).await? {
            return Ok((StatusCode::CREATED, Json(existing)));
        }
    }

    // Look up whether the recipient handle is registered here, and autodeposit.
    let registration = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
        "SELECT customer_id, autodeposit_account_id FROM interac_handles \
         WHERE handle_value=$1 AND active=TRUE",
    )
    .bind(&recipient_handle)
    .fetch_optional(&state.pool)
    .await?;

    // Non-autodeposit transfers require a security question + answer.
    let autodeposit = registration.as_ref().and_then(|(_, ad)| *ad);
    let (question, answer_hash) = if autodeposit.is_some() {
        (None, None)
    } else {
        let q = req.security_question.clone()
            .ok_or_else(|| AppError::BadRequest("security_question required (recipient has no autodeposit)".into()))?;
        let a = req.security_answer.clone()
            .ok_or_else(|| AppError::BadRequest("security_answer required".into()))?;
        (Some(q), Some(hash_password(&a.to_lowercase())?))
    };

    let mut tx = state.pool.begin().await?;

    // Fund the hold: sender account must belong to caller, be active, and have funds.
    let sender = fetch_account_for_update(&mut tx, req.from_account_id).await?
        .ok_or_else(|| AppError::NotFound("account not found".into()))?;
    if sender.customer_id != caller.customer_id {
        return Err(AppError::NotFound("account not found".into())); // 404, not 403
    }
    if amount > sender.available_balance {
        return Err(AppError::InsufficientFunds);
    }

    let hold = rail.hold(&state, &mut tx, sender.account_id, amount,
        &format!("Interac e-Transfer to {recipient_handle}")).await?;

    // Create the etransfer row (outbound, held).
    let claim_token = crate::handlers::cards::reference_number("CLM");
    let etransfer_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO interac_etransfers
            (direction, status, amount, sender_customer_id, sender_account_id,
             recipient_handle_type, recipient_handle_value, recipient_customer_id,
             security_question, security_answer_hash, claim_token, memo,
             hold_transaction_id, idempotency_key, expires_at)
        VALUES ('outbound','held',$1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,
                CURRENT_TIMESTAMP + ($13 || ' days')::interval)
        RETURNING etransfer_id
        "#,
    )
    .bind(amount).bind(caller.customer_id).bind(sender.account_id)
    .bind(req.recipient_handle_type).bind(&recipient_handle)
    .bind(registration.as_ref().map(|(c, _)| *c))
    .bind(&question).bind(&answer_hash).bind(&claim_token).bind(&req.memo)
    .bind(hold.transaction_id).bind(&req.idempotency_key)
    .bind(expiry_days().to_string())
    .fetch_one(&mut *tx)
    .await
    .map_err(idempotency_conflict)?;

    // Route based on the recipient.
    let status = match (registration.as_ref(), autodeposit) {
        (Some((recipient_customer, _)), Some(deposit_acct)) => {
            // Autodeposit: release into their account immediately.
            let _ = recipient_customer;
            rail.release(&state, &mut tx, &hold, Destination::Internal(deposit_acct),
                "Interac e-Transfer autodeposit").await?;
            mark_deposited(&mut tx, etransfer_id, deposit_acct).await?;
            notify(&mut tx, etransfer_id, &recipient_handle, "deposit_completed",
                &format!("${amount} was automatically deposited"), None).await?;
            "deposited"
        }
        (Some(_), None) => {
            // Registered here, manual claim.
            set_available(&mut tx, etransfer_id).await?;
            notify(&mut tx, etransfer_id, &recipient_handle, "incoming_transfer",
                &format!("You have an Interac e-Transfer of ${amount}"), Some(&claim_token)).await?;
            "available"
        }
        (None, _) => {
            // External recipient — the network (simulator) settles later (Task 13).
            set_available(&mut tx, etransfer_id).await?;
            notify(&mut tx, etransfer_id, &recipient_handle, "incoming_transfer",
                &format!("You have an Interac e-Transfer of ${amount}"), Some(&claim_token)).await?;
            "available"
        }
    };

    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(load_etransfer(&state, etransfer_id).await?)))
    .map(|(s, j)| { let _ = status; (s, j) })
}
```

- [ ] **Step 2: Add the shared DB helpers used above** (append to the module)

```rust
async fn set_available(tx: &mut crate::rails::PgTx<'_>, id: Uuid) -> Result<(), AppError> {
    sqlx::query("UPDATE interac_etransfers SET status='available', notified_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
        .bind(id).execute(&mut **tx).await?;
    Ok(())
}

async fn mark_deposited(tx: &mut crate::rails::PgTx<'_>, id: Uuid, account: Uuid) -> Result<(), AppError> {
    sqlx::query("UPDATE interac_etransfers SET status='deposited', recipient_account_id=$2, resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
        .bind(id).bind(account).execute(&mut **tx).await?;
    Ok(())
}

async fn notify(tx: &mut crate::rails::PgTx<'_>, etransfer_id: Uuid, handle: &str,
    kind: &str, message: &str, claim_token: Option<&str>) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO interac_notifications (etransfer_id, handle_value, kind, message, claim_token) \
         VALUES ($1,$2,$3::interac_notification_kind,$4,$5)",
    )
    .bind(etransfer_id).bind(handle).bind(kind).bind(message).bind(claim_token)
    .execute(&mut **tx).await?;
    Ok(())
}

fn idempotency_conflict(e: sqlx::Error) -> AppError {
    match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") =>
            AppError::Conflict("idempotency_key already used with different parameters".into()),
        _ => AppError::from(e),
    }
}

async fn load_etransfer(state: &AppState, id: Uuid) -> Result<EtransferResponse, AppError> {
    let r = sqlx::query_as::<_, (Uuid, String, String, Decimal, String, Option<String>, Option<String>,
        Option<chrono::DateTime<chrono::Utc>>, chrono::DateTime<chrono::Utc>)>(
        "SELECT etransfer_id, direction::text, status::text, amount, recipient_handle_value, \
         security_question, memo, expires_at, created_at FROM interac_etransfers WHERE etransfer_id=$1",
    )
    .bind(id).fetch_one(&state.pool).await?;
    Ok(EtransferResponse {
        etransfer_id: r.0, direction: r.1, status: r.2, amount: r.3,
        recipient_handle_value: r.4, security_question: r.5, memo: r.6,
        expires_at: r.7, created_at: r.8,
    })
}

async fn load_etransfer_by_key(state: &AppState, sender: Uuid, key: &str)
    -> Result<Option<EtransferResponse>, AppError> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT etransfer_id FROM interac_etransfers WHERE sender_customer_id=$1 AND idempotency_key=$2",
    ).bind(sender).bind(key).fetch_optional(&state.pool).await?;
    match id { Some(i) => Ok(Some(load_etransfer(state, i).await?)), None => Ok(None) }
}
```

> Simplify the `send_etransfer` tail: replace the awkward `.map(...)` line with a plain
> `let _ = status; tx.commit().await?; Ok((StatusCode::CREATED, Json(load_etransfer(&state, etransfer_id).await?)))` — move `tx.commit()` before the final `load_etransfer` (which reads via pool). Keep `status` for the tracing log: `tracing::info!(%etransfer_id, status, "📨 e-Transfer sent");`.

- [ ] **Step 3: Type-check**

Run: `cd api && cargo check`
Expected: compiles.

- [ ] **Step 4: Smoke-test all three routes**

```bash
# autodeposit path: register bob's handle w/ autodeposit, then send to it
curl -sS -X POST localhost:8081/api/v1/interac/etransfers -H "Authorization: Bearer $TOK" \
  -H 'content-type: application/json' \
  -d '{"from_account_id":"'$ACC'","amount":25.00,"recipient_handle_type":"email","recipient_handle_value":"bob@autodep.ca"}' | jq .status
# → "deposited"

# claim path: unregistered recipient requires a security question
curl -sS -X POST localhost:8081/api/v1/interac/etransfers -H "Authorization: Bearer $TOK" \
  -H 'content-type: application/json' \
  -d '{"from_account_id":"'$ACC'","amount":40.00,"recipient_handle_type":"email","recipient_handle_value":"carol@elsewhere.ca","security_question":"fav colour","security_answer":"Blue"}' | jq .status
# → "available"
```
Verify balances moved / held:
```bash
kubectl exec -i -n nano-bank deployment/postgres -- psql -U nanobank_user -d nano_bank_db -c \
 "SELECT status, amount FROM interac_etransfers ORDER BY created_at DESC LIMIT 2;
  SELECT a.account_type, a.balance FROM accounts a JOIN customers c ON c.customer_id=a.customer_id
  WHERE c.email='interac@nano.bank';"
```
Expected: the held (`available`) $40 shows as a positive `INTERAC_CLEARING` (chequing) balance; the autodeposited $25 nets back out of clearing.

- [ ] **Step 5: Commit**

```bash
git add api/src/handlers/interac.rs
git commit -m "feat(interac): send e-Transfer (autodeposit / claim / external routing) + idempotency"
```

---

## Task 9: Claim & Decline (`POST /etransfers/:id/claim|decline`)

**Files:**
- Modify: `api/src/handlers/interac.rs`

**Interfaces:**
- Consumes: `ClaimEtransferRequest`, `verify_password`, `InteracRail::{release, refund}`.
- Produces: `claim` (200) — verify security answer (3-strike lock), release hold to the caller's chosen account, status `deposited`; `decline` (200) — refund the sender, status `declined`. Both guard the `available → …` transition so concurrent claim/cancel/expire race to a single winner (others `409`).

- [ ] **Step 1: Implement claim & decline**

```rust
use crate::models::interac::ClaimEtransferRequest;
use crate::utils::password::verify_password;

/// Lock an available e-Transfer FOR UPDATE and return the fields we need, or the
/// right error (404 unknown, 409 if no longer 'available').
async fn lock_available(tx: &mut crate::rails::PgTx<'_>, id: Uuid)
    -> Result<(Decimal, Uuid, Option<Uuid>, Option<String>, i32, String), AppError> {
    let row = sqlx::query_as::<_, (String, Decimal, Option<Uuid>, Option<Uuid>, Option<String>, i32, String)>(
        "SELECT status::text, amount, sender_account_id, recipient_customer_id, \
         security_answer_hash, wrong_answer_attempts, \
         COALESCE((SELECT reference_number FROM transactions WHERE transaction_id=hold_transaction_id),'') \
         FROM interac_etransfers WHERE etransfer_id=$1 FOR UPDATE",
    )
    .bind(id).fetch_optional(&mut **tx).await?
    .ok_or_else(|| AppError::NotFound("e-Transfer not found".into()))?;
    if row.0 != "available" {
        return Err(AppError::Conflict(format!("e-Transfer is {}", row.0)));
    }
    Ok((row.1, row.2.unwrap_or_default(), row.3, row.4, row.5, row.6))
}

async fn claim_etransfer(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Path(id): Path<Uuid>,
    AxumJson(req): AxumJson<ClaimEtransferRequest>,
) -> Result<(StatusCode, Json<EtransferResponse>), AppError> {
    let rail = resolve_interac(&state).await?;
    let mut tx = state.pool.begin().await?;
    let (amount, sender_account, _rcpt, answer_hash, attempts, hold_ref) = lock_available(&mut tx, id).await?;

    // The deposit account must belong to the caller.
    let owns: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE account_id=$1 AND customer_id=$2)")
        .bind(req.deposit_account_id).bind(caller.customer_id).fetch_one(&mut *tx).await?;
    if !owns { return Err(AppError::NotFound("deposit account not found".into())); }

    // Verify the security answer (case-insensitive), 3-strike lock.
    if let Some(hash) = &answer_hash {
        if !verify_password(&req.security_answer.to_lowercase(), hash)? {
            let n = attempts + 1;
            if n >= 3 {
                sqlx::query("UPDATE interac_etransfers SET status='failed', wrong_answer_attempts=$2, resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
                    .bind(id).bind(n).execute(&mut *tx).await?;
                tx.commit().await?;
                return Err(AppError::Authorization("too many incorrect answers; e-Transfer locked".into()));
            }
            sqlx::query("UPDATE interac_etransfers SET wrong_answer_attempts=$2 WHERE etransfer_id=$1")
                .bind(id).bind(n).execute(&mut *tx).await?;
            tx.commit().await?;
            return Err(AppError::BadRequest("incorrect security answer".into()));
        }
    }

    let hold = crate::rails::Hold {
        from_account: sender_account, amount, reference: hold_ref,
        transaction_id: Uuid::nil(),
    };
    rail.release(&state, &mut tx, &hold, crate::rails::Destination::Internal(req.deposit_account_id),
        "Interac e-Transfer claim").await?;
    mark_deposited(&mut tx, id, req.deposit_account_id).await?;
    let handle: String = sqlx::query_scalar("SELECT recipient_handle_value FROM interac_etransfers WHERE etransfer_id=$1")
        .bind(id).fetch_one(&mut *tx).await?;
    notify(&mut tx, id, &handle, "deposit_completed", &format!("${amount} deposited"), None).await?;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(load_etransfer(&state, id).await?)))
}

async fn decline_etransfer(
    State(state): State<AppState>,
    _caller: AuthenticatedCustomer,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<EtransferResponse>), AppError> {
    let rail = resolve_interac(&state).await?;
    let mut tx = state.pool.begin().await?;
    let (amount, sender_account, _r, _h, _a, hold_ref) = lock_available(&mut tx, id).await?;
    let hold = crate::rails::Hold { from_account: sender_account, amount, reference: hold_ref, transaction_id: Uuid::nil() };
    rail.refund(&state, &mut tx, &hold, "Interac e-Transfer declined").await?;
    sqlx::query("UPDATE interac_etransfers SET status='declined', resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
        .bind(id).execute(&mut *tx).await?;
    let handle: String = sqlx::query_scalar("SELECT recipient_handle_value FROM interac_etransfers WHERE etransfer_id=$1")
        .bind(id).fetch_one(&mut *tx).await?;
    notify(&mut tx, id, &handle, "declined", &format!("${amount} was declined and returned"), None).await?;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(load_etransfer(&state, id).await?)))
}
```

> The `FOR UPDATE` + status re-check inside the tx is the concurrency guard: two concurrent claim/decline/cancel calls serialize on the row lock; the first flips `available`, the second sees a non-`available` status and gets `409`.

- [ ] **Step 2: Type-check + smoke-test**

Run: `cd api && cargo check`
```bash
# from Task 8 the $40 to carol is 'available'; grab its id + claim it as carol
EID=<etransfer_id>
curl -sS -X POST localhost:8081/api/v1/interac/etransfers/$EID/claim -H "Authorization: Bearer $CAROL_TOK" \
  -H 'content-type: application/json' -d '{"security_answer":"blue","deposit_account_id":"'$CAROL_ACC'"}' | jq .status
# → "deposited"; a wrong answer → 400, third wrong → 403 + status 'failed'
```
Expected: correct answer deposits; `INTERAC_CLEARING` balance drops back by $40; `carol`'s account balance rises by $40.

- [ ] **Step 3: Commit**

```bash
git add api/src/handlers/interac.rs
git commit -m "feat(interac): claim (security Q&A, 3-strike) + decline with concurrency guard"
```

---

## Task 10: Cancel (`POST /etransfers/:id/cancel`)

**Files:**
- Modify: `api/src/handlers/interac.rs`

**Interfaces:**
- Produces: `cancel` (200) — the **sender** cancels an `available` transfer before it's claimed; refunds the sender, status `cancelled`. Ownership: caller must be `sender_customer_id` (else 404). Same `available`-guard as claim/decline.

- [ ] **Step 1: Implement cancel**

```rust
async fn cancel_etransfer(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<EtransferResponse>), AppError> {
    let rail = resolve_interac(&state).await?;
    let mut tx = state.pool.begin().await?;

    // Ownership check folded into the lock: only the sender may cancel.
    let sender: Option<Uuid> = sqlx::query_scalar(
        "SELECT sender_customer_id FROM interac_etransfers WHERE etransfer_id=$1 FOR UPDATE")
        .bind(id).fetch_optional(&mut *tx).await?
        .ok_or_else(|| AppError::NotFound("e-Transfer not found".into()))?;
    if sender != Some(caller.customer_id) {
        return Err(AppError::NotFound("e-Transfer not found".into())); // 404, not 403
    }
    let (amount, sender_account, _r, _h, _a, hold_ref) = lock_available(&mut tx, id).await?;
    let hold = crate::rails::Hold { from_account: sender_account, amount, reference: hold_ref, transaction_id: Uuid::nil() };
    rail.refund(&state, &mut tx, &hold, "Interac e-Transfer cancelled").await?;
    sqlx::query("UPDATE interac_etransfers SET status='cancelled', resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
        .bind(id).execute(&mut *tx).await?;
    let handle: String = sqlx::query_scalar("SELECT recipient_handle_value FROM interac_etransfers WHERE etransfer_id=$1")
        .bind(id).fetch_one(&mut *tx).await?;
    notify(&mut tx, id, &handle, "cancelled", &format!("${amount} transfer was cancelled"), None).await?;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(load_etransfer(&state, id).await?)))
}
```

> `lock_available` re-locks the same row (`FOR UPDATE` is re-entrant within the tx) and enforces the `available` status. Sender check runs first so a non-sender always gets 404.

- [ ] **Step 2: Type-check + smoke-test**

Run: `cd api && cargo check`
```bash
# send a fresh claim-path transfer, then cancel it as the sender
curl -sS -X POST localhost:8081/api/v1/interac/etransfers/$EID/cancel -H "Authorization: Bearer $TOK" | jq .status
# → "cancelled"; sender balance restored; a second cancel → 409
```

- [ ] **Step 3: Commit**

```bash
git add api/src/handlers/interac.rs
git commit -m "feat(interac): sender cancel with ownership + available guard"
```

---

## Task 11: List & Get (`GET /etransfers`, `GET /etransfers/:id`)

**Files:**
- Modify: `api/src/handlers/interac.rs`

**Interfaces:**
- Consumes: `EtransferResponse`.
- Produces: `GET /etransfers` — the caller's sent **and** received transfers (by `sender_customer_id` OR `recipient_customer_id`), newest first, optional `?status=` filter; `GET /etransfers/:id` — single, visible only to sender or recipient (else 404).

- [ ] **Step 1: Implement list & get**

```rust
use std::collections::HashMap;

async fn list_etransfers(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<EtransferResponse>>, AppError> {
    let status = params.get("status").cloned();
    let rows = sqlx::query_as::<_, (Uuid, String, String, Decimal, String, Option<String>, Option<String>,
        Option<chrono::DateTime<chrono::Utc>>, chrono::DateTime<chrono::Utc>)>(
        "SELECT etransfer_id, direction::text, status::text, amount, recipient_handle_value, \
         security_question, memo, expires_at, created_at FROM interac_etransfers \
         WHERE (sender_customer_id=$1 OR recipient_customer_id=$1) \
           AND ($2::text IS NULL OR status::text=$2) \
         ORDER BY created_at DESC LIMIT 100",
    )
    .bind(caller.customer_id).bind(&status)
    .fetch_all(&state.pool).await?;
    Ok(Json(rows.into_iter().map(|r| EtransferResponse {
        etransfer_id: r.0, direction: r.1, status: r.2, amount: r.3,
        recipient_handle_value: r.4, security_question: r.5, memo: r.6, expires_at: r.7, created_at: r.8,
    }).collect()))
}

async fn get_etransfer(
    State(state): State<AppState>,
    caller: AuthenticatedCustomer,
    Path(id): Path<Uuid>,
) -> Result<Json<EtransferResponse>, AppError> {
    let visible: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM interac_etransfers WHERE etransfer_id=$1 \
         AND (sender_customer_id=$2 OR recipient_customer_id=$2))")
        .bind(id).bind(caller.customer_id).fetch_one(&state.pool).await?;
    if !visible { return Err(AppError::NotFound("e-Transfer not found".into())); }
    Ok(Json(load_etransfer(&state, id).await?))
}
```

- [ ] **Step 2: Type-check + smoke-test**

Run: `cd api && cargo check`
```bash
curl -sS localhost:8081/api/v1/interac/etransfers -H "Authorization: Bearer $TOK" | jq length
curl -sS localhost:8081/api/v1/interac/etransfers?status=deposited -H "Authorization: Bearer $TOK" | jq '.[0].status'
```
Expected: array of the caller's transfers; filter narrows to `deposited`; another customer's `GET /etransfers/:id` → 404.

- [ ] **Step 3: Commit**

```bash
git add api/src/handlers/interac.rs
git commit -m "feat(interac): list + single e-Transfer (ownership-scoped)"
```

---

## Task 12: Network inbound (`POST /network/inbound`)

**Files:**
- Modify: `api/src/handlers/interac.rs`

**Interfaces:**
- Consumes: `InboundEtransferRequest`, `AuthenticatedService`, `InteracRail::{hold, accept_inbound}`, `hash_password`.
- Produces: `network_inbound` (201) — an external bank sends a transfer **into** nano-bank. If the recipient handle is registered with autodeposit → `accept_inbound` (Dr SETTLEMENT / Cr recipient), status `deposited`. Otherwise → an inbound hold from SETTLEMENT into CLEARING, status `available`, notification; the recipient later claims it (Task 9's `claim`, which releases from CLEARING to their account). If the handle isn't registered at nano-bank at all → 404 (we can't route it).

- [ ] **Step 1: Implement network_inbound**

```rust
use crate::models::interac::InboundEtransferRequest;

async fn network_inbound(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
    AxumJson(req): AxumJson<InboundEtransferRequest>,
) -> Result<(StatusCode, Json<EtransferResponse>), AppError> {
    let amount = normalize_amount(req.amount)?;
    let handle = normalize_handle(req.recipient_handle_type, &req.recipient_handle_value);
    let rail = resolve_interac(&state).await?;

    // The recipient must be a known nano-bank handle.
    let reg = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
        "SELECT customer_id, autodeposit_account_id FROM interac_handles WHERE handle_value=$1 AND active=TRUE")
        .bind(&handle).fetch_optional(&state.pool).await?
        .ok_or_else(|| AppError::NotFound("recipient handle not registered at this institution".into()))?;

    let answer_hash = match &req.security_answer {
        Some(a) => Some(hash_password(&a.to_lowercase())?),
        None => None,
    };
    let claim_token = crate::handlers::cards::reference_number("CLM");

    let mut tx = state.pool.begin().await?;

    if let Some(deposit_acct) = reg.1 {
        // Autodeposit fast path.
        let posting = rail.accept_inbound(&state, &mut tx, deposit_acct, amount,
            &format!("Interac e-Transfer from {}", req.sender_name)).await?;
        let id = insert_inbound(&mut tx, amount, &req, &handle, reg.0, &claim_token,
            None, Some(deposit_acct), Some(posting.transaction_id), "deposited").await?;
        notify(&mut tx, id, &handle, "deposit_completed", &format!("${amount} auto-deposited"), None).await?;
        tx.commit().await?;
        return Ok((StatusCode::CREATED, Json(load_etransfer(&state, id).await?)));
    }

    // Held path: money arrives from the network into clearing (from = SETTLEMENT).
    let hold = rail.hold(&state, &mut tx, rail.accounts.settlement_id, amount,
        &format!("Interac inbound e-Transfer from {}", req.sender_name)).await?;
    let id = insert_inbound(&mut tx, amount, &req, &handle, reg.0, &claim_token,
        answer_hash, None, Some(hold.transaction_id), "available").await?;
    notify(&mut tx, id, &handle, "incoming_transfer",
        &format!("You have an Interac e-Transfer of ${amount} from {}", req.sender_name), Some(&claim_token)).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(load_etransfer(&state, id).await?)))
}

#[allow(clippy::too_many_arguments)]
async fn insert_inbound(
    tx: &mut crate::rails::PgTx<'_>, amount: Decimal, req: &InboundEtransferRequest,
    handle: &str, recipient_customer: Uuid, claim_token: &str,
    answer_hash: Option<String>, recipient_account: Option<Uuid>,
    hold_txn: Option<Uuid>, status: &str,
) -> Result<Uuid, AppError> {
    let id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO interac_etransfers
            (direction, status, amount, sender_name, recipient_handle_type, recipient_handle_value,
             recipient_customer_id, recipient_account_id, counterparty_institution,
             security_question, security_answer_hash, claim_token, memo, hold_transaction_id, expires_at)
        VALUES ('inbound',$1::interac_status,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,
                CURRENT_TIMESTAMP + interval '30 days')
        RETURNING etransfer_id
        "#,
    )
    .bind(status).bind(amount).bind(&req.sender_name).bind(req.recipient_handle_type)
    .bind(handle).bind(recipient_customer).bind(recipient_account).bind(&req.counterparty_institution)
    .bind(&req.security_question).bind(&answer_hash).bind(claim_token).bind(&req.memo).bind(hold_txn)
    .fetch_one(&mut **tx).await?;
    Ok(id)
}
```

- [ ] **Step 2: Type-check + smoke-test**

Run: `cd api && cargo check`
```bash
# autodeposit recipient
curl -sS -X POST localhost:8081/api/v1/interac/network/inbound -H "Authorization: Bearer $STOK" \
  -H 'content-type: application/json' \
  -d '{"amount":75.00,"sender_name":"External Sender","counterparty_institution":"003","recipient_handle_type":"email","recipient_handle_value":"bob@autodep.ca"}' | jq .status
# → "deposited"; bob's balance +75; INTERAC_SETTLEMENT goes negative (network owes us)
```
Expected: unregistered handle → 404; held path → `available` + notification; a customer token on this endpoint → 403.

- [ ] **Step 3: Commit**

```bash
git add api/src/handlers/interac.rs
git commit -m "feat(interac): inbound network endpoint (autodeposit + held/claim)"
```

---

## Task 13: Network settle (`POST /network/etransfers/:id/settle`)

**Files:**
- Modify: `api/src/handlers/interac.rs`

**Interfaces:**
- Consumes: `SettleEtransferRequest`, `AuthenticatedService`, `InteracRail::{release, refund}`.
- Produces: `network_settle` (200) — the far side ACKs an **outbound-to-external** `available` transfer. `outcome:"claimed"` → `release` External (Dr CLEARING / Cr SETTLEMENT), record `counterparty_institution`, status `deposited`; `outcome:"declined"` → `refund` sender, status `declined`. Only valid when the transfer is outbound + external (no `recipient_customer_id`).

- [ ] **Step 1: Implement network_settle**

```rust
use crate::models::interac::SettleEtransferRequest;

async fn network_settle(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
    Path(id): Path<Uuid>,
    AxumJson(req): AxumJson<SettleEtransferRequest>,
) -> Result<(StatusCode, Json<EtransferResponse>), AppError> {
    let rail = resolve_interac(&state).await?;
    let mut tx = state.pool.begin().await?;

    // Must be an outbound, external, still-available transfer.
    let row = sqlx::query_as::<_, (String, String, Decimal, Option<Uuid>, Option<Uuid>, String)>(
        "SELECT status::text, direction::text, amount, sender_account_id, recipient_customer_id, \
         COALESCE((SELECT reference_number FROM transactions WHERE transaction_id=hold_transaction_id),'') \
         FROM interac_etransfers WHERE etransfer_id=$1 FOR UPDATE")
        .bind(id).fetch_optional(&mut *tx).await?
        .ok_or_else(|| AppError::NotFound("e-Transfer not found".into()))?;
    if row.0 != "available" { return Err(AppError::Conflict(format!("e-Transfer is {}", row.0))); }
    if row.1 != "outbound" || row.4.is_some() {
        return Err(AppError::BadRequest("not an external outbound transfer".into()));
    }
    let hold = crate::rails::Hold {
        from_account: row.3.ok_or_else(|| AppError::Internal("missing sender account".into()))?,
        amount: row.2, reference: row.5, transaction_id: Uuid::nil(),
    };

    let (new_status, handle_kind, msg) = match req.outcome.as_str() {
        "claimed" => {
            rail.release(&state, &mut tx, &hold, crate::rails::Destination::External(req.institution.clone()),
                "Interac e-Transfer settled to external bank").await?;
            sqlx::query("UPDATE interac_etransfers SET status='deposited', counterparty_institution=$2, resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
                .bind(id).bind(&req.institution).execute(&mut *tx).await?;
            ("deposited", "deposit_completed", "deposited at the recipient's bank")
        }
        "declined" => {
            rail.refund(&state, &mut tx, &hold, "Interac e-Transfer declined by network").await?;
            sqlx::query("UPDATE interac_etransfers SET status='declined', resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
                .bind(id).execute(&mut *tx).await?;
            ("declined", "declined", "declined and returned")
        }
        other => return Err(AppError::BadRequest(format!("unknown outcome '{other}'"))),
    };

    let handle: String = sqlx::query_scalar("SELECT recipient_handle_value FROM interac_etransfers WHERE etransfer_id=$1")
        .bind(id).fetch_one(&mut *tx).await?;
    notify(&mut tx, id, &handle, handle_kind, &format!("Your e-Transfer was {msg}"), None).await?;
    let _ = new_status;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(load_etransfer(&state, id).await?)))
}
```

- [ ] **Step 2: Type-check + smoke-test**

Run: `cd api && cargo check`
```bash
# send an external transfer (recipient not registered), grab its id, settle as network
curl -sS -X POST localhost:8081/api/v1/interac/network/etransfers/$EID/settle -H "Authorization: Bearer $STOK" \
  -H 'content-type: application/json' -d '{"outcome":"claimed","institution":"004"}' | jq .status
# → "deposited"; INTERAC_CLEARING drops, INTERAC_SETTLEMENT rises (we owe the network)
```

- [ ] **Step 3: Commit**

```bash
git add api/src/handlers/interac.rs
git commit -m "feat(interac): network settle for outbound-external (claimed/declined)"
```

---

## Task 14: Admin sweep-expired (`POST /admin/sweep-expired`)

**Files:**
- Modify: `api/src/handlers/interac.rs`

**Interfaces:**
- Consumes: `AuthenticatedService`, `InteracRail::refund`.
- Produces: `sweep_expired` (200) — finds `available` outbound transfers past `expires_at`, refunds each sender, sets status `expired`, posts an expiry notification; returns `{ "expired": N }`. Idempotent (only touches still-`available` rows). Inbound held transfers past expiry are refunded to SETTLEMENT (returned to network).

- [ ] **Step 1: Implement sweep_expired**

```rust
use serde_json::json as sjson;

async fn sweep_expired(
    State(state): State<AppState>,
    _svc: AuthenticatedService,
) -> Result<Json<serde_json::Value>, AppError> {
    let rail = resolve_interac(&state).await?;
    // Snapshot the due ids first (short read), then process each in its own tx so
    // one bad row can't roll back the batch.
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT etransfer_id FROM interac_etransfers \
         WHERE status='available' AND expires_at < CURRENT_TIMESTAMP")
        .fetch_all(&state.pool).await?;

    let mut expired = 0i64;
    for id in due {
        let mut tx = state.pool.begin().await?;
        // Re-lock + re-check (a concurrent claim may have won).
        let guard = lock_available(&mut tx, id).await;
        let (amount, from_account, _r, _h, _a, hold_ref) = match guard {
            Ok(v) => v,
            Err(_) => { tx.rollback().await?; continue; }
        };
        let hold = crate::rails::Hold { from_account, amount, reference: hold_ref, transaction_id: Uuid::nil() };
        rail.refund(&state, &mut tx, &hold, "Interac e-Transfer expired").await?;
        sqlx::query("UPDATE interac_etransfers SET status='expired', resolved_at=CURRENT_TIMESTAMP WHERE etransfer_id=$1")
            .bind(id).execute(&mut *tx).await?;
        let handle: String = sqlx::query_scalar("SELECT recipient_handle_value FROM interac_etransfers WHERE etransfer_id=$1")
            .bind(id).fetch_one(&mut *tx).await?;
        notify(&mut tx, id, &handle, "expired", &format!("${amount} expired and was returned"), None).await?;
        tx.commit().await?;
        expired += 1;
    }
    Ok(Json(sjson!({ "expired": expired })))
}
```

> Production would drive this from a cron/systemd timer (like the repo's other cron jobs); an optional tokio interval task calling the same logic is a follow-up. For `from_account` on an inbound hold this is the SETTLEMENT account, so `refund` correctly returns those funds to the network.

- [ ] **Step 2: Type-check + smoke-test**

Run: `cd api && cargo check`
```bash
# force-expire a held transfer, then sweep
kubectl exec -i -n nano-bank deployment/postgres -- psql -U nanobank_user -d nano_bank_db -c \
 "UPDATE interac_etransfers SET expires_at=CURRENT_TIMESTAMP - interval '1 day' WHERE status='available';"
curl -sS -X POST localhost:8081/api/v1/interac/admin/sweep-expired -H "Authorization: Bearer $STOK" | jq
# → { "expired": N }; those transfers now 'expired', senders refunded
```

- [ ] **Step 3: Commit**

```bash
git add api/src/handlers/interac.rs
git commit -m "feat(interac): admin sweep-expired (per-row refund + notification)"
```

---

## Task 15: Bruno collection (`bruno/6_Interac/`)

**Files:**
- Create: `bruno/6_Interac/*.bru` (Register Autodeposit, Send, Claim, Decline, Cancel, List, Get, Network Inbound, Network Settle, Sweep Expired)
- Modify: `bruno/environments/local.bru` (add `interac_etransfer_id`, `handle` vars if useful)

**Interfaces:**
- Consumes: the endpoints from Tasks 7–14.

- [ ] **Step 1: Author the `.bru` files using Bruno's required format**

Follow the working `_2` files' structure (the `post {}` block MUST declare `body: json` and `auth: inherit` alongside the URL, or Bruno ignores the body). Example `bruno/6_Interac/Send.bru`:
```
meta {
  name: Send e-Transfer
  type: http
  seq: 2
}

post {
  url: {{base_url}}/api/v1/interac/etransfers
  body: json
  auth: inherit
}

body:json {
  {
    "from_account_id": "{{account_id}}",
    "amount": 40.00,
    "recipient_handle_type": "email",
    "recipient_handle_value": "carol@elsewhere.ca",
    "security_question": "fav colour",
    "security_answer": "Blue"
  }
}
```
Author the remaining requests the same way (service-token requests set `auth: inherit` against a service-token env var).

- [ ] **Step 2: Verify in Bruno** (open the collection, run Send → 201). No automated check; visual.

- [ ] **Step 3: Commit**

```bash
git add bruno/6_Interac bruno/environments/local.bru
git commit -m "test(interac): Bruno collection for the e-Transfer flows"
```

---

## Task 16: Interac network simulator (`testing/interac/`)

**Files:**
- Create: `testing/interac/interac_simulator.py`
- Create: `testing/interac/Dockerfile`
- Create: `testing/interac/requirements.txt` (`requests`)
- Modify: `testing/docker-compose.yml` (or the harness's compose) to add the `interac-simulator` service

**Interfaces:**
- Consumes: the customer + service endpoints; the notification outbox (via DB or an admin read). Plays "the rest of the Interac network".

- [ ] **Step 1: Write `interac_simulator.py`**

The simulator loops:
1. Poll undelivered `interac_notifications` (via a small read — either a DB connection like `cleanup.sh` uses, or a to-be-added `GET /interac/admin/notifications`; use a direct psql/`psycopg` read to match how `viewer` reads data). For each `incoming_transfer` on an **external/outbound** transfer (no local recipient), call `POST /interac/network/etransfers/{id}/settle` with `{"outcome":"claimed","institution":"004"}` (or randomly `declined`), answering is not needed (the far bank owns the claim). Mark the notification delivered.
2. Periodically **originate inbound** transfers: pick a seeded nano-bank handle and `POST /interac/network/inbound` with a random amount, half to autodeposit handles, half requiring a claim.

Concrete skeleton:
```python
import os, time, random, requests, psycopg2

API = os.environ.get("API_URL", "http://nano-bank-api:8081")
STOK = os.environ["SERVICE_TOKEN"]
DB = os.environ.get("DATABASE_URL", "postgresql://nanobank_user:...@postgres:5432/nano_bank_db")
H = {"Authorization": f"Bearer {STOK}", "content-type": "application/json"}

def undelivered_external():
    conn = psycopg2.connect(DB); cur = conn.cursor()
    cur.execute("""
        SELECT n.notification_id, e.etransfer_id
        FROM interac_notifications n JOIN interac_etransfers e ON e.etransfer_id=n.etransfer_id
        WHERE n.delivered=FALSE AND n.kind='incoming_transfer'
          AND e.direction='outbound' AND e.recipient_customer_id IS NULL
    """)
    rows = cur.fetchall(); conn.close(); return rows

def settle(etransfer_id):
    outcome = "claimed" if random.random() > 0.15 else "declined"
    requests.post(f"{API}/api/v1/interac/network/etransfers/{etransfer_id}/settle",
                  json={"outcome": outcome, "institution": "004"}, headers=H)

def mark_delivered(nid):
    conn = psycopg2.connect(DB); cur = conn.cursor()
    cur.execute("UPDATE interac_notifications SET delivered=TRUE WHERE notification_id=%s", (nid,))
    conn.commit(); conn.close()

while True:
    for nid, eid in undelivered_external():
        settle(eid); mark_delivered(nid)
    # occasionally originate an inbound transfer to a seeded handle
    if random.random() < 0.3:
        requests.post(f"{API}/api/v1/interac/network/inbound",
            json={"amount": round(random.uniform(5, 200), 2), "sender_name": "Sim Sender",
                  "counterparty_institution": "003", "recipient_handle_type": "email",
                  "recipient_handle_value": random.choice(["bob@autodep.ca", "carol@elsewhere.ca"])},
            headers=H)
    time.sleep(5)
```

- [ ] **Step 2: Dockerfile + compose service** (mirror `testing/visa/`). Add env `API_URL`, `SERVICE_TOKEN`, `DATABASE_URL`.

- [ ] **Step 3: Run & verify**

```bash
cd testing && docker compose up -d interac-simulator && docker compose logs -f interac-simulator
```
Expected: external outbound transfers flip to `deposited`/`declined`; inbound transfers appear; notification rows get `delivered=TRUE`.

- [ ] **Step 4: Commit**

```bash
git add testing/interac testing/docker-compose.yml
git commit -m "test(interac): network simulator (settles outbound-external, originates inbound)"
```

---

## Task 17: Viewer Interac tab

**Files:**
- Modify: `testing/viewer/app.py`

**Interfaces:**
- Consumes: `interac_etransfers`, `interac_notifications`, the Interac system accounts.

- [ ] **Step 1: Add an "Interac" tab** showing: in-flight transfers (status counts + a table of recent `interac_etransfers`), the `INTERAC_CLEARING` / `INTERAC_SETTLEMENT` balances (join `accounts`→`customers` on `interac@nano.bank`), and the notification outbox timeline (recent `interac_notifications`, newest first). Follow the existing tab/query pattern in `app.py`.

- [ ] **Step 2: Verify** — `streamlit run` (or the compose viewer on :8504) shows the tab; drive traffic via the simulator and watch balances/notifications update.

- [ ] **Step 3: Commit**

```bash
git add testing/viewer/app.py
git commit -m "test(interac): viewer tab for e-Transfers, clearing/settlement, notifications"
```

---

## Task 18: Docs — record the Interac rail

**Files:**
- Modify: `CLAUDE.md` (add an "Interac e-Transfer rail" subsection near the cards section)
- Modify: `api/CLAUDE.md` (note `rails/` module + the Interac endpoints)
- Modify: `README.md` (endpoint table row for `/api/v1/interac/*`)

**Interfaces:** none (documentation).

- [ ] **Step 1: Document** the `Rail` port, the `interac@nano.bank` system customer + `INTERAC_CLEARING`/`INTERAC_SETTLEMENT`, the three auth planes, the e-Transfer lifecycle, and the simulator. Cross-reference `docs/specs/2026-07-04-interac-rail-foundation-design.md` and the `.claude/skills/nano-bank-rails` skill.

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md api/CLAUDE.md README.md
git commit -m "docs(interac): document the Interac e-Transfer rail"
```

---

## Self-Review

**Spec coverage:**
- Rail foundation (routing cols, `rail_participants`, per-rail clearing/settlement) → Tasks 1, 3. ✓
- `Rail` port beside `Ledger`, four verbs, dual-post → Tasks 3, 4. ✓
- Two-account settlement (`INTERAC_CLEARING`/`INTERAC_SETTLEMENT`) → Tasks 3, 4 (+ the money-flow table realized in each verb). ✓
- Handle model + autodeposit → Tasks 2, 7. ✓
- Full lifecycle: send/autodeposit/claim(security Q&A, 3-strike)/decline/cancel/expire → Tasks 8, 9, 10, 14. ✓
- Inbound + external settle → Tasks 12, 13. ✓
- Outbox notifications → Task 8+ (`notify`), surfaced in Tasks 16, 17. ✓
- Three auth planes → Tasks 6–14 (extractors). ✓
- Idempotency, funds check, per-transfer cap, concurrency guard → Tasks 2 (unique), 8 (cap/funds/idempotency), 9–14 (`FOR UPDATE` guard). ✓
- PR #15 coexistence (no `transactions.rs` edits; reuse `pub(crate)` helpers) → honored throughout; Global Constraints. ✓
- Simulator + viewer + Bruno → Tasks 15, 16, 17. ✓
- Deferred (called out, not built): shared `account_limits` integration, ACSS settlement sweep, Request Money — per spec §11. ✓

**Type consistency:** `Hold { from_account, amount, reference, transaction_id }`, `Destination::{Internal(Uuid), External(String)}`, `RailPosting { transaction_id, gl_entry }`, `InteracAccounts { clearing_id, settlement_id }`, `HandleType::{Email,Phone}` used identically across Tasks 3–14. The `Rail` verbs' signatures (`&AppState, &mut PgTx, …, description: &str`) match every call site.

**Placeholder scan:** the only intentional temporaries are the Task 6 handler stubs (each explicitly replaced in its own task) and the Task 3→5 `HandleType` shim (explicitly deleted in Task 5). No `TODO`/`TBD` in delivered code.

**Known ergonomic note to fix during execution:** the Task 8 `send_etransfer` tail is written twice (a first draft with an awkward `.map`, then the corrected version in the Step 2 note) — implement the corrected version (commit `tx.commit()` before the pool-read `load_etransfer`, keep `status` only for the tracing line).
