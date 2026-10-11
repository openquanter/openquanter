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

/// An unanswered settlement becomes overdue, not economically terminal.
const OVERDUE_MS: i64 = 30 * 60_000;
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
    overdue: bool,
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
    /// Original venue ledger identities, retained for independent replay.
    pub lines: Vec<FundingCharge>,
}

/// An interval where no next settlement was known. Rediscovery closes
/// its receive boundary, not its historical coverage obligation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleGap {
    pub since_ms: i64,
    pub until_ms: Option<i64>,
}

/// Confirmed cash during an unknown schedule interval. There is no model
/// amount: historical model inventory was not observed at settlement.
#[derive(Debug, Clone)]
pub struct RecoveredFunding {
    pub since_ms: i64,
    pub through_ms: i64,
    pub lines: Vec<FundingCharge>,
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
    booked_lines: std::collections::BTreeMap<i64, (i64, Cash)>,
    schedule_gaps: Vec<ScheduleGap>,
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
            booked_lines: std::collections::BTreeMap::new(),
            schedule_gaps: Vec::new(),
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
        let Some(t) = self.next_ms else {
            self.begin_schedule_gap(now_ms);
            return;
        };
        if now_ms < t {
            return;
        }
        self.pending.push(Pending {
            at_ms: t,
            live,
            model,
            overdue: false,
        });
        // Asked again rather than added to: intervals differ by contract
        // and the venue changes them.
        self.next_ms = next().filter(|n| *n > t);
        if self.next_ms.is_none() {
            self.begin_schedule_gap(t.saturating_add(1));
        }
        if self.next_ms.is_none() && self.problem.is_none() {
            self.problem = Some(format!("the settlement after {t} could not be read"));
        }
    }

    fn begin_schedule_gap(&mut self, since_ms: i64) {
        if self
            .schedule_gaps
            .last()
            .is_none_or(|gap| gap.until_ms.is_some())
        {
            self.schedule_gaps.push(ScheduleGap {
                since_ms,
                until_ms: None,
            });
        }
    }

    /// Host polling, rather than every event-loop iteration, retries I/O.
    #[must_use]
    pub const fn needs_schedule(&self) -> bool {
        self.next_ms.is_none()
    }

    /// Apply the result of a host's schedule request at an injected time.
    /// Never extrapolate an interval or install a stale next settlement.
    pub fn rediscover_schedule(&mut self, now_ms: i64, next: Result<Option<i64>, String>) {
        if !self.needs_schedule() {
            return;
        }
        self.begin_schedule_gap(now_ms);
        if let Some(next_ms) = next.ok().flatten().filter(|next_ms| *next_ms > now_ms) {
            self.next_ms = Some(next_ms);
            if let Some(gap) = self.schedule_gaps.last_mut() {
                gap.until_ms = Some(now_ms);
            }
        }
    }

    /// Unknown schedule intervals remain obligations even after retry
    /// succeeds; current discovery cannot certify what happened earlier.
    #[must_use]
    pub fn schedule_gaps(&self) -> &[ScheduleGap] {
        &self.schedule_gaps
    }

    /// Whether there is anything to ask the venue.
    #[must_use]
    pub fn waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The earliest settlement still waiting, to ask the venue from.
    #[must_use]
    pub fn asking_since(&self) -> Option<i64> {
        let pending = self
            .pending
            .iter()
            .map(|p| p.at_ms)
            .min()
            .map(|t| t.saturating_sub(SAME_SETTLEMENT_MS));
        pending
            .into_iter()
            .chain(self.schedule_gaps.iter().map(|gap| gap.since_ms))
            .min()
    }

    #[must_use]
    pub fn needs_poll(&self) -> bool {
        self.waiting() || self.needs_schedule() || !self.schedule_gaps.is_empty()
    }

    /// Book only previously unseen venue rows within recorded unknown
    /// intervals. Known pending settlements retain their model snapshot
    /// and must go through `resolve`, even if their rate is still missing.
    pub fn recover_unknown(
        &mut self,
        lines: &[FundingCharge],
        now_ms: i64,
    ) -> Option<RecoveredFunding> {
        let since_ms = self.schedule_gaps.iter().map(|gap| gap.since_ms).min()?;
        let mut unique = std::collections::BTreeMap::new();
        for line in lines {
            if self
                .booked_lines
                .get(&line.id)
                .is_some_and(|prior| *prior != (line.time_ms, line.amount))
                || unique
                    .insert(line.id, line)
                    .is_some_and(|prior: &FundingCharge| prior != line)
            {
                self.problem = Some(format!(
                    "funding recovery identity {} has conflicting evidence",
                    line.id
                ));
                return None;
            }
        }
        let mut recovered = Vec::new();
        for line in unique.into_values() {
            if self.booked_lines.contains_key(&line.id) || line.time_ms > now_ms {
                continue;
            }
            if !self.schedule_gaps.iter().any(|gap| {
                line.time_ms >= gap.since_ms && line.time_ms <= gap.until_ms.unwrap_or(now_ms)
            }) {
                continue;
            }
            if self.pending.iter().any(|p| {
                i128::from(line.time_ms).abs_diff(i128::from(p.at_ms)) <= SAME_SETTLEMENT_MS as u128
            }) {
                continue;
            }
            recovered.push(*line);
        }
        if recovered.is_empty() {
            return None;
        }
        recovered.sort_by_key(|line| (line.time_ms, line.id));
        for line in &recovered {
            self.booked_lines
                .insert(line.id, (line.time_ms, line.amount));
        }
        Some(RecoveredFunding {
            since_ms,
            through_ms: now_ms,
            lines: recovered,
        })
    }

    /// Resolve what the venue's figures now answer. Each resolved
    /// settlement is returned for the caller to book and record.
    pub fn resolve(
        &mut self,
        rates: &[SettledRate],
        lines: &[FundingCharge],
        now_ms: i64,
    ) -> Vec<Settled> {
        // History requests can overlap pages or race stream delivery. A
        // stable ledger identity appears once; conflicting evidence must
        // never be quietly replaced by whichever row arrived last.
        let mut unique = std::collections::BTreeMap::new();
        for line in lines {
            if self
                .booked_lines
                .get(&line.id)
                .is_some_and(|prior| *prior != (line.time_ms, line.amount))
            {
                self.problem = Some(format!(
                    "funding ledger identity {} changed after booking",
                    line.id
                ));
                return Vec::new();
            }
            if unique
                .insert(line.id, line)
                .is_some_and(|prior| prior.time_ms != line.time_ms || prior.amount != line.amount)
            {
                self.problem = Some(format!(
                    "funding ledger identity {} has conflicting evidence",
                    line.id
                ));
                return Vec::new();
            }
        }
        let mut out = Vec::new();
        let mut still = Vec::new();
        for p in std::mem::take(&mut self.pending) {
            let rate = rates
                .iter()
                .find(|r| (r.time_ms - p.at_ms).abs() <= SAME_SETTLEMENT_MS);
            let booked: Vec<&FundingCharge> = unique
                .values()
                .copied()
                .filter(|l| (l.time_ms - p.at_ms).abs() <= SAME_SETTLEMENT_MS)
                .collect();
            if booked.iter().any(|l| self.booked_lines.contains_key(&l.id)) {
                self.problem = Some(format!(
                    "the settlement at {} overlaps an already booked ledger identity",
                    p.at_ms
                ));
                Self::keep_pending(p, now_ms, &mut still);
                continue;
            }
            let Some(rate) = rate else {
                Self::keep_pending(p, now_ms, &mut still);
                continue;
            };
            let (Some(expected), Some(model)) = (
                charge_legs(&self.instrument, p.live, &rate.rate, &rate.mark),
                charge_legs(&self.instrument, p.model, &rate.rate, &rate.mark),
            ) else {
                self.problem = Some(format!("the settlement at {} did not compute", p.at_ms));
                Self::keep_pending(p, now_ms, &mut still);
                continue;
            };
            // A position that was charged has a ledger line; until it
            // appears, the settlement is not over.
            if booked.is_empty() && expected != Cash::ZERO {
                Self::keep_pending(p, now_ms, &mut still);
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
            for line in &booked {
                self.booked_lines
                    .insert(line.id, (line.time_ms, line.amount));
            }
            out.push(Settled {
                at_ms: p.at_ms,
                rate: rate.rate.clone(),
                mark: rate.mark.clone(),
                venue,
                model,
                verified,
                lines: booked.into_iter().cloned().collect(),
            });
        }
        self.pending = still;
        out
    }

    fn keep_pending(mut p: Pending, now_ms: i64, still: &mut Vec<Pending>) {
        p.overdue |= now_ms.saturating_sub(p.at_ms) > OVERDUE_MS;
        still.push(p);
    }

    /// Settlements lacking rate or ledger evidence, retained until answered.
    #[must_use]
    pub fn unresolved(&self) -> Vec<i64> {
        self.pending.iter().map(|p| p.at_ms).collect()
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
                "the settlement at {} is {}waiting for the venue to publish its rate and ledger",
                p.at_ms,
                if p.overdue {
                    "overdue after 30 minutes and still "
                } else {
                    ""
                }
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
    fn an_overdue_settlement_stays_unresolved_and_late_evidence_is_booked_once() {
        let t = 1_790_323_200_000;
        let mut w = FundingWatch::new(btc(), Ok(Some(t)));
        w.on_time(
            t,
            (QtyLots(4), QtyLots(0)),
            (QtyLots(0), QtyLots(0)),
            || Some(t + 28_800_000),
        );
        assert!(w.resolve(&[rate(t)], &[], t + OVERDUE_MS + 1).is_empty());
        assert!(w.waiting());
        assert_eq!(w.unresolved(), vec![t]);
        assert!(w.evidence().is_err_and(|p| p.contains("overdue")));
        let rows = [line(t, -3_359_784, 1), line(t, -3_359_784, 1)];
        let late = w.resolve(&[rate(t)], &rows, t + OVERDUE_MS + 2);
        assert_eq!(late.len(), 1);
        assert_eq!(late[0].venue, Cash(-3_359_784));
        assert!(w.unresolved().is_empty());
        assert_eq!(w.evidence(), Ok((Cash(-3_359_784), Cash::ZERO)));
        assert!(w.resolve(&[rate(t)], &rows, t + OVERDUE_MS + 3).is_empty());
        assert_eq!(w.settled(), 1);
    }

    #[test]
    fn conflicting_history_identity_cannot_change_economics_or_discard_pending() {
        let t = 1_790_323_200_000;
        let mut w = FundingWatch::new(btc(), Ok(Some(t)));
        w.on_time(
            t,
            (QtyLots(4), QtyLots(0)),
            (QtyLots(4), QtyLots(0)),
            || Some(t + 28_800_000),
        );
        assert!(
            w.resolve(
                &[rate(t)],
                &[line(t, -3_359_784, 1), line(t, -3_359_783, 1)],
                t + 1
            )
            .is_empty()
        );
        assert_eq!(w.unresolved(), vec![t]);
        assert_eq!(w.settled(), 0);
        assert!(
            w.problem()
                .is_some_and(|p| p.contains("conflicting evidence"))
        );
        let resolved = w.resolve(&[rate(t)], &[line(t, -3_359_784, 1)], t + 2);
        assert_eq!(
            resolved[0].venue,
            Cash(-3_359_784),
            "confirmed venue cash is still booked while attribution is untrusted"
        );
        assert!(w.evidence().is_err());
    }

    #[test]
    fn a_missing_rate_also_remains_recoverable_after_the_overdue_boundary() {
        let t = 1_790_323_200_000;
        let mut w = FundingWatch::new(btc(), Ok(Some(t)));
        w.on_time(
            t,
            (QtyLots(4), QtyLots(0)),
            (QtyLots(4), QtyLots(0)),
            || Some(t + 28_800_000),
        );
        let rows = [line(t, -3_359_784, 1)];
        assert!(w.resolve(&[], &rows, t + OVERDUE_MS + 1).is_empty());
        assert_eq!(w.asking_since(), Some(t - SAME_SETTLEMENT_MS));
        assert_eq!(w.unresolved(), vec![t]);
        assert_eq!(w.resolve(&[rate(t)], &rows, t + OVERDUE_MS + 2).len(), 1);
        assert_eq!(w.evidence(), Ok((Cash(-3_359_784), Cash(-3_359_784))));
    }

    #[test]
    fn overlapping_settlement_windows_cannot_book_the_same_identity_twice() {
        let t = 1_790_323_200_000;
        let mut w = FundingWatch::new(btc(), Ok(Some(t)));
        let legs = (QtyLots(4), QtyLots(0));
        w.on_time(t, legs, legs, || Some(t + 1));
        w.on_time(t + 1, legs, legs, || Some(t + 28_800_000));
        let rows = [line(t, -3_359_784, 1)];
        let resolved = w.resolve(&[rate(t)], &rows, t + 2);
        assert_eq!(resolved.len(), 1);
        assert_eq!(w.settled(), 1);
        assert_eq!(w.unresolved(), vec![t + 1]);
        assert!(w.problem().is_some_and(|p| p.contains("overlaps")));
        assert!(w.resolve(&[rate(t)], &rows, t + 3).is_empty());
    }

    #[test]
    fn schedule_rediscovery_resumes_future_booking_without_certifying_the_unknown_interval() {
        let t = 1_790_323_200_000;
        let mut w = FundingWatch::new(btc(), Err("temporary discovery failure".into()));
        let legs = (QtyLots(4), QtyLots(0));
        w.on_time(t, legs, legs, || {
            panic!("retry belongs to host polling, not the hot loop")
        });
        w.on_time(t + 1, legs, legs, || panic!("no repeated schedule I/O"));
        assert_eq!(
            w.schedule_gaps(),
            &[ScheduleGap {
                since_ms: t,
                until_ms: None
            }]
        );
        w.rediscover_schedule(t + 10, Err("still down".into()));
        w.rediscover_schedule(t + 20, Ok(Some(t + 20)));
        assert!(w.needs_schedule(), "stale schedule is not extrapolated");
        w.rediscover_schedule(t + 30, Ok(Some(t + 60_000)));
        assert!(!w.needs_schedule());
        assert_eq!(
            w.schedule_gaps(),
            &[ScheduleGap {
                since_ms: t,
                until_ms: Some(t + 30)
            }]
        );
        w.on_time(t + 60_000, legs, legs, || Some(t + 28_800_000));
        let resolved = w.resolve(
            &[rate(t + 60_000)],
            &[line(t + 60_000, -3_359_784, 1)],
            t + 60_001,
        );
        assert_eq!(resolved[0].venue, Cash(-3_359_784));
        assert!(
            w.evidence().is_err(),
            "rediscovery cannot reconstruct model inventory during the unknown interval"
        );
    }

    #[test]
    fn losing_the_next_schedule_keeps_the_known_settlement_and_records_a_new_gap() {
        let t = 1_790_323_200_000;
        let legs = (QtyLots(4), QtyLots(0));
        let mut w = FundingWatch::new(btc(), Ok(Some(t)));
        w.on_time(t, legs, legs, || None);
        assert!(w.needs_schedule());
        assert_eq!(w.unresolved(), vec![t]);
        assert_eq!(
            w.schedule_gaps(),
            &[ScheduleGap {
                since_ms: t + 1,
                until_ms: None
            }]
        );
        w.rediscover_schedule(t + 10, Ok(Some(t + 100_000)));
        assert_eq!(
            w.resolve(&[rate(t)], &[line(t, -3_359_784, 1)], t + 11)
                .len(),
            1
        );
        w.on_time(t + 100_000, legs, legs, || None);
        assert_eq!(w.schedule_gaps().len(), 2);
        assert_eq!(w.schedule_gaps()[0].until_ms, Some(t + 10));
        assert_eq!(
            w.schedule_gaps()[1],
            ScheduleGap {
                since_ms: t + 100_001,
                until_ms: None
            }
        );
        assert!(w.evidence().is_err());
    }

    #[test]
    fn unknown_interval_cash_recovers_once_without_creating_model_evidence() {
        let t = 1_790_323_200_000;
        let legs = (QtyLots(4), QtyLots(0));
        let mut w = FundingWatch::new(btc(), Err("discovery failed".into()));
        w.on_time(t, legs, legs, || None);
        w.rediscover_schedule(t + 10, Ok(Some(t + 100_000)));
        let rows = [
            line(t + 5, -10, 1),
            line(t + 5, -10, 1),
            line(t - 1, -20, 2),
            line(t + 100_000, -30, 3),
        ];
        let recovered = w.recover_unknown(&rows, t + 20).unwrap();
        assert_eq!(recovered.lines, vec![line(t + 5, -10, 1)]);
        assert_eq!((recovered.since_ms, recovered.through_ms), (t, t + 20));
        assert!(w.recover_unknown(&rows, t + 30).is_none());
        assert!(
            w.needs_poll(),
            "closed gaps still poll for late published rows"
        );
        assert_eq!(
            w.settled(),
            0,
            "a recovered ledger row is not a model settlement"
        );
        assert!(w.evidence().is_err());
    }

    #[test]
    fn known_pending_settlement_rows_are_not_stolen_by_unknown_interval_recovery() {
        let t = 1_790_323_200_000;
        let legs = (QtyLots(4), QtyLots(0));
        let mut w = FundingWatch::new(btc(), Ok(Some(t)));
        w.on_time(t, legs, legs, || None);
        let rows = [line(t + 5, -3_359_784, 1)];
        assert!(w.recover_unknown(&rows, t + 10).is_none());
        assert_eq!(w.unresolved(), vec![t]);
        assert_eq!(w.resolve(&[rate(t)], &rows, t + 11).len(), 1);
        assert!(w.recover_unknown(&rows, t + 12).is_none());
    }

    #[test]
    fn conflicting_unknown_history_is_refused_before_any_new_identity_is_consumed() {
        let t = 1_790_323_200_000;
        let mut w = FundingWatch::new(btc(), Err("discovery failed".into()));
        w.on_time(
            t,
            (QtyLots(0), QtyLots(0)),
            (QtyLots(0), QtyLots(0)),
            || None,
        );
        assert!(
            w.recover_unknown(&[line(t + 1, 10, 1), line(t + 1, 11, 1)], t + 2)
                .is_none()
        );
        let recovered = w.recover_unknown(&[line(t + 1, 10, 1)], t + 3).unwrap();
        assert_eq!(recovered.lines.len(), 1);
        assert!(
            w.evidence().is_err(),
            "correcting rows does not silently erase the observed conflict"
        );
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
