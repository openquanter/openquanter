//! Funding, as the venue charged it and as the model would have been
//! charged.
//!
//! # Two numbers, each to the last place
//!
//! Attribution splits live minus model into causes, and one is funding:
//! what the account was charged against what the model, holding its own
//! positions, would have been. Both must be measurements. A funding
//! component nobody read is *unavailable*, and reporting zero instead
//! would claim the two sides agreed.
//!
//! - **The venue's side is its ledger.** Each settlement's funding lines
//!   are read back and booked as they stand ([`oq_core::Event::FundingCharged`]),
//!   never recomputed.
//! - **The model's side is computed,** because nobody charged the model.
//!   It is computed by [`charge`], which reproduces the venue's own
//!   arithmetic: quantity times the settlement's mark times its rate,
//!   truncated toward zero at eight places. The mark is the venue's text,
//!   not a price on the contract's grid — the venue settles at a mark
//!   with more places than any order can carry.
//!
//! # Why the model's number can be trusted
//!
//! Because the same function is checked against the venue at every
//! settlement. The positions the live books held at the settlement go
//! through [`charge`] with the venue's rate and mark, and the result must
//! equal the venue's ledger lines exactly. Checked against a testnet
//! account's ledger before this was written — nine lines across five
//! settlements, hedged, both signs of rate, four of which truncation and
//! rounding would have told apart — it reproduces every one.
//!
//! When a check fails — a fill landed in the instant between the
//! settlement and the snapshot, or something traded the account that
//! this process did not — the model's funding for the whole run becomes
//! unavailable, with the reason. The venue's lines are still booked: they
//! are what the account was actually charged.
//!
//! # When
//!
//! The venue publishes its next settlement time; when the loop passes it,
//! both books' legs are recorded. The rate, the mark and the ledger lines
//! arrive a little after, and are asked for on a timer until they do.
//! Until every settlement the run crossed is resolved, funding is
//! unavailable; a run that has crossed none has measured exactly zero.

use oq_gateway::account::{FundingCharge, SettledRate};
use oq_types::{Cash, Instrument, QtyLots};

/// How long to keep asking about a settlement before saying it will not
/// be answered.
const GIVE_UP_MS: i64 = 30 * 60_000;
/// How far apart a ledger line's time and its settlement's may be.
const SAME_SETTLEMENT_MS: i64 = 60_000;

/// A decimal as an integer and its number of places.
fn decimal(text: &str) -> Option<(i128, u32)> {
    let t = text.trim();
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    if frac.bytes().any(|b| !b.is_ascii_digit()) {
        return None;
    }
    let negative = whole.starts_with('-');
    let digits: i128 = format!("{}{frac}", whole.trim_start_matches(['-', '+']))
        .parse()
        .ok()?;
    Some((
        if negative { -digits } else { digits },
        u32::try_from(frac.len()).ok()?,
    ))
}

/// What one leg is charged at one settlement, the venue's way: received
/// positive, paid negative, truncated toward zero at eight places.
///
/// `lots` is signed, long positive. Linear contracts only — a quantity of
/// the underlying times a price in the quote — and `None` for any other,
/// or for an input that does not read or does not fit.
#[must_use]
pub fn charge(instrument: &Instrument, lots: QtyLots, rate: &str, mark: &str) -> Option<Cash> {
    if instrument.contract_size != oq_types::CONTRACT_SCALE {
        return None;
    }
    let (rate_m, rate_s) = decimal(rate)?;
    let (mark_m, mark_s) = decimal(mark)?;
    // lots × 10^-qty_scale × mark × rate, in units of 10^-8: a long pays
    // a positive rate.
    let numerator = i128::from(lots.0)
        .checked_mul(mark_m)?
        .checked_mul(rate_m)?
        .checked_mul(100_000_000)?
        .checked_neg()?;
    let places = u32::from(instrument.qty_scale) + mark_s + rate_s;
    let denominator = 10_i128.checked_pow(places)?;
    // Integer division truncates toward zero, which is what the venue does.
    i64::try_from(numerator / denominator).ok().map(Cash)
}

/// Both legs, long then short, as held at a settlement.
pub type Legs = (QtyLots, QtyLots);

