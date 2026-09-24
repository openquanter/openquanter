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
use std::time::Instant;

use crate::binance::VenueError;
use crate::exec::{Handshake, Opening, UserEvent, UserStream};

/// How often this venue speaks on a stream with nothing to report.
///
/// Binance pings a user data stream every three minutes whether or not
/// the account moves. That ping is the only thing that distinguishes a
/// quiet account from a dead link, and every threshold below is a
/// multiple of it rather than a round number that felt safe.
pub const VENUE_PING_PERIOD: Duration = Duration::from_secs(3 * 60);

/// Silence beyond which a user stream is presumed dead.
///
/// Three ping periods, so one lost ping and its retransmission are
/// survivable and only a link that has actually stopped is condemned.
///
/// Deliberately not the thirty seconds market data uses: depth and
/// trades arrive several times a second, while an account can honestly
/// have nothing to say for hours. Different silences, different windows.
pub const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(10 * 60);

/// A connected user data stream.
pub struct UserStreamReader {
    socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    /// When the venue last said anything at all, a ping included.
    last_message: Instant,
    /// Silence beyond which the connection is presumed dead.
    stale_after: Duration,
    /// How this venue's messages are read.
    events: std::sync::Arc<dyn crate::exec::Events>,
    /// Events from a frame that carried more than one.
    ///
    /// One venue sends a `data` array, so a single read can produce
    /// several fills. They are held here and delivered one per `next`
    /// rather than dropped, because the caller's contract is one event
    /// per call and the alternative loses every fill after the first.
    pending: std::collections::VecDeque<UserEvent>,
}

/// How long a stream has been silent, when that is long enough to
/// condemn it. `None` while it is still within its window.
///
/// Pulled out of [`UserStreamReader::next`] so the judgement is
/// testable: `next` needs a socket, and a test cannot half-open one.
fn silence_verdict(last_message: Instant, stale_after: Duration) -> Option<Duration> {
    let silent = last_message.elapsed();
    (silent > stale_after).then_some(silent)
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
    /// before reporting [`StreamOutcome::Idle`]. It is not on its own a
    /// liveness check — one timed-out read means nothing happened — but
    /// [`DEFAULT_STALE_AFTER`] of them in a row is, and `next` says so.
    ///
    /// # Errors
    /// Anything the handshake reports.
    pub fn connect(stream: &UserStream, read_timeout: Duration) -> Result<Self, VenueError> {
        let socket = connect_bounded(stream.url(), HANDSHAKE_TIMEOUT)
            .map_err(|e| VenueError::Transport(e.to_string()))?;
        let mut reader = Self {
            socket,
            // A fresh connection has not been silent. Without this it
            // would inherit the epoch and be condemned on its first read.
            last_message: Instant::now(),
            stale_after: DEFAULT_STALE_AFTER,
            events: stream.events(),
            pending: std::collections::VecDeque::new(),
        };
        reader
            .set_read_timeout(read_timeout)
            .map_err(|e| VenueError::Transport(e.to_string()))?;
        // A venue that authenticates in its URL has nothing to say here
        // and this costs it nothing. One that authenticates on the
        // socket is not connected until this returns: an open socket
        // that never logged in answers every read with a timeout, which
        // is the shape of a quiet account and not of a broken one.
        if !stream.opening().is_empty() {
            perform_opening(stream.opening(), &mut reader, OPENING_READ_BUDGET)?;
            // Whatever answered is also evidence the link is alive.
            reader.last_message = Instant::now();
        }
        Ok(reader)
    }

    /// How long this stream may say nothing before it is presumed dead.
    ///
    /// The same shape market data's `Stream::stale_after` has, because
    /// it is the same decision about a different socket.
    #[must_use]
    pub fn stale_after(mut self, after: Duration) -> Self {
        self.stale_after = after;
        self
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
        // An open socket is not a delivering socket.
        //
        // A half-open connection answers every read with a timeout and
        // answers it forever, so [`StreamOutcome::Idle`] on its own
        // cannot tell a quiet account from a dead link. Measured, on
        // this venue: a stream stopped delivering and the process went
        // on reporting `Idle` for thirty-three hours, during which the
        // account filled two orders and the books learned of neither.
        // The socket stayed ESTABLISHED throughout and nothing was ever
        // written to it, because nothing here had a reason to.
        //
        // `oq-live`'s market data path learned this and grew a staleness
        // check; this module was left with a comment saying the caller
        // would do it, and the caller did not.
        // A frame that carried several events is drained before the
        // socket is read again. Everything queued here already happened
        // on the account, and reading ahead of it would deliver a later
        // fill before an earlier one.
        if let Some(event) = self.pending.pop_front() {
            return StreamOutcome::Event(event);
        }
        if let Some(silent) = silence_verdict(self.last_message, self.stale_after) {
            // Restarted here, so a caller that reconnects into another
            // dead socket gets its next verdict a full window later
            // rather than on the very next read.
            self.last_message = Instant::now();
            return StreamOutcome::Disconnected(format!(
                "silent for {}s; presumed dead",
                silent.as_secs()
            ));
        }
        match self.socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                self.last_message = Instant::now();
                let mut events = self.events.read(&text).into_iter();
                match events.next() {
                    Some(first) => {
                        self.pending.extend(events);
                        StreamOutcome::Event(first)
                    }
                    None => StreamOutcome::Ignored,
                }
            }
            // The library answers pings itself; a pong arriving here is
            // an answer to one this side sent, and carries no account
            // information.
            //
            // No account information, but proof of life: on a quiet
            // account this venue's three-minute ping is the only thing
            // that arrives, which makes it the whole basis of the check
            // above. Counting it as silence would condemn every healthy
            // stream that simply had nothing to report.
            Ok(tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_)) => {
                self.last_message = Instant::now();
                StreamOutcome::Ignored
            }
            Ok(tungstenite::Message::Close(frame)) => StreamOutcome::Disconnected(
                frame.map_or_else(|| "closed by venue".to_string(), |f| f.reason.to_string()),
            ),
            Ok(_) => {
                self.last_message = Instant::now();
                StreamOutcome::Ignored
            }
            // The one outcome that does not refresh the clock. Every
            // other arm above had something arrive; this arm is the
            // absence the window is measuring.
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

