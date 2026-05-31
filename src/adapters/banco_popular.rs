//! Banco Popular Dominicano adapter.
//!
//! Banco Popular sends a Spanish "Notificación de Consumo" whenever a card is
//! charged. Unlike PayPal there is no stable transaction id, so dedup falls back
//! to the composite hash (see [`crate::dedup`]). The notification carries a
//! small table — `Monto | Moneda | Fecha | Comercio | Estatus` (sometimes with
//! an extra `Razón` column) — which we ask the LLM to flatten into JSON, then
//! parse here into [`Extracted`].
//!
//! Three quirks drive the parsing:
//! - the amount is prefixed with a currency code (`EUR$1.50`), which must be
//!   stripped to a bare decimal,
//! - amounts are rendered with a thousands separator (`JPY$5,130.00`), which we
//!   strip in this adapter's parse step BEFORE the [`crate::schema::Amount`]
//!   sanitizing gate so the gate sees a clean decimal (`5130.00`), and
//! - dates are `DD/MM/YYYY`, NOT US `m/d` — `27/05/2026` is 27 May, not an
//!   invalid month. The date format list reflects this.
//!
//! ## Two body shapes, one sender
//!
//! The same sender (`notificaciones@popularenlinea.com`) also delivers a
//! "Confirmación Mensaje Swift Operado" — an outbound international-wire
//! confirmation carrying a SWIFT pacs.008 message. Because adapter selection is
//! sender-only, this adapter must handle BOTH shapes. It disambiguates from the
//! *body* in [`deterministic_extract`](BancoPopularAdapter::deterministic_extract):
//! a SWIFT confirmation ([`swift::try_parse_swift`] matches) is parsed
//! deterministically into an [`Outcome::Transfer`] (BPD debtor account → the
//! user's own foreign account); a normal consumo returns `None` there and falls
//! through UNCHANGED to the LLM `prompt`/`postprocess` path below.

use anyhow::{Context, Result, anyhow};
use chrono::NaiveDate;
use serde_json::Value;

use super::parse::{
    collect_objects, currency_field, parse_amount_with, parse_date_with, string_field,
    strip_thousands_commas,
};
use super::swift;
use super::{Adapter, Outcome};
use crate::schema::{Direction, Extracted, Money, Source};

/// Sender substring that identifies a Banco Popular consumo notification.
const BANCO_SENDER: &str = "popularenlinea.com";

pub struct BancoPopularAdapter;

