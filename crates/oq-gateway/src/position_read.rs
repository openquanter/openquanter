//! Explicit position coverage and read-cut comparability.
//!
//! Receipt bounds use the client's wall-clock domain, not the position's
//! last activity timestamp. A successfully read vector alone is not proof
//! that an absent leg is flat.

use crate::PositionSnapshot;

#[derive(Debug, Clone, PartialEq)]
pub struct PositionObservation {
    pub symbol: String,
    pub hedged: bool,
    /// Every expected leg, including an explicit zero for a flat leg.
    pub positions: Vec<PositionSnapshot>,
    pub requested_ms: i64,
    pub received_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionReadError {
    Scope,
    Mode,
    Coverage,
    InvalidValue,
    Clock,
    Stale,
    LocalChanged,
}

impl std::fmt::Display for PositionReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Scope => "position read has foreign symbol scope",
            Self::Mode => "position read mode differs from the local account mode",
            Self::Coverage => "position read lacks exactly one explicit row for every expected leg",
            Self::InvalidValue => "position read contains invalid size, entry or leg sign",
            Self::Clock => "position read receipt window is invalid or in the future",
            Self::Stale => "position read is stale at the comparison boundary",
            Self::LocalChanged => {
                "local executions changed during the position read; a new cut is required"
            }
        })
    }
}
impl std::error::Error for PositionReadError {}

impl PositionObservation {
    /// Validate a complete symbol/mode cut before a comparison. An accepted
    /// fill between request and comparison changes the revision even when
    /// a round trip left the same net position. No state is repaired.
    pub fn check(
        &self,
        symbol: &str,
        hedged: bool,
        evaluated_ms: i64,
        max_age_ms: i64,
        before_revision: usize,
        after_revision: usize,
    ) -> Result<(), PositionReadError> {
        if self.symbol != symbol || self.positions.iter().any(|p| p.symbol != symbol) {
            return Err(PositionReadError::Scope);
        }
        let mut sides: Vec<_> = self
            .positions
            .iter()
            .map(|p| p.position_side.as_str())
            .collect();
        sides.sort_unstable();
        if sides
            != if self.hedged {
                vec!["LONG", "SHORT"]
            } else {
                vec!["BOTH"]
            }
        {
            return Err(PositionReadError::Coverage);
        }
        if self.hedged != hedged {
            return Err(PositionReadError::Mode);
        }
        if self.positions.iter().any(|p| {
            !p.amount.is_finite()
                || !p.entry_price.is_finite()
                || p.entry_price < 0.0
                || (p.position_side == "LONG" && p.amount < 0.0)
                || (p.position_side == "SHORT" && p.amount > 0.0)
                || p.amount_text.parse::<f64>().ok() != Some(p.amount)
                || p.entry_text.parse::<f64>().ok() != Some(p.entry_price)
        }) {
            return Err(PositionReadError::InvalidValue);
        }
        if self.requested_ms < 0
            || self.received_ms < self.requested_ms
            || evaluated_ms < self.received_ms
            || max_age_ms < 0
        {
            return Err(PositionReadError::Clock);
        }
        if evaluated_ms - self.requested_ms > max_age_ms {
            return Err(PositionReadError::Stale);
        }
        if before_revision != after_revision {
            return Err(PositionReadError::LocalChanged);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(side: &str, amount: f64) -> PositionSnapshot {
        let entry = if amount == 0.0 { 0.0 } else { 100.0 };
        PositionSnapshot {
            symbol: "SYNTH".into(),
            position_side: side.into(),
            amount,
            entry_price: entry,
            unrealized: 0.0,
            amount_text: amount.to_string(),
            entry_text: entry.to_string(),
        }
    }
    fn observed(hedged: bool, positions: Vec<PositionSnapshot>) -> PositionObservation {
        PositionObservation {
            symbol: "SYNTH".into(),
            hedged,
            positions,
            requested_ms: 10,
            received_ms: 12,
        }
    }
    #[test]
    fn explicit_flat_is_comparable_but_empty_or_missing_is_unknown() {
        let flat = observed(false, vec![row("BOTH", 0.0)]);
        assert_eq!(flat.check("SYNTH", false, 15, 10, 0, 0), Ok(()));
        for rows in [
            vec![],
            vec![row("LONG", 0.0)],
            vec![row("LONG", 0.0), row("LONG", 0.0)],
        ] {
            assert_eq!(
                observed(true, rows).check("SYNTH", true, 15, 10, 0, 0),
                Err(PositionReadError::Coverage)
            );
        }
        let hedged = observed(true, vec![row("SHORT", -2.0), row("LONG", 2.0)]);
        assert_eq!(hedged.check("SYNTH", true, 15, 10, 0, 0), Ok(()));
    }
    #[test]
    fn stale_future_wrong_scope_mode_and_invalid_values_do_not_compare() {
        let source = observed(false, vec![row("BOTH", 0.0)]);
        assert_eq!(
            source.check("OTHER", false, 15, 10, 0, 0),
            Err(PositionReadError::Scope)
        );
        assert_eq!(
            source.check("SYNTH", true, 15, 10, 0, 0),
            Err(PositionReadError::Mode)
        );
        assert_eq!(
            source.check("SYNTH", false, 12, 1, 0, 0),
            Err(PositionReadError::Stale)
        );
        assert_eq!(
            source.check("SYNTH", false, 11, 10, 0, 0),
            Err(PositionReadError::Clock)
        );
        let bad = observed(true, vec![row("LONG", -1.0), row("SHORT", 0.0)]);
        assert_eq!(
            bad.check("SYNTH", true, 15, 10, 0, 0),
            Err(PositionReadError::InvalidValue)
        );
        let bad = observed(false, vec![row("BOTH", f64::NAN)]);
        assert_eq!(
            bad.check("SYNTH", false, 15, 10, 0, 0),
            Err(PositionReadError::InvalidValue)
        );
    }
    #[test]
    fn stream_before_rest_and_rest_before_stream_have_distinct_read_cuts() {
        let source = observed(false, vec![row("BOTH", 1.0)]);
        // The stream fill was booked before the request: same revision.
        assert_eq!(source.check("SYNTH", false, 15, 10, 1, 1), Ok(()));
        // A fill arrived after the request: even an equal net is no proof.
        assert_eq!(
            source.check("SYNTH", false, 15, 10, 0, 1),
            Err(PositionReadError::LocalChanged)
        );
        // Opening and closing while REST was in flight still changes cut.
        let flat = observed(false, vec![row("BOTH", 0.0)]);
        assert_eq!(
            flat.check("SYNTH", false, 15, 10, 0, 2),
            Err(PositionReadError::LocalChanged)
        );
    }
}
