//! The capture loop.
//!
//! One thread, one stream, blocking reads. No async runtime: a capture
//! process follows a handful of streams and spends its life waiting on
//! sockets, so threads are the simpler tool and their scheduling is
//! easier to reason about when something goes wrong at 3am.
//!
//! What the loop is responsible for, in the order the responsibilities
//! matter:
//!
//! 1. Never lose a message it has received — receive, stamp, write.
//! 2. Never lie about what it did not receive — every disconnect leaves
//!    a gap marker in the stream.
//! 3. Never take the host down — it stops itself when free space falls
//!    to the floor, rather than filling the disk under whatever else
//!    runs on the machine.
//!
//! Reconnection is not clever on purpose. Fixed backoff, a gap marker,
//! and a fresh snapshot: an exponential ladder tuned by nobody tends to
//! be either too slow to recover or fast enough to get rate-limited at
//! the worst moment.

use std::sync::atomic::{AtomicBool, Ordering};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::frame::Record;
use crate::stream::{Software, StreamId};
use crate::venue::binance_event_time_ns;
use crate::writer::CaptureWriter;

/// How the session decides when to stop.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Archive root.
    pub root: PathBuf,
    /// Which stream is being captured.
    pub stream: StreamId,
    /// Capture software identity, archived in every session record.
    pub software: Software,
    /// WebSocket URL to connect to.
    pub url: String,
    /// Stop after this long. `None` runs until interrupted.
    pub duration: Option<Duration>,
    /// Stop when free space falls below this many bytes.
    pub disk_floor_bytes: u64,
    /// How often to check free space, in records.
    pub disk_check_every: u64,
    /// How long buffered records may stay in memory before being
    /// written out.
    ///
    /// Flushing only on a record count is not enough: a stream that
    /// produces one message a second would hold sixteen minutes of data
    /// in a buffer, where a crash loses it *silently* — the messages
    /// were received, so nothing marks them missing. It also leaves an
    /// operator unable to tell a working low-rate capture from one that
    /// never connected, because the file stays empty either way.
    pub flush_interval: Duration,
    /// Wait between reconnection attempts.
    pub reconnect_wait: Duration,
    /// Give up after this many consecutive failed connections.
    pub max_consecutive_failures: u32,
}

impl SessionConfig {
    /// A configuration with defaults that are safe on a shared host: a
    /// 10 GiB floor, checked every thousand records.
    #[must_use]
    pub fn new(
        root: impl Into<PathBuf>,
        stream: StreamId,
        software: Software,
        url: impl Into<String>,
    ) -> Self {
        Self {
            root: root.into(),
            stream,
            software,
            url: url.into(),
            duration: None,
            disk_floor_bytes: 10 * 1024 * 1024 * 1024,
            disk_check_every: 1_000,
            flush_interval: Duration::from_secs(5),
            reconnect_wait: Duration::from_secs(2),
            max_consecutive_failures: 10,
        }
    }
}

/// Why a session stopped. Never "it just ended".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The configured duration elapsed.
    DurationElapsed,
    /// Free space reached the floor.
    DiskFloor,
    /// Too many consecutive connection failures.
    ConnectionLost,
    /// SIGTERM or SIGINT arrived and the capture wound down on purpose.
    Signalled,
}

/// Set by the signal handler, read by the capture loops.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// True once a termination signal has been seen.
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::Relaxed)
}

extern "C" fn on_signal(_sig: i32) {
    // The only thing a signal handler may safely do here. Everything
    // that actually matters -- flushing, sealing, fsync -- happens on
    // the capture thread once it observes this.
    SHUTDOWN.store(true, Ordering::Relaxed);
}

