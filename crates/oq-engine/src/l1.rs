//! L1: queue position, entry and response latency, and taker impact.
//!
//! `FR-MATCH-5`. Three things L0 does not model, each of which makes a
//! backtest optimistic in a different way:
//!
//! - **Queue position.** L0 fills a resting order the moment the price
//!   reaches it. In a real book the order joins a queue and fills only
//!   after the volume ahead of it has traded, which for a maker strategy
//!   is most of the difference between the backtest and the account.
//! - **Latency.** L0 matches an order against the observation that
//!   triggered it. A real order arrives later, into a market that has
//!   moved, and its fill is learned about later still.
//! - **Impact.** L0 fills any size at the displayed price. A real taker
//!   order of meaningful size walks the book.
//!
//! # L1 wraps L0 rather than replacing it
//!
//! `FR-MATCH-2` freezes L0 as the migration and regression anchor, and
//! the cheapest way to keep a promise like that is to make breaking it
//! impossible rather than to test for it. [`L1Engine`] owns an
//! [`L0Engine`] and does not modify it: orders are held outside it until
//! they are entitled to be in it, and its fills are adjusted after it
//! produces them. L0's behaviour with an empty policy is L0's behaviour,
//! byte for byte, and a test asserts that against the same inputs.
//!
//! # Every parameter here is an assumption, and is named as one
//!
//! The tick format carries a price path and a cumulative volume. It does
//! not carry book depth, and it does not carry the latency this
//! deployment actually experienced. So L1 cannot *measure* queue-ahead
//! or latency; it applies a policy, and the policy is the user's claim
//! about their market rather than the engine's knowledge of it.
//!
//! That is not a weakness to apologise for — it is the honest shape of
//! the problem, and the alternative is an engine that invents the
//! numbers and reports them as findings. What matters is that the
//! assumptions are visible: [`Policy`] has no `Default` that trades,
//! [`Policy::describe`] renders them for a fidelity report, and a policy
//! that models nothing says so rather than looking like a measurement.
//!
//! # What L1 does not model, and why it is not here
//!
//! **Feed latency** — the delay between the venue's event and the
//! strategy seeing it — is a property of the event producer, not the
//! matcher. The host decides which observation a strategy is shown;
//! putting it here as well would delay the same event twice, and the
//! second delay would be invisible. `FR-MATCH-5` names three segments
//! and this models the two that are properties of order handling, which
//! is stated rather than quietly counted as three.

use oq_types::{Fill, Nanos, OrderId, PriceTicks, QtyLots, Side, Working};

use crate::l0::{L0Engine, L0Fill};
use crate::tick::Tick;

/// How much volume is assumed to be queued ahead of a resting order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAhead {
    /// None: an order fills as soon as trading reaches its price.
    ///
    /// This is L0's assumption, available here so a run can isolate the
    /// effect of latency and impact without also changing queue
    /// behaviour.
    None,
    /// A fixed number of lots, whatever the market is doing.
    ///
    /// Crude and predictable. Useful when the depth at the touch is
    /// roughly known and roughly constant.
    Fixed(QtyLots),
    /// A multiple of the volume that traded in the observation the
    /// order arrived in.
    ///
    /// Scales with activity, which is closer to how a real book behaves:
    /// the queue at the touch is deeper when more is trading. The
    /// multiple is in hundredths, so 150 means one and a half times the
    /// arrival observation's volume.
    VolumeMultiple(u32),
}

/// One latency segment: a constant, or the shape a caller measured.
///
/// # Why a distribution and not a mean
///
/// A resting order's fate is decided by its tail, not its middle. A
/// constant models the median and then claims, silently, that the slow
/// tenth of orders behaved the same way — which is the tenth that
/// misses the queue, arrives after the price moved, and turns a maker
/// fill into no fill at all. Reporting one number for a segment whose
/// dispersion is the whole story is the same error as reporting a mean
/// drawdown.
///
/// # Why four quantiles and not a fitted distribution
///
/// Because four quantiles are what anybody actually has. A latency
/// measurement produces a p50, a p90, a p99 and a p999; nobody measures
/// a lognormal's σ. Asking for a fitted shape would make every caller
/// convert what they have into what the type wants, and a conversion
/// with a choice in it is a number this crate would then be reporting
/// as a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delay {
    /// Every order waits the same.
    Fixed(Nanos),
    /// Drawn from four measured quantiles, nearest-rank.
    ///
    /// Half the orders get `p50`, four in ten get `p90`, nine in a
    /// hundred get `p99`, one in a hundred gets `p999`. Nearest-rank
    /// and never interpolated, which is this workspace's convention
    /// everywhere a quantile appears: an interpolated quantile is a
    /// value nothing was observed at.
    ///
    /// The consequence is worth stating rather than discovering: the
    /// middle is **overstated**, because forty percent of orders are
    /// given the ninetieth-percentile delay. That makes it pessimistic,
    /// which is the direction L1 is deliberately wrong in — a
    /// conservative queue model and an optimistic latency model would
    /// cancel, and the cancelling is what makes a fidelity tier stop
    /// meaning anything.
    Measured {
        p50: Nanos,
        p90: Nanos,
        p99: Nanos,
        p999: Nanos,
    },
}