/// How many reads an opening frame may go unanswered before the stream
/// is called unusable.
///
/// Counted in reads rather than in seconds because a read here is
/// already bounded by the socket's timeout, and a budget in messages is
/// a budget a test can exhaust without waiting for a clock.
const OPENING_READ_BUDGET: usize = 20;

/// The two things opening a stream needs from a socket.
///
/// A trait so the sequence below can be exercised without one. A login
/// that is only ever run against a live venue is a login that gets
/// debugged in production, and this one cannot be reached from a unit
/// test any other way.
trait Wire {
    fn send(&mut self, text: &str) -> Result<(), VenueError>;
    /// `Ok(None)` when the read timed out and nothing arrived.
    fn read(&mut self) -> Result<Option<String>, VenueError>;
}

impl Wire for UserStreamReader {
    fn send(&mut self, text: &str) -> Result<(), VenueError> {
        self.socket
            .send(tungstenite::Message::Text(text.into()))
            .map_err(|e| VenueError::Transport(e.to_string()))
    }

    fn read(&mut self) -> Result<Option<String>, VenueError> {
        match self.socket.read() {
            Ok(tungstenite::Message::Text(text)) => Ok(Some(text.to_string())),
            // A ping or a pong is the link working, not an answer.
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(VenueError::Transport(e.to_string())),
        }
    }
}

