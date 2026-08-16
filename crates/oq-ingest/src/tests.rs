//! The two conventions that are easy to get backwards get a test each,
//! because both failures are silent: a wrong extreme produces a plausible
//! number, and a wrong volume convention halves a difference nobody
//! checks.

use super::*;
use oq_l2feed::frame::{Kind, Record};

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
    let records = vec![
        depth(T0, 1, 1, "99.00", "101.00"),
        depth(T0 + SECOND, 2, 2, "99.50", "100.50"),
        Record::control(
            T0 + 2 * SECOND,
            oq_l2feed::manifest::control::gap("test", None, 0),
        ),
        depth(T0 + 3 * SECOND, 9, 9, "98.00", "102.00"),
    ];
    let (ticks, report) = to_ticks(
        &[Source {
            records: &records,
            stream: "depth",
        }],
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
    assert!(to_ticks(&[], scales(), 0).is_err());
}