impl Delay {
    /// The delay this order waits.
    ///
    /// `key` decides the draw and nothing else does. Same key, same
    /// answer, on every machine and every replay — a matcher that read
    /// a clock or a thread-local generator would produce a run that
    /// cannot be reproduced, and reproducibility is the anchor
    /// everything else in this workspace is checked against.
    #[must_use]
    pub const fn at(&self, key: u64) -> Nanos {
        match self {
            Self::Fixed(n) => *n,
            Self::Measured {
                p50,
                p90,
                p99,
                p999,
            } => {
                // Thousandths, so the bands below are exact rather than
                // a rounding of a float.
                let u = mix(key) % 1000;
                if u < 500 {
                    *p50
                } else if u < 900 {
                    *p90
                } else if u < 990 {
                    *p99
                } else {
                    *p999
                }
            }
        }
    }

    /// One segment, for the assumptions line of a fidelity report.
    ///
    /// A distribution prints all four points. Collapsing it to a median
    /// would put the same sentence on a run that modelled dispersion
    /// and a run that did not, which is the difference the caller
    /// chose to make.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Fixed(n) => format!("{} ns fixed", n.0),
            Self::Measured {
                p50,
                p90,
                p99,
                p999,
            } => format!(
                "p50 {} / p90 {} / p99 {} / p999 {} ns",
                p50.0, p90.0, p99.0, p999.0
            ),
        }
    }

    /// Whether this segment delays anything.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        match self {
            Self::Fixed(n) => n.0 == 0,
            Self::Measured {
                p50,
                p90,
                p99,
                p999,
            } => p50.0 == 0 && p90.0 == 0 && p99.0 == 0 && p999.0 == 0,
        }
    }

    /// The longest this segment can be, for a report.
    #[must_use]
    pub const fn worst(&self) -> Nanos {
        match self {
            Self::Fixed(n) => *n,
            Self::Measured { p999, .. } => *p999,
        }
    }
}

impl Default for Delay {
    fn default() -> Self {
        Self::Fixed(Nanos(0))
    }
}

/// SplitMix64, so a draw needs no state and no dependency.
///
/// Counter-based rather than a generator carried in the matcher: a
/// stateful stream makes the delay of one order depend on how many
/// orders came before it, so replaying from a snapshot — which starts
/// mid-sequence — would produce different fills from the same events.
const fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut x = z;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The three-segment latency L1 enforces.
///
/// Feed latency is deliberately absent; see the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Latency {
    /// From the strategy's decision to the order being live at the
    /// venue.
    pub entry: Delay,
    /// From the fill happening to the strategy being told.
    pub response: Delay,
}

/// How much a taker order moves the price against itself.
///
/// The square-root law: cost scales with the square root of the order's
/// share of volume. The coefficient is not derivable from tick data and
/// has to be supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Impact {
    /// Coefficient, in hundredths. Zero models no impact.
    ///
    /// The penalty is `coefficient/100 * range * sqrt(qty / volume)`,
    /// where `range` is the observation's high-low. A coefficient of 100
    /// means an order taking the whole observation's volume pays its
    /// entire range.
    pub coefficient: u32,
}

/// Everything L1 assumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Volume assumed ahead of a resting order.
    pub queue: QueueAhead,
    /// Delays applied to entry and to fill reporting.
    pub latency: Latency,
    /// Taker impact.
    pub impact: Impact,
}

impl Policy {
    /// A policy that models nothing, so L1 reproduces L0 exactly.
    ///
    /// Not a `Default` implementation. A default policy would be one a
    /// caller could get without choosing it, and every number L1 produces
    /// is a consequence of the choice — a run at L1 with assumptions
    /// nobody made is a run reporting L0's answer under L1's name.
    pub const TRANSPARENT: Self = Self {
        queue: QueueAhead::None,
        latency: Latency {
            entry: Delay::Fixed(Nanos(0)),
            response: Delay::Fixed(Nanos(0)),
        },
        impact: Impact { coefficient: 0 },
    };

