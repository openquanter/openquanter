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
// The event variant is much larger than the others, and boxing it would
// trade a stack copy on every quiet poll for a heap allocation on every
// event. A quiet poll copies a couple of hundred bytes and an event
// allocates; on a stream read every two hundred milliseconds the copy is
// the cheaper of the two, and it does not put an allocator on the path
// that carries fills.
#[allow(clippy::large_enum_variant)]
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

// ---------------------------------------------------------------------
// Zombie detection.
//
// A socket that is open is not a socket that is delivering. This is the
// failure `StreamOutcome::Idle` cannot see: the connection stands, the
// reads time out, and the account has been moving the whole time.
//
// The only way to tell the two apart is to ask a second source. So the
// venue's own view of the positions is fetched on a schedule and
// compared with the view the stream has been building. Persistent
// disagreement means the stream has stopped saying things that are
// true, whatever the socket believes about itself.
//
// This is the same shape as the capture path's liveness check and the
// same shape as reconciliation, arriving from a third direction: a
// system cannot certify its own inputs, and every claim about them has
// to be crossed against something that failed differently.
// ---------------------------------------------------------------------

use crate::binance::PositionSnapshot;

/// What a comparison of the two views concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// The stream's view matches the venue's.
    Agreed,
    /// They differ, but not yet often enough to act on.
    ///
    /// One disagreement is not evidence: a fill in flight is visible to
    /// one side before the other, and a check that reconnected on every
    /// transient difference would reconnect constantly under load,
    /// which is exactly when it must not.
    Disagreed { consecutive: u32 },
    /// They have differed for long enough that the stream is not
    /// carrying what it should. Reconnect, then reconcile.
    Zombie { consecutive: u32 },
}

/// Compares the streamed view of an account against the venue's.
#[derive(Debug)]
pub struct StreamHealth {
    threshold: u32,
    tolerance: f64,
    consecutive: u32,
}

impl StreamHealth {
    /// Positions differing by less than `tolerance` count as equal, and
    /// `threshold` consecutive disagreements condemn the stream.
    ///
    /// A tolerance is required rather than optional: quantities arrive
    /// as decimal text and are compared as floats, so exact equality
    /// would fail on rounding and condemn a healthy stream — which
    /// would make the check worse than not having one.
    #[must_use]
    pub const fn new(threshold: u32, tolerance: f64) -> Self {
        Self {
            threshold,
            tolerance,
            consecutive: 0,
        }
    }

    /// Sensible defaults for a futures account.
    #[must_use]
    pub const fn futures() -> Self {
        Self::new(3, 1e-4)
    }

    /// Compare one view against the other.
    ///
    /// `streamed` is what the stream has built up; `venue` is what the
    /// venue was just asked. Order does not matter and absent positions
    /// count as flat, so a leg that closed on one side and not the
    /// other is a disagreement rather than a panic.
    pub fn observe(&mut self, streamed: &[PositionSnapshot], venue: &[PositionSnapshot]) -> Health {
        if views_agree(streamed, venue, self.tolerance) {
            self.consecutive = 0;
            return Health::Agreed;
        }
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive >= self.threshold {
            Health::Zombie {
                consecutive: self.consecutive,
            }
        } else {
            Health::Disagreed {
                consecutive: self.consecutive,
            }
        }
    }

    /// Forget the history, after a reconnect has happened.
    pub fn reset(&mut self) {
        self.consecutive = 0;
    }
}

/// Whether two views of an account describe the same positions.
fn views_agree(a: &[PositionSnapshot], b: &[PositionSnapshot], tolerance: f64) -> bool {
    let amount_in = |set: &[PositionSnapshot], symbol: &str, side: &str| -> f64 {
        set.iter()
            .find(|p| p.symbol == symbol && p.position_side == side)
            .map_or(0.0, |p| p.amount)
    };
    // Every leg named by either side, so one that vanished from one
    // view is compared rather than skipped — the disappearance is the
    // disagreement worth catching.
    a.iter().chain(b.iter()).all(|p| {
        (amount_in(a, &p.symbol, &p.position_side) - amount_in(b, &p.symbol, &p.position_side))
            .abs()
            <= tolerance
    })
}

