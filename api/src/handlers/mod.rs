pub mod accounts;
pub mod aft;
pub mod agent_api;
pub mod agents;
pub mod app;
pub mod approvals;
pub mod auth;
pub mod cards;
pub mod customers;
pub mod docs;
pub mod health;
pub mod interac;
pub mod interac_payees;
pub mod lynx;
pub mod ledger;
pub mod mandates;
pub mod security;
pub mod transactions;

use std::sync::Arc;

use crate::config::{database::DatabasePool, Settings};
use crate::ledger::Ledger;

// Shared application state
#[derive(Clone)]
pub struct AppState {
    pub pool: DatabasePool,
    pub settings: Settings,
    /// The accounting core (modern or legacy) behind the swappable Ledger port.
    pub ledger: Arc<dyn Ledger>,
}