    /// Whether this policy changes anything at all.
    #[must_use]
    pub fn models_nothing(&self) -> bool {
        *self == Self::TRANSPARENT
    }

    /// The assumptions, for a fidelity report.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.models_nothing() {
            return "L1 with a transparent policy: queue, latency and impact all \
                    switched off, so this run is L0 wearing L1's name"
                .to_string();
        }
        let queue = match self.queue {
            QueueAhead::None => "no queue assumed".to_string(),
            QueueAhead::Fixed(q) => format!("{} lots assumed queued ahead", q.0),
            QueueAhead::VolumeMultiple(m) => format!(
                "{}.{:02}x the arrival observation's volume assumed queued ahead",
                m / 100,
                m % 100
            ),
        };
        format!(
            "queue: {queue}; entry latency {}, response latency {}; \
             impact coefficient {}.{:02}. Every figure is an assumption about this \
             market, not a measurement of it.",
            self.latency.entry.describe(),
            self.latency.response.describe(),
            self.impact.coefficient / 100,
            self.impact.coefficient % 100,
        )
    }
}

/// An order waiting to become live at the venue.
#[derive(Debug, Clone, Copy)]
struct Pending {
    live_at: Nanos,
    order: Working,
}

/// A resting order's remaining queue.
#[derive(Debug, Clone, Copy)]
struct Queued {
    /// Lots that must trade at or through this price before the order
    /// can fill.
    remaining: i64,
    order: Working,
}

/// A fill the venue has made and the strategy has not been told about.
#[derive(Debug, Clone, Copy)]
struct Delayed {
    known_at: Nanos,
    fill: L0Fill,
}

/// L0, with L1's assumptions applied around it.
#[derive(Debug)]
pub struct L1Engine {
    inner: L0Engine,
    policy: Policy,
    pending: Vec<Pending>,
    queued: Vec<Queued>,
    delayed: Vec<Delayed>,
    /// Fills released to the caller this observation.
    released: Vec<L0Fill>,
    /// Cumulative volume at the previous observation, for the traded
    /// volume of this one.
    prev_volume: Option<QtyLots>,
}

impl L1Engine {
    /// Build an L1 engine over a fresh L0.
    #[must_use]
    pub fn new(instrument: oq_types::InstrumentId, policy: Policy) -> Self {
        Self {
            inner: L0Engine::new(instrument),
            policy,
            pending: Vec::new(),
            queued: Vec::new(),
            delayed: Vec::new(),
            released: Vec::new(),
            prev_volume: None,
        }
    }

    /// The assumptions this engine is running under.
    #[must_use]
    pub const fn policy(&self) -> &Policy {
        &self.policy
    }

    /// The L0 engine underneath, for a caller that needs its state.
    #[must_use]
    pub const fn inner(&self) -> &L0Engine {
        &self.inner
    }

    /// Submit an order.
    ///
    /// Under a policy with entry latency the order is held here rather
    /// than reaching the book, because an order that is not yet at the
    /// venue cannot trade — and holding it outside L0 is what keeps L0
    /// unaware that L1 exists.
    pub fn submit(&mut self, order: Working, now: Nanos) {
        if !self.policy.latency.entry.is_zero() {
            self.pending.push(Pending {
                live_at: Nanos(now.0 + self.policy.latency.entry.at(order.id().0).0),
                order,
            });
            return;
        }
        self.admit(order, None);
    }

    /// Withdraw an order, wherever it currently is.
    ///
    /// An order can be in three places under L1 — waiting to arrive,
    /// waiting in a queue, or resting in the book — and a cancel that
    /// only searched the book would silently fail for the first two,
    /// which is a resting order nobody can remove.
    pub fn cancel(&mut self, id: OrderId) -> bool {
        let before = self.pending.len() + self.queued.len();
        self.pending.retain(|p| p.order.id() != id);
        self.queued.retain(|q| q.order.id() != id);
        if self.pending.len() + self.queued.len() < before {
            return true;
        }
        self.inner.cancel(id)
    }

    /// Orders that exist but are not yet in the book.
    ///
    /// Reported because a risk gate counting resting orders must count
    /// these too: an order in flight is an order that can fill.
    #[must_use]
    pub fn shadowed(&self) -> usize {
        self.pending.len() + self.queued.len()
    }

