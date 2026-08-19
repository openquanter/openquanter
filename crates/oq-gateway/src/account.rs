//! The account side of a venue, as one thing a runner can hold.
//!
//! [`Execution`] says how to send an order. It is not enough to run a
//! strategy: before the first order there is a clock to agree on, an
//! instrument whose precision and grid come from the deployment, a
//! position mode to discover, a balance and a position and a book of
//! resting orders to read, and a stream to hear the answers on.
//!
//! Those were reached for directly on a concrete `Binance`, in eight
//! places, so the runner knew which venue it was running against even
//! though every one of those questions has a venue-independent answer.
//! An adapter could implement `Execution` and still be unable to run —
//! which is what happened: an OKX adapter existed and could not be used,
//! and nothing said why, because the missing part had no name.
//!
//! It has one now. A venue that implements [`Account`] can be handed to
//! the runner; one that cannot is told by the compiler exactly what it
//! still owes.
//!
//! # Why a trait object
//!
//! Which venue to connect to is read from configuration, so it is a
//! runtime question and `Box<dyn Account>` is the honest shape for it.
//! The graph the runner builds is still fixed at compile time; only the
//! identity of the venue at one edge of it is not. That distinction is
//! design decision D17, and this is the edge it was talking about.
//!
//! # What is deliberately not here
//!
//! Market data. It already has its own seam — `oq_l2feed::Venue` — and
//! is already reached through a trait object. Folding the two together
//! would couple a capture tool to an account credential, and the capture
//! side has good reasons to run with neither.

use oq_types::Instrument;

use crate::binance::{AccountSnapshot, OpenOrder, PositionSnapshot, VenueError};
use crate::broker::IdRules;
use crate::exec::{Execution, NewOrder, OrderAck, Placed, UserStream};

/// Everything a live run needs from the account side of one venue.
///
/// `Execution` is a supertrait rather than a member because sending an
/// order is not a different capability from reading the account: they
/// are the same authority, and splitting them would suggest a venue
/// could offer one without the other.
pub trait Account: Execution {
    /// The venue's identity, e.g. `binance-perp`.
    ///
    /// Not cosmetic: it names the deployment in a run's records and in
    /// the interlock that stops two processes from sharing one account,
    /// so a label that disagrees with the implementation would let two
    /// runs believe they are on different venues.
    fn id(&self) -> &'static str;

    /// What this venue will accept as a client order id.
    ///
    /// Venues disagree — one allows 36 characters with punctuation,
    /// another 32 alphanumeric — and an id built to the wrong rules is
    /// rejected at submission, which is the worst place to find out:
    /// the strategy has already decided to trade.
    fn id_rules(&self) -> IdRules;

    /// Agree with the venue about the time, and report the offset.
    ///
    /// # Errors
    /// Whatever the request reports.
    fn sync_clock(&mut self) -> Result<i64, VenueError>;

    /// Round trip of the sample the offset came from, in milliseconds.
    ///
    /// Separate from the offset because a slow link and a wrong clock
    /// are different faults, and at least one venue answers the first
    /// with an error message naming the second.
    fn round_trip_ms(&self) -> i64;

    /// Precision, grid and minimum size, as this deployment publishes
    /// them right now.
    ///
    /// Not from a table compiled in: deployments of the same venue
    /// disagree — one publishes three decimal places of quantity where
    /// another publishes four — and a size expressed in lots means a
    /// different amount on each.
    ///
    /// # Errors
    /// A description of what could not be read or believed.
    fn instrument(&self, symbol: &str) -> Result<Instrument, String>;

    /// Whether the account keeps long and short as separate positions.
    ///
    /// # Errors
    /// Whatever the request reports.
    fn is_hedged(&self) -> Result<bool, VenueError>;

    /// Positions the venue holds for a symbol.
    ///
    /// # Errors
    /// Whatever the request reports.
    fn positions(&self, symbol: &str) -> Result<Vec<PositionSnapshot>, VenueError>;

    /// Wallet balance and unrealised profit, from the venue.
    ///
    /// # Errors
    /// Whatever the request reports.
    fn balances(&self) -> Result<AccountSnapshot, VenueError>;

    /// Orders resting on the venue for a symbol.
    ///
    /// # Errors
    /// Whatever the request reports.
    fn open_orders(&self, symbol: &str) -> Result<Vec<OpenOrder>, VenueError>;

    /// Open the stream the account's own events arrive on.
    ///
    /// # Errors
    /// Whatever the request reports.
    fn open_user_stream(&self) -> Result<UserStream, VenueError>;

    /// Tell the venue the stream is still wanted.
    ///
    /// Part of the trait rather than the caller's business because a
    /// venue that drops an unrenewed stream and one that keeps it open
    /// forever both look identical until the moment fills stop arriving.
    ///
    /// # Errors
    /// Whatever the request reports.
    fn keepalive_user_stream(&self) -> Result<(), VenueError>;

    /// Give the stream back at the end of a run.
    ///
    /// # Errors
    /// Whatever the request reports.
    fn close_user_stream(&self) -> Result<(), VenueError>;
}

/// So a boxed account can be used where an `Execution` is wanted.
///
/// Without this the runner would have to unbox at every call site, and
/// `Session` and `Trader` — both generic over `E: Execution` — could not
/// be given one at all.
impl Execution for Box<dyn Account> {
    fn place(&self, order: &NewOrder, instrument: &Instrument) -> Placed {
        (**self).place(order, instrument)
    }

