//! What a market that stops trading costs an account that did not.
//!
//! `oq-ingest` produces ticks; `oq-core` prices an account from them.
//! Nothing between the two ever objected to a tick with no price in it,
//! and that is the whole reason this test lives here rather than in
//! either crate: the aggregator emitted `last = 0` without complaint,
//! every accessor on `Tick` returned zero without complaint, and the
//! first thing that noticed was the account being closed.
//!
//! Over twelve hours of a real testnet feed, 56% of ticks carried
//! `last = 0`.

use oq_backtest::{Context, Intent, MarginMode, RunConfig, Strategy, run};
use oq_ingest::agg::Aggregator;
use oq_l2feed::venue::Trade;
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, InstrumentId, OrderId, QtyLots, Side};

const SEC: i64 = 1_000_000_000;

fn trade(price: i64, qty: i64) -> Trade {
    Trade { price, qty }
}

/// Buys once and then does nothing, so every later number is the
/// account's, not the strategy's.
struct BuyOnce(bool);

impl Strategy for BuyOnce {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        if !self.0 && ctx.position.is_zero() && ctx.tick.last.0 > 0 {
            self.0 = true;
            out.push(ctx.market(OrderId(1), Side::Buy, QtyLots(1)));
        }
    }
    fn name(&self) -> &str {
        "buy-once"
    }
}

/// A market that trades for a while and then goes quiet.
///
/// The busy windows come first for a reason the first version of this
/// fixture got wrong: it put a single trade in window zero and silence
/// after it, so the market order placed on that observation matched
/// against the *next* window — which was one of the empty ones — and
/// filled at a price of zero. A position opened at zero and marked at
/// zero shows no loss, so the test passed against the unfixed code and
/// proved nothing. The account has to be opened at a real price before
/// a zero can hurt it.
fn busy_then_quiet(price: i64) -> Vec<oq_engine::Tick> {
    let mut a = Aggregator::new(SEC).expect("positive window");
    let mut ticks = Vec::new();
    // Four windows that trade, so the position is opened and priced.
    for w in 0..4 {
        a.on_trade(w * SEC, w * SEC, &trade(price, 1));
        if let Some(t) = a.advance_to((w + 1) * SEC, (w + 1) * SEC) {
            ticks.push(t);
        }
    }
    // Then twenty in which nothing trades at all.
    for w in 5..25 {
        if let Some(t) = a.advance_to(w * SEC, w * SEC) {
            ticks.push(t);
        }
    }
    ticks.extend(a.flush());
    ticks
}

fn liquidations(balance: i64, ticks: &[oq_engine::Tick]) -> usize {
    let cfg = RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(balance),
    )
    .with_margin(MarginMode::Enforced);
    run(&cfg, &mut BuyOnce(false), ticks).liquidations.len()
}

/// A quiet market does not close a levered account.
///
/// Before the aggregator carried `last` across windows, the first quiet
/// window emitted a zero, `Kernel::on_tick` took it as the mark, and
/// `check_liquidation` compared equity at a price of nothing against a
/// maintenance requirement of nothing. Any position whose entry notional
/// reached the balance — **1x leverage, not some extreme** — was closed
/// on the spot, on a market that had not moved at all.
///
/// Measured at the time: 10x leverage liquidated, 1.00x liquidated,
/// 0.86x did not.
#[test]
fn a_quiet_market_does_not_liquidate_a_levered_account() {
    let ticks = busy_then_quiet(6_000_000);
    assert!(ticks.len() > 20, "the fixture produced no quiet windows");
    assert!(
        ticks[..4].iter().all(|t| t.last.0 > 0),
        "the fixture must open the position at a real price, or a zero \
         later on has nothing to be wrong about"
    );
    for balance in [10_000, 1_000, 700, 600, 60] {
        assert_eq!(
            liquidations(balance, &ticks),
            0,
            "a market that never moved closed an account with {balance} units \
             of capital behind one lot"
        );
    }
}

/// And it does not invent a drawdown in the accounts it leaves open.
///
/// This is the half that made the other half worth finding. Below 1x
/// leverage nothing was liquidated and nothing was reported — the run
/// simply recorded a worst equity lower by exactly the position's
/// notional, on a market that had not moved. A backtest that quietly
/// overstates the drawdown of every strategy replayed from a real
/// capture is a worse outcome than one that fails loudly.
#[test]
fn a_quiet_market_does_not_invent_a_drawdown() {
    let ticks = busy_then_quiet(6_000_000);
    let cfg = RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(10_000),
    )
    .with_margin(MarginMode::Enforced);
    let r = run(&cfg, &mut BuyOnce(false), &ticks);
    assert_eq!(r.fills.len(), 1, "the fixture did not open a position");
    assert_eq!(
        r.min_equity,
        Cash::from_units(10_000),
        "a motionless market produced a drawdown; worst equity came back as {}",
        r.min_equity.0 as f64 / 100_000_000.0
    );
}
