//! What a running process believed it held, reconstructed from its
//! journal.
//!
//! # The gap this fills
//!
//! [`CUTOVER`](../../../docs/CUTOVER.md) §6 has carried this since the
//! playbook was written: `oq-recon --record` writes the **venue's**
//! account and `oq-recon --against` compares a later reading of the
//! venue against it. That catches the position moving. It does not catch
//! the new system *misreading* a position that never moved — which is
//! the failure a position-carrying cutover is actually exposed to, since
//! step 5 hands a live position to a process that has never seen it.
//!
//! Nothing here talks to a venue. It reads a journal and answers a
//! different question with the same vocabulary, so the two answers can
//! be diffed against one record:
//!
//! ```text
//! oq-recon  BTCUSDT --against before.txt   # is the venue where we left it
//! oq-belief run.oqj --against before.txt   # does the new process agree
//! ```
//!
//! # Why this could not be built until now
//!
//! A run started with `--adopt-existing` took a position into memory and
//! wrote nothing about it, so replaying its journal produced a belief
//! short by exactly the position being carried — the one thing a cutover
//! turns on. `Record::Reconciled` closed that, and it is the reason this
//! module can exist at all. A journal from before that change will
//! reconstruct as flat, and [`Belief::adopted`] says whether one was
//! seen so a caller can tell the two apart.
//!
//! # What it does not do
//!
//! **No margin, no equity, no liquidation price.** Those need the
//! contract specification, and a journal does not record one. Inventing
//! a plausible contract would make every number depend on a guess, and a
//! number that depends on a guess is worse here than a number that is
//! absent — the whole point is to compare against a venue reading.
//!
//! **One-way netting only.** Under hedge accounting a fill's leg is not
//! recoverable from `Submitted`, which records a side and not a leg.
//! Rather than assume, [`Belief::from_journal`] reports `hedged` when the
//! adopted legs show both directions, and a caller that sees it should
//! not trust the netted position.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use oq_journal::Reader;
use oq_types::Side;

use crate::record::{OutcomeTag, Record};

/// A process's own account, as its journal describes it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Belief {
    pub symbol: Option<String>,
    /// Signed net position in lots: positive long.
    pub position_lots: i64,
    /// Volume-weighted entry in ticks, or zero when flat.
    pub entry_ticks: i64,
    /// Client ids the journal leaves resting.
    pub resting: Vec<String>,
    /// Price and quantity decimals, for rendering.
    pub price_scale: u8,
    pub qty_scale: u8,
    /// Whether a `Reconciled` record was seen.
    ///
    /// `false` on a journal written before adopted positions were
    /// recorded, where a flat reconstruction may mean *flat* or may mean
    /// *carrying a position nobody wrote down*. The two are
    /// indistinguishable from the file, so they are not distinguished
    /// here either.
    pub adopted: bool,
    /// Adopted legs pointed in both directions.
    pub hedged: bool,
    /// When this run adopted the venue's position, in nanoseconds.
    ///
    /// The first moment this journal describes the venue, and so the
    /// earliest reading of the account that can be compared with it: a
    /// reading taken before this is a reading of the run *before* this
    /// one, and every order the two have apart would be reported as a
    /// difference in both directions.
    pub adopted_at: Option<i64>,
    /// Each position leg as `(name, signed lots, entry ticks)`: `LONG`
    /// and `SHORT` on a hedged account, one net leg named by its sign on
    /// a one-way one. What [`Belief::to_record`] reports, because a
    /// hedged account compared as a net number disagrees with itself.
    pub legs: Vec<(String, i64, i64)>,
    /// Records the reader could not decode.
    pub undecodable: u64,
}

