//! Every cent between a backtest and the live run, accounted for.
//!
//! This is the sentence `WHY.md` compresses the project into, and the
//! last line of the report is the one it calls the product:
//!
//! ```text
//! Backtest expected   +12,400
//! Live actual         +11,940
//! ──────────────────────────
//! Gap                    -460
//!   slippage             -148
//!   queue position       -112
//!   funding vs model      -96
//!   latency               -61
//!   fee tier              -22
//! ──────────────────────────
//!   unexplained residual  -21   ← this line is the product
//! ```
//!
//! `FR-ATTRIB-3` asks for the decomposition and `FR-ATTRIB-4` for that
//! last line, "in currency and as a share of P&L", with the requirement
//! that it **must never** be reported as zero when attribution simply
//! failed.
//!
//! # Two ways to build this wrong
//!
//! **Deriving the gap from the components.** If the gap is the sum of
//! what was explained, the residual is zero by construction and the
//! report is a lie that looks like an achievement. So the gap here comes
//! from two independent sources — the venue's realized P&L and the
//! kernel's — and the components are computed separately from
//! observations. The residual is what is left over, and a test
//! deliberately breaks a component to confirm the error lands in it
//! rather than being absorbed.
//!
//! **Double-counting latency inside slippage.** The price difference on
//! a matched fill has two causes that a naive decomposition charges
//! twice: the market moved while the order was in flight, and the order
//! paid worse than the price prevailing when it arrived. They are split
//! at the *execution reference price* — the prevailing price at the
//! moment the venue filled:
//!
//! ```text
//! venue_price - model_price
//!   = (venue_price - reference)   <- slippage: paid beyond the market
//!   + (reference  - model_price)  <- latency: the market moved meanwhile
//! ```
//!
//! Additive by construction, and neither term can silently contain the
//! other. When the reference price is not available **both** are
//! unavailable, rather than slippage quietly absorbing latency — which
//! would produce a plausible number for a quantity nobody measured.
//!
//! # What is a modelling choice, and is labelled as one
//!
//! Slippage, latency, funding and fees are differences between two
//! observed quantities. Queue position is not: it prices a fill that one
//! side made and the other did not, and pricing a trade that did not
//! happen requires deciding what it was worth. This values it at the
//! reference price — the instantaneous edge the fill captured or missed.
//! That is defensible and it is a choice, and
//! [`Component::is_observed`] says which components are which so a
//! reader can weigh them differently.

use oq_types::{Cash, Instrument, PriceTicks, QtyLots, Side};

use crate::manifest::RunManifest;

/// One named cause of the gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Component {
    /// Paid worse than the price prevailing when the order arrived.
    Slippage,
    /// A fill one side made and the other did not.
    QueuePosition,
    /// The market moved between the decision and the execution.
    Latency,
    /// Funding charged against funding modelled.
    Funding,
    /// Fees charged against fees modelled.
    FeeTier,
}

impl Component {
    /// The order the report prints them in.
    pub const ALL: [Self; 5] = [
        Self::Slippage,
        Self::QueuePosition,
        Self::Latency,
        Self::Funding,
        Self::FeeTier,
    ];

    /// Whether this is a difference between two observed quantities.
    ///
    /// `false` means the number required a decision about what something
    /// was worth. Both kinds belong in the report; conflating them does
    /// not.
    #[must_use]
    pub const fn is_observed(self) -> bool {
        !matches!(self, Self::QueuePosition)
    }

    /// The name the report prints.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Slippage => "slippage",
            Self::QueuePosition => "queue position",
            Self::Latency => "latency",
            Self::Funding => "funding vs model",
            Self::FeeTier => "fee tier",
        }
    }
}

impl core::fmt::Display for Component {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.label())
    }
}

/// A component's value, or the reason there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attributed {
    /// Computed, in account currency.
    Explained(Cash),
    /// Could not be computed, and why.
    ///
    /// Deliberately not `Cash(0)`. A cause that was not measured and a
    /// cause that measured zero are opposite facts, and `FR-ATTRIB-6`
    /// exists because collapsing them produces a report showing a gap
    /// fully explained by causes nobody looked at.
    Unavailable(String),
}

impl Attributed {
    /// The amount, when there is one.
    #[must_use]
    pub const fn amount(&self) -> Option<Cash> {
        match self {
            Self::Explained(c) => Some(*c),
            Self::Unavailable(_) => None,
        }
    }
}

