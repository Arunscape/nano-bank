//! ISO 20022 message codec for the Lynx wire rail. Authentic in shape
//! (namespaced `<Document>` envelopes with the real element names), and
//! round-trippable — NOT schema-validated against the official XSDs. Same
//! philosophy as the AFT rail's `cpa005.rs`.
//!
//! Covered messages:
//! - **pacs.008** FIToFICustomerCreditTransfer (outbound customer wire)
//! - **pacs.009** FinancialInstitutionCreditTransfer (FI-to-FI; inbound may be either)
//! - **camt.056** FIToFIPaymentCancellationRequest (recall request)
//! - **camt.029** ResolutionOfInvestigation (recall accept/reject)

use std::str::FromStr;

use rust_decimal::Decimal;

#[derive(Debug, thiserror::Error)]
pub enum Iso20022Error {
    #[error("malformed ISO 20022 message: {0}")]
    Malformed(String),
}

/// A customer or FI credit transfer (pacs.008 / pacs.009 share this body here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditTransfer {
    pub uetr: String,
    pub debtor_name: String,
    pub debtor_agent: String,
    pub debtor_account: String,
    pub creditor_name: String,
    pub creditor_agent: String,
    pub creditor_account: String,
    pub amount: Decimal,
    pub currency: String,
    pub remittance: Option<String>,
}

/// pacs.008 and pacs.009 differ only in namespace + root element name here.
pub type Pacs008 = CreditTransfer;
pub type Pacs009 = CreditTransfer;

/// camt.056 — a request to cancel/recall a prior payment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Camt056 {
    pub uetr: String,
    pub original_uetr: String,
    pub reason: String,
}

/// camt.029 — the resolution of a cancellation request. `status` is `ACCP`
/// (accepted) or `RJCR` (rejected).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Camt029 {
    pub uetr: String,
    pub original_uetr: String,
    pub status: String,
    pub reason: Option<String>,
}

// --- tiny XML helpers (find inner text between <tag> … </tag>) ---

fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

fn required<'a>(xml: &'a str, name: &str) -> Result<&'a str, Iso20022Error> {
    tag(xml, name).ok_or_else(|| Iso20022Error::Malformed(format!("missing <{name}>")))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn parse_amount(s: &str) -> Result<Decimal, Iso20022Error> {
    Decimal::from_str(s.trim())
        .map_err(|e| Iso20022Error::Malformed(format!("bad amount '{s}': {e}")))
}

// --- credit transfer (pacs.008 / pacs.009) ---

fn encode_credit_transfer(m: &CreditTransfer, ns: &str, root: &str) -> String {
    let rmt = m
        .remittance
        .as_deref()
        .map(|r| format!("<RmtInf><Ustrd>{}</Ustrd></RmtInf>", xml_escape(r)))
        .unwrap_or_default();
    format!(
        concat!(
            "<Document xmlns=\"{ns}\"><{root}><CdtTrfTxInf>",
            "<PmtId><UETR>{uetr}</UETR></PmtId>",
            "<IntrBkSttlmAmt Ccy=\"{ccy}\">{amt:.2}</IntrBkSttlmAmt>",
            "<Dbtr><Nm>{dbtr}</Nm></Dbtr>",
            "<DbtrAcct><Id><Othr><Id>{dbtr_acct}</Id></Othr></Id></DbtrAcct>",
            "<DbtrAgt><FinInstnId><ClrSysMmbId><MmbId>{dbtr_agt}</MmbId></ClrSysMmbId></FinInstnId></DbtrAgt>",
            "<Cdtr><Nm>{cdtr}</Nm></Cdtr>",
            "<CdtrAcct><Id><Othr><Id>{cdtr_acct}</Id></Othr></Id></CdtrAcct>",
            "<CdtrAgt><FinInstnId><ClrSysMmbId><MmbId>{cdtr_agt}</MmbId></ClrSysMmbId></FinInstnId></CdtrAgt>",
            "{rmt}",
            "</CdtTrfTxInf></{root}></Document>",
        ),
        ns = ns,
        root = root,
        uetr = m.uetr,
        ccy = xml_escape(&m.currency),
        amt = m.amount,
        dbtr = xml_escape(&m.debtor_name),
        dbtr_acct = xml_escape(&m.debtor_account),
        dbtr_agt = xml_escape(&m.debtor_agent),
        cdtr = xml_escape(&m.creditor_name),
        cdtr_acct = xml_escape(&m.creditor_account),
        cdtr_agt = xml_escape(&m.creditor_agent),
        rmt = rmt,
    )
}