impl Belief {
    /// Replay `path` and report what the run that wrote it believed.
    ///
    /// # Errors
    /// Anything the journal reports. An absent journal is an error here
    /// rather than an empty belief: `recovery::recover` may treat a
    /// missing file as a first run, but a cutover check pointed at a
    /// file that is not there has compared nothing.
    pub fn from_journal(path: impl AsRef<Path>) -> Result<Self, oq_journal::JournalError> {
        let replay = Reader::open(path)?.replay()?;
        let mut b = Self::default();

        // Side and reduce-only per submission, so a fill can be applied
        // in the right direction. A fill record names a client id and a
        // quantity; the direction lives in the submission it answers.
        let mut submitted: HashMap<String, (Side, bool, String)> = HashMap::new();
        let mut accepted: Vec<String> = Vec::new();
        let mut filled: HashMap<String, i64> = HashMap::new();
        let mut withdrawn: HashSet<String> = HashSet::new();
        let mut ordered: Vec<i64> = Vec::new();
        // Leg name -> position. `NET` is a one-way account's single
        // position, named by its sign on the way out.
        let mut legs: std::collections::BTreeMap<String, Leg> = std::collections::BTreeMap::new();
        // The net position, folded alongside the legs and written onto
        // the belief once the replay is done: an entry is a quotient,
        // and it is taken once, after the last fill, rather than carried
        // through the replay as a number every fill divides again.
        let mut net = Leg::default();

        for frame in replay.since(0) {
            match Record::decode(frame.kind, &frame.payload) {
                Some(Record::SessionStart {
                    symbol,
                    price_scale,
                    qty_scale,
                    ..
                }) => {
                    b.symbol = Some(symbol);
                    b.price_scale = price_scale;
                    b.qty_scale = qty_scale;
                }
                Some(Record::Reconciled { at, legs: adopted }) => {
                    // The venue's whole position at a process's start, and
                    // it already contains every fill before it. So it
                    // replaces what the journal had built, rather than
                    // adding to it: a journal a restarted process appended
                    // to counted the first run's position twice. Nothing
                    // earlier is resting either — a process starts only
                    // with no order on the venue.
                    b.adopted = true;
                    b.adopted_at = Some(at.0);
                    net = Leg::default();
                    b.hedged = false;
                    legs.clear();
                    accepted.clear();
                    filled.clear();
                    withdrawn.clear();
                    let mut longs = false;
                    let mut shorts = false;
                    for (_symbol, side, lots, entry) in adopted {
                        // `saturating_abs`: a lot count of `i64::MIN`
                        // is not a position, and `abs` on it panics
                        // where a report is what is wanted.
                        let size = lots.saturating_abs();
                        let (name, signed) = if side.eq_ignore_ascii_case("SHORT") {
                            shorts = true;
                            ("SHORT", -size)
                        } else {
                            longs = true;
                            ("LONG", size)
                        };
                        net.fold(signed, entry);
                        legs.entry(name.to_string())
                            .or_default()
                            .fold(signed, entry);
                    }
                    b.hedged = longs && shorts;
                }
                Some(Record::Submitted {
                    client_id,
                    side,
                    reduce_only,
                    leg,
                    ..
                }) => {
                    submitted.insert(client_id, (side, reduce_only, leg));
                }
                Some(Record::Cancelled { client_id, .. }) => {
                    withdrawn.insert(client_id);
                }
                Some(Record::Outcome { client_id, tag, .. }) => match tag {
                    OutcomeTag::Accepted => {
                        if !accepted.contains(&client_id) {
                            accepted.push(client_id);
                        }
                    }
                    // Rejected: it never existed. Unknown: nobody knows,
                    // and a belief that listed it as resting would be
                    // asserting the thing `Placed::Unknown` exists to
                    // refuse to assert. It is left out and the caller
                    // finds it through `recovery::recover`, which is the
                    // function whose whole job is unresolved orders.
                    OutcomeTag::Rejected | OutcomeTag::Unknown => {}
                },
                Some(Record::Fill {
                    client_id,
                    qty,
                    price,
                    ..
                }) => {
                    let Some((side, _, leg)) = submitted.get(&client_id).cloned() else {
                        // A fill for a submission this journal does not
                        // contain. Counted as undecodable rather than
                        // guessed: applying it with an assumed side is
                        // how a reconstruction quietly reports the
                        // opposite position.
                        b.undecodable += 1;
                        continue;
                    };
                    let lots = parse_scaled(&qty, b.qty_scale);
                    let ticks = parse_scaled(&price, b.price_scale);
                    let (Some(lots), Some(ticks)) = (lots, ticks) else {
                        b.undecodable += 1;
                        continue;
                    };
                    let signed = if side == Side::Buy { lots } else { -lots };
                    // Which leg it moved. A hedged account's leg is only
                    // in the submission; one written before it was
                    // recorded cannot be placed, and is counted as a hole
                    // rather than guessed — a guess here is a close of the
                    // short booked as an open of the long.
                    let key =
                        if leg.eq_ignore_ascii_case("LONG") || leg.eq_ignore_ascii_case("SHORT") {
                            Some(leg.to_ascii_uppercase())
                        } else {
                            net_leg(&mut legs)
                        };
                    let Some(key) = key else {
                        b.undecodable += 1;
                        continue;
                    };
                    *filled.entry(client_id).or_default() += lots;
                    ordered.push(lots);
                    net.fold(signed, ticks);
                    legs.entry(key).or_default().fold(signed, ticks);
                }
                Some(_) => {}
                None => b.undecodable += 1,
            }
        }

        // Resting: accepted, not consumed by fills, and not withdrawn.
        //
        // The third clause was missing until a journal claimed many
        // times the resting orders the account actually held. Every
        // extra one had been cancelled — and the journal had no record
        // of a cancellation to read, so this could not have known.
        // Both halves were fixed together: `Record::Cancelled` exists
        // now, and this subtracts it.
        //
        // Any fill at all takes an order off this list, so a partially
        // filled order is not reported as resting even though the
        // remainder is. Understating is the safe direction here — the
        // venue's own open orders are what a cutover cancels against —
        // and correcting it needs the submitted quantity compared
        // against the filled one, which is a separate change.
        b.resting = accepted
            .into_iter()
            .filter(|id| filled.get(id).copied().unwrap_or(0) == 0 && !withdrawn.contains(id))
            .collect();
        b.resting.sort();
        // Every fill has been folded, so the one division an entry needs
        // happens here — once, on the finished sum.
        b.position_lots = net.lots;
        b.entry_ticks = net.entry_ticks();
        b.legs = legs
            .into_iter()
            .filter(|(_, leg)| leg.lots != 0)
            .map(|(name, leg)| {
                let name = if name == "NET" {
                    if leg.lots > 0 { "LONG" } else { "SHORT" }.to_string()
                } else {
                    name
                };
                (name, leg.lots, leg.entry_ticks())
            })
            .collect();
        b.legs.sort_by(|a, c| a.0.cmp(&c.0));
        Ok(b)
    }

