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
use oq_strategy::{Context, Intent, Strategy};
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
    Unresolved { local: OrderId, why: String },
    /// A cancel naming an order this process has no client id for.
    ///
    /// Reported rather than ignored: it means the strategy and this
    /// process disagree about what is resting, and a strategy acting on
    /// a belief that orders are gone when they are not will keep sizing
    /// against a position that is about to change.
    UnknownOrder(OrderId),
    /// A cancel that was sent.
    Cancelled { local: OrderId, client_id: String },
}

/// A strategy, a session, and the map between them.
pub struct Trader<S: Strategy, E: Execution> {
    strategy: S,
    session: Session<E>,
    /// Strategy id to the client id the venue knows.
    live: HashMap<u64, String>,
    intents: Vec<Intent>,
}

impl<S: Strategy, E: Execution> Trader<S, E> {
    /// The strategy, for asking it what it is waiting for.
    ///
    /// Read-only on purpose: the host observes the strategy, it does not
    /// reach in and change it.
    pub const fn strategy(&self) -> &S {
        &self.strategy
    }

    #[must_use]
    pub fn new(strategy: S, session: Session<E>) -> Self {
        Self {
            strategy,
            session,
            live: HashMap::new(),
            intents: Vec::new(),
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
            .map(|i| self.act(i, ctx.tick.last, now))
            .collect();
        self.intents = intents;
        self.report_placements(&out);
        out
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
                // Unresolved, unknown, cancelled: not an answer about
                // whether this submission is resting.
                Outcome::Unresolved { .. }
                | Outcome::UnknownOrder(_)
                | Outcome::Cancelled { .. } => {}
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
            .map(|i| self.act(i, ctx.tick.last, now))
            .collect();
        self.intents = intents;
        self.report_placements(&out);
        out
    }

    /// The venue says an order has ended. Forget its association.
    pub fn forget(&mut self, client_id: &str) {
        self.live.retain(|_, v| v != client_id);
    }

    /// Client ids this process believes are resting.
    #[must_use]
    pub fn resting(&self) -> Vec<&str> {
        self.live.values().map(String::as_str).collect()
    }

    fn act(&mut self, intent: &Intent, mark: PriceTicks, now: Nanos) -> Outcome {
        match intent {
            Intent::Limit {
                id,
                side,
                price,
                qty,
                offset,
            } => self.send(
                *id,
                ProposedOrder {
                    side: *side,
                    limit_price: Some(*price),
                    qty: *qty,
                    reduce_only: matches!(offset, Offset::Close),
                },
                mark,
                now,
            ),
            Intent::Market {
                id,
                side,
                qty,
                offset,
            } => self.send(
                *id,
                ProposedOrder {
                    side: *side,
                    limit_price: None,
                    qty: *qty,
                    reduce_only: matches!(offset, Offset::Close),
                },
                mark,
                now,
            ),
            Intent::Cancel(id) => match self.live.get(&id.0) {
                Some(client_id) => {
                    let client_id = client_id.clone();
                    self.session.cancel(&client_id);
                    Outcome::Cancelled {
                        local: *id,
                        client_id,
                    }
                }
                None => Outcome::UnknownOrder(*id),
            },
            Intent::CancelAll => {
                // Reported as one outcome per order rather than one for
                // the request, because a partial failure here is the
                // interesting case and a single result would hide it.
                let all: Vec<(u64, String)> =
                    self.live.iter().map(|(k, v)| (*k, v.clone())).collect();
                for (_, client_id) in &all {
                    self.session.cancel(client_id);
                }
                all.first()
                    .map_or(Outcome::UnknownOrder(OrderId(0)), |(k, v)| {
                        Outcome::Cancelled {
                            local: OrderId(*k),
                            client_id: v.clone(),
                        }
                    })
            }
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
            Submission::Unresolved(why) => Outcome::Unresolved { local, why },
        }
    }
}