fn charge_legs(instrument: &Instrument, legs: Legs, rate: &str, mark: &str) -> Option<Cash> {
    let mut total = Cash::ZERO;
    for lots in [legs.0, legs.1] {
        if lots.0 != 0 {
            total = total.add(charge(instrument, lots, rate, mark)?);
        }
    }
    Some(total)
}

/// A settlement the run crossed, waiting for the venue's figures.
#[derive(Debug, Clone, Copy)]
struct Pending {
    at_ms: i64,
    live: Legs,
    model: Legs,
}

/// One settlement, resolved: what to book on each side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    pub at_ms: i64,
    pub rate: String,
    pub mark: String,
    /// The venue's ledger lines, summed.
    pub venue: Cash,
    /// The model's positions through [`charge`].
    pub model: Cash,
    /// Whether [`charge`] on the live positions matched the venue.
    pub verified: bool,
}

/// Settlements crossed, and what has been learnt about them.
#[derive(Debug, Clone)]
pub struct FundingWatch {
    instrument: Instrument,
    next_ms: Option<i64>,
    pending: Vec<Pending>,
    venue_total: Cash,
    model_total: Cash,
    /// Why funding cannot be reported for this run, once there is one.
    problem: Option<String>,
    settled: usize,
}

impl FundingWatch {
    /// Start watching, from the venue's answer to "when is the next
    /// settlement". An adapter that does not know leaves funding
    /// unavailable for the run, with that as the reason.
    #[must_use]
    pub fn new(instrument: Instrument, next: Result<Option<i64>, String>) -> Self {
        let (next_ms, problem) = match next {
            Ok(Some(t)) => (Some(t), None),
            Ok(None) => (
                None,
                Some("this venue's adapter does not report funding settlements".to_string()),
            ),
            Err(e) => (
                None,
                Some(format!(
                    "the next funding settlement could not be read: {e}"
                )),
            ),
        };
        let problem = problem.or_else(|| {
            (instrument.contract_size != oq_types::CONTRACT_SCALE)
                .then(|| "funding is computed for linear contracts only".to_string())
        });
        Self {
            instrument,
            next_ms,
            pending: Vec::new(),
            venue_total: Cash::ZERO,
            model_total: Cash::ZERO,
            problem,
            settled: 0,
        }
    }

    /// The loop's clock moved: if it passed a settlement, record what
    /// both books held, and when the next one is.
    pub fn on_time(
        &mut self,
        now_ms: i64,
        live: Legs,
        model: Legs,
        next: impl FnOnce() -> Option<i64>,
    ) {
        let Some(t) = self.next_ms else { return };
        if now_ms < t {
            return;
        }
        self.pending.push(Pending {
            at_ms: t,
            live,
            model,
        });
        // Asked again rather than added to: intervals differ by contract
        // and the venue changes them.
        self.next_ms = next().filter(|n| *n > t);
        if self.next_ms.is_none() && self.problem.is_none() {
            self.problem = Some(format!("the settlement after {t} could not be read"));
        }
    }

    /// Whether there is anything to ask the venue.
    #[must_use]
    pub fn waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The earliest settlement still waiting, to ask the venue from.
    #[must_use]
    pub fn asking_since(&self) -> Option<i64> {
        self.pending
            .iter()
            .map(|p| p.at_ms)
            .min()
            .map(|t| t - SAME_SETTLEMENT_MS)
    }

