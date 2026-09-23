//! Sending an order, and the three things that can happen next.
//!
//! The types here are venue-independent on purpose. Everything specific
//! to one exchange — its paths, its parameter names, its error codes —
//! is in the adapter that implements [`Execution`]; what a caller sees
//! is the same shape whichever venue is behind it. Market data already
//! has this seam, and a second exchange proved it by needing no change
//! to the binary that captures with it. The order path had none, which
//! is the more expensive half to leave open: incidents originate at the
//! venue boundary far more often than in the matching kernel.
//!
//! # Why placement does not return a `Result`
//!
//! An order is not a request that returns a result. It is a claim
//! submitted to a system that will act on it whether or not the answer
//! comes back. A timeout does not mean the order failed; it means
//! nobody knows. Folding that into `Err` is the defect that produces
//! duplicate positions: a caller that retries has, half the time,
//! placed two orders, and one that gives up has, half the time,
//! abandoned a live one.
//!
//! So [`Placed`] has three variants and the third is not an error. The
//! compiler makes every caller decide what to do about not knowing.
//!
//! # Why every order carries an id the caller chose
//!
//! Because the answer to "did it land?" has to be askable, and an id
//! assigned by the venue cannot answer it — the whole problem is that
//! the venue's answer never arrived. A client order id is chosen before
//! the request is sent, so it survives the request's failure and can be
//! used to interrogate the venue afterwards.
//!
//! Idempotency is not a feature of this design. It is the reason it is
//! safe.

use oq_types::{Instrument, QtyLots, Side, TimeInForce};

/// Which deployment of a venue to talk to.
///
/// A type rather than a string, because a string that is wrong by one
/// character is production. There is no way to arrive at [`Endpoint::Live`]
/// except by naming it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// The venue's test deployment. Same API, same credentials shape,
    /// different money — which is none.
    Testnet,
    /// Real money.
    Live,
}

/// Which leg of a hedged account an order applies to.
///
/// A venue can hold one position per contract or two — a long leg and a
/// short leg carried at once. Under the second, an order that does not
/// say which leg it belongs to is refused, and the refusal talks about
/// a position side the caller never mentioned.
///
/// [`PositionSide::OneWay`] omits the parameter, which is what an
/// account holding a single net position expects. It is not a default
/// that happens to work: sending a leg on a one-way account is refused
/// just as surely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PositionSide {
    /// One net position per contract.
    #[default]
    OneWay,
    /// The long leg of a hedged account.
    Long,
    /// The short leg of a hedged account.
    Short,
}

impl PositionSide {
    /// Whether this account carries both legs at once.
    #[must_use]
    pub const fn is_hedged(self) -> bool {
        matches!(self, Self::Long | Self::Short)
    }
}

/// An order to send.
///
/// Prices and quantities are fixed-point integers, formatted against
/// the instrument's own precision at the moment of sending. They are
/// never floats: a venue rejects a price with too many decimal places,
/// and printing a float is exactly how a price acquires them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOrder {
    /// The venue's symbol, in the venue's own spelling.
    pub symbol: String,
    pub side: Side,
    /// `None` for a market order.
    pub limit_price: Option<oq_types::PriceTicks>,
    pub qty: QtyLots,
    /// How long a limit order rests. Ignored for market orders.
    pub tif: TimeInForce,
    /// Chosen by the caller before sending, and the only handle that
    /// survives a request whose answer never came back.
    pub client_id: String,
    /// Refuse to open or increase a position with this order.
    ///
    /// Mutually exclusive with a hedged [`NewOrder::position_side`]: a
    /// venue that carries both legs expresses "close" by naming the leg
    /// rather than by this flag, and refuses an order that sets both.
    pub reduce_only: bool,
    /// Which leg, on a hedged account.
    pub position_side: PositionSide,
}

