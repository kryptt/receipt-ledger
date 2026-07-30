//! Deterministic statement parser: [`TextRow`]s → [`ParsedStatement`].
//!
//! No PDF dependency — input is the geometry-only [`TextRow`] produced by
//! [`super::pdf`], so the whole grammar is exercised by unit tests against
//! synthetic rows. The grammar mirrors the real statement (validated in
//! Phase 0):
//!
//! - Sections begin with a `VISA PRESTIGE DOP|USD` title, followed by a card
//!   row `****-****-****-NNNN  <línea> <disponible> <corte> <límite>
//!   <balance anterior>` — currency drives account routing, `corte` (FECHA DE
//!   CORTE) anchors year inference.
//! - A transaction is `ENTRADA(DD/MM)  TRANSAC(DD/MM)  REF  DESC [LOC]
//!   [-]AMOUNT`, optionally followed by a continuation row `MCC(4)  AUTH(6)`.
//! - A leading `-` on the amount marks a credit/payment (`Direction::In`).
//! - Transaction dates carry no year; it is inferred from the cut date: a month
//!   after the cut month belongs to the previous year (the Dec→Jan wrap).
//!
//! The parse is two-stage: every row is first classified into a [`RowKind`]
//! independently of surrounding rows (precedence visible in [`classify_row`]),
//! then a single exhaustive `match` folds the kinds into the statement, carrying
//! only the section currency, its cut date, and the balance-capture flag as
//! state. New row kinds force the `match` to be revisited — there is no silent
//! fall-through.

use anyhow::{Context, Result, anyhow};
use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;

use super::{
    AuthCode, Last4, Mcc, ParsedStatement, Reference, Section, SectionCurrency, StatementTxn,
    TextRow, join_cells,
};
use crate::adapters::parse::strip_thousands_commas;
use crate::eval::scorer::collapse_whitespace;
use crate::schema::{Amount, Direction, Money};

/// What a single row is, decided without reference to its neighbours. The
/// borrow on [`RowKind::Txn`] defers the (cut-date-dependent) field parse to the
/// fold, where the section context is known.
/// A footer numeric value row carries at least this many decimal cells
/// (`CUOTAS  MONTO  PAGO MÍN  A PAGAR  TOTAL`), distinguishing it from interest
/// lines that show a single figure.
const FOOTER_MIN_DECIMAL_CELLS: usize = 4;

enum RowKind<'a> {
    /// `VISA PRESTIGE DOP|USD` — opens a section.
    SectionTitle(SectionCurrency),
    /// `****-****-****-NNNN … <corte> … <balance anterior>` — the card/header row.
    Card {
        last4: Last4,
        cut: NaiveDate,
        balance_anterior: Option<rust_decimal::Decimal>,
    },
    /// A transaction row. The reference is already parsed during classification
    /// (it needs no section context); only the year-inference of the dates is
    /// deferred to the fold, where the cut date is known.
    Txn {
        reference: Reference,
        row: &'a TextRow,
    },
    /// `MCC(4)  AUTH(6)` continuation for the preceding transaction.
    Continuation { mcc: Mcc, auth: AuthCode },
    /// The footer label row carrying `BALANCE TOTAL`.
    BalanceLabel,
    /// A footer numeric row whose last cell is a balance figure.
    BalanceValue(Decimal),
    /// Marketing copy, summaries, per-page header repeats — skipped.
    Ignored,
}

/// Parse the ordered statement rows into sections + transactions.
///
/// Returns `Err` only on a structurally unusable statement (no section header,
/// or a transaction whose date/amount cannot be parsed). Non-transaction rows
/// are classified [`RowKind::Ignored`] and skipped.
pub fn parse_statement(rows: &[TextRow]) -> Result<ParsedStatement> {
    let mut out = ParsedStatement::default();
    let mut current: Option<SectionCurrency> = None;
    let mut cut_date: Option<NaiveDate> = None;
    // `BALANCE TOTAL`'s label and value sit on separate rows; armed by the label
    // kind, consumed by the next value kind.
    let mut balance_pending = false;

    for row in rows {
        match classify_row(row) {
            RowKind::SectionTitle(cur) => current = Some(cur),

            RowKind::Card {
                last4,
                cut,
                balance_anterior,
            } => {
                let cur = current.context("card row before any section title")?;
                cut_date = Some(cut);
                // The card header repeats on every page; collapse consecutive
                // repeats of the same currency into one logical section.
                let repeat = out.sections.last().is_some_and(|s| s.currency == cur);
                if !repeat {
                    out.sections.push(Section {
                        currency: cur,
                        primary_last4: last4,
                        cut_date: cut,
                        balance_anterior,
                        balance_total: None,
                    });
                }
            }

            RowKind::Txn { reference, row } => {
                let cur = current.context("transaction before any section")?;
                let cut = cut_date.context("transaction before any cut date")?;
                out.txns.push(parse_txn(row, reference, cur, cut)?);
            }

            RowKind::Continuation { mcc, auth } => {
                if let Some(last) = out.txns.last_mut() {
                    last.mcc.get_or_insert(mcc);
                    last.auth_code.get_or_insert(auth);
                }
            }

            RowKind::BalanceLabel => balance_pending = true,

            RowKind::BalanceValue(total) => {
                if balance_pending && let Some(section) = out.sections.last_mut() {
                    section.balance_total = Some(total);
                    balance_pending = false;
                }
            }

            RowKind::Ignored => {}
        }
    }

    if out.sections.is_empty() {
        return Err(anyhow!(
            "no statement sections found (not an estado de cuenta?)"
        ));
    }
    Ok(out)
}

