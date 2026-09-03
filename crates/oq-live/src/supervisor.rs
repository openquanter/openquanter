//! When to renew, when to check, when to reconnect, when to stop.
//!
//! Pure. It is handed what happened and the time it happened, and it
//! returns what should be done about it. Nothing here opens a socket or
//! reads a clock, so the whole supervision policy is testable without a
//! venue and reproducible from a recording — which is what makes an
//! incident reconstructable rather than merely regrettable.
//!
//! # Reconnecting is never the whole answer
//!
//! Every path that loses the stream returns [`Action::Reconnect`]
//! *followed by* [`Action::Reconcile`], and the order is deliberate.
//! Whatever the venue said while the stream was down was said to
//! nobody, so the beliefs built from that stream are stale in a way no
//! reconnection repairs. A supervisor that reconnected and carried on
//! would be one that recovered its connection and kept its wrong
//! numbers.

use core::time::Duration;

use oq_gateway::{Health, StreamHealth, UserEvent};
use oq_types::Nanos;

/// What the process should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Renew the stream key before it lapses.
    RenewKey,
    /// Fetch the venue's own positions and compare.
    CheckPositions,
    /// Replace the beliefs with the venue's own.
    Reconcile,
    /// Drop the stream and open a new one.
    Reconnect,
    /// Stop trading. Carries what convinced the supervisor.
    Halt(&'static str),
}

/// The schedule the supervisor keeps.
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    /// How often to renew the stream key.
    pub renew_every: Duration,
    /// How often to compare positions against the venue.
    pub check_every: Duration,
}

impl Default for Timings {
    /// Renewal at a third of the key's life, so a failed renewal has
    /// two more chances before anything lapses; comparison every
    /// minute, which is frequent enough to catch a dead stream long
    /// before a strategy has traded much against stale beliefs.
    fn default() -> Self {
        Self {
            renew_every: Duration::from_secs(20 * 60),
            check_every: Duration::from_secs(60),
        }
    }
}

/// Decides what the session should do.
#[derive(Debug)]
pub struct Supervisor {
    timings: Timings,
    health: StreamHealth,
    last_renew: Option<Nanos>,
    last_check: Option<Nanos>,
    /// Placements whose outcome was never established.
    unresolved: u32,
    /// How many may accumulate before the process stops.
    unresolved_limit: u32,
    /// Consecutive attempts to read the venue's own view that failed.
    unreadable: u32,
    /// How many may fail in a row before the link is treated as the
    /// cause and replaced.
    unreadable_limit: u32,
}

impl Supervisor {
    #[must_use]
    pub fn new(timings: Timings) -> Self {
        Self {
            timings,
            health: StreamHealth::futures(),
            last_renew: None,
            last_check: None,
            unresolved: 0,
            unresolved_limit: 3,
            unreadable: 0,
            // Five checks at the default minute apart. Long enough that
            // a bad minute on the link is not a reconnection, short
            // enough that blindness is measured in minutes rather than
            // in the hours it took to notice the last one.
            unreadable_limit: 5,
        }
    }

    /// Work that is due because time has passed.
    ///
    /// Called whether or not anything arrived, because the two things
    /// that must happen on a quiet stream — renewing the key and
    /// checking that the quiet is real — are exactly the things a
    /// message-driven loop would never do.
    pub fn due(&mut self, now: Nanos) -> Vec<Action> {
        let mut out = Vec::new();
        if elapsed(self.last_renew, now, self.timings.renew_every) {
            self.last_renew = Some(now);
            out.push(Action::RenewKey);
        }
        if elapsed(self.last_check, now, self.timings.check_every) {
            self.last_check = Some(now);
            out.push(Action::CheckPositions);
        }
        out
    }

    /// Something arrived on the stream.
    pub fn on_event(&mut self, event: &UserEvent) -> Vec<Action> {
        match event {
            // The key lapsed, so the stream is closed and the interval
            // in which it lapsed was unobserved. Both halves matter.
            UserEvent::Expired => vec![Action::Reconnect, Action::Reconcile],
            UserEvent::Order(_) | UserEvent::Other { .. } => Vec::new(),
        }
    }

    /// The stream went away.
    pub fn on_disconnect(&mut self) -> Vec<Action> {
        vec![Action::Reconnect, Action::Reconcile]
    }

