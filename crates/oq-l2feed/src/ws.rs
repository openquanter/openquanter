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

use std::io;
use std::time::Duration;

use tungstenite::{Message, connect};

use crate::session::{Connector, MessageSource};

/// A connected WebSocket stream.
pub struct WsSource {
    socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
}

impl MessageSource for WsSource {
    fn next_message(&mut self) -> io::Result<Vec<u8>> {
        loop {
            let message = self.socket.read().map_err(io::Error::other)?;
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

/// Opens WebSocket connections to a fixed URL.
pub struct WsConnector {
    url: String,
    read_timeout: Duration,
}

impl WsConnector {
    /// A connector for `url`.
    ///
    /// `read_timeout` bounds how long a silent connection is tolerated.
    /// Without it a half-open socket looks exactly like a quiet market,
    /// and capture would sit there recording nothing while believing it
    /// was connected — the failure a gap marker exists to make visible.
    #[must_use]
    pub fn new(url: impl Into<String>, read_timeout: Duration) -> Self {
        Self {
            url: url.into(),
            read_timeout,
        }
    }
}

impl Connector for WsConnector {
    type Source = WsSource;

    fn connect(&mut self) -> io::Result<Self::Source> {
        let (socket, _response) = connect(&self.url).map_err(io::Error::other)?;

        match socket.get_ref() {
            tungstenite::stream::MaybeTlsStream::Plain(stream) => {
                stream.set_read_timeout(Some(self.read_timeout))?;
            }
            tungstenite::stream::MaybeTlsStream::Rustls(stream) => {
                stream.get_ref().set_read_timeout(Some(self.read_timeout))?;
            }
            _ => {}
        }

        Ok(WsSource { socket })
    }
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