/// Classify a row by trying the kinds in precedence order. Pure and total — the
/// ordering (title → card → txn → continuation → balance) lives here, where it
/// is visible and testable, rather than implied by control flow in the fold.
fn classify_row(row: &TextRow) -> RowKind<'_> {
    let joined = row.joined();
    if let Some(cur) = section_title(&joined) {
        return RowKind::SectionTitle(cur);
    }
    if let Some((last4, cut, balance_anterior)) = card_row(row) {
        return RowKind::Card {
            last4,
            cut,
            balance_anterior,
        };
    }
    if let Some(reference) = txn_reference(row) {
        return RowKind::Txn { reference, row };
    }
    if let Some((mcc, auth)) = continuation(row) {
        return RowKind::Continuation { mcc, auth };
    }
    if joined.to_uppercase().contains("BALANCE TOTAL") {
        return RowKind::BalanceLabel;
    }
    // A footer value row (`CUOTAS  MONTO  PAGO MÍN  A PAGAR  TOTAL`) — several
    // decimals; the last is BALANCE TOTAL. Checked after txn so a transaction
    // (whose digit-only reference also parses as a decimal) can never land here.
    if decimal_cell_count(row) >= FOOTER_MIN_DECIMAL_CELLS
        && let Some(total) = last_decimal(row)
    {
        return RowKind::BalanceValue(total);
    }
    RowKind::Ignored
}

/// `VISA PRESTIGE DOP|USD` → the section currency.
fn section_title(joined: &str) -> Option<SectionCurrency> {
    let u = joined.to_uppercase();
    if u.contains("VISA PRESTIGE DOP") {
        Some(SectionCurrency::Dop)
    } else if u.contains("VISA PRESTIGE USD") {
        Some(SectionCurrency::Usd)
    } else {
        None
    }
}

/// The card/header row: first cell `****-****-****-NNNN`, a `DD/MM/YYYY`
/// FECHA DE CORTE, and (last decimal cell) the BALANCE ANTERIOR.
fn card_row(row: &TextRow) -> Option<(Last4, NaiveDate, Option<Decimal>)> {
    let first = row.cells.first()?.text.trim();
    let rest = first.strip_prefix("****-****-****-")?;
    let last4 = Last4::parse(rest)?;
    let cut = first_full_date(row)?;
    Some((last4, cut, last_decimal(row)))
}

/// The first `DD/MM/YYYY` cell in a row (the card row's FECHA DE CORTE).
fn first_full_date(row: &TextRow) -> Option<NaiveDate> {
    row.cells
        .iter()
        .find_map(|c| NaiveDate::parse_from_str(c.text.trim(), "%d/%m/%Y").ok())
}

/// If the row is a transaction (two leading `DD/MM` cells then a valid
/// reference), return its parsed [`Reference`]. Returning the parsed value — not
/// a bool — means the reference is parsed exactly once, in classification.
fn txn_reference(row: &TextRow) -> Option<Reference> {
    if row.cells.len() >= 4 && is_ddmm(&row.cells[0].text) && is_ddmm(&row.cells[1].text) {
        Reference::parse(&row.cells[2].text)
    } else {
        None
    }
}

/// A continuation row carrying just `MCC(4)  AUTH(6)` for the prior transaction.
fn continuation(row: &TextRow) -> Option<(Mcc, AuthCode)> {
    if let [mcc, auth] = row.cells.as_slice() {
        return Some((Mcc::parse(&mcc.text)?, AuthCode::parse(&auth.text)?));
    }
    None
}

