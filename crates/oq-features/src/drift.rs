//! Watching a feature drift away from the data a model was fitted on.
//!
//! [`consistency`](crate::consistency) answers "do these two
//! computations of one feature agree?". This answers a different
//! question that is often confused with it: "is the feature being served
//! today drawn from the same distribution as the one the model learned
//! from?". Both implementations can be perfectly consistent while the
//! market moves underneath them, and a model served inputs it never saw
//! in training is wrong in a way no consistency check can see.
//!
//! # Why the bins are frozen
//!
//! The reference distribution is fitted once, from the data the model
//! was fitted on, and its bin edges never change. This is the one
//! decision that makes the rest work, and getting it wrong is the
//! standard way this measurement fails: bins re-derived from the current
//! window move with the data, so a distribution that has shifted bodily
//! re-bins itself and reports that nothing has happened. Frozen bins
//! cannot do that — a shift moves mass between fixed bins, which is
//! exactly what is to be detected.
//!
//! # Why the bins are quantiles
//!
//! Equal-width bins over a skewed feature — and most features worth
//! having are skewed — put nearly every observation in one bin, leaving
//! a statistic that can only see catastrophic change. Equal-frequency
//! bins from the reference spend resolution where the data actually is.
//! They also make empty reference bins rare, which matters because the
//! statistic divides by the reference proportion.
//!
//! # The three things reported separately
//!
//! A single number would hide the two failures that matter most.
//!
//! - **PSI** moves when mass moves between bins. It is the usual
//!   summary and it is reported, with the caveat below.
//! - **Values outside the reference range** are counted on their own.
//!   Binning puts them in the first or last bin, where they are
//!   indistinguishable from ordinary mass at the edge — and a feature
//!   producing values nothing in training resembled is the most
//!   alarming case there is, not one to be absorbed.
//! - **The undefined share** is counted on its own too. A feature that
//!   has stopped producing values does not move any bin, so PSI is
//!   blind to it, and "no value" reaching a model is a different
//!   failure from "an unusual value".
//!
//! # On the thresholds
//!
//! The conventional PSI readings — under 0.1 stable, 0.1 to 0.25 some
//! shift, over 0.25 significant — are convention. They come from credit
//! scoring practice, not from a derivation, and nothing about them is
//! calibrated to tick features. [`Drift::alarming`] takes the threshold
//! as an argument for that reason, and the default named here is a
//! starting point to be replaced by whatever a particular feature's
//! history shows, not a fact.

use crate::Feature;
use oq_engine::Tick;

/// The conventional "investigate this" reading, offered as a starting
/// point rather than a finding. See the note on thresholds above.
pub const CONVENTIONAL_PSI_THRESHOLD: f64 = 0.25;

/// Smallest reference proportion used in the ratio.
///
/// A reference bin with no observations in it would divide by zero, and
/// an empty bin that fills up later is real drift that must not become
/// an infinity. Flooring is a fudge and is named as one; quantile bins
/// make it rare, and it only bites on features with heavy ties.
const MIN_SHARE: f64 = 1e-6;

/// What went wrong while fitting a reference.
///
/// `PartialEq` but not `Eq`: one variant carries the offending value,
/// and that value is a float.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Fewer defined values than bins, so the quantiles are meaningless.
    TooFewValues { defined: usize, bins: usize },
    /// Zero or one bin was asked for.
    TooFewBins { bins: usize },
    /// Every defined value was identical, so there is nothing to bin.
    NoSpread { value: f64 },
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooFewValues { defined, bins } => {
                write!(f, "{defined} defined value(s) cannot describe {bins} bins")
            }
            Self::TooFewBins { bins } => {
                write!(f, "{bins} bin(s) measures nothing; use at least two")
            }
            Self::NoSpread { value } => {
                write!(
                    f,
                    "every reference value was {value}; there is no distribution to bin"
                )
            }
        }
    }
}

impl core::error::Error for Error {}

/// The distribution a model was fitted on, frozen.
#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    name: String,
    /// Interior bin edges, ascending. `bins() == edges.len() + 1`.
    edges: Vec<f64>,
    /// Share of reference observations in each bin, summing to 1.
    shares: Vec<f64>,
    /// Defined observations the reference was fitted from.
    defined: usize,
    /// Observations where the feature had no value.
    undefined: usize,
    /// The extremes actually seen, so later values can be called outside.
    low: f64,
    high: f64,
}

