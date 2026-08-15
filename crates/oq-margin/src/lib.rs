//! Margin, liquidation, and funding.
//!
//! A backtest that cannot liquidate you is not modelling the risk you
//! are actually taking. It is the single largest source of tail
//! optimism in leveraged strategy research: every path where the
//! account would have been closed out by the venue is scored as a
//! drawdown the strategy patiently survived, and the equity curve
//! reports a recovery that would never have happened. The higher the
//! leverage and the more the strategy adds to losing positions, the
//! more of the distribution this hides.
//!
//! This crate is deliberately an **orthogonal overlay**, not a field on
//! an account object. Any fidelity tier can run with it or without it,
//! and for a leveraged strategy `L0 + margin` catches more real risk
//! than `L2` without margin would. Execution realism and account
//! realism are different axes, and conflating them means a research
//! team has to buy both when they only need one.
//!
//! ## What is modelled
//!
//! - **Tiered maintenance margin.** Venues raise the maintenance rate
//!   as position notional grows, in brackets. The bracket boundaries
//!   matter: a position that is comfortable in one bracket can be
//!   liquidatable one lot into the next.
//! - **Liquidation price**, derived rather than approximated (see
//!   [`position`]).
//! - **Funding**, charged on the position at settlement instants, with
//!   a scenario hook for the spikes that only appear in the tail.
//!
//! ## Rules are bitemporal
//!
//! Venues change their margin tables. A backtest over 2024 must use the
//! 2024 table, and a backtest run today over 2024 must produce the same
//! answer as one run last year. [`schedule::TierSchedule`] therefore
//! keys tables by the date they took effect and resolves them by the
//! *event's* time rather than the run's. Silently applying today's
//! rules to old data is the same class of error as survivorship bias:
//! it makes the past look like the present in exactly the way that
//! flatters a strategy.

#![forbid(unsafe_code)]

pub mod funding;
pub mod position;
pub mod schedule;
pub mod tier;

pub use funding::{FundingRate, FundingSchedule, FundingSettlement};
pub use position::{LiquidationOutcome, MarginedPosition};
pub use schedule::TierSchedule;
pub use tier::{Contract, MarginTier, TierTable};
