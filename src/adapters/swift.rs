//! Banco Popular "Confirmación Mensaje Swift Operado" parser.
//!
//! An outbound international-wire confirmation that carries a SWIFT pacs.008
//! (`FIToFICustomerCreditTransfer`) message. Money left the user's BPD account
//! and arrived in another of the user's own accounts abroad, so it books as a
//! Firefly **transfer** (BPD debtor account → foreign creditor account), not a
//! withdrawal.
//!
//! This module is pure (no I/O) and is reached from the Banco Popular adapter's
//! `deterministic_extract` seam: the SWIFT confirmation arrives from the *same*
//! sender (`notificaciones@popularenlinea.com`) as a normal consumo, so it
//! cannot have its own sender-selected adapter — [`try_parse_swift`] decides,
//! from the *body*, whether this is a SWIFT confirmation (→ `Some`) or a consumo
//! that should fall through to the LLM path (→ `None`).
//!
//! Robustness note: the real email interleaves the SWIFT XML with page-break
//! lines (`29/05/26-05:12:56  ICMACKINTERTMX-0277-000001  2`) and underscore
//! rules. Every field is therefore extracted **by tag** (`<Tag>…</Tag>`),
//! scanning the whole body, never by line position — so an interruption between
//! the open and close tags, or between two fields, does not break extraction.

use anyhow::{Result, anyhow};
use chrono::NaiveDate;

use super::parse::strip_thousands_commas;
use super::{DestHint, Outcome, SourceHint, TransferRecord};
use crate::schema::{Amount, Currency, Money};

/// Structural markers that identify a SWIFT pacs.008 confirmation body, matched
/// case-insensitively. Unlike a loose prose phrase ("mensaje swift"), these are
/// the actual XML message-type tokens, so a consumo that merely mentions the
/// words "mensaje swift" is NOT misclassified, and a wire is recognised by its
/// document structure rather than by a translatable subject line.
///
/// A body is a SWIFT confirmation only when it carries a pacs.008 message-type
/// marker (`pacs.008.001` or the `FIToFICstmrCdtTrf`/`FIToFICustomerCreditTransfer`
/// element name) AND the interbank-settlement-amount element that every pacs.008
/// credit transfer carries — see [`is_swift_confirmation`].
const SWIFT_DOC_MARKERS: &[&str] = &[
    "pacs.008.001",
    "fitoficstmrcdttrf",
    "fitoficustomercredittransfer",
];

/// The interbank-settlement-amount element, required (in addition to a document
/// marker) to classify a body as a SWIFT confirmation. Its presence is the
/// structural signal that this is a real pacs.008 credit transfer rather than
/// prose that happens to name the message type.
const SWIFT_SETTLEMENT_MARKER: &str = "<intrbksttlmamt";

/// Try to parse `body` as a SWIFT pacs.008 wire confirmation.
///
/// Returns:
/// - `Some(Ok(TransferRecord))` — the body is a SWIFT confirmation and every
///   required field parsed;
/// - `Some(Err(..))` — the body *looks* like a SWIFT confirmation (carries a
///   [`SWIFT_MARKERS`] marker) but a required field is missing/malformed, so it
///   must go to Review, never a silent skip;
/// - `None` — the body is not a SWIFT confirmation (a normal consumo), so the
///   caller falls through to its usual (LLM) extraction path.
///
/// Pure: no I/O.
#[must_use]
pub fn try_parse_swift(body: &str) -> Option<Result<TransferRecord>> {
    if !is_swift_confirmation(body) {
        return None;
    }
    // Strip the page-break / underscore-rule interruptions BEFORE tag extraction
    // so a value split across an interruption (e.g. `<IntrBkSttlmAmt Ccy="USD">`
    // then a page-break line then the amount) rejoins into one tag span.
    let cleaned = strip_page_breaks(body);
    Some(parse_swift(&cleaned))
}

/// Whether `body` is a SWIFT pacs.008 confirmation, by PRECISE structural
/// markers (case-insensitive). Requires BOTH a document-type marker
/// ([`SWIFT_DOC_MARKERS`]) AND the interbank-settlement-amount element
/// ([`SWIFT_SETTLEMENT_MARKER`]). This deliberately rejects a consumo body that
/// merely contains a prose phrase like "mensaje swift" (no pacs.008 structure),
/// and recognises a real wire by its XML rather than a translatable subject.
fn is_swift_confirmation(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    let has_doc_marker = SWIFT_DOC_MARKERS.iter().any(|m| lower.contains(m));
    let has_settlement = lower.contains(SWIFT_SETTLEMENT_MARKER);
    has_doc_marker && has_settlement
}

