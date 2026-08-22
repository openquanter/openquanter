//! A venue snapshot, and the guarantee that it is whole.
//!
//! A snapshot is only useful for diffing if the client knows it is
//! complete. Diff a partial answer and everything not yet received looks
//! like it has vanished — which, on the resolution side, means every
//! order the reply had not reached yet gets treated as terminal.
//!
//! FIX solved this with `LastRptRequested`; Roq brackets its download
//! with `DownloadBegin`/`DownloadEnd`. Both exist because the failure is
//! not hypothetical. Here the same guarantee is carried in the type:
//! [`Snapshot`] cannot be constructed except by sealing a
//! [`SnapshotBuilder`], and every read that fails leaves the builder
//! unsealable rather than producing a snapshot with a hole in it.

use crate::binance::{AccountSnapshot, OpenOrder, PositionSnapshot};

/// Which reads a complete snapshot requires.
///
/// Named rather than counted so a partial snapshot can say what it is
/// missing. "Incomplete" without a subject sends a reader to the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Part {
    Account,
    Positions,
    OpenOrders,
}

impl Part {
    /// Every part a snapshot needs.
    pub const ALL: [Self; 3] = [Self::Account, Self::Positions, Self::OpenOrders];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Positions => "positions",
            Self::OpenOrders => "open orders",
        }
    }
}

/// A snapshot under construction.
#[derive(Debug, Default)]
pub struct SnapshotBuilder {
    account: Option<AccountSnapshot>,
    positions: Option<Vec<PositionSnapshot>>,
    open_orders: Option<Vec<OpenOrder>>,
    /// The symbol every read was scoped to, for the record.
    symbol: String,
}

impl SnapshotBuilder {
    #[must_use]
    pub fn new(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn account(mut self, a: AccountSnapshot) -> Self {
        self.account = Some(a);
        self
    }

    #[must_use]
    pub fn positions(mut self, p: Vec<PositionSnapshot>) -> Self {
        self.positions = Some(p);
        self
    }

    #[must_use]
    pub fn open_orders(mut self, o: Vec<OpenOrder>) -> Self {
        self.open_orders = Some(o);
        self
    }

    /// What is still missing, in a stable order.
    #[must_use]
    pub fn missing(&self) -> Vec<Part> {
        let mut out = Vec::new();
        if self.account.is_none() {
            out.push(Part::Account);
        }
        if self.positions.is_none() {
            out.push(Part::Positions);
        }
        if self.open_orders.is_none() {
            out.push(Part::OpenOrders);
        }
        out
    }

    /// Seal into a snapshot, or report what is missing.
    ///
    /// # Errors
    /// The parts that were never supplied. A read that failed must leave
    /// its part unset rather than substituting an empty result: an empty
    /// position list and a failed position query are the same value and
    /// opposite facts, and only one of them means "flat".
    pub fn seal(self) -> Result<Snapshot, Vec<Part>> {
        let missing = self.missing();
        if !missing.is_empty() {
            return Err(missing);
        }
        Ok(Snapshot {
            symbol: self.symbol,
            account: self.account.expect("checked"),
            positions: self.positions.expect("checked"),
            open_orders: self.open_orders.expect("checked"),
        })
    }
}

/// A complete view of the account at one moment.
///
/// Existence of this value is the assertion that every read succeeded.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub symbol: String,
    pub account: AccountSnapshot,
    pub positions: Vec<PositionSnapshot>,
    pub open_orders: Vec<OpenOrder>,
}

impl Snapshot {
    /// Net position across both legs, in the venue's units.
    ///
    /// Under hedge accounting the legs are reported separately and this
    /// sums them. It is the figure a one-way account would have held,
    /// and it is *not* a substitute for comparing the legs — an account
    /// long and short in equal size nets to zero while holding margin
    /// for both.
    #[must_use]
    pub fn net_position(&self) -> f64 {
        self.positions.iter().map(|p| p.amount).sum()
    }

    /// The leg matching `side`, if the venue reports one.
    #[must_use]
    pub fn leg(&self, side: &str) -> Option<&PositionSnapshot> {
        self.positions.iter().find(|p| p.position_side == side)
    }

    /// When the account read was taken, in venue milliseconds.
    #[must_use]
    pub const fn read_at_ms(&self) -> i64 {
        self.account.read_at_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> AccountSnapshot {
        AccountSnapshot {
            wallet_balance: 20_000.0,
            unrealized: -2_197.91,
            margin_balance: 17_802.09,
            read_at_ms: 1_700_000_000_000,
        }
    }

    fn leg(side: &str, amount: f64) -> PositionSnapshot {
        PositionSnapshot {
            symbol: "BTCUSDT".into(),
            position_side: side.into(),
            amount,
            entry_price: 71_444.87,
            unrealized: -2_197.75,
        }
    }

    #[test]
    fn a_snapshot_missing_a_part_cannot_be_sealed() {
        let err = SnapshotBuilder::new("BTCUSDT")
            .account(account())
            .positions(vec![])
            .seal()
            .expect_err("must not seal");
        assert_eq!(err, vec![Part::OpenOrders]);
    }

    /// The failure names what is missing. "Incomplete" on its own sends
    /// the reader to the logs to find out which read failed.
    #[test]
    fn the_failure_names_every_missing_part() {
        let err = SnapshotBuilder::new("BTCUSDT")
            .seal()
            .expect_err("must not seal");
        assert_eq!(err, Part::ALL.to_vec());
        assert_eq!(err[0].name(), "account");
    }

    #[test]
    fn a_complete_snapshot_seals() {
        let s = SnapshotBuilder::new("BTCUSDT")
            .account(account())
            .positions(vec![leg("LONG", 0.256), leg("SHORT", -0.004)])
            .open_orders(vec![])
            .seal()
            .expect("seals");
        assert_eq!(s.positions.len(), 2);
        assert!((s.net_position() - 0.252).abs() < 1e-9);
    }

    /// An account long and short in equal size nets to zero and is
    /// holding margin for both legs. Anything that reads only the net is
    /// looking at the number least likely to reveal that.
    #[test]
    fn a_hedged_pair_nets_to_zero_while_both_legs_stand() {
        let s = SnapshotBuilder::new("BTCUSDT")
            .account(account())
            .positions(vec![leg("LONG", 0.2), leg("SHORT", -0.2)])
            .open_orders(vec![])
            .seal()
            .expect("seals");
        assert!(s.net_position().abs() < 1e-12);
        assert_eq!(s.leg("LONG").expect("long").amount, 0.2);
        assert_eq!(s.leg("SHORT").expect("short").amount, -0.2);
    }

    #[test]
    fn legs_are_addressable_by_side() {
        let s = SnapshotBuilder::new("BTCUSDT")
            .account(account())
            .positions(vec![leg("LONG", 0.256)])
            .open_orders(vec![])
            .seal()
            .expect("seals");
        assert!(s.leg("SHORT").is_none());
    }
}
