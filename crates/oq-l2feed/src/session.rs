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
/// The only wall-clock read in the capture path, and it is deliberate:
/// `local_ts` exists to record when *this host* saw the message, which
/// is what latency modelling needs and what no other clock can supply.
#[must_use]
pub fn now_ns() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since.as_nanos()).unwrap_or(i64::MAX)
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

    writer.append_session_start(now_ns())?;

    'outer: loop {
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

            match source.next_message() {
                Ok(payload) => {
                    let local_ts = now_ns();
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
                        now_ns(),
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

        let stats = run(&config, &mut connector, &mut writer).expect("run");
        writer.seal().expect("seal");

        assert_eq!(stats.payloads, 3, "every scripted message was written");
        assert_eq!(stats.gaps, 2, "each closed connection left a marker");
        assert_eq!(stats.stop, StopReason::ConnectionLost);

        let bytes = std::fs::read(
            stream.file_for(&root, crate::UtcDay::from_nanos(1_786_780_800_000_000_000)),
        )
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

        let stats = run(&config, &mut connector, &mut writer).expect("run");
        assert_eq!(stats.stop, StopReason::DiskFloor);
        assert_eq!(
            stats.payloads, 5,
            "stopped at the first check, not at the end"
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

        let stats = run(&config, &mut connector, &mut writer).expect("run");
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
