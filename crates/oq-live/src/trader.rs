//! From what a strategy wanted to what a venue was asked.
//!
//! A strategy returns intents naming orders by its own id, and the
//! venue knows orders by the client id this process chose. Something
//! has to hold those two together, and this is it — because the
//! alternative is that a strategy's cancel silently refers to nothing.
//!
//! # The mapping is the whole job
//!
//! A strategy says "cancel 7". Seven is a number it made up. The venue
//! has never heard of it and never will; what it has is
//! `live-3`, which this process invented at the moment order seven was
//! sent. Losing that association does not fail loudly — the cancel is
//! simply sent for an id the venue does not recognise, or is not sent
//! at all, and the order stays resting while the strategy believes it
//! is gone.
//!
//! So the association is stored when the order is accepted and dropped
//! when the venue says the order has ended, and a cancel for an id that
//! is not in it is reported rather than swallowed.

use std::collections::HashMap;

use oq_gateway::Execution;
use oq_risk::ProposedOrder;
use oq_strategy::{Context, Ending, Intent, Strategy};
use oq_types::{Nanos, Offset, OrderId, PriceTicks};

use crate::session::{Session, Submission};

/// What became of one intent.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// An order was sent. Carries the strategy's id and the venue's.
    Sent { local: OrderId, client_id: String },
    /// The gate or the venue refused it. The order does not exist.
    Refused { local: OrderId, why: String },
    /// Sent, and nobody knows whether it landed.
    ///
    /// Separate from `Refused` because they oblige opposite actions. A
    /// refusal is final and the order can be replaced; an unresolved
    /// submission may be resting right now, and replacing it is the one
    /// move that turns *maybe one order* into *certainly two*.
    ///
    /// It was folded into `Refused` until 2026-08-19, which meant
    /// `report_placements` — whose own comment says an unresolved
    /// placement must not be reported as a refusal — reported every one
    /// of them as exactly that. The comment described a variant that did
    /// not exist, so the guard it described could never fire.
    Unresolved {
        local: OrderId,
        /// The id the venue was given, so the question can be asked
        /// again. Without it, "answerable later" is not true of this
        /// value.
        client_id: String,
        why: String,
    },
    /// A cancel naming an order this process has no client id for.
    ///
    /// Reported rather than ignored: it means the strategy and this
    /// process disagree about what is resting, and a strategy acting on
    /// a belief that orders are gone when they are not will keep sizing
    /// against a position that is about to change.
    UnknownOrder(OrderId),
    /// A cancel that was sent.
    Cancelled { local: OrderId, client_id: String },
    /// A cancel the venue did not accept.
    ///
    /// The order is still resting. Reported as its own outcome because
    /// for a while it was reported as [`Outcome::Cancelled`] — the
    /// result of `cancel` was discarded and every withdrawal was
    /// announced as having happened, so a failed one left this process
    /// and the strategy both believing an order was gone while it went
    /// on filling.
    CancelFailed {
        local: OrderId,
        client_id: String,
        why: String,
    },
}

/// A strategy, a session, and the map between them.
pub struct Trader<S: Strategy, E: Execution> {
    strategy: S,
    session: Session<E>,
    /// Strategy id to the client id the venue knows.
    live: HashMap<u64, String>,
    intents: Vec<Intent>,
    /// Submissions the venue has not answered, and the id it was given.
    unanswered: Vec<(OrderId, String)>,
}

impl<S: Strategy, E: Execution> Trader<S, E> {
    /// The strategy, for asking it what it is waiting for.
    ///
    /// Read-only on purpose: the host observes the strategy, it does not
    /// reach in and change it.
    pub const fn strategy(&self) -> &S {
        &self.strategy
    }

    /// The strategy, for replaying history into it.
    ///
    /// Not a general escape hatch: the only caller is the warm-up, and
    /// the callback it reaches cannot produce intents.
    pub const fn strategy_mut(&mut self) -> &mut S {
        &mut self.strategy
    }

    #[must_use]
    pub fn new(strategy: S, session: Session<E>) -> Self {
        Self {
            strategy,
            session,
            live: HashMap::new(),
            intents: Vec::new(),
            unanswered: Vec::new(),
        }
    }

    #[must_use]
    pub const fn session(&self) -> &Session<E> {
        &self.session
    }

    #[must_use]
    pub fn session_mut(&mut self) -> &mut Session<E> {
        &mut self.session
    }

    /// Give the strategy a tick and act on what it returns.
    pub fn on_tick(&mut self, ctx: &Context, now: Nanos) -> Vec<Outcome> {
        self.intents.clear();
        self.strategy.on_tick(ctx, &mut self.intents);
        // Taken out of self so the session can be borrowed mutably
        // while they are read; the buffer itself is reused, so this
        // costs no allocation per tick.
        let intents = core::mem::take(&mut self.intents);
        let out: Vec<Outcome> = intents
            .iter()
            .flat_map(|i| self.act(i, ctx.tick.last, now))
            .collect();
        self.intents = intents;
        self.report_placements(&out);
        out
    }

