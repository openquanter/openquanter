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
use oq_types::{Instrument, Nanos, PriceTicks};

use crate::book::{Book, Position};

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
    /// The venue could not be asked.
    Unreachable(String),
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
            Self::Unreachable(e) => write!(f, "the venue could not be asked: {e}"),
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
    Unresolved(String),
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

/// A running trading process.
pub struct Session<E: Execution> {
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

        let mut book = Book::new();
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
            position: oq_types::QtyLots(0),
            mark,
            working: self.book.working(),
        };
        let permit = match self.gate.check(&order, &account, &self.instrument, now) {
            Decision::Permit(p) => p,
            Decision::Refuse(b) => return Submission::Refused(b),
        };
        self.send(&permit)
    }

    /// Turn a permit into an order and send it.
    fn send(&mut self, permit: &Permit) -> Submission {
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
            reduce_only: approved.reduce_only && !self.position_side.is_hedged(),
            position_side: self.position_side,
        };
        match self.venue.place(&order, &self.instrument) {
            Placed::Accepted(a) => Submission::Sent(a.client_id),
            Placed::Rejected(r) => Submission::Rejected(r.message),
            // Ask, using the id chosen before sending. This is the
            // entire reason the id exists.
            Placed::Unknown(_) => match self.venue.order_status(&self.symbol, &client_id) {
                Ok(Some(a)) => Submission::Sent(a.client_id),
                Ok(None) => Submission::Rejected(
                    "the order never reached the venue; it may be sent again".to_string(),
                ),
                Err(e) => Submission::Unresolved(format!("{client_id}: {e}")),
            },
        }
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

/// Anything the loop could not do.
pub type LoopError = VenueError;