    /// The venue's own view could not be read at all.
    ///
    /// Neither agreement nor disagreement: **nothing was compared**. The
    /// distinction is the whole point, because the zombie check below is
    /// the only thing that can see a stream which has quietly stopped
    /// delivering, and a check that could not run is not a check that
    /// passed. `oq-recon` states the same rule for its own exit codes —
    /// 3 is not 0 — and this is that rule inside the loop.
    ///
    /// One failure is a lost packet. Enough in a row and the process has
    /// been blind for as long as they took, so the link itself is
    /// treated as the suspect and replaced.
    pub fn on_read_failed(&mut self) -> Vec<Action> {
        self.unreadable = self.unreadable.saturating_add(1);
        if self.unreadable >= self.unreadable_limit {
            self.unreadable = 0;
            // Reconnect before reconciling, and for once not because the
            // stream is known bad: the commonest reason the account
            // cannot be read is the same link trouble that strands the
            // stream, and a reader that came back is worth more than one
            // more read that will not.
            vec![Action::Reconnect, Action::Reconcile]
        } else {
            Vec::new()
        }
    }

    /// The venue's view was read, whatever it then turned out to say.
    pub fn on_read_succeeded(&mut self) {
        self.unreadable = 0;
    }

    /// How many consecutive reads of the account have failed.
    #[must_use]
    pub const fn unreadable(&self) -> u32 {
        self.unreadable
    }

    /// The result of comparing the two views of the account.
    pub fn on_positions(
        &mut self,
        streamed: &[oq_gateway::PositionSnapshot],
        venue: &[oq_gateway::PositionSnapshot],
    ) -> Vec<Action> {
        match self.health.observe(streamed, venue) {
            Health::Agreed => Vec::new(),
            // Not yet evidence. A fill in flight is visible to one side
            // before the other, and acting on the first difference
            // would reconnect constantly under load.
            Health::Disagreed { .. } => Vec::new(),
            Health::Zombie { .. } => {
                self.health.reset();
                vec![Action::Reconnect, Action::Reconcile]
            }
        }
    }

    /// A placement whose outcome could not be established, even after
    /// asking the venue about it.
    ///
    /// One is survivable and is resolved by querying. Several in a row
    /// mean the process no longer knows what it has working, and a
    /// process in that state must not keep sending orders — every new
    /// one is sized against a picture it cannot verify.
    pub fn on_unresolved(&mut self) -> Vec<Action> {
        self.unresolved = self.unresolved.saturating_add(1);
        if self.unresolved >= self.unresolved_limit {
            vec![Action::Halt(
                "too many placements with an unknown outcome; the process no longer \
                 knows what it has working",
            )]
        } else {
            vec![Action::Reconcile]
        }
    }

    /// An outcome was established after all.
    pub fn on_resolved(&mut self) {
        self.unresolved = 0;
    }

    #[must_use]
    pub const fn unresolved(&self) -> u32 {
        self.unresolved
    }
}