/// What the venue said, or the fact that it did not say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placed {
    /// The venue named the order. It exists.
    Accepted(OrderAck),
    /// The venue refused, and said why. The order does not exist, and
    /// this is final: retrying the identical request gets the identical
    /// refusal.
    Rejected(Reject),
    /// Nobody knows. The order may exist.
    ///
    /// Not an error — an error would let a caller `?` past it, and the
    /// one thing that must not happen here is passing it on unhandled.
    /// Resolve with [`Execution::order_status`] using the client id.
    Unknown(Unresolved),
}

/// The venue's acknowledgement of an order that exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderAck {
    /// The venue's own id, as the venue wrote it.
    ///
    /// Text, not a number. Two venues here number their orders and a
    /// third names them with a UUID, and an integer field would have
    /// meant either refusing that venue or inventing a number for it —
    /// and an invented id is one that cannot be quoted back in a
    /// support ticket.
    ///
    /// Nothing joins on this. [`OrderUpdate::client_id`] is the join
    /// key, by `L4`, precisely so the venue's own handle can be whatever
    /// the venue likes.
    pub venue_id: String,
    /// The id the caller chose, echoed back.
    pub client_id: String,
    /// The venue's status word, unmapped.
    ///
    /// Deliberately not an enum. A venue that invents a status this
    /// build has never heard of should surface it, not be forced into
    /// the nearest known variant — which is how an unrecognised state
    /// becomes a wrong one.
    pub status: String,
    /// Quantity already filled, in the venue's own decimal text.
    pub executed_qty: String,
}

/// A refusal, in the venue's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reject {
    /// The venue's error code, when it gave one.
    pub code: Option<i64>,
    /// The venue's message.
    pub message: String,
}

/// A placement whose outcome is not known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    /// The id to ask about.
    pub client_id: String,
    /// What happened instead of an answer.
    pub reason: String,
}

/// What a venue must provide to be traded on.
pub trait Execution {
    /// Send an order.
    ///
    /// `instrument` supplies the precision the venue expects; sending a
    /// price with more decimal places than the contract quotes is
    /// rejected, and formatting a float is how that happens.
    fn place(&self, order: &NewOrder, instrument: &Instrument) -> Placed;

    /// Withdraw an order by the id the caller gave it.
    ///
    /// Returns [`Placed`] for the same reason placement does: a cancel
    /// whose answer never arrived may or may not have cancelled, and a
    /// caller that assumes it failed will size its next order against a
    /// position that is about to change.
    fn cancel(&self, symbol: &str, client_id: &str) -> Placed;

    /// Ask the venue about an order by client id.
    ///
    /// `Ok(None)` means the venue has no such order — which, after an
    /// [`Placed::Unknown`], is the answer that says the order never
    /// landed and may be sent again.
    ///
    /// # Errors
    /// Whatever the venue or the transport reports.
    fn order_status(
        &self,
        symbol: &str,
        client_id: &str,
    ) -> Result<Option<OrderAck>, crate::VenueError>;

    /// What the account stream would have said about an order, rebuilt
    /// from the venue's own records.
    ///
    /// One report per trade, each carrying the venue's trade id, then a
    /// final report with the order's status when it has ended — the same
    /// shape the stream delivers, so a caller books them through the
    /// same path and the trade id deduplicates whatever the stream did
    /// deliver. An order still resting and never filled yields nothing.
    ///
    /// This exists because a stream that drops and reconnects does not
    /// replay what it missed. A take-profit filled during such a gap and
    /// the process went on believing it held the position: the
    /// reconciler saw the difference and halted, correctly, but nothing
    /// could have told it *why*, and the strategy never learned its exit
    /// had happened.
    ///
    /// `Ok(None)` means this adapter cannot answer — a third state, as
    /// with [`crate::account::Account::fees_charged`]: an adapter that
    /// has not implemented this says so rather than answering "nothing
    /// happened".
    ///
    /// # Errors
    /// Whatever the venue or the transport reports.
    fn recover_order(
        &self,
        _symbol: &str,
        _client_id: &str,
    ) -> Result<Option<Vec<OrderUpdate>>, crate::VenueError> {
        Ok(None)
    }
}