    /// The venue, for the questions a run asks once it is over.
    ///
    /// Reading only. Everything that places or cancels goes through the
    /// session's own methods, so the gate cannot be walked around by
    /// holding this.
    #[must_use]
    pub const fn venue(&self) -> &E {
        self.session.venue()
    }

    /// The intents the strategy produced on the last call.
    ///
    /// Held so a caller can pair an [`Outcome::Sent`] back to what was
    /// actually asked for: the outcome carries two ids and nothing about
    /// side, size or price, and the books and the shadow both need those
    /// to record that an order exists.
    ///
    /// It is the whole list, refusals included. Which ones reached the
    /// venue is the outcomes' answer, not this one's — a method that
    /// filtered would be making that judgement twice, in two places, and
    /// they would eventually disagree.
    #[must_use]
    pub fn submitted(&self) -> &[Intent] {
        &self.intents
    }

    /// Tell the strategy which submissions the venue answered.
    ///
    /// Only the ones it answered. An unresolved placement is not reported
    /// as a refusal — nobody knows whether it landed, and telling a
    /// strategy `false` would be telling it the order does not exist,
    /// which is the mistake `Placed::Unknown` exists to prevent.
    fn report_placements(&mut self, outcomes: &[Outcome]) {
        for o in outcomes {
            match o {
                Outcome::Sent { local, .. } => self.strategy.on_placed(*local, true),
                Outcome::Refused { local, .. } => self.strategy.on_placed(*local, false),
                // A withdrawal that did not take. The order is still
                // there, and the strategy is the only thing that can
                // decide what to do about it.
                Outcome::CancelFailed { local, why, .. } => {
                    self.strategy.on_cancel_failed(*local, why);
                }
                // Not an answer yet, so the strategy is not told one.
                // Kept instead, and asked about again — the id was
                // chosen before sending precisely so that this question
                // stays answerable.
                Outcome::Unresolved {
                    local, client_id, ..
                } => self.unanswered.push((*local, client_id.clone())),
                // Unknown and cancelled: not an answer about whether
                // this submission is resting.
                Outcome::UnknownOrder(_) | Outcome::Cancelled { .. } => {}
            }
        }
    }

    /// Tell the strategy about a fill.
    pub fn on_fill(&mut self, fill: &oq_types::Fill, ctx: &Context, now: Nanos) -> Vec<Outcome> {
        self.intents.clear();
        self.strategy.on_fill(fill, ctx, &mut self.intents);
        let intents = core::mem::take(&mut self.intents);
        let out: Vec<Outcome> = intents
            .iter()
            .flat_map(|i| self.act(i, ctx.tick.last, now))
            .collect();
        self.intents = intents;
        self.report_placements(&out);
        out
    }

    /// The venue says an order has ended. Forget its association.
    pub fn forget(&mut self, client_id: &str) {
        self.live.retain(|_, v| v != client_id);
    }

    /// Ask the venue again about submissions that never got an answer.
    ///
    /// A submission whose outcome is unknown is asked about once, at the
    /// moment it happens, inside `Session::submit`. If that question
    /// also fails there is nothing further to try *then* — but there is
    /// something to try later, and until this existed nothing did. The
    /// count was reported at shutdown and the strategy was never told,
    /// so an order that did land went unmanaged for the life of the run
    /// and one that did not was never replaced.
    ///
    /// The reference drains a retry queue on its timer for the same
    /// reason. This is that queue, with the venue as the authority
    /// rather than a resend — resending is what turns "maybe one order"
    /// into "certainly two".
    ///
    /// Called on the heartbeat rather than per observation: it is a
    /// round trip to the venue, and the answer does not change between
    /// ticks.
    pub fn chase_unanswered(&mut self) -> Vec<(OrderId, bool)> {
        if self.unanswered.is_empty() {
            return Vec::new();
        }
        let symbol = self.session.symbol().to_string();
        let mut settled = Vec::new();
        let mut still_open = Vec::new();
        for (local, client_id) in core::mem::take(&mut self.unanswered) {
            match self.session.venue().order_status(&symbol, &client_id) {
                Ok(Some(_)) => settled.push((local, true)),
                Ok(None) => settled.push((local, false)),
                // Still no answer. Kept, not guessed at.
                Err(_) => still_open.push((local, client_id)),
            }
        }
        self.unanswered = still_open;
        for (local, resting) in &settled {
            self.strategy.on_placed(*local, *resting);
        }
        settled
    }

    /// Submissions still without an answer.
    #[must_use]
    pub fn unanswered(&self) -> usize {
        self.unanswered.len()
    }

