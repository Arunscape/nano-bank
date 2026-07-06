//! The built-in consent UI (`GET /app`).
//!
//! A single self-contained page (no external assets, no build step) compiled
//! into the binary — the `/docs` pattern. It drives the ordinary customer API
//! same-origin: signup/login, accounts, agent registration, mandate
//! grant/revoke, and the per-mandate activity (audit) view.

use axum::response::Html;

const APP_HTML: &str = include_str!("../../assets/app.html");

pub async fn consent_app() -> Html<&'static str> {
    Html(APP_HTML)
}