/// Send each opening frame and wait for the answer it is owed.
///
/// Sequential rather than pipelined: a venue that requires a login
/// before a subscription will refuse the subscription that arrived
/// first, and the refusal is easy to read as the login having failed.
fn perform_opening(
    opening: &[Opening],
    wire: &mut impl Wire,
    budget: usize,
) -> Result<(), VenueError> {
    for step in opening {
        wire.send(step.frame())?;
        let Some(answer) = step.answer() else {
            continue;
        };
        let mut reads = 0usize;
        loop {
            if reads >= budget {
                return Err(VenueError::Transport(
                    "the venue never answered the frame that opens this stream; \
                     a socket that is open but not logged in delivers silence, \
                     which reads as a quiet account"
                        .to_string(),
                ));
            }
            reads += 1;
            let Some(text) = wire.read()? else { continue };
            match answer(&text) {
                Handshake::Confirmed => break,
                Handshake::Refused => {
                    return Err(VenueError::Transport(format!(
                        "the venue refused the frame that opens this stream: {text}"
                    )));
                }
                // Not about this frame. A venue is free to say something
                // else first and one of them does.
                Handshake::Unrelated => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod opening_sequence {
    use super::*;
    use std::collections::VecDeque;

    /// A socket that was never opened.
    struct Fake {
        sent: Vec<String>,
        /// `None` is a read that timed out.
        inbox: VecDeque<Option<String>>,
    }

    impl Fake {
        fn with(messages: Vec<Option<&str>>) -> Self {
            Self {
                sent: Vec::new(),
                inbox: messages
                    .into_iter()
                    .map(|m| m.map(str::to_string))
                    .collect(),
            }
        }
    }

    impl Wire for Fake {
        fn send(&mut self, text: &str) -> Result<(), VenueError> {
            self.sent.push(text.to_string());
            Ok(())
        }
        fn read(&mut self) -> Result<Option<String>, VenueError> {
            // An exhausted inbox is a socket with nothing to say, which
            // is a timeout and not an error.
            Ok(self.inbox.pop_front().flatten())
        }
    }

    /// The shape of answer a venue that logs in on the socket gives.
    fn login_answer(message: &str) -> Handshake {
        if message.contains(r#""event":"login""#) {
            if message.contains(r#""code":"0""#) {
                Handshake::Confirmed
            } else {
                Handshake::Refused
            }
        } else if message.contains(r#""event":"error""#) {
            Handshake::Refused
        } else {
            Handshake::Unrelated
        }
    }

    #[test]
    fn a_venue_that_authenticates_in_its_url_sends_nothing() {
        let mut wire = Fake::with(vec![]);
        perform_opening(&[], &mut wire, OPENING_READ_BUDGET).expect("no frames is not a failure");
        assert!(
            wire.sent.is_empty(),
            "an empty opening must cost the existing venue nothing"
        );
    }

    #[test]
    fn a_login_is_confirmed_before_the_next_frame_is_sent() {
        // Sequential on purpose: a subscription that overtakes the login
        // is refused, and that refusal reads like a failed login.
        let mut wire = Fake::with(vec![Some(r#"{"event":"login","code":"0"}"#)]);
        perform_opening(
            &[
                Opening::awaited("login".to_string(), login_answer),
                Opening::sent("subscribe".to_string()),
            ],
            &mut wire,
            OPENING_READ_BUDGET,
        )
        .expect("a confirmed login");
        assert_eq!(wire.sent, vec!["login", "subscribe"]);
    }

    #[test]
    fn a_refused_login_fails_the_connection_and_carries_the_reason() {
        // The failure that arrives exactly where the confirmation would.
        let refusal = r#"{"event":"error","code":"60009","msg":"Login failed."}"#;
        let mut wire = Fake::with(vec![Some(refusal)]);
        let e = perform_opening(
            &[
                Opening::awaited("login".to_string(), login_answer),
                Opening::sent("subscribe".to_string()),
            ],
            &mut wire,
            OPENING_READ_BUDGET,
        )
        .expect_err("a refused login is not a usable stream");
        assert!(
            format!("{e}").contains("Login failed."),
            "the venue's own words are the ones worth reporting: {e}"
        );
        assert_eq!(
            wire.sent,
            vec!["login"],
            "nothing may be subscribed on a stream that did not log in"
        );
    }

    #[test]
    fn a_message_that_is_not_an_answer_does_not_end_the_wait() {
        // A venue is free to say something else first, and one does.
        let mut wire = Fake::with(vec![
            Some(r#"{"event":"channel-conn-count","channel":"orders"}"#),
            None,
            Some(r#"{"event":"login","code":"0"}"#),
        ]);
        perform_opening(
            &[Opening::awaited("login".to_string(), login_answer)],
            &mut wire,
            OPENING_READ_BUDGET,
        )
        .expect("an unrelated message is not a refusal");
    }

    #[test]
    fn a_login_that_is_never_answered_gives_up_rather_than_waiting_forever() {
        // The whole point of the budget: an open socket that never
        // logged in answers every read with a timeout, which is exactly
        // what a quiet account looks like.
        let mut wire = Fake::with(vec![None; 4]);
        let e = perform_opening(
            &[Opening::awaited("login".to_string(), login_answer)],
            &mut wire,
            3,
        )
        .expect_err("silence is not a login");
        assert!(format!("{e}").contains("never answered"), "{e}");
    }

    #[test]
    fn a_frame_that_needs_no_answer_does_not_wait_for_one() {
        let mut wire = Fake::with(vec![]);
        perform_opening(
            &[Opening::sent("subscribe".to_string())],
            &mut wire,
            OPENING_READ_BUDGET,
        )
        .expect("a frame that awaits nothing");
        assert_eq!(wire.sent, vec!["subscribe"]);
    }

    #[test]
    fn two_openings_are_the_same_when_the_same_thing_gets_sent() {
        // Equality is over the frame, because comparing two function
        // pointers is not something the language promises an answer to.
        fn other_answer(_: &str) -> Handshake {
            Handshake::Confirmed
        }
        let a = Opening::awaited("login".to_string(), login_answer);
        let b = Opening::awaited("login".to_string(), other_answer);
        assert_eq!(a, b);
        assert_ne!(a, Opening::sent("login".to_string()));
        assert_ne!(a, Opening::awaited("other".to_string(), login_answer));
    }

    #[test]
    fn a_login_frame_does_not_print_its_signature() {
        // It carries an HMAC over the account's secret.
        let opening = Opening::awaited(r#"{"op":"login","sign":"AAAA"}"#.to_string(), login_answer);
        let shown = format!("{opening:?}");
        assert!(!shown.contains("AAAA"), "a signature must not reach a log");
        assert!(shown.contains("awaits_answer"));
    }
}

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

    #[test]
    fn silence_past_the_window_is_death_and_silence_within_it_is_not() {
        // `Instant` has no epoch to build from, so this walks backwards
        // from now. On a machine whose monotonic clock has not run that
        // long there is nowhere to walk back to, and the judgement is
        // unreachable rather than wrong.
        let Some(long_ago) =
            Instant::now().checked_sub(DEFAULT_STALE_AFTER + Duration::from_secs(60))
        else {
            return;
        };
        assert!(
            silence_verdict(long_ago, DEFAULT_STALE_AFTER).is_some(),
            "a stream past its whole window is the failure this exists for"
        );

        let Some(recent) = Instant::now().checked_sub(VENUE_PING_PERIOD) else {
            return;
        };
        assert!(
            silence_verdict(recent, DEFAULT_STALE_AFTER).is_none(),
            "one ping period of quiet is an ordinary account, not a dead link"
        );
    }

    #[test]
    fn the_window_outlasts_a_lost_ping() {
        // A window shorter than two ping periods would condemn a healthy
        // stream the first time one ping went missing — a reconnection
        // storm on a working link, which is worse than no check at all.
        assert!(
            DEFAULT_STALE_AFTER >= VENUE_PING_PERIOD * 3,
            "the staleness window must outlast more than one lost ping"
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

/// The same bound as `oq_l2feed::ws::connect_bounded`, kept in both
/// crates because neither depends on the other.
///
/// How long opening a WebSocket may take, from the TCP connect to the
/// server's `101`.
///
/// `tungstenite::connect` sets no timeout on any of it. A peer that
/// completes the TCP handshake and then says nothing — or goes away
/// after the TLS hello — holds the calling thread in a read forever, and
/// with the signal handler restarting interrupted reads, SIGTERM cannot
/// end it either. A capture process sat in exactly that state for 29
/// hours with an ESTABLISHED socket and an empty file.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Open a WebSocket with every step bounded by `timeout`.
///
/// The TCP connect uses `connect_timeout` per resolved address, and the
/// socket's read and write timeouts are set *before* the TLS and HTTP
/// handshakes rather than after them, which is where the unbounded wait
/// was. The caller sets its own read timeout for the data phase; the
/// write timeout stays, so a send cannot block forever either.
///
/// Name resolution is not bounded: the standard library offers no way
/// to, and a resolver that hangs is a different failure from a peer
/// that does.
///
/// # Errors
/// Anything the connect or the handshake reports, a handshake that did
/// not finish within `timeout` included.
pub fn connect_bounded(
    url: &str,
    timeout: Duration,
) -> Result<
    tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    tungstenite::Error,
> {
    use std::net::{TcpStream, ToSocketAddrs};
    use tungstenite::client::{IntoClientRequest, uri_mode};
    use tungstenite::stream::Mode;

    let request = url.into_client_request()?;
    let mode = uri_mode(request.uri())?;
    let host = request
        .uri()
        .host()
        .ok_or(tungstenite::Error::Url(
            tungstenite::error::UrlError::NoHostName,
        ))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = request.uri().port_u16().unwrap_or(match mode {
        Mode::Plain => 80,
        Mode::Tls => 443,
    });

    let mut last = None;
    let mut stream = None;
    for addr in (host.as_str(), port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let stream = stream.ok_or_else(|| {
        tungstenite::Error::Io(last.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{host} resolved to nothing"),
            )
        }))
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let _ = stream.set_nodelay(true);

    match tungstenite::client_tls(request, stream) {
        Ok((socket, _response)) => Ok(socket),
        Err(tungstenite::HandshakeError::Failure(e)) => Err(e),
        // A blocking socket interrupts the handshake only when a read or
        // write timed out.
        Err(tungstenite::HandshakeError::Interrupted(_)) => {
            Err(tungstenite::Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("the WebSocket handshake did not complete within {timeout:?}"),
            )))
        }
    }
}
