//! The only path that can send an order.
//!
//! [`Session::submit`] consults the gate, takes the permit it returns,
//! and sends *that* — so there is no route through this crate that
//! reaches a venue without having been checked, and no gap between what
//! was checked and what was sent.
//!
//! # Startup refuses rather than warns
//!
//! A process that starts trading beside a position it does not know
//! about has risk limits that mean nothing: they are computed against a
//! picture that is already wrong, and the first order it sends is sized
//! by that picture. [`Session::start`] therefore refuses to begin when
//! the venue holds something the caller did not declare.
//!
//! It is the cheapest possible moment to fail. Nothing has been sent,
//! nothing is resting, and the operator is present — none of which will
//! be true the next time the discrepancy matters.

use oq_gateway::{Execution, NewOrder, Placed, PositionSide, PositionSnapshot, VenueError};
use oq_risk::{AccountState, Breach, Decision, Permit, ProposedOrder, RiskGate};
use oq_types::{Instrument, Nanos, PriceTicks, Side};

use crate::book::{Book, Position};
use crate::latency::Latency;
use crate::record::{OutcomeTag, Record};

/// Why a session would not start.
#[derive(Debug, Clone, PartialEq)]
pub enum StartupRefusal {
    /// The venue holds a position the caller did not declare.
    ///
    /// Fatal by design. Every alternative — warn, adopt, ignore —
    /// begins trading against a picture known to be incomplete.
    UndeclaredPosition {
        symbol: String,
        side: String,
        amount: f64,
    },
    /// The venue holds an order the caller did not declare.
    UndeclaredOrder { client_id: String },
}

impl core::fmt::Display for StartupRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UndeclaredPosition {
                symbol,
                side,
                amount,
            } => write!(
                f,
                "the venue holds {amount} of {symbol} ({side}) that this process was not \
                 told about; starting would size every order against a picture that is \
                 already wrong"
            ),
            Self::UndeclaredOrder { client_id } => {
                write!(
                    f,
                    "the venue has a resting order this process did not place: {client_id}"
                )
            }
        }
    }
}

impl core::error::Error for StartupRefusal {}

/// What happened to a submission.
#[derive(Debug, Clone, PartialEq)]
pub enum Submission {
    /// Sent, and the venue named it.
    Sent(String),
    /// The gate said no. Nothing was sent.
    Refused(Breach),
    /// The venue said no.
    Rejected(String),
    /// Sent, outcome unknown, and asking the venue did not settle it.
    ///
    /// The caller must not resend: the id is known, so the question is
    /// answerable later, and resending is the one action that can turn
    /// "maybe one order" into "certainly two".
    ///
    /// The id is carried rather than folded into the message, because
    /// "answerable later" is only true for a caller that still has it.
    /// It was a string for a while, and in that form the sentence above
    /// was advice nobody could follow.
    Unresolved { client_id: String, why: String },
}

/// What a session trades and how it identifies its orders.
///
/// Grouped rather than passed one by one because these are the facts
/// that do not change while the session runs, and a start function
/// taking nine positional arguments is one where two of the same type
/// eventually get swapped.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// The venue's symbol, in the venue's own spelling.
    pub symbol: String,
    /// Precision, grid and economics for that contract.
    pub instrument: Instrument,
    /// Which leg, on a hedged account.
    pub position_side: PositionSide,
    /// Prefix for client order ids.
    ///
    /// Should identify this process: the ids are what a human reads
    /// when reconciling by hand, and two processes sharing a prefix
    /// makes that reading ambiguous exactly when it matters.
    pub id_prefix: String,
}

impl Submission {
    /// Whether an order reached the venue.
    #[must_use]
    pub const fn is_sent(&self) -> bool {
        matches!(self, Self::Sent(_))
    }
}

/// A running trading process.
pub struct Session<E: Execution> {
    /// Time from the journal flush returning to the client being called.
    ///
    /// The in-process segment of what G6 asks for. The gate names the
    /// socket write as its far boundary and the HTTP client does not
    /// report that instant — connect, write and read happen inside one
    /// call — so measuring the call would give a number dominated by the
    /// venue's round trip, tens of milliseconds against a hundred
    /// microsecond budget. This measures the part this project controls,
    /// and G6 stays uncertified until the client boundary is instrumented.
    submit_latency: Latency,
    /// Where decisions are written, if anywhere.
    ///
    /// Optional because a session without one is still a working session
    /// — it simply cannot be replayed or attributed afterwards, and that
    /// is a choice a caller should have to make rather than a default it
    /// falls into. `oq-trade` opens one unless told not to.
    journal: Option<oq_journal::Writer>,
    venue: E,
    gate: RiskGate,
    book: Book,
    symbol: String,
    instrument: Instrument,
    position_side: PositionSide,
    /// Increments per order so client ids do not repeat within a run.
    sequence: u64,
    prefix: String,
}

