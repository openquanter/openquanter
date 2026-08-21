//! The two conventions that are easy to get backwards get a test each,
//! because both failures are silent: a wrong extreme produces a plausible
//! number, and a wrong volume convention halves a difference nobody
//! checks.

use super::*;
use oq_l2feed::frame::{Kind, Record};
use oq_l2feed::venue::binance::BinancePerp;

const SECOND: i64 = 1_000_000_000;
const T0: i64 = 1_786_000_000_000_000_000;

fn scales() -> Scales {
    Scales { price: 2, qty: 3 }
}

fn trade(at: i64, price: &str, qty: &str) -> Record {
    Record {
        kind: Kind::Payload,
        local_ts: at,
        exch_ts: at,
        payload: format!(r#"{{"e":"trade","E":{at},"p":"{price}","q":"{qty}"}}"#).into_bytes(),
    }
}

fn convert(records: &[Record], window: i64) -> (Vec<Tick>, Report) {
    to_ticks(
        &BinancePerp::new(),
        &[Source {
            records,
            stream: "trade",
        }],
        scales(),
        window,
    )
    .expect("convert")
}

#[test]
fn extremes_belong_to_their_own_window() {
    // A rising window then a falling one. If the high were carried
    // forward, the second window would report the first window's peak —
    // the exact shape that filled a take-profit 1506 points off the
    // market in the engine's own history.
    let records = vec![
        trade(T0, "100.00", "1.000"),
        trade(T0 + SECOND / 2, "110.00", "1.000"),
        trade(T0 + SECOND, "105.00", "1.000"),
        trade(T0 + SECOND + SECOND / 2, "95.00", "1.000"),
    ];
    let (ticks, report) = convert(&records, SECOND);

    assert_eq!(ticks.len(), 2);
    assert_eq!(report.trades, 4);

    assert_eq!(ticks[0].high.0, 11_000, "first window peaked at 110");
    assert_eq!(ticks[0].low.0, 10_000);

    assert_eq!(
        ticks[1].high.0, 10_500,
        "second window's high is its own 105, not the 110 that came before"
    );
    assert_eq!(ticks[1].low.0, 9_500);
    assert_eq!(
        ticks[1].last.0, 9_500,
        "last is the final trade, not the extreme"
    );
}

#[test]
fn volume_accumulates_so_differences_are_per_window() {
    let records = vec![
        trade(T0, "100.00", "1.500"),
        trade(T0 + SECOND, "100.00", "2.000"),
        trade(T0 + 2 * SECOND, "100.00", "0.500"),
    ];
    let (ticks, _) = convert(&records, SECOND);
    assert_eq!(ticks.len(), 3);

    // Running total, at qty scale 3.
    assert_eq!(ticks[0].volume.0, 1_500);
    assert_eq!(ticks[1].volume.0, 3_500);
    assert_eq!(ticks[2].volume.0, 4_000);

    // Which is what makes the documented reading convention work.
    let per_window: Vec<i64> = ticks
        .windows(2)
        .map(|w| w[1].volume.0 - w[0].volume.0)
        .collect();
    assert_eq!(per_window, vec![2_000, 500]);
}

#[test]
fn a_stretch_with_no_events_produces_no_windows() {
    // Windows exist where events land; a silent stretch is a hole in the
    // series rather than a run of empty ticks. Named for what it does,
    // because a consumer stepping through ticks has to know that the
    // stamp can jump.
    let records = vec![
        trade(T0, "100.00", "1.000"),
        trade(T0 + 3 * SECOND, "101.00", "1.000"),
    ];
    let (ticks, report) = convert(&records, SECOND);

    assert_eq!(ticks.len(), 2, "two events, two windows, nothing between");
    assert_eq!(report.quiet_windows, 0);
    assert_eq!(
        ticks[1].stamp.exch.0 - ticks[0].stamp.exch.0,
        3 * SECOND,
        "the gap shows up as a jump in the stamp"
    );
}

#[test]
fn a_window_of_trades_still_reports_the_book() {
    // Found by reviewing this crate rather than by a failing run: top of
    // book was recorded only when a depth update landed inside a window,
    // so a window holding trades and no depth update reported
    // `bid = ask = 0`. The engine reads zero as "unknown" and falls back
    // to trade prices, so the quote was silently discarded — invisible
    // on a feed like depth@0ms, where every window happens to contain an
    // update, and wrong everywhere else.
    let depth = Record {
        kind: Kind::Payload,
        local_ts: T0,
        exch_ts: T0,
        payload: br#"{"e":"depthUpdate","E":1,"U":1,"u":1,"b":[["99.00","1.000"]],"a":[["101.00","1.000"]]}"#.to_vec(),
    };
    let trades = vec![trade(T0 + 2 * SECOND, "100.00", "1.000")];

    let (ticks, _) = to_ticks(
        &BinancePerp::new(),
        &[
            Source {
                records: core::slice::from_ref(&depth),
                stream: "depth",
            },
            Source {
                records: &trades,
                stream: "trade",
            },
        ],
        scales(),
        SECOND,
    )
    .expect("convert");

    let last = ticks.last().expect("a window for the trade");
    assert_eq!(last.last.0, 10_000);
    assert_eq!(last.bid.0, 9_900, "the book was known and must be carried");
    assert_eq!(last.ask.0, 10_100);
}

#[test]
fn an_unreadable_payload_is_counted_rather_than_fatal() {
    let mut records = vec![trade(T0, "100.00", "1.000")];
    records.push(Record {
        kind: Kind::Payload,
        local_ts: T0 + 1,
        exch_ts: T0 + 1,
        payload: br#"{"e":"trade","E":1,"p":"not a number","q":"1.0"}"#.to_vec(),
    });
    let (ticks, report) = convert(&records, SECOND);

    assert_eq!(report.unparseable, 1);
    assert_eq!(report.trades, 1);
    assert_eq!(
        ticks.len(),
        1,
        "the readable trade still produced its window"
    );
}

#[test]
fn depth_supplies_top_of_book_and_a_gap_clears_it() {
    let depth = |at: i64, first: u64, last: u64, bid: &str, ask: &str| {
        Record {
        kind: Kind::Payload,
        local_ts: at,
        exch_ts: at,
        payload: format!(
            r#"{{"e":"depthUpdate","E":{at},"U":{first},"u":{last},"b":[["{bid}","1.000"]],"a":[["{ask}","1.000"]]}}"#
        )
        .into_bytes(),
    }
    };
    // A trade opens the stream. Without one there is no price to
    // publish and no tick is produced at all — which is the subject of
    // `depth_alone_publishes_nothing` below, not of this test.
    let records = [
        trade(T0, "100.00", "1.000"),
        depth(T0, 1, 1, "99.00", "101.00"),
        depth(T0 + SECOND, 2, 2, "99.50", "100.50"),
        Record::control(
            T0 + 2 * SECOND,
            oq_l2feed::manifest::control::gap("test", None, 0),
        ),
        depth(T0 + 3 * SECOND, 9, 9, "98.00", "102.00"),
    ];
    let (ticks, report) = to_ticks(
        &BinancePerp::new(),
        &[
            Source {
                records: &records[..1],
                stream: "trade",
            },
            Source {
                records: &records[1..],
                stream: "depth",
            },
        ],
        scales(),
        SECOND,
    )
    .expect("convert");

    assert_eq!(report.gaps, 1);
    assert!(report.depth_applied >= 3);
    assert_eq!(ticks[0].bid.0, 9_900);
    assert_eq!(ticks[0].ask.0, 10_100);
    assert_eq!(ticks[1].bid.0, 9_950);

    // After the gap the book was dropped and rebuilt, so the last window
    // carries the new book rather than a stale quote from before it.
    let last = ticks.last().expect("a window after the gap");
    assert_eq!(last.bid.0, 9_800);
    assert_eq!(last.ask.0, 10_200);
}

#[test]
fn a_zero_window_is_rejected_rather_than_dividing_by_it() {
    assert!(to_ticks(&BinancePerp::new(), &[], scales(), 0).is_err());
}

/// Depth alone publishes nothing.
///
/// A tick's `last` becomes the kernel's mark price with no guard, so a
/// book-only stream would set the mark to zero on every window. The
/// reference implementation reaches the same conclusion from the other
/// direction: its depth branch never calls `on_tick` at all, and each of
/// its four publish sites is guarded by `last_price > 0`.
///
/// This is a real limitation rather than a workaround. A capture with no
/// trades has no traded price in it, and inventing one from the book
/// would put a number in the mark that no trade produced.
#[test]
fn depth_alone_publishes_nothing() {
    let depth = |at: i64, first: u64, last: u64, bid: &str, ask: &str| {
        Record {
        kind: Kind::Payload,
        local_ts: at,
        exch_ts: at,
        payload: format!(
            r#"{{"e":"depthUpdate","E":{at},"U":{first},"u":{last},"b":[["{bid}","1.000"]],"a":[["{ask}","1.000"]]}}"#
        )
        .into_bytes(),
    }
    };
    let records = vec![
        depth(T0, 1, 1, "99.00", "101.00"),
        depth(T0 + SECOND, 2, 2, "99.50", "100.50"),
        depth(T0 + 2 * SECOND, 3, 3, "99.75", "100.25"),
    ];
    let (ticks, report) = to_ticks(
        &BinancePerp::new(),
        &[Source {
            records: &records,
            stream: "depth",
        }],
        scales(),
        SECOND,
    )
    .expect("convert");

    assert!(report.depth_applied >= 3, "the book was still built");
    assert!(
        ticks.is_empty(),
        "no trade means no price; publishing would set the mark to zero: {ticks:?}"
    );
}

/// The dropped windows are counted, and the count reaches the report.
///
/// Both halves matter, and the second half is the one that broke. The
/// counter existed on the aggregator and was correct there; the report
/// was assembled field by field in two places, and the binary's copy
/// was missed — so `oq-ingest` printed nothing, and would have gone on
/// printing nothing, because a field that is never assigned reads zero
/// rather than failing.
///
/// That number is the difference between the windows a capture crossed
/// and the ticks it wrote. Anyone comparing a re-converted file against
/// an older one needs it, and should not have to derive it.
#[test]
fn windows_dropped_before_the_first_trade_reach_the_report() {
    let depth = |at: i64, first: u64, last: u64| {
        Record {
        kind: Kind::Payload,
        local_ts: at,
        exch_ts: at,
        payload: format!(
            r#"{{"e":"depthUpdate","E":{at},"U":{first},"u":{last},"b":[["99.00","1.000"]],"a":[["101.00","1.000"]]}}"#
        )
        .into_bytes(),
    }
    };
    // Three windows of book with no trade in them, then a trade.
    let books = [
        depth(T0, 1, 1),
        depth(T0 + SECOND, 2, 2),
        depth(T0 + 2 * SECOND, 3, 3),
    ];
    let trades = [trade(T0 + 3 * SECOND, "100.00", "1.000")];
    let (ticks, report) = to_ticks(
        &BinancePerp::new(),
        &[
            Source {
                records: &books,
                stream: "depth",
            },
            Source {
                records: &trades,
                stream: "trade",
            },
        ],
        scales(),
        SECOND,
    )
    .expect("convert");

    assert_eq!(
        report.windows_before_first_trade, 3,
        "three windows were crossed before the trade stream said anything"
    );
    assert!(
        ticks.iter().all(|t| t.last.0 > 0),
        "and none of them was published: {ticks:?}"
    );
    // The identity a reader compares two conversions with.
    assert_eq!(report.ticks, ticks.len() as u64);
}

// ---- The depth-carrying fold ----

fn depth(at: i64, first: u64, last: u64, prev: u64, bid: &str, qty: &str) -> Record {
    Record {
        kind: Kind::Payload,
        local_ts: at,
        exch_ts: at,
        payload: format!(
            r#"{{"e":"depthUpdate","E":{at},"T":{at},"s":"BTCUSDT","U":{first},"u":{last},"pu":{prev},"b":[["{bid}","{qty}"]],"a":[]}}"#
        )
        .into_bytes(),
    }
}

/// The two folds must project the same ticks.
///
/// One of them is what a strategy sees and the other is what the
/// matcher is driven by; if they disagree about what happened, a run
/// reports fills against observations its strategy never saw. That is
/// not a difference anyone would notice from the outside — both files
/// look plausible — which is why it is asserted rather than assumed.
#[test]
fn both_folds_produce_the_same_ticks() {
    let venue = BinancePerp::new();
    let trades = [
        trade(T0, "100.00", "1.000"),
        trade(T0 + SECOND / 2, "101.00", "2.000"),
        trade(T0 + 2 * SECOND, "99.00", "1.500"),
    ];
    let depths = [
        depth(T0, 1, 1, 0, "99.50", "10.000"),
        depth(T0 + SECOND, 2, 2, 1, "99.40", "20.000"),
    ];
    let sources = [
        Source {
            records: &trades,
            stream: "trade",
        },
        Source {
            records: &depths,
            stream: "depth",
        },
    ];

    let mut agg_a = Aggregator::new(SECOND).expect("window");
    let mut report_a = Report::default();
    let ticks = fold_into(&venue, &sources, scales(), &mut agg_a, &mut report_a);

    let mut agg_b = Aggregator::new(SECOND).expect("window");
    let mut report_b = Report::default();
    let observed = fold_into_observations(&venue, &sources, scales(), &mut agg_b, &mut report_b);

    let from_observations: Vec<Tick> = observed
        .iter()
        .filter_map(|o| match o {
            Observation::Tick(t) => Some(*t),
            _ => None,
        })
        .collect();

    assert!(!ticks.is_empty(), "the fixture must produce ticks");
    assert_eq!(from_observations, ticks);
    assert_eq!(report_b.depth_applied, report_a.depth_applied);
}

/// An update inside an open window reaches the book before that
/// window's tick.
///
/// The book moved before the window closed, so that is where it goes.
#[test]
fn an_update_inside_a_window_precedes_that_windows_tick() {
    let venue = BinancePerp::new();
    // The last trade is what closes the first window; without it the
    // batch ends with the window still open and produces no tick at
    // all, and an ordering assertion over nothing passes for the wrong
    // reason.
    let trades = [
        trade(T0, "100.00", "1.000"),
        trade(T0 + 2 * SECOND, "101.00", "1.000"),
    ];
    let depths = [depth(T0 + SECOND / 2, 1, 1, 0, "99.50", "10.000")];
    let sources = [
        Source {
            records: &trades,
            stream: "trade",
        },
        Source {
            records: &depths,
            stream: "depth",
        },
    ];

    let mut agg = Aggregator::new(SECOND).expect("window");
    let mut report = Report::default();
    let out = fold_into_observations(&venue, &sources, scales(), &mut agg, &mut report);

    let d = out
        .iter()
        .position(|o| matches!(o, Observation::Depth(_)))
        .expect("a depth update");
    let t = out
        .iter()
        .position(|o| matches!(o, Observation::Tick(_)))
        .expect("a tick, or this test asserts nothing");
    assert!(d < t, "update at {d} must precede its window's tick at {t}");
}

/// An update that *closes* a window comes after that window's tick.
///
/// It is the first event of the next window, and the tick it closed
/// summarises the one before. Emitting it first hands the matcher a
/// book from the next window and lets it match the previous one against
/// it -- the direction that flatters a backtest, and the reason this is
/// asserted separately from the case above.
#[test]
fn an_update_that_closes_a_window_follows_that_windows_tick() {
    let venue = BinancePerp::new();
    let trades = [trade(T0, "100.00", "1.000")];
    // Past the window boundary, so rolling to it closes the first
    // window and this update belongs to the second.
    let depths = [depth(T0 + 3 * SECOND / 2, 1, 1, 0, "99.50", "10.000")];
    let sources = [
        Source {
            records: &trades,
            stream: "trade",
        },
        Source {
            records: &depths,
            stream: "depth",
        },
    ];

    let mut agg = Aggregator::new(SECOND).expect("window");
    let mut report = Report::default();
    let out = fold_into_observations(&venue, &sources, scales(), &mut agg, &mut report);

    let t = out
        .iter()
        .position(|o| matches!(o, Observation::Tick(_)))
        .expect("a tick, or this test asserts nothing");
    let d = out
        .iter()
        .position(|o| matches!(o, Observation::Depth(_)))
        .expect("a depth update");
    assert!(
        t < d,
        "the closed window's tick at {t} must precede the update at {d} that closed it"
    );
}

/// Every depth update in the batch reaches the stream.
///
/// Dropping one is how a reconstruction develops a hole that produces
/// plausible queues, and the sequence check downstream would report it
/// as the venue's gap rather than ours.
#[test]
fn no_depth_update_is_lost_between_the_folds() {
    let venue = BinancePerp::new();
    let depths: Vec<Record> = (1..=8)
        .map(|i| {
            depth(
                T0 + i * SECOND / 4,
                i as u64,
                i as u64,
                (i - 1) as u64,
                "99.50",
                "10.000",
            )
        })
        .collect();
    let sources = [Source {
        records: &depths,
        stream: "depth",
    }];

    let mut agg = Aggregator::new(SECOND).expect("window");
    let mut report = Report::default();
    let out = fold_into_observations(&venue, &sources, scales(), &mut agg, &mut report);

    let emitted = out
        .iter()
        .filter(|o| matches!(o, Observation::Depth(_)))
        .count();
    assert_eq!(emitted, depths.len(), "every update must be forwarded");
}
