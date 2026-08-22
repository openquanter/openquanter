//! Fill-by-fill comparison with difference attribution.
//!
//! The output of a parity run has to answer one question: *what changed,
//! and where did it start?* A count of differing fills is close to
//! useless — a single early divergence cascades into thousands of
//! downstream differences, and the tail of that cascade tells you
//! nothing. So the report leads with the first divergence and treats
//! everything after it as consequence until the streams resynchronize.

use crate::manifest::{BaselineStatus, RunManifest};
use crate::record::{Fill, RunOutput};

/// How far ahead the aligner looks when trying to resynchronize after a
/// divergence. Beyond this the runs are treated as structurally
/// different rather than shifted.
const RESYNC_WINDOW: usize = 32;

/// A single difference between two runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Difference {
    /// The baseline produced a fill the candidate did not.
    Missing {
        /// Position in the baseline.
        index: usize,
        /// The fill that was not reproduced.
        fill: Fill,
    },
    /// The candidate produced a fill the baseline did not.
    Extra {
        /// Position in the candidate.
        index: usize,
        /// The unexpected fill.
        fill: Fill,
    },
    /// Both runs produced a fill at the same position, but they differ.
    Mismatch {
        /// Position in both streams.
        index: usize,
        /// Which fields differ, with both values.
        fields: Vec<FieldDifference>,
        /// The baseline fill.
        baseline: Fill,
        /// The candidate fill.
        candidate: Fill,
    },
}

impl Difference {
    /// Position in the stream where the difference occurs.
    #[must_use]
    pub fn index(&self) -> usize {
        match self {
            Self::Missing { index, .. }
            | Self::Extra { index, .. }
            | Self::Mismatch { index, .. } => *index,
        }
    }
}

/// One differing field of an otherwise aligned pair of fills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDifference {
    /// Field name, e.g. `price`.
    pub field: &'static str,
    /// Value in the baseline run.
    pub baseline: String,
    /// Value in the candidate run.
    pub candidate: String,
    /// Signed difference in the field's own units, when it is numeric.
    /// A one-tick price difference and a thousand-tick one are not the
    /// same finding, and the report should not flatten them.
    pub delta: Option<i64>,
}

/// The result of comparing two runs.
#[derive(Debug, Clone)]
pub struct ParityReport {
    /// Whether the baseline still describes the same experiment.
    pub baseline_status: BaselineStatus,
    /// Differences found, in stream order. Empty when the baseline is
    /// invalidated: nothing is concluded from a stale baseline.
    pub differences: Vec<Difference>,
    /// Index of the first divergence, if any.
    pub first_divergence: Option<usize>,
    /// Fills compared before the first divergence.
    pub matched_prefix: usize,
    /// Fill count in each run.
    pub fill_counts: (usize, usize),
    /// Realized P&L of each run.
    pub pnl: (f64, f64),
    /// Relative P&L error, or `None` when the baseline P&L is zero.
    pub pnl_relative_error: Option<f64>,
}

impl ParityReport {
    /// Whether the runs are semantically identical within `pnl_tolerance`.
    ///
    /// Always false for an invalidated baseline: "we cannot tell" is not
    /// the same answer as "they agree".
    #[must_use]
    pub fn passes(&self, pnl_tolerance: f64) -> bool {
        self.baseline_status.permits_behavioral_conclusions()
            && self.differences.is_empty()
            && self.pnl_relative_error.is_none_or(|e| e <= pnl_tolerance)
    }
}

/// Compare a candidate run against a baseline run.
///
/// When the baseline's data or configuration hash differs, the
/// comparison stops before it starts: the report says the baseline is
/// invalidated and names what moved, rather than producing differences
/// that would be read as a regression.
#[must_use]
pub fn compare(
    baseline_manifest: &RunManifest,
    baseline: &RunOutput,
    candidate_manifest: &RunManifest,
    candidate: &RunOutput,
) -> ParityReport {
    let baseline_status = baseline_manifest.compare(candidate_manifest);

    let pnl_relative_error = if baseline.pnl == 0.0 {
        None
    } else {
        Some(((candidate.pnl - baseline.pnl) / baseline.pnl).abs())
    };

    if !baseline_status.permits_behavioral_conclusions() {
        return ParityReport {
            baseline_status,
            differences: Vec::new(),
            first_divergence: None,
            matched_prefix: 0,
            fill_counts: (baseline.fills.len(), candidate.fills.len()),
            pnl: (baseline.pnl, candidate.pnl),
            pnl_relative_error,
        };
    }

    let differences = align(&baseline.fills, &candidate.fills);
    let first_divergence = differences.first().map(Difference::index);

    ParityReport {
        baseline_status,
        matched_prefix: first_divergence.unwrap_or(baseline.fills.len().min(candidate.fills.len())),
        first_divergence,
        differences,
        fill_counts: (baseline.fills.len(), candidate.fills.len()),
        pnl: (baseline.pnl, candidate.pnl),
        pnl_relative_error,
    }
}

