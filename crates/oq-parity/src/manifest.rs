//! Run identity.
//!
//! A baseline pinned to a code commit alone expires silently: correct the
//! input data, move a configuration default, re-export a dataset, and the
//! code is untouched while the baseline is no longer describing the same
//! experiment. The next comparison then reports a mismatch that is not a
//! regression, and the reader has no way to tell "the engine changed"
//! from "the inputs changed".
//!
//! A run is therefore identified by three things — code, data, and
//! configuration — and any of them moving is a distinct, named outcome
//! rather than a difference to be explained away.
//!
//! See design decision D13 in `docs/IMPLEMENTATION.md`.

use crate::hash::sha256_hex;

/// The identity of a run: what code, over what data, under what settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunManifest {
    /// Commit of the code that produced the run.
    pub code_commit: String,
    /// Content hash of the input data.
    pub data_hash: String,
    /// Hash of the effective configuration, after defaults are applied.
    pub config_hash: String,
    /// Free-form label, e.g. the fidelity tier. Not part of identity.
    pub label: String,
}

/// One element of a run's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityElement {
    /// The code changed.
    CodeCommit,
    /// The input data changed.
    DataHash,
    /// The effective configuration changed.
    ConfigHash,
}

impl IdentityElement {
    /// What a reader should do about it.
    #[must_use]
    pub fn explanation(self) -> &'static str {
        match self {
            Self::CodeCommit => {
                "the code differs: differences may be a regression, or may be the intended change"
            }
            Self::DataHash => {
                "the input data differs: this baseline describes a different experiment and must be rebased"
            }
            Self::ConfigHash => {
                "the effective configuration differs: this baseline describes a different experiment and must be rebased"
            }
        }
    }
}

impl core::fmt::Display for IdentityElement {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = match self {
            Self::CodeCommit => "code commit",
            Self::DataHash => "input data hash",
            Self::ConfigHash => "configuration hash",
        };
        f.write_str(name)
    }
}

/// Whether a baseline still describes the same experiment as the run
/// under test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaselineStatus {
    /// Code, data and configuration all match: differences in the output
    /// are behavioral and worth investigating.
    Comparable,
    /// The code moved but data and configuration did not. Differences are
    /// attributable to the code change — which is exactly what a parity
    /// run wants to measure during a port or refactor.
    CodeChanged,
    /// Data or configuration moved. The baseline is stale, not violated;
    /// nothing about the engine can be concluded until it is rebased.
    Invalidated {
        /// Which elements changed.
        changed: Vec<IdentityElement>,
    },
}

impl BaselineStatus {
    /// Whether output differences may be interpreted as behavior.
    #[must_use]
    pub fn permits_behavioral_conclusions(&self) -> bool {
        matches!(self, Self::Comparable | Self::CodeChanged)
    }
}

impl RunManifest {
    /// Build a manifest, hashing the raw input data and configuration.
    #[must_use]
    pub fn from_content(
        code_commit: impl Into<String>,
        input_data: &[u8],
        config: &[u8],
        label: impl Into<String>,
    ) -> Self {
        Self {
            code_commit: code_commit.into(),
            data_hash: sha256_hex(input_data),
            config_hash: sha256_hex(config),
            label: label.into(),
        }
    }

    /// Compare this manifest, taken as the baseline, against the manifest
    /// of the run under test.
    #[must_use]
    pub fn compare(&self, candidate: &Self) -> BaselineStatus {
        let mut changed = Vec::new();
        if self.data_hash != candidate.data_hash {
            changed.push(IdentityElement::DataHash);
        }
        if self.config_hash != candidate.config_hash {
            changed.push(IdentityElement::ConfigHash);
        }

        if !changed.is_empty() {
            return BaselineStatus::Invalidated { changed };
        }
        if self.code_commit != candidate.code_commit {
            return BaselineStatus::CodeChanged;
        }
        BaselineStatus::Comparable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(commit: &str, data: &[u8], config: &[u8]) -> RunManifest {
        RunManifest::from_content(commit, data, config, "L0")
    }

    #[test]
    fn identical_inputs_are_comparable() {
        let a = manifest("abc123", b"ticks", b"settings");
        let b = manifest("abc123", b"ticks", b"settings");
        assert_eq!(a.compare(&b), BaselineStatus::Comparable);
        assert!(a.compare(&b).permits_behavioral_conclusions());
    }

    #[test]
    fn a_code_change_still_permits_conclusions() {
        let baseline = manifest("abc123", b"ticks", b"settings");
        let candidate = manifest("def456", b"ticks", b"settings");
        assert_eq!(baseline.compare(&candidate), BaselineStatus::CodeChanged);
        assert!(
            baseline
                .compare(&candidate)
                .permits_behavioral_conclusions(),
            "a port is exactly the case parity exists to measure"
        );
    }

    #[test]
    fn corrected_input_data_invalidates_the_baseline() {
        // The failure this whole module exists to prevent: the data was
        // fixed, the code was not touched, and the old baseline now
        // describes a different experiment.
        let baseline = manifest("abc123", b"ticks with a corrupt week", b"settings");
        let candidate = manifest("abc123", b"ticks, week repaired", b"settings");

        let status = baseline.compare(&candidate);
        assert_eq!(
            status,
            BaselineStatus::Invalidated {
                changed: vec![IdentityElement::DataHash]
            }
        );
        assert!(
            !status.permits_behavioral_conclusions(),
            "a stale baseline must not be read as a regression"
        );
    }

    #[test]
    fn a_configuration_change_invalidates_the_baseline() {
        let baseline = manifest("abc123", b"ticks", b"fees: 0.0004");
        let candidate = manifest("abc123", b"ticks", b"fees: 0.0002");
        assert_eq!(
            baseline.compare(&candidate),
            BaselineStatus::Invalidated {
                changed: vec![IdentityElement::ConfigHash]
            }
        );
    }

    #[test]
    fn every_changed_element_is_named() {
        let baseline = manifest("abc123", b"old data", b"old config");
        let candidate = manifest("def456", b"new data", b"new config");
        let BaselineStatus::Invalidated { changed } = baseline.compare(&candidate) else {
            panic!("expected the baseline to be invalidated");
        };
        assert_eq!(
            changed,
            vec![IdentityElement::DataHash, IdentityElement::ConfigHash]
        );
        for element in changed {
            assert!(!element.explanation().is_empty());
        }
    }

    #[test]
    fn the_label_is_not_part_of_identity() {
        let a = RunManifest::from_content("abc123", b"ticks", b"settings", "L0");
        let b = RunManifest::from_content("abc123", b"ticks", b"settings", "L0+margin");
        assert_eq!(a.compare(&b), BaselineStatus::Comparable);
    }
}
