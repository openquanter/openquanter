//! Everything that can happen, as data.
//!
//! The core has no ambient authority: it cannot read a clock, open a
//! socket, or consult a random number generator. Anything it needs to
//! know arrives as an [`Event`]. That is what makes a replay a replay
//! rather than a re-run that happens to look similar — feed the same
//! events in the same order and the same state comes out, on any
//! machine, at any later date.
//!
//! **Time is an event.** [`Event::Time`] is how the clock advances.
//! This looks pedantic until the first time a strategy's behaviour
//! depends on the wall clock and the replay diverges from the original
//! for reasons no diff can explain.
//!
//! Encoding is fixed-layout and little-endian, and the discriminant is
//! carried in the journal record's `kind` field rather than inside the
//! payload, so a reader can route a record without decoding it. New
//! event types take new discriminants; **a discriminant is never
//! reused**, because a journal outlives the build that wrote it.

use oq_engine::Tick;
use oq_types::{Nanos, Offset, OrderId, PriceTicks, QtyLots, Ratio, Side, Stamp};

/// Journal record kinds. Append-only; values are permanent.
pub mod kind {
    pub const TICK: u16 = 1;
    /// Submit as it was before orders stated open or close. Decoded, never
    /// written: a payload of this kind means every order in that journal
    /// was an open, which is what one-way netting made it.
    pub const SUBMIT_LEGACY: u16 = 2;
    pub const SUBMIT: u16 = 7;
    pub const CANCEL: u16 = 3;
    pub const FUNDING: u16 = 4;
    pub const TIME: u16 = 5;
    pub const MARGIN_DEPOSIT: u16 = 6;
    /// A fill the **venue** decided, rather than one the matcher
    /// produced.
    ///
    /// The difference is the whole of what separates a backtest from a
    /// live run: in a backtest the matcher decides which orders trade,
    /// and live the venue does. Both end up as the same accounting, so
    /// the kernel applies both the same way — but the journal records
    /// which it was, because a replay that fed a venue fill back through
    /// a matcher would book it twice.
    pub const VENUE_FILL: u16 = 8;
    /// A depth update from the venue, for a matcher that reads one.
    ///
    /// **The first variable-length kind.** Every other payload here is
    /// a fixed size and decode refuses anything else, which is what
    /// stops a truncated record being read as a valid shorter one. A
    /// depth update is a list of levels, so it carries its own counts
    /// and decode checks the byte count against them -- the same rule,
    /// stated against a declared length rather than a constant.
    ///
    /// Recorded because it changes results. An L2 run's fills depend on
    /// the book its orders queued in, and a journal without it replays
    /// the orders into a different market. That is the difference
    /// between a record of what happened and a record of what was
    /// asked for.
    pub const DEPTH: u16 = 9;
}

/// An input to the core.
///
/// `Clone` but not `Copy`, since [`Event::Depth`] carries a list of
/// levels. Everything else here is a handful of integers and copying it
/// was free; a depth update is the one input whose size depends on what
/// the venue sent, and pretending otherwise would mean either leaving
/// it out of the journal or putting a fixed cap on how deep a book may
/// be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A market observation.
    Tick(Tick),
    /// Place an order.
    Submit {
        id: OrderId,
        side: Side,
        /// A limit price, or `None` for a market order.
        price: Option<PriceTicks>,
        qty: QtyLots,
        /// Whether this order adds to a position or reduces one.
        ///
        /// Under one-way netting the distinction is derivable from the
        /// side and the current position, and carrying it changes
        /// nothing. Under hedge accounting it is not: a buy while a
        /// short is open may be closing that short or opening a long,
        /// and only the order knows which. The venue asks the same
        /// question and calls the answer `positionSide`.
        offset: Offset,
        stamp: Stamp,
    },
    /// Withdraw an order.
    Cancel { id: OrderId, stamp: Stamp },
    /// A funding settlement.
    Funding {
        at: Nanos,
        rate: Ratio,
        mark: PriceTicks,
    },
    /// The clock advanced. The only way the core learns the time.
    Time(Nanos),
    /// Collateral added to or removed from the account.
    MarginDeposit { amount: i64, at: Nanos },
    /// A fill the venue decided.
    ///
    /// Only meaningful under [`crate::kernel::Matching::Venue`]; a
    /// simulated run produces its own fills and one arriving from
    /// outside would be a second matcher.
    VenueFill(oq_types::Fill),
    /// A depth update from the venue.
    ///
    /// Read by [`Matcher::L2`](crate::matcher::Matcher::L2) and by no
    /// other tier. A tier that does not keep a book reports it as
    /// unread rather than failing: a run is allowed to be handed data
    /// it does not use, and is not allowed to be silent about it.
    ///
    /// Boxed because it is the one variant that is not a handful of
    /// integers, and an enum is as large as its largest arm -- every
    /// tick in every journal would otherwise carry the footprint of a
    /// book it does not hold.
    Depth(Box<oq_engine::DepthUpdate>),
}

