//! Sensitivity to pre-evaluation history, over one common window.
//!
//! Each path is fresh. History callbacks receive only past observations;
//! evaluation callbacks receive each observation once in order. Undefined
//! values and decisions are counted, never replaced with zero or hold.

use oq_engine::Tick;

use crate::{Consistency, Feature, consistency};

/// A diagnostic path may wrap a feature and a stateful decision policy.
/// History updates must use the policy's history callback, not execute
/// hypothetical orders. Decisions should be canonical meaningful intents.
pub trait WarmupPath {
    type Decision: Eq;
    fn name(&self) -> &str;
    fn on_history(&mut self, tick: &Tick);
    /// `None` decision means undefined; a defined no-action decision must
    /// be represented as `Some` (for example, an empty intent vector).
    fn on_evaluation(&mut self, tick: &Tick) -> (Option<f64>, Option<Self::Decision>);
}

/// Feature-only warm-up with a decision function evaluated on the common
/// window. Policies needing history state implement `WarmupPath` directly.
pub struct FeatureDecisions<F, R, D> {
    feature: F,
    rule: R,
    decision: std::marker::PhantomData<D>,
}

impl<F, R, D> FeatureDecisions<F, R, D> {
    #[must_use]
    pub fn new(feature: F, rule: R) -> Self {
        Self {
            feature,
            rule,
            decision: std::marker::PhantomData,
        }
    }
}

