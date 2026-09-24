//! Where the price went after each fill.
//!
//! A fill's markout at a horizon is how far the market moved in the
//! fill's favour by then: the price `h` after the fill against the fill
//! price, signed by the side, in basis points. A buy followed by a rise is
//! positive. A strategy whose fills are negative on average is being
//! filled by counterparties who know something it does not — adverse
//! selection — and a backtest whose fills are *less* negative than the
//! live run's is a backtest filling orders the market would not have.
//!
//! That comparison is what this is for, and it is a diagnostic rather
//! than an attribution cause: a markout gap says the two runs were
//! filled differently, not why, so it sits beside `attribution` rather
//! than inside it.
//!
//! Each fill counts once, whatever its size. A markout weighted by
//! quantity answers "where did the money go", which the P&L already
//! does; unweighted it answers "what happens after this strategy is
//! filled", which is the question the fill model is being checked on.
//!
//! Below [`MIN_SAMPLES`] fills a horizon is reported as too few to say
//! anything, rather than as a number: a mean over a handful of fills is
//! the noise of that handful.

use oq_types::{PriceTicks, Side};

use crate::record::{Fill, Nanos};

/// Fewer fills than this at a horizon and no distribution is reported.
pub const MIN_SAMPLES: usize = 30;

/// The horizons used when a caller names none: one second, ten, sixty.
pub const DEFAULT_HORIZONS: [Nanos; 3] = [
    Nanos(1_000_000_000),
    Nanos(10_000_000_000),
    Nanos(60_000_000_000),
];

/// One point of the price path: a traded price at an exchange time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub at: Nanos,
    pub price: PriceTicks,
}

/// Markouts at one horizon, in basis points of the fill price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Distribution {
    pub horizon: Nanos,
    pub samples: usize,
    pub mean_bps: f64,
    pub median_bps: f64,
    pub p10_bps: f64,
    pub p90_bps: f64,
    /// Share of fills the market moved against.
    pub adverse_share: f64,
}

/// What one horizon measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Markout {
    Measured(Distribution),
    /// Fewer than [`MIN_SAMPLES`] fills reached this horizon inside the
    /// price path.
    TooFew {
        horizon: Nanos,
        samples: usize,
    },
}

impl Markout {
    #[must_use]
    pub const fn horizon(&self) -> Nanos {
        match self {
            Self::Measured(d) => d.horizon,
            Self::TooFew { horizon, .. } => *horizon,
        }
    }
}

/// Markouts of `fills` at each of `horizons`, against `path`.
///
/// `path` is read once, in order, and must be in ascending time — as a
/// tick file is. The price at `t + h` is the last one at or before it,
/// and a fill whose horizon runs past the end of the path is left out of
/// that horizon rather than priced at wherever the data stopped.
pub fn markout<I>(fills: &[Fill], path: I, horizons: &[Nanos]) -> Vec<Markout>
where
    I: IntoIterator<Item = Point>,
{
    // Every (fill, horizon) as a time to price at, soonest first, so one
    // pass over the path resolves them all.
    let mut targets: Vec<(i64, usize, usize)> = fills
        .iter()
        .enumerate()
        .flat_map(|(f, fill)| {
            horizons
                .iter()
                .enumerate()
                .map(move |(h, horizon)| (fill.ts.0.saturating_add(horizon.0), f, h))
        })
        .collect();
    targets.sort_unstable();

    let mut values: Vec<Vec<f64>> = vec![Vec::new(); horizons.len()];
    let mut next = 0;
    let mut last: Option<PriceTicks> = None;
    for point in path {
        // Every target strictly before this point is priced by the one
        // before it; one at this very instant waits for it.
        while next < targets.len() && targets[next].0 < point.at.0 {
            let (_, f, h) = targets[next];
            if let Some(price) = last {
                values[h].push(bps(&fills[f], price));
            }
            next += 1;
        }
        last = Some(point.price);
        if next == targets.len() {
            break;
        }
    }

    horizons
        .iter()
        .zip(values)
        .map(|(&horizon, v)| summarise(horizon, v))
        .collect()
}

/// The move in the fill's favour, in basis points of its price.
fn bps(fill: &Fill, later: PriceTicks) -> f64 {
    let sign = match fill.side {
        Side::Buy => 1.0,
        Side::Sell => -1.0,
    };
    if fill.price.0 == 0 {
        return 0.0;
    }
    sign * (later.0 - fill.price.0) as f64 / fill.price.0 as f64 * 10_000.0
}