fn decode_credit_transfer(xml: &str) -> Result<CreditTransfer, Iso20022Error> {
    // amount lives in <IntrBkSttlmAmt Ccy="CAD">25000.00</IntrBkSttlmAmt>
    let amt_open = xml
        .find("<IntrBkSttlmAmt")
        .ok_or_else(|| Iso20022Error::Malformed("missing <IntrBkSttlmAmt>".into()))?;
    let amt_gt = xml[amt_open..]
        .find('>')
        .map(|i| amt_open + i + 1)
        .ok_or_else(|| Iso20022Error::Malformed("unterminated <IntrBkSttlmAmt>".into()))?;
    let amt_end = xml[amt_gt..]
        .find("</IntrBkSttlmAmt>")
        .map(|i| amt_gt + i)
        .ok_or_else(|| Iso20022Error::Malformed("unterminated IntrBkSttlmAmt".into()))?;
    let amount = parse_amount(&xml[amt_gt..amt_end])?;
    let ccy = {
        let seg = &xml[amt_open..amt_gt];
        let key = "Ccy=\"";
        let s = seg
            .find(key)
            .map(|i| i + key.len())
            .ok_or_else(|| Iso20022Error::Malformed("missing Ccy".into()))?;
        let e = seg[s..]
            .find('"')
            .map(|i| s + i)
            .ok_or_else(|| Iso20022Error::Malformed("unterminated Ccy".into()))?;
        seg[s..e].to_string()
    };

    // Debtor/creditor blocks so <Nm> and <Id> don't collide across parties.
    let dbtr_block = required(xml, "Dbtr")?;
    let cdtr_block = required(xml, "Cdtr")?;
    let dbtr_acct_block = required(xml, "DbtrAcct")?;
    let cdtr_acct_block = required(xml, "CdtrAcct")?;

    Ok(CreditTransfer {
        uetr: required(xml, "UETR")?.to_string(),
        debtor_name: xml_unescape(required(dbtr_block, "Nm")?),
        debtor_agent: required(required(xml, "DbtrAgt")?, "MmbId")?.to_string(),
        debtor_account: xml_unescape(required(dbtr_acct_block, "Id")?.trim_start_matches("<Othr><Id>").split("</Id>").next().unwrap_or("")),
        creditor_name: xml_unescape(required(cdtr_block, "Nm")?),
        creditor_agent: required(required(xml, "CdtrAgt")?, "MmbId")?.to_string(),
        creditor_account: xml_unescape(required(cdtr_acct_block, "Id")?.trim_start_matches("<Othr><Id>").split("</Id>").next().unwrap_or("")),
        amount,
        currency: ccy,
        remittance: tag(xml, "Ustrd").map(xml_unescape),
    })
}

pub fn encode_pacs008(m: &Pacs008) -> String {
    encode_credit_transfer(
        m,
        "urn:iso:std:iso:20022:tech:xsd:pacs.008.001.08",
        "FIToFICstmrCdtTrf",
    )
}
pub fn decode_pacs008(xml: &str) -> Result<Pacs008, Iso20022Error> {
    decode_credit_transfer(xml)
}
pub fn encode_pacs009(m: &Pacs009) -> String {
    encode_credit_transfer(
        m,
        "urn:iso:std:iso:20022:tech:xsd:pacs.009.001.08",
        "FICdtTrf",
    )
}
pub fn decode_pacs009(xml: &str) -> Result<Pacs009, Iso20022Error> {
    decode_credit_transfer(xml)
}

// --- camt.056 (recall request) ---

