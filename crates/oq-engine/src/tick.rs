//! The market observation an L0 replay consumes.
//!
//! A tick here is an *aggregated* record, not a single trade: venues
//! publish depth and trade streams that a capture pipeline merges into
//! fixed windows, and each window carries the last traded price plus
//! the extremes reached inside it. Those extremes are what make L0
//! replay usable at all — without them a window looks like a single
//! price point and every order that would have been touched by an
//! intra-window excursion is missed.
//!
//! Zero has a meaning here, and it is "absent" rather than "free":
//! a captured record with no top-of-book carries `bid = ask = 0`, and
//! the matching rules fall back to trade prices. This convention comes
//! from the reference implementation whose behavior L0 must reproduce
//! exactly; it is preserved rather than improved, because an L0 that
//! is *better* than the reference is an L0 that fails parity.

use oq_types::{PriceTicks, Stamp};

/// One aggregated market observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tick {
    pub stamp: Stamp,
    /// Last traded price in the window.
    pub last: PriceTicks,
    /// Highest price reached inside the window; zero when unknown.
    pub high: PriceTicks,
    /// Lowest price reached inside the window; zero when unknown.
    pub low: PriceTicks,
    /// Best bid at the end of the window; zero when unknown.
    pub bid: PriceTicks,
    /// Best ask at the end of the window; zero when unknown.
    pub ask: PriceTicks,
}

impl Tick {
    /// A tick with trade prices only, as an aggregated trade feed
    /// without depth produces.
    #[must_use]
    pub const fn trades_only(stamp: Stamp, last: i64, high: i64, low: i64) -> Self {
        Self {
            stamp,
            last: PriceTicks(last),
            high: PriceTicks(high),
            low: PriceTicks(low),
            bid: PriceTicks::ZERO,
            ask: PriceTicks::ZERO,
        }
    }

    /// A tick with top-of-book.
    #[must_use]
    pub const fn quoted(stamp: Stamp, last: i64, high: i64, low: i64, bid: i64, ask: i64) -> Self {
        Self {
            stamp,
            last: PriceTicks(last),
            high: PriceTicks(high),
            low: PriceTicks(low),
            bid: PriceTicks(bid),
            ask: PriceTicks(ask),
        }
    }

    /// The price a buy is matched against.
    ///
    /// Prefer the ask; fall back to the window low, then to the last
    /// trade. The fallback to *low* rather than to *last* is what lets
    /// a resting buy be filled by an excursion that ended above it.
    #[must_use]
    pub const fn buy_trigger(&self) -> PriceTicks {
        if self.ask.0 > 0 {
            self.ask
        } else if self.low.0 > 0 {
            self.low
        } else {
            self.last
        }
    }

    /// The price a buy is filled at once triggered.
    #[must_use]
    pub const fn buy_fill_reference(&self) -> PriceTicks {
        if self.ask.0 > 0 { self.ask } else { self.last }
    }

    /// The price a sell is matched against.
    #[must_use]
    pub const fn sell_trigger(&self) -> PriceTicks {
        if self.bid.0 > 0 {
            self.bid
        } else if self.high.0 > 0 {
            self.high
        } else {
            self.last
        }
    }

    /// The price a sell is filled at once triggered.
    #[must_use]
    pub const fn sell_fill_reference(&self) -> PriceTicks {
        if self.bid.0 > 0 { self.bid } else { self.last }
    }

    /// Upper extent of the window, for gap detection.
    #[must_use]
    pub const fn up_extent(&self) -> PriceTicks {
        if self.high.0 > 0 {
            self.high
        } else {
            self.last
        }
    }

    /// Lower extent of the window, for gap detection.
    #[must_use]
    pub const fn dn_extent(&self) -> PriceTicks {
        if self.low.0 > 0 { self.low } else { self.last }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_take_precedence_over_trade_extremes() {
        let t = Tick::quoted(Stamp::synthetic(0), 100, 110, 90, 99, 101);
        assert_eq!(t.buy_trigger(), PriceTicks(101));
        assert_eq!(t.sell_trigger(), PriceTicks(99));
    }

    #[test]
    fn without_quotes_the_window_extremes_trigger() {
        let t = Tick::trades_only(Stamp::synthetic(0), 100, 110, 90);
        assert_eq!(
            t.buy_trigger(),
            PriceTicks(90),
            "a buy is reached by the low"
        );
        assert_eq!(
            t.sell_trigger(),
            PriceTicks(110),
            "a sell is reached by the high"
        );
        assert_eq!(t.buy_fill_reference(), PriceTicks(100));
        assert_eq!(t.sell_fill_reference(), PriceTicks(100));
    }

    #[test]
    fn a_degenerate_tick_falls_back_to_last() {
        let t = Tick::trades_only(Stamp::synthetic(0), 100, 0, 0);
        assert_eq!(t.buy_trigger(), PriceTicks(100));
        assert_eq!(t.sell_trigger(), PriceTicks(100));
        assert_eq!(t.up_extent(), PriceTicks(100));
        assert_eq!(t.dn_extent(), PriceTicks(100));
    }
}