/// A fixed-point integer as the decimal text a venue expects.
///
/// Exact by construction. The alternative — dividing into a float and
/// printing it — is how `0.1` becomes `0.09999999999999999` and a
/// perfectly valid order is refused for a precision it never had.
#[must_use]
pub fn decimal(value: i64, scale: u8) -> String {
    if scale == 0 {
        return value.to_string();
    }
    let sign = if value < 0 { "-" } else { "" };
    let magnitude = value.unsigned_abs();
    let divisor = 10_u64.pow(u32::from(scale));
    let whole = magnitude / divisor;
    let frac = magnitude % divisor;
    format!("{sign}{whole}.{frac:0width$}", width = usize::from(scale))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_venue_id_can_be_a_name_rather_than_a_number() {
        // Two venues here number their orders; a third names them with
        // a UUID. An integer field would have meant refusing that venue
        // or inventing a number for it, and an invented id is one that
        // cannot be quoted back in a support ticket.
        //
        // Nothing joins on this — `client_id` is the join key by L4 —
        // which is exactly why the venue's own handle is allowed to be
        // whatever the venue likes.
        let ack = OrderAck {
            venue_id: "179f9af8-e45e-469d-b3e9-2fd4675cb7d0".to_string(),
            client_id: "oq1".to_string(),
            status: "placed".to_string(),
            executed_qty: "0".to_string(),
        };
        assert_eq!(ack.venue_id, "179f9af8-e45e-469d-b3e9-2fd4675cb7d0");
        assert_eq!(ack.client_id, "oq1", "the join key is still the caller's");
    }

    #[test]
    fn a_fixed_point_price_becomes_exact_decimal_text() {
        // 120_000.0 at one decimal place, the way BTC quotes.
        assert_eq!(decimal(1_200_000, 1), "120000.0");
        // A tenth, which is the value a float prints wrongly.
        assert_eq!(decimal(1, 1), "0.1");
        assert_eq!(decimal(1, 8), "0.00000001");
    }

    #[test]
    fn a_scale_of_zero_prints_an_integer_without_a_point() {
        // A venue that quotes whole contracts rejects "3.", so the
        // fractional part has to be absent rather than empty.
        assert_eq!(decimal(3, 0), "3");
    }

    #[test]
    fn the_fraction_keeps_its_leading_zeros() {
        // 1.005, not 1.5 — the bug this guards against turns a price
        // into two hundred times itself.
        assert_eq!(decimal(1005, 3), "1.005");
    }

    #[test]
    fn negatives_keep_their_sign_and_their_magnitude() {
        assert_eq!(decimal(-1005, 3), "-1.005");
    }

    #[test]
    fn an_unknown_outcome_is_not_an_error_type() {
        // A compile-time assertion in test form: the third outcome sits
        // in the success type, so `?` cannot skip past it.
        let p = Placed::Unknown(Unresolved {
            client_id: "abc".into(),
            reason: "timeout".into(),
        });
        assert!(matches!(p, Placed::Unknown(_)));
    }
}

/// Reads a venue's account-stream messages.
///
/// A trait rather than a function pointer, because reading a message can
/// require knowing something first: OKX reports sizes in contracts, and
/// turning one into a quantity of the underlying needs the contract
/// value from its listing. A pointer has nowhere to keep that, and
/// passing contracts upward instead would put one venue's unit into a
/// type whose whole purpose is not to have one.
///
/// `Send + Sync` because a stream is a value a caller may move between
/// threads, and finding out otherwise at the point of moving it is a
/// worse time than now.
pub trait Events: Send + Sync {
    /// Every account event this message carries, in the order it
    /// carries them.
    ///
    /// A list, not an option. Binance sends one event per message and
    /// OKX sends a `data` array; a reader that returned the first and
    /// dropped the rest would lose a fill, and a lost fill is a position
    /// that never existed.
    ///
    /// Empty is for a message that is not an account event — a
    /// subscription acknowledgement, a heartbeat. It is not for one that
    /// is an account event and could not be read: that is
    /// [`UserEvent::Other`], which keeps the payload so a venue that
    /// added an event type produces something a reader can see rather
    /// than nothing.
    fn read(&self, message: &str) -> Vec<UserEvent>;
}

