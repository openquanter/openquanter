//! Comparing what we believe against what the venue reports.
//!
//! The rule this implements, from `docs/LIVE-PATH.md`: mark every known
//! order stale, overwrite with what the venue reports, and treat the
//! residue as a fact requiring resolution rather than a state to invent.
//! Nothing here repairs anything. It classifies, and it says which
//! classes must stop the process.
//!
//! ## Why classification rather than a boolean
//!
//! "Reconciled" and "not reconciled" is the wrong shape. An order the
//! venue has not mentioned since a reconnect and a position that is the
//! wrong size are both differences, and they call for opposite
//! responses: the first may resolve on the next read, the second means
//! every ladder rung the strategy places from here is anchored on a
//! number that is wrong. A single boolean forces those into one bucket,
//! and whichever way it is set, half the cases are handled wrongly.
//!
//! ## Why nothing is repaired
//!
//! NautilusTrader closes position gaps by fabricating a fill — priced,
//! when nothing better exists, by a market order — and its own comment
//! concedes the information loss. For a strategy whose ladder is
//! anchored on the position's average entry, inventing a fill invents
//! that anchor: the rungs land at the wrong prices and the take-profit
//! is never reached. The gap is the finding. Closing it destroys the
//! finding and keeps the cause.

use crate::snapshot::Snapshot;

/// What we believe one leg holds.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedLeg {
    /// `LONG` or `SHORT`, matching the venue's own naming so a
    /// comparison never turns on a translation.
    pub side: String,
    /// Signed, in the instrument's quantity units.
    pub amount: f64,
    pub entry_price: f64,
}

/// What we believe the account holds.
///
/// Filled by whatever holds the local model — the kernel in an assembled
/// system, a recorded state in a shadow run. Kept free of any dependency
/// on the core so that a reconciler can run beside a system written in
/// another language, which is exactly the first use.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Expectation {
    pub legs: Vec<ExpectedLeg>,
    /// Client order ids we believe are resting at the venue.
    pub working_orders: Vec<String>,
}

/// One classified difference.
#[derive(Debug, Clone, PartialEq)]
pub enum Divergence {
    /// Both sides hold the leg, in different sizes.
    PositionSize {
        side: String,
        expected: f64,
        venue: f64,
    },
    /// Both sides hold the leg, at different entry prices.
    ///
    /// Separate from a size difference because it has a different cause
    /// — a missed fill changes size, a mis-averaged fill changes only
    /// this — and because a ladder anchored on the average is wrong from
    /// this alone.
    PositionEntry {
        side: String,
        expected: f64,
        venue: f64,
    },
    /// We believe we hold it; the venue reports nothing.
    PositionAbsentAtVenue { side: String, expected: f64 },
    /// The venue holds it; we did not know.
    PositionUnknownLocally { side: String, venue: f64 },
    /// We believe it rests; the venue did not report it.
    ///
    /// Roq's stale order: by construction the venue declined to mention
    /// it, so it is terminal and unknowable. Emitted once, never
    /// resolved by guessing which terminal state it reached.
    OrderStale { client_order_id: String },
    /// The venue reports an order we did not place, or no longer track.
    OrderUnknownLocally {
        client_order_id: String,
        venue_order_id: i64,
    },
    /// A venue figure that does not land on the instrument's grid.
    ///
    /// Not pedantry: it means the assumed tick or lot size is wrong, and
    /// every comparison made through that conversion is meaningless.
    /// Better caught as its own class than as a thousand tiny position
    /// differences.
    OffGrid { what: &'static str, value: f64 },
}

impl Divergence {
    /// Whether this difference must stop the process.
    ///
    /// Position differences are fatal because everything the strategy
    /// does next is computed from the position. Order-level differences
    /// are reported and not fatal: an order the venue has not mentioned
    /// may be mentioned on the next read, and stopping on that would
    /// make a reconnect indistinguishable from a corruption.
    ///
    /// An off-grid value is fatal because it invalidates the comparison
    /// itself rather than reporting a result of it.
    #[must_use]
    pub const fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::PositionSize { .. }
                | Self::PositionEntry { .. }
                | Self::PositionAbsentAtVenue { .. }
                | Self::PositionUnknownLocally { .. }
                | Self::OffGrid { .. }
        )
    }

    /// A line a person reads at three in the morning.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::PositionSize {
                side,
                expected,
                venue,
            } => format!("{side} position: we hold {expected}, the venue holds {venue}"),
            Self::PositionEntry {
                side,
                expected,
                venue,
            } => format!("{side} entry price: we have {expected}, the venue has {venue}"),
            Self::PositionAbsentAtVenue { side, expected } => {
                format!("{side} position: we hold {expected}, the venue reports none")
            }
            Self::PositionUnknownLocally { side, venue } => {
                format!("{side} position: the venue holds {venue}, we know of none")
            }
            Self::OrderStale { client_order_id } => format!(
                "order {client_order_id} was not reported by the venue; \
                 it is terminal and its outcome is unknowable from here"
            ),
            Self::OrderUnknownLocally {
                client_order_id,
                venue_order_id,
            } => format!(
                "the venue reports order {client_order_id} ({venue_order_id}), we do not track it"
            ),
            Self::OffGrid { what, value } => format!(
                "{what} {value} does not land on the instrument's grid; \
                 the assumed tick or lot size is wrong and every comparison \
                 through it is meaningless"
            ),
        }
    }
}

