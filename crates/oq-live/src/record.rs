//! What the live process writes down, and when.
//!
//! # Before, not after
//!
//! An order is recorded **before** it is sent. That ordering is the
//! point, and it is the same ordering `oq-core` enforces for its own
//! path: a process that sends first and records afterwards can die in
//! between, and what it leaves behind is a live order that its own
//! journal has never heard of. Nothing later can recover from that,
//! because there is nothing to recover *from* — the client id, the one
//! handle that could ask the venue about it, was never written.
//!
//! Recording first means the opposite failure: a journal that mentions
//! an order which may not exist. That one is recoverable, and the
//! mechanism already exists — it is the same question
//! [`crate::Submission`] answers with an unknown outcome, asked after a
//! restart instead of after a timeout. A record with no outcome beside
//! it is exactly a placement whose answer never arrived.
//!
//! # Why a format rather than a log line
//!
//! Because the attribution the roadmap asks for compares a live run
//! against a replay of the same decisions, and that comparison needs the
//! decisions, not prose about them. The engine already replays a journal
//! deterministically; this is the missing half — the live side writing
//! one it can read.
//!
//! The encoding is explicit and little-endian, with length-prefixed
//! strings, in the same spirit as the capture and tick formats. No
//! derive, no schema library: a format this small is cheaper to read
//! than a dependency is to justify, and a reader written from the field
//! list below cannot silently disagree with a writer generated from a
//! macro.

use oq_types::{Nanos, PriceTicks, QtyLots, Side};

/// Frame kinds. Numbered explicitly and never reused: a reader from an
/// older build must be able to skip what it does not know rather than
/// misread it as something it does.
pub mod kind {
    /// The run's identity and the contract it trades.
    pub const SESSION_START: u16 = 1;
    /// An observation handed to the strategy.
    pub const TICK: u16 = 2;
    /// An order about to be sent. Written **before** sending.
    pub const SUBMITTED: u16 = 3;
    /// What the venue said about a submitted order.
    pub const OUTCOME: u16 = 4;
    /// A fill reported on the account stream.
    pub const FILL: u16 = 5;
    /// The gate refused an order. Nothing was sent.
    pub const REFUSED: u16 = 6;
    /// Positions adopted from the venue.
    pub const RECONCILED: u16 = 7;
    /// What the strategy was waiting for, sampled on a timer.
    pub const WAITING: u16 = 8;
}

/// How a submitted order turned out.
///
/// The unknown case is a value rather than an absence so a reader can
/// tell "we asked and could not find out" from "the process died before
/// writing anything", which are different situations with different
/// recoveries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeTag {
    Accepted = 1,
    Rejected = 2,
    Unknown = 3,
}

impl OutcomeTag {
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Accepted),
            2 => Some(Self::Rejected),
            3 => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// One record, decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    SessionStart {
        prefix: String,
        symbol: String,
        price_scale: u8,
        qty_scale: u8,
    },
    Tick {
        at: Nanos,
        last: PriceTicks,
        bid: PriceTicks,
        ask: PriceTicks,
        volume: QtyLots,
    },
    Submitted {
        at: Nanos,
        client_id: String,
        side: Side,
        /// Zero means a market order, which the side and quantity still
        /// describe fully.
        limit_price: PriceTicks,
        qty: QtyLots,
        reduce_only: bool,
    },
    Outcome {
        at: Nanos,
        client_id: String,
        tag: OutcomeTag,
        detail: String,
    },
    Fill {
        at: Nanos,
        client_id: String,
        trade_id: i64,
        qty: String,
        price: String,
    },
    Refused {
        at: Nanos,
        breach: String,
    },
    /// Positions the venue already held, taken over at startup.
    ///
    /// Written once, immediately after the session opens its journal and
    /// before anything is sent. Until this was emitted the record existed
    /// — encoded, decoded, rendered by `oq-replay` — with nothing in the
    /// tree constructing it, so `--adopt-existing` was the one startup
    /// step that left no trace. That is the step a migration is made of:
    /// a tool rebuilding "what this system believes it holds" from the
    /// journal would have come up short by exactly the migrated part.
    ///
    /// Each leg is (symbol, side, lots, entry in price ticks). The entry
    /// is here because a position without its basis is not a position a
    /// reader can do anything with: no unrealised figure, no cost, no
    /// comparison against the venue.
    Reconciled {
        at: Nanos,
        legs: Vec<(String, String, i64, i64)>,
    },
    /// What the strategy was waiting for, sampled on a timer.
    ///
    /// Every other record here is something that happened. This is the
    /// one that says why nothing did — which is the state a run is
    /// hardest to explain in, and the state a run that places no orders
    /// spends all of its time in.
    Waiting {
        at: Nanos,
        entries: Vec<(String, i64)>,
    },
}