/// Arrange for SIGTERM and SIGINT to stop the capture cleanly.
///
/// Without this, the default disposition kills the process where it
/// stands. Three things are lost every time, all of them silently:
///
/// * the `BufWriter`'s contents, up to the buffer size or the flush
///   interval -- real records, gone;
/// * the manifest, so the archive holds a file of unknown completeness
///   and no way to tell a quiet market from a truncated capture;
/// * the final `sync_all`, leaving the tail in the page cache where a
///   host failure takes it.
///
/// Restarts are routine -- deploys, config changes, a watchdog
/// replacing a dead stream -- so "only on shutdown" is not rare.
///
/// # Panics
///
/// Panics if the handler cannot be installed, which would mean the
/// process cannot shut down cleanly and should not pretend otherwise.
// The workspace warns on `unsafe_code` so that every use has to argue
// for itself. This one: std exposes no signal API, and the alternative
// to installing a handler is losing the buffer, the manifest and the
// fsync on every restart. The unsafety is confined to one libc call
// whose handler does a single relaxed atomic store.
#[allow(unsafe_code)]
pub fn install_signal_handlers() {
    #[cfg(unix)]
    // SAFETY: `on_signal` is async-signal-safe -- it performs a single
    // relaxed atomic store and calls nothing.
    unsafe {
        for sig in [libc::SIGTERM, libc::SIGINT] {
            assert!(
                libc::signal(sig, on_signal as *const () as libc::sighandler_t) != libc::SIG_ERR,
                "cannot install handler for signal {sig}"
            );
        }
    }
}

/// What a session did.
#[derive(Debug, Clone)]
pub struct SessionStats {
    /// Payload records written.
    pub payloads: u64,
    /// Bytes of payload received, before framing overhead.
    pub payload_bytes: u64,
    /// Disconnects survived.
    pub gaps: u64,
    /// Total time spent disconnected.
    pub outage: Duration,
    /// Wall time the session ran.
    pub elapsed: Duration,
    /// Why it stopped.
    pub stop: StopReason,
}

impl SessionStats {
    /// Bytes of payload per day at the observed rate, the number that
    /// decides whether a host can hold a capture.
    #[must_use]
    pub fn projected_bytes_per_day(&self) -> u64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            return 0;
        }
        #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
        {
            ((self.payload_bytes as f64 / secs) * 86_400.0) as u64
        }
    }
}

/// Nanoseconds since the Unix epoch, from the host clock.
///
/// `local_ts` exists to record when *this host* saw the message, which
/// is what latency modelling needs and what no other clock can supply.
#[must_use]
pub fn now_ns() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since.as_nanos()).unwrap_or(i64::MAX)
}

/// The source of `local_ts`.
///
/// Injected rather than read directly, for the same reason the event
/// kernel forbids clock reads: a test that reads the wall clock is not
/// reproducible from `(seed, commit)`. This one was not hypothetical —
/// the first version of these tests passed on the day they were written
/// and failed the next morning, because fixtures dated one day met a
/// session record stamped with the next.
pub trait Clock {
    /// Nanoseconds since the Unix epoch.
    fn now_ns(&self) -> i64;
}

/// The host clock. What production uses.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        now_ns()
    }
}

/// A source of messages, so the loop can be tested without a network.
pub trait MessageSource {
    /// Block until the next message, or report that the connection
    /// ended.
    ///
    /// # Errors
    ///
    /// Any transport failure. The loop treats every error as a
    /// disconnect: it records a gap and reconnects.
    fn next_message(&mut self) -> io::Result<Vec<u8>>;
}

/// Something that can open a [`MessageSource`].
pub trait Connector {
    /// The source this connector produces.
    type Source: MessageSource;

    /// Open a connection.
    ///
    /// # Errors
    ///
    /// Any failure to connect.
    fn connect(&mut self) -> io::Result<Self::Source>;
}

/// Run a capture session until it stops.
///
/// # Errors
///
/// Propagates write failures. A write failure is fatal on purpose:
/// continuing would mean receiving messages that go nowhere, which
/// looks like capture and is not.
pub fn run<C: Connector>(
    config: &SessionConfig,
    connector: &mut C,
    writer: &mut CaptureWriter,
) -> io::Result<SessionStats> {
    run_with_clock(config, connector, writer, &SystemClock)
}