/// The outcome of one comparison.
#[derive(Debug, Clone, PartialEq)]
pub struct Reconciliation {
    pub divergences: Vec<Divergence>,
    /// When the snapshot was read, in venue milliseconds.
    pub read_at_ms: i64,
}

impl Reconciliation {
    #[must_use]
    pub fn agrees(&self) -> bool {
        self.divergences.is_empty()
    }

    /// Whether anything found must stop the process.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        self.divergences.iter().any(Divergence::is_fatal)
    }

    /// Differences that must stop the process.
    #[must_use]
    pub fn fatal(&self) -> Vec<&Divergence> {
        self.divergences.iter().filter(|d| d.is_fatal()).collect()
    }

    /// A report a person reads.
    #[must_use]
    pub fn render(&self) -> String {
        if self.agrees() {
            return format!(
                "reconciled at {}: the model matches the venue\n",
                self.read_at_ms
            );
        }
        let mut out = format!(
            "reconciled at {}: {} difference(s)\n",
            self.read_at_ms,
            self.divergences.len()
        );
        for d in &self.divergences {
            let mark = if d.is_fatal() { "FATAL" } else { "note " };
            out.push_str(&format!("  {mark}  {}\n", d.describe()));
        }
        if self.is_fatal() {
            out.push_str(
                "\nNothing has been adjusted. A position that is wrong is not \
                 repaired by writing down the venue's number: whatever produced \
                 the difference is still there, and the difference is the only \
                 evidence of it.\n",
            );
        }
        out
    }
}

/// How closely two figures must agree.
///
/// A tolerance is needed because the venue reports decimal strings and
/// the model holds fixed-point, so exact float equality would report a
/// difference on every read. It is deliberately far below one lot: this
/// absorbs representation, not disagreement.
#[derive(Debug, Clone, Copy)]
pub struct Tolerance {
    pub quantity: f64,
    pub price: f64,
    /// The instrument's quantity step, for the off-grid check. Zero
    /// disables that check.
    pub lot: f64,
}

impl Default for Tolerance {
    fn default() -> Self {
        Self {
            quantity: 1e-9,
            price: 1e-6,
            lot: 0.0,
        }
    }
}

