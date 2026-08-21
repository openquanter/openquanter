//! Order flow: which side crossed the spread, in sequence.
//!
//! The fifth stylized fact, and the one [`StylizedFacts`] leaves out
//! because it needs something a return series does not carry. A price
//! path says what happened; the order flow says who made it happen.
//!
//! # The fact
//!
//! **Signed order flow is strongly and persistently autocorrelated.**
//! Returns are not — a market where they were would be one an arbitrage
//! had already removed — but the flow that produces them is, with
//! coefficients around 0.2 at lag 1 and decaying slowly enough to stay
//! positive over hundreds of trades.
//!
//! The reason is mechanical rather than behavioural: an order too large
//! to trade at once is worked in pieces, and every piece hits the same
//! side. What looks like persistent conviction is one participant's
//! single decision, spread over time.
//!
//! # Why it matters here rather than as trivia
//!
//! It is the assumption under every queue model. A queue that depletes
//! against *independent* arrivals empties at a rate that has nothing to
//! do with how a real one empties, because a real one faces runs of
//! same-side trades. L2's queue depletes against the trades that
//! actually arrived, which is why it does not need this to be modelled
//! — but a probabilistic queue model would need exactly this number,
//! which is why the roadmap orders that after calibration rather than
//! before.
//!
//! # A trade is not an order, and the difference is the whole number
//!
//! The literature's coefficient is over **orders**. A public feed
//! carries **trades**, and one order that crosses several resting ones
//! produces several trades — all on the same side, within milliseconds.
//!
//! Measured raw, that turns one decision into a run. On an hour of
//! captured BTCUSDT the longest same-side run of trades is 3,335, and
//! it spans **14 milliseconds**: not three thousand participants
//! agreeing, one participant's order eating three thousand resting
//! ones. The lag-1 coefficient over raw trades is 0.83, four times what
//! the literature reports, and the gap is entirely this.
//!
//! [`as_orders`] collapses consecutive same-side trades that share a
//! timestamp into one event, which is the closest a feed without
//! participant identifiers gets to the quantity the literature means.
//! Both are reported, because the raw series is the right input for a
//! queue — a queue faces every trade — and the collapsed one is the
//! right input for a comparison to published work.
//!
//! # What this does not do
//!
//! It does not decompose the autocorrelation into order splitting
//! versus herding. That distinction needs participant identifiers no
//! public feed carries, and the two produce the same sequence.

use crate::{Result, StatsError, autocorrelation};

/// Which side crossed the spread.
///
/// A local copy rather than `oq_types::Side`, because this crate depends
/// on nothing and is not about to start for two variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggressor {
    Buy,
    Sell,
}

impl Aggressor {
    /// `+1` for a buy, `-1` for a sell.
    #[must_use]
    pub const fn sign(self) -> f64 {
        match self {
            Self::Buy => 1.0,
            Self::Sell => -1.0,
        }
    }
}

/// Collapse consecutive same-side trades sharing a timestamp into one.
///
/// One order crossing several resting ones produces several trades, on
/// the same side and within the same millisecond. Counting them
/// separately reports one decision as a run, which is what inflates the
/// raw coefficient far past anything published.
///
/// This is an approximation and the direction of its error is known: it
/// merges genuinely separate orders that happened to land on the same
/// side in the same millisecond, so it **under**-counts events and the
/// coefficient it produces is a lower bound on the raw one. Exactness
/// needs participant identifiers, which no public feed carries.
///
/// `stamps` is in whatever unit the venue timestamps trades in; only
/// equality is used. A stamps slice shorter than the flow stops the
/// collapse where it runs out, rather than pairing a side with the
/// wrong time.
#[must_use]
pub fn as_orders(flow: &[Aggressor], stamps: &[i64]) -> Vec<Aggressor> {
    let mut out = Vec::new();
    let mut previous: Option<(i64, Aggressor)> = None;
    for (side, stamp) in flow.iter().zip(stamps) {
        let here = (*stamp, *side);
        if previous != Some(here) {
            out.push(*side);
            previous = Some(here);
        }
    }
    out
}