    /// Advance to an observation and return the fills the strategy is
    /// entitled to know about.
    pub fn on_tick(&mut self, tick: &Tick) -> &[L0Fill] {
        let now = tick.stamp.exch;
        let traded = self.traded_volume(tick);

        // 1. Orders whose entry latency has elapsed reach the venue. An
        //    order becomes live at the observation on or after its
        //    arrival time, never before it.
        let mut arriving: Vec<Working> = Vec::new();
        self.pending.retain(|p| {
            if p.live_at.0 <= now.0 {
                arriving.push(p.order);
                false
            } else {
                true
            }
        });
        for order in arriving {
            self.admit(order, Some(traded));
        }

        // 2. Queues deplete against the volume that traded at or through
        //    each order's price. A price that gapped clean through
        //    empties the queue outright: everything ahead traded.
        let mut promoted: Vec<Working> = Vec::new();
        let queue_snapshot = core::mem::take(&mut self.queued);
        for mut q in queue_snapshot {
            let Some(price) = q.order.price() else {
                promoted.push(q.order);
                continue;
            };
            if gapped_through(tick, q.order.side(), price) {
                promoted.push(q.order);
                continue;
            }
            if touched(tick, price) {
                q.remaining -= traded;
            }
            if q.remaining <= 0 {
                promoted.push(q.order);
            } else {
                self.queued.push(q);
            }
        }
        for order in promoted {
            self.inner.submit(order);
        }

        // 3. L0 matches, untouched.
        let produced: Vec<L0Fill> = self.inner.on_tick(tick).to_vec();

        // 4. Impact worsens taker fills. Applied after matching because
        //    it is a property of the fill's size against the market, and
        //    L0 has no concept of either.
        let adjusted: Vec<L0Fill> = produced
            .into_iter()
            .map(|f| self.with_impact(f, tick, traded))
            .collect();

        // 5. Response latency holds a fill back from the strategy. The
        //    fill has happened; the account has it; the strategy does
        //    not yet know.
        self.released.clear();
        if !self.policy.latency.response.is_zero() {
            for fill in adjusted {
                self.delayed.push(Delayed {
                    known_at: Nanos(now.0 + self.policy.latency.response.at(fill.fill.order.0).0),
                    fill,
                });
            }
            let ready: Vec<L0Fill> = self
                .delayed
                .iter()
                .filter(|d| d.known_at.0 <= now.0)
                .map(|d| d.fill)
                .collect();
            self.delayed.retain(|d| d.known_at.0 > now.0);
            self.released.extend(ready);
        } else {
            self.released.extend(adjusted);
        }

        self.prev_volume = Some(tick.volume);
        &self.released
    }

    /// Fills that have happened and not yet been reported.
    ///
    /// A run that ends must account for these: they are the account's
    /// and the strategy has not seen them, which is precisely the state
    /// a restart has to reconcile.
    #[must_use]
    pub fn unreported(&self) -> usize {
        self.delayed.len()
    }

    /// Put an order into the book, or into a queue in front of it.
    fn admit(&mut self, order: Working, arrival_volume: Option<i64>) {
        let ahead = match (self.policy.queue, order.price()) {
            // A market order queues for nothing; it is the thing the
            // queue is waiting for.
            (_, None) | (QueueAhead::None, _) => 0,
            (QueueAhead::Fixed(q), _) => q.0,
            (QueueAhead::VolumeMultiple(m), _) => {
                let base = arrival_volume.unwrap_or(0);
                i64::from(m) * base / 100
            }
        };
        if ahead > 0 {
            self.queued.push(Queued {
                remaining: ahead,
                order,
            });
        } else {
            self.inner.submit(order);
        }
    }

    /// Volume that traded in this observation.
    fn traded_volume(&self, tick: &Tick) -> i64 {
        match self.prev_volume {
            // Cumulative volume that went backwards is not cumulative,
            // and a negative traded volume would refund a queue.
            Some(prev) => (tick.volume.0 - prev.0).max(0),
            None => 0,
        }
    }

    /// Worsen a taker fill by the square-root impact penalty.
    fn with_impact(&self, mut f: L0Fill, tick: &Tick, traded: i64) -> L0Fill {
        if self.policy.impact.coefficient == 0 || f.fill.liquidity != oq_types::Liquidity::Taker {
            return f;
        }
        if traded <= 0 {
            // No volume to be a share of. Charging a penalty against an
            // unknown denominator would be inventing one.
            return f;
        }
        let range = (tick.high.0 - tick.low.0).max(0);
        if range == 0 {
            return f;
        }
        let share = f.fill.qty.0 as f64 / traded as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let penalty = (f64::from(self.policy.impact.coefficient) / 100.0
            * range as f64
            * share.sqrt()) as i64;
        if penalty == 0 {
            return f;
        }
        // Always against the taker: a buy pays more, a sell receives
        // less. Impact that could help would not be impact.
        f.fill.price = PriceTicks(match f.fill.side {
            Side::Buy => f.fill.price.0 + penalty,
            Side::Sell => f.fill.price.0 - penalty,
        });
        f
    }
}