/// Where a user data stream lives, and the key that opens it.
///
/// The key is a bearer credential with an expiry: anyone holding it can
/// read the account's order flow, and it stops working an hour after it
/// was issued unless renewed. Both halves matter — the first is why it
/// is not printed, the second is why a stream that has been quiet is
/// not evidence of a quiet account.
/// Not `PartialEq`: two streams are the same when they read the same
/// account, and the reader below is a trait object whose identity is an
/// address rather than a meaning. Nothing compared two of these, so the
/// derive went rather than acquiring a sense it could not keep.
#[derive(Clone)]
pub struct UserStream {
    /// Full websocket URL, key included.
    url: String,
    /// The key alone, for renewal and closing.
    key: String,
    /// How this venue's messages are read.
    events: std::sync::Arc<dyn Events>,
    /// What has to be said on the socket before it carries anything.
    ///
    /// Empty for a venue that authenticates in the URL. A venue that
    /// authenticates on the socket puts its login here, and the reader
    /// sends these in order before it reports a single event — because
    /// a stream that is open but not logged in delivers silence, and
    /// silence is what this crate spent thirty-three hours mistaking
    /// for a quiet account.
    opening: Vec<Opening>,
}

/// A frame to send when a stream opens, and how to know it worked.
#[derive(Clone)]
pub struct Opening {
    frame: String,
    answer: Option<fn(&str) -> Handshake>,
}

/// Equality is over what gets sent, not over where the answer reader
/// lives.
///
/// Written out because two function pointers comparing equal is not
/// something the language promises — the same function can have two
/// addresses — so a derived `PartialEq` here would be a comparison that
/// is right in a debug build and arbitrary in a release one. What a
/// caller means by "the same opening" is the same frame, waited on the
/// same way.
impl PartialEq for Opening {
    fn eq(&self, other: &Self) -> bool {
        self.frame == other.frame && self.answer.is_some() == other.answer.is_some()
    }
}

impl Eq for Opening {}

/// What one message says about a frame that is waiting for an answer.
///
/// Three outcomes rather than two, for the same reason placement has
/// three: a message that is not an answer is not a refusal, and folding
/// them together would either abandon a login that was about to succeed
/// or accept one that had already failed. OKX answers a bad passphrase
/// with an error frame that arrives exactly where the confirmation
/// would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handshake {
    /// The frame was accepted.
    Confirmed,
    /// The venue refused it. Carried as text because the reason is the
    /// venue's and belongs in the error a caller sees.
    Refused,
    /// Not about this frame. Keep reading.
    Unrelated,
}

impl Opening {
    /// A frame that needs no answer, such as a subscription a venue
    /// acknowledges by simply starting to deliver.
    #[must_use]
    pub const fn sent(frame: String) -> Self {
        Self {
            frame,
            answer: None,
        }
    }

    /// A frame whose answer decides whether the stream is usable.
    #[must_use]
    pub const fn awaited(frame: String, answer: fn(&str) -> Handshake) -> Self {
        Self {
            frame,
            answer: Some(answer),
        }
    }

    /// The text to send.
    #[must_use]
    pub fn frame(&self) -> &str {
        &self.frame
    }

    /// How to read a message that might answer it.
    #[must_use]
    pub const fn answer(&self) -> Option<fn(&str) -> Handshake> {
        self.answer
    }
}

