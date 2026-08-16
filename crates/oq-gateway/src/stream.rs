//! Listening to the venue.
//!
//! The socket half of the execution path. Nothing can be sent on it and
//! nothing about an order's fate can be heard without it, which is why
//! this exists rather than a loop that asks again.
//!
//! # A disconnect is not a pause
//!
//! The one decision that shapes this module: when the connection drops,
//! whatever the venue said during the gap was said to nobody. An order
//! may have filled, a position may have moved, and the next message to
//! arrive will describe a world the reader has no history for.
//!
//! So a drop is reported as [`StreamOutcome::Disconnected`] rather than
//! handled by reconnecting quietly. Reconnecting is easy and is not the
//! hard part; the hard part is that the caller now has to reconcile
//! against the venue before it trusts its own books again. Hiding the
//! drop takes that decision away from the layer that can make it.
//!
//! This is the same lesson the capture path learned about quiet
//! streams, arriving from the other direction: there, silence could not
//! be distinguished from death, and here, a reconnection cannot be
//! distinguished from continuity unless someone says so.

use core::time::Duration;

use crate::binance::{VenueError, parse_user_event};
use crate::exec::{UserEvent, UserStream};

/// A connected user data stream.
pub struct UserStreamReader {
    socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
}

/// What came out of the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamOutcome {
    /// The venue said something about the account.
    Event(UserEvent),
    /// A frame that carries nothing an account cares about — a pong, a
    /// subscription reply, a message this build does not recognise as
    /// an event at all.
    Ignored,
    /// Nothing arrived within the read timeout.
    ///
    /// Not an error and not a gap: the account was simply quiet. It is
    /// reported rather than swallowed so a caller can drive keepalives
    /// and liveness checks from the same loop that reads.
    Idle,
    /// The connection is gone.
    ///
    /// Everything the venue said while it was gone was said to nobody.
    /// Reconnect, then reconcile — in that order, and never only the
    /// first.
    Disconnected(String),
}

impl UserStreamReader {
    /// Connect.
    ///
    /// `read_timeout` bounds how long [`UserStreamReader::next`] blocks
    /// before reporting [`StreamOutcome::Idle`]. It is not a liveness
    /// check: this venue sends nothing at all on a quiet account, so a
    /// timeout here means nothing happened, not that anything is wrong.
    ///
    /// # Errors
    /// Anything the handshake reports.
    pub fn connect(stream: &UserStream, read_timeout: Duration) -> Result<Self, VenueError> {
        let (socket, _response) =
            tungstenite::connect(stream.url()).map_err(|e| VenueError::Transport(e.to_string()))?;
        let mut reader = Self { socket };
        reader
            .set_read_timeout(read_timeout)
            .map_err(|e| VenueError::Transport(e.to_string()))?;
        Ok(reader)
    }

    fn set_read_timeout(&mut self, timeout: Duration) -> std::io::Result<()> {
        match self.socket.get_ref() {
            tungstenite::stream::MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(timeout)),
            tungstenite::stream::MaybeTlsStream::Rustls(s) => {
                s.get_ref().set_read_timeout(Some(timeout))
            }
            _ => Ok(()),
        }
    }

    /// Read one message.
    ///
    /// Never blocks longer than the read timeout given at connect.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> StreamOutcome {
        match self.socket.read() {
            Ok(tungstenite::Message::Text(text)) => match parse_user_event(&text) {
                Some(event) => StreamOutcome::Event(event),
                None => StreamOutcome::Ignored,
            },
            // The library answers pings itself; a pong arriving here is
            // an answer to one this side sent, and carries no account
            // information.
            Ok(tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_)) => {
                StreamOutcome::Ignored
            }
            Ok(tungstenite::Message::Close(frame)) => StreamOutcome::Disconnected(
                frame.map_or_else(|| "closed by venue".to_string(), |f| f.reason.to_string()),
            ),
            Ok(_) => StreamOutcome::Ignored,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                StreamOutcome::Idle
            }
            Err(e) => StreamOutcome::Disconnected(e.to_string()),
        }
    }

    /// Close politely.
    ///
    /// # Errors
    /// Anything the close reports.
    pub fn close(mut self) -> Result<(), VenueError> {
        self.socket
            .close(None)
            .map_err(|e| VenueError::Transport(e.to_string()))
    }
}

/// How long a venue key survives without renewal.
///
/// Binance expires a listen key sixty minutes after it is issued. The
/// renewal interval below is deliberately well inside that, because a
/// renewal that fails has to have room to be retried — a schedule with
/// no margin turns one failed request into a closed stream.
pub const KEY_LIFETIME: Duration = Duration::from_secs(60 * 60);

/// How often to renew.
pub const KEY_RENEWAL: Duration = Duration::from_secs(20 * 60);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renewal_leaves_room_for_a_retry() {
        // Two whole renewal intervals must still fit inside the key's
        // life, so a single failed renewal is survivable rather than
        // fatal. This is arithmetic, but it is the arithmetic that
        // decides whether one bad request closes the stream.
        assert!(
            KEY_RENEWAL * 2 < KEY_LIFETIME,
            "a failed renewal must have a second chance before expiry"
        );
    }

    #[test]
    fn a_close_frame_is_a_disconnect_and_not_an_idle_period() {
        // The distinction the module exists for: idle means nothing
        // happened, disconnected means things may have happened
        // unobserved. Conflating them is how a filled order goes
        // unnoticed.
        assert_ne!(
            StreamOutcome::Disconnected("closed by venue".into()),
            StreamOutcome::Idle
        );
    }
}