/// Whether this observation traded at the order's price at all.
fn touched(tick: &Tick, price: PriceTicks) -> bool {
    tick.low.0 <= price.0 && price.0 <= tick.high.0
}

/// Whether the price went clean through, leaving nothing ahead.
///
/// A buy at 100 is passed through when the market traded entirely below
/// 100: everything queued at 100 was lifted on the way.
fn gapped_through(tick: &Tick, side: Side, price: PriceTicks) -> bool {
    match side {
        Side::Buy => tick.high.0 < price.0,
        Side::Sell => tick.low.0 > price.0,
    }
}

/// A fill, for a caller that wants only the fill.
#[must_use]
pub fn fills_of(fills: &[L0Fill]) -> Vec<Fill> {
    fills.iter().map(|f| f.fill).collect()
}

#[cfg(test)]
mod tests {

    /// A drawn delay depends on the key and on nothing else.
    ///
    /// This is the property replay rests on. A generator carried in the
    /// matcher would make one order's delay depend on how many came
    /// before it, so a run resumed from a snapshot — which starts
    /// mid-sequence — would fill differently from the same events.
    #[test]
    fn the_same_key_draws_the_same_delay() {
        let d = Delay::Measured {
            p50: Nanos(1),
            p90: Nanos(2),
            p99: Nanos(3),
            p999: Nanos(4),
        };
        for key in [0, 1, 7, 12_345, u64::MAX] {
            assert_eq!(d.at(key), d.at(key));
        }
    }

    /// Every draw is one of the four measured points.
    ///
    /// Nearest-rank, never interpolated — the convention everywhere a
    /// quantile appears in this workspace. An interpolated quantile is
    /// a value nothing was observed at, and a latency nobody measured
    /// is exactly what this type exists to avoid inventing.
    #[test]
    fn a_draw_is_always_a_measured_point() {
        let d = Delay::Measured {
            p50: Nanos(10),
            p90: Nanos(20),
            p99: Nanos(30),
            p999: Nanos(40),
        };
        for key in 0..5_000 {
            let n = d.at(key).0;
            assert!(
                [10, 20, 30, 40].contains(&n),
                "drew {n}, which nothing measured"
            );
        }
    }

    /// And the four appear at roughly the frequencies they name.
    ///
    /// Loose bounds on purpose: the point is that the bands are wired
    /// to the right points, not that a hash is uniform to three
    /// decimals. A tight bound here would fail on a future hash change
    /// that was not a defect.
    #[test]
    fn the_bands_have_roughly_the_frequencies_they_claim() {
        let d = Delay::Measured {
            p50: Nanos(1),
            p90: Nanos(2),
            p99: Nanos(3),
            p999: Nanos(4),
        };
        let mut count = [0u32; 5];
        for key in 0..100_000u64 {
            count[d.at(key).0 as usize] += 1;
        }
        assert!((45_000..55_000).contains(&count[1]), "p50: {}", count[1]);
        assert!((35_000..45_000).contains(&count[2]), "p90: {}", count[2]);
        assert!((7_000..11_000).contains(&count[3]), "p99: {}", count[3]);
        assert!((500..1_500).contains(&count[4]), "p999: {}", count[4]);
    }

    /// A distribution of zeroes delays nothing, so the transparent
    /// policy stays transparent however it is written.
    #[test]
    fn a_distribution_of_zeroes_is_zero() {
        assert!(Delay::Fixed(Nanos(0)).is_zero());
        assert!(
            Delay::Measured {
                p50: Nanos(0),
                p90: Nanos(0),
                p99: Nanos(0),
                p999: Nanos(0),
            }
            .is_zero()
        );
        assert!(
            !Delay::Measured {
                p50: Nanos(0),
                p90: Nanos(0),
                p99: Nanos(0),
                p999: Nanos(1),
            }
            .is_zero(),
            "a tail that delays is not no delay"
        );
    }

    /// The assumptions line shows all four points.
    ///
    /// Collapsing to a median would print the same sentence for a run
    /// that modelled dispersion and one that did not, and the
    /// difference is the choice the caller made.
    #[test]
    fn the_report_line_distinguishes_a_distribution_from_a_constant() {
        let fixed = Delay::Fixed(Nanos(5)).describe();
        let drawn = Delay::Measured {
            p50: Nanos(5),
            p90: Nanos(50),
            p99: Nanos(500),
            p999: Nanos(5_000),
        }
        .describe();
        assert_ne!(fixed, drawn);
        assert!(drawn.contains("5000"), "the tail must be visible: {drawn}");
    }

