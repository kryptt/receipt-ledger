//! USD-equivalent maximum-amount ceiling.
//!
//! `RECEIPT_MAX_AMOUNT` is a **US-dollar** threshold: the operator wants
//! ">$100,000 USD → Review". A charge arrives in its own currency, so the raw
//! figure cannot answer the question on its own — ₩100,000 (≈ $72) must *not*
//! trip a $100,000 ceiling, while $100,001 must.
//!
//! The conversion to USD needs a live FX rate, so the *check* runs in the async
//! pipeline ([`crate::process_message`]) after the pure [`crate::validate`]
//! gate mints a [`crate::validate::Validated`]. The *decision* itself — given an
//! already-resolved rate — is pure, lives here, and is unit tested. That keeps
//! the network out of the arithmetic and the arithmetic under `./test.sh`.

use rust_decimal::Decimal;

/// The verdict of the USD-equivalent ceiling check on a single charge.
///
/// Exhaustive: a caller must handle both arms, so a future third disposition
/// cannot be silently dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CeilingVerdict {
    /// The charge is within the ceiling (or no ceiling is configured). Carries
    /// the computed USD-equivalent so the caller can log it.
    Within { usd_equivalent: Decimal },
    /// The charge's USD-equivalent strictly exceeds the configured ceiling.
    /// Carries both figures for a clear Review reason.
    Over {
        usd_equivalent: Decimal,
        ceiling: Decimal,
    },
}