    /// Resolve what the venue's figures now answer. Each resolved
    /// settlement is returned for the caller to book and record.
    pub fn resolve(
        &mut self,
        rates: &[SettledRate],
        lines: &[FundingCharge],
        now_ms: i64,
    ) -> Vec<Settled> {
        let mut out = Vec::new();
        let mut still = Vec::new();
        for p in std::mem::take(&mut self.pending) {
            let rate = rates
                .iter()
                .find(|r| (r.time_ms - p.at_ms).abs() <= SAME_SETTLEMENT_MS);
            let booked: Vec<&FundingCharge> = lines
                .iter()
                .filter(|l| (l.time_ms - p.at_ms).abs() <= SAME_SETTLEMENT_MS)
                .collect();
            let Some(rate) = rate else {
                self.keep_or_give_up(p, now_ms, "its rate", &mut still);
                continue;
            };
            let (Some(expected), Some(model)) = (
                charge_legs(&self.instrument, p.live, &rate.rate, &rate.mark),
                charge_legs(&self.instrument, p.model, &rate.rate, &rate.mark),
            ) else {
                self.problem = Some(format!("the settlement at {} did not compute", p.at_ms));
                continue;
            };
            // A position that was charged has a ledger line; until it
            // appears, the settlement is not over.
            if booked.is_empty() && expected != Cash::ZERO {
                self.keep_or_give_up(p, now_ms, "its ledger lines", &mut still);
                continue;
            }
            let venue = booked.iter().fold(Cash::ZERO, |a, l| a.add(l.amount));
            let verified = venue == expected;
            if !verified && self.problem.is_none() {
                self.problem = Some(format!(
                    "the settlement at {} was charged {} where the live positions compute {}: \
                     the model's funding cannot be trusted for this run",
                    p.at_ms, venue.0, expected.0
                ));
            }
            self.venue_total = self.venue_total.add(venue);
            self.model_total = self.model_total.add(model);
            self.settled += 1;
            out.push(Settled {
                at_ms: p.at_ms,
                rate: rate.rate.clone(),
                mark: rate.mark.clone(),
                venue,
                model,
                verified,
            });
        }
        self.pending = still;
        out
    }

    fn keep_or_give_up(&mut self, p: Pending, now_ms: i64, what: &str, still: &mut Vec<Pending>) {
        if now_ms - p.at_ms > GIVE_UP_MS {
            self.problem = Some(format!(
                "the settlement at {} has no {what} after 30 minutes",
                p.at_ms
            ));
        } else {
            still.push(p);
        }
    }

    /// Funding as attribution takes it: `(venue, model)`, or why not —
    /// the schedule unknown, a settlement not yet answered, or a check
    /// that failed. A run that crossed no settlement measured zero.
    ///
    /// # Errors
    /// The reason funding cannot be reported right now.
    pub fn evidence(&self) -> Result<(Cash, Cash), String> {
        if let Some(why) = &self.problem {
            return Err(why.clone());
        }
        if let Some(p) = self.pending.first() {
            return Err(format!(
                "the settlement at {} is waiting for the venue to publish its rate and ledger",
                p.at_ms
            ));
        }
        Ok((self.venue_total, self.model_total))
    }

