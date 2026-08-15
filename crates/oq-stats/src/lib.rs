//! Statistics that make backtest results honest.
//!
//! Two families, both aimed at the same failure mode — a strategy that
//! looks good because it was selected from many, not because it works:
//!
//! - [`dsr`]: the probabilistic and deflated Sharpe ratios, which ask
//!   whether an observed Sharpe ratio survives correction for the number
//!   of trials that produced it, and for non-normal returns.
//! - [`pbo`]: the probability of backtest overfitting, estimated by
//!   combinatorially symmetric cross-validation, which asks whether the
//!   selection procedure itself generalizes.
//!
//! Neither statistic rescues a bad research process. They make the cost
//! of one visible.

pub mod dsr;
pub mod moments;
pub mod normal;
pub mod pbo;
pub mod trials;

pub use dsr::{deflated_sharpe_ratio, expected_max_sharpe, probabilistic_sharpe_ratio};
pub use moments::Moments;
pub use pbo::{PboReport, probability_of_backtest_overfitting};
pub use trials::TrialRegistry;

/// Errors returned by the statistics in this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatsError {
    /// Fewer observations than the statistic requires.
    TooFewObservations { got: usize, need: usize },
    /// The sample has no dispersion, so a Sharpe ratio is undefined.
    ZeroVariance,
    /// A performance matrix was ragged or empty.
    MalformedMatrix(&'static str),
    /// The number of CSCV splits must be even and at least 4.
    InvalidSplitCount { got: usize },
    /// An input was NaN or infinite.
    NotFinite(&'static str),
}

impl core::fmt::Display for StatsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooFewObservations { got, need } => {
                write!(f, "too few observations: got {got}, need at least {need}")
            }
            Self::ZeroVariance => write!(f, "sample has zero variance; Sharpe ratio is undefined"),
            Self::MalformedMatrix(why) => write!(f, "malformed performance matrix: {why}"),
            Self::InvalidSplitCount { got } => {
                write!(f, "split count must be even and at least 4, got {got}")
            }
            Self::NotFinite(what) => write!(f, "{what} must be finite"),
        }
    }
}

impl core::error::Error for StatsError {}

/// Result alias used throughout the crate.
pub type Result<T> = core::result::Result<T, StatsError>;