/// One fill both sides made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Matched {
    /// Which way the order went.
    pub side: Side,
    /// Quantity, in lots.
    pub qty: QtyLots,
    /// Where the model filled it.
    pub model_price: PriceTicks,
    /// Where the venue filled it.
    pub venue_price: PriceTicks,
    /// The price prevailing at the venue when it filled.
    ///
    /// `None` makes both slippage and latency unavailable for the whole
    /// report, because without it the two cannot be separated and a
    /// number that silently contains both is worse than no number.
    pub reference_price: Option<PriceTicks>,
}

/// One fill only one side made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unmatched {
    /// Which way it went.
    pub side: Side,
    /// Quantity, in lots.
    pub qty: QtyLots,
    /// The price it filled at.
    pub price: PriceTicks,
    /// The price prevailing at that moment.
    pub reference_price: Option<PriceTicks>,
    /// Whether the venue made it (`true`) or only the model did.
    pub at_venue: bool,
}

/// Everything the decomposition is computed from.
///
/// Assembled by whatever watched the run — `oq_live::shadow` produces
/// the fill differences, the journal supplies the timestamps, and the
/// venue's own statements supply funding and fees.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Evidence {
    /// Fills both sides made.
    pub matched: Vec<Matched>,
    /// Fills only one side made.
    pub unmatched: Vec<Unmatched>,
    /// Funding the venue charged, and funding the model computed.
    pub funding: Option<(Cash, Cash)>,
    /// Fees the venue charged, and fees the model computed.
    pub fees: Option<(Cash, Cash)>,
}

/// The report.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribution {
    /// What the account actually made.
    pub live_pnl: Cash,
    /// What the same events through the kernel would have made.
    pub model_pnl: Cash,
    /// `live - model`, computed from those two and from nothing else.
    pub gap: Cash,
    /// Each cause, in report order.
    pub components: Vec<(Component, Attributed)>,
    /// What will not decompose.
    ///
    /// `None` when any component is unavailable. A residual computed
    /// against an incomplete decomposition is a number that looks like
    /// the product and is not one — the missing causes would arrive in
    /// it wearing the name "unexplained", which is exactly the claim it
    /// must not make.
    pub residual: Option<Cash>,
    /// The run this describes, so a third party can reproduce it.
    pub manifest: RunManifest,
}

impl Attribution {
    /// The residual as a share of the live result.
    ///
    /// `None` when there is no residual, or when the live result is
    /// zero — a share of nothing is not a large share, it is undefined.
    #[must_use]
    pub fn residual_share(&self) -> Option<f64> {
        let residual = self.residual?;
        if self.live_pnl.0 == 0 {
            return None;
        }
        Some(residual.0 as f64 / self.live_pnl.0.abs() as f64)
    }

    /// Whether every component was measured.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.components
            .iter()
            .all(|(_, a)| matches!(a, Attributed::Explained(_)))
    }

    /// Components that could not be measured, and why.
    #[must_use]
    pub fn unavailable(&self) -> Vec<(Component, &str)> {
        self.components
            .iter()
            .filter_map(|(c, a)| match a {
                Attributed::Unavailable(why) => Some((*c, why.as_str())),
                Attributed::Explained(_) => None,
            })
            .collect()
    }

    /// The report, as text.
    #[must_use]
    pub fn render(&self) -> String {
        use core::fmt::Write as _;
        // CASH_SCALE, as a float, once. Written out rather than
        // converted from a literal so the divisor is unambiguous.
        let money = |c: Cash| format!("{:+.2}", c.0 as f64 / 100_000_000.0);
        let mut out = String::new();
        let _ = writeln!(out, "run                 {}", self.manifest.label);
        let _ = writeln!(
            out,
            "code                {}",
            &self.manifest.code_commit[..12.min(self.manifest.code_commit.len())]
        );
        let _ = writeln!(out, "Backtest expected   {}", money(self.model_pnl));
        let _ = writeln!(out, "Live actual         {}", money(self.live_pnl));
        let _ = writeln!(out, "──────────────────────────");
        let _ = writeln!(out, "Gap                 {}", money(self.gap));
        for (component, value) in &self.components {
            match value {
                Attributed::Explained(c) => {
                    let _ = writeln!(
                        out,
                        "  {:<18}{}{}",
                        component.label(),
                        money(*c),
                        if component.is_observed() {
                            ""
                        } else {
                            "  (modelled)"
                        }
                    );
                }
                Attributed::Unavailable(why) => {
                    let _ = writeln!(out, "  {:<18}NOT MEASURED — {why}", component.label());
                }
            }
        }
        let _ = writeln!(out, "──────────────────────────");
        match self.residual {
            Some(r) => {
                let share = self
                    .residual_share()
                    .map_or_else(String::new, |s| format!("  ({:+.1}% of P&L)", s * 100.0));
                let _ = writeln!(out, "  unexplained residual {}{share}", money(r));
            }
            None => {
                let _ = writeln!(
                    out,
                    "  NO RESIDUAL: {} cause(s) were not measured, so the gap is not\n\
                     \x20              decomposed and a residual here would name the\n\
                     \x20              missing measurements as though they were unexplained.",
                    self.unavailable().len()
                );
            }
        }
        out
    }
}