/// By hand, so a login frame cannot print its signature into a log.
impl core::fmt::Debug for Opening {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Opening")
            .field("frame", &"<redacted>")
            .field("awaits_answer", &self.answer.is_some())
            .finish()
    }
}

impl UserStream {
    /// `events` is required rather than defaulted.
    ///
    /// A default would be one venue's reader, and the venue that forgot
    /// to pass its own would not fail to compile. It would connect, stay
    /// silent, and look exactly like an account where nothing happens —
    /// a shape this crate has already paid thirty-three hours for once.
    #[must_use]
    pub fn new(url: String, key: String, events: std::sync::Arc<dyn Events>) -> Self {
        Self {
            url,
            key,
            events,
            opening: Vec::new(),
        }
    }

    /// How to read a message from this stream.
    #[must_use]
    pub fn events(&self) -> std::sync::Arc<dyn Events> {
        std::sync::Arc::clone(&self.events)
    }

    /// The frames to send once the socket is open.
    #[must_use]
    pub fn with_opening(mut self, opening: Vec<Opening>) -> Self {
        self.opening = opening;
        self
    }

    /// What has to be said before this stream carries anything.
    #[must_use]
    pub fn opening(&self) -> &[Opening] {
        &self.opening
    }

    /// The URL to connect to. Contains the key.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The key, for renewal.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }
}

/// By hand, so a stream cannot print its own credential into a log.
impl core::fmt::Debug for UserStream {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UserStream")
            .field("url", &"<redacted>")
            .field("key", &"<redacted>")
            .field("opening", &self.opening.len())
            .finish()
    }
}

/// Something the venue pushed about the account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserEvent {
    /// An order changed state.
    Order(OrderUpdate),
    /// The key expired. The stream is closed and whatever happened
    /// after it closed was not seen — a gap, not silence.
    Expired,
    /// Recognised as an account event but not mapped by this build.
    ///
    /// Kept rather than dropped. A venue that adds an event type should
    /// produce something a reader can see, not nothing.
    Other { kind: String, payload: String },
}

/// An order's state, as the venue reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderUpdate {
    pub symbol: String,
    /// The id the caller chose. The join key for everything else.
    pub client_id: String,
    /// The venue's own id, as the venue wrote it. See [`OrderAck::venue_id`].
    pub venue_id: String,
    /// The venue's status word, unmapped for the same reason as
    /// [`OrderAck::status`].
    pub status: String,
    /// Quantity filled by this event, in the venue's decimal text.
    pub last_qty: String,
    /// Quantity filled in total.
    pub cumulative_qty: String,
    /// Price of this fill.
    pub last_price: String,
    /// The venue's trade id, or `None` when the event is not a fill.
    ///
    /// The deduplication key: a stream that reconnects can redeliver,
    /// and a fill counted twice is a position that never existed.
    /// `"BUY"` or `"SELL"`, in the venue's own spelling.
    ///
    /// Read rather than inferred. Without it a fill can only be booked
    /// by looking up what this process asked for, which works for its
    /// own orders and not at all for anything else on the account — and
    /// "we sent a buy" is a different fact from "the venue filled a
    /// buy".
    pub side: String,
    /// Which leg, on a hedged account: `LONG`, `SHORT`, or `BOTH`.
    ///
    /// Without it a fill cannot be read as opening or closing. A sell on
    /// the long leg reduces it; the same sell on the short leg opens.
    /// Assuming every fill opens leaves a position that never goes away
    /// in the books, which is a position a strategy will keep trying to
    /// close — and did, seven times in forty seconds on a live account.
    pub position_side: String,
    /// Whether this fill made liquidity.
    ///
    /// Decides the fee, which is the difference between a rebate and a
    /// charge on some venues — an order of magnitude, not a rounding.
    pub maker: bool,
    pub trade_id: Option<i64>,
    /// Venue event time, milliseconds.
    pub event_ms: i64,
}