/// Parse a transaction row into a typed [`StatementTxn`]. The `reference` was
/// already parsed during classification ([`txn_reference`]); only the
/// cut-date-dependent fields (the two dates) and the defensive amount/merchant
/// extraction happen here.
fn parse_txn(
    row: &TextRow,
    reference: Reference,
    section: SectionCurrency,
    cut: NaiveDate,
) -> Result<StatementTxn> {
    let posting_date = infer_year(&row.cells[0].text, cut).context("posting date")?;
    let auth_date = infer_year(&row.cells[1].text, cut).context("auth date")?;

    // The amount is the *last cell that parses as a signed decimal*, not blindly
    // the last cell — a trailing artifact run must never be booked as the money
    // value. It must sit after the merchant columns (index ≥ 3); the reference
    // (index 2, all digits) also parses as a decimal, so `< 3` means no amount.
    let amount_idx = row
        .cells
        .iter()
        .rposition(|c| parse_decimal_cell(&c.text).is_some())
        .filter(|&i| i >= 3)
        .ok_or_else(|| anyhow!("transaction row has no amount cell after the merchant"))?;
    let raw_amount = row.cells[amount_idx].text.trim();
    let direction = if raw_amount.starts_with('-') {
        Direction::In
    } else {
        Direction::Out
    };
    let cleaned = strip_thousands_commas(raw_amount.trim_start_matches('-'));
    let amount = Amount::parse(&cleaned)
        .map_err(|e| anyhow!("statement amount {raw_amount:?} rejected: {e}"))?;

    // Merchant = the cells between the reference and the amount, whitespace
    // collapsed (cell joins can introduce runs that the original PDF did not).
    let merchant = collapse_whitespace(&join_cells(&row.cells[3..amount_idx]));
    if merchant.is_empty() {
        return Err(anyhow!("transaction row has no merchant description"));
    }

    Ok(StatementTxn {
        section,
        posting_date,
        auth_date,
        reference,
        merchant,
        money: Money::new(amount, section.currency()),
        direction,
        mcc: None,
        auth_code: None,
    })
}

/// Infer the full date from a year-less `DD/MM`, anchored on the cut date: a
/// month *after* the cut month belongs to the previous year (the Dec→Jan wrap);
/// otherwise the cut year.
fn infer_year(ddmm: &str, cut: NaiveDate) -> Result<NaiveDate> {
    let t = ddmm.trim();
    let (d, m) = t
        .split_once('/')
        .ok_or_else(|| anyhow!("date {t:?} is not DD/MM"))?;
    let day: u32 = d.parse().with_context(|| format!("day in {t:?}"))?;
    let month: u32 = m.parse().with_context(|| format!("month in {t:?}"))?;
    let year = if month <= cut.month() {
        cut.year()
    } else {
        cut.year() - 1
    };
    NaiveDate::from_ymd_opt(year, month, day)
        .ok_or_else(|| anyhow!("invalid date {day:02}/{month:02}/{year}"))
}

/// `DD/MM`: exactly five chars, digits around a single `/`.
fn is_ddmm(s: &str) -> bool {
    let b = s.trim().as_bytes();
    b.len() == 5
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2] == b'/'
        && b[3].is_ascii_digit()
        && b[4].is_ascii_digit()
}

/// Parse a statement cell as a (comma-grouped, optionally `-`-signed) decimal —
/// the single definition of "this cell is a number" for the parser. A lone `-`
/// or blank is not a number. This is the *balance/value* parser (signed); the
/// transaction amount additionally goes through the non-negative [`Amount`] gate
/// in [`parse_txn`].
fn parse_decimal_cell(s: &str) -> Option<Decimal> {
    use std::str::FromStr;
    let t = s.trim();
    let body = t.strip_prefix('-').unwrap_or(t);
    if body.is_empty() {
        return None;
    }
    Decimal::from_str(&strip_thousands_commas(t)).ok()
}

/// The last cell that parses as a decimal — used for the footer BALANCE TOTAL.
fn last_decimal(row: &TextRow) -> Option<Decimal> {
    row.cells
        .iter()
        .rev()
        .find_map(|c| parse_decimal_cell(&c.text))
}

/// How many of a row's cells parse as decimals — spots the footer's numeric
/// value row (`CUOTAS  MONTO  PAGO MÍN  A PAGAR  TOTAL`).
fn decimal_cell_count(row: &TextRow) -> usize {
    row.cells
        .iter()
        .filter(|c| parse_decimal_cell(&c.text).is_some())
        .count()
}