/// What a sequence of aggressors looks like.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderFlow {
    /// Trades measured.
    pub n: usize,
    /// Share that were buys, in `0.0..=1.0`.
    ///
    /// Near a half in any market with two sides. Far from it means the
    /// window caught a one-directional episode, which is worth seeing
    /// beside the autocorrelation because a lopsided sample inflates it.
    pub buy_share: f64,
    /// Autocorrelation of the signed series at each lag from 1.
    ///
    /// Reported as a curve rather than one number because the fact is
    /// about the *decay*: a single lag-1 coefficient is also what a
    /// two-trade alternation would produce, and those are opposite
    /// markets.
    pub acf: Vec<f64>,
    /// The longest run of same-side trades.
    ///
    /// The blunt version of the same fact, and the one a queue model is
    /// actually exposed to: a queue faces the run, not the average.
    pub longest_run: usize,
}

impl OrderFlow {
    /// Measure a sequence of aggressors.
    ///
    /// # Errors
    /// [`StatsError::TooFewObservations`] below `lags + 2` trades, since
    /// the last lag would otherwise be computed from one pair and
    /// reported beside coefficients computed from thousands.
    pub fn measure(flow: &[Aggressor], lags: usize) -> Result<Self> {
        let need = lags + 2;
        if flow.len() < need {
            return Err(StatsError::TooFewObservations {
                need,
                got: flow.len(),
            });
        }
        let signs: Vec<f64> = flow.iter().map(|a| a.sign()).collect();
        let buys = flow.iter().filter(|a| **a == Aggressor::Buy).count();

        let mut longest = 1;
        let mut current = 1;
        for w in flow.windows(2) {
            if w[0] == w[1] {
                current += 1;
                longest = longest.max(current);
            } else {
                current = 1;
            }
        }

        Ok(Self {
            n: flow.len(),
            #[allow(clippy::cast_precision_loss)]
            buy_share: buys as f64 / flow.len() as f64,
            // `None` at a lag means the series has no variance -- every
            // trade the same side -- and zero would read as "measured,
            // and there is no correlation", which is the opposite.
            acf: (1..=lags)
                .map(|k| autocorrelation(&signs, k).unwrap_or(f64::NAN))
                .collect(),
            longest_run: longest,
        })
    }

    /// Whether the flow persists: positive at lag 1 and still positive
    /// after decaying.
    ///
    /// Both halves, because either alone is met by a series that is not
    /// the fact. A single positive lag-1 is produced by any run at all;
    /// staying positive without starting high is noise around zero.
    #[must_use]
    pub fn persists(&self) -> bool {
        let first = self.acf.first().copied().unwrap_or(f64::NAN);
        let last = self.acf.last().copied().unwrap_or(f64::NAN);
        first > 0.1 && last > 0.0
    }