impl<E: Execution> Session<E> {
    /// Check the venue against what the caller expects, and start only
    /// if they agree.
    ///
    /// `expected` is what the process believes it should find — empty
    /// for a fresh start. Anything the venue holds that is not in it
    /// stops the session.
    ///
    /// # Errors
    /// [`StartupRefusal`] when the venue holds something undeclared, or
    /// could not be asked.
    pub fn start(
        venue: E,
        gate: RiskGate,
        config: SessionConfig,
        venue_positions: &[PositionSnapshot],
        venue_orders: &[String],
        expected: &[Position],
    ) -> Result<Self, StartupRefusal> {
        for p in venue_positions {
            // A leg that has been closed reads as a position of zero
            // rather than as an absence, and refusing to start over one
            // would make every restart a manual step.
            if p.amount == 0.0 {
                continue;
            }
            let declared = expected
                .iter()
                .any(|e| e.symbol == p.symbol && e.side == p.position_side && e.amount == p.amount);
            if !declared {
                return Err(StartupRefusal::UndeclaredPosition {
                    symbol: p.symbol.clone(),
                    side: p.position_side.clone(),
                    amount: p.amount,
                });
            }
        }
        if let Some(id) = venue_orders.first() {
            return Err(StartupRefusal::UndeclaredOrder {
                client_id: id.clone(),
            });
        }

        // Scoped to this process's own client ids: the account stream
        // carries every order the account places, including another
        // system's, and counting those would let them consume this
        // process's limits.
        let mut book = Book::owning(&config.id_prefix);
        book.adopt(
            venue_positions
                .iter()
                .filter(|p| p.amount != 0.0)
                .map(|p| Position {
                    symbol: p.symbol.clone(),
                    side: p.position_side.clone(),
                    amount: p.amount,
                })
                .collect(),
        );

        Ok(Self {
            submit_latency: Latency::new(),
            journal: None,
            venue,
            gate,
            book,
            symbol: config.symbol,
            instrument: config.instrument,
            position_side: config.position_side,
            sequence: 0,
            prefix: config.id_prefix,
        })
    }

    /// Write decisions to `journal` from here on.
    ///
    /// Records go down **before** the venue is called, which is the
    /// ordering `oq-core` already enforces for its own path. Sending
    /// first and recording after leaves, on a crash in between, a live
    /// order this process's own journal has never heard of — and no
    /// recovery is possible, because the client id that could ask the
    /// venue about it was never written. Recording first leaves the
    /// opposite: a record with no outcome beside it, which is exactly a
    /// placement whose answer never arrived, asked after a restart
    /// instead of after a timeout.
    #[must_use]
    pub fn journalling(mut self, journal: oq_journal::Writer) -> Self {
        let start = Record::SessionStart {
            prefix: self.prefix.clone(),
            symbol: self.symbol.clone(),
            price_scale: self.instrument.price_scale,
            qty_scale: self.instrument.qty_scale,
        };
        self.journal = Some(journal);
        self.write(&start);
        self
    }

    /// Append one record, if journalling, flushing before returning.
    ///
    /// Failures are reported and do not stop the session. A journal that
    /// cannot be written is a lost audit trail; refusing to trade
    /// because of it would turn a recording problem into a trading
    /// outage, and the venue does not care either way. The count of
    /// failures is what a reader should look at.
    fn write(&mut self, record: &Record) {
        let Some(journal) = self.journal.as_mut() else {
            return;
        };
        let payload = record.encode();
        if let Err(e) = journal.append(record.kind(), &payload) {
            eprintln!("journal: could not append {:?}: {e}", record.kind());
            return;
        }
        // Flushed here rather than on drop: the whole point is that the
        // record exists before the order does, and a record sitting in a
        // buffer does not exist to anything that reads the file.
        if let Err(e) = journal.flush() {
            eprintln!("journal: could not flush: {e}");
        }
    }