// -- statement-parse unit tests --
#[cfg(test)]
mod tests {
    use super::*;
    use crate::statement::Cell;
    use crate::test_support::dec;

    /// Build a row from (x, text) pairs at a given y.
    fn row(y: f32, cells: &[(f32, &str)]) -> TextRow {
        TextRow {
            y,
            cells: cells
                .iter()
                .map(|(x, t)| Cell {
                    x: *x,
                    text: t.to_string(),
                })
                .collect(),
        }
    }

    /// A realistic two-section statement skeleton with one charge + one payment.
    fn sample() -> Vec<TextRow> {
        vec![
            row(700.0, &[(25.0, "VISA PRESTIGE DOP")]),
            row(
                680.0,
                &[
                    (25.0, "****-****-****-7524"),
                    (195.0, "432,000.00"),
                    (273.0, "431,000.04"),
                    (375.0, "22/05/2026"), // FECHA DE CORTE
                    (458.0, "16/06/2026"), // FECHA LÍMITE
                    (540.0, "60,999.77"),  // BALANCE ANTERIOR
                ],
            ),
            // A payment (negative → credit/In).
            row(
                613.0,
                &[
                    (29.0, "28/04"),
                    (76.0, "28/04"),
                    (113.0, "0601324353"),
                    (252.0, "Pago Via App"),
                    (546.0, "-60,999.81"),
                ],
            ),
            // A charge with a continuation (MCC/auth) row.
            row(
                550.0,
                &[
                    (29.0, "25/04"),
                    (76.0, "24/04"),
                    (113.0, "24492166114100057344389"),
                    (252.0, "DONACION JOMPEAME"),
                    (340.0, "JOMPEAME.COM"),
                    (555.0, "1,000.00"),
                ],
            ),
            row(541.0, &[(113.0, "8398"), (150.0, "090531")]),
            // Footer: label row then a separate numeric value row (real layout).
            row(
                130.0,
                &[
                    (150.0, "MONTO VENCIDO"),
                    (300.0, "PAGO MÍNIMO"),
                    (430.0, "BALANCE A PAGAR"),
                    (520.0, "BALANCE TOTAL"),
                ],
            ),
            row(
                120.0,
                &[
                    (40.0, "0"),
                    (150.0, "0.00"),
                    (300.0, "50.00"),
                    (430.0, "500.00"),
                    (520.0, "999.77"),
                ],
            ),
            // Second section header (USD card).
            row(695.0, &[(25.0, "VISA PRESTIGE USD")]),
            row(
                675.0,
                &[
                    (30.0, "****-****-****-7524"),
                    (200.0, "8,300.00"),
                    (380.0, "22/05/2026"),
                    (463.0, "16/06/2026"),
                    (545.0, "2,491.46"),
                ],
            ),
            row(
                613.0,
                &[
                    (29.0, "14/05"),
                    (76.0, "17/04"),
                    (113.0, "74987506133002256024229"),
                    (252.0, "7-Eleven B315"),
                    (340.0, "Kastrup"),
                    (555.0, "7.28"),
                ],
            ),
        ]
    }

    #[test]
    fn parses_two_sections() {
        let s = parse_statement(&sample()).unwrap();
        assert_eq!(s.sections.len(), 2);
        assert_eq!(s.sections[0].currency, SectionCurrency::Dop);
        assert_eq!(s.sections[0].primary_last4.as_str(), "7524");
        assert_eq!(
            s.sections[0].cut_date,
            NaiveDate::from_ymd_opt(2026, 5, 22).unwrap()
        );
        assert_eq!(
            s.sections[0].balance_anterior,
            Some(dec("60999.77")),
            "BALANCE ANTERIOR = last decimal of the card row"
        );
        assert_eq!(s.sections[1].currency, SectionCurrency::Usd);
    }

    #[test]
    fn captures_balance_total() {
        let stmt = parse_statement(&sample()).unwrap();
        assert_eq!(stmt.sections[0].balance_total, Some(dec("999.77")));
    }

    #[test]
    fn parses_charge_payment_and_currencies() {
        let s = parse_statement(&sample()).unwrap();
        assert_eq!(s.txns.len(), 3);

        let payment = &s.txns[0];
        assert_eq!(
            payment.direction,
            Direction::In,
            "negative = credit/payment"
        );
        assert_eq!(payment.merchant, "Pago Via App");
        assert_eq!(payment.money.currency.as_str(), "DOP");
        assert_eq!(payment.money.amount.value(), dec("60999.81"));

        let charge = &s.txns[1];
        assert_eq!(charge.direction, Direction::Out);
        assert_eq!(charge.merchant, "DONACION JOMPEAME JOMPEAME.COM");
        assert_eq!(charge.reference.as_str(), "24492166114100057344389");
        assert_eq!(charge.money.amount.value(), dec("1000.00"));

        let usd = &s.txns[2];
        assert_eq!(usd.section, SectionCurrency::Usd);
        assert_eq!(usd.money.currency.as_str(), "USD");
        assert_eq!(usd.merchant, "7-Eleven B315 Kastrup");
    }