impl Adapter for BancoPopularAdapter {
    fn name(&self) -> &'static str {
        "banco_popular"
    }

    fn matches(&self, original_sender: &str) -> bool {
        original_sender.contains(BANCO_SENDER)
    }

    /// Both a consumo notification and a SWIFT confirmation are real
    /// transactions, so neither is a clean skip. A consumo carries no rigid
    /// pre-LLM marker, so we keep the permissive default (`true`) for it and let
    /// the LLM + validation gates decide; a SWIFT body is likewise a transaction
    /// (handled deterministically in [`deterministic_extract`](Self::deterministic_extract)).
    /// Returning `true` for both ensures the SWIFT body is not skipped before the
    /// deterministic seam runs.
    fn is_transaction(&self, _body: &str) -> bool {
        true
    }

    /// Disambiguate the two body shapes this single sender delivers. A SWIFT
    /// pacs.008 confirmation is parsed deterministically into an
    /// [`Outcome::Transfer`] (`Some(Ok(..))`), or `Some(Err(..))` when it looks
    /// like SWIFT but a required field is missing (→ Review). A normal consumo
    /// returns `None`, falling through UNCHANGED to the LLM `prompt`/`postprocess`
    /// path. Pure: no I/O.
    fn deterministic_extract(&self, body: &str) -> Option<Result<Outcome>> {
        swift::try_parse_swift_outcome(body)
    }

    fn prompt(&self, email_text: &str) -> String {
        format!(
            r#"Extraes una transacción de una "Notificación de Consumo" del Banco Popular Dominicano.
Devuelve SOLO un objeto JSON (sin prosa, sin bloques de markdown) con EXACTAMENTE estas claves:

{{
  "amount": string,        // el Monto como decimal puro
  "currency": string,      // código ISO-4217 de 3 letras MAYÚSCULAS
  "direction": "out",      // un consumo es siempre una compra/cargo: "out"
  "date": string,          // la Fecha en ISO YYYY-MM-DD
  "merchant": string,      // el valor de la columna Comercio
  "account_hint": string,  // los últimos 4 dígitos de la tarjeta, p.ej. "4417"
  "status": string,        // "Aprobada" o "Declinada"
  "raw_ref": string        // "" (no hay id de referencia)
}}

La tabla tiene columnas Monto | Moneda | Fecha | Comercio | Estatus (a veces sin
la columna Moneda, o con una columna extra Razón). El Monto viene con un prefijo
de moneda y "$", p.ej. "EUR$1.50", "JPY$5,130.00", "US$65.33", "RD$1,450.00".

MONTO ("amount"):
- Quita el prefijo de moneda Y el signo "$", y quita los separadores de miles
  (comas). Deja un decimal positivo con punto. Ejemplos:
    "EUR$1.50"     -> "1.50"
    "JPY$5,130.00" -> "5130.00"
    "US$65.33"     -> "65.33"
    "RD$1,450.00"  -> "1450.00"

MONEDA ("currency") — SIEMPRE un código ISO-4217 de 3 letras en mayúsculas,
NUNCA el nombre en español. Decide usando el prefijo "XXX$" del Monto Y/O el
nombre de la columna "Moneda":
- prefijo "EUR$"  o  Moneda "Euro"                         -> EUR
- prefijo "JPY$"  o  Moneda "Yen"                          -> JPY
- prefijo "KRW$"  o  Moneda "Won"                          -> KRW
- prefijo "US$"/"USD$" o Moneda "Dólar"/"Dólares"/"Dólar estadounidense" -> USD
- prefijo "RD$"   o  Moneda "Peso"/"Pesos"/"Peso dominicano"             -> DOP
- prefijo "GBP$"  o  Moneda "Libra"/"Libra esterlina"      -> GBP
- Moneda "Franco suizo" -> CHF; "Yuan" -> CNY;
  "Dólar canadiense" -> CAD; "Dólar australiano" -> AUD.
- Si el Monto trae un prefijo ISO claro (p.ej. "JPY$..."), usa ESE código tal
  cual aunque la columna Moneda use otro nombre.

FECHA ("date") — la FUENTE viene en DD/MM/YYYY (día primero, NO mes primero).
Conviértela a ISO YYYY-MM-DD: "27/05/2026" -> "2026-05-27"; "04/03/2026" ->
"2026-03-04" (4 de marzo, no 3 de abril).

ESTATUS ("status") — copia el texto: "Aprobada" si la transacción se aprobó,
"Declinada" si se rechazó/declinó.

ACCOUNT_HINT — los 4 dígitos de "Tarjeta ... terminada en NNNN", p.ej. "4417".

Ejemplos:
- "EUR$64.20  Euro  04/03/2026  Tulip Press Studio  Aprobada"
  -> {{"amount":"64.20","currency":"EUR","date":"2026-03-04","merchant":"Tulip Press Studio","status":"Aprobada","direction":"out","account_hint":"4417","raw_ref":""}}
- "JPY$5,130.00  Yen  12/03/2026  Sakura Ramen House  Aprobada"
  -> {{"amount":"5130.00","currency":"JPY", ...}}
- "KRW$8,700.00  Won  19/03/2026  Hanok Tea Garden  Aprobada"
  -> {{"amount":"8700.00","currency":"KRW", ...}}
- "RD$1,450.00  Peso dominicano  25/03/2026  Colmado La Esquina  Aprobada"
  -> {{"amount":"1450.00","currency":"DOP", ...}}
- "USD$318.40  Dólar  27/03/2026  Velocity Auto Parts  Declinada"
  -> {{"amount":"318.40","currency":"USD","status":"Declinada", ...}}

No inventes valores; si un campo realmente no está presente usa "".

Notificación:
---
{email_text}
---"#
        )
    }

    fn postprocess(&self, json: &Value) -> Result<Outcome> {
        let objects = collect_objects(json);
        if objects.is_empty() {
            return Err(anyhow!("LLM JSON contained no transaction object"));
        }
        let records = objects.iter().map(parse_one).collect::<Result<Vec<_>>>()?;
        Ok(Outcome::Transaction(records))
    }
}