    /// Why [`FundingWatch::evidence`] is an error for the rest of the run,
    /// once something makes it so.
    #[must_use]
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// Settlements resolved so far.
    #[must_use]
    pub const fn settled(&self) -> usize {
        self.settled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn btc() -> Instrument {
        Instrument::linear(1, 3)
    }

    /// A testnet account's own ledger, read on 2026-09-25: rate, mark,
    /// the leg's signed quantity, and the line the venue booked for it.
    /// Four of these round differently than they truncate.
    const LEDGER: [(&str, &str, i64, i64); 9] = [
        ("0.00008995", "84374.02268116", 4, -3_035_777),
        ("0.00008995", "84374.02268116", -8, 6_071_554),
        ("0.00010000", "83994.60000000", 4, -3_359_784),
        ("0.00010000", "83994.60000000", -8, 6_719_568),
        ("-0.00001633", "84397.47284420", 2, 275_642),
        ("-0.00001633", "84397.47284420", -8, -1_102_568),
        ("0.00010000", "84475.60000000", -8, 6_758_048),
        ("0.00005369", "84378.74802536", 6, -2_718_176),
        ("0.00005369", "84378.74802536", -4, 1_812_117),
    ];

    #[test]
    fn every_line_of_a_real_ledger_is_reproduced_to_the_last_place() {
        for (rate, mark, lots, booked) in LEDGER {
            assert_eq!(
                charge(&btc(), QtyLots(lots), rate, mark),
                Some(Cash(booked)),
                "{rate} {mark} {lots}"
            );
        }
    }

    #[test]
    fn a_contract_that_is_not_linear_is_not_guessed_at() {
        let inverse = Instrument {
            contract_size: 100 * oq_types::CONTRACT_SCALE,
            ..btc()
        };
        assert_eq!(charge(&inverse, QtyLots(1), "0.0001", "1"), None);
        assert!(FundingWatch::new(inverse, Ok(Some(1))).evidence().is_err());
    }

    fn rate(t: i64) -> SettledRate {
        SettledRate {
            time_ms: t,
            rate: "0.00010000".into(),
            mark: "83994.60000000".into(),
        }
    }

    fn line(t: i64, amount: i64, id: i64) -> FundingCharge {
        FundingCharge {
            time_ms: t,
            amount: Cash(amount),
            id,
        }
    }

    #[test]
    fn a_run_that_crossed_no_settlement_measured_zero_and_one_unknown_is_unavailable() {
        assert_eq!(
            FundingWatch::new(btc(), Ok(Some(1_000))).evidence(),
            Ok((Cash::ZERO, Cash::ZERO))
        );
        assert!(FundingWatch::new(btc(), Ok(None)).evidence().is_err());
        assert!(
            FundingWatch::new(btc(), Err("down".into()))
                .evidence()
                .is_err()
        );
    }

    #[test]
    fn a_settlement_is_booked_from_the_ledger_and_checked_against_the_live_legs() {
        let t = 1_790_323_200_000;
        let mut w = FundingWatch::new(btc(), Ok(Some(t)));
        let live = (QtyLots(4), QtyLots(-8));
        let model = (QtyLots(4), QtyLots(0));
        w.on_time(t + 150, live, model, || Some(t + 28_800_000));
        assert!(
            w.evidence().is_err_and(|e| e.contains("waiting")),
            "waiting is not zero"
        );
        // Not published yet: nothing resolves, and it keeps waiting.
        assert!(w.resolve(&[], &[], t + 60_000).is_empty());
        assert!(w.waiting());
        let s = w.resolve(
            &[rate(t)],
            &[line(t, -3_359_784, 1), line(t, 6_719_568, 2)],
            t + 120_000,
        );
        assert_eq!(s.len(), 1);
        assert!(s[0].verified);
        assert_eq!(s[0].venue, Cash(3_359_784));
        assert_eq!(s[0].model, Cash(-3_359_784), "the model held only the long");
        assert_eq!(w.evidence(), Ok((Cash(3_359_784), Cash(-3_359_784))));
    }

    #[test]
    fn a_ledger_that_disagrees_with_the_live_legs_makes_the_model_untrusted() {
        let t = 1_790_323_200_000;
        let mut w = FundingWatch::new(btc(), Ok(Some(t)));
        w.on_time(
            t,
            (QtyLots(4), QtyLots(-8)),
            (QtyLots(4), QtyLots(-8)),
            || Some(t + 28_800_000),
        );
        let s = w.resolve(
            &[rate(t)],
            &[line(t, -3_359_784, 1), line(t, 6_719_569, 2)],
            t + 1,
        );
        assert!(!s[0].verified);
        assert_eq!(
            s[0].venue,
            Cash(3_359_785),
            "the venue's lines are still what was charged"
        );
        assert!(w.evidence().is_err_and(|e| e.contains("cannot be trusted")));
    }

    #[test]
    fn a_settlement_never_answered_is_given_up_on_and_says_so() {
        let t = 1_790_323_200_000;
        let mut w = FundingWatch::new(btc(), Ok(Some(t)));
        w.on_time(
            t,
            (QtyLots(4), QtyLots(0)),
            (QtyLots(0), QtyLots(0)),
            || Some(t + 28_800_000),
        );
        assert!(w.resolve(&[rate(t)], &[], t + GIVE_UP_MS + 1).is_empty());
        assert!(!w.waiting());
        assert!(w.problem().is_some_and(|p| p.contains("ledger lines")));
    }

    #[test]
    fn a_flat_account_has_no_line_and_needs_none() {
        let t = 1_790_323_200_000;
        let mut w = FundingWatch::new(btc(), Ok(Some(t)));
        w.on_time(
            t,
            (QtyLots(0), QtyLots(0)),
            (QtyLots(4), QtyLots(0)),
            || Some(t + 28_800_000),
        );
        let s = w.resolve(&[rate(t)], &[], t + 1);
        assert!(s[0].verified);
        assert_eq!((s[0].venue, s[0].model), (Cash::ZERO, Cash(-3_359_784)));
    }
}