impl Record {
    /// The frame kind this record is written under.
    #[must_use]
    pub const fn kind(&self) -> u16 {
        match self {
            Self::SessionStart { .. } => kind::SESSION_START,
            Self::Tick { .. } => kind::TICK,
            Self::Submitted { .. } => kind::SUBMITTED,
            Self::Outcome { .. } => kind::OUTCOME,
            Self::Fill { .. } => kind::FILL,
            Self::Refused { .. } => kind::REFUSED,
            Self::Reconciled { .. } => kind::RECONCILED,
            Self::Waiting { .. } => kind::WAITING,
        }
    }

    /// Encode the payload. The frame kind travels outside it, in the
    /// journal's own header, so it is not repeated here.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::SessionStart {
                prefix,
                symbol,
                price_scale,
                qty_scale,
            } => {
                put_str(&mut out, prefix);
                put_str(&mut out, symbol);
                out.push(*price_scale);
                out.push(*qty_scale);
            }
            Self::Tick {
                at,
                last,
                bid,
                ask,
                volume,
            } => {
                put_i64(&mut out, at.0);
                put_i64(&mut out, last.0);
                put_i64(&mut out, bid.0);
                put_i64(&mut out, ask.0);
                put_i64(&mut out, volume.0);
            }
            Self::Submitted {
                at,
                client_id,
                side,
                limit_price,
                qty,
                reduce_only,
            } => {
                put_i64(&mut out, at.0);
                put_str(&mut out, client_id);
                out.push(match side {
                    Side::Buy => 1,
                    Side::Sell => 2,
                });
                put_i64(&mut out, limit_price.0);
                put_i64(&mut out, qty.0);
                out.push(u8::from(*reduce_only));
            }
            Self::Outcome {
                at,
                client_id,
                tag,
                detail,
            } => {
                put_i64(&mut out, at.0);
                put_str(&mut out, client_id);
                out.push(*tag as u8);
                put_str(&mut out, detail);
            }
            Self::Fill {
                at,
                client_id,
                trade_id,
                qty,
                price,
            } => {
                put_i64(&mut out, at.0);
                put_str(&mut out, client_id);
                put_i64(&mut out, *trade_id);
                put_str(&mut out, qty);
                put_str(&mut out, price);
            }
            Self::Refused { at, breach } => {
                put_i64(&mut out, at.0);
                put_str(&mut out, breach);
            }
            Self::Waiting { at, entries } => {
                put_i64(&mut out, at.0);
                put_i64(&mut out, i64::try_from(entries.len()).unwrap_or(0));
                for (name, value) in entries {
                    put_str(&mut out, name);
                    put_i64(&mut out, *value);
                }
            }
            Self::Reconciled { at, legs } => {
                put_i64(&mut out, at.0);
                put_i64(&mut out, i64::try_from(legs.len()).unwrap_or(0));
                for (symbol, side, lots, entry) in legs {
                    put_str(&mut out, symbol);
                    put_str(&mut out, side);
                    put_i64(&mut out, *lots);
                    put_i64(&mut out, *entry);
                }
            }
        }
        out
    }

    /// Decode a payload written under `kind`.
    ///
    /// `None` for a kind this build does not know, or a payload that
    /// runs out — a truncated record is the normal shape of a process
    /// that died mid-write, and reading past its end would invent data.
    #[must_use]
    pub fn decode(kind: u16, mut p: &[u8]) -> Option<Self> {
        Some(match kind {
            kind::SESSION_START => Self::SessionStart {
                prefix: take_str(&mut p)?,
                symbol: take_str(&mut p)?,
                price_scale: take_u8(&mut p)?,
                qty_scale: take_u8(&mut p)?,
            },
            kind::TICK => Self::Tick {
                at: Nanos(take_i64(&mut p)?),
                last: PriceTicks(take_i64(&mut p)?),
                bid: PriceTicks(take_i64(&mut p)?),
                ask: PriceTicks(take_i64(&mut p)?),
                volume: QtyLots(take_i64(&mut p)?),
            },
            kind::SUBMITTED => Self::Submitted {
                at: Nanos(take_i64(&mut p)?),
                client_id: take_str(&mut p)?,
                side: match take_u8(&mut p)? {
                    1 => Side::Buy,
                    2 => Side::Sell,
                    _ => return None,
                },
                limit_price: PriceTicks(take_i64(&mut p)?),
                qty: QtyLots(take_i64(&mut p)?),
                reduce_only: take_u8(&mut p)? != 0,
            },
            kind::OUTCOME => Self::Outcome {
                at: Nanos(take_i64(&mut p)?),
                client_id: take_str(&mut p)?,
                tag: OutcomeTag::from_u8(take_u8(&mut p)?)?,
                detail: take_str(&mut p)?,
            },
            kind::FILL => Self::Fill {
                at: Nanos(take_i64(&mut p)?),
                client_id: take_str(&mut p)?,
                trade_id: take_i64(&mut p)?,
                qty: take_str(&mut p)?,
                price: take_str(&mut p)?,
            },
            kind::REFUSED => Self::Refused {
                at: Nanos(take_i64(&mut p)?),
                breach: take_str(&mut p)?,
            },
            kind::WAITING => {
                let at = Nanos(take_i64(&mut p)?);
                let n = take_i64(&mut p)?;
                let mut entries = Vec::new();
                for _ in 0..n.max(0) {
                    entries.push((take_str(&mut p)?, take_i64(&mut p)?));
                }
                Self::Waiting { at, entries }
            }
            kind::RECONCILED => {
                let at = Nanos(take_i64(&mut p)?);
                let n = take_i64(&mut p)?;
                let mut legs = Vec::new();
                for _ in 0..n.max(0) {
                    legs.push((
                        take_str(&mut p)?,
                        take_str(&mut p)?,
                        take_i64(&mut p)?,
                        take_i64(&mut p)?,
                    ));
                }
                Self::Reconciled { at, legs }
            }
            _ => return None,
        })
    }
}