/// Decide whether `amount` (denominated in the charge currency) is within the
/// USD ceiling, given the already-resolved `rate` to convert the charge
/// currency into USD (multiply the amount by it) and the configured `ceiling`.
///
/// Pure: all I/O (resolving `rate`) is the caller's job. A `None` ceiling means
/// no upper bound, so every charge is [`CeilingVerdict::Within`]. The
/// comparison is *strictly greater than* — a charge exactly at the ceiling
/// books, matching the ">$100,000 → Review" wording.
#[must_use]
pub fn check(amount: Decimal, rate: Decimal, ceiling: Option<Decimal>) -> CeilingVerdict {
    let usd_equivalent = amount * rate;
    match ceiling {
        Some(max) if usd_equivalent > max => CeilingVerdict::Over {
            usd_equivalent,
            ceiling: max,
        },
        _ => CeilingVerdict::Within { usd_equivalent },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    /// No ceiling configured → always Within, regardless of magnitude.
    #[test]
    fn no_ceiling_is_always_within() {
        // A USD charge of ten million, no ceiling → Within.
        let v = check(dec("10000000"), Decimal::ONE, None);
        assert!(matches!(v, CeilingVerdict::Within { .. }));
    }

    /// A USD charge ($1 → USD rate is 1) just over the ceiling routes to Over.
    #[test]
    fn usd_charge_just_over_ceiling_is_over() {
        let v = check(dec("100001"), Decimal::ONE, Some(dec("100000")));
        match v {
            CeilingVerdict::Over {
                usd_equivalent,
                ceiling,
            } => {
                assert_eq!(usd_equivalent, dec("100001"));
                assert_eq!(ceiling, dec("100000"));
            }
            CeilingVerdict::Within { .. } => panic!("expected Over"),
        }
    }

    /// A charge exactly at the ceiling books (strictly-greater comparison).
    #[test]
    fn exactly_at_ceiling_is_within() {
        let v = check(dec("100000"), Decimal::ONE, Some(dec("100000")));
        assert!(matches!(v, CeilingVerdict::Within { .. }));
    }

    /// The motivating case: a EUR charge whose EUR figure is over the ceiling
    /// but whose USD-equivalent is under. EUR 95,000 at 1.08 USD/EUR = $102,600
    /// → Over; at 1.00 = $95,000 → Within. Proves the gate is on USD, not the
    /// raw figure.
    #[test]
    fn eur_charge_judged_in_usd_not_raw() {
        // EUR 95,000 @ 1.08 → $102,600 > $100,000 → Over.
        let over = check(dec("95000"), dec("1.08"), Some(dec("100000")));
        match over {
            CeilingVerdict::Over { usd_equivalent, .. } => {
                assert_eq!(usd_equivalent, dec("102600.00"));
            }
            CeilingVerdict::Within { .. } => panic!("EUR 95k @1.08 should be Over"),
        }
        // EUR 95,000 @ 0.99 → $94,050 < $100,000 → Within.
        let within = check(dec("95000"), dec("0.99"), Some(dec("100000")));
        assert!(matches!(within, CeilingVerdict::Within { .. }));
    }

    /// The false-positive the raw check used to produce: ₩100,000 (raw figure ==
    /// ceiling boundary in the *old* raw interpretation) is really ≈ $72 and
    /// must be Within. KRW→USD ≈ 0.00072.
    #[test]
    fn krw_100k_is_about_72_usd_and_within() {
        // ₩100,000 @ 0.00072 USD/KRW = $72 → comfortably Within a $100,000 ceiling.
        let v = check(dec("100000"), dec("0.00072"), Some(dec("100000")));
        match v {
            CeilingVerdict::Within { usd_equivalent } => {
                assert_eq!(usd_equivalent, dec("72.00000"));
            }
            CeilingVerdict::Over { .. } => {
                panic!("₩100,000 ≈ $72 must NOT trip a $100,000 ceiling")
            }
        }
    }

    /// A JPY charge just over the ceiling in USD terms routes to Over.
    /// ¥16,000,000 @ 0.0064 = $102,400 > $100,000.
    #[test]
    fn jpy_charge_over_in_usd_is_over() {
        let v = check(dec("16000000"), dec("0.0064"), Some(dec("100000")));
        assert!(matches!(v, CeilingVerdict::Over { .. }));
    }

    /// And a JPY charge just under the ceiling in USD terms books.
    /// ¥15,000,000 @ 0.0064 = $96,000 < $100,000.
    #[test]
    fn jpy_charge_under_in_usd_is_within() {
        let v = check(dec("15000000"), dec("0.0064"), Some(dec("100000")));
        assert!(matches!(v, CeilingVerdict::Within { .. }));
    }

    // --- threaded through the FX client seam ------------------------------
    //
    // The pipeline resolves the rate via `FxClient::rate` then calls `check`.
    // These exercise that exact composition with a *seeded* FX rate (no
    // network), once just over and once just under the ceiling, for EUR and JPY
    // — the scenario the task calls out explicitly.

    use crate::fx::FxClient;
    use chrono::NaiveDate;
    use reqwest::Client;

    /// Resolve a seeded `from→USD` rate and run the ceiling decision exactly as
    /// the pipeline does. No network: the rate comes from the seeded cache.
    fn check_via_seeded_fx(
        from: &str,
        amount: &str,
        seeded_rate: &str,
        ceiling: &str,
    ) -> CeilingVerdict {
        let http = Client::new();
        let date = NaiveDate::from_ymd_opt(2026, 5, 27).unwrap();
        let fx = FxClient::with_seeded_rate(&http, from, "USD", date, dec(seeded_rate));
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let rate = rt
            .block_on(fx.rate(from, "USD", date))
            .expect("seeded rate resolves without network");
        check(dec(amount), rate, Some(dec(ceiling)))
    }

    /// EUR charge JUST OVER the $100,000 ceiling: EUR 93,000 @ 1.10 = $102,300.
    #[test]
    fn eur_just_over_ceiling_via_seeded_fx_is_over() {
        let v = check_via_seeded_fx("EUR", "93000", "1.10", "100000");
        match v {
            CeilingVerdict::Over { usd_equivalent, .. } => {
                assert_eq!(usd_equivalent, dec("102300.00"));
            }
            CeilingVerdict::Within { .. } => panic!("EUR 93k @1.10 → $102,300 should be Over"),
        }
    }

    /// EUR charge JUST UNDER the ceiling: EUR 89,000 @ 1.10 = $97,900.
    #[test]
    fn eur_just_under_ceiling_via_seeded_fx_is_within() {
        let v = check_via_seeded_fx("EUR", "89000", "1.10", "100000");
        assert!(matches!(v, CeilingVerdict::Within { .. }));
    }

    /// JPY charge JUST OVER the ceiling: ¥16,000,000 @ 0.0064 = $102,400.
    #[test]
    fn jpy_just_over_ceiling_via_seeded_fx_is_over() {
        let v = check_via_seeded_fx("JPY", "16000000", "0.0064", "100000");
        assert!(matches!(v, CeilingVerdict::Over { .. }));
    }

    /// JPY charge JUST UNDER the ceiling: ¥15,000,000 @ 0.0064 = $96,000.
    #[test]
    fn jpy_just_under_ceiling_via_seeded_fx_is_within() {
        let v = check_via_seeded_fx("JPY", "15000000", "0.0064", "100000");
        assert!(matches!(v, CeilingVerdict::Within { .. }));
    }
}