    use super::*;
    use oq_types::{InstrumentId, Offset, Stamp};

    const SEC: i64 = 1_000_000_000;

    fn tick(i: i64, last: i64, high: i64, low: i64, volume: i64) -> Tick {
        Tick {
            stamp: Stamp {
                exch: Nanos(i * SEC),
                local: Nanos(i * SEC),
            },
            last: PriceTicks(last),
            high: PriceTicks(high),
            low: PriceTicks(low),
            bid: PriceTicks(last - 1),
            ask: PriceTicks(last + 1),
            volume: QtyLots(volume),
        }
    }

    /// A price path with volume, used by every test below so the
    /// differences are the policy and nothing else.
    fn series() -> Vec<Tick> {
        vec![
            tick(1, 100, 101, 99, 1_000),
            tick(2, 102, 103, 101, 1_100),
            tick(3, 105, 106, 102, 1_250),
            tick(4, 103, 106, 102, 1_400),
            tick(5, 99, 104, 98, 1_600),
            tick(6, 97, 100, 96, 1_900),
        ]
    }

    fn order(id: u64, kind: oq_types::OrderKind, qty: i64) -> Working {
        Working::Live(
            oq_types::Order::with_offset(
                OrderId(id),
                Side::Buy,
                kind,
                QtyLots(qty),
                oq_types::TimeInForce::GoodTilCancel,
                Stamp {
                    exch: Nanos(0),
                    local: Nanos(0),
                },
                Offset::Open,
            )
            .expect("positive quantity")
            .accept(),
        )
    }

    fn limit_buy(id: u64, price: i64, qty: i64) -> Working {
        order(
            id,
            oq_types::OrderKind::Limit {
                price: PriceTicks(price),
            },
            qty,
        )
    }

    fn market_buy(id: u64, qty: i64) -> Working {
        order(id, oq_types::OrderKind::Market, qty)
    }

    /// **The property that keeps FR-MATCH-2's promise.** L0 is the
    /// migration and regression anchor, and the cheapest way to keep a
    /// promise like that is to make breaking it impossible rather than
    /// to test for it — but the test is what proves the construction
    /// worked.
    #[test]
    fn a_transparent_policy_reproduces_l0_exactly() {
        let ticks = series();

        let mut l0 = L0Engine::new(InstrumentId::new(1));
        l0.submit(limit_buy(1, 100, 5));
        let mut from_l0: Vec<Fill> = Vec::new();
        for t in &ticks {
            from_l0.extend(l0.on_tick(t).iter().map(|f| f.fill));
        }

        let mut l1 = L1Engine::new(InstrumentId::new(1), Policy::TRANSPARENT);
        l1.submit(limit_buy(1, 100, 5), Nanos(0));
        let mut from_l1: Vec<Fill> = Vec::new();
        for t in &ticks {
            from_l1.extend(l1.on_tick(t).iter().map(|f| f.fill));
        }

        assert!(!from_l0.is_empty(), "the fixture must actually fill");
        assert_eq!(from_l1, from_l0, "L1 must not change L0's answer");
    }

    /// And it says so, rather than a transparent run looking like a
    /// higher-fidelity one in a report.
    #[test]
    fn a_transparent_policy_admits_what_it_is() {
        assert!(Policy::TRANSPARENT.models_nothing());
        assert!(
            Policy::TRANSPARENT
                .describe()
                .contains("L0 wearing L1's name"),
            "{}",
            Policy::TRANSPARENT.describe()
        );
    }

    /// The maker strategy's whole problem: L0 fills when the price
    /// arrives, and a real book fills when the queue clears.
    #[test]
    fn a_queue_delays_a_fill_that_l0_would_have_given_immediately() {
        let ticks = series();
        let policy = Policy {
            queue: QueueAhead::Fixed(QtyLots(400)),
            ..Policy::TRANSPARENT
        };

        let mut transparent = L1Engine::new(InstrumentId::new(1), Policy::TRANSPARENT);
        transparent.submit(limit_buy(1, 100, 5), Nanos(0));
        let mut queued = L1Engine::new(InstrumentId::new(1), policy);
        queued.submit(limit_buy(1, 100, 5), Nanos(0));

        let mut first_transparent = None;
        let mut first_queued = None;
        for (i, t) in ticks.iter().enumerate() {
            if first_transparent.is_none() && !transparent.on_tick(t).is_empty() {
                first_transparent = Some(i);
            }
            if first_queued.is_none() && !queued.on_tick(t).is_empty() {
                first_queued = Some(i);
            }
        }

        let a = first_transparent.expect("L0 fills this");
        let b = first_queued.expect("the queue clears eventually");
        assert!(
            b > a,
            "the queue must delay the fill: transparent {a}, queued {b}"
        );
    }

