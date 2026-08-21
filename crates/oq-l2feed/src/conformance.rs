//! What any venue adapter must do, expressed as something that runs.
//!
//! The [`Venue`](crate::venue::Venue) trait says what an adapter provides
//! and cannot say what it must *mean*. Two adapters can both compile and
//! disagree about whether a size is a quantity, whether a sequence number
//! is inclusive, whether a snapshot has a predecessor. Those disagreements
//! do not fail — they produce archives that parse and mean something else.
//!
//! This is the missing half: a suite an adapter is driven through, using
//! payloads it supplies from its own venue. It is what makes "a second
//! venue is an implementation rather than a rewrite" a claim with a test
//! behind it instead of an aspiration.
//!
//! # Why the samples come from the adapter
//!
//! A suite carrying its own fixtures would be a suite testing one venue's
//! wire format. Every venue's bytes are different and that is the point of
//! the seam, so each adapter brings a recorded message of each kind and
//! states what that message means. The suite checks the adapter against
//! its own stated meaning, which is the only thing an outside test can
//! honestly check.
//!
//! # What this does not cover
//!
//! Anything requiring a network. Subscription acknowledgement, keepalive
//! and reconnection are contract terms too, and they are exercised by
//! `oq-sim`'s corpus and by running the capture. A conformance suite that
//! needed a venue would be a suite nobody runs on a laptop, and one nobody
//! runs is one that stops being true.

use crate::depth::Scales;
use crate::venue::Venue;

/// A recorded message and what the adapter says it means.
///
/// The expectations are the adapter's own claims. The suite's job is to
/// hold it to them, not to know what a venue's bytes should contain.
pub struct Samples {
    /// The symbol these samples are for, in the venue's own spelling.
    pub symbol: &'static str,
    /// One depth update, verbatim.
    pub depth: &'static [u8],
    /// The first and last sequence numbers that update carries.
    pub depth_ids: (u64, Option<u64>),
    /// One trade, verbatim.
    pub trade: &'static [u8],
    /// Price and quantity that trade carries, in instrument units.
    pub trade_price_qty: (i64, i64),
    /// Which side crossed the spread in that trade.
    ///
    /// The adapter's own claim about its own sample. A venue that
    /// publishes the aggressor and an adapter that drops it look
    /// identical downstream — the order flow is simply absent — so the
    /// suite asks each adapter to state it and holds it to that.
    pub trade_aggressor: oq_types::Side,
    /// The exchange timestamp both messages carry, in nanoseconds.
    pub event_time_ns: i64,
    /// A payload that is not a message of any kind this adapter parses.
    pub not_a_message: &'static [u8],
    /// A message in this venue's own shape that declares no trade.
    ///
    /// Separate from `not_a_message` because it is not malformed: it
    /// parses, and what it parses to is nothing. Binance really
    /// publishes these; whether a given venue does is beside the point,
    /// since an adapter that would pass a zero price through is wrong
    /// whether or not its venue has yet sent one.
    pub non_trade: &'static [u8],
}

/// What the suite found.
#[derive(Debug, Default)]
pub struct Report {
    pub checks: usize,
    pub failures: Vec<String>,
}

impl Report {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    fn check(&mut self, ok: bool, what: impl FnOnce() -> String) {
        self.checks += 1;
        if !ok {
            self.failures.push(what());
        }
    }
}