/// Whether `period` has passed since `last`, treating "never" as due.
fn elapsed(last: Option<Nanos>, now: Nanos, period: Duration) -> bool {
    let Ok(period) = i64::try_from(period.as_nanos()) else {
        return false;
    };
    match last {
        None => true,
        Some(t) => now.0.saturating_sub(t.0) >= period,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_gateway::PositionSnapshot;

    const SEC: i64 = 1_000_000_000;

    fn pos(amount: f64) -> Vec<PositionSnapshot> {
        vec![PositionSnapshot {
            symbol: "BTCUSDT".into(),
            position_side: "BOTH".into(),
            amount_text: String::new(),
            entry_text: String::new(),
            amount,
            entry_price: 0.0,
            unrealized: 0.0,
        }]
    }

    #[test]
    fn both_kinds_of_upkeep_are_due_immediately_at_startup() {
        // "Never renewed" must count as due, or the first renewal waits
        // a full period after a key that is already ticking.
        let mut s = Supervisor::new(Timings::default());
        let due = s.due(Nanos(0));
        assert!(due.contains(&Action::RenewKey));
        assert!(due.contains(&Action::CheckPositions));
    }

    #[test]
    fn upkeep_is_not_repeated_before_its_period() {
        let mut s = Supervisor::new(Timings::default());
        s.due(Nanos(0));
        assert!(
            s.due(Nanos(SEC)).is_empty(),
            "one second later, nothing is due"
        );
        assert_eq!(
            s.due(Nanos(60 * SEC)),
            vec![Action::CheckPositions],
            "the check comes round long before the renewal"
        );
    }

    #[test]
    fn a_disconnect_reconciles_as_well_as_reconnects() {
        // The order matters and so does the second half: reconnecting
        // alone recovers the connection and keeps the wrong numbers.
        let mut s = Supervisor::new(Timings::default());
        assert_eq!(
            s.on_disconnect(),
            vec![Action::Reconnect, Action::Reconcile]
        );
    }

    #[test]
    fn an_expired_key_is_treated_as_a_gap_not_as_a_quiet_period() {
        let mut s = Supervisor::new(Timings::default());
        assert_eq!(
            s.on_event(&UserEvent::Expired),
            vec![Action::Reconnect, Action::Reconcile]
        );
    }

    #[test]
    fn an_ordinary_fill_asks_for_nothing() {
        let mut s = Supervisor::new(Timings::default());
        let e = UserEvent::Other {
            kind: "ACCOUNT_UPDATE".into(),
            payload: "{}".into(),
        };
        assert!(s.on_event(&e).is_empty());
    }

    #[test]
    fn a_persistently_disagreeing_stream_is_replaced_but_a_blip_is_not() {
        let mut s = Supervisor::new(Timings::default());
        let streamed = pos(1.0);
        let venue = pos(2.0);
        assert!(s.on_positions(&streamed, &venue).is_empty(), "first");
        assert!(s.on_positions(&streamed, &venue).is_empty(), "second");
        assert_eq!(
            s.on_positions(&streamed, &venue),
            vec![Action::Reconnect, Action::Reconcile],
            "third disagreement condemns the stream"
        );
    }

    #[test]
    fn agreement_after_a_blip_clears_it() {
        let mut s = Supervisor::new(Timings::default());
        assert!(s.on_positions(&pos(1.0), &pos(2.0)).is_empty());
        assert!(s.on_positions(&pos(1.0), &pos(1.0)).is_empty());
        assert!(s.on_positions(&pos(1.0), &pos(2.0)).is_empty());
        assert!(
            s.on_positions(&pos(1.0), &pos(2.0)).is_empty(),
            "the count restarted, so two is still not three"
        );
    }

    #[test]
    fn one_unknown_placement_reconciles_and_three_stop_the_process() {
        // A process that cannot establish what it has working must not
        // keep sending orders: each new one is sized against a picture
        // it cannot verify.
        let mut s = Supervisor::new(Timings::default());
        assert_eq!(s.on_unresolved(), vec![Action::Reconcile]);
        assert_eq!(s.on_unresolved(), vec![Action::Reconcile]);
        assert!(matches!(s.on_unresolved().first(), Some(Action::Halt(_))));
    }

    #[test]
    fn resolving_an_outcome_clears_the_count() {
        let mut s = Supervisor::new(Timings::default());
        s.on_unresolved();
        s.on_unresolved();
        s.on_resolved();
        assert_eq!(s.unresolved(), 0);
        assert_eq!(s.on_unresolved(), vec![Action::Reconcile]);
    }

    #[test]
    fn reads_that_keep_failing_replace_the_link_rather_than_passing() {
        // Not checking is not the same as passing. Four failures are a
        // bad few minutes on the link; the fifth means this process has
        // had no second opinion for five whole checks, which is the
        // state the zombie check exists to prevent.
        let mut s = Supervisor::new(Timings::default());
        for i in 1..5 {
            assert!(s.on_read_failed().is_empty(), "failure {i} is not evidence");
        }
        assert_eq!(
            s.on_read_failed(),
            vec![Action::Reconnect, Action::Reconcile],
            "the fifth consecutive unreadable account replaces the link"
        );
    }

    #[test]
    fn a_read_that_succeeds_clears_the_failures() {
        // The count is of *consecutive* failures. A run that lost one
        // read an hour for a day has a working link and must not be
        // reconnected for it.
        let mut s = Supervisor::new(Timings::default());
        s.on_read_failed();
        s.on_read_failed();
        s.on_read_succeeded();
        assert_eq!(s.unreadable(), 0);
        assert!(s.on_read_failed().is_empty());
    }
}