impl<F: Feature, R, D: Eq> WarmupPath for FeatureDecisions<F, R, D>
where
    R: FnMut(&Tick, Option<f64>) -> Option<D>,
{
    type Decision = D;
    fn name(&self) -> &str {
        self.feature.name()
    }
    fn on_history(&mut self, tick: &Tick) {
        self.feature.update(tick);
    }
    fn on_evaluation(&mut self, tick: &Tick) -> (Option<f64>, Option<D>) {
        let value = self.feature.update(tick);
        (value, (self.rule)(tick, value))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WarmupConfig {
    /// Common half-open evaluation interval in the input observation axis.
    pub evaluation_start: usize,
    pub evaluation_end: usize,
    /// Distinct history lengths, each fully available before the start.
    pub warmup_ticks: Vec<usize>,
    pub tolerance: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionConsistency {
    pub compared: usize,
    pub defined_pairs: usize,
    pub changes: usize,
    pub undefined_reference: usize,
    pub undefined_candidate: usize,
    pub definedness_mismatches: usize,
    /// Relative to the common evaluation window; includes definedness.
    pub first_divergence: Option<usize>,
}

impl DecisionConsistency {
    #[must_use]
    pub fn complete_and_stable(&self) -> bool {
        self.compared > 0 && self.defined_pairs == self.compared && self.changes == 0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WarmupComparison {
    pub warmup_ticks: usize,
    pub feature: Consistency,
    pub numeric_pairs: usize,
    pub undefined_reference: usize,
    pub undefined_candidate: usize,
    pub nonfinite_reference: usize,
    pub nonfinite_candidate: usize,
    pub decisions: DecisionConsistency,
}

impl WarmupComparison {
    #[must_use]
    pub fn features_converged(&self, tolerance: f64) -> bool {
        tolerance.is_finite()
            && tolerance >= 0.0
            && self.feature.compared > 0
            && self.numeric_pairs == self.feature.compared
            && self.feature.agree(tolerance)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WarmupSensitivity {
    pub name: String,
    pub evaluation_start: usize,
    pub evaluation_end: usize,
    pub reference_warmup: usize,
    pub tolerance: f64,
    pub comparisons: Vec<WarmupComparison>,
}

impl WarmupSensitivity {
    /// Complete measurements and equal decisions for every shorter history.
    /// Indicator tolerance alone cannot certify decision stability.
    #[must_use]
    pub fn stable(&self) -> bool {
        !self.comparisons.is_empty()
            && self
                .comparisons
                .iter()
                .all(|c| c.features_converged(self.tolerance) && c.decisions.complete_and_stable())
    }

    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "{} warm-up sensitivity: evaluation [{}..{}), reference {} ticks, tolerance {:.3e}\n",
            self.name,
            self.evaluation_start,
            self.evaluation_end,
            self.reference_warmup,
            self.tolerance
        );
        for c in &self.comparisons {
            let diff = if c.numeric_pairs == 0 {
                "NOT MEASURED".into()
            } else {
                format!("{:.3e}", c.feature.max_abs_diff)
            };
            out.push_str(&format!("  warmup {}: numeric {}/{}, max |diff| {}, undefined ref/candidate {}/{}, nonfinite {}/{}, feature complete within tolerance {}\n", c.warmup_ticks, c.numeric_pairs, c.feature.compared, diff, c.undefined_reference, c.undefined_candidate, c.nonfinite_reference, c.nonfinite_candidate, c.features_converged(self.tolerance)));
            out.push_str(&format!("    decisions: defined {}/{}, changed {}, undefined {}/{}, definedness mismatches {}, first divergence {:?}, complete and stable {}\n", c.decisions.defined_pairs, c.decisions.compared, c.decisions.changes, c.decisions.undefined_reference, c.decisions.undefined_candidate, c.decisions.definedness_mismatches, c.decisions.first_divergence.map(|i| self.evaluation_start + i), c.decisions.complete_and_stable()));
        }
        out.push_str(&format!("  common-window stability: {}\n", self.stable()));
        out
    }
}

fn decisions<D: Eq>(reference: &[Option<D>], candidate: &[Option<D>]) -> DecisionConsistency {
    let mut result = DecisionConsistency {
        compared: reference.len(),
        defined_pairs: 0,
        changes: 0,
        undefined_reference: 0,
        undefined_candidate: 0,
        definedness_mismatches: 0,
        first_divergence: None,
    };
    for (i, (a, b)) in reference.iter().zip(candidate).enumerate() {
        result.undefined_reference += usize::from(a.is_none());
        result.undefined_candidate += usize::from(b.is_none());
        let differs = match (a, b) {
            (Some(a), Some(b)) => {
                result.defined_pairs += 1;
                result.changes += usize::from(a != b);
                a != b
            }
            (None, None) => false,
            _ => {
                result.definedness_mismatches += 1;
                true
            }
        };
        if differs && result.first_divergence.is_none() {
            result.first_divergence = Some(i);
        }
    }
    result
}

/// Compare each shorter warm-up against the longest on exactly the same
/// observation indices. Reject unavailable histories or empty windows;
/// silently clamping either would make unlike experiments look identical.
pub fn sensitivity<P: WarmupPath>(
    ticks: &[Tick],
    config: &WarmupConfig,
    factory: impl Fn() -> P,
) -> Result<WarmupSensitivity, String> {
    if config.evaluation_start >= config.evaluation_end
        || config.evaluation_end > ticks.len()
        || !config.tolerance.is_finite()
        || config.tolerance < 0.0
    {
        return Err("invalid common evaluation window or numeric tolerance".into());
    }
    let mut warmups = config.warmup_ticks.clone();
    warmups.sort_unstable();
    if warmups.len() < 2
        || warmups.windows(2).any(|pair| pair[0] == pair[1])
        || warmups
            .last()
            .is_some_and(|warmup| *warmup > config.evaluation_start)
    {
        return Err("warm-up lengths must be distinct, at least two, and fully available".into());
    }
    let reference_warmup = *warmups.last().expect("checked");
    let mut name: Option<String> = None;
    let mut runs = Vec::new();
    for warmup in warmups {
        let mut path = factory();
        if name.as_ref().is_some_and(|name| name != path.name()) {
            return Err("factory changed diagnostic path identity".into());
        }
        name.get_or_insert_with(|| path.name().to_string());
        for tick in &ticks[config.evaluation_start - warmup..config.evaluation_start] {
            path.on_history(tick);
        }
        let mut values = Vec::new();
        let mut choices = Vec::new();
        for tick in &ticks[config.evaluation_start..config.evaluation_end] {
            let (value, decision) = path.on_evaluation(tick);
            values.push(value);
            choices.push(decision);
        }
        runs.push((warmup, values, choices));
    }
    let (_, reference, choices) = runs.last().expect("checked");
    let mut comparisons = Vec::new();
    for (warmup_ticks, candidate, decisions_candidate) in &runs[..runs.len() - 1] {
        comparisons.push(WarmupComparison {
            warmup_ticks: *warmup_ticks,
            feature: consistency(name.clone().expect("checked"), reference, candidate),
            numeric_pairs: reference
                .iter()
                .zip(candidate)
                .filter(|(a, b)| a.is_some_and(f64::is_finite) && b.is_some_and(f64::is_finite))
                .count(),
            undefined_reference: reference.iter().filter(|value| value.is_none()).count(),
            undefined_candidate: candidate.iter().filter(|value| value.is_none()).count(),
            nonfinite_reference: reference
                .iter()
                .filter(|value| value.is_some_and(|value| !value.is_finite()))
                .count(),
            nonfinite_candidate: candidate
                .iter()
                .filter(|value| value.is_some_and(|value| !value.is_finite()))
                .count(),
            decisions: decisions(choices, decisions_candidate),
        });
    }
    Ok(WarmupSensitivity {
        name: name.expect("checked"),
        evaluation_start: config.evaluation_start,
        evaluation_end: config.evaluation_end,
        reference_warmup,
        tolerance: config.tolerance,
        comparisons,
    })
}