/// Decompose the gap between a live run and the same events through the
/// kernel.
///
/// `live_pnl` and `model_pnl` must come from two independent sources —
/// the venue's own statement and the kernel's accounting. The gap is
/// their difference and nothing else; if it were the sum of the
/// components the residual would be zero by construction and the report
/// would be worthless.
#[must_use]
pub fn attribute(
    manifest: RunManifest,
    instrument: &Instrument,
    live_pnl: Cash,
    model_pnl: Cash,
    evidence: &Evidence,
) -> Attribution {
    let gap = Cash(live_pnl.0 - model_pnl.0);

    let Some(tick_cash) = instrument.tick_cash() else {
        // Without this there is no way to turn a tick difference into
        // money, so nothing derived from prices can be measured. Said
        // once, per component, rather than producing four zeroes.
        let why = "the instrument does not price a tick: contract size and scales \
                   give a tick worth less than the smallest cash unit";
        return Attribution {
            live_pnl,
            model_pnl,
            gap,
            components: Component::ALL
                .iter()
                .map(|c| {
                    let value = match c {
                        Component::Funding => cash_difference(evidence.funding, "funding"),
                        Component::FeeTier => cash_difference(evidence.fees, "fees"),
                        _ => Attributed::Unavailable(why.to_string()),
                    };
                    (*c, value)
                })
                .collect(),
            residual: None,
            manifest,
        };
    };

    // Slippage and latency are separated at the reference price, so
    // neither can contain the other. A single matched fill without one
    // makes both unavailable for the whole report: a partial sum would
    // be a number for a subset nobody chose.
    let missing_reference = evidence.matched.iter().any(|m| m.reference_price.is_none());

    let (slippage, latency) = if missing_reference {
        let why = "at least one matched fill has no prevailing price at execution, \
                   so slippage and latency cannot be separated";
        (
            Attributed::Unavailable(why.to_string()),
            Attributed::Unavailable(why.to_string()),
        )
    } else {
        let mut slip = 0i128;
        let mut lat = 0i128;
        for m in &evidence.matched {
            let reference = m.reference_price.unwrap_or(m.venue_price);
            // A buy paying more is a loss; a sell receiving more is a
            // gain. `sign` carries that and nothing else.
            let sign = i128::from(direction(m.side));
            let qty = i128::from(m.qty.0);
            slip += sign * i128::from(reference.0 - m.venue_price.0) * qty;
            lat += sign * i128::from(m.model_price.0 - reference.0) * qty;
        }
        (
            Attributed::Explained(to_cash(slip, tick_cash)),
            Attributed::Explained(to_cash(lat, tick_cash)),
        )
    };

    let queue = {
        let missing = evidence
            .unmatched
            .iter()
            .any(|u| u.reference_price.is_none());
        if missing {
            Attributed::Unavailable(
                "at least one unmatched fill has no prevailing price, so the trade \
                 that did not happen cannot be priced"
                    .to_string(),
            )
        } else {
            let mut total = 0i128;
            for u in &evidence.unmatched {
                let reference = u.reference_price.unwrap_or(u.price);
                let edge = i128::from(direction(u.side)) * i128::from(reference.0 - u.price.0);
                // A fill the venue made and the model did not is edge
                // the account captured and the model missed; the other
                // way round is edge the model claimed and the account
                // never had.
                let sign = if u.at_venue { 1 } else { -1 };
                total += sign * edge * i128::from(u.qty.0);
            }
            Attributed::Explained(to_cash(total, tick_cash))
        }
    };

    let components = vec![
        (Component::Slippage, slippage),
        (Component::QueuePosition, queue),
        (Component::Latency, latency),
        (
            Component::Funding,
            cash_difference(evidence.funding, "funding"),
        ),
        (Component::FeeTier, cash_difference(evidence.fees, "fees")),
    ];

    let complete = components
        .iter()
        .all(|(_, a)| matches!(a, Attributed::Explained(_)));
    let residual = complete.then(|| {
        let explained: i64 = components
            .iter()
            .filter_map(|(_, a)| a.amount())
            .map(|c| c.0)
            .sum();
        Cash(gap.0 - explained)
    });

    Attribution {
        live_pnl,
        model_pnl,
        gap,
        components,
        residual,
        manifest,
    }
}