    /// The venue says an order has ended: tell the strategy, then forget
    /// the association.
    ///
    /// In that order, and it has to be that order — the translation from
    /// the venue's client id to the strategy's own id lives in the map
    /// this call clears, so a strategy told after the forgetting would be
    /// told about `OrderId(0)`.
    ///
    /// An order this process did not place still gets forgotten and the
    /// strategy is not told, because it was never its order to end.
    pub fn on_ended(
        &mut self,
        client_id: &str,
        ending: Ending,
        ctx: &Context,
        now: Nanos,
    ) -> Vec<Outcome> {
        let Some(id) = self.local_id(client_id) else {
            self.forget(client_id);
            return Vec::new();
        };
        self.intents.clear();
        self.strategy.on_ended(id, ending, &mut self.intents);
        let intents = core::mem::take(&mut self.intents);
        let out: Vec<Outcome> = intents
            .iter()
            .flat_map(|i| self.act(i, ctx.tick.last, now))
            .collect();
        self.intents = intents;
        self.report_placements(&out);
        self.forget(client_id);
        out
    }

    /// The strategy's own id for an order the venue is talking about.
    ///
    /// A venue reports fills against the client id it was given, and a
    /// strategy recognises its orders by the id it issued. Without this
    /// translation a strategy cannot tell that its own entry filled —
    /// which is not theoretical: a live run opened two positions and
    /// managed neither, because every fill arrived carrying an id that
    /// matched nothing it had sent.
    #[must_use]
    pub fn local_id(&self, client_id: &str) -> Option<OrderId> {
        self.live
            .iter()
            .find(|(_, v)| v.as_str() == client_id)
            .map(|(k, _)| OrderId(*k))
    }

    /// Client ids this process believes are resting.
    #[must_use]
    pub fn resting(&self) -> Vec<&str> {
        self.live.values().map(String::as_str).collect()
    }

    fn act(&mut self, intent: &Intent, mark: PriceTicks, now: Nanos) -> Vec<Outcome> {
        match intent {
            Intent::Limit {
                id,
                side,
                price,
                qty,
                offset,
            } => vec![self.send(
                *id,
                ProposedOrder {
                    side: *side,
                    limit_price: Some(*price),
                    qty: *qty,
                    reduce_only: matches!(offset, Offset::Close),
                },
                mark,
                now,
            )],
            Intent::Market {
                id,
                side,
                qty,
                offset,
            } => vec![self.send(
                *id,
                ProposedOrder {
                    side: *side,
                    limit_price: None,
                    qty: *qty,
                    reduce_only: matches!(offset, Offset::Close),
                },
                mark,
                now,
            )],
            Intent::Cancel(id) => match self.live.get(&id.0) {
                Some(client_id) => vec![self.withdraw(*id, client_id.clone())],
                None => vec![Outcome::UnknownOrder(*id)],
            },
            Intent::CancelAll => {
                // One outcome per order rather than one for the request,
                // because a partial failure is the interesting case and
                // a single result hides it. The comment said this before
                // the code did: it returned the first order's outcome
                // and dropped the rest, so a sweep in which one cancel
                // failed read exactly like one in which none did.
                let all: Vec<(u64, String)> =
                    self.live.iter().map(|(k, v)| (*k, v.clone())).collect();
                all.into_iter()
                    .map(|(local, client_id)| self.withdraw(OrderId(local), client_id))
                    .collect()
            }
        }
    }

    /// Withdraw one order and say what actually happened to the request.
    fn withdraw(&mut self, local: OrderId, client_id: String) -> Outcome {
        match self.session.cancel(&client_id) {
            Submission::Sent(_) => Outcome::Cancelled { local, client_id },
            Submission::Refused(b) => Outcome::CancelFailed {
                local,
                client_id,
                why: format!("{b:?}"),
            },
            Submission::Rejected(why) => Outcome::CancelFailed {
                local,
                client_id,
                why,
            },
            // Unknown is not success. The order may or may not still be
            // resting, and the one thing that is certain is that nobody
            // may act as though it is gone.
            Submission::Unresolved { why, .. } => Outcome::CancelFailed {
                local,
                client_id,
                why,
            },
        }
    }

    fn send(
        &mut self,
        local: OrderId,
        order: ProposedOrder,
        mark: PriceTicks,
        now: Nanos,
    ) -> Outcome {
        match self.session.submit(order, mark, now) {
            Submission::Sent(client_id) => {
                self.live.insert(local.0, client_id.clone());
                Outcome::Sent { local, client_id }
            }
            Submission::Refused(b) => Outcome::Refused {
                local,
                why: format!("{b:?}"),
            },
            Submission::Rejected(why) => Outcome::Refused { local, why },
            Submission::Unresolved { client_id, why } => Outcome::Unresolved {
                local,
                client_id,
                why,
            },
        }
    }
}