/// Walk both streams, resynchronizing after insertions and deletions.
fn align(baseline: &[Fill], candidate: &[Fill]) -> Vec<Difference> {
    let mut differences = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);

    while i < baseline.len() && j < candidate.len() {
        if baseline[i].same_event(&candidate[j]) {
            i += 1;
            j += 1;
            continue;
        }

        // Does the baseline fill reappear later in the candidate? Then
        // the candidate inserted fills.
        if let Some(offset) = find_within(&baseline[i], &candidate[j..], RESYNC_WINDOW) {
            for k in 0..offset {
                differences.push(Difference::Extra {
                    index: j + k,
                    fill: candidate[j + k].clone(),
                });
            }
            j += offset;
            continue;
        }

        // Does the candidate fill appear later in the baseline? Then the
        // candidate dropped fills.
        if let Some(offset) = find_within(&candidate[j], &baseline[i..], RESYNC_WINDOW) {
            for k in 0..offset {
                differences.push(Difference::Missing {
                    index: i + k,
                    fill: baseline[i + k].clone(),
                });
            }
            i += offset;
            continue;
        }

        // Same position, different content.
        differences.push(Difference::Mismatch {
            index: i,
            fields: field_differences(&baseline[i], &candidate[j]),
            baseline: baseline[i].clone(),
            candidate: candidate[j].clone(),
        });
        i += 1;
        j += 1;
    }

    for (offset, fill) in baseline[i..].iter().enumerate() {
        differences.push(Difference::Missing {
            index: i + offset,
            fill: fill.clone(),
        });
    }
    for (offset, fill) in candidate[j..].iter().enumerate() {
        differences.push(Difference::Extra {
            index: j + offset,
            fill: fill.clone(),
        });
    }

    differences
}

fn find_within(needle: &Fill, haystack: &[Fill], window: usize) -> Option<usize> {
    haystack
        .iter()
        .take(window)
        .position(|fill| fill.same_event(needle))
        .filter(|offset| *offset > 0)
}

fn field_differences(baseline: &Fill, candidate: &Fill) -> Vec<FieldDifference> {
    let mut fields = Vec::new();

    if baseline.ts != candidate.ts {
        fields.push(FieldDifference {
            field: "ts",
            baseline: baseline.ts.0.to_string(),
            candidate: candidate.ts.0.to_string(),
            delta: Some(candidate.ts.0 - baseline.ts.0),
        });
    }
    if baseline.symbol != candidate.symbol {
        fields.push(FieldDifference {
            field: "symbol",
            baseline: baseline.symbol.clone(),
            candidate: candidate.symbol.clone(),
            delta: None,
        });
    }
    if baseline.side != candidate.side {
        fields.push(FieldDifference {
            field: "side",
            baseline: format!("{:?}", baseline.side),
            candidate: format!("{:?}", candidate.side),
            delta: None,
        });
    }
    if baseline.price != candidate.price {
        fields.push(FieldDifference {
            field: "price",
            baseline: baseline.price.0.to_string(),
            candidate: candidate.price.0.to_string(),
            delta: Some(candidate.price.0 - baseline.price.0),
        });
    }
    if baseline.qty != candidate.qty {
        fields.push(FieldDifference {
            field: "qty",
            baseline: baseline.qty.0.to_string(),
            candidate: candidate.qty.0.to_string(),
            delta: Some(candidate.qty.0 - baseline.qty.0),
        });
    }

    fields
}

impl core::fmt::Display for ParityReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.baseline_status {
            BaselineStatus::Invalidated { changed } => {
                writeln!(f, "BASELINE INVALIDATED - rebase required")?;
                for element in changed {
                    writeln!(f, "  {element} changed: {}", element.explanation())?;
                }
                writeln!(
                    f,
                    "  no comparison was made; a stale baseline cannot show a regression"
                )?;
                return Ok(());
            }
            BaselineStatus::CodeChanged => {
                writeln!(f, "baseline code commit differs (data and config match)")?;
            }
            BaselineStatus::Comparable => {}
        }

        writeln!(
            f,
            "fills: baseline {}, candidate {}",
            self.fill_counts.0, self.fill_counts.1
        )?;
        writeln!(
            f,
            "pnl:   baseline {:.8}, candidate {:.8}{}",
            self.pnl.0,
            self.pnl.1,
            match self.pnl_relative_error {
                Some(e) => format!(" (relative error {e:.3e})"),
                None => String::new(),
            }
        )?;

        if self.differences.is_empty() {
            writeln!(f, "no fill differences")?;
            return Ok(());
        }

        writeln!(
            f,
            "{} difference(s); first divergence at fill {} after {} matching fills",
            self.differences.len(),
            self.first_divergence.unwrap_or(0),
            self.matched_prefix
        )?;

        for difference in self.differences.iter().take(10) {
            match difference {
                Difference::Missing { index, fill } => {
                    writeln!(
                        f,
                        "  [{index}] missing: {} {:?} {} @ {}",
                        fill.symbol, fill.side, fill.qty.0, fill.price.0
                    )?;
                }
                Difference::Extra { index, fill } => {
                    writeln!(
                        f,
                        "  [{index}] extra:   {} {:?} {} @ {}",
                        fill.symbol, fill.side, fill.qty.0, fill.price.0
                    )?;
                }
                Difference::Mismatch { index, fields, .. } => {
                    write!(f, "  [{index}] differs:")?;
                    for field in fields {
                        write!(
                            f,
                            " {}={}->{}",
                            field.field, field.baseline, field.candidate
                        )?;
                        if let Some(delta) = field.delta {
                            write!(f, " ({delta:+})")?;
                        }
                    }
                    writeln!(f)?;
                }
            }
        }
        if self.differences.len() > 10 {
            writeln!(
                f,
                "  ... {} more; everything after the first divergence is likely consequence",
                self.differences.len() - 10
            )?;
        }

        Ok(())
    }
}