impl Reference {
    /// Fit a reference from the values a model was fitted on.
    ///
    /// `bins` is the number of equal-frequency buckets. Ten is the usual
    /// choice and is usual for the same reason most conventions are —
    /// it is not derived either.
    ///
    /// # Errors
    ///
    /// [`Error`] when there is not enough spread or not enough data to
    /// describe that many bins. Refused rather than degraded: a
    /// reference fitted from too little is a monitor that reports calm.
    pub fn fit(
        name: impl Into<String>,
        values: &[Option<f64>],
        bins: usize,
    ) -> Result<Self, Error> {
        if bins < 2 {
            return Err(Error::TooFewBins { bins });
        }
        let undefined = values.iter().filter(|v| v.is_none()).count();
        let mut defined: Vec<f64> = values.iter().filter_map(|v| *v).collect();
        if defined.len() < bins {
            return Err(Error::TooFewValues {
                defined: defined.len(),
                bins,
            });
        }
        // Total order, because feature values can be NaN and a partial
        // sort would leave them wherever they happened to land.
        defined.sort_by(f64::total_cmp);
        let low = defined[0];
        let high = defined[defined.len() - 1];
        if low == high {
            return Err(Error::NoSpread { value: low });
        }

        // Interior edges at the (i/bins) quantiles, deduplicated: a
        // feature with heavy ties produces repeated edges, and a bin
        // that cannot contain anything is worse than one fewer bin.
        let mut edges = Vec::with_capacity(bins - 1);
        for i in 1..bins {
            let at = (i * defined.len()) / bins;
            let e = defined[at.min(defined.len() - 1)];
            if edges.last().is_none_or(|last: &f64| *last < e) {
                edges.push(e);
            }
        }

        let mut counts = vec![0usize; edges.len() + 1];
        for v in &defined {
            counts[bin_of(&edges, *v)] += 1;
        }
        #[allow(clippy::cast_precision_loss)]
        let total = defined.len() as f64;
        #[allow(clippy::cast_precision_loss)]
        let shares = counts.iter().map(|c| *c as f64 / total).collect();

        Ok(Self {
            name: name.into(),
            edges,
            shares,
            defined: defined.len(),
            undefined,
            low,
            high,
        })
    }

    /// Fit a reference by running a feature over ticks.
    ///
    /// # Errors
    /// As [`Reference::fit`].
    pub fn from_feature<F: Feature + ?Sized>(
        feature: &mut F,
        ticks: &[Tick],
        bins: usize,
    ) -> Result<Self, Error> {
        let name = feature.name().to_owned();
        Self::fit(name, &crate::offline(feature, ticks), bins)
    }

    /// What this reference is a distribution of.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How many bins it ended up with, after deduplicating tied edges.
    #[must_use]
    pub fn bins(&self) -> usize {
        self.shares.len()
    }

    /// Defined observations it was fitted from.
    #[must_use]
    pub const fn defined(&self) -> usize {
        self.defined
    }

    /// Share of the fitting observations where the feature had no value.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn undefined_share(&self) -> f64 {
        let total = self.defined + self.undefined;
        if total == 0 {
            0.0
        } else {
            self.undefined as f64 / total as f64
        }
    }

    /// Start watching live values against this reference.
    #[must_use]
    pub fn watch(&self) -> Monitor<'_> {
        Monitor {
            reference: self,
            counts: vec![0; self.shares.len()],
            defined: 0,
            undefined: 0,
            invalid: 0,
            below: 0,
            above: 0,
        }
    }
}

/// Which bin a value falls in, given interior edges.
///
/// Edges are upper-inclusive on the left side — a value equal to an edge
/// belongs to the lower bin — so that the reference's own quantile
/// values land where the fit counted them.
///
/// Callers must screen out NaN first. It is not screened here because
/// there is no bin it belongs in: every comparison against it is false,
/// so it would land in bin zero and be counted as an ordinary small
/// value. [`Monitor::observe`] counts it as the defect it is.
fn bin_of(edges: &[f64], v: f64) -> usize {
    debug_assert!(!v.is_nan(), "NaN has no bin; screen it before calling");
    edges.partition_point(|e| *e < v)
}

/// Live values, binned against a frozen reference.
///
/// Costs one comparison-per-edge and one increment per observation, so
/// it can run inside the loop that produces the feature rather than in a
/// batch job that notices tomorrow.
#[derive(Debug, Clone)]
pub struct Monitor<'a> {
    reference: &'a Reference,
    counts: Vec<usize>,
    defined: usize,
    undefined: usize,
    invalid: usize,
    below: usize,
    above: usize,
}

