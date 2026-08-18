//! The account, written down, so a later reading can be checked against
//! it by a command rather than by a person.
//!
//! # Why this exists
//!
//! The cutover playbook's step 2 takes the venue's own view of the
//! account and calls it the record everything else is checked against.
//! Step 5 then requires the new system's view to equal that record
//! *exactly*. Until this module that comparison was an operator reading
//! two terminal outputs side by side, at the one moment in the procedure
//! when the position is naked and the clock is running — which is
//! `CUTOVER.md` §6's second missing piece, and the reason a rehearsal
//! could not be run cleanly.
//!
//! # The format
//!
//! Text, one field per line, sorted. Not because a binary format would
//! be harder, but because this file is read by a person under pressure
//! and pasted into an incident note afterwards. A format that needs a
//! tool to inspect is a format nobody checks.
//!
//! It is deliberately *not* the capture or tick format: those are data
//! this project owns and versions. This is a note an operator takes,
//! and its whole lifetime is one maintenance window.
//!
//! # What it does not do
//!
//! It does not compare equity. Equity moves with the mark on every tick,
//! so an exact comparison would fail always and a tolerant one would
//! need a tolerance nobody can justify. Positions and resting orders are
//! what a cutover carries; equity is an observation about the market.

use core::fmt::Write as _;

use crate::snapshot::Snapshot;

/// A written record of an account at one instant.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// The contract this is about.
    pub symbol: String,
    /// When the venue's view was read, in epoch milliseconds.
    pub read_at_ms: i64,
    /// Each leg: side, amount, entry price.
    pub legs: Vec<(String, f64, f64)>,
    /// Client order ids of every resting order, sorted.
    pub orders: Vec<String>,
}

impl Record {
    /// Take a record from a snapshot.
    #[must_use]
    pub fn of(snapshot: &Snapshot) -> Self {
        let mut legs: Vec<(String, f64, f64)> = snapshot
            .positions
            .iter()
            .filter(|p| p.amount != 0.0)
            .map(|p| (p.position_side.clone(), p.amount, p.entry_price))
            .collect();
        // Sorted so two records of the same account compare equal
        // regardless of the order the venue happened to list them in.
        legs.sort_by(|a, b| a.0.cmp(&b.0));

        let mut orders: Vec<String> = snapshot
            .open_orders
            .iter()
            .map(|o| o.client_order_id.clone())
            .collect();
        orders.sort();

        Self {
            symbol: snapshot.symbol.clone(),
            read_at_ms: snapshot.read_at_ms(),
            legs,
            orders,
        }
    }