impl Event {
    /// The journal record kind for this event.
    #[must_use]
    pub const fn kind(&self) -> u16 {
        match self {
            Self::Tick(_) => kind::TICK,
            Self::Submit { .. } => kind::SUBMIT,
            Self::Cancel { .. } => kind::CANCEL,
            Self::Funding { .. } => kind::FUNDING,
            Self::Time(_) => kind::TIME,
            Self::MarginDeposit { .. } => kind::MARGIN_DEPOSIT,
            Self::VenueFill(_) => kind::VENUE_FILL,
            Self::Depth(_) => kind::DEPTH,
        }
    }

    /// The event's time, for ordering checks.
    #[must_use]
    pub const fn at(&self) -> Nanos {
        match self {
            Self::Tick(t) => t.stamp.exch,
            Self::Submit { stamp, .. } | Self::Cancel { stamp, .. } => stamp.exch,
            Self::Funding { at, .. } | Self::Time(at) | Self::MarginDeposit { at, .. } => *at,
            // The venue's clock, not this process's. A fill is ordered
            // by when it happened, and the local receive time is a
            // property of the link rather than of the trade.
            Self::VenueFill(f) => f.stamp.exch,
            // The venue's event time, in the unit everything else here
            // uses. A depth update carries milliseconds because that is
            // what the venue sends; ordering against ticks needs
            // nanoseconds, and converting at the boundary keeps one unit
            // in the kernel.
            Self::Depth(u) => Nanos(u.event_ms.saturating_mul(1_000_000)),
        }
    }

    /// Encode the payload (the kind travels in the record header).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        // The one variable-length payload, taken first because the match
        // below is over a copy and a list of levels is not one.
        //
        // Layout: the four sequence fields, then a byte saying whether
        // `prev_final_id` is present, then the two level counts, then
        // the levels. Counts before the data, so a reader knows how much
        // to expect before it reads it -- and can refuse a payload whose
        // size disagrees with what it declared, which is the same rule
        // the fixed-size kinds enforce against a constant.
        if let Self::Depth(u) = self {
            let mut out = Vec::with_capacity(41 + (u.bids.len() + u.asks.len()) * 16);
            out.extend_from_slice(&u.event_ms.to_le_bytes());
            out.extend_from_slice(&u.first_id.to_le_bytes());
            out.extend_from_slice(&u.final_id.to_le_bytes());
            out.extend_from_slice(&u.prev_final_id.unwrap_or(0).to_le_bytes());
            out.push(u8::from(u.prev_final_id.is_some()));
            out.extend_from_slice(&(u.bids.len() as u32).to_le_bytes());
            out.extend_from_slice(&(u.asks.len() as u32).to_le_bytes());
            for level in u.bids.iter().chain(u.asks.iter()) {
                out.extend_from_slice(&level.price.to_le_bytes());
                out.extend_from_slice(&level.qty.to_le_bytes());
            }
            return out;
        }

