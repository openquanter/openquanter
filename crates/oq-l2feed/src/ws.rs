//! WebSocket transport.
//!
//! A thin adapter from `tungstenite` to [`MessageSource`]. Everything
//! interesting about capture lives in [`crate::session`]; this file
//! exists so that logic can be tested without a network, and so the
//! choice of client library stays replaceable.
//!
//! Synchronous on purpose. A capture process follows a handful of
//! streams and spends its life blocked on a socket; an async runtime
//! would add a scheduler between the wire and the disk without removing
//! any waiting. The project's rule is that async belongs at the gateway
//! edge, and capture is not that edge.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use tungstenite::{Message, connect};

use crate::session::{Connector, MessageSource};
use crate::venue::{AckPolicy, Transport};

/// A connected WebSocket stream.
pub struct WsSource {
    socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    /// Messages read while confirming the subscription.
    ///
    /// Confirmation consumes messages, and on a venue where the first
    /// message *is* the acknowledgement that message is also data.
    /// Dropping it would make the capture lose a record for every
    /// connection and every reconnection, which is precisely the kind of
    /// small, regular loss that never shows up in a total.
    pending: VecDeque<Vec<u8>>,
    /// How long silence may run before this end proves it is alive.
    /// `None` on a venue that drives its own keepalive.
    keepalive: Option<Duration>,
    /// Consecutive silent reads. Reset by any frame at all, a pong
    /// included, so this counts genuine silence rather than quiet market
    /// data.
    silent_rounds: u32,
}

/// How many silent rounds may pass before the connection is called dead.
///
/// A keepalive that is answered resets the count, so reaching this means
/// nothing came back from several pings — a socket that is open and no
/// longer carrying anything, which is the failure a bounded wait exists
/// to turn into a reconnect.
const SILENT_ROUNDS_ALLOWED: u32 = 3;

impl MessageSource for WsSource {
    /// A connection with a keepalive proves itself alive on every tick,
    /// so its silence says nothing about whether it is still there.
    fn silence_is_a_disconnect(&self) -> bool {
        self.keepalive.is_none()
    }

    fn next_message(&mut self) -> io::Result<Vec<u8>> {
        if let Some(buffered) = self.pending.pop_front() {
            return Ok(buffered);
        }
        loop {
            let message = match self.socket.read() {
                Ok(m) => m,
                Err(e) if self.keepalive.is_some() && is_read_timeout(&e) => {
                    self.silent_rounds += 1;
                    if self.silent_rounds > SILENT_ROUNDS_ALLOWED {
                        return Err(io::Error::other(
                            "no data and no answer to a keepalive; treating the connection as dead",
                        ));
                    }
                    // Silence on a quiet channel is ordinary: ping, then
                    // hand the round back to the caller rather than
                    // waiting again here. Staying inside this loop kept
                    // the connection alive and the caller asleep -- a
                    // capture process that could not see its own
                    // shutdown flag, measured in production as three
                    // minutes of ignoring SIGTERM.
                    //
                    // `WouldBlock` and not an error kind of its own: the
                    // callers that poll already read it as "no data this
                    // instant", and the capture loop asks
                    // `silence_is_a_disconnect` before deciding what to
                    // do about it.
                    self.socket
                        .send(Message::Ping(Vec::new().into()))
                        .map_err(io::Error::other)?;
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "keepalive sent, no data yet",
                    ));
                }
                // A read timeout keeps its kind. Wrapping it in
                // `io::Error::other` erases `WouldBlock`, and a caller
                // that polls with a short timeout then cannot tell "no
                // data this instant" from "the connection is gone" — so
                // it drops a healthy socket and opens another, once per
                // quiet interval. Measured on a real feed: fourteen
                // reconnections in two minutes on a contract that was
                // simply not trading.
                //
                // The capture loop is unaffected: it counts any error as
                // a disconnect without inspecting the kind, which is the
                // right reading for the long timeout it uses.
                Err(e) if is_read_timeout(&e) => {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, e.to_string()));
                }
                Err(e) => return Err(io::Error::other(e)),
            };
            self.silent_rounds = 0;
            match message {
                Message::Text(text) => return Ok(text.as_bytes().to_vec()),
                Message::Binary(bytes) => return Ok(bytes.to_vec()),
                // Answer keepalives in place: a venue that stops hearing
                // from us disconnects, and a disconnect costs a gap.
                Message::Ping(payload) => {
                    self.socket
                        .send(Message::Pong(payload))
                        .map_err(io::Error::other)?;
                }
                Message::Pong(_) | Message::Frame(_) => {}
                Message::Close(_) => {
                    return Err(io::Error::other("venue closed the connection"));
                }
            }
        }
    }
}