    /// The same shape `oq-recon --record` writes, so the two compare.
    #[must_use]
    pub fn to_record(&self, read_at_ms: i64) -> oq_gateway::record::Record {
        let scale = |v: i64, decimals: u8| -> f64 {
            #[allow(clippy::cast_precision_loss)]
            let x = v as f64;
            x / 10f64.powi(i32::from(decimals))
        };
        let legs = self
            .legs
            .iter()
            .map(|(name, lots, entry)| {
                (
                    name.clone(),
                    scale(*lots, self.qty_scale),
                    scale(*entry, self.price_scale),
                )
            })
            .collect();
        oq_gateway::record::Record {
            symbol: self.symbol.clone().unwrap_or_default(),
            read_at_ms,
            legs,
            orders: self.resting.clone(),
        }
    }
}

/// One leg of a position: signed lots, and the signed notional behind
/// them.
///
/// An entry price is a quotient, and holding one means dividing on every
/// fill. A venue splits one order into partial fills at the same price,
/// so a reconstruction that re-averages meets that division once per
/// partial fill, and every one of them rounds. The rounded entry is then
/// the input to the next division, so the error is not just repeated but
/// fed back — a position built out of two partial fills lands a unit or
/// two off the entry the venue reports, and the console calls that a
/// disagreement when nothing disagrees.
///
/// What a venue holds is the notional: price times quantity, summed.
/// Its entry price is that sum divided by the position, once, at the
/// moment somebody asks. Holding the same sum here has no rounding in it
/// at all, so a fill split in two lands exactly where one fill would
/// have — and the one division that remains happens at the very end, in
/// [`Leg::entry_ticks`], where there is nothing left to feed it into.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Leg {
    lots: i64,
    /// Price ticks times lots, in tick-lots — the unit is why this is
    /// wide. Signed with the position.
    notional: i128,
}