    /// Render for writing to a file.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "# openquanter account record");
        let _ = writeln!(
            out,
            "# Written by oq-recon --record. Compare with oq-recon --against."
        );
        let _ = writeln!(out, "symbol {}", self.symbol);
        let _ = writeln!(out, "read_at_ms {}", self.read_at_ms);
        for (side, amount, entry) in &self.legs {
            let _ = writeln!(out, "leg {side} {amount} {entry}");
        }
        for id in &self.orders {
            let _ = writeln!(out, "order {id}");
        }
        out
    }

    /// Read one back.
    ///
    /// # Errors
    /// Names the line it could not read. A record that half-parses is
    /// worse than one that does not: the comparison would pass against
    /// the half that survived.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut symbol = None;
        let mut read_at_ms = None;
        let mut legs = Vec::new();
        let mut orders = Vec::new();

        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let fail = |what: &str| format!("line {}: {what}: {line:?}", n + 1);
            match parts.next() {
                Some("symbol") => {
                    symbol = Some(parts.next().ok_or_else(|| fail("no symbol"))?.to_string());
                }
                Some("read_at_ms") => {
                    read_at_ms = Some(
                        parts
                            .next()
                            .ok_or_else(|| fail("no timestamp"))?
                            .parse::<i64>()
                            .map_err(|_| fail("timestamp is not a number"))?,
                    );
                }
                Some("leg") => {
                    let side = parts.next().ok_or_else(|| fail("leg has no side"))?;
                    let amount = parts
                        .next()
                        .ok_or_else(|| fail("leg has no amount"))?
                        .parse::<f64>()
                        .map_err(|_| fail("leg amount is not a number"))?;
                    let entry = parts
                        .next()
                        .ok_or_else(|| fail("leg has no entry price"))?
                        .parse::<f64>()
                        .map_err(|_| fail("leg entry is not a number"))?;
                    legs.push((side.to_string(), amount, entry));
                }
                Some("order") => {
                    orders.push(parts.next().ok_or_else(|| fail("no order id"))?.to_string());
                }
                Some(other) => return Err(fail(&format!("unknown field {other:?}"))),
                None => {}
            }
        }

        legs.sort_by(|a, b| a.0.cmp(&b.0));
        orders.sort();
        Ok(Self {
            symbol: symbol.ok_or("the record has no symbol")?,
            read_at_ms: read_at_ms.ok_or("the record has no timestamp")?,
            legs,
            orders,
        })
    }

    /// Every way this record and a later one differ.
    ///
    /// Empty means they describe the same account. The timestamp is
    /// deliberately not compared: the whole point is that the second
    /// reading is later.
    #[must_use]
    pub fn differences(&self, other: &Self) -> Vec<String> {
        let mut out = Vec::new();
        if self.symbol != other.symbol {
            out.push(format!(
                "symbol: recorded {} and read {}",
                self.symbol, other.symbol
            ));
        }

        for (side, amount, entry) in &self.legs {
            match other.legs.iter().find(|(s, _, _)| s == side) {
                None => out.push(format!("leg {side}: recorded {amount} and is now absent")),
                Some((_, a, e)) => {
                    if a != amount {
                        out.push(format!("leg {side}: recorded {amount} and read {a}"));
                    }
                    if e != entry {
                        // Not cosmetic. Every subsequent P&L and every
                        // stop distance is computed from this number.
                        out.push(format!("leg {side} entry: recorded {entry} and read {e}"));
                    }
                }
            }
        }
        for (side, amount, _) in &other.legs {
            if !self.legs.iter().any(|(s, _, _)| s == side) {
                out.push(format!("leg {side}: not recorded, and now holds {amount}"));
            }
        }

        for id in &self.orders {
            if !other.orders.contains(id) {
                out.push(format!("order {id}: recorded and now absent"));
            }
        }
        for id in &other.orders {
            if !self.orders.contains(id) {
                out.push(format!("order {id}: not recorded, and is now resting"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binance::{AccountSnapshot, OpenOrder, PositionSnapshot};
    use crate::snapshot::SnapshotBuilder;

    fn snapshot(legs: &[(&str, f64, f64)], orders: &[&str]) -> Snapshot {
        SnapshotBuilder::new("BTCUSDT")
            .account(AccountSnapshot {
                wallet_balance: 100.0,
                unrealized: 0.0,
                margin_balance: 100.0,
                read_at_ms: 1_700_000_000_000,
            })
            .positions(
                legs.iter()
                    .map(|(side, amount, entry)| PositionSnapshot {
                        symbol: "BTCUSDT".to_string(),
                        position_side: (*side).to_string(),
                        amount: *amount,
                        entry_price: *entry,
                        unrealized: 0.0,
                    })
                    .collect(),
            )
            .open_orders(
                orders
                    .iter()
                    .map(|id| OpenOrder {
                        symbol: "BTCUSDT".to_string(),
                        order_id: 1,
                        client_order_id: (*id).to_string(),
                        side: "BUY".to_string(),
                        position_side: "BOTH".to_string(),
                        price: 60_000.0,
                        orig_qty: 1.0,
                        executed_qty: 0.0,
                        status: "NEW".to_string(),
                    })
                    .collect(),
            )
            .seal()
            .expect("complete")
    }

    /// The whole point: a record survives being written and read, or the
    /// comparison at step 5 is against something else.
    #[test]
    fn a_record_survives_the_round_trip() {
        let r = Record::of(&snapshot(&[("LONG", 0.256, 71_444.87)], &["oq-1", "oq-2"]));
        let back = Record::parse(&r.render()).expect("readable");
        assert_eq!(r, back);
    }

    /// Two readings of the same account must compare equal even when the
    /// venue listed things in a different order, or the tool cries wolf
    /// at the worst moment in the procedure.
    #[test]
    fn ordering_from_the_venue_does_not_make_two_readings_differ() {
        let a = Record::of(&snapshot(
            &[("LONG", 1.0, 100.0), ("SHORT", -2.0, 200.0)],
            &["b", "a"],
        ));
        let b = Record::of(&snapshot(
            &[("SHORT", -2.0, 200.0), ("LONG", 1.0, 100.0)],
            &["a", "b"],
        ));
        assert_eq!(a.differences(&b), Vec::<String>::new());
    }

    /// A flat leg is not a position. The venue reports one for every
    /// instrument it knows about, and keeping them would make every
    /// record disagree with every other.
    #[test]
    fn flat_legs_are_not_recorded() {
        let r = Record::of(&snapshot(&[("LONG", 0.5, 100.0), ("BOTH", 0.0, 0.0)], &[]));
        assert_eq!(r.legs.len(), 1);
    }

    /// The difference that matters most and looks least alarming: the
    /// position is right and the average entry is not. Every subsequent
    /// P&L and every stop distance is computed from that number.
    #[test]
    fn an_entry_price_that_moved_is_reported() {
        let recorded = Record::of(&snapshot(&[("LONG", 0.256, 71_444.87)], &[]));
        let read = Record::of(&snapshot(&[("LONG", 0.256, 71_444.88)], &[]));
        let d = recorded.differences(&read);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].contains("entry"), "{d:?}");
    }

    /// An order that appeared between the two readings is the tell that
    /// the old system is still alive, which the playbook says to abort
    /// on rather than investigate.
    #[test]
    fn an_order_that_appeared_is_reported() {
        let recorded = Record::of(&snapshot(&[], &[]));
        let read = Record::of(&snapshot(&[], &["stranger-1"]));
        let d = recorded.differences(&read);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(
            d[0].contains("stranger-1") && d[0].contains("now resting"),
            "{d:?}"
        );
    }

    /// A leg that vanished is as serious as one that appeared, and a
    /// comparison that only looked one way would miss it.
    #[test]
    fn a_leg_that_vanished_is_reported() {
        let recorded = Record::of(&snapshot(&[("LONG", 1.0, 100.0)], &[]));
        let read = Record::of(&snapshot(&[], &[]));
        assert_eq!(recorded.differences(&read).len(), 1);
    }

    /// The timestamp is not compared. The second reading is later by
    /// construction, and a tool that reported that would report a
    /// difference every time.
    #[test]
    fn the_time_between_readings_is_not_a_difference() {
        let mut a = Record::of(&snapshot(&[("LONG", 1.0, 100.0)], &[]));
        let mut b = a.clone();
        a.read_at_ms = 1;
        b.read_at_ms = 999_999;
        assert_eq!(a.differences(&b), Vec::<String>::new());
    }

    /// A half-parsed record is worse than an unreadable one: the
    /// comparison would pass against whatever survived.
    #[test]
    fn a_record_that_does_not_parse_is_refused_by_line() {
        let e = Record::parse("symbol BTCUSDT\nread_at_ms 1\nleg LONG notanumber 5")
            .expect_err("bad amount");
        assert!(e.contains("line 3"), "{e}");
        assert!(Record::parse("read_at_ms 1").is_err(), "no symbol");
        assert!(Record::parse("symbol X").is_err(), "no timestamp");
        assert!(
            Record::parse("symbol X\nread_at_ms 1\nwat 3").is_err(),
            "an unknown field must not be skipped"
        );
    }

    /// Comments and blank lines survive, because this file gets pasted
    /// into an incident note and annotated.
    #[test]
    fn comments_are_allowed() {
        let r = Record::parse(
            "# taken at step 2, before cancelling\n\nsymbol BTCUSDT\nread_at_ms 7\nleg LONG 1 100\n",
        )
        .expect("readable");
        assert_eq!(r.symbol, "BTCUSDT");
        assert_eq!(r.legs.len(), 1);
    }
}