/// A venue-versus-model pair, or the reason there is none.
fn cash_difference(pair: Option<(Cash, Cash)>, what: &str) -> Attributed {
    match pair {
        Some((venue, model)) => Attributed::Explained(Cash(venue.0 - model.0)),
        None => Attributed::Unavailable(format!(
            "no {what} was recorded for either side; a difference of zero would \
             claim they agreed"
        )),
    }
}

/// `+1` for a buy, `-1` for a sell.
const fn direction(side: Side) -> i64 {
    match side {
        Side::Buy => 1,
        Side::Sell => -1,
    }
}

/// Tick-lots into cash, saturating rather than wrapping.
fn to_cash(tick_lots: i128, tick_cash: i64) -> Cash {
    let v = tick_lots.saturating_mul(i128::from(tick_cash));
    Cash(i64::try_from(v).unwrap_or(if v.is_negative() { i64::MIN } else { i64::MAX }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One tick × one lot is worth exactly one cash unit, so every
    /// number below can be read without arithmetic. At these scales
    /// `tick_cash` reduces to `contract_size`, which is why it is 1.
    fn instrument() -> Instrument {
        Instrument {
            price_scale: 0,
            qty_scale: 0,
            contract_size: 1,
            price_tick: 1,
            qty_step: 1,
            min_notional: Cash(0),
        }
    }

    fn manifest() -> RunManifest {
        RunManifest::from_content("abc123", b"ticks", b"cfg", "session-1")
    }

    fn matched(side: Side, qty: i64, model: i64, venue: i64, reference: i64) -> Matched {
        Matched {
            side,
            qty: QtyLots(qty),
            model_price: PriceTicks(model),
            venue_price: PriceTicks(venue),
            reference_price: Some(PriceTicks(reference)),
        }
    }

    /// **The property the whole module rests on.** The gap comes from
    /// the two P&L figures and the components are computed separately,
    /// so a cause that is measured wrongly lands in the residual instead
    /// of being absorbed. If this ever failed, every residual this tool
    /// has ever printed would have been zero by construction.
    #[test]
    fn an_error_in_a_component_lands_in_the_residual() {
        let evidence = Evidence {
            // A buy that paid 10 ticks over the prevailing price.
            matched: vec![matched(Side::Buy, 1, 100, 110, 100)],
            funding: Some((Cash(0), Cash(0))),
            fees: Some((Cash(0), Cash(0))),
            ..Evidence::default()
        };
        // The venue says the account lost 40. Slippage explains 10 of
        // it. The other 30 is not explained by anything supplied.
        let a = attribute(manifest(), &instrument(), Cash(-40), Cash(0), &evidence);

        assert_eq!(a.gap, Cash(-40));
        assert_eq!(
            a.components[0],
            (Component::Slippage, Attributed::Explained(Cash(-10)))
        );
        assert_eq!(
            a.residual,
            Some(Cash(-30)),
            "the unexplained 30 must survive: {}",
            a.render()
        );
    }

    /// And when everything is explained, the residual is genuinely zero
    /// — which is a finding, not a default.
    #[test]
    fn a_fully_explained_gap_leaves_nothing() {
        let evidence = Evidence {
            matched: vec![matched(Side::Buy, 2, 100, 105, 100)],
            funding: Some((Cash(-7), Cash(0))),
            fees: Some((Cash(-3), Cash(-1))),
            ..Evidence::default()
        };
        // slippage -10, latency 0, funding -7, fees -2 => -19
        let a = attribute(manifest(), &instrument(), Cash(-19), Cash(0), &evidence);
        assert_eq!(a.residual, Some(Cash(0)), "{}", a.render());
        assert!(a.is_complete());
    }

    /// Latency and slippage split at the reference price and must sum to
    /// the whole price difference. A decomposition where they overlap
    /// charges the same money twice and still adds up, which is why this
    /// is asserted against the total rather than against each term.
    #[test]
    fn slippage_and_latency_sum_to_the_price_difference_and_do_not_overlap() {
        // Model filled at 100. By the time the venue matched, the market
        // was at 106 — six ticks of latency. It filled at 109, three
        // ticks worse than the market — three ticks of slippage.
        let evidence = Evidence {
            matched: vec![matched(Side::Buy, 1, 100, 109, 106)],
            funding: Some((Cash(0), Cash(0))),
            fees: Some((Cash(0), Cash(0))),
            ..Evidence::default()
        };
        let a = attribute(manifest(), &instrument(), Cash(-9), Cash(0), &evidence);

        let by = |c: Component| {
            a.components
                .iter()
                .find(|(k, _)| *k == c)
                .and_then(|(_, v)| v.amount())
                .expect("measured")
        };
        assert_eq!(by(Component::Slippage), Cash(-3), "{}", a.render());
        assert_eq!(by(Component::Latency), Cash(-6), "{}", a.render());
        assert_eq!(
            by(Component::Slippage).0 + by(Component::Latency).0,
            -9,
            "the two must account for the whole price difference"
        );
        assert_eq!(a.residual, Some(Cash(0)));
    }

    /// A sell is the mirror image: receiving less is the loss.
    #[test]
    fn the_sign_follows_the_side() {
        let evidence = Evidence {
            // Sold at 95 when the market was 100: five ticks worse.
            matched: vec![matched(Side::Sell, 1, 100, 95, 100)],
            funding: Some((Cash(0), Cash(0))),
            fees: Some((Cash(0), Cash(0))),
            ..Evidence::default()
        };
        let a = attribute(manifest(), &instrument(), Cash(-5), Cash(0), &evidence);
        assert_eq!(
            a.components[0],
            (Component::Slippage, Attributed::Explained(Cash(-5)))
        );
        assert_eq!(a.residual, Some(Cash(0)));
    }

    /// FR-ATTRIB-6. A cause nobody measured must not be reported as a
    /// cause that measured zero, and a residual computed against an
    /// incomplete decomposition would name the missing measurements as
    /// "unexplained" — which is precisely the claim it must not make.
    #[test]
    fn a_missing_measurement_produces_no_residual_rather_than_a_zero() {
        let evidence = Evidence {
            matched: vec![matched(Side::Buy, 1, 100, 110, 100)],
            // Nobody recorded funding.
            funding: None,
            fees: Some((Cash(0), Cash(0))),
            ..Evidence::default()
        };
        let a = attribute(manifest(), &instrument(), Cash(-40), Cash(0), &evidence);

        assert!(!a.is_complete());
        assert_eq!(a.residual, None, "{}", a.render());
        assert_eq!(a.residual_share(), None);
        let missing = a.unavailable();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].0, Component::Funding);
        assert!(
            a.render().contains("NO RESIDUAL"),
            "the report must say so loudly: {}",
            a.render()
        );
    }

    /// Without the prevailing price the two cannot be separated, and a
    /// slippage figure that silently contained the latency would be a
    /// plausible number for a quantity nobody measured. Both go
    /// unavailable together.
    #[test]
    fn a_missing_reference_price_disables_both_terms_not_one() {
        let evidence = Evidence {
            matched: vec![Matched {
                reference_price: None,
                ..matched(Side::Buy, 1, 100, 110, 100)
            }],
            funding: Some((Cash(0), Cash(0))),
            fees: Some((Cash(0), Cash(0))),
            ..Evidence::default()
        };
        let a = attribute(manifest(), &instrument(), Cash(-40), Cash(0), &evidence);
        let names: Vec<Component> = a.unavailable().into_iter().map(|(c, _)| c).collect();
        assert_eq!(names, vec![Component::Slippage, Component::Latency]);
        assert_eq!(a.residual, None);
    }

    /// A fill the venue made and the model did not is edge the account
    /// captured; the other way round is edge the model claimed and the
    /// account never had. The signs must be opposite or the component
    /// nets two real effects to nothing.
    #[test]
    fn the_two_directions_of_a_queue_difference_have_opposite_signs() {
        let one = |at_venue: bool| Evidence {
            unmatched: vec![Unmatched {
                side: Side::Buy,
                qty: QtyLots(1),
                // Bought at 95 when the market was 100: five ticks of edge.
                price: PriceTicks(95),
                reference_price: Some(PriceTicks(100)),
                at_venue,
            }],
            funding: Some((Cash(0), Cash(0))),
            fees: Some((Cash(0), Cash(0))),
            ..Evidence::default()
        };
        let venue = attribute(manifest(), &instrument(), Cash(0), Cash(0), &one(true));
        let model = attribute(manifest(), &instrument(), Cash(0), Cash(0), &one(false));

        let queue = |a: &Attribution| {
            a.components
                .iter()
                .find(|(k, _)| *k == Component::QueuePosition)
                .and_then(|(_, v)| v.amount())
                .expect("measured")
        };
        assert_eq!(queue(&venue), Cash(5));
        assert_eq!(queue(&model), Cash(-5));
    }

    /// Queue position prices a trade that did not happen, which is a
    /// decision rather than an observation. The report says which
    /// components are which, so a reader can weigh them differently.
    #[test]
    fn the_modelled_component_is_marked_as_one() {
        assert!(Component::Slippage.is_observed());
        assert!(Component::Latency.is_observed());
        assert!(Component::Funding.is_observed());
        assert!(Component::FeeTier.is_observed());
        assert!(!Component::QueuePosition.is_observed());

        let evidence = Evidence {
            unmatched: vec![Unmatched {
                side: Side::Buy,
                qty: QtyLots(1),
                price: PriceTicks(95),
                reference_price: Some(PriceTicks(100)),
                at_venue: true,
            }],
            funding: Some((Cash(0), Cash(0))),
            fees: Some((Cash(0), Cash(0))),
            ..Evidence::default()
        };
        let a = attribute(manifest(), &instrument(), Cash(5), Cash(0), &evidence);
        assert!(a.render().contains("(modelled)"), "{}", a.render());
    }

    /// An instrument that cannot price a tick makes every price-derived
    /// cause unmeasurable. Four zeroes would claim the prices agreed.
    #[test]
    fn an_instrument_that_cannot_price_a_tick_measures_nothing_derived_from_prices() {
        let mut i = instrument();
        // Nine decimal places on the price makes one tick worth less
        // than the smallest cash unit, so it cannot be priced at all.
        i.price_scale = 9;
        assert!(
            i.tick_cash().is_none(),
            "the fixture must actually be unpriceable"
        );

        let evidence = Evidence {
            matched: vec![matched(Side::Buy, 1, 100, 110, 100)],
            funding: Some((Cash(-5), Cash(0))),
            fees: Some((Cash(0), Cash(0))),
            ..Evidence::default()
        };
        let a = attribute(manifest(), &i, Cash(-40), Cash(0), &evidence);

        assert_eq!(a.residual, None);
        let names: Vec<Component> = a.unavailable().into_iter().map(|(c, _)| c).collect();
        assert_eq!(
            names,
            vec![
                Component::Slippage,
                Component::QueuePosition,
                Component::Latency
            ]
        );
        // Funding does not come from prices, so it survives.
        assert_eq!(
            a.components
                .iter()
                .find(|(k, _)| *k == Component::Funding)
                .and_then(|(_, v)| v.amount()),
            Some(Cash(-5))
        );
    }

    /// A share of nothing is undefined, not a large share.
    #[test]
    fn a_residual_against_zero_pnl_has_no_share() {
        let evidence = Evidence {
            funding: Some((Cash(-3), Cash(0))),
            fees: Some((Cash(0), Cash(0))),
            ..Evidence::default()
        };
        let a = attribute(manifest(), &instrument(), Cash(0), Cash(0), &evidence);
        assert_eq!(a.residual, Some(Cash(3)));
        assert_eq!(a.residual_share(), None);

        let b = attribute(manifest(), &instrument(), Cash(-100), Cash(0), &evidence);
        assert_eq!(b.residual, Some(Cash(-97)));
        let share = b.residual_share().expect("live P&L is non-zero");
        assert!((share + 0.97).abs() < 1e-9, "{share}");
    }

    /// The report binds to a manifest, so a third party can reproduce
    /// it on the same inputs. FR-ATTRIB-5.
    #[test]
    fn the_report_names_the_run_it_describes() {
        let a = attribute(
            manifest(),
            &instrument(),
            Cash(0),
            Cash(0),
            &Evidence {
                funding: Some((Cash(0), Cash(0))),
                fees: Some((Cash(0), Cash(0))),
                ..Evidence::default()
            },
        );
        assert_eq!(a.manifest.label, "session-1");
        assert!(a.render().contains("session-1"));
        assert!(a.render().contains("abc123"));
    }
}