    /// An order still in a queue is an order that exists. A risk gate
    /// counting only the book would let a strategy hold more than its
    /// cap, because the excess is invisible.
    #[test]
    fn an_order_waiting_in_a_queue_is_still_counted() {
        let policy = Policy {
            queue: QueueAhead::Fixed(QtyLots(10_000)),
            ..Policy::TRANSPARENT
        };
        let mut e = L1Engine::new(InstrumentId::new(1), policy);
        e.submit(limit_buy(1, 100, 5), Nanos(0));
        assert_eq!(e.shadowed(), 1, "queued, not resting, and not invisible");
        assert_eq!(e.inner().book().len(), 0, "and not in the book");
    }

    /// A cancel has to reach an order wherever it is. One that only
    /// searched the book would silently fail for an order in flight or
    /// in a queue, leaving a resting order nobody can remove.
    #[test]
    fn a_cancel_reaches_an_order_in_every_place_it_can_be() {
        let policy = Policy {
            queue: QueueAhead::Fixed(QtyLots(10_000)),
            latency: Latency {
                entry: Delay::Fixed(Nanos(10 * SEC)),
                response: Delay::Fixed(Nanos(0)),
            },
            ..Policy::TRANSPARENT
        };

        // In flight.
        let mut e = L1Engine::new(InstrumentId::new(1), policy);
        e.submit(limit_buy(1, 100, 5), Nanos(0));
        assert_eq!(e.shadowed(), 1);
        assert!(e.cancel(OrderId(1)), "in flight");
        assert_eq!(e.shadowed(), 0);

        // In a queue.
        let mut e = L1Engine::new(
            InstrumentId::new(1),
            Policy {
                latency: Latency::default(),
                ..policy
            },
        );
        e.submit(limit_buy(1, 100, 5), Nanos(0));
        assert_eq!(e.shadowed(), 1);
        assert!(e.cancel(OrderId(1)), "queued");

        // Resting.
        let mut e = L1Engine::new(InstrumentId::new(1), Policy::TRANSPARENT);
        e.submit(limit_buy(1, 100, 5), Nanos(0));
        assert!(e.cancel(OrderId(1)), "resting");
        assert!(!e.cancel(OrderId(1)), "and not twice");
    }

    /// An order that has not arrived cannot trade, however good the
    /// price gets in the meantime.
    #[test]
    fn entry_latency_keeps_an_order_out_of_a_market_it_has_not_reached() {
        let policy = Policy {
            latency: Latency {
                entry: Delay::Fixed(Nanos(4 * SEC)),
                response: Delay::Fixed(Nanos(0)),
            },
            ..Policy::TRANSPARENT
        };
        let mut e = L1Engine::new(InstrumentId::new(1), policy);
        // Submitted at t=1, so it is not live until t=5.
        e.submit(market_buy(1, 1), Nanos(SEC));

        let ticks = series();
        assert!(e.on_tick(&ticks[1]).is_empty(), "t=2: not arrived");
        assert!(e.on_tick(&ticks[2]).is_empty(), "t=3: not arrived");
        assert!(e.on_tick(&ticks[3]).is_empty(), "t=4: not arrived");
        assert!(!e.on_tick(&ticks[4]).is_empty(), "t=5: arrived and filled");
    }

    /// The fill has happened and the account has it; the strategy does
    /// not yet know. That gap is the state a restart has to reconcile,
    /// so it is counted rather than smoothed away.
    #[test]
    fn response_latency_holds_a_fill_back_and_says_how_many() {
        let policy = Policy {
            latency: Latency {
                entry: Delay::Fixed(Nanos(0)),
                response: Delay::Fixed(Nanos(2 * SEC)),
            },
            ..Policy::TRANSPARENT
        };
        let mut e = L1Engine::new(InstrumentId::new(1), policy);
        e.submit(market_buy(1, 1), Nanos(0));

        let ticks = series();
        assert!(e.on_tick(&ticks[0]).is_empty(), "filled, not yet reported");
        assert_eq!(e.unreported(), 1, "and the run knows it is owed one");
        assert!(e.on_tick(&ticks[1]).is_empty(), "still inside the delay");
        assert_eq!(e.on_tick(&ticks[2]).len(), 1, "now reported");
        assert_eq!(e.unreported(), 0);
    }