/// Drive `venue` through the contract, using its own samples.
///
/// Returns every failure rather than the first. An adapter under
/// development is usually wrong in several ways at once, and a suite that
/// stops at the first turns one afternoon into several.
#[must_use]
pub fn check(venue: &dyn Venue, s: &Samples) -> Report {
    let mut r = Report::default();

    // The instrument has to exist, because everything else is scaled by
    // it. An adapter that parses without one is parsing at a guessed
    // scale, which is the failure that reads as a market that moved.
    let Some(instrument) = venue.instrument(s.symbol) else {
        r.failures.push(format!(
            "{}: no instrument definition for {}, so every price and size below \
             would be parsed at a guessed scale",
            venue.id(),
            s.symbol
        ));
        return r;
    };
    r.checks += 1;
    let scales = Scales {
        price: u32::from(instrument.price_scale),
        qty: u32::from(instrument.qty_scale),
    };

    r.check(instrument.contract_size > 0, || {
        format!(
            "{}: contract size is {}, so a quantity has no stated unit",
            venue.id(),
            instrument.contract_size
        )
    });
    r.check(instrument.tick_cash().is_some(), || {
        format!(
            "{}: {} has no representable cash per tick, so no notional can be computed",
            venue.id(),
            s.symbol
        )
    });

    // Depth: the ids the adapter claims, and the chaining convention.
    match venue.parse_depth(s.depth, scales) {
        Ok(update) => {
            r.check(update.first_id == s.depth_ids.0, || {
                format!(
                    "{}: depth first id is {}, the sample says {}",
                    venue.id(),
                    update.first_id,
                    s.depth_ids.0
                )
            });
            r.check(update.prev_final_id == s.depth_ids.1, || {
                format!(
                    "{}: depth predecessor is {:?}, the sample says {:?}",
                    venue.id(),
                    update.prev_final_id,
                    s.depth_ids.1
                )
            });
            r.check(update.final_id >= update.first_id, || {
                format!(
                    "{}: depth range runs backwards, {} to {}",
                    venue.id(),
                    update.first_id,
                    update.final_id
                )
            });
            // A predecessor equal to this update's own last id would make
            // every update follow itself, and a gap check built on it
            // would never fire.
            r.check(update.prev_final_id != Some(update.final_id), || {
                format!(
                    "{}: an update whose predecessor is its own final id chains to itself",
                    venue.id()
                )
            });
        }
        Err(e) => r.failures.push(format!(
            "{}: its own depth sample does not parse: {e:?}",
            venue.id()
        )),
    }

    // Trades: the numbers, at the instrument's scale.
    match venue.parse_trade(s.trade, scales) {
        Some(t) => {
            r.check(t.price == s.trade_price_qty.0, || {
                format!(
                    "{}: trade price parsed as {}, the sample says {}",
                    venue.id(),
                    t.price,
                    s.trade_price_qty.0
                )
            });
            r.check(t.qty == s.trade_price_qty.1, || {
                format!(
                    "{}: trade qty parsed as {}, the sample says {}",
                    venue.id(),
                    t.qty,
                    s.trade_price_qty.1
                )
            });
            r.check(t.aggressor == Some(s.trade_aggressor), || {
                format!(
                    "{}: aggressor parsed as {:?}, the sample says {:?}",
                    venue.id(),
                    t.aggressor,
                    s.trade_aggressor
                )
            });
            r.check(t.price > 0 && t.qty > 0, || {
                format!(
                    "{}: a trade at {} for {} is not one",
                    venue.id(),
                    t.price,
                    t.qty
                )
            });
        }
        None => r.failures.push(format!(
            "{}: its own trade sample does not parse",
            venue.id()
        )),
    }

    // Trade ids: what proves nothing was missed.
    let ids = venue.trade_ids(s.trade);
    r.check(!ids.is_empty(), || {
        format!(
            "{}: no trade id in its own trade sample, so completeness cannot be proven",
            venue.id()
        )
    });

    // Event time, from both the method and the function pointer, because
    // the capture path uses the pointer and a divergence between them
    // would file records under a different day than the one checked here.
    r.check(
        venue.event_time_ns(s.depth) == Some(s.event_time_ns),
        || {
            format!(
                "{}: depth event time is {:?}, the sample says {}",
                venue.id(),
                venue.event_time_ns(s.depth),
                s.event_time_ns
            )
        },
    );
    let reader = venue.event_time_reader();
    r.check(reader(s.depth) == venue.event_time_ns(s.depth), || {
        format!(
            "{}: the event time reader and the method disagree, so a record would be \
             filed under a different day than the one checked",
            venue.id()
        )
    });

    // Something that is not a message must be refused rather than parsed
    // into zeros. A zero price that reaches a book is a book with a zero
    // in it.
    r.check(venue.parse_depth(s.not_a_message, scales).is_err(), || {
        format!("{}: parsed a non-message as depth", venue.id())
    });
    // A record the venue publishes on the trade stream that is not a
    // trade. One zero price reaching a window makes its low zero, and a
    // resting buy is triggered by the low -- so this is the difference
    // between a backtest that fills orders the venue would have filled
    // and one that does not.
    r.check(venue.parse_trade(s.non_trade, scales).is_none(), || {
        format!("{}: a message declaring no trade parsed as one", venue.id())
    });

    r.check(venue.parse_trade(s.not_a_message, scales).is_none(), || {
        format!("{}: parsed a non-message as a trade", venue.id())
    });
    r.check(venue.event_time_ns(s.not_a_message).is_none(), || {
        format!("{}: found an event time in a non-message", venue.id())
    });

    // Streams and transports: named, non-empty, and pointing somewhere.
    let streams = venue.streams(s.symbol);
    r.check(!streams.is_empty(), || {
        format!("{}: publishes no streams", venue.id())
    });
    for spec in &streams {
        let t = venue.transport(spec);
        r.check(
            t.url.starts_with("wss://") || t.url.starts_with("ws://"),
            || format!("{}: stream {} has url {:?}", venue.id(), spec.name, t.url),
        );
        r.check(!spec.topic.is_empty(), || {
            format!("{}: stream {} has an empty topic", venue.id(), spec.name)
        });
    }

    // A day. The default is the clock's day and a venue may override it;
    // either way the same timestamp must land in the same window twice.
    let ts = 20_000 * 24 * 3_600_000_000_000i64 + 13 * 3_600_000_000_000;
    for rotation in [crate::day::Rotation::Daily, crate::day::Rotation::Hourly] {
        r.check(
            venue.window_of(ts, rotation) == venue.window_of(ts, rotation),
            || {
                format!(
                    "{}: window_of is not a function of its arguments",
                    venue.id()
                )
            },
        );
    }

    r
}
