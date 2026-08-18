//! The consistency metric, tested against the mistakes it exists to
//! catch.
//!
//! A metric that only ever reports agreement is decoration. Each test
//! here writes a *plausible* offline implementation — the kind someone
//! writes when the derived path is too slow — makes one of the classic
//! errors, and checks that the metric names it.

use oq_engine::Tick;
use oq_features::builtin::MidReturn;
use oq_features::{Feature, consistency, offline};
use oq_strategy::indicator::{Ema, Warmup};
use oq_types::{Nanos, PriceTicks, QtyLots, Stamp};

const PERIOD: usize = 12;

/// A market with a moving mid and a few ticks where the book is empty,
/// because a feature only tested on a well-formed book is only tested on
/// the easy half of production.
fn ticks(n: usize) -> Vec<Tick> {
    (0..n)
        .map(|i| {
            let i = i as i64;
            let mid = 6_000_000 + i * 3 + ((i * 7) % 40 - 20);
            let (bid, ask) = if i % 37 == 36 {
                (0, 0)
            } else {
                (mid - 2, mid + 2)
            };
            Tick {
                stamp: Stamp {
                    exch: Nanos(1_700_000_000_000_000_000 + i * 1_000_000),
                    local: Nanos(1_700_000_000_000_000_000 + i * 1_000_000 + 90_000),
                },
                last: PriceTicks(mid),
                high: PriceTicks(mid + 5),
                low: PriceTicks(mid - 5),
                bid: PriceTicks(bid),
                ask: PriceTicks(ask),
                volume: QtyLots(i),
            }
        })
        .collect()
}

/// The derived offline path — the reference every test compares against.
fn reference(series: &[Tick]) -> Vec<Option<f64>> {
    offline(&mut MidReturn::new(PERIOD), series)
}

/// The baseline: a feature run twice is the same feature. If this failed
/// nothing else in the file would mean anything.
#[test]
fn the_derived_path_is_deterministic() {
    let series = ticks(500);
    let c = consistency("mid_return", &reference(&series), &reference(&series));
    assert!(c.agree(0.0), "{}", c.summary_line());
    assert_eq!(c.first_divergence, None);
    assert_eq!(c.compared, 500);
}

/// Mistake 1: normalising over the whole series.
///
/// The most common and the most damaging, because it is invisible in the
/// output — the numbers look like a feature, they are just a feature
/// computed with knowledge of the future. Its backtest is excellent.
#[test]
fn a_feature_normalised_over_the_whole_series_is_caught() {
    let series = ticks(500);
    let mut leaky = reference(&series);

    let values: Vec<f64> = leaky.iter().flatten().copied().collect();
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let sd = (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64).sqrt();
    for v in leaky.iter_mut().flatten() {
        *v = (*v - mean) / sd;
    }

    let c = consistency("mid_return", &reference(&series), &leaky);
    assert!(
        !c.agree(1e-9),
        "look-ahead normalisation must not pass: {}",
        c.summary_line()
    );
    assert_eq!(
        c.warmup_mismatches, 0,
        "this error changes values, not warm-up"
    );
    assert!(c.first_divergence.is_some());
}

/// Mistake 2: filling the warm-up with zeros.
///
/// `None` means "not defined yet"; a zero means "defined, and it is
/// zero". No tolerance reconciles them, which is why warm-up mismatches
/// are counted apart from numeric ones.
#[test]
fn filling_the_warmup_with_zeros_is_caught_at_any_tolerance() {
    let series = ticks(300);
    let filled: Vec<Option<f64>> = reference(&series)
        .into_iter()
        .map(|v| Some(v.unwrap_or(0.0)))
        .collect();

    let c = consistency("mid_return", &reference(&series), &filled);
    assert!(c.warmup_mismatches > 0, "{}", c.summary_line());
    assert!(
        !c.agree(f64::MAX),
        "a warm-up mismatch must fail even at an absurd tolerance: {}",
        c.summary_line()
    );
}