    fn cancel(&self, symbol: &str, client_id: &str) -> Placed {
        (**self).cancel(symbol, client_id)
    }

    fn order_status(&self, symbol: &str, client_id: &str) -> Result<Option<OrderAck>, VenueError> {
        (**self).order_status(symbol, client_id)
    }
}

impl Account for Box<dyn Account> {
    fn id(&self) -> &'static str {
        (**self).id()
    }
    fn id_rules(&self) -> IdRules {
        (**self).id_rules()
    }
    fn sync_clock(&mut self) -> Result<i64, VenueError> {
        (**self).sync_clock()
    }
    fn round_trip_ms(&self) -> i64 {
        (**self).round_trip_ms()
    }
    fn instrument(&self, symbol: &str) -> Result<Instrument, String> {
        (**self).instrument(symbol)
    }
    fn is_hedged(&self) -> Result<bool, VenueError> {
        (**self).is_hedged()
    }
    fn positions(&self, symbol: &str) -> Result<Vec<PositionSnapshot>, VenueError> {
        (**self).positions(symbol)
    }
    fn balances(&self) -> Result<AccountSnapshot, VenueError> {
        (**self).balances()
    }
    fn open_orders(&self, symbol: &str) -> Result<Vec<OpenOrder>, VenueError> {
        (**self).open_orders(symbol)
    }
    fn open_user_stream(&self) -> Result<UserStream, VenueError> {
        (**self).open_user_stream()
    }
    fn keepalive_user_stream(&self) -> Result<(), VenueError> {
        (**self).keepalive_user_stream()
    }
    fn close_user_stream(&self) -> Result<(), VenueError> {
        (**self).close_user_stream()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A venue that is not Binance, implemented against nothing but the
    /// trait.
    ///
    /// It exists to make one claim checkable at compile time: that
    /// [`Account`] contains no Binance in it. If someone adds a method
    /// whose only sensible implementation is Binance's — a listen-key,
    /// a `positionSide` string, a weight budget — this stops compiling,
    /// and it stops compiling here rather than six months later in
    /// somebody's half-finished adapter.
    ///
    /// The predecessor had exactly that adapter: an OKX client that
    /// implemented the send-an-order half and could not be run, with
    /// nothing to say what it still owed. The list is now this trait,
    /// and this type is the proof the list is complete.
    struct Nowhere;

    impl Execution for Nowhere {
        fn place(&self, _: &NewOrder, _: &Instrument) -> Placed {
            unimplemented!("nothing is sent from a test")
        }
        fn cancel(&self, _: &str, _: &str) -> Placed {
            unimplemented!("nothing is sent from a test")
        }
        fn order_status(&self, _: &str, _: &str) -> Result<Option<OrderAck>, VenueError> {
            Ok(None)
        }
    }

    impl Account for Nowhere {
        fn id(&self) -> &'static str {
            "nowhere-perp"
        }
        // Deliberately the stricter of the two rule sets in `broker`, so
        // that a runner which built ids to Binance's 36-character rule
        // would produce ids this venue rejects — which is what the
        // hardcoded `IdRules::BINANCE` in the runner used to do.
        fn id_rules(&self) -> IdRules {
            IdRules::OKX
        }
        fn sync_clock(&mut self) -> Result<i64, VenueError> {
            Ok(0)
        }
        fn round_trip_ms(&self) -> i64 {
            0
        }
        fn instrument(&self, _: &str) -> Result<Instrument, String> {
            Ok(Instrument::linear(2, 4))
        }
        fn is_hedged(&self) -> Result<bool, VenueError> {
            Ok(false)
        }
        fn positions(&self, _: &str) -> Result<Vec<PositionSnapshot>, VenueError> {
            Ok(Vec::new())
        }
        fn balances(&self) -> Result<AccountSnapshot, VenueError> {
            Ok(AccountSnapshot {
                wallet_balance: 0.0,
                unrealized: 0.0,
                margin_balance: 0.0,
                read_at_ms: 0,
            })
        }
        fn open_orders(&self, _: &str) -> Result<Vec<OpenOrder>, VenueError> {
            Ok(Vec::new())
        }
        fn open_user_stream(&self) -> Result<UserStream, VenueError> {
            unimplemented!("no stream is opened from a test")
        }
        fn keepalive_user_stream(&self) -> Result<(), VenueError> {
            Ok(())
        }
        fn close_user_stream(&self) -> Result<(), VenueError> {
            Ok(())
        }
    }

    /// The trait is object-safe, which is the whole reason it can be
    /// chosen at runtime. A method taking `self` by value or returning
    /// `Self` would break this line and nothing else.
    #[test]
    fn an_account_can_be_boxed() {
        let venue: Box<dyn Account> = Box::new(Nowhere);
        assert_eq!(venue.id(), "nowhere-perp");
    }

    /// Both delegating impls forward rather than answer for themselves.
    ///
    /// Worth a test because a `Box<dyn Account>` that reported its own
    /// id rules instead of the venue's would build client ids to the
    /// wrong length, and the venue would refuse them at submission —
    /// after the strategy had already decided to trade.
    #[test]
    fn a_boxed_account_answers_as_the_venue_it_holds() {
        let venue: Box<dyn Account> = Box::new(Nowhere);
        assert_eq!(venue.id_rules(), IdRules::OKX);
        assert_eq!(
            venue.instrument("ANY").expect("resolves").qty_scale,
            4,
            "the instrument comes from the venue, not from a table"
        );
        assert!(!venue.is_hedged().expect("answers"));
    }
}