impl Monitor<'_> {
    /// Record one observation of the feature.
    pub fn observe(&mut self, value: Option<f64>) {
        let Some(v) = value else {
            self.undefined += 1;
            return;
        };
        // A NaN is not a value that drifted, it is a computation that
        // failed, and it has no bin: every comparison against it is
        // false, so binning it would file it as an ordinary small
        // number. Counted on its own and kept out of the index, so the
        // index stays a statement about values that exist.
        if v.is_nan() {
            self.invalid += 1;
            return;
        }
        self.defined += 1;
        if v < self.reference.low {
            self.below += 1;
        } else if v > self.reference.high {
            self.above += 1;
        }
        self.counts[bin_of(&self.reference.edges, v)] += 1;
    }

    /// Observations recorded, of every kind.
    #[must_use]
    pub const fn observed(&self) -> usize {
        self.defined + self.undefined + self.invalid
    }

    /// What the values seen so far say about the distribution.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn drift(&self) -> Drift {
        let total = self.defined as f64;
        let mut psi = 0.0;
        let mut worst = None;
        for (i, (count, reference_share)) in
            self.counts.iter().zip(&self.reference.shares).enumerate()
        {
            let live = if self.defined == 0 {
                0.0
            } else {
                *count as f64 / total
            };
            let r = reference_share.max(MIN_SHARE);
            let l = live.max(MIN_SHARE);
            let contribution = (l - r) * (l / r).ln();
            psi += contribution;
            if worst.is_none_or(|(_, w)| contribution > w) {
                worst = Some((i, contribution));
            }
        }

        let observed = self.observed();
        Drift {
            name: self.reference.name.clone(),
            psi,
            observed,
            defined: self.defined,
            reference_defined: self.reference.defined,
            undefined_share: if observed == 0 {
                0.0
            } else {
                self.undefined as f64 / observed as f64
            },
            reference_undefined_share: self.reference.undefined_share(),
            below: self.below,
            above: self.above,
            invalid: self.invalid,
            worst_bin: worst,
        }
    }
}

/// What the live distribution says about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Drift {
    /// The feature this describes.
    pub name: String,
    /// Population stability index against the frozen reference.
    pub psi: f64,
    /// Observations recorded, defined and undefined together.
    pub observed: usize,
    /// Of those, the ones that had a value.
    pub defined: usize,
    /// Defined observations the reference was fitted from, for judging
    /// whether the comparison has enough behind it to mean anything.
    pub reference_defined: usize,
    /// Share of live observations where the feature had no value.
    pub undefined_share: f64,
    /// The same share in the reference, since a feature may always have
    /// been undefined some of the time.
    pub reference_undefined_share: f64,
    /// Values below anything the reference saw.
    pub below: usize,
    /// Values above anything the reference saw.
    pub above: usize,
    /// Observations whose value was NaN — a failed computation rather
    /// than an unusual number, and kept out of the index for that
    /// reason.
    pub invalid: usize,
    /// The bin contributing most to the index, and by how much.
    pub worst_bin: Option<(usize, f64)>,
}

impl Drift {
    /// Values outside the range the reference ever saw.
    #[must_use]
    pub const fn outside(&self) -> usize {
        self.below + self.above
    }

    /// Whether this deserves attention, given a threshold and a minimum
    /// sample.
    ///
    /// Three conditions, deliberately not one number. The index catches
    /// mass moving between bins; the outside count catches values the
    /// reference never saw, which binning would otherwise absorb into an
    /// edge bin; and the undefined share catches a feature that has
    /// stopped answering, which moves no bin at all.
    ///
    /// `minimum` guards the whole thing: an index computed from a
    /// handful of observations is noise, and a monitor that cries on its
    /// first few is one nobody reads by the time it is right.
    #[must_use]
    pub fn alarming(&self, threshold: f64, minimum: usize) -> bool {
        if self.observed < minimum {
            return false;
        }
        self.psi > threshold
            || self.outside() > 0
            || self.invalid > 0
            || self.undefined_share > self.reference_undefined_share + 0.01
    }

    /// One line, for a log or an alert.
    #[must_use]
    pub fn summary_line(&self) -> String {
        format!(
            "{}: psi {:.4} over {} obs (reference {}), {} outside reference range, \
             {} invalid, undefined {:.2}% vs {:.2}%",
            self.name,
            self.psi,
            self.observed,
            self.reference_defined,
            self.outside(),
            self.invalid,
            100.0 * self.undefined_share,
            100.0 * self.reference_undefined_share,
        )
    }
}

#[cfg(test)]
mod tests;