pub fn encode_camt056(m: &Camt056) -> String {
    format!(
        concat!(
            "<Document xmlns=\"urn:iso:std:iso:20022:tech:xsd:camt.056.001.08\"><FIToFIPmtCxlReq>",
            "<Case><Id>{uetr}</Id></Case>",
            "<Undrlyg><TxInf><OrgnlUETR>{orig}</OrgnlUETR>",
            "<CxlRsnInf><Rsn><Cd>{reason}</Cd></Rsn></CxlRsnInf>",
            "</TxInf></Undrlyg>",
            "</FIToFIPmtCxlReq></Document>",
        ),
        uetr = m.uetr,
        orig = m.original_uetr,
        reason = xml_escape(&m.reason),
    )
}

pub fn decode_camt056(xml: &str) -> Result<Camt056, Iso20022Error> {
    Ok(Camt056 {
        uetr: required(required(xml, "Case")?, "Id")?.to_string(),
        original_uetr: required(xml, "OrgnlUETR")?.to_string(),
        reason: xml_unescape(required(xml, "Cd")?),
    })
}

// --- camt.029 (recall resolution) ---

pub fn encode_camt029(m: &Camt029) -> String {
    let reason = m
        .reason
        .as_deref()
        .map(|r| format!("<RsnInf><Rsn><Cd>{}</Cd></Rsn></RsnInf>", xml_escape(r)))
        .unwrap_or_default();
    format!(
        concat!(
            "<Document xmlns=\"urn:iso:std:iso:20022:tech:xsd:camt.029.001.09\"><RsltnOfInvstgtn>",
            "<Case><Id>{uetr}</Id></Case>",
            "<Sts><Conf>{status}</Conf></Sts>",
            "<CxlDtls><TxInfAndSts><OrgnlUETR>{orig}</OrgnlUETR>{reason}</TxInfAndSts></CxlDtls>",
            "</RsltnOfInvstgtn></Document>",
        ),
        uetr = m.uetr,
        status = xml_escape(&m.status),
        orig = m.original_uetr,
        reason = reason,
    )
}

pub fn decode_camt029(xml: &str) -> Result<Camt029, Iso20022Error> {
    Ok(Camt029 {
        uetr: required(required(xml, "Case")?, "Id")?.to_string(),
        original_uetr: required(xml, "OrgnlUETR")?.to_string(),
        status: xml_unescape(required(required(xml, "Sts")?, "Conf")?),
        reason: tag(xml, "Cd").map(xml_unescape),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn sample() -> CreditTransfer {
        CreditTransfer {
            uetr: "11111111-1111-1111-1111-111111111111".into(),
            debtor_name: "Alice Payer".into(),
            debtor_agent: "900".into(),
            debtor_account: "000000000123".into(),
            creditor_name: "Bob Payee".into(),
            creditor_agent: "001".into(),
            creditor_account: "000000000456".into(),
            amount: Decimal::new(2500000, 2),
            currency: "CAD".into(),
            remittance: Some("invoice 42".into()),
        }
    }

    #[test]
    fn pacs008_round_trips() {
        let m = sample();
        assert_eq!(decode_pacs008(&encode_pacs008(&m)).unwrap(), m);
    }

    #[test]
    fn pacs009_round_trips_without_remittance() {
        let mut m = sample();
        m.remittance = None;
        assert_eq!(decode_pacs009(&encode_pacs009(&m)).unwrap(), m);
    }

    #[test]
    fn camt056_and_029_round_trip() {
        let r = Camt056 {
            uetr: "22222222-2222-2222-2222-222222222222".into(),
            original_uetr: "11111111-1111-1111-1111-111111111111".into(),
            reason: "DUPL".into(),
        };
        assert_eq!(decode_camt056(&encode_camt056(&r)).unwrap(), r);

        let a = Camt029 {
            uetr: "33333333-3333-3333-3333-333333333333".into(),
            original_uetr: "11111111-1111-1111-1111-111111111111".into(),
            status: "ACCP".into(),
            reason: None,
        };
        assert_eq!(decode_camt029(&encode_camt029(&a)).unwrap(), a);

        let rej = Camt029 {
            uetr: "44444444-4444-4444-4444-444444444444".into(),
            original_uetr: "11111111-1111-1111-1111-111111111111".into(),
            status: "RJCR".into(),
            reason: Some("LEGAL".into()),
        };
        assert_eq!(decode_camt029(&encode_camt029(&rej)).unwrap(), rej);
    }
}