impl Leg {
    /// The volume-weighted entry in ticks, or zero when flat.
    fn entry_ticks(self) -> i64 {
        if self.lots == 0 {
            return 0;
        }
        i64::try_from(self.notional.abs() / i128::from(self.lots.saturating_abs()))
            .unwrap_or(i64::MAX)
    }

    /// Fold one signed quantity at one price into the leg.
    ///
    /// Volume-weighted while adding, untouched while reducing, and reset
    /// when the position crosses through flat — the same convention a
    /// venue reports, because the number exists to be compared with one.
    fn fold(&mut self, signed_lots: i64, price_ticks: i64) {
        if signed_lots == 0 {
            return;
        }
        let before = self.lots;
        let after = before.saturating_add(signed_lots);
        if before == 0 || (before > 0) == (signed_lots > 0) {
            // Opening or adding. Exact: summed, never re-averaged.
            self.notional = self
                .notional
                .saturating_add(i128::from(signed_lots).saturating_mul(i128::from(price_ticks)));
        } else if (before > 0) != (after > 0) && after != 0 {
            // Crossed through flat: the remainder is a new position at
            // the price that reversed it.
            self.notional = i128::from(after).saturating_mul(i128::from(price_ticks));
        } else if after == 0 {
            self.notional = 0;
        } else {
            // Reducing: the entry does not move, so the notional shrinks
            // in proportion. The truncation cannot move the entry —
            // what it drops is less than one lot's worth of it, and
            // dividing what is left by a smaller position gives back the
            // same quotient.
            let shrunk = self
                .notional
                .abs()
                .saturating_mul(i128::from(after.saturating_abs()))
                / i128::from(before.saturating_abs());
            self.notional = if self.notional < 0 { -shrunk } else { shrunk };
        }
        self.lots = after;
    }
}

/// The key a one-way fill goes to: the account's single position.
///
/// A leg adopted under its direction's name becomes that position, since
/// on a one-way account it is the only one. An account holding both legs
/// has no single position, and `None` says so.
fn net_leg(legs: &mut std::collections::BTreeMap<String, Leg>) -> Option<String> {
    if legs.contains_key("NET") {
        return Some("NET".into());
    }
    let named: Vec<String> = legs.keys().filter(|k| *k != "NET").cloned().collect();
    match named.as_slice() {
        [] => {
            legs.insert("NET".into(), Leg::default());
            Some("NET".into())
        }
        [one] => {
            let pos = legs.remove(one).unwrap_or_default();
            legs.insert("NET".into(), pos);
            Some("NET".into())
        }
        _ => None,
    }
}