    #[test]
    fn attaches_mcc_and_auth_continuation() {
        let s = parse_statement(&sample()).unwrap();
        let charge = &s.txns[1];
        assert_eq!(charge.mcc.as_ref().map(Mcc::as_str), Some("8398"));
        assert_eq!(
            charge.auth_code.as_ref().map(AuthCode::as_str),
            Some("090531")
        );
        // The payment had no continuation row.
        assert_eq!(s.txns[0].mcc, None);
    }

    #[test]
    fn amount_is_last_decimal_not_blindly_last_cell() {
        // A trailing non-amount artifact cell after the real amount must not be
        // booked as the amount; the real amount (a signed decimal) wins.
        let r = row(
            600.0,
            &[
                (30.0, "25/04"),
                (78.0, "24/04"),
                (115.0, "0601324353"),
                (255.0, "SOME MERCHANT"),
                (542.0, "42.15"),
                (602.0, "*"), // trailing artifact
            ],
        );
        let cut = NaiveDate::from_ymd_opt(2026, 5, 22).unwrap();
        let reference = Reference::parse("0601324353").unwrap();
        let txn = parse_txn(&r, reference, SectionCurrency::Usd, cut).unwrap();
        assert_eq!(txn.money.amount.value(), dec("42.15"));
        assert_eq!(txn.merchant, "SOME MERCHANT");
    }

    #[test]
    fn year_inference_same_and_prior_year() {
        let cut = NaiveDate::from_ymd_opt(2026, 5, 22).unwrap();
        // Month <= cut month (May) → cut year.
        assert_eq!(
            infer_year("28/04", cut).unwrap(),
            NaiveDate::from_ymd_opt(2026, 4, 28).unwrap()
        );
        // Same-month date stays in cut year.
        let may22 = NaiveDate::from_ymd_opt(2026, 5, 22).unwrap();
        assert_eq!(infer_year("22/05", cut).unwrap(), may22);
        // Month after cut month → previous year (Dec→Jan wrap; 12 > 05).
        let dec31 = NaiveDate::from_ymd_opt(2025, 12, 31).unwrap();
        assert_eq!(infer_year("31/12", cut).unwrap(), dec31);
    }

    #[test]
    fn year_inference_rejects_impossible_date() {
        let cut = NaiveDate::from_ymd_opt(2026, 5, 22).unwrap();
        assert!(infer_year("31/02", cut).is_err());
        assert!(infer_year("notadate", cut).is_err());
    }

    #[test]
    fn no_sections_is_an_error() {
        let rows = vec![row(500.0, &[(20.0, "Gracias por su preferencia")])];
        assert!(parse_statement(&rows).is_err());
    }

    #[test]
    fn row_classification() {
        assert!(matches!(
            classify_row(&row(700.0, &[(25.0, "VISA PRESTIGE USD")])),
            RowKind::SectionTitle(SectionCurrency::Usd)
        ));
        assert!(matches!(
            classify_row(&row(
                680.0,
                &[(25.0, "****-****-****-7524"), (375.0, "22/05/2026")]
            )),
            RowKind::Card { .. }
        ));
        assert!(matches!(
            classify_row(&row(541.0, &[(113.0, "8398"), (150.0, "090531")])),
            RowKind::Continuation { .. }
        ));
        // A single-cell row with no DD/MM pattern is noise, not a data row.
        let noise = row(500.0, &[(20.0, "marketing copy here")]);
        assert!(matches!(classify_row(&noise), RowKind::Ignored));
    }

    // Verify the low-level DD/MM and decimal-cell recognition helpers.
    #[test]
    fn row_predicates() {
        assert!(is_ddmm("28/04"));
        assert!(!is_ddmm("2/4"));
        assert!(!is_ddmm("28/04/26"));
        assert!(parse_decimal_cell("-60,999.81").is_some());
        assert!(parse_decimal_cell("7.28").is_some());
        assert!(parse_decimal_cell("Kastrup").is_none());
        assert!(parse_decimal_cell("").is_none());
        assert!(
            parse_decimal_cell("-").is_none(),
            "a lone minus is not a number"
        );
    }
}