/// Opens WebSocket connections described by a [`Transport`].
///
/// The transport carries the venue's differences as data: where to
/// connect, what to send once connected, and how the subscription is
/// confirmed. Binance puts the subscription in the URL and sends
/// nothing; OKX and Coinbase connect to one endpoint and then send JSON.
/// Keeping that as data rather than as branches is what stops this file
/// from growing a section per venue.
pub struct WsConnector {
    transport: Transport,
    read_timeout: Duration,
}

impl WsConnector {
    /// A connector for `transport`.
    ///
    /// `read_timeout` bounds how long a silent connection is tolerated
    /// once it is running. Without it a half-open socket looks exactly
    /// like a quiet market, and capture would sit there recording
    /// nothing while believing it was connected — the failure a gap
    /// marker exists to make visible.
    #[must_use]
    pub fn new(transport: Transport, read_timeout: Duration) -> Self {
        Self {
            transport,
            read_timeout,
        }
    }

    /// A connector for a bare URL, with no handshake and no confirmation.
    #[must_use]
    pub fn from_url(url: impl Into<String>, read_timeout: Duration) -> Self {
        Self::new(
            Transport {
                url: url.into(),
                subscribe: Vec::new(),
                ack: AckPolicy::None,
                keepalive: None,
            },
            read_timeout,
        )
    }
}

impl Connector for WsConnector {
    type Source = WsSource;

    fn connect(&mut self) -> io::Result<Self::Source> {
        let (mut socket, _response) = connect(&self.transport.url).map_err(io::Error::other)?;

        for frame in &self.transport.subscribe {
            let text = String::from_utf8(frame.clone())
                .map_err(|_| io::Error::other("subscribe frame is not valid UTF-8"))?;
            socket
                .send(Message::Text(text.into()))
                .map_err(io::Error::other)?;
        }

        let keepalive = self.transport.keepalive;
        let mut source = WsSource {
            socket,
            pending: VecDeque::new(),
            keepalive,
            silent_rounds: 0,
        };
        source.confirm(&self.transport.ack)?;
        // The read has to wake up often enough to send the keepalive,
        // so a venue that wants one shortens the wait rather than
        // lengthening its own tolerance.
        source
            .set_read_timeout(keepalive.map_or(self.read_timeout, |k| k.min(self.read_timeout)))?;
        Ok(source)
    }
}

impl WsSource {
    fn set_read_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        match self.socket.get_ref() {
            tungstenite::stream::MaybeTlsStream::Plain(stream) => {
                stream.set_read_timeout(Some(timeout))
            }
            tungstenite::stream::MaybeTlsStream::Rustls(stream) => {
                stream.get_ref().set_read_timeout(Some(timeout))
            }
            _ => Ok(()),
        }
    }

    /// Wait for the venue to confirm the subscription.
    ///
    /// The failure this exists for is not a socket that breaks — that is
    /// already visible — but one that stays open and delivers nothing.
    /// Binance accepts any stream name without validating it, so a
    /// misspelled or retired stream subscribes successfully and is
    /// silent forever, indistinguishable from a market with nothing to
    /// say until somebody notices the file never grew. Turning that into
    /// a bounded wait makes it a connection error, which the session
    /// loop already knows how to record.
    fn confirm(&mut self, policy: &AckPolicy) -> io::Result<()> {
        let deadline = match policy {
            AckPolicy::None => return Ok(()),
            AckPolicy::FirstDataIsAck { deadline } | AckPolicy::Explicit { deadline, .. } => {
                *deadline
            }
        };
        self.set_read_timeout(deadline)?;
        let started = Instant::now();

        loop {
            if started.elapsed() >= deadline {
                return Err(io::Error::other(
                    "subscription was accepted but delivered nothing before the deadline",
                ));
            }
            let payload = match self.socket.read() {
                Ok(Message::Text(t)) => t.as_bytes().to_vec(),
                Ok(Message::Binary(b)) => b.to_vec(),
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => continue,
                Ok(Message::Close(_)) => {
                    return Err(io::Error::other("venue closed the connection"));
                }
                Err(e) => return Err(io::Error::other(e)),
            };

            match classify(policy, &payload) {
                Step::ConfirmedKeeping => {
                    // The acknowledgement is also data, so it is queued
                    // rather than consumed.
                    self.pending.push_back(payload);
                    return Ok(());
                }
                Step::ConfirmedDiscarding => return Ok(()),
                Step::Rejected => {
                    // Reported now, with the venue's own words, rather
                    // than waited out. A refusal will not become an
                    // acceptance, and it is not data: queueing it would
                    // write the error frame into the archive.
                    return Err(io::Error::other(format!(
                        "venue refused the subscription: {}",
                        String::from_utf8_lossy(&payload).trim()
                    )));
                }
                Step::KeepWaiting => {
                    // Data that arrived before the acknowledgement is
                    // still data.
                    self.pending.push_back(payload);
                }
            }
        }
    }
}

