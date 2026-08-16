//! A market whose day is not the clock's day still gets one file per
//! day.
//!
//! The archive's central invariant is that a file holds one whole
//! period. Dividing by the UTC clock satisfies it for a market that
//! never closes and breaks it for every market that does: a futures
//! session that opens the evening before and runs past midnight is one
//! trading day and two UTC days, so the clock rule splits it in half and
//! every tool downstream inherits the split.
//!
//! This is a seam, not a calendar. No exchange calendar ships here —
//! writing one without a real venue to check it against would be
//! guessing. What is checked is that a venue *can* override the rule and
//! that the writer honours it, so that the assumption is not structural
//! by the time somebody has a market that needs it.

use core::time::Duration;

use oq_l2feed::day::{Rotation, UtcDay, Window};
use oq_l2feed::depth::{DepthUpdate, ParseError, Scales};
use oq_l2feed::frame::{Kind, Record};
use oq_l2feed::stream::{Software, StreamId};
use oq_l2feed::venue::{AckPolicy, Instrument, PollSpec, StreamSpec, Trade, Transport, Venue};
use oq_l2feed::writer::CaptureWriter;

const HOUR: i64 = 3_600_000_000_000;
const DAY: i64 = 24 * HOUR;

/// A venue whose trading day starts at 18:00 UTC the evening before,
/// the way an evening-open futures session does.
struct EveningOpen;

/// Records from 18:00 onward belong to the next calendar day, because
/// that is the session they are part of.
fn session_window(ts: i64, rotation: Rotation) -> Window {
    let shifted = ts + 6 * HOUR;
    let day = UtcDay::from_nanos(shifted);
    match rotation {
        Rotation::Daily => Window { day, hour: None },
        Rotation::Hourly => {
            let into = shifted - day.start_nanos();
            Window {
                day,
                hour: Some(u32::try_from(into / HOUR).unwrap_or(0)),
            }
        }
    }
}

impl Venue for EveningOpen {
    fn id(&self) -> &'static str {
        "evening-open"
    }
    fn streams(&self, _symbol: &str) -> Vec<StreamSpec> {
        Vec::new()
    }
    fn polls(&self, _symbol: &str) -> Vec<PollSpec> {
        Vec::new()
    }
    fn transport(&self, _spec: &StreamSpec) -> Transport {
        Transport {
            url: String::new(),
            subscribe: Vec::new(),
            ack: AckPolicy::None,
            keepalive: None,
        }
    }
    fn event_time_ns(&self, _payload: &[u8]) -> Option<i64> {
        None
    }
    fn event_time_reader(&self) -> fn(&[u8]) -> Option<i64> {
        |_| None
    }
    fn instrument(&self, _symbol: &str) -> Option<Instrument> {
        None
    }
    fn parse_trade(&self, _payload: &[u8], _scales: Scales) -> Option<Trade> {
        None
    }
    fn trade_ids(&self, _payload: &[u8]) -> Vec<u64> {
        Vec::new()
    }
    fn parse_depth(&self, _payload: &[u8], _scales: Scales) -> Result<DepthUpdate, ParseError> {
        Err(ParseError::NotDepth)
    }
    fn window_of(&self, ts: i64, rotation: Rotation) -> Window {
        session_window(ts, rotation)
    }
}

fn payload_at(ts: i64) -> Record {
    Record {
        kind: Kind::Payload,
        local_ts: ts,
        exch_ts: ts,
        payload: b"{}".to_vec(),
    }
}

#[test]
fn a_session_crossing_midnight_stays_one_file() {
    let base = 20_000 * DAY; // midnight UTC on some day
    let evening = base + 20 * HOUR; // 20:00, after the session opened
    let after_midnight = base + 26 * HOUR; // 02:00 the next calendar day

    // The clock rule puts these in different files, which is the
    // failure: one trading session, two archives.
    assert_ne!(
        Window::from_nanos(evening, Rotation::Daily),
        Window::from_nanos(after_midnight, Rotation::Daily),
        "the clock splits this session, which is what the seam exists to fix"
    );

    // The venue's rule keeps them together.
    assert_eq!(
        EveningOpen.window_of(evening, Rotation::Daily),
        EveningOpen.window_of(after_midnight, Rotation::Daily),
        "one session must be one window"
    );
}

#[test]
fn the_writer_honours_the_venue_rather_than_the_clock() {
    let dir = std::env::temp_dir().join(format!("oq-session-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let stream = StreamId::new("evening-open", "ESZ6", "trade");

    let base = 20_000 * DAY;
    let mut w = CaptureWriter::new(&dir, stream.clone(), Software::new("test", "unknown"))
        .expect("writer")
        .with_rotation(Rotation::Daily)
        .with_grace(Duration::from_secs(0))
        .with_windowing(session_window);

    // Evening, then past midnight: the same session on this venue.
    w.append(&payload_at(base + 20 * HOUR)).expect("evening");
    w.append(&payload_at(base + 26 * HOUR))
        .expect("after midnight");
    let sealed = w.seal().expect("seal");

    assert_eq!(
        sealed.manifest.records, 2,
        "both records belong to the session, so both are in its file"
    );

    let files: Vec<_> = walk(&dir)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "oqcap"))
        .collect();
    assert_eq!(
        files.len(),
        1,
        "one session is one file; the clock rule would have made two"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn a_venue_that_says_nothing_still_divides_by_the_clock() {
    // The default must be exactly today's behaviour, or adding the seam
    // would have changed how every existing archive is written.
    struct Continuous;
    impl Venue for Continuous {
        fn id(&self) -> &'static str {
            "continuous"
        }
        fn streams(&self, _s: &str) -> Vec<StreamSpec> {
            Vec::new()
        }
        fn polls(&self, _s: &str) -> Vec<PollSpec> {
            Vec::new()
        }
        fn transport(&self, _s: &StreamSpec) -> Transport {
            Transport {
                url: String::new(),
                subscribe: Vec::new(),
                ack: AckPolicy::None,
                keepalive: None,
            }
        }
        fn event_time_ns(&self, _p: &[u8]) -> Option<i64> {
            None
        }
        fn event_time_reader(&self) -> fn(&[u8]) -> Option<i64> {
            |_| None
        }
        fn instrument(&self, _s: &str) -> Option<Instrument> {
            None
        }
        fn parse_trade(&self, _p: &[u8], _s: Scales) -> Option<Trade> {
            None
        }
        fn trade_ids(&self, _p: &[u8]) -> Vec<u64> {
            Vec::new()
        }
        fn parse_depth(&self, _p: &[u8], _s: Scales) -> Result<DepthUpdate, ParseError> {
            Err(ParseError::NotDepth)
        }
    }

    let ts = 20_000 * DAY + 20 * HOUR;
    for rotation in [Rotation::Daily, Rotation::Hourly] {
        assert_eq!(
            Continuous.window_of(ts, rotation),
            Window::from_nanos(ts, rotation)
        );
    }
}
