//! A moving-average crossover: the canonical shape, written to show the
//! API rather than to make money.
//!
//! ```text
//! cargo run --example ma_cross
//! ```
//!
//! What to take from it: how state lives in the strategy, how `on_fill`
//! sees executions before the tick that caused them, and how flipping a
//! position is expressed as intents. What *not* to take from it: the
//! parameters. They are round numbers, chosen before the run, and never
//! adjusted to improve the outcome — because a tuned example is a
//! lesson in overfitting dressed as a tutorial.
//!
//! If you change the windows and the result improves, you have just
//! performed the search that [`oq_stats`](../oq-stats) exists to
//! penalise. Run a sweep and read the deflated Sharpe ratio before
//! believing any of it.

use oq_backtest::{Context, Intent, RunConfig, Strategy, run};
use oq_examples::{MarketShape, money, series};
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, Fill, InstrumentId, OrderId, QtyLots, Side};

/// A simple moving average over a fixed window.
struct Sma {
    window: usize,
    samples: Vec<i64>,
    cursor: usize,
    filled: bool,
    sum: i64,
}

impl Sma {
    fn new(window: usize) -> Self {
        Self {
            window,
            samples: vec![0; window],
            cursor: 0,
            filled: false,
            sum: 0,
        }
    }

    /// Push a sample; returns the average once the window is full.
    ///
    /// `None` until then, rather than an average over a partial window:
    /// a warm-up value that looks like a signal is how a backtest starts
    /// trading on nothing.
    fn push(&mut self, value: i64) -> Option<i64> {
        self.sum -= self.samples[self.cursor];
        self.samples[self.cursor] = value;
        self.sum += value;
        self.cursor = (self.cursor + 1) % self.window;
        if self.cursor == 0 {
            self.filled = true;
        }
        self.filled
            .then(|| self.sum / i64::try_from(self.window).unwrap_or(1))
    }
}

/// Long when the fast average is above the slow one, flat otherwise.
struct MaCross {
    fast: Sma,
    slow: Sma,
    position_open: bool,
    next_id: u64,
    flips: usize,
}

impl MaCross {
    fn new() -> Self {
        Self {
            fast: Sma::new(20),
            slow: Sma::new(100),
            position_open: false,
            next_id: 1,
            flips: 0,
        }
    }

    fn id(&mut self) -> OrderId {
        let id = OrderId::new(self.next_id);
        self.next_id += 1;
        id
    }
}

impl Strategy for MaCross {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        let last = ctx.tick.last.0;
        let (Some(fast), Some(slow)) = (self.fast.push(last), self.slow.push(last)) else {
            return; // still warming up
        };

        let want_long = fast > slow;
        if want_long == self.position_open {
            return;
        }

        self.position_open = want_long;
        self.flips += 1;
        let id = self.id();
        out.push(ctx.market(
            id,
            if want_long { Side::Buy } else { Side::Sell },
            QtyLots(if ctx.position.0 == 0 {
                5
            } else {
                ctx.position.0.abs()
            }),
        ));
    }

    fn on_fill(&mut self, _fill: &Fill, _ctx: &Context, _out: &mut Vec<Intent>) {
        // Nothing to manage: the position is flat or fully on, and the
        // next crossing decides which.
    }

    fn name(&self) -> &str {
        "ma-cross-20-100"
    }
}

fn main() {
    let ticks = series(MarketShape::trending(4_000), 5);
    let config = RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(10_000),
    )
    // The fidelity report wants to know how close the account came to
    // its maintenance requirement, and that costs throughput, so a run
    // has to ask for it. See RunConfig::tracking_margin.
    .tracking_margin();

    let mut strategy = MaCross::new();
    let result = run(&config, &mut strategy, &ticks);

    println!("strategy      {}", result.strategy);
    println!("observations  {}", result.ticks);
    println!("signal flips  {}", strategy.flips);
    println!("fills         {}", result.fills.len());
    println!("final equity  {} USDT", money(result.final_equity));
    println!("lowest equity {} USDT", money(result.min_equity));
    println!("realized      {} USDT", money(result.realized));
    println!(
        "worst excursion against position: {} ticks",
        result.max_adverse_ticks
    );
    println!();
    println!(
        "One run of one parameter pair on one generated series says nothing about\n\
         whether this works. That is the point: the number above is a demonstration\n\
         of the API, not evidence. Evidence needs a sweep and a deflated Sharpe ratio."
    );

    // The result above is only worth acting on if the run could have
    // happened. FR-MATCH-3 asks every backtest to say so; this is that
    // report, and the participation line is the one that decides it.
    println!();
    print!(
        "{}",
        oq_backtest::fidelity_report(
            &result,
            &ticks,
            // Sixty observations to a window. A strategy can be small
            // over a day and enormous over a minute, and the minute is
            // what invalidates a replay.
            60,
            oq_backtest::validity::DEFAULT_THRESHOLD,
        )
        .render()
    );
}
