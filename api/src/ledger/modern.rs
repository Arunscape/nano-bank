//! Adapter for the modern core (`nano-bank-modern-core`): a thin HTTP client that
//! speaks its clean REST ledger API.

use async_trait::async_trait;
use serde_json::json;

use super::{AccountBalance, Ledger, LedgerError, NewEntry, PostedEntry};

pub struct ModernLedger {
    base_url: String,
    http: reqwest::Client,
}

impl ModernLedger {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Ledger for ModernLedger {
    fn backend(&self) -> &'static str {
        "modern"
    }

    async fn post_entry(&self, entry: NewEntry) -> Result<PostedEntry, LedgerError> {
        let lines: Vec<_> = entry
            .lines
            .iter()
            .map(|l| {
                json!({
                    "account": l.account.modern_code(),
                    "direction": l.direction.modern(),
                    "amount": l.amount,
                })
            })
            .collect();
        let body = json!({
            "reference": entry.reference,
            "description": entry.description,
            "lines": lines,
        });

        let resp = self
            .http
            .post(format!("{}/entries", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(transport)?;
        let resp = check(resp).await?;
        let value: serde_json::Value = resp.json().await.map_err(transport)?;
        let id = value.get("id").map(|v| v.to_string()).unwrap_or_default();
        Ok(PostedEntry {
            id,
            backend: "modern".into(),
        })
    }

    async fn balances(&self) -> Result<Vec<AccountBalance>, LedgerError> {
        let resp = self
            .http
            .get(format!("{}/balances", self.base_url))
            .send()
            .await
            .map_err(transport)?;
        let resp = check(resp).await?;
        resp.json::<Vec<AccountBalance>>().await.map_err(transport)
    }
}

fn transport(e: reqwest::Error) -> LedgerError {
    LedgerError::Transport(e.to_string())
}

/// Turn a non-2xx response into a `Backend` error, otherwise pass it through.
async fn check(resp: reqwest::Response) -> Result<reqwest::Response, LedgerError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Err(LedgerError::Backend { status, body })
}