fn put_i64(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&bytes[..len as usize]);
}

fn take_u8(p: &mut &[u8]) -> Option<u8> {
    let (a, rest) = p.split_first()?;
    *p = rest;
    Some(*a)
}

fn take_i64(p: &mut &[u8]) -> Option<i64> {
    if p.len() < 8 {
        return None;
    }
    let (a, rest) = p.split_at(8);
    *p = rest;
    Some(i64::from_le_bytes(a.try_into().ok()?))
}

fn take_str(p: &mut &[u8]) -> Option<String> {
    if p.len() < 4 {
        return None;
    }
    let (l, rest) = p.split_at(4);
    let len = u32::from_le_bytes(l.try_into().ok()?) as usize;
    if rest.len() < len {
        return None;
    }
    let (s, rest) = rest.split_at(len);
    *p = rest;
    String::from_utf8(s.to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(r: &Record) {
        let bytes = r.encode();
        let back = Record::decode(r.kind(), &bytes).expect("decodes");
        assert_eq!(&back, r, "round trip");
    }

    #[test]
    fn every_record_survives_a_round_trip() {
        roundtrip(&Record::SessionStart {
            prefix: "oq123".into(),
            symbol: "ETHUSDT".into(),
            price_scale: 2,
            qty_scale: 3,
        });
        roundtrip(&Record::Tick {
            at: Nanos(1_786_976_849_705_000_000),
            last: PriceTicks(300_000),
            bid: PriceTicks(299_990),
            ask: PriceTicks(300_010),
            volume: QtyLots(1234),
        });
        roundtrip(&Record::Submitted {
            at: Nanos(7),
            client_id: "oq123-1".into(),
            side: Side::Sell,
            limit_price: PriceTicks(299_000),
            qty: QtyLots(8),
            reduce_only: true,
        });
        roundtrip(&Record::Outcome {
            at: Nanos(8),
            client_id: "oq123-1".into(),
            tag: OutcomeTag::Unknown,
            detail: "timed out".into(),
        });
        roundtrip(&Record::Fill {
            at: Nanos(9),
            client_id: "oq123-1".into(),
            trade_id: 481_923,
            qty: "0.008".into(),
            price: "2999.90".into(),
        });
        roundtrip(&Record::Refused {
            at: Nanos(10),
            breach: "Halted".into(),
        });
        roundtrip(&Record::Waiting {
            at: Nanos(12),
            entries: vec![("bars".into(), 15), ("volume_gate".into(), 1)],
        });
        roundtrip(&Record::Reconciled {
            at: Nanos(11),
            legs: vec![
                ("ETHUSDT".into(), "LONG".into(), 160, 250_000),
                ("ETHUSDT".into(), "SHORT".into(), -40, 251_500),
            ],
        });
    }

    #[test]
    fn a_market_order_is_a_zero_limit_price_and_reads_back_as_one() {
        // The side and quantity describe it fully; zero is the sentinel
        // the tick format already uses, so a reader has one convention
        // to learn rather than two.
        let r = Record::Submitted {
            at: Nanos(1),
            client_id: "oq-1".into(),
            side: Side::Buy,
            limit_price: PriceTicks(0),
            qty: QtyLots(1),
            reduce_only: false,
        };
        roundtrip(&r);
    }

    #[test]
    fn a_truncated_record_decodes_to_nothing_rather_than_to_garbage() {
        // The normal shape of a process that died mid-write. Reading past
        // the end would invent an order that was never submitted, which
        // is worse than reporting the end of the journal.
        let full = Record::Submitted {
            at: Nanos(1),
            client_id: "oq123-1".into(),
            side: Side::Buy,
            limit_price: PriceTicks(5),
            qty: QtyLots(1),
            reduce_only: false,
        }
        .encode();
        for cut in 1..full.len() {
            assert!(
                Record::decode(kind::SUBMITTED, &full[..cut]).is_none(),
                "a payload cut at {cut} must not decode"
            );
        }
    }

    #[test]
    fn an_unknown_kind_is_skipped_rather_than_misread() {
        // A reader from an older build meeting a record it does not know
        // must skip it. Guessing at the payload of an unknown kind is how
        // a format stops being forward compatible.
        assert!(Record::decode(9999, b"anything").is_none());
    }

    #[test]
    fn an_unknown_outcome_tag_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&7i64.to_le_bytes());
        bytes.extend_from_slice(&5u32.to_le_bytes());
        bytes.extend_from_slice(b"oq-1a");
        bytes.push(77); // not a tag this build knows
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert!(Record::decode(kind::OUTCOME, &bytes).is_none());
    }

    #[test]
    fn the_kinds_are_distinct_and_stable() {
        // Reusing a number would make an old reader misread a new record
        // as an old one, which is the failure the numbering exists to
        // prevent.
        let all = [
            kind::SESSION_START,
            kind::TICK,
            kind::SUBMITTED,
            kind::OUTCOME,
            kind::FILL,
            kind::REFUSED,
            kind::RECONCILED,
        ];
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(unique.len(), all.len());
        assert_eq!(all, [1, 2, 3, 4, 5, 6, 7], "numbers are part of the format");
    }
}