/// Parse one JSON object into a typed [`Extracted`]. A consumo has no
/// transaction id, so `external_id` is always `None` and `direction` is always
/// `Out`.
fn parse_one(obj: &Value) -> Result<Extracted> {
    let map = obj
        .as_object()
        .ok_or_else(|| anyhow!("expected JSON object, got {obj}"))?;

    // Strip the thousands separator HERE, before the sanitizing amount gate, so
    // a legitimately-grouped `5,130.00` becomes a clean `5130.00`.
    let amount =
        parse_amount_with(map.get("amount"), strip_thousands_commas).context("parsing `amount`")?;
    let currency = currency_field(map, "currency")?;
    let date = parse_date(map.get("date")).context("parsing `date`")?;
    let merchant = string_field(map, "merchant").ok_or_else(|| anyhow!("missing `merchant`"))?;
    let status = string_field(map, "status").unwrap_or_default();

    let account_hint = string_field(map, "account_hint");
    let raw_ref = string_field(map, "raw_ref").unwrap_or_default();

    Ok(Extracted {
        source: Source::BancoPopular,
        // No transaction id — dedup composite-hashes id-less records.
        external_id: None,
        money: Money::new(amount, currency),
        // A consumo is always an outgoing charge.
        direction: Direction::Out,
        date,
        merchant,
        account_hint,
        status,
        raw_ref,
    })
}

