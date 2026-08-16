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
}

/// An input to the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        }
    }

    /// The event's time, for ordering checks.
    #[must_use]
    pub const fn at(&self) -> Nanos {
        match self {
            Self::Tick(t) => t.stamp.exch,
            Self::Submit { stamp, .. } | Self::Cancel { stamp, .. } => stamp.exch,
            Self::Funding { at, .. } | Self::Time(at) | Self::MarginDeposit { at, .. } => *at,
        }
    }

    /// Encode the payload (the kind travels in the record header).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
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
            Self::MarginDeposit { amount, at } => {
                put_i64(&mut out, amount);
                put_i64(&mut out, at.0);
            }
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
        ]
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

    #[test]
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
