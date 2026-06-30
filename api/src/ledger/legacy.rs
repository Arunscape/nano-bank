//! Adapter for the legacy core (`nano-bank-legacy-core`): maps the neutral port
//! onto its document-posting REST API (`/api/v1/documents`, `/api/v1/gl-balances`),
//! translating semantic accounts to its `0000xxxxxx` numbers, `Direction` to the
//! S/H indicator, and tagging everything with company code `1000`.

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use super::{AccountBalance, Ledger, LedgerError, NewEntry, PostedEntry};

const COMPANY_CODE: &str = "1000";
const FISCAL_YEAR: &str = "2026";

pub struct LegacyLedger {
    base_url: String,
    http: reqwest::Client,
}

impl LegacyLedger {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Ledger for LegacyLedger {
    fn backend(&self) -> &'static str {
        "legacy"
    }

    async fn post_entry(&self, entry: NewEntry) -> Result<PostedEntry, LedgerError> {
        let items: Vec<_> = entry
            .lines
            .iter()
            .map(|l| {
                json!({
                    "hkont": l.account.legacy_account(),
                    "shkzg": l.direction.legacy(),
                    "dmbtr": l.amount,
                })
            })
            .collect();
        // Respect the legacy field limits: bktxt is VARCHAR(25), xblnr VARCHAR(16).
        let body = json!({
            "bukrs": COMPANY_CODE,
            "blart": "SA",
            "budat": Utc::now().date_naive().to_string(),
            "waers": "CAD",
            "bktxt": truncate(entry.description.as_deref(), 25),
            "xblnr": truncate(entry.reference.as_deref(), 16),
            "items": items,
        });

        let resp = self
            .http
            .post(format!("{}/api/v1/documents", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(transport)?;
        let resp = check(resp).await?;
        let posted: LegacyPostResponse = resp.json().await.map_err(transport)?;
        Ok(PostedEntry {
            id: posted.belnr,
            backend: "legacy".into(),
        })
    }

    async fn balances(&self) -> Result<Vec<AccountBalance>, LedgerError> {
        let resp = self
            .http
            .get(format!("{}/api/v1/gl-balances", self.base_url))
            .query(&[("bukrs", COMPANY_CODE), ("gjahr", FISCAL_YEAR)])
            .send()
            .await
            .map_err(transport)?;
        let resp = check(resp).await?;
        let rows: Vec<LegacyBalance> = resp.json().await.map_err(transport)?;
        Ok(rows
            .into_iter()
            .map(|r| AccountBalance {
                account: r.hkont,
                balance: r.balance,
            })
            .collect())
    }
}

#[derive(Deserialize)]
struct LegacyPostResponse {
    belnr: String,
}

#[derive(Deserialize)]
struct LegacyBalance {
    hkont: String,
    balance: rust_decimal::Decimal,
}

/// Truncate to the legacy column width (character count, matching VARCHAR(n)).
fn truncate(s: Option<&str>, max: usize) -> Option<String> {
    s.map(|x| x.chars().take(max).collect())
}

fn transport(e: reqwest::Error) -> LedgerError {
    LedgerError::Transport(e.to_string())
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response, LedgerError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Err(LedgerError::Backend { status, body })
}
