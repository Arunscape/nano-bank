//! The **Rail port**: nano-bank's interface to an external payment rail
//! (Interac, AFT, Lynx). A rail sits BESIDE the Ledger port — it owns the local
//! double-entry (customer account ↔ its clearing/settlement system accounts) AND
//! posts the aggregate GL effect through `Ledger`, in one DB transaction.
//!
//! The trait's verbs are the clearing/settlement plumbing common to every rail;
//! product lifecycle (Interac's claim/decline/expiry) lives in the handler.

pub mod aft;
pub(crate) mod common;
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