    /// Positions this run took over from the venue at startup.
    ///
    /// Call once, after the journal is open and before anything is sent.
    /// Adopting a position and recording the adoption are two different
    /// acts, and only the second one has to wait for the writer — which
    /// is why this is a separate call rather than something `adopt` does.
    ///
    /// Nothing is written when the run has no journal, and nothing is
    /// written for an empty list: a record saying "took over nothing" and
    /// no record at all are the same claim, and only one of them can be
    /// mistaken for a run that forgot to look.
    pub fn record_reconciled(
        &mut self,
        at: oq_types::Nanos,
        legs: Vec<(String, String, i64, i64)>,
    ) {
        if legs.is_empty() {
            return;
        }
        self.write(&Record::Reconciled { at, legs });
    }

    /// What the strategy is waiting for, sampled on a timer.
    ///
    /// Nothing is written when the strategy names no conditions: an
    /// empty record would claim the question was asked and answered,
    /// when it was asked and declined.
    pub fn record_waiting(&mut self, at: oq_types::Nanos, entries: Vec<(String, i64)>) {
        if entries.is_empty() {
            return;
        }
        self.write(&Record::Waiting { at, entries });
    }

    /// A fill the venue reported and this process booked.
    ///
    /// Written after the books accept it, so the journal contains what
    /// was believed rather than what arrived — a redelivered fill is
    /// discarded by the books and must not appear here twice, or a
    /// replay would count it twice.
    ///
    /// The strategy's own order id travels with it. Without that a
    /// replay can rebuild a *position* but not a *ladder*: which rungs
    /// had filled is the difference between resuming and starting over
    /// on top of one.
    pub fn record_fill(&mut self, fill: &oq_types::Fill, client_id: &str) {
        self.write(&Record::Fill {
            at: fill.stamp.exch,
            client_id: client_id.to_string(),
            trade_id: i64::try_from(fill.trade.0).unwrap_or(0),
            qty: oq_gateway::exec::decimal(fill.qty.0, self.instrument.qty_scale),
            price: oq_gateway::exec::decimal(fill.price.0, self.instrument.price_scale),
            order: fill.order.0,
            side: format!("{:?}", fill.side),
        });
    }

    /// A tick the strategy is about to see.
    pub fn record_tick(&mut self, tick: &oq_engine::Tick) {
        self.write(&Record::Tick {
            at: tick.stamp.exch,
            last: tick.last,
            bid: tick.bid,
            ask: tick.ask,
            volume: tick.volume,
        });
    }

    /// The in-process submit latency, so far.
    #[must_use]
    pub const fn submit_latency(&self) -> &Latency {
        &self.submit_latency
    }

    /// The gate, for tripping the kill switch from outside the loop.
    #[must_use]
    pub const fn gate(&self) -> &RiskGate {
        &self.gate
    }

    #[must_use]
    pub const fn book(&self) -> &Book {
        &self.book
    }

    /// Send an order, if the gate allows it.
    ///
    /// The only route to a venue in this crate, and it cannot be taken
    /// without a permit because the permit is what carries the order.
    pub fn submit(&mut self, order: ProposedOrder, mark: PriceTicks, now: Nanos) -> Submission {
        let account = AccountState {
            // The position the venue last confirmed, not zero. A limit
            // compared against a hardcoded zero can never fire, which
            // makes the position cap decoration — the exact failure the
            // gate's own documentation warns about.
            position: self.book.net_lots(&self.symbol, self.instrument.qty_scale),
            mark,
            working: self.book.working(),
        };
        let permit = match self.gate.check(&order, &account, &self.instrument, now) {
            Decision::Permit(p) => p,
            Decision::Refuse(b) => {
                self.write(&Record::Refused {
                    at: now,
                    breach: format!("{b:?}"),
                });
                return Submission::Refused(b);
            }
        };
        self.send(&permit, now)
    }