/// What one message means while waiting for confirmation.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Subscription confirmed, and this message is data to keep.
    ConfirmedKeeping,
    /// Subscription confirmed by a message that is only an
    /// acknowledgement.
    ConfirmedDiscarding,
    /// The venue said no. Terminal, and not data.
    Rejected,
    /// Not the acknowledgement; keep the message and keep waiting.
    KeepWaiting,
}

/// The decision separated from the socket, so it can be tested without
/// one. The I/O around it is a read loop and a deadline; the part worth
/// getting right is which messages count as confirmation and which are
/// data that must survive it.
fn classify(policy: &AckPolicy, payload: &[u8]) -> Step {
    match policy {
        AckPolicy::None => Step::ConfirmedKeeping,
        AckPolicy::FirstDataIsAck { .. } => Step::ConfirmedKeeping,
        AckPolicy::Explicit {
            marker,
            reject_marker,
            ..
        } => {
            if contains(payload, marker) {
                Step::ConfirmedDiscarding
            } else if contains(payload, reject_marker) {
                Step::Rejected
            } else {
                Step::KeepWaiting
            }
        }
    }
}

/// Whether a read failed because nothing arrived in time, as opposed to
/// because the connection broke. Only the first is silence.
/// Exposed for a test in another file; not part of the public contract.
#[doc(hidden)]
#[must_use]
pub fn is_read_timeout_for_test(e: &tungstenite::Error) -> bool {
    is_read_timeout(e)
}