    /// One line per lag, for a report.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "order flow over {} trades, {:.1}% buys, longest run {}\n",
            self.n,
            100.0 * self.buy_share,
            self.longest_run
        );
        for (i, r) in self.acf.iter().enumerate() {
            out.push_str(&format!("  lag {:<5} {:+.4}\n", i + 1, r));
        }
        out.push_str(if self.persists() {
            "  persistent: holds\n"
        } else {
            "  persistent: absent\n"
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alternating(n: usize) -> Vec<Aggressor> {
        (0..n)
            .map(|i| {
                if i % 2 == 0 {
                    Aggressor::Buy
                } else {
                    Aggressor::Sell
                }
            })
            .collect()
    }

    fn runs(run: usize, count: usize) -> Vec<Aggressor> {
        let mut out = Vec::new();
        for i in 0..count {
            let side = if i % 2 == 0 {
                Aggressor::Buy
            } else {
                Aggressor::Sell
            };
            out.extend(std::iter::repeat_n(side, run));
        }
        out
    }

    /// A market that alternates is the opposite of a persistent one, and
    /// the lag-1 coefficient says so with a sign.
    #[test]
    fn alternating_flow_is_negatively_autocorrelated() {
        let f = OrderFlow::measure(&alternating(200), 5).expect("enough");
        assert!(f.acf[0] < -0.9, "lag 1 was {}", f.acf[0]);
        assert_eq!(f.longest_run, 1);
        assert!(!f.persists());
    }

    /// Flow worked in pieces is what the fact describes.
    #[test]
    fn flow_in_runs_is_positively_autocorrelated_and_decays() {
        let f = OrderFlow::measure(&runs(20, 20), 10).expect("enough");
        assert!(f.acf[0] > 0.8, "lag 1 was {}", f.acf[0]);
        assert!(f.acf[0] > f.acf[9], "it must decay");
        assert_eq!(f.longest_run, 20);
        assert!(f.persists());
    }

    /// Both halves of the verdict are required.
    ///
    /// A high lag-1 that has fallen through zero by the last lag is a
    /// short cycle, not persistence. Three-trade runs produce exactly
    /// that: neighbours mostly match, and trades three apart never do.
    ///
    /// Two-trade runs would not have worked as the fixture — at lag 1
    /// they are half matched and half not, so the coefficient sits at
    /// zero rather than high, which tests the other half of the
    /// verdict.
    #[test]
    fn a_short_cycle_is_not_persistence() {
        let f = OrderFlow::measure(&runs(3, 60), 3).expect("enough");
        assert!(f.acf[0] > 0.1, "neighbours mostly match: {:?}", f.acf);
        assert!(f.acf[2] < 0.0, "three apart never do: {:?}", f.acf);
        assert!(!f.persists(), "so it is a cycle, not persistence");
    }

    /// A one-sided window has no variance, so there is no coefficient.
    ///
    /// Reported as `NaN` rather than zero: zero reads as "measured, and
    /// there is no correlation", which is the opposite of "there was
    /// nothing to measure".
    #[test]
    fn a_one_sided_window_has_no_coefficient() {
        let f = OrderFlow::measure(&[Aggressor::Buy; 50], 3).expect("enough");
        assert!(f.acf.iter().all(|r| r.is_nan()));
        assert_eq!(f.buy_share, 1.0);
        assert_eq!(f.longest_run, 50);
        assert!(!f.persists());
    }

    /// One order crossing many resting ones is one event, not many.
    #[test]
    fn same_side_trades_in_one_millisecond_collapse_to_one_order() {
        let flow = vec![Aggressor::Buy; 100];
        let stamps = vec![7_i64; 100];
        assert_eq!(as_orders(&flow, &stamps), vec![Aggressor::Buy]);
    }

    /// A side change is a new event even inside one millisecond, and so
    /// is the same side in a later one.
    #[test]
    fn a_side_change_or_a_new_timestamp_starts_a_new_order() {
        let flow = [
            Aggressor::Buy,
            Aggressor::Buy,
            Aggressor::Sell,
            Aggressor::Buy,
        ];
        assert_eq!(as_orders(&flow, &[1, 1, 1, 1]).len(), 3, "side changes");
        assert_eq!(as_orders(&flow, &[1, 2, 3, 4]).len(), 4, "new stamps");
    }

    /// **The measurement this exists to correct.**
    ///
    /// A hundred trades from one order, alternating with single trades
    /// from others, reads as overwhelming persistence raw and as
    /// alternation once collapsed. Same market, and only the second is
    /// comparable to anything published.
    #[test]
    fn collapsing_changes_the_verdict_on_a_split_order() {
        let mut flow = Vec::new();
        let mut stamps = Vec::new();
        for i in 0..40_i64 {
            let side = if i % 2 == 0 {
                Aggressor::Buy
            } else {
                Aggressor::Sell
            };
            // One order, filled against a hundred resting orders, in one
            // millisecond.
            for _ in 0..100 {
                flow.push(side);
                stamps.push(i);
            }
        }

        let raw = OrderFlow::measure(&flow, 5).expect("enough");
        assert!(raw.acf[0] > 0.9, "raw reads as near-total persistence");
        assert_eq!(raw.longest_run, 100);

        let collapsed = as_orders(&flow, &stamps);
        assert_eq!(collapsed.len(), 40, "forty orders, not four thousand");
        let orders = OrderFlow::measure(&collapsed, 5).expect("enough");
        assert!(orders.acf[0] < -0.9, "which is alternation, the opposite");
    }

    /// A stamps slice that runs out stops the collapse rather than
    /// pairing a side with someone else's time.
    #[test]
    fn a_short_stamps_slice_truncates_rather_than_misaligns() {
        let flow = vec![Aggressor::Buy; 10];
        assert_eq!(as_orders(&flow, &[1, 1, 1]), [Aggressor::Buy]);
        assert!(as_orders(&flow, &[]).is_empty());
    }

    /// The last lag has to be computed from more than one pair.
    #[test]
    fn too_few_trades_for_the_lags_asked_for_is_refused() {
        assert!(OrderFlow::measure(&alternating(6), 5).is_err());
        assert!(OrderFlow::measure(&alternating(7), 5).is_ok());
    }
}