/// Parse a venue's decimal string into fixed point at `decimals`.
///
/// Returns `None` rather than zero on anything unparseable. Zero is a
/// quantity and a price, and a parse failure reported as one is a fill
/// silently applied at no size.
fn parse_scaled(text: &str, decimals: u8) -> Option<i64> {
    let text = text.trim();
    let (neg, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (int, frac) = text.split_once('.').unwrap_or((text, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    if !int.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let d = usize::from(decimals);
    let mut digits = String::with_capacity(int.len() + d);
    digits.push_str(int);
    for i in 0..d {
        digits.push(frac.as_bytes().get(i).map_or('0', |b| *b as char));
    }
    // More decimals than the contract quotes is the venue disagreeing
    // with the instrument table, which is worth refusing rather than
    // rounding away.
    if frac.len() > d {
        return None;
    }
    let v: i64 = digits.parse().ok()?;
    Some(if neg { -v } else { v })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decimal_string_becomes_fixed_point() {
        assert_eq!(parse_scaled("1.5", 3), Some(1_500));
        assert_eq!(parse_scaled("0.001", 3), Some(1));
        assert_eq!(parse_scaled("-2", 2), Some(-200));
        assert_eq!(parse_scaled("12", 0), Some(12));
    }

    /// More precision than the contract quotes is refused.
    ///
    /// Rounding it away would make the reconstruction disagree with the
    /// venue by less than a tick and give no reason why.
    #[test]
    fn more_decimals_than_the_contract_quotes_is_refused() {
        assert_eq!(parse_scaled("1.0001", 3), None);
    }

    #[test]
    fn garbage_is_none_rather_than_zero() {
        assert_eq!(parse_scaled("", 2), None);
        assert_eq!(parse_scaled("abc", 2), None);
        assert_eq!(parse_scaled("1.2.3", 2), None);
    }

    #[test]
    fn adding_averages_the_entry_and_reducing_leaves_it() {
        let mut leg = Leg::default();
        leg.fold(2, 100);
        leg.fold(2, 200);
        assert_eq!(leg.lots, 4);
        assert_eq!(leg.entry_ticks(), 150);
        leg.fold(-2, 999);
        assert_eq!(leg.lots, 2);
        assert_eq!(
            leg.entry_ticks(),
            150,
            "a reduction must not move the entry"
        );
    }

    /// Crossing through flat starts a new position at the reversing
    /// price rather than carrying the old average into the other side.
    #[test]
    fn crossing_through_flat_resets_the_entry() {
        let mut leg = Leg::default();
        leg.fold(2, 100);
        leg.fold(-5, 300);
        assert_eq!(leg.lots, -3);
        assert_eq!(leg.entry_ticks(), 300);
    }

    #[test]
    fn closing_exactly_leaves_no_entry() {
        let mut leg = Leg::default();
        leg.fold(3, 100);
        leg.fold(-3, 400);
        assert_eq!(leg.lots, 0);
        assert_eq!(leg.entry_ticks(), 0);
    }

    /// Reducing lands on the same entry it started from, whatever the
    /// position and whatever is left of it.
    ///
    /// The notional shrinks by integer division, so this is the claim
    /// that the truncation cannot feed back into the quotient.
    #[test]
    fn reducing_never_moves_the_entry() {
        for before in [3_i64, 7, 40, 256, 999] {
            for price in [8_317_320_i64, 100, 8_367_321] {
                let mut leg = Leg::default();
                leg.fold(before, price);
                let entry = leg.entry_ticks();
                for left in 1..before {
                    let mut reduced = leg;
                    reduced.fold(-(before - left), 0);
                    assert_eq!(reduced.lots, left);
                    assert_eq!(
                        reduced.entry_ticks(),
                        entry,
                        "reducing {before} to {left} at {price} moved the entry"
                    );
                }
            }
        }
    }

    /// An order the venue split into partial fills lands where the same
    /// trade taken whole would have.
    ///
    /// This is the shape of the defect the notional exists to prevent.
    /// The entry used to be re-averaged and truncated once per fill, so
    /// two partial fills at one price came out a unit below the same
    /// quantity filled at once — and the console read that unit as the
    /// account disagreeing with the venue, which is the one thing it
    /// exists to report.
    #[test]
    fn a_split_fill_lands_where_a_whole_one_would() {
        let whole = {
            let mut leg = Leg::default();
            leg.fold(20, 8_367_320);
            leg.fold(20, 8_317_320);
            leg.entry_ticks()
        };
        let split = {
            let mut leg = Leg::default();
            leg.fold(20, 8_367_320);
            leg.fold(16, 8_317_320);
            leg.fold(4, 8_317_320);
            leg.entry_ticks()
        };
        assert_eq!(whole, 8_342_320);
        assert_eq!(split, whole, "the partial fills drifted off the entry");
    }
}
