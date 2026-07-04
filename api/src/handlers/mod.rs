pub mod auth;
pub mod accounts;
pub mod cards;
pub mod customers;
pub mod docs;
pub mod health;
pub mod interac;
pub mod ledger;
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