    /// Impact is always against the taker. One that could help would not
    /// be impact.
    #[test]
    fn impact_worsens_a_taker_fill_and_never_improves_it() {
        let ticks = series();
        let mut plain = L1Engine::new(InstrumentId::new(1), Policy::TRANSPARENT);
        plain.submit(market_buy(1, 1), Nanos(0));
        plain.on_tick(&ticks[0]);
        let base = plain.on_tick(&ticks[1]);
        let base_price = base.first().map(|f| f.fill.price);

        let policy = Policy {
            impact: Impact { coefficient: 100 },
            ..Policy::TRANSPARENT
        };
        let mut hit = L1Engine::new(InstrumentId::new(1), policy);
        hit.submit(market_buy(1, 100), Nanos(0));
        hit.on_tick(&ticks[0]);
        let with = hit.on_tick(&ticks[1]);

        if let (Some(b), Some(w)) = (base_price, with.first().map(|f| f.fill.price)) {
            assert!(
                w.0 >= b.0,
                "a buy must not fill better under impact: {} vs {}",
                w.0,
                b.0
            );
        }
    }

    /// A share needs a denominator. Charging a penalty against volume
    /// nobody observed would be inventing one.
    #[test]
    fn impact_is_not_charged_when_there_is_no_volume_to_be_a_share_of() {
        let policy = Policy {
            impact: Impact { coefficient: 500 },
            ..Policy::TRANSPARENT
        };
        // The very first observation has no previous volume to
        // difference against, so nothing traded as far as this engine
        // knows. Compared against a transparent run rather than a
        // hard-coded price, because the unpenalised price is the ask and
        // asserting a number would be asserting the fixture.
        let flat = tick(1, 100, 100, 100, 1_000);

        let mut charged = L1Engine::new(InstrumentId::new(1), policy);
        charged.submit(market_buy(1, 50), Nanos(0));
        let with: Vec<PriceTicks> = charged
            .on_tick(&flat)
            .iter()
            .map(|f| f.fill.price)
            .collect();

        let mut plain = L1Engine::new(InstrumentId::new(1), Policy::TRANSPARENT);
        plain.submit(market_buy(1, 50), Nanos(0));
        let without: Vec<PriceTicks> = plain.on_tick(&flat).iter().map(|f| f.fill.price).collect();

        assert!(!without.is_empty(), "the fixture must fill");
        assert_eq!(with, without, "no volume, no penalty");
    }

    /// Cumulative volume that goes backwards would refund a queue,
    /// letting an order fill sooner because the feed glitched.
    #[test]
    fn volume_going_backwards_does_not_refund_a_queue() {
        let policy = Policy {
            queue: QueueAhead::Fixed(QtyLots(500)),
            ..Policy::TRANSPARENT
        };
        let mut e = L1Engine::new(InstrumentId::new(1), policy);
        e.submit(limit_buy(1, 100, 1), Nanos(0));
        e.on_tick(&tick(1, 100, 101, 99, 5_000));
        // A reset: cumulative volume drops.
        e.on_tick(&tick(2, 100, 101, 99, 10));
        assert_eq!(e.shadowed(), 1, "the queue must not have been credited");
    }

    /// A price that gapped clean through the order lifted everything
    /// ahead of it, whatever the assumed queue was.
    #[test]
    fn a_price_gapping_through_empties_the_queue() {
        let policy = Policy {
            queue: QueueAhead::Fixed(QtyLots(1_000_000)),
            ..Policy::TRANSPARENT
        };
        let mut e = L1Engine::new(InstrumentId::new(1), policy);
        e.submit(limit_buy(1, 100, 1), Nanos(0));
        assert_eq!(e.shadowed(), 1);
        // The market traded entirely below the order.
        e.on_tick(&tick(1, 90, 95, 85, 1_000));
        assert_eq!(
            e.shadowed(),
            0,
            "everything queued at 100 was taken on the way down"
        );
    }

    /// The description is what reaches a fidelity report, so it has to
    /// carry every assumption rather than a summary of them.
    #[test]
    fn the_policy_describes_every_assumption_it_holds() {
        let p = Policy {
            queue: QueueAhead::VolumeMultiple(150),
            latency: Latency {
                entry: Delay::Fixed(Nanos(3_000_000)),
                response: Delay::Fixed(Nanos(7_000_000)),
            },
            impact: Impact { coefficient: 250 },
        };
        let text = p.describe();
        assert!(text.contains("1.50x"), "{text}");
        assert!(
            text.contains("3000000") && text.contains("7000000"),
            "{text}"
        );
        assert!(text.contains("2.50"), "{text}");
        assert!(
            text.contains("assumption about this market, not a measurement"),
            "{text}"
        );
    }
}