        let mut out = Vec::with_capacity(64);
        let put_i64 = |out: &mut Vec<u8>, v: i64| out.extend_from_slice(&v.to_le_bytes());
        match *self {
            Self::Tick(t) => {
                put_i64(&mut out, t.stamp.exch.0);
                put_i64(&mut out, t.stamp.local.0);
                put_i64(&mut out, t.last.0);
                put_i64(&mut out, t.high.0);
                put_i64(&mut out, t.low.0);
                put_i64(&mut out, t.bid.0);
                put_i64(&mut out, t.ask.0);
                put_i64(&mut out, t.volume.0);
            }
            Self::Submit {
                id,
                side,
                price,
                qty,
                offset,
                stamp,
            } => {
                out.extend_from_slice(&id.0.to_le_bytes());
                out.push(match side {
                    Side::Buy => 0,
                    Side::Sell => 1,
                });
                // A market order is encoded as "no price" rather than as
                // price zero, so the distinction survives the journal
                // even though the matching engine's legacy wire format
                // uses zero as a sentinel.
                out.push(u8::from(price.is_some()));
                put_i64(&mut out, price.map_or(0, |p| p.0));
                put_i64(&mut out, qty.0);
                put_i64(&mut out, stamp.exch.0);
                put_i64(&mut out, stamp.local.0);
                // Appended last so that a journal written before this
                // field existed is a prefix of one written after, and
                // replays with the offset it implied: everything was
                // Open.
                out.push(match offset {
                    Offset::Open => 0,
                    Offset::Close => 1,
                });
            }
            Self::Cancel { id, stamp } => {
                out.extend_from_slice(&id.0.to_le_bytes());
                put_i64(&mut out, stamp.exch.0);
                put_i64(&mut out, stamp.local.0);
            }
            Self::Funding { at, rate, mark } => {
                put_i64(&mut out, at.0);
                put_i64(&mut out, rate.0);
                put_i64(&mut out, mark.0);
            }
            Self::Time(at) => put_i64(&mut out, at.0),
            Self::VenueFill(f) => {
                put_i64(&mut out, f.stamp.exch.0);
                put_i64(&mut out, f.stamp.local.0);
                out.extend_from_slice(&f.instrument.0.to_le_bytes());
                out.extend_from_slice(&f.order.0.to_le_bytes());
                out.extend_from_slice(&f.trade.0.to_le_bytes());
                put_i64(&mut out, f.price.0);
                put_i64(&mut out, f.qty.0);
                out.push(match f.side {
                    Side::Buy => 0,
                    Side::Sell => 1,
                });
                out.push(match f.offset {
                    oq_types::Offset::Open => 0,
                    oq_types::Offset::Close => 1,
                });
                out.push(match f.liquidity {
                    oq_types::Liquidity::Maker => 0,
                    oq_types::Liquidity::Taker => 1,
                });
            }
            Self::MarginDeposit { amount, at } => {
                put_i64(&mut out, amount);
                put_i64(&mut out, at.0);
            }
            Self::Depth(_) => unreachable!("handled before the match"),
        }
        out
    }

    /// Decode a payload of the given kind.
    ///
    /// Returns `None` for an unknown kind or a payload that is the
    /// wrong length. A journal written by a newer build can therefore
    /// be *detected* by an older one rather than misread — the failure
    /// mode a fixed-layout format has to get right.
    #[must_use]
    pub fn decode(kind: u16, payload: &[u8]) -> Option<Self> {
        fn i64_at(b: &[u8], i: usize) -> Option<i64> {
            b.get(i * 8..i * 8 + 8)
                .map(|s| i64::from_le_bytes(s.try_into().expect("8 bytes")))
        }
        match kind {
            kind::TICK => {
                if payload.len() != 64 {
                    return None;
                }
                Some(Self::Tick(Tick {
                    stamp: Stamp::new(i64_at(payload, 0)?, i64_at(payload, 1)?),
                    last: PriceTicks(i64_at(payload, 2)?),
                    high: PriceTicks(i64_at(payload, 3)?),
                    low: PriceTicks(i64_at(payload, 4)?),
                    bid: PriceTicks(i64_at(payload, 5)?),
                    ask: PriceTicks(i64_at(payload, 6)?),
                    volume: oq_types::QtyLots(i64_at(payload, 7)?),
                }))
            }
            kind::SUBMIT | kind::SUBMIT_LEGACY => {
                // Length is exact per kind rather than "either of two".
                // Accepting both lengths under one kind would make a
                // truncated new record indistinguishable from a valid old
                // one, and decode is where that has to be caught — the
                // whole point of refusing a wrong length is that a record
                // is never quietly read as something it is not.
                let expected = if kind == kind::SUBMIT { 43 } else { 42 };
                if payload.len() != expected {
                    return None;
                }
                let offset = if kind == kind::SUBMIT {
                    match payload[42] {
                        0 => Offset::Open,
                        1 => Offset::Close,
                        _ => return None,
                    }
                } else {
                    Offset::Open
                };
                let id = OrderId(u64::from_le_bytes(
                    payload[0..8].try_into().expect("8 bytes"),
                ));
                let side = match payload[8] {
                    0 => Side::Buy,
                    1 => Side::Sell,
                    _ => return None,
                };
                let has_price = payload[9] == 1;
                let rest = &payload[10..];
                let price_raw = i64_at(rest, 0)?;
                Some(Self::Submit {
                    id,
                    side,
                    price: has_price.then_some(PriceTicks(price_raw)),
                    qty: QtyLots(i64_at(rest, 1)?),
                    offset,
                    stamp: Stamp::new(i64_at(rest, 2)?, i64_at(rest, 3)?),
                })
            }
            kind::CANCEL => {
                if payload.len() != 24 {
                    return None;
                }
                let id = OrderId(u64::from_le_bytes(
                    payload[0..8].try_into().expect("8 bytes"),
                ));
                let rest = &payload[8..];
                Some(Self::Cancel {
                    id,
                    stamp: Stamp::new(i64_at(rest, 0)?, i64_at(rest, 1)?),
                })
            }
            kind::FUNDING => {
                if payload.len() != 24 {
                    return None;
                }
                Some(Self::Funding {
                    at: Nanos(i64_at(payload, 0)?),
                    rate: Ratio(i64_at(payload, 1)?),
                    mark: PriceTicks(i64_at(payload, 2)?),
                })
            }
            kind::TIME => {
                if payload.len() != 8 {
                    return None;
                }
                Some(Self::Time(Nanos(i64_at(payload, 0)?)))
            }
            kind::MARGIN_DEPOSIT => {
                if payload.len() != 16 {
                    return None;
                }
                Some(Self::MarginDeposit {
                    amount: i64_at(payload, 0)?,
                    at: Nanos(i64_at(payload, 1)?),
                })
            }
            kind::DEPTH => {
                // The only kind whose length is declared rather than
                // fixed, so the check is against the declaration: the
                // header, then exactly the levels it says it carries.
                // A payload that disagrees with its own counts is a
                // truncation or a corruption, and reading it as a
                // shorter book would put a queue behind levels that
                // were never there.
                const HEADER: usize = 8 + 8 + 8 + 8 + 1 + 4 + 4;
                if payload.len() < HEADER {
                    return None;
                }
                let at = |i: usize| -> Option<i64> {
                    payload
                        .get(i..i + 8)?
                        .try_into()
                        .ok()
                        .map(i64::from_le_bytes)
                };
                let u64_at = |i: usize| -> Option<u64> {
                    payload
                        .get(i..i + 8)?
                        .try_into()
                        .ok()
                        .map(u64::from_le_bytes)
                };
                let u32_at = |i: usize| -> Option<u32> {
                    payload
                        .get(i..i + 4)?
                        .try_into()
                        .ok()
                        .map(u32::from_le_bytes)
                };
                let prev = u64_at(24)?;
                let prev_final_id = match payload[32] {
                    0 => None,
                    1 => Some(prev),
                    // A third value is a record this build cannot read,
                    // not a `None` to fall back on.
                    _ => return None,
                };
                let n_bids = u32_at(33)? as usize;
                let n_asks = u32_at(37)? as usize;
                let levels = n_bids.checked_add(n_asks)?;
                if payload.len() != HEADER + levels.checked_mul(16)? {
                    return None;
                }
                let mut read = Vec::with_capacity(levels);
                for i in 0..levels {
                    let o = HEADER + i * 16;
                    read.push(oq_engine::Level {
                        price: at(o)?,
                        qty: at(o + 8)?,
                    });
                }
                let asks = read.split_off(n_bids);
                Some(Self::Depth(Box::new(oq_engine::DepthUpdate {
                    event_ms: at(0)?,
                    first_id: u64_at(8)?,
                    final_id: u64_at(16)?,
                    prev_final_id,
                    bids: read,
                    asks,
                })))
            }
            kind::VENUE_FILL => {
                // Exact length, like every other kind: a truncated
                // record must not read as a valid shorter one.
                // 8+8 stamp, 4 instrument, 8 order, 8 trade, 8 price,
                // 8 qty, and three one-byte enums.
                if payload.len() != 55 {
                    return None;
                }
                let u32_at = |i: usize| -> Option<u32> {
                    payload
                        .get(i..i + 4)
                        .map(|s| u32::from_le_bytes(s.try_into().expect("4")))
                };
                let u64_at = |i: usize| -> Option<u64> {
                    payload
                        .get(i..i + 8)
                        .map(|s| u64::from_le_bytes(s.try_into().expect("8")))
                };
                Some(Self::VenueFill(oq_types::Fill {
                    stamp: Stamp::new(i64_at(payload, 0)?, i64_at(payload, 1)?),
                    instrument: oq_types::InstrumentId(u32_at(16)?),
                    order: OrderId(u64_at(20)?),
                    trade: oq_types::TradeId(u64_at(28)?),
                    price: PriceTicks(i64::from_le_bytes(payload.get(36..44)?.try_into().ok()?)),
                    qty: oq_types::QtyLots(i64::from_le_bytes(
                        payload.get(44..52)?.try_into().ok()?,
                    )),
                    side: match payload.get(52)? {
                        0 => Side::Buy,
                        1 => Side::Sell,
                        _ => return None,
                    },
                    offset: match payload.get(53)? {
                        0 => oq_types::Offset::Open,
                        1 => oq_types::Offset::Close,
                        _ => return None,
                    },
                    liquidity: match payload.get(54)? {
                        0 => oq_types::Liquidity::Maker,
                        1 => oq_types::Liquidity::Taker,
                        _ => return None,
                    },
                }))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<Event> {
        vec![
            Event::Tick(Tick::quoted(Stamp::new(10, 12), 100, 110, 90, 99, 101)),
            Event::Tick(Tick::trades_only(Stamp::new(20, 21), 100, 0, 0)),
            Event::Submit {
                id: OrderId::new(7),
                side: Side::Buy,
                price: Some(PriceTicks(950)),
                qty: QtyLots(3),
                stamp: Stamp::new(30, 31),
                offset: oq_types::Offset::Open,
            },
            Event::Submit {
                id: OrderId::new(8),
                side: Side::Sell,
                price: None,
                qty: QtyLots(1),
                stamp: Stamp::new(40, 41),
                offset: oq_types::Offset::Open,
            },
            Event::Cancel {
                id: OrderId::new(7),
                stamp: Stamp::new(50, 51),
            },
            Event::Funding {
                at: Nanos(60),
                rate: Ratio::from_ppm(100),
                mark: PriceTicks(1000),
            },
            Event::Time(Nanos(70)),
            Event::MarginDeposit {
                amount: -12_345,
                at: Nanos(80),
            },
            Event::Depth(Box::new(oq_engine::DepthUpdate {
                event_ms: 1_786_000_000_123,
                first_id: 7_000,
                final_id: 7_005,
                prev_final_id: Some(6_999),
                bids: vec![
                    oq_engine::Level {
                        price: 6_200_010,
                        qty: 1_500,
                    },
                    oq_engine::Level {
                        price: 6_200_000,
                        qty: 2_500,
                    },
                ],
                asks: vec![oq_engine::Level {
                    price: 6_200_020,
                    qty: 2_000,
                }],
            })),
        ]
    }

    fn depth(bids: Vec<oq_engine::Level>, asks: Vec<oq_engine::Level>) -> Event {
        Event::Depth(Box::new(oq_engine::DepthUpdate {
            event_ms: 1,
            first_id: 2,
            final_id: 3,
            prev_final_id: None,
            bids,
            asks,
        }))
    }

    fn level(price: i64, qty: i64) -> oq_engine::Level {
        oq_engine::Level { price, qty }
    }

    /// The two sides survive as two sides.
    ///
    /// They are encoded as one run of levels behind two counts, so an
    /// off-by-one in the split reads a bid as an ask -- which produces a
    /// crossed book, prices that look plausible, and a queue measured
    /// against the wrong side.
    #[test]
    fn the_sides_do_not_swap_across_the_round_trip() {
        let e = depth(vec![level(99, 10), level(98, 20)], vec![level(101, 30)]);
        let Event::Depth(back) = Event::decode(e.kind(), &e.encode()).expect("decodes") else {
            panic!("wrong kind");
        };
        assert_eq!(back.bids, vec![level(99, 10), level(98, 20)]);
        assert_eq!(back.asks, vec![level(101, 30)]);
    }

    /// An empty side is empty, not absent.
    #[test]
    fn a_one_sided_update_round_trips() {
        for e in [
            depth(vec![level(99, 10)], Vec::new()),
            depth(Vec::new(), vec![level(101, 10)]),
            depth(Vec::new(), Vec::new()),
        ] {
            assert_eq!(Event::decode(e.kind(), &e.encode()).expect("decodes"), e);
        }
    }

    /// A payload that disagrees with its own counts is refused.
    ///
    /// This kind is the only one whose length is declared rather than
    /// fixed, so the rule every other kind states against a constant --
    /// a truncated record must not read as a valid shorter one -- has to
    /// be stated against the declaration instead. Reading it anyway
    /// would put a queue behind levels that were never there.
    #[test]
    fn a_payload_disagreeing_with_its_counts_is_refused() {
        let e = depth(vec![level(99, 10), level(98, 20)], vec![level(101, 30)]);
        let full = e.encode();

        for cut in 1..=(3 * 16) {
            let short = &full[..full.len() - cut];
            assert!(
                Event::decode(kind::DEPTH, short).is_none(),
                "a payload {cut} bytes short must not decode"
            );
        }

        let mut long = full.clone();
        long.push(0);
        assert!(Event::decode(kind::DEPTH, &long).is_none());

        // A count claiming more levels than the bytes hold. The 16 MiB
        // frame cap is the only thing between this and an allocation the
        // checksum has not yet had a chance to reject.
        let mut lying = full;
        lying[33..37].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Event::decode(kind::DEPTH, &lying).is_none());
    }

    /// The presence byte for `prev_final_id` takes two values.
    ///
    /// A third is a record this build cannot read. Falling back to
    /// `None` would turn an unreadable record into a snapshot boundary,
    /// and the book would accept the next update as a fresh start.
    #[test]
    fn an_unknown_presence_byte_is_refused() {
        let e = depth(vec![level(99, 10)], Vec::new());
        let mut bytes = e.encode();
        bytes[32] = 2;
        assert!(Event::decode(kind::DEPTH, &bytes).is_none());
    }

    #[test]
    fn every_event_round_trips() {
        for e in samples() {
            let decoded = Event::decode(e.kind(), &e.encode()).expect("decodes");
            assert_eq!(decoded, e, "round trip failed for {e:?}");
        }
    }

    #[test]
    fn a_market_order_stays_distinct_from_a_zero_price_limit() {
        let market = Event::Submit {
            id: OrderId::new(1),
            side: Side::Buy,
            price: None,
            qty: QtyLots(1),
            stamp: Stamp::synthetic(0),
            offset: oq_types::Offset::Open,
        };
        let zero_limit = Event::Submit {
            id: OrderId::new(1),
            side: Side::Buy,
            price: Some(PriceTicks::ZERO),
            qty: QtyLots(1),
            stamp: Stamp::synthetic(0),
            offset: oq_types::Offset::Open,
        };
        assert_ne!(market.encode(), zero_limit.encode());
        assert_eq!(
            Event::decode(market.kind(), &market.encode()).expect("decodes"),
            market
        );
    }

    #[test]
    fn an_unknown_kind_is_refused_rather_than_guessed() {
        assert!(Event::decode(9_999, &[0u8; 8]).is_none());
    }

    /// A journal written before orders stated open or close must still
    /// replay, and replay as what it meant: everything was an open.
    #[test]
    fn a_legacy_submit_decodes_as_an_open() {
        let modern = Event::Submit {
            id: OrderId::new(9),
            side: Side::Buy,
            price: Some(PriceTicks(1_234_500)),
            qty: QtyLots(7),
            offset: Offset::Open,
            stamp: Stamp::new(11, 12),
        };
        // The legacy layout is the modern one without its last byte,
        // which is what makes an old journal readable at all.
        let bytes = modern.encode();
        assert_eq!(bytes.len(), 43);
        let legacy = &bytes[..42];

        let decoded = Event::decode(kind::SUBMIT_LEGACY, legacy).expect("legacy decodes");
        assert_eq!(decoded, modern);
    }

    /// And a legacy payload under the modern kind is a truncated modern
    /// record, not an old one — the distinction the separate kind exists
    /// to preserve.
    #[test]
    fn a_legacy_length_under_the_modern_kind_is_refused() {
        let bytes = Event::Submit {
            id: OrderId::new(9),
            side: Side::Buy,
            price: Some(PriceTicks(1_234_500)),
            qty: QtyLots(7),
            offset: Offset::Close,
            stamp: Stamp::new(11, 12),
        }
        .encode();
        assert!(Event::decode(kind::SUBMIT, &bytes[..42]).is_none());
    }

    #[test]
    fn a_close_survives_the_round_trip() {
        let e = Event::Submit {
            id: OrderId::new(3),
            side: Side::Sell,
            price: None,
            qty: QtyLots(2),
            offset: Offset::Close,
            stamp: Stamp::new(1, 2),
        };
        assert_eq!(Event::decode(e.kind(), &e.encode()), Some(e));
    }

    #[test]
    fn a_wrong_length_payload_is_refused() {
        // What an older build must do with a record a newer build wrote.
        for e in samples() {
            let mut bytes = e.encode();
            bytes.push(0);
            assert!(
                Event::decode(e.kind(), &bytes).is_none(),
                "over-long payload accepted for {e:?}"
            );
            let short = &e.encode()[..e.encode().len().saturating_sub(1)];
            assert!(
                Event::decode(e.kind(), short).is_none(),
                "truncated payload accepted for {e:?}"
            );
        }
    }

    #[test]
    fn kinds_are_distinct() {
        let mut kinds: Vec<u16> = samples().iter().map(Event::kind).collect();
        kinds.sort_unstable();
        let before = kinds.len();
        kinds.dedup();
        // Two Tick samples and two Submit samples share kinds by design.
        assert_eq!(before - kinds.len(), 2);
    }
}