/// Strip the page-break and underscore-rule lines that interrupt the SWIFT XML
/// in the real email, so a field straddling an interruption rejoins.
///
/// Two interruption shapes are removed (the rest of each line — including the
/// newline — is dropped so adjacent tag fragments become contiguous):
/// - page-break lines: a `DD/MM/YY-HH:MM:SS` timestamp, an `ICMACK…` reference,
///   and a trailing page number (e.g.
///   `29/05/26-05:12:56        ICMACKINTERTMX-0277-000001            2`);
/// - underscore-rule lines: a run of `_` (optionally with surrounding
///   whitespace) and nothing else.
///
/// Anything that is not one of those shapes is preserved verbatim. Pure.
fn strip_page_breaks(body: &str) -> String {
    body.lines()
        .filter(|line| !is_page_break_line(line) && !is_underscore_rule(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a line is a page-break interruption: a `DD/MM/YY-HH:MM:SS` timestamp
/// followed by an `ICMACK` reference and a trailing page number. Matched
/// structurally (no regex dependency) so it cannot accidentally strip a real
/// XML line. Case-insensitive on the `ICMACK` token.
fn is_page_break_line(line: &str) -> bool {
    let mut fields = line.split_whitespace();
    let (Some(stamp), Some(reference), Some(page)) =
        (fields.next(), fields.next(), fields.next())
    else {
        return false;
    };
    // Exactly three whitespace-separated fields.
    if fields.next().is_some() {
        return false;
    }
    looks_like_timestamp(stamp)
        && reference.to_ascii_uppercase().contains("ICMACK")
        && page.chars().all(|c| c.is_ascii_digit())
}

/// Whether `s` matches the page-break stamp shape `DD/MM/YY-HH:MM:SS`: two
/// `/`-separated date parts, a `-`, then three `:`-separated time parts, all
/// digits. Lenient on field widths (the real feed is fixed-width, but we do not
/// rely on that).
fn looks_like_timestamp(s: &str) -> bool {
    let Some((date, time)) = s.split_once('-') else {
        return false;
    };
    let date_ok = {
        let mut parts = date.split('/');
        let (Some(d), Some(m), Some(y), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        [d, m, y]
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    };
    let time_ok = {
        let mut parts = time.split(':');
        let (Some(h), Some(mi), Some(se), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        [h, mi, se]
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    };
    date_ok && time_ok
}

/// Whether a line is an underscore rule: a non-empty run of `_` once trimmed.
fn is_underscore_rule(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().all(|c| c == '_')
}

/// The fixed description booked for a SWIFT wire transfer. Either
/// `SWIFT wire to <Cdtr Nm>` when a creditor name is present, or the BIC-only
/// `International transfer (<BIC>)` fallback. Deterministic.
fn swift_description(creditor_name: Option<&str>, creditor_bic: &str) -> String {
    match creditor_name {
        Some(name) if !name.trim().is_empty() => format!("SWIFT wire to {}", name.trim()),
        _ => format!("International transfer ({creditor_bic})"),
    }
}

/// Parse a SWIFT confirmation body into a [`TransferRecord`]. The caller has
/// already confirmed a SWIFT marker is present, so an `Err` here means a
/// required field is absent or malformed (→ Review).
fn parse_swift(body: &str) -> Result<TransferRecord> {
    // A pacs.008 may legally carry more than one `<CdtTrfTxInf>` transaction. Our
    // first-occurrence tag scan would silently book only the FIRST and drop the
    // rest, so a multi-transaction wire MUST go to Review rather than lose money.
    guard_single_transaction(body)?;

    let (amount, currency) = parse_settlement_amount(body)?;
    // A true FX wire instructs one currency (`<InstdAmt Ccy="DOP">`) but settles
    // another (`<IntrBkSttlmAmt Ccy="USD">`). Booking the settlement figure as a
    // same-currency transfer misrepresents the debit, so a cross-currency wire
    // goes to Review.
    guard_single_currency(body, currency.as_str())?;

    let date = parse_settlement_date(body)?;
    let debtor_last4 = parse_debtor_last4(body)?;
    let creditor_bic = parse_creditor_bic(body)?;
    let uetr = parse_uetr(body)?;
    let creditor_name = creditor_name(body);

    Ok(TransferRecord {
        money: Money::new(amount, currency),
        date,
        description: swift_description(creditor_name.as_deref(), &creditor_bic),
        // The UETR is globally unique per payment, so the duplicate confirmation
        // email (the same wire is notified twice) yields the same external_id →
        // Firefly dedups the second as a duplicate.
        external_id: format!("swift:{uetr}"),
        // The debtor IBAN resolves against the DEDICATED SWIFT debtor map, never
        // the PayPal funding map — so a colliding last-4 cannot mis-route.
        source: SourceHint::SwiftDebtorLast4(debtor_last4),
        dest: DestHint::CreditorBic(creditor_bic),
    })
}

/// Reject a multi-transaction pacs.008 (→ Review). A credit transfer message can
/// carry several `<CdtTrfTxInf>` blocks under one `<GrpHdr>`; we book only one
/// and have no per-transaction routing, so booking a subset would silently lose
/// money. Detect it by either a `<NbOfTxs>` greater than 1 OR more than one
/// `<CdtTrfTxInf>` block — whichever the body exposes.
fn guard_single_transaction(body: &str) -> Result<()> {
    let declared = tag_value(body, "NbOfTxs")
        .and_then(|raw| raw.trim().parse::<u64>().ok());
    let blocks = count_occurrences(body, "<CdtTrfTxInf");
    let txns = match (declared, blocks) {
        // Trust the larger of the declared count and the observed block count:
        // either alone being >1 means more than one transaction is present.
        (Some(n), b) => n.max(b as u64),
        (None, b) => b as u64,
    };
    if txns > 1 {
        return Err(anyhow!(
            "multi-transaction SWIFT wire not supported: {txns} txns"
        ));
    }
    Ok(())
}

/// Reject a cross-currency (FX) wire (→ Review). When the body carries an
/// `<InstdAmt Ccy="…">` whose currency differs from the settled
/// `<IntrBkSttlmAmt Ccy="…">` currency (`settled_ccy`), the wire converted
/// currencies and booking the settlement amount as a same-currency transfer
/// would misrepresent the debit. A missing or matching `InstdAmt` is the normal
/// case and proceeds.
fn guard_single_currency(body: &str, settled_ccy: &str) -> Result<()> {
    let Some((attrs, _)) = element(body, "InstdAmt") else {
        return Ok(()); // no instructed amount → nothing to disagree with
    };
    let Some(instd_ccy) = attr_value(&attrs, "Ccy") else {
        return Ok(()); // malformed/absent Ccy → not a currency-mismatch signal
    };
    if !instd_ccy.trim().eq_ignore_ascii_case(settled_ccy) {
        return Err(anyhow!(
            "cross-currency SWIFT wire not supported: instructed CCY {} vs settled CCY {}",
            instd_ccy.trim(),
            settled_ccy
        ));
    }
    Ok(())
}

/// Count non-overlapping occurrences of `needle` in `haystack`.
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// Extract the interbank settlement amount + currency from the
/// `<IntrBkSttlmAmt Ccy="USD">2100.00</IntrBkSttlmAmt>` element. The `Ccy`
/// attribute is the ISO currency; the element text is the decimal amount
/// (thousands commas stripped before the sanitizing [`Amount::parse`] gate).
fn parse_settlement_amount(body: &str) -> Result<(Amount, Currency)> {
    let (attrs, text) = element(body, "IntrBkSttlmAmt")
        .ok_or_else(|| anyhow!("no `<IntrBkSttlmAmt>` settlement amount element"))?;
    let ccy = attr_value(&attrs, "Ccy")
        .ok_or_else(|| anyhow!("`<IntrBkSttlmAmt>` has no `Ccy` attribute"))?;
    let amount = Amount::parse(&strip_thousands_commas(text.trim()))
        .map_err(|e| anyhow!("settlement amount {text:?} rejected: {e}"))?;
    let currency =
        Currency::parse(&ccy).map_err(|e| anyhow!("settlement currency {ccy:?}: {e}"))?;
    Ok((amount, currency))
}

/// Extract the ISO settlement date from `<IntrBkSttlmDt>YYYY-MM-DD</IntrBkSttlmDt>`.
fn parse_settlement_date(body: &str) -> Result<NaiveDate> {
    let raw = tag_value(body, "IntrBkSttlmDt")
        .ok_or_else(|| anyhow!("no `<IntrBkSttlmDt>` settlement date"))?;
    NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
        .map_err(|e| anyhow!("settlement date {raw:?} not ISO YYYY-MM-DD: {e}"))
}

/// Extract the debtor account's last-4 from the FIRST `<DbtrAcct>` element's
/// inner `<Id>` value — the last four alphanumeric characters of the account id
/// (the IBAN `DO96BPDO00000000000802394189` → `4189`).
fn parse_debtor_last4(body: &str) -> Result<String> {
    let (_, dbtr) =
        element(body, "DbtrAcct").ok_or_else(|| anyhow!("no `<DbtrAcct>` debtor account element"))?;
    let id = innermost_id(&dbtr)
        .ok_or_else(|| anyhow!("`<DbtrAcct>` carries no `<Id>` value"))?;
    let alnum: Vec<char> = id.chars().filter(char::is_ascii_alphanumeric).collect();
    if alnum.len() < 4 {
        return Err(anyhow!("debtor account id {id:?} has fewer than 4 alphanumerics"));
    }
    Ok(alnum[alnum.len() - 4..].iter().collect())
}

/// Extract the creditor agent's BIC from `<CdtrAgt>…<BICFI>VALUE</BICFI>`,
/// normalized to the 8-char institution BIC (a trailing `XXX` branch code is
/// stripped: `CHASUS33XXX` → `CHASUS33`, `ABNANL2AXXX` → `ABNANL2A`).
fn parse_creditor_bic(body: &str) -> Result<String> {
    let (_, cdtr_agt) =
        element(body, "CdtrAgt").ok_or_else(|| anyhow!("no `<CdtrAgt>` creditor agent element"))?;
    let bic = tag_value(&cdtr_agt, "BICFI")
        .ok_or_else(|| anyhow!("`<CdtrAgt>` carries no `<BICFI>` value"))?;
    Ok(normalize_bic(bic.trim()))
}

/// Normalize a SWIFT BIC for map lookup. Only an 11-char BIC whose trailing
/// 3-char branch code is the DEFAULT branch `XXX` (case-insensitive) is cut to
/// its 8-char institution form (`CHASUS33XXX` → `CHASUS33`); an 11-char BIC with
/// a real (non-`XXX`) branch is preserved (`CHASUS33ABC` stays `CHASUS33ABC`),
/// as is an 8-char BIC and any other length. Uppercased so map lookups are
/// case-insensitive on the stored key. A value that is not a configured 8/11-char
/// key simply will not match the map → Review, which is the safe outcome.
fn normalize_bic(bic: &str) -> String {
    let upper = bic.trim().to_ascii_uppercase();
    if upper.len() == 11 && upper.ends_with("XXX") {
        upper[..8].to_string()
    } else {
        upper
    }
}

/// The beneficiary name: the `<Nm>` INSIDE the `<Cdtr>` element. Scoping to the
/// creditor matters because a real pacs.008 lists the DEBTOR (`<Dbtr>`) before
/// the creditor, so an unscoped first-`<Nm>` scan would name the SENDER. Returns
/// `None` when the body has no `<Cdtr>` element or it carries no `<Nm>`.
fn creditor_name(body: &str) -> Option<String> {
    let (_, cdtr) = element(body, "Cdtr")?;
    tag_value(&cdtr, "Nm")
}

/// Extract the UETR (the wire's globally-unique uuid) from `<UETR>uuid</UETR>`.
fn parse_uetr(body: &str) -> Result<String> {
    let raw = tag_value(body, "UETR").ok_or_else(|| anyhow!("no `<UETR>` element"))?;
    let uetr = raw.trim();
    if uetr.is_empty() {
        return Err(anyhow!("`<UETR>` value is empty"));
    }
    Ok(uetr.to_string())
}

// --- tiny tag-scanning helpers (tolerant of interrupted XML) ----------------

/// The text content of the first `<tag>…</tag>` (no attributes), scanning the
/// whole body. Tolerates anything (page-break lines, underscore rules) appearing
/// *between* the open and close tags — it returns everything in between, which
/// for a leaf value element is just the value. Returns `None` if the tag is
/// absent. Trims surrounding whitespace is left to the caller.
fn tag_value(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let rest = &body[start..];
    let end = rest.find(&close)?;
    Some(rest[..end].to_string())
}

/// The `(attributes, inner text)` of the first `<tag ...>…</tag>` element,
/// allowing attributes on the open tag (`<IntrBkSttlmAmt Ccy="USD">`). Scans the
/// whole body. Returns `None` if the tag is absent or its open tag is unclosed.
fn element(body: &str, tag: &str) -> Option<(String, String)> {
    let open_marker = format!("<{tag}");
    // Find an occurrence whose next char is '>' or whitespace, so the marker
    // `<Cdtr` matches `<Cdtr>` / `<Cdtr ...>` but NOT `<CdtrAgt>`/`<CdtrAcct>`
    // (which would otherwise be picked up and paired with the wrong close tag).
    let mut search_from = 0usize;
    let (after_marker, open_end) = loop {
        let rel = body[search_from..].find(&open_marker)?;
        let open_start = search_from + rel;
        let after_marker = open_start + open_marker.len();
        let next = body[after_marker..].chars().next();
        // The marker is a whole tag only when the next char closes the open tag
        // (`>`) or separates an attribute (whitespace). Otherwise it is a prefix
        // of a longer tag name (`Cdtr` in `CdtrAgt`) — skip past and keep looking.
        if matches!(next, Some('>')) || next.is_some_and(char::is_whitespace) {
            let open_end_rel = body[after_marker..].find('>')?;
            break (after_marker, after_marker + open_end_rel);
        }
        search_from = after_marker;
    };
    let attrs = body[after_marker..open_end].trim().to_string();

    let close = format!("</{tag}>");
    let inner_start = open_end + 1;
    let close_rel = body[inner_start..].find(&close)?;
    let inner = body[inner_start..inner_start + close_rel].to_string();
    Some((attrs, inner))
}

/// The value of an attribute `name="value"` within an open-tag attribute string.
fn attr_value(attrs: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let start = attrs.find(&key)? + key.len();
    let rest = &attrs[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The innermost `<Id>` value inside a fragment like
/// `<Id><Othr><Id>DO96BPDO…</Id></Othr></Id>` — the deepest (last) `<Id>…</Id>`,
/// which is the actual account identifier rather than the wrapping scheme node.
fn innermost_id(fragment: &str) -> Option<String> {
    let open = "<Id>";
    let close = "</Id>";
    // The last `<Id>` open tag begins the innermost value (the close tags nest
    // outward, so the final open is the deepest).
    let last_open = fragment.rfind(open)? + open.len();
    let rest = &fragment[last_open..];
    let end = rest.find(close)?;
    Some(rest[..end].trim().to_string())
}

/// Convenience for the Banco Popular adapter: run [`try_parse_swift`] and wrap a
/// successful parse in [`Outcome::Transfer`]. Mirrors the `Option<Result<..>>`
/// shape `deterministic_extract` returns, so the adapter is a thin pass-through.
#[must_use]
pub fn try_parse_swift_outcome(body: &str) -> Option<Result<Outcome>> {
    try_parse_swift(body).map(|r| r.map(Outcome::Transfer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    /// Real sample 1 (after the Gmail-forward unwrap): a pacs.008 confirmation
    /// with page-break lines interrupting the XML.
    fn sample_1() -> &'static str {
        "Confirmacion Mensaje Swift Operado\n\
         _______________________________________________________________\n\
         29/05/26-05:12:56        ICMACKINTERTMX-0277-000001            1\n\
         \n\
       MX Input     : pacs.008.001.08 CBPRPlus-pacs.008.001.08_FIToFICustomerCreditTransfer\n\
             <UETR>5dd60267-659f-446e-92c4-c1540b8f8253</UETR>\n\
             <IntrBkSttlmAmt Ccy=\"USD\">2100.00</IntrBkSttlmAmt>\n\
             <IntrBkSttlmDt>2026-05-29</IntrBkSttlmDt>\n\
         29/05/26-05:12:56        ICMACKINTERTMX-0277-000001            2\n\
         _______________________________________________________________\n\
             <InstdAmt Ccy=\"USD\">2100.00</InstdAmt>\n\
             <DbtrAcct><Id><Othr><Id>DO96BPDO00000000000802394189</Id></Othr></Id></DbtrAcct>\n\
             <DbtrAgt><FinInstnId><BICFI>BPDODOSX</BICFI></FinInstnId></DbtrAgt>\n\
             <CdtrAgt><FinInstnId><BICFI>CHASUS33XXX</BICFI></FinInstnId></CdtrAgt>\n\
             <Cdtr><Nm>RODOLFO HANSEN</Nm></Cdtr>\n\
             <CdtrAcct><Id><Othr><Id>123720130</Id></Othr></Id></CdtrAcct>\n"
    }

    /// Real sample 2: a different amount, UETR and creditor BIC, same debtor.
    fn sample_2() -> &'static str {
        "Confirmacion Mensaje Swift Operado\n\
       MX Input     : pacs.008.001.08 CBPRPlus-pacs.008.001.08_FIToFICustomerCreditTransfer\n\
             <UETR>e5b9060e-1473-44b9-ba24-37db7e7cbc9c</UETR>\n\
             <IntrBkSttlmAmt Ccy=\"USD\">4000.00</IntrBkSttlmAmt>\n\
             <IntrBkSttlmDt>2026-05-29</IntrBkSttlmDt>\n\
             <DbtrAcct><Id><Othr><Id>DO96BPDO00000000000802394189</Id></Othr></Id></DbtrAcct>\n\
             <DbtrAgt><FinInstnId><BICFI>BPDODOSX</BICFI></FinInstnId></DbtrAgt>\n\
             <CdtrAgt><FinInstnId><BICFI>ABNANL2AXXX</BICFI></FinInstnId></CdtrAgt>\n\
             <Cdtr><Nm>RODOLFO HANSEN</Nm></Cdtr>\n"
    }

    /// A normal Banco Popular consumo notification body — NOT a SWIFT message.
    fn consumo_body() -> &'static str {
        "Notificación de Consumo\n\
         Monto | Moneda | Fecha | Comercio | Estatus\n\
         EUR$1.50 | Euro | 27/05/2026 | Example Cafe Amsterdam | Aprobada\n\
         Tarjeta terminada en 1234"
    }

    fn ok(body: &str) -> TransferRecord {
        try_parse_swift(body)
            .expect("a SWIFT confirmation yields Some")
            .expect("the sample parses")
    }

    /// The debtor last-4 carried by a SWIFT [`SourceHint`], or a panic if the
    /// source is not a SWIFT debtor.
    fn swift_debtor_last4(t: &TransferRecord) -> &str {
        match &t.source {
            SourceHint::SwiftDebtorLast4(l4) => l4,
            other => panic!("expected a SWIFT debtor source hint, got {other:?}"),
        }
    }

    #[test]
    fn parses_sample_1() {
        let t = ok(sample_1());
        assert_eq!(t.money.amount.value(), Decimal::from_str("2100.00").unwrap());
        assert_eq!(t.money.currency.as_str(), "USD");
        assert_eq!(t.date, NaiveDate::from_ymd_opt(2026, 5, 29).unwrap());
        assert_eq!(swift_debtor_last4(&t), "4189");
        assert_eq!(t.dest, DestHint::CreditorBic("CHASUS33".to_string()));
        assert_eq!(t.external_id, "swift:5dd60267-659f-446e-92c4-c1540b8f8253");
        assert_eq!(t.description, "SWIFT wire to RODOLFO HANSEN");
    }

    #[test]
    fn parses_sample_2() {
        let t = ok(sample_2());
        assert_eq!(t.money.amount.value(), Decimal::from_str("4000.00").unwrap());
        assert_eq!(t.money.currency.as_str(), "USD");
        assert_eq!(t.date, NaiveDate::from_ymd_opt(2026, 5, 29).unwrap());
        assert_eq!(swift_debtor_last4(&t), "4189");
        assert_eq!(t.dest, DestHint::CreditorBic("ABNANL2A".to_string()));
        assert_eq!(t.external_id, "swift:e5b9060e-1473-44b9-ba24-37db7e7cbc9c");
    }

    #[test]
    fn page_break_interruptions_do_not_break_parsing() {
        // Sample 1 specifically interleaves page-break lines and underscore rules
        // between the SWIFT fields; it must still parse every field by tag.
        let t = ok(sample_1());
        assert_eq!(t.money.amount.value(), Decimal::from_str("2100.00").unwrap());
        assert_eq!(swift_debtor_last4(&t), "4189");
    }

    #[test]
    fn page_break_inside_a_field_value_rejoins() {
        // Fix 6: an interruption lands BETWEEN the `<IntrBkSttlmAmt Ccy="USD">`
        // open tag and its amount text. After page-break stripping the value
        // rejoins and the field parses (it would not survive line-position
        // extraction).
        let body = "Confirmacion Mensaje Swift Operado\n\
           MX Input : pacs.008.001.08 FIToFICustomerCreditTransfer\n\
                 <UETR>5dd60267-659f-446e-92c4-c1540b8f8253</UETR>\n\
                 <IntrBkSttlmAmt Ccy=\"USD\">\n\
             29/05/26-05:12:56        ICMACKINTERTMX-0277-000001            2\n\
             _______________________________________________________________\n\
             2100.00</IntrBkSttlmAmt>\n\
                 <IntrBkSttlmDt>2026-05-29</IntrBkSttlmDt>\n\
                 <DbtrAcct><Id><Othr><Id>DO96BPDO00000000000802394189</Id></Othr></Id></DbtrAcct>\n\
                 <CdtrAgt><FinInstnId><BICFI>CHASUS33XXX</BICFI></FinInstnId></CdtrAgt>\n\
                 <Cdtr><Nm>RODOLFO HANSEN</Nm></Cdtr>\n";
        let t = ok(body);
        assert_eq!(t.money.amount.value(), Decimal::from_str("2100.00").unwrap());
        assert_eq!(t.money.currency.as_str(), "USD");
    }

    #[test]
    fn consumo_body_is_not_swift() {
        assert!(try_parse_swift(consumo_body()).is_none());
    }

    #[test]
    fn consumo_mentioning_mensaje_swift_is_not_swift() {
        // Fix 2: a consumo body that merely contains the prose phrase "mensaje
        // swift" must NOT be classified as a SWIFT confirmation (no pacs.008
        // structure → None → falls through to the LLM path).
        let body = "Notificación de Consumo\n\
             Su mensaje swift fue procesado? No: esto es un consumo normal.\n\
             EUR$1.50 | Euro | 27/05/2026 | Example Cafe | Aprobada\n\
             Tarjeta terminada en 1234";
        assert!(
            try_parse_swift(body).is_none(),
            "prose 'mensaje swift' with no pacs.008 structure is NOT a SWIFT wire"
        );
    }

    #[test]
    fn real_wire_is_classified_swift() {
        // Fix 2: a real wire (pacs.008 document marker + settlement element) IS
        // classified as SWIFT and parses.
        assert!(try_parse_swift(sample_1()).is_some());
        let t = ok(sample_1());
        assert_eq!(t.money.currency.as_str(), "USD");
    }

    #[test]
    fn bic_normalization_strips_only_default_branch() {
        // Fix 7: only an 11-char BIC ending in the default branch `XXX` is cut.
        assert_eq!(normalize_bic("CHASUS33XXX"), "CHASUS33");
        assert_eq!(normalize_bic("ABNANL2AXXX"), "ABNANL2A");
        // An already-8-char BIC is unchanged; case is normalized.
        assert_eq!(normalize_bic("CHASUS33"), "CHASUS33");
        assert_eq!(normalize_bic("BPDODOSX"), "BPDODOSX");
        assert_eq!(normalize_bic("chasus33"), "CHASUS33");
        // A real (non-XXX) 11-char branch is NOT stripped — over-stripping it
        // would collapse distinct branches onto one institution key.
        assert_eq!(normalize_bic("CHASUS33ABC"), "CHASUS33ABC");
        assert_eq!(normalize_bic("chasus33abc"), "CHASUS33ABC");
        // The default branch is recognised case-insensitively.
        assert_eq!(normalize_bic("CHASUS33xxx"), "CHASUS33");
    }

    #[test]
    fn creditor_name_scoped_to_cdtr_not_debtor() {
        // Fix 3: a body with BOTH a debtor name (which appears first) and a
        // creditor name must describe the wire by the CREDITOR (beneficiary).
        let body = "Confirmacion Mensaje Swift Operado\n\
           MX Input : pacs.008.001.08 FIToFICustomerCreditTransfer\n\
                 <UETR>5dd60267-659f-446e-92c4-c1540b8f8253</UETR>\n\
                 <IntrBkSttlmAmt Ccy=\"USD\">2100.00</IntrBkSttlmAmt>\n\
                 <IntrBkSttlmDt>2026-05-29</IntrBkSttlmDt>\n\
                 <Dbtr><Nm>SENDER</Nm></Dbtr>\n\
                 <DbtrAcct><Id><Othr><Id>DO96BPDO00000000000802394189</Id></Othr></Id></DbtrAcct>\n\
                 <CdtrAgt><FinInstnId><BICFI>CHASUS33XXX</BICFI></FinInstnId></CdtrAgt>\n\
                 <Cdtr><Nm>RECIPIENT</Nm></Cdtr>\n";
        let t = ok(body);
        assert_eq!(t.description, "SWIFT wire to RECIPIENT");
    }

    #[test]
    fn debtor_last4_takes_last_four_alphanumerics_of_iban() {
        assert_eq!(parse_debtor_last4(sample_1()).unwrap(), "4189");
    }

    #[test]
    fn settlement_amount_preferred_over_instructed_amount() {
        // Sample 1 carries both IntrBkSttlmAmt and InstdAmt; we read the former.
        let (amount, currency) = parse_settlement_amount(sample_1()).unwrap();
        assert_eq!(amount.value(), Decimal::from_str("2100.00").unwrap());
        assert_eq!(currency.as_str(), "USD");
    }

    #[test]
    fn swift_marker_but_missing_field_is_error() {
        // Carries the structural markers (pacs.008 doc + settlement element, so it
        // IS classified as SWIFT) but the settlement amount is unparseable and the
        // date/debtor/BIC/UETR are absent → Some(Err(..)) (→ Review), never a
        // silent skip.
        let body = "MX Input : pacs.008.001.08 FIToFICustomerCreditTransfer\n\
             <IntrBkSttlmAmt Ccy=\"USD\">not-a-number</IntrBkSttlmAmt>\nincomplete";
        let r = try_parse_swift(body).expect("markers present → Some");
        assert!(r.is_err(), "missing/malformed required fields must be an Err");
    }

    #[test]
    fn outcome_wrapper_yields_transfer() {
        let outcome = try_parse_swift_outcome(sample_1())
            .expect("Some for a SWIFT body")
            .expect("parses");
        match outcome {
            Outcome::Transfer(t) => assert_eq!(swift_debtor_last4(&t), "4189"),
            other => panic!("expected transfer, got {other:?}"),
        }
    }

    #[test]
    fn multi_transaction_wire_routes_to_review() {
        // Fix 1: a pacs.008 declaring NbOfTxs=2 (and carrying two CdtTrfTxInf
        // blocks) must NOT silently book only the first — it is Some(Err) → Review.
        let body = "Confirmacion Mensaje Swift Operado\n\
           MX Input : pacs.008.001.08 FIToFICustomerCreditTransfer\n\
                 <GrpHdr><NbOfTxs>2</NbOfTxs></GrpHdr>\n\
                 <CdtTrfTxInf>\n\
                 <UETR>5dd60267-659f-446e-92c4-c1540b8f8253</UETR>\n\
                 <IntrBkSttlmAmt Ccy=\"USD\">2100.00</IntrBkSttlmAmt>\n\
                 <IntrBkSttlmDt>2026-05-29</IntrBkSttlmDt>\n\
                 <DbtrAcct><Id><Othr><Id>DO96BPDO00000000000802394189</Id></Othr></Id></DbtrAcct>\n\
                 <CdtrAgt><FinInstnId><BICFI>CHASUS33XXX</BICFI></FinInstnId></CdtrAgt>\n\
                 <Cdtr><Nm>RECIPIENT ONE</Nm></Cdtr>\n\
                 </CdtTrfTxInf>\n\
                 <CdtTrfTxInf>\n\
                 <UETR>e5b9060e-1473-44b9-ba24-37db7e7cbc9c</UETR>\n\
                 <IntrBkSttlmAmt Ccy=\"USD\">4000.00</IntrBkSttlmAmt>\n\
                 <Cdtr><Nm>RECIPIENT TWO</Nm></Cdtr>\n\
                 </CdtTrfTxInf>\n";
        let r = try_parse_swift(body).expect("a SWIFT body → Some");
        let err = r.expect_err("a multi-transaction wire must be an Err (→ Review)");
        let msg = err.to_string();
        assert!(msg.contains("multi-transaction"), "{msg}");
        assert!(msg.contains("2 txns"), "{msg}");
    }

    #[test]
    fn cross_currency_wire_routes_to_review() {
        // Fix 4: an FX wire instructing DOP but settling USD must NOT be booked as
        // a same-currency transfer — it is Some(Err) → Review.
        let body = "Confirmacion Mensaje Swift Operado\n\
           MX Input : pacs.008.001.08 FIToFICustomerCreditTransfer\n\
                 <UETR>5dd60267-659f-446e-92c4-c1540b8f8253</UETR>\n\
                 <IntrBkSttlmAmt Ccy=\"USD\">2100.00</IntrBkSttlmAmt>\n\
                 <IntrBkSttlmDt>2026-05-29</IntrBkSttlmDt>\n\
                 <InstdAmt Ccy=\"DOP\">125000.00</InstdAmt>\n\
                 <DbtrAcct><Id><Othr><Id>DO96BPDO00000000000802394189</Id></Othr></Id></DbtrAcct>\n\
                 <CdtrAgt><FinInstnId><BICFI>CHASUS33XXX</BICFI></FinInstnId></CdtrAgt>\n\
                 <Cdtr><Nm>RECIPIENT</Nm></Cdtr>\n";
        let r = try_parse_swift(body).expect("a SWIFT body → Some");
        let err = r.expect_err("a cross-currency wire must be an Err (→ Review)");
        let msg = err.to_string();
        assert!(msg.contains("cross-currency"), "{msg}");
        assert!(msg.contains("DOP") && msg.contains("USD"), "{msg}");
    }

    #[test]
    fn same_currency_instructed_amount_is_fine() {
        // The normal case: InstdAmt and IntrBkSttlmAmt share a currency (sample 1
        // carries both as USD) → no cross-currency error.
        assert!(guard_single_currency(sample_1(), "USD").is_ok());
    }
}
