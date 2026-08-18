//! One feature definition, two execution paths, and a number that says
//! whether they agree.
//!
//! # The failure this exists to prevent
//!
//! A feature is computed twice in the life of a strategy: over history,
//! to fit and evaluate; and tick by tick, in production. Almost nobody
//! computes it the same way both times. The offline path is written
//! against a whole array because that is fast and pleasant; the online
//! path is written against one observation at a time because that is all
//! there is. The two implementations then drift — a different warm-up
//! rule, an off-by-one in the window, a normalisation over the full
//! series — and the model is served inputs that do not resemble the ones
//! it was fitted on. The industry calls this training/serving skew. It
//! does not announce itself: the strategy simply underperforms its
//! backtest and nobody can say why.
//!
//! # The shape of the answer
//!
//! A feature here is defined **once**, as an online state machine
//! ([`Feature`]), and the offline path is *derived* from it by folding
//! ([`offline`]). Two paths, one definition, and no way for them to
//! disagree — which is the point, and is why [`Feature::update`] takes
//! one [`Tick`] and has no access to the series it came from. A feature
//! that cannot see the future cannot leak it.
//!
//! That constraint does not survive contact with reality on its own.
//! Sooner or later someone writes a vectorised offline implementation
//! because the derived one is too slow over ten years of ticks, and the
//! guarantee becomes a convention again. [`consistency`] exists for that
//! moment: it takes the two output series and reports where they first
//! part company and by how much, so the claim can be tested rather than
//! trusted.
//!
//! # What this skeleton is not
//!
//! It is not a feature store: no persistence, no registry, no
//! materialisation, no scheduling. It is not a drift monitor — comparing
//! a feature against *itself later* is a different problem from
//! comparing two implementations of it now. Those are M5. This is the
//! part that has to be right before any of them are worth building,
//! because a store that materialises a skewed feature has industrialised
//! the bug.

#![forbid(unsafe_code)]

pub mod builtin;

use oq_engine::Tick;

/// A feature, defined once, as something that consumes ticks in order.
///
/// The trait deliberately offers no batch method. A definition that
/// could see the whole series would be able to normalise over it,
/// look ahead, or centre on a future mean — each of which produces a
/// backtest that cannot be reproduced live, and none of which is
/// visible in the resulting numbers.
pub trait Feature {
    /// What this feature is called, for a consistency report to name.
    fn name(&self) -> &str;

    /// Consume one tick and return the feature's value *as of that
    /// tick*, or `None` while it is still warming up.
    ///
    /// `None` is not a missing value. It is the honest statement that
    /// the feature is not yet defined, and it must be propagated rather
    /// than filled: a warm-up period filled with zeros is a period of
    /// confident wrong answers.
    fn update(&mut self, tick: &Tick) -> Option<f64>;
}

/// Run a feature over a series, producing one value per tick.
///
/// This *is* the offline path. It is a fold over the online definition
/// rather than a second implementation of it, so the two agree by
/// construction rather than by discipline.
pub fn offline<F: Feature + ?Sized>(feature: &mut F, ticks: &[Tick]) -> Vec<Option<f64>> {
    ticks.iter().map(|t| feature.update(t)).collect()
}

/// Where two computations of the same feature part company.
#[derive(Debug, Clone, PartialEq)]
pub struct Consistency {
    /// The feature these series claim to be.
    pub name: String,
    /// Values compared. Zero when the series had different lengths.
    pub compared: usize,
    /// Index of the first disagreement, or `None` when there was none.
    pub first_divergence: Option<usize>,
    /// The largest absolute difference over the compared region.
    pub max_abs_diff: f64,
    /// Positions where one series had a value and the other did not.
    ///
    /// Counted separately from numeric differences because the cause is
    /// different and so is the fix: this is a warm-up disagreement, and
    /// no tolerance makes it go away.
    pub warmup_mismatches: usize,
    /// The two series were not the same length, so nothing was compared.
    ///
    /// Reported rather than truncating to the shorter: two feature
    /// series of different lengths are not two computations of one
    /// feature, and comparing their common prefix would produce a
    /// reassuring number for a real defect.
    pub length_mismatch: Option<(usize, usize)>,
}

impl Consistency {
    /// Whether the two paths agree within `tolerance`.
    ///
    /// A warm-up mismatch is never within tolerance, and neither is a
    /// length mismatch.
    #[must_use]
    pub fn agree(&self, tolerance: f64) -> bool {
        self.length_mismatch.is_none()
            && self.warmup_mismatches == 0
            && self.max_abs_diff <= tolerance
    }

    /// One line, for a log or a CI check.
    #[must_use]
    pub fn summary_line(&self) -> String {
        if let Some((a, b)) = self.length_mismatch {
            return format!(
                "{}: LENGTH MISMATCH {a} vs {b}; nothing compared",
                self.name
            );
        }
        match self.first_divergence {
            None => format!("{}: {} values, identical", self.name, self.compared),
            Some(i) => format!(
                "{}: {} values, first divergence at {i}, max |diff| {:.3e}, {} warm-up mismatch(es)",
                self.name, self.compared, self.max_abs_diff, self.warmup_mismatches
            ),
        }
    }
}

/// Compare two computations of one feature.
///
/// `reference` is the derived offline path; `candidate` is whatever was
/// written to replace it. The argument order matters only for reading
/// the report — the comparison is symmetric.
#[must_use]
pub fn consistency(
    name: impl Into<String>,
    reference: &[Option<f64>],
    candidate: &[Option<f64>],
) -> Consistency {
    let name = name.into();
    if reference.len() != candidate.len() {
        return Consistency {
            name,
            compared: 0,
            first_divergence: None,
            max_abs_diff: 0.0,
            warmup_mismatches: 0,
            length_mismatch: Some((reference.len(), candidate.len())),
        };
    }

    let mut first = None;
    let mut max_abs = 0.0f64;
    let mut warmup = 0usize;

    for (i, (r, c)) in reference.iter().zip(candidate).enumerate() {
        let differs = match (r, c) {
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => {
                warmup += 1;
                true
            }
            (Some(a), Some(b)) => {
                // NaN never equals itself, so a NaN on either side is a
                // divergence rather than a silent pass.
                let d = (a - b).abs();
                if d.is_nan() {
                    max_abs = f64::INFINITY;
                    true
                } else {
                    max_abs = max_abs.max(d);
                    d > 0.0
                }
            }
        };
        if differs && first.is_none() {
            first = Some(i);
        }
    }

    Consistency {
        name,
        compared: reference.len(),
        first_divergence: first,
        max_abs_diff: max_abs,
        warmup_mismatches: warmup,
        length_mismatch: None,
    }
}
