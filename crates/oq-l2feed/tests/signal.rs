//! SIGTERM stops a capture cleanly.
//!
//! In its own integration binary on purpose. The shutdown flag is
//! process-global, so raising a signal inside the unit-test binary
//! would set it for every other test sharing that process and stop
//! their capture loops early — a flake that appears only under
//! parallelism and only sometimes.

#![allow(unsafe_code)]

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use oq_l2feed::day::Rotation;
use oq_l2feed::frame::{Kind, decode_all};
use oq_l2feed::session::{
    Clock, Connector, MessageSource, SessionConfig, StopReason, install_signal_handlers,
    run_with_clock, shutdown_requested,
};
use oq_l2feed::stream::{Software, StreamId};
use oq_l2feed::writer::CaptureWriter;

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now_ns(&self) -> i64 {
        self.0
    }
}

/// Emits payloads forever, raising SIGTERM once partway through.
///
/// Modelling a live stream matters here: the point is that a capture
/// with messages still arriving notices the signal, rather than only
/// noticing when the socket happens to go quiet.
struct SignallingSource {
    sent: usize,
    signal_after: usize,
}

impl MessageSource for SignallingSource {
    fn next_message(&mut self) -> io::Result<Vec<u8>> {
        self.sent += 1;
        if self.sent == self.signal_after {
            // SAFETY: raising a signal at ourselves, with a handler
            // already installed by the test.
            unsafe {
                libc::raise(libc::SIGTERM);
            }
        }
        Ok(format!(r#"{{"e":"depthUpdate","u":{}}}"#, self.sent).into_bytes())
    }
}

struct SignallingConnector {
    signal_after: usize,
}

impl Connector for SignallingConnector {
    type Source = SignallingSource;
    fn connect(&mut self) -> io::Result<Self::Source> {
        Ok(SignallingSource {
            sent: 0,
            signal_after: self.signal_after,
        })
    }
}

#[test]
fn sigterm_stops_the_capture_and_keeps_what_it_received() {
    let dir = tempdir();
    let stream = StreamId::new("binance-perp", "BTCUSDT", "depth");
    let mut config = SessionConfig::new(
        &dir,
        stream.clone(),
        software_for_writer(),
        "wss://example.invalid",
    );
    config.duration = Some(Duration::from_secs(60));
    config.flush_interval = Duration::from_secs(3600); // never flush on the timer

    install_signal_handlers();
    assert!(
        !shutdown_requested(),
        "flag must start clear or the test proves nothing"
    );

    let clock = FixedClock(1_786_000_000_000_000_000);
    let mut writer = CaptureWriter::new(&dir, stream, software_for_writer())
        .expect("writer should open")
        .with_rotation(Rotation::Daily);
    let mut connector = SignallingConnector { signal_after: 50 };

    let stats = run_with_clock(&config, &mut connector, &mut writer, &clock).expect("run");

    assert_eq!(
        stats.stop,
        StopReason::Signalled,
        "the loop must attribute the stop to the signal, not to a timeout"
    );
    assert!(shutdown_requested());

    // Sealing is what turns a killed capture into an archivable one:
    // the buffer reaches disk and the manifest records what is in it.
    let sealed = writer.seal().expect("seal after signal");
    assert_eq!(
        sealed.manifest.records,
        stats.payloads + 1,
        "manifest must count every payload plus the session-start record"
    );

    // The flush interval was set to an hour, so anything on disk got
    // there because the shutdown path wrote it, not because a timer
    // happened to fire.
    let bytes = std::fs::read(&sealed.path).expect("read sealed file");
    let (records, torn) = decode_all(&bytes).expect("sealed file must decode");
    assert_eq!(torn, 0, "a sealed file must not end mid-record");
    let payloads = records.iter().filter(|r| r.kind == Kind::Payload).count();
    assert_eq!(
        payloads as u64, stats.payloads,
        "every received payload must survive the signal"
    );
    assert!(
        payloads >= 50,
        "the signal fires at message 50, so at least that many were received"
    );
}

fn software_for_writer() -> Software {
    Software::new("oq-l2feed test", "unknown")
}

fn tempdir() -> PathBuf {
    let base = std::env::temp_dir().join(format!("oq-signal-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("create temp dir");
    base
}

/// A restart inside a window must not leave the manifest undercounting.
///
/// With hourly rotation every restart lands mid-window, so this is the
/// common case, not an edge one. A manifest that describes only the
/// records written since the last restart is worse than no manifest at
/// all: nothing downstream can tell that it is wrong, and its whole
/// purpose is to answer "is this hour complete".
#[test]
fn reopening_a_window_keeps_the_manifest_describing_the_whole_file() {
    let dir = tempdir().join("reopen");
    let _ = std::fs::remove_dir_all(&dir);
    let stream = StreamId::new("binance-perp", "ETHUSDT", "trade");
    let clock = FixedClock(1_786_000_000_000_000_000);

    let write_some = |n: usize| {
        let mut w = CaptureWriter::new(&dir, stream.clone(), software_for_writer())
            .expect("open writer")
            .with_rotation(Rotation::Daily);
        for i in 0..n {
            w.append(&oq_l2feed::frame::Record {
                kind: Kind::Payload,
                local_ts: clock.now_ns(),
                exch_ts: clock.now_ns(),
                payload: format!(r#"{{"i":{i}}}"#).into_bytes(),
            })
            .expect("append");
        }
        w.seal().expect("seal")
    };

    let first = write_some(10);
    assert_eq!(first.manifest.records, 10);

    // Second session appends to the same window's file.
    let second = write_some(7);
    assert_eq!(
        second.manifest.records, 17,
        "the manifest must count everything in the file, not just this session"
    );

    let bytes = std::fs::read(&second.path).expect("read file");
    let (records, torn) = decode_all(&bytes).expect("decode");
    assert_eq!(torn, 0);
    assert_eq!(
        records.len() as u64,
        second.manifest.records,
        "manifest and file must agree on how many records exist"
    );
}

/// A restart leaves a hole, and the manifest has to say so.
///
/// Until this was counted, `gaps: 0` meant "no disconnect while the
/// process was running" while reading as "nothing is missing". An
/// upgrade or a crash restart put a multi-minute hole in a window that
/// the manifest still described as complete -- the exact conclusion the
/// field exists to prevent a reader from drawing.
#[test]
fn a_restart_seam_is_counted_as_a_gap() {
    let dir = tempdir().join("seam");
    let _ = std::fs::remove_dir_all(&dir);
    let stream = StreamId::new("binance-perp", "BTCUSDT", "trade");

    const SECOND: i64 = 1_000_000_000;
    let t0 = 1_786_000_000_000_000_000i64;

    let session = |start: i64, n: i64| {
        let mut w = CaptureWriter::new(&dir, stream.clone(), software_for_writer())
            .expect("open")
            .with_rotation(Rotation::Daily);
        w.append_session_start(start).expect("session start");
        for i in 0..n {
            w.append(&oq_l2feed::frame::Record {
                kind: Kind::Payload,
                local_ts: start + i * SECOND,
                exch_ts: start + i * SECOND,
                payload: b"{}".to_vec(),
            })
            .expect("append");
        }
        w.seal().expect("seal")
    };

    let first = session(t0, 5);
    assert_eq!(
        first.manifest.gaps, 0,
        "a window opened fresh has nothing before it to be missing"
    );

    // Restart 90 seconds after the first session's last record.
    let last_of_first = t0 + 4 * SECOND;
    let restart_at = last_of_first + 90 * SECOND;
    let second = session(restart_at, 5);

    assert_eq!(
        second.manifest.gaps, 1,
        "the silence between two sessions in one window is a gap"
    );
    assert_eq!(
        second.manifest.gap_ns_total,
        90 * SECOND,
        "and its length is the silence, measured from the last record to the restart"
    );

    // The seam must be in the stream, not only in the manifest beside
    // it. A replay tool reads the file; when the gap lived only in the
    // manifest, an order-book check called a known, recorded loss
    // "silent" -- correctly, because nothing in the bytes said
    // otherwise.
    let bytes = std::fs::read(&second.path).expect("read sealed file");
    let (records, _torn) = decode_all(&bytes).expect("decode");
    let markers = records
        .iter()
        .filter(|r| r.kind == Kind::Control && oq_l2feed::manifest::is_gap(r))
        .count();
    assert_eq!(
        markers, 1,
        "the restart seam must appear in the stream as a gap marker"
    );
}