fn summarise(horizon: Nanos, mut v: Vec<f64>) -> Markout {
    if v.len() < MIN_SAMPLES {
        return Markout::TooFew {
            horizon,
            samples: v.len(),
        };
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    let rank = |q: f64| v[((q * (n - 1) as f64).round() as usize).min(n - 1)];
    Markout::Measured(Distribution {
        horizon,
        samples: n,
        mean_bps: v.iter().sum::<f64>() / n as f64,
        median_bps: rank(0.5),
        p10_bps: rank(0.1),
        p90_bps: rank(0.9),
        adverse_share: v.iter().filter(|x| **x < 0.0).count() as f64 / n as f64,
    })
}

/// How much better the second set's fills fared than the first's, per
/// horizon, in basis points — `None` where either had too few to say.
///
/// Called with a backtest first and a live run second, a negative number
/// is the size of the adverse selection the backtest did not model.
#[must_use]
pub fn contrast(first: &[Markout], second: &[Markout]) -> Vec<(Nanos, Option<f64>)> {
    first
        .iter()
        .map(|a| {
            let b = second.iter().find(|b| b.horizon() == a.horizon());
            let difference = match (a, b) {
                (Markout::Measured(a), Some(Markout::Measured(b))) => Some(b.mean_bps - a.mean_bps),
                _ => None,
            };
            (a.horizon(), difference)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    fn path(prices: &[(i64, i64)]) -> Vec<Point> {
        prices
            .iter()
            .map(|&(at, p)| Point {
                at: Nanos(at),
                price: PriceTicks(p),
            })
            .collect()
    }

    fn fill(ts: i64, side: Side, price: i64) -> Fill {
        Fill::new(ts, "X", side, price, 1)
    }

    /// A buy before a rise and a sell before a fall are both favourable,
    /// by the same amount.
    #[test]
    fn a_markout_is_signed_by_the_side() {
        let p = path(&[(0, 10_000), (S, 10_010), (2 * S, 10_000), (3 * S, 10_000)]);
        let buys: Vec<Fill> = (0..30).map(|_| fill(0, Side::Buy, 10_000)).collect();
        let [Markout::Measured(d)] = markout(&buys, p.clone(), &[Nanos(S)])[..] else {
            panic!("measured");
        };
        assert!((d.mean_bps - 10.0).abs() < 1e-9, "{d:?}");
        assert_eq!(d.adverse_share, 0.0);

        let sells: Vec<Fill> = (0..30).map(|_| fill(S, Side::Sell, 10_010)).collect();
        let [Markout::Measured(d)] = markout(&sells, p, &[Nanos(S)])[..] else {
            panic!("measured");
        };
        assert!((d.mean_bps - 9.99).abs() < 0.01, "{d:?}");
    }

    /// The price at the horizon is the last one at or before it, not the
    /// next one after.
    #[test]
    fn the_price_at_a_horizon_is_the_last_known_by_then() {
        let p = path(&[(0, 100), (S / 2, 110), (S, 120), (3 * S, 200)]);
        let fills: Vec<Fill> = (0..30).map(|_| fill(0, Side::Buy, 100)).collect();
        let out = markout(&fills, p, &[Nanos(S), Nanos(2 * S)]);
        let Markout::Measured(at_one) = out[0] else {
            panic!()
        };
        let Markout::Measured(at_two) = out[1] else {
            panic!()
        };
        assert!(
            (at_one.mean_bps - 2_000.0).abs() < 1e-9,
            "priced at 120: {at_one:?}"
        );
        assert!(
            (at_two.mean_bps - 2_000.0).abs() < 1e-9,
            "still 120 at 2s: {at_two:?}"
        );
    }

    /// A horizon past the end of the data is not priced at wherever the
    /// data stopped.
    #[test]
    fn a_horizon_past_the_data_is_left_out() {
        let p = path(&[(0, 100), (S, 101)]);
        let fills: Vec<Fill> = (0..40).map(|_| fill(0, Side::Buy, 100)).collect();
        let out = markout(&fills, p, &[Nanos(S / 2), Nanos(10 * S)]);
        assert!(matches!(out[0], Markout::Measured(d) if d.samples == 40));
        assert_eq!(
            out[1],
            Markout::TooFew {
                horizon: Nanos(10 * S),
                samples: 0
            }
        );
    }

    #[test]
    fn too_few_fills_is_not_a_number() {
        let p = path(&[(0, 100), (2 * S, 101)]);
        let fills: Vec<Fill> = (0..29).map(|_| fill(0, Side::Buy, 100)).collect();
        assert_eq!(
            markout(&fills, p, &[Nanos(S)]),
            vec![Markout::TooFew {
                horizon: Nanos(S),
                samples: 29
            }]
        );
    }

    /// The comparison the module exists for: live fills that fare worse
    /// than the backtest's are a negative difference.
    #[test]
    fn a_live_run_filled_worse_shows_as_a_negative_contrast() {
        let p = path(&[(0, 10_000), (S, 10_010), (2 * S, 10_020)]);
        let backtest: Vec<Fill> = (0..30).map(|_| fill(0, Side::Buy, 10_000)).collect();
        let live: Vec<Fill> = (0..30).map(|_| fill(0, Side::Buy, 10_010)).collect();
        let h = [Nanos(S)];
        let c = contrast(
            &markout(&backtest, p.clone(), &h),
            &markout(&live, p.clone(), &h),
        );
        let (_, Some(d)) = c[0] else { panic!("{c:?}") };
        assert!(d < -9.0, "{d}");
        let few = markout(&live[..3], p, &h);
        assert_eq!(
            contrast(&markout(&backtest, path(&[(0, 1), (2 * S, 1)]), &h), &few)[0].1,
            None
        );
    }
}