/// Accept ISO `YYYY-MM-DD` first, then Banco Popular's `DD/MM/YYYY`. Crucially
/// `%d/%m/%Y` (day-first), NOT US `%m/%d/%Y` — `27/05/2026` is 27 May.
fn parse_date(v: Option<&Value>) -> Result<NaiveDate> {
    const FORMATS: &[&str] = &["%Y-%m-%d", "%d/%m/%Y"];
    parse_date_with(v, FORMATS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::{Verdict, validate};
    use rust_decimal::Decimal;
    use serde_json::json;
    use std::str::FromStr;

    /// JSON a correct model returns for an APPROVED consumo (the autoforward
    /// fixture): `EUR$1.50  Euro  27/05/2026  Example Cafe Amsterdam  Aprobada`.
    fn approved_json() -> Value {
        json!({
            "amount": "1.50",
            "currency": "EUR",
            "direction": "out",
            "date": "27/05/2026",
            "merchant": "Example Cafe Amsterdam",
            "account_hint": "1234",
            "status": "Aprobada",
            "raw_ref": ""
        })
    }

    /// The single record from a `Transaction` outcome, or a panic.
    fn one(outcome: Outcome) -> Extracted {
        match outcome {
            Outcome::Transaction(mut v) => {
                assert_eq!(v.len(), 1);
                v.pop().unwrap()
            }
            Outcome::Transfer(_) => panic!("expected transaction, got transfer"),
            Outcome::NotATransaction { reason } => {
                panic!("expected transaction, got skip: {reason}")
            }
        }
    }

    #[test]
    fn matches_banco_sender() {
        assert!(BancoPopularAdapter.matches("notificaciones@popularenlinea.com"));
        assert!(BancoPopularAdapter.matches("<notificaciones@popularenlinea.com>"));
        assert!(!BancoPopularAdapter.matches("service@paypal.com"));
    }

    /// A SWIFT confirmation body shares the consumo sender, so it must be
    /// disambiguated in `deterministic_extract` and yield a Transfer.
    fn swift_body() -> &'static str {
        "Confirmacion Mensaje Swift Operado\n\
       MX Input : pacs.008.001.08 FIToFICustomerCreditTransfer\n\
             <UETR>5dd60267-659f-446e-92c4-c1540b8f8253</UETR>\n\
             <IntrBkSttlmAmt Ccy=\"USD\">2100.00</IntrBkSttlmAmt>\n\
             <IntrBkSttlmDt>2026-05-29</IntrBkSttlmDt>\n\
             <DbtrAcct><Id><Othr><Id>DO96BPDO00000000000802394189</Id></Othr></Id></DbtrAcct>\n\
             <CdtrAgt><FinInstnId><BICFI>CHASUS33XXX</BICFI></FinInstnId></CdtrAgt>\n\
             <Cdtr><Nm>RODOLFO HANSEN</Nm></Cdtr>\n"
    }

    /// A plain-text consumo body (NOT the LLM JSON) — the input the pipeline
    /// hands `deterministic_extract`. It must NOT be classified as SWIFT.
    fn consumo_body() -> &'static str {
        "Notificación de Consumo\n\
         EUR$1.50  Euro  27/05/2026  Example Cafe Amsterdam  Aprobada\n\
         Tarjeta terminada en 1234"
    }

    #[test]
    fn deterministic_extract_takes_over_swift_body() {
        // A SWIFT confirmation → Some(Ok(Transfer)); the consumo's LLM path is
        // bypassed for it.
        let outcome = BancoPopularAdapter
            .deterministic_extract(swift_body())
            .expect("SWIFT body → Some")
            .expect("the sample parses");
        match outcome {
            Outcome::Transfer(t) => {
                assert_eq!(
                    t.source,
                    crate::adapters::SourceHint::SwiftDebtorLast4("4189".to_string())
                );
                assert_eq!(t.external_id, "swift:5dd60267-659f-446e-92c4-c1540b8f8253");
            }
            other => panic!("expected transfer, got {other:?}"),
        }
    }

    #[test]
    fn deterministic_extract_passes_through_consumo_to_llm() {
        // A normal consumo body → None, so the pipeline uses the LLM prompt path
        // (UNCHANGED). SWIFT handling never touches the consumo route.
        assert!(BancoPopularAdapter.deterministic_extract(consumo_body()).is_none());
    }

    #[test]
    fn approved_consumo_postprocesses_and_books() {
        let e = one(BancoPopularAdapter.postprocess(&approved_json()).unwrap());

        assert_eq!(e.source, Source::BancoPopular);
        assert_eq!(e.external_id, None);
        assert_eq!(e.amount().value(), Decimal::from_str("1.50").unwrap());
        assert_eq!(e.currency().as_str(), "EUR");
        assert_eq!(e.direction, Direction::Out);
        assert_eq!(e.merchant, "Example Cafe Amsterdam");
        assert_eq!(e.account_hint.as_deref(), Some("1234"));

        match validate(e) {
            Verdict::Booked(b) => {
                assert_eq!(b.as_extracted().source, Source::BancoPopular);
                assert_eq!(b.as_extracted().external_id, None);
            }
            Verdict::Review { reason } => panic!("approved consumo should book: {reason}"),
        }
    }

    #[test]
    fn declined_consumo_routes_to_review() {
        let mut v = approved_json();
        v["status"] = json!("Declinada");
        v["amount"] = json!("49.08");
        v["merchant"] = json!("Example Shop B.V.");
        let e = one(BancoPopularAdapter.postprocess(&v).unwrap());
        assert!(matches!(validate(e), Verdict::Review { .. }));
    }

    #[test]
    fn date_is_day_first_not_us_month_first() {
        // 27/05/2026: day 27 > 12, so this is unambiguously DD/MM. A US m/d
        // parser would reject it (month 27 invalid) — assert the correct day.
        let e = one(BancoPopularAdapter.postprocess(&approved_json()).unwrap());
        assert_eq!(e.date, NaiveDate::from_ymd_opt(2026, 5, 27).unwrap());
    }

    #[test]
    fn strips_currency_prefix_when_model_leaves_it() {
        // Defensive: a model that fails to strip "EUR$" leaves a non-decimal
        // amount, which must error rather than book a garbage figure.
        let mut v = approved_json();
        v["amount"] = json!("EUR$1.50");
        assert!(BancoPopularAdapter.postprocess(&v).is_err());
    }

    #[test]
    fn strips_thousands_separator_in_adapter() {
        // The adapter strips the thousands comma BEFORE the amount gate, so a
        // legitimately-grouped figure books cleanly even if the model leaves it.
        let mut v = approved_json();
        v["currency"] = json!("JPY");
        v["amount"] = json!("5,130.00");
        v["merchant"] = json!("Example Ramen Tokyo");
        let e = one(BancoPopularAdapter.postprocess(&v).unwrap());
        assert_eq!(e.amount().value(), Decimal::from_str("5130.00").unwrap());
    }

    #[test]
    fn accepts_transactions_wrapper() {
        let v = json!({ "transactions": [approved_json()] });
        match BancoPopularAdapter.postprocess(&v).unwrap() {
            Outcome::Transaction(v) => assert_eq!(v.len(), 1),
            Outcome::Transfer(_) => panic!("unexpected transfer"),
            Outcome::NotATransaction { reason } => panic!("unexpected skip: {reason}"),
        }
    }

    /// The model maps "Moneda" = "Yen" (rendered `JPY$5,130.00`) to ISO "JPY".
    /// We assert the postprocessed record carries currency "JPY" and the bare
    /// decimal amount (the model strips the prefix + thousands separator).
    #[test]
    fn yen_row_postprocesses_with_jpy_currency() {
        let mut v = approved_json();
        v["currency"] = json!("JPY");
        v["amount"] = json!("5130.00");
        v["merchant"] = json!("Example Ramen Tokyo");
        let e = one(BancoPopularAdapter.postprocess(&v).unwrap());
        assert_eq!(e.currency().as_str(), "JPY");
        assert_eq!(e.amount().value(), Decimal::from_str("5130.00").unwrap());
        // JPY is a known currency, so an approved row books.
        assert!(matches!(validate(e), Verdict::Booked(_)));
    }

    /// The model maps "Moneda" = "Won" (rendered `KRW$8,700.00`) to ISO "KRW".
    #[test]
    fn won_row_postprocesses_with_krw_currency() {
        let mut v = approved_json();
        v["currency"] = json!("KRW");
        v["amount"] = json!("8700.00");
        v["merchant"] = json!("Example Bibimbap Seoul");
        let e = one(BancoPopularAdapter.postprocess(&v).unwrap());
        assert_eq!(e.currency().as_str(), "KRW");
        assert_eq!(e.amount().value(), Decimal::from_str("8700.00").unwrap());
        assert!(matches!(validate(e), Verdict::Booked(_)));
    }

    // --- property tests --------------------------------------------------

    use proptest::prelude::*;

    proptest! {
        /// Banco Popular dates are day-first (`%d/%m/%Y`): a `DD/MM/YYYY` source
        /// must parse to exactly that day/month, never the US month-first
        /// interpretation. Generates day > 12 so the two interpretations differ
        /// (a US parser would reject month > 12).
        #[test]
        fn prop_date_is_day_first(day in 13u32..=28, month in 1u32..=12, year in 2020i32..=2030) {
            let mut v = approved_json();
            v["date"] = json!(format!("{day:02}/{month:02}/{year}"));
            let e = one(BancoPopularAdapter.postprocess(&v).unwrap());
            prop_assert_eq!(e.date, NaiveDate::from_ymd_opt(year, month, day).unwrap());
        }
    }
}