/// Compare a belief against a sealed snapshot.
///
/// Takes [`Snapshot`] rather than its parts, because a snapshot exists
/// only if every read succeeded. A comparison against a partial answer
/// would report everything not yet received as absent from the venue,
/// which is the precise failure a terminator exists to prevent.
#[must_use]
pub fn reconcile(expected: &Expectation, venue: &Snapshot, tol: Tolerance) -> Reconciliation {
    let mut divergences = Vec::new();

    if tol.lot > 0.0 {
        for p in &venue.positions {
            let steps = p.amount / tol.lot;
            if (steps - steps.round()).abs() > 1e-6 {
                divergences.push(Divergence::OffGrid {
                    what: "position size",
                    value: p.amount,
                });
            }
        }
    }

    // Every leg we believe in, checked against the venue's answer.
    for want in &expected.legs {
        // A leg we believe is flat and a leg the venue does not mention
        // are the same fact. The venue reports a flat leg for every
        // instrument it knows and those are dropped on the way in, so a
        // model that still lists a leg it has just closed to zero would
        // otherwise be told its position is absent — and that is fatal,
        // which stops a process over an agreement.
        if want.amount.abs() <= tol.quantity && venue.leg(&want.side).is_none() {
            continue;
        }
        match venue.leg(&want.side) {
            Some(got) => {
                if (want.amount - got.amount).abs() > tol.quantity {
                    divergences.push(Divergence::PositionSize {
                        side: want.side.clone(),
                        expected: want.amount,
                        venue: got.amount,
                    });
                }
                if (want.entry_price - got.entry_price).abs() > tol.price {
                    divergences.push(Divergence::PositionEntry {
                        side: want.side.clone(),
                        expected: want.entry_price,
                        venue: got.entry_price,
                    });
                }
            }
            None => divergences.push(Divergence::PositionAbsentAtVenue {
                side: want.side.clone(),
                expected: want.amount,
            }),
        }
    }

    // And the reverse: anything the venue holds that we do not. A leg
    // the venue reports as flat is not something it holds; this venue
    // drops those already, and another need not.
    for got in &venue.positions {
        if got.amount.abs() <= tol.quantity {
            continue;
        }
        if !expected.legs.iter().any(|w| w.side == got.position_side) {
            divergences.push(Divergence::PositionUnknownLocally {
                side: got.position_side.clone(),
                venue: got.amount,
            });
        }
    }

    // Orders: mark stale, overwrite from the venue, surface the residue.
    let reported: Vec<&str> = venue
        .open_orders
        .iter()
        .map(|o| o.client_order_id.as_str())
        .collect();
    for ours in &expected.working_orders {
        if !reported.contains(&ours.as_str()) {
            divergences.push(Divergence::OrderStale {
                client_order_id: ours.clone(),
            });
        }
    }
    for theirs in &venue.open_orders {
        if !expected.working_orders.contains(&theirs.client_order_id) {
            divergences.push(Divergence::OrderUnknownLocally {
                client_order_id: theirs.client_order_id.clone(),
                venue_order_id: theirs.order_id,
            });
        }
    }

    Reconciliation {
        divergences,
        read_at_ms: venue.read_at_ms(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binance::{AccountSnapshot, OpenOrder, PositionSnapshot};
    use crate::snapshot::SnapshotBuilder;

    fn leg(side: &str, amount: f64, entry: f64) -> PositionSnapshot {
        PositionSnapshot {
            symbol: "BTCUSDT".into(),
            position_side: side.into(),
            amount,
            entry_price: entry,
            unrealized: 0.0,
        }
    }

    fn order(cid: &str, id: i64) -> OpenOrder {
        OpenOrder {
            symbol: "BTCUSDT".into(),
            order_id: id,
            client_order_id: cid.into(),
            side: "BUY".into(),
            position_side: "LONG".into(),
            price: 60_000.0,
            orig_qty: 0.002,
            executed_qty: 0.0,
            status: "NEW".into(),
        }
    }

    fn snapshot(positions: Vec<PositionSnapshot>, orders: Vec<OpenOrder>) -> Snapshot {
        SnapshotBuilder::new("BTCUSDT")
            .account(AccountSnapshot {
                wallet_balance: 20_000.0,
                unrealized: 0.0,
                margin_balance: 20_000.0,
                read_at_ms: 1_700_000_000_000,
            })
            .positions(positions)
            .open_orders(orders)
            .seal()
            .expect("seals")
    }

    fn expect(legs: Vec<ExpectedLeg>, orders: Vec<&str>) -> Expectation {
        Expectation {
            legs,
            working_orders: orders.into_iter().map(String::from).collect(),
        }
    }

    fn want(side: &str, amount: f64, entry: f64) -> ExpectedLeg {
        ExpectedLeg {
            side: side.into(),
            amount,
            entry_price: entry,
        }
    }

    #[test]
    fn agreement_reports_nothing() {
        let r = reconcile(
            &expect(vec![want("LONG", 0.256, 71_444.87)], vec!["c-1"]),
            &snapshot(vec![leg("LONG", 0.256, 71_444.87)], vec![order("c-1", 1)]),
            Tolerance::default(),
        );
        assert!(r.agrees(), "{}", r.render());
        assert!(!r.is_fatal());
    }

    /// A leg closed to zero locally and a leg the venue does not mention
    /// are the same fact. The venue reports a flat leg for every
    /// instrument it knows and those are dropped on the way in, so a
    /// model that still lists the leg it just closed would be told its
    /// position is absent — and that is fatal, which stops a process
    /// over an agreement.
    #[test]
    fn a_flat_leg_agrees_with_a_venue_that_does_not_mention_it() {
        let venue = snapshot(Vec::new(), Vec::new());
        let expected = Expectation {
            legs: vec![want("LONG", 0.0, 0.0)],
            working_orders: Vec::new(),
        };
        let r = reconcile(&expected, &venue, Tolerance::default());
        assert!(
            r.agrees(),
            "both sides say flat, so there is nothing to report: {:?}",
            r.divergences
        );
        assert!(!r.is_fatal());
    }

    /// And the size that is genuinely missing is still found: the
    /// exemption is for zero, not for any absent leg.
    #[test]
    fn a_leg_with_size_is_still_reported_absent() {
        let venue = snapshot(Vec::new(), Vec::new());
        let expected = Expectation {
            legs: vec![want("LONG", 0.25, 71_444.87)],
            working_orders: Vec::new(),
        };
        let r = reconcile(&expected, &venue, Tolerance::default());
        assert_eq!(
            r.divergences,
            vec![Divergence::PositionAbsentAtVenue {
                side: "LONG".into(),
                expected: 0.25,
            }]
        );
        assert!(r.is_fatal());
    }

    /// A venue that does not drop its own flat legs must not turn them
    /// into positions we failed to know about.
    #[test]
    fn a_flat_leg_the_venue_does_report_is_not_a_position_we_missed() {
        let venue = snapshot(vec![leg("SHORT", 0.0, 0.0)], Vec::new());
        let r = reconcile(&Expectation::default(), &venue, Tolerance::default());
        assert!(
            r.agrees(),
            "a flat leg is not something the venue holds: {:?}",
            r.divergences
        );
    }

    #[test]
    fn a_size_difference_is_fatal() {
        let r = reconcile(
            &expect(vec![want("LONG", 0.256, 71_444.87)], vec![]),
            &snapshot(vec![leg("LONG", 0.128, 71_444.87)], vec![]),
            Tolerance::default(),
        );
        assert!(r.is_fatal());
        assert_eq!(r.fatal().len(), 1);
    }

    /// A ladder anchored on the average is wrong from an entry-price
    /// difference alone, even when the size agrees.
    #[test]
    fn an_entry_price_difference_is_fatal_on_its_own() {
        let r = reconcile(
            &expect(vec![want("LONG", 0.256, 71_444.87)], vec![]),
            &snapshot(vec![leg("LONG", 0.256, 71_000.00)], vec![]),
            Tolerance::default(),
        );
        assert!(r.is_fatal());
        assert!(matches!(r.divergences[0], Divergence::PositionEntry { .. }));
    }

    /// The direction that a naive reconciler misses: the venue holds
    /// something we never knew about.
    #[test]
    fn a_position_only_the_venue_knows_about_is_found() {
        let r = reconcile(
            &expect(vec![], vec![]),
            &snapshot(vec![leg("SHORT", -0.064, 60_000.0)], vec![]),
            Tolerance::default(),
        );
        assert!(r.is_fatal());
        assert!(matches!(
            r.divergences[0],
            Divergence::PositionUnknownLocally { .. }
        ));
    }

    /// Hummingbot's orphan: an order resting at the venue that nothing
    /// local tracks. Its own issue asking for this was closed without
    /// implementing it.
    #[test]
    fn an_order_only_the_venue_knows_about_is_found() {
        let r = reconcile(
            &expect(vec![], vec![]),
            &snapshot(vec![], vec![order("someone-elses", 99)]),
            Tolerance::default(),
        );
        assert_eq!(r.divergences.len(), 1);
        assert!(matches!(
            r.divergences[0],
            Divergence::OrderUnknownLocally { .. }
        ));
    }

    /// Roq's stale order. Reported, and not fatal: the venue may mention
    /// it on the next read, and halting here would make a reconnect
    /// indistinguishable from a corruption.
    #[test]
    fn an_order_the_venue_did_not_mention_is_reported_but_not_fatal() {
        let r = reconcile(
            &expect(vec![], vec!["c-7"]),
            &snapshot(vec![], vec![]),
            Tolerance::default(),
        );
        assert_eq!(r.divergences.len(), 1);
        assert!(matches!(r.divergences[0], Divergence::OrderStale { .. }));
        assert!(!r.is_fatal(), "an unmentioned order is not a corruption");
    }

    /// Both legs are compared. A netted comparison would report
    /// agreement here — long 0.2 against short 0.2 nets to zero on both
    /// sides — while the venue holds margin for two legs and we believe
    /// in one.
    #[test]
    fn both_legs_are_compared_rather_than_the_net() {
        let r = reconcile(
            &expect(vec![want("LONG", 0.2, 60_000.0)], vec![]),
            &snapshot(
                vec![leg("LONG", 0.2, 60_000.0), leg("SHORT", -0.2, 60_000.0)],
                vec![],
            ),
            Tolerance::default(),
        );
        assert!(r.is_fatal());
        assert!(matches!(
            r.divergences[0],
            Divergence::PositionUnknownLocally { .. }
        ));
    }

    /// A value off the lot grid means the assumed lot size is wrong, so
    /// every comparison made through it is meaningless. Caught as itself
    /// rather than as a shower of tiny position differences.
    #[test]
    fn an_off_grid_quantity_is_reported_as_such() {
        let tol = Tolerance {
            lot: 0.001,
            ..Tolerance::default()
        };
        let r = reconcile(
            &expect(vec![], vec![]),
            &snapshot(vec![leg("LONG", 0.000_25, 60_000.0)], vec![]),
            tol,
        );
        assert!(
            r.divergences
                .iter()
                .any(|d| matches!(d, Divergence::OffGrid { .. })),
            "{}",
            r.render()
        );
        assert!(r.is_fatal());
    }

    #[test]
    fn a_quantity_on_the_grid_passes_the_grid_check() {
        let tol = Tolerance {
            lot: 0.001,
            ..Tolerance::default()
        };
        let r = reconcile(
            &expect(vec![want("LONG", 0.256, 60_000.0)], vec![]),
            &snapshot(vec![leg("LONG", 0.256, 60_000.0)], vec![]),
            tol,
        );
        assert!(r.agrees(), "{}", r.render());
    }

    /// The report says explicitly that nothing was changed. A reader
    /// finding a discrepancy needs to know whether they are looking at
    /// the problem or at its aftermath.
    #[test]
    fn a_fatal_report_states_that_nothing_was_adjusted() {
        let r = reconcile(
            &expect(vec![want("LONG", 0.256, 71_444.87)], vec![]),
            &snapshot(vec![leg("LONG", 0.128, 71_444.87)], vec![]),
            Tolerance::default(),
        );
        let text = r.render();
        assert!(text.contains("FATAL"));
        assert!(text.contains("Nothing has been adjusted"));
    }

    /// Every rendered line ends in a newline. A quiet account produces
    /// one of these per read for hours, and without the terminator they
    /// concatenate into a single unreadable line — observed on the first
    /// live run.
    #[test]
    fn an_agreement_is_newline_terminated_like_everything_else() {
        let r = reconcile(
            &expect(vec![want("LONG", 0.400, 30_000.0)], vec![]),
            &snapshot(vec![leg("LONG", 0.400, 30_000.0)], vec![]),
            Tolerance::default(),
        );
        assert!(r.agrees());
        assert!(r.render().ends_with('\n'), "{:?}", r.render());
    }

    #[test]
    fn representation_noise_is_absorbed_and_disagreement_is_not() {
        let noise = reconcile(
            &expect(vec![want("LONG", 0.256, 71_444.87)], vec![]),
            &snapshot(vec![leg("LONG", 0.256 + 1e-12, 71_444.87)], vec![]),
            Tolerance::default(),
        );
        assert!(noise.agrees());

        let real = reconcile(
            &expect(vec![want("LONG", 0.256, 71_444.87)], vec![]),
            &snapshot(vec![leg("LONG", 0.257, 71_444.87)], vec![]),
            Tolerance::default(),
        );
        assert!(real.is_fatal());
    }
}