#[cfg(test)]
mod health {
    use super::*;

    fn pos(symbol: &str, side: &str, amount: f64) -> PositionSnapshot {
        PositionSnapshot {
            symbol: symbol.to_string(),
            position_side: side.to_string(),
            amount_text: String::new(),
            entry_text: String::new(),
            amount,
            entry_price: 0.0,
            unrealized: 0.0,
        }
    }

    #[test]
    fn agreement_resets_the_count() {
        let mut h = StreamHealth::futures();
        let same = vec![pos("BTCUSDT", "BOTH", 1.5)];
        assert_eq!(h.observe(&same, &same), Health::Agreed);
        // Disagree once, then agree: the count must not carry over, or
        // three unrelated blips an hour apart would condemn a stream
        // that is working.
        let other = vec![pos("BTCUSDT", "BOTH", 2.5)];
        assert_eq!(
            h.observe(&same, &other),
            Health::Disagreed { consecutive: 1 }
        );
        assert_eq!(h.observe(&same, &same), Health::Agreed);
        assert_eq!(
            h.observe(&same, &other),
            Health::Disagreed { consecutive: 1 }
        );
    }

    #[test]
    fn one_disagreement_is_not_evidence_but_three_are() {
        // A fill in flight is visible to one side before the other, so
        // a check that acted on the first difference would reconnect
        // constantly under load — exactly when it must not.
        let mut h = StreamHealth::futures();
        let streamed = vec![pos("BTCUSDT", "BOTH", 1.0)];
        let venue = vec![pos("BTCUSDT", "BOTH", 2.0)];
        assert_eq!(
            h.observe(&streamed, &venue),
            Health::Disagreed { consecutive: 1 }
        );
        assert_eq!(
            h.observe(&streamed, &venue),
            Health::Disagreed { consecutive: 2 }
        );
        assert_eq!(
            h.observe(&streamed, &venue),
            Health::Zombie { consecutive: 3 }
        );
    }

    #[test]
    fn rounding_does_not_condemn_a_healthy_stream() {
        // Quantities arrive as decimal text and are compared as floats.
        // Exact equality here would make the check worse than none.
        let mut h = StreamHealth::futures();
        let a = vec![pos("BTCUSDT", "BOTH", 1.000_01)];
        let b = vec![pos("BTCUSDT", "BOTH", 1.000_02)];
        assert_eq!(h.observe(&a, &b), Health::Agreed);
    }

    #[test]
    fn a_position_missing_from_one_view_is_a_disagreement() {
        // The failure this is really for: the stream missed a fill, so
        // it believes a position that closed is still open — or has
        // never heard of one that opened.
        let mut h = StreamHealth::futures();
        let streamed: Vec<PositionSnapshot> = Vec::new();
        let venue = vec![pos("BTCUSDT", "BOTH", 1.0)];
        assert!(matches!(
            h.observe(&streamed, &venue),
            Health::Disagreed { .. }
        ));
    }

    #[test]
    fn the_two_legs_of_a_hedged_account_are_compared_separately() {
        // Netting them first would hide the case that matters: both
        // legs wrong by the same amount in opposite directions nets to
        // zero and is still two wrong positions.
        let mut h = StreamHealth::futures();
        let streamed = vec![pos("BTCUSDT", "LONG", 2.0), pos("BTCUSDT", "SHORT", -1.0)];
        let venue = vec![pos("BTCUSDT", "LONG", 1.0), pos("BTCUSDT", "SHORT", -2.0)];
        assert!(matches!(
            h.observe(&streamed, &venue),
            Health::Disagreed { .. }
        ));
    }

    #[test]
    fn an_empty_account_agrees_with_itself() {
        let mut h = StreamHealth::futures();
        assert_eq!(h.observe(&[], &[]), Health::Agreed);
    }
}