    /// Turn a permit into an order and send it.
    fn send(&mut self, permit: &Permit, now: Nanos) -> Submission {
        let approved = permit.order();
        self.sequence += 1;
        let client_id = format!("{}-{}", self.prefix, self.sequence);
        let order = NewOrder {
            symbol: self.symbol.clone(),
            side: approved.side,
            limit_price: approved.limit_price,
            qty: approved.qty,
            tif: oq_types::TimeInForce::GoodTilCancel,
            client_id: client_id.clone(),
            // On a one-way account the flag is how a close is expressed.
            // On a hedged one the venue refuses the flag and expects the
            // leg to be named instead, so the flag is dropped and the leg
            // carries the meaning.
            reduce_only: approved.reduce_only && !self.position_side.is_hedged(),
            position_side: leg_for(self.position_side, approved.side, approved.reduce_only),
        };
        // Before the venue, not after. See `journalling`.
        let journalled_at = std::time::Instant::now();
        self.write(&Record::Submitted {
            at: now,
            client_id: client_id.clone(),
            side: order.side,
            limit_price: order.limit_price.unwrap_or(PriceTicks(0)),
            qty: order.qty,
            reduce_only: order.reduce_only,
        });
        // Measured here: everything this process did between the record
        // being durable and the client being handed the order.
        self.submit_latency
            .record(u64::try_from(journalled_at.elapsed().as_nanos()).unwrap_or(u64::MAX));
        let placed = self.venue.place(&order, &self.instrument);
        let (tag, detail) = match &placed {
            Placed::Accepted(a) => (OutcomeTag::Accepted, a.status.clone()),
            Placed::Rejected(r) => (OutcomeTag::Rejected, r.message.clone()),
            Placed::Unknown(u) => (OutcomeTag::Unknown, u.reason.clone()),
        };
        self.write(&Record::Outcome {
            at: now,
            client_id: client_id.clone(),
            tag,
            detail,
        });
        match placed {
            Placed::Accepted(a) => {
                // Believed resting now, not when the acknowledgement
                // arrives. The bound on working orders is checked
                // against this count, and a count that lags a burst is
                // no bound at all.
                self.book.on_sent(&a.client_id);
                Submission::Sent(a.client_id)
            }
            Placed::Rejected(r) => Submission::Rejected(r.message),
            // Ask, using the id chosen before sending. This is the
            // entire reason the id exists.
            Placed::Unknown(_) => match self.venue.order_status(&self.symbol, &client_id) {
                Ok(Some(a)) => {
                    self.book.on_sent(&a.client_id);
                    Submission::Sent(a.client_id)
                }
                Ok(None) => Submission::Rejected(
                    "the order never reached the venue; it may be sent again".to_string(),
                ),
                Err(e) => Submission::Unresolved {
                    client_id: client_id.clone(),
                    why: e.to_string(),
                },
            },
        }
    }

    /// Withdraw a resting order.
    ///
    /// Not gated: the gate exists to stop exposure being taken on, and
    /// a cancel only ever removes it. A gate that could block a cancel
    /// would be a gate that traps a position, which is the failure mode
    /// worth being careful about here — the kill switch stops new
    /// orders and must not stop the way out.
    pub fn cancel(&self, client_id: &str) -> Submission {
        match self.venue.cancel(&self.symbol, client_id) {
            Placed::Accepted(a) => Submission::Sent(a.client_id),
            Placed::Rejected(r) => Submission::Rejected(r.message),
            Placed::Unknown(u) => Submission::Unresolved {
                client_id: u.client_id,
                why: u.reason,
            },
        }
    }

    /// The venue's symbol for what this session trades.
    #[must_use]
    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    /// Adopt the venue's own view after a reconciliation.
    pub fn reconcile(&mut self, venue_positions: &[PositionSnapshot]) {
        self.book.adopt(
            venue_positions
                .iter()
                .filter(|p| p.amount != 0.0)
                .map(|p| Position {
                    symbol: p.symbol.clone(),
                    side: p.position_side.clone(),
                    amount: p.amount,
                })
                .collect(),
        );
    }

    /// Apply a stream event.
    pub fn apply(&mut self, update: &oq_gateway::OrderUpdate) -> bool {
        self.book.apply(update)
    }

    /// Read-only access to the venue, for the loop's own queries.
    #[must_use]
    pub const fn venue(&self) -> &E {
        &self.venue
    }
}

/// Which leg an order belongs to on a hedged account.
///
/// The mapping that a fixed leg per session gets wrong, and gets wrong in
/// the worst available direction. A hedged venue refuses `reduceOnly` and
/// expects the leg to be named, so with the leg pinned to one value a
/// strategy's close became an open on that leg: an exit that increased
/// the position instead of reducing it.
///
/// The leg is decided by the direction and the intent together, which is
/// how the venue reads it — `side` says what to do, `positionSide` says
/// to which leg:
///
/// | intent | side | leg |
/// |---|---|---|
/// | open | buy | long |
/// | open | sell | short |
/// | close | sell | long — selling closes the long |
/// | close | buy | short — buying closes the short |
///
/// A one-way account passes through unchanged: it has one position and
/// names no leg.
#[must_use]
pub const fn leg_for(configured: PositionSide, side: Side, closing: bool) -> PositionSide {
    if !configured.is_hedged() {
        return configured;
    }
    match (side, closing) {
        (Side::Buy, false) | (Side::Sell, true) => PositionSide::Long,
        (Side::Sell, false) | (Side::Buy, true) => PositionSide::Short,
    }
}

/// Anything the loop could not do.
pub type LoopError = VenueError;