fn is_read_timeout(e: &tungstenite::Error) -> bool {
    matches!(
        e,
        tungstenite::Error::Io(io)
            if io.kind() == io::ErrorKind::WouldBlock || io.kind() == io::ErrorKind::TimedOut
    )
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

/// A REST endpoint polled on an interval, presented as a message source.
///
/// Some data a margin model needs has no working stream — mark price and
/// funding on this venue arrive only over REST. Rather than build a
/// second capture path for it, the poller wears the same interface as a
/// socket, so framing, day rotation, gap markers and manifests all apply
/// unchanged. A failed poll is a disconnect, which is exactly right: the
/// archive should record that we were not receiving.
pub struct PollSource {
    url: String,
    interval: Duration,
    next_due: std::time::Instant,
}

impl MessageSource for PollSource {
    fn next_message(&mut self) -> io::Result<Vec<u8>> {
        let now = std::time::Instant::now();
        if self.next_due > now {
            std::thread::sleep(self.next_due - now);
        }
        self.next_due = std::time::Instant::now() + self.interval;

        let mut response = ureq::get(&self.url).call().map_err(io::Error::other)?;
        let body = response
            .body_mut()
            .read_to_vec()
            .map_err(io::Error::other)?;
        if body.is_empty() {
            return Err(io::Error::other("empty response"));
        }
        Ok(body)
    }
}

/// Opens pollers against a fixed URL.
pub struct PollConnector {
    url: String,
    interval: Duration,
}

impl PollConnector {
    /// A connector polling `url` every `interval`.
    #[must_use]
    pub fn new(url: impl Into<String>, interval: Duration) -> Self {
        Self {
            url: url.into(),
            interval,
        }
    }
}

impl Connector for PollConnector {
    type Source = PollSource;

    fn connect(&mut self) -> io::Result<Self::Source> {
        Ok(PollSource {
            url: self.url.clone(),
            interval: self.interval,
            // Poll immediately on connect, then on the interval.
            next_due: std::time::Instant::now(),
        })
    }
}

/// Fetch an order book snapshot over REST.
///
/// Called after every reconnect: the incremental stream only makes sense
/// against a known starting book, and a gap without a following snapshot
/// leaves the archive unable to reconstruct one.
///
/// # Errors
///
/// Any transport or HTTP failure.
pub fn fetch_snapshot(url: &str) -> io::Result<Vec<u8>> {
    let mut response = ureq::get(url).call().map_err(io::Error::other)?;
    response.body_mut().read_to_vec().map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEADLINE: Duration = Duration::from_secs(30);

    #[test]
    fn the_first_message_confirms_and_is_still_data() {
        // On a venue where the first message is the acknowledgement,
        // that message is also a market event. Consuming it would lose
        // one record per connection — and per reconnection, which is
        // where it would add up without ever looking like a fault.
        assert_eq!(
            classify(
                &AckPolicy::FirstDataIsAck { deadline: DEADLINE },
                br#"{"e":"depthUpdate"}"#
            ),
            Step::ConfirmedKeeping
        );
    }

    #[test]
    fn an_explicit_ack_is_consumed_but_data_before_it_is_not() {
        let policy = AckPolicy::Explicit {
            marker: b"\"event\":\"subscribe\"".to_vec(),
            reject_marker: b"\"event\":\"error\"".to_vec(),
            deadline: DEADLINE,
        };
        assert_eq!(
            classify(&policy, br#"{"event":"subscribe","arg":{}}"#),
            Step::ConfirmedDiscarding,
            "the acknowledgement itself is not market data"
        );
        assert_eq!(
            classify(&policy, br#"{"arg":{},"data":[{"px":"1"}]}"#),
            Step::KeepWaiting,
            "data arriving before the ack must survive the wait"
        );
    }

    /// A venue that answers explicitly answers both ways. Treating the
    /// refusal as data waits out the whole deadline and then reports
    /// that the subscription "was accepted but delivered nothing" — the
    /// opposite of what happened — and files the error frame in the
    /// archive on the way.
    #[test]
    fn an_explicit_refusal_is_neither_data_nor_something_to_wait_out() {
        let policy = AckPolicy::Explicit {
            marker: b"\"event\":\"subscribe\"".to_vec(),
            reject_marker: b"\"event\":\"error\"".to_vec(),
            deadline: DEADLINE,
        };
        let refusal = br#"{"event":"error","msg":"Wrong URL or channel:books,instId:BTC-USDTT-SWAP doesn't exist.","code":"60018"}"#;
        assert_eq!(classify(&policy, refusal), Step::Rejected);
    }

    /// A venue with no distinct refusal to look for keeps the old
    /// behaviour: an empty marker must not match every message and turn
    /// the first book update into a refusal.
    #[test]
    fn an_empty_reject_marker_matches_nothing() {
        let policy = AckPolicy::Explicit {
            marker: b"\"event\":\"subscribe\"".to_vec(),
            reject_marker: Vec::new(),
            deadline: DEADLINE,
        };
        assert_eq!(
            classify(&policy, br#"{"arg":{},"data":[{"px":"1"}]}"#),
            Step::KeepWaiting
        );
    }

    #[test]
    fn no_policy_confirms_immediately_and_keeps_the_message() {
        assert_eq!(
            classify(&AckPolicy::None, b"anything"),
            Step::ConfirmedKeeping
        );
    }

    #[test]
    fn contains_does_not_match_an_empty_needle() {
        // An empty marker would otherwise confirm on the first byte of
        // anything, turning a configuration mistake into a subscription
        // that always looks healthy.
        assert!(!contains(b"payload", b""));
        assert!(contains(b"payload", b"loa"));
        assert!(!contains(b"pay", b"payload"));
    }
}