/// Run a capture session against a supplied clock.
///
/// # Errors
///
/// As [`run`].
pub fn run_with_clock<C: Connector, K: Clock>(
    config: &SessionConfig,
    connector: &mut C,
    writer: &mut CaptureWriter,
    clock: &K,
) -> io::Result<SessionStats> {
    let started = Instant::now();
    let mut stats = SessionStats {
        payloads: 0,
        payload_bytes: 0,
        gaps: 0,
        outage: Duration::ZERO,
        elapsed: Duration::ZERO,
        stop: StopReason::DurationElapsed,
    };
    let mut consecutive_failures = 0u32;
    let mut since_disk_check = 0u64;
    let mut last_flush = Instant::now();

    writer.append_session_start(clock.now_ns())?;

    'outer: loop {
        if shutdown_requested() {
            stats.stop = StopReason::Signalled;
            break;
        }
        if let Some(limit) = config.duration
            && started.elapsed() >= limit
        {
            stats.stop = StopReason::DurationElapsed;
            break;
        }

        let disconnected_at = Instant::now();
        let mut source = match connector.connect() {
            Ok(source) => {
                consecutive_failures = 0;
                source
            }
            Err(_) => {
                consecutive_failures += 1;
                if consecutive_failures >= config.max_consecutive_failures {
                    stats.stop = StopReason::ConnectionLost;
                    break;
                }
                std::thread::sleep(config.reconnect_wait);
                continue;
            }
        };

        loop {
            if let Some(limit) = config.duration
                && started.elapsed() >= limit
            {
                stats.stop = StopReason::DurationElapsed;
                break 'outer;
            }
            // Checked here as well as in the outer loop: a healthy
            // stream never leaves this loop, so an outer-loop-only
            // check would wait for a disconnect that may never come.
            // A silent stream still blocks in next_message() until its
            // read timeout, but a stream with nothing to say also has
            // nothing buffered to lose.
            if shutdown_requested() {
                stats.stop = StopReason::Signalled;
                break 'outer;
            }

            match source.next_message() {
                Ok(payload) => {
                    let local_ts = clock.now_ns();
                    let exch_ts =
                        binance_event_time_ns(&payload).unwrap_or(crate::frame::NO_EXCH_TS);
                    stats.payload_bytes += payload.len() as u64;
                    writer.append(&Record {
                        kind: crate::frame::Kind::Payload,
                        local_ts,
                        exch_ts,
                        payload,
                    })?;
                    stats.payloads += 1;

                    if last_flush.elapsed() >= config.flush_interval {
                        writer.flush()?;
                        last_flush = Instant::now();
                    }

                    since_disk_check += 1;
                    if since_disk_check >= config.disk_check_every {
                        since_disk_check = 0;
                        writer.flush()?;
                        if !crate::disk::above_floor(&config.root, config.disk_floor_bytes)? {
                            stats.stop = StopReason::DiskFloor;
                            break 'outer;
                        }
                    }
                }
                Err(_) => {
                    let outage = disconnected_at.elapsed();
                    stats.gaps += 1;
                    stats.outage += outage;
                    writer.append_gap(
                        clock.now_ns(),
                        "connection lost",
                        None,
                        i64::try_from(outage.as_nanos()).unwrap_or(i64::MAX),
                    )?;
                    std::thread::sleep(config.reconnect_wait);
                    break;
                }
            }
        }
    }

    writer.flush()?;
    stats.elapsed = started.elapsed();
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::decode_all;

    /// A clock the test sets, so a fixture states the day it means
    /// instead of inheriting whatever day the suite happens to run on.
    struct FixedClock(std::cell::Cell<i64>);

    impl FixedClock {
        fn at(ns: i64) -> Self {
            Self(std::cell::Cell::new(ns))
        }
    }

    impl Clock for FixedClock {
        fn now_ns(&self) -> i64 {
            // Advance a microsecond per read so ordering is still
            // strictly increasing, as a real clock would be.
            let now = self.0.get();
            self.0.set(now + 1_000);
            now
        }
    }

    /// The instant every fixture in this module is anchored to.
    const FIXTURE_NS: i64 = 1_786_780_800_000_000_000;

    /// A source that yields a scripted set of messages, then fails.
    struct Scripted {
        messages: Vec<Vec<u8>>,
        index: usize,
    }

    impl MessageSource for Scripted {
        fn next_message(&mut self) -> io::Result<Vec<u8>> {
            let message = self
                .messages
                .get(self.index)
                .ok_or_else(|| io::Error::other("connection closed"))?;
            self.index += 1;
            Ok(message.clone())
        }
    }

    /// Hands out one scripted connection per attempt.
    struct ScriptedConnector {
        connections: Vec<Vec<Vec<u8>>>,
        attempt: usize,
    }

    impl Connector for ScriptedConnector {
        type Source = Scripted;

        fn connect(&mut self) -> io::Result<Self::Source> {
            let messages = self
                .connections
                .get(self.attempt)
                .ok_or_else(|| io::Error::other("no more connections"))?
                .clone();
            self.attempt += 1;
            Ok(Scripted { messages, index: 0 })
        }
    }

    fn depth(event_ms: i64, seq: u64) -> Vec<u8> {
        format!("{{\"e\":\"depthUpdate\",\"E\":{event_ms},\"u\":{seq}}}").into_bytes()
    }

    fn setup(name: &str) -> (PathBuf, StreamId, CaptureWriter) {
        let root = std::env::temp_dir().join(format!("oq-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let stream = StreamId::new("venue", "SYM", "depth");
        let writer = CaptureWriter::new(&root, stream.clone(), Software::new("test", "commit"))
            .expect("writer");
        (root, stream, writer)
    }

    #[test]
    fn writes_every_received_message_and_marks_the_disconnect() {
        let (root, stream, mut writer) = setup("basic");
        let mut connector = ScriptedConnector {
            connections: vec![
                vec![depth(1_786_780_800_000, 1), depth(1_786_780_800_100, 2)],
                vec![depth(1_786_780_800_200, 3)],
            ],
            attempt: 0,
        };
        let mut config = SessionConfig::new(
            &root,
            stream.clone(),
            Software::new("test", "commit"),
            "unused",
        );
        config.max_consecutive_failures = 1;
        config.reconnect_wait = Duration::from_millis(1);
        config.disk_floor_bytes = 0;

        let clock = FixedClock::at(FIXTURE_NS);
        let stats = run_with_clock(&config, &mut connector, &mut writer, &clock).expect("run");
        writer.seal().expect("seal");

        assert_eq!(stats.payloads, 3, "every scripted message was written");
        assert_eq!(stats.gaps, 2, "each closed connection left a marker");
        assert_eq!(stats.stop, StopReason::ConnectionLost);

        let bytes = std::fs::read(stream.file_for(
            &root,
            crate::day::Window::from_nanos(1_786_780_800_000_000_000, crate::day::Rotation::Daily),
        ))
        .expect("read");
        let (records, remainder) = decode_all(&bytes).expect("decode");
        assert_eq!(remainder, 0);
        let payloads = records
            .iter()
            .filter(|r| r.kind == crate::frame::Kind::Payload)
            .count();
        let gaps = records
            .iter()
            .filter(|r| crate::manifest::is_gap(r))
            .count();
        assert_eq!(payloads, 3);
        assert_eq!(gaps, 2, "the gaps are in the stream, not only in the stats");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn stops_at_the_disk_floor_instead_of_filling_the_host() {
        let (root, _stream, mut writer) = setup("floor");
        let mut connector = ScriptedConnector {
            connections: vec![
                (0..50)
                    .map(|i| depth(1_786_780_800_000 + i, i as u64))
                    .collect(),
            ],
            attempt: 0,
        };
        let mut config = SessionConfig::new(
            &root,
            StreamId::new("venue", "SYM", "depth"),
            Software::new("test", "commit"),
            "unused",
        );
        // A floor no filesystem can satisfy: the guard must trip.
        config.disk_floor_bytes = u64::MAX;
        config.disk_check_every = 5;
        config.max_consecutive_failures = 1;
        config.reconnect_wait = Duration::from_millis(1);

        let clock = FixedClock::at(FIXTURE_NS);
        let stats = run_with_clock(&config, &mut connector, &mut writer, &clock).expect("run");
        assert_eq!(stats.stop, StopReason::DiskFloor);
        assert_eq!(
            stats.payloads, 5,
            "stopped at the first check, not at the end"
        );
        std::fs::remove_dir_all(root).ok();
    }

    /// A source that reports the size of the capture file as it goes, so
    /// a test can see whether records reached the disk *during* the run
    /// rather than only when it ended.
    struct Watching {
        messages: Vec<Vec<u8>>,
        index: usize,
        path: PathBuf,
        size_seen_midway: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl MessageSource for Watching {
        fn next_message(&mut self) -> io::Result<Vec<u8>> {
            if self.index == self.messages.len() {
                let size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
                self.size_seen_midway.set(size);
                return Err(io::Error::other("scripted end"));
            }
            let message = self.messages[self.index].clone();
            self.index += 1;
            Ok(message)
        }
    }

    struct WatchingConnector {
        messages: Vec<Vec<u8>>,
        path: PathBuf,
        size_seen_midway: std::rc::Rc<std::cell::Cell<u64>>,
        attempts: usize,
    }

    impl Connector for WatchingConnector {
        type Source = Watching;

        fn connect(&mut self) -> io::Result<Self::Source> {
            if self.attempts > 0 {
                return Err(io::Error::other("one connection only"));
            }
            self.attempts += 1;
            Ok(Watching {
                messages: self.messages.clone(),
                index: 0,
                path: self.path.clone(),
                size_seen_midway: self.size_seen_midway.clone(),
            })
        }
    }

    #[test]
    fn a_low_rate_stream_reaches_disk_without_waiting_for_the_record_threshold() {
        // The failure this guards against: a stream producing one
        // message a second holds sixteen minutes of data in a buffer
        // when flushing is driven by a record count alone. A crash then
        // loses messages that were received, so nothing marks them
        // missing, and an empty file looks the same as a dead feed.
        let (root, stream, mut writer) = setup("timedflush");
        let path = stream.file_for(
            &root,
            crate::day::Window::from_nanos(1_786_780_800_000_000_000, crate::day::Rotation::Daily),
        );
        let seen = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let mut connector = WatchingConnector {
            messages: (0..3)
                .map(|i| depth(1_786_780_800_000 + i, i as u64))
                .collect(),
            path,
            size_seen_midway: seen.clone(),
            attempts: 0,
        };

        let mut config =
            SessionConfig::new(&root, stream, Software::new("test", "commit"), "unused");
        config.flush_interval = Duration::ZERO;
        // Deliberately far above the message count: only the time-based
        // flush can put anything on disk here.
        config.disk_check_every = 1_000_000;
        config.max_consecutive_failures = 1;
        config.reconnect_wait = Duration::from_millis(1);

        run_with_clock(
            &config,
            &mut connector,
            &mut writer,
            &FixedClock::at(FIXTURE_NS),
        )
        .expect("run");
        assert!(
            seen.get() > 0,
            "records must reach the disk during the run, not only at the end"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn projects_a_daily_volume_from_what_it_measured() {
        let stats = SessionStats {
            payloads: 100,
            payload_bytes: 3_600,
            gaps: 0,
            outage: Duration::ZERO,
            elapsed: Duration::from_secs(3_600),
            stop: StopReason::DurationElapsed,
        };
        // 3600 bytes in an hour is 86_400 bytes a day.
        assert_eq!(stats.projected_bytes_per_day(), 86_400);
    }

    #[test]
    fn a_payload_without_an_event_time_still_lands_in_a_file() {
        let (root, stream, mut writer) = setup("noevent");
        let mut connector = ScriptedConnector {
            connections: vec![vec![br#"{"result":null,"id":1}"#.to_vec()]],
            attempt: 0,
        };
        let mut config = SessionConfig::new(
            &root,
            stream.clone(),
            Software::new("test", "commit"),
            "unused",
        );
        config.max_consecutive_failures = 1;
        config.reconnect_wait = Duration::from_millis(1);
        config.disk_floor_bytes = 0;

        let clock = FixedClock::at(FIXTURE_NS);
        let stats = run_with_clock(&config, &mut connector, &mut writer, &clock).expect("run");
        assert_eq!(stats.payloads, 1);
        let sealed = writer.seal().expect("seal");
        // Day attribution fell back to local time rather than dropping
        // the record.
        assert_eq!(
            sealed.manifest.records, 3,
            "session_start, the payload, and one gap — nothing dropped"
        );
        std::fs::remove_dir_all(root).ok();
    }
}