/// Mistake 3: shifting by one.
///
/// A vectorised `diff` lined up with the wrong row gives every value the
/// previous tick's answer. In backtest this is harmless or even
/// beneficial — the feature is merely stale — and in production it is a
/// different feature.
#[test]
fn an_off_by_one_shift_is_caught() {
    let series = ticks(400);
    let mut shifted = reference(&series);
    shifted.insert(0, None);
    shifted.pop();

    let c = consistency("mid_return", &reference(&series), &shifted);
    assert!(!c.agree(1e-12), "{}", c.summary_line());
    assert!(
        c.first_divergence.is_some_and(|i| i <= PERIOD + 2),
        "a shift should show up almost immediately, got {:?}",
        c.first_divergence
    );
}

/// Mistake 4: inventing a mid where the book had none.
///
/// The tempting fix for an empty book is to fall back to `last`. It
/// makes the series look complete and makes the feature quietly
/// different in thin markets, which is where it matters most.
#[test]
fn inventing_a_mid_for_an_empty_book_is_caught() {
    let series = ticks(400);

    struct Fallback {
        ema: Ema,
        previous: Option<f64>,
    }
    impl Feature for Fallback {
        fn name(&self) -> &str {
            "mid_return_fallback"
        }
        fn update(&mut self, tick: &Tick) -> Option<f64> {
            let mid = MidReturn::mid(tick).unwrap_or(tick.last.0 as f64);
            let previous = self.previous.replace(mid)?;
            self.ema.update((mid / previous).ln())
        }
    }

    let mut candidate = Fallback {
        ema: Ema::new(PERIOD, Warmup::SimpleAverage),
        previous: None,
    };
    let c = consistency(
        "mid_return",
        &reference(&series),
        &offline(&mut candidate, &series),
    );

    assert!(!c.agree(1e-9), "{}", c.summary_line());
    assert!(
        c.max_abs_diff > 0.0,
        "the fallback changes values, not only their presence: {}",
        c.summary_line()
    );
}

/// Two series of different lengths are not two computations of one
/// feature. Comparing their common prefix would produce a reassuring
/// number for a real defect, so nothing is compared at all.
#[test]
fn different_lengths_are_refused_rather_than_truncated() {
    let series = ticks(200);
    let full = reference(&series);
    let short = reference(&series[..150]);

    let c = consistency("mid_return", &full, &short);
    assert_eq!(c.length_mismatch, Some((200, 150)));
    assert_eq!(
        c.compared, 0,
        "nothing may be compared across a length mismatch"
    );
    assert!(!c.agree(f64::MAX));
    assert!(c.summary_line().contains("LENGTH MISMATCH"));
}

/// A NaN equals nothing, including itself, so a comparison written with
/// `==` would call two NaNs identical and a NaN against a number "within
/// tolerance". Both are wrong.
#[test]
fn a_nan_is_a_divergence_and_not_a_free_pass() {
    let c = consistency(
        "f",
        &[Some(1.0), Some(2.0), Some(3.0)],
        &[Some(1.0), Some(f64::NAN), Some(3.0)],
    );
    assert_eq!(c.first_divergence, Some(1));
    assert!(!c.agree(f64::MAX), "{}", c.summary_line());
}

/// The feature cannot see the future, and the way to check that is not
/// to read the code: truncate the input and confirm the prefix of the
/// output is unchanged. A feature that peeked ahead would answer
/// differently when there was nothing ahead to peek at.
#[test]
fn the_prefix_of_the_output_does_not_depend_on_what_comes_after() {
    let series = ticks(600);
    let whole = reference(&series);

    for cut in [50usize, 137, 400] {
        let prefix = reference(&series[..cut]);
        let c = consistency(format!("mid_return@{cut}"), &whole[..cut], &prefix);
        assert!(
            c.agree(0.0),
            "the first {cut} values changed when later ticks were removed: {}",
            c.summary_line()
        );
    }
}
