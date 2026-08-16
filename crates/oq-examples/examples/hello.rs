//! The smallest thing that runs: buy once, hold, print the outcome.
//!
//! ```text
//! cargo run --example hello
//! ```
//!
//! Its job is not to make money — it holds a position through whatever
//! the market does. Its job is to prove the loop turns: a strategy sees
//! observations, returns intents, and the host fills them and keeps the
//! books. Read it before anything else.

use oq_backtest::{Context, Intent, RunConfig, Strategy, run};
use oq_examples::{MarketShape, money, series};
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, InstrumentId, OrderId, QtyLots, Side};

/// Buys once, then does nothing at all.
struct BuyAndHold {
    bought: bool,
}

impl Strategy for BuyAndHold {
    fn on_tick(&mut self, _ctx: &Context, out: &mut Vec<Intent>) {
        if !self.bought {
            self.bought = true;
            out.push(Intent::Market {
                id: OrderId::new(1),
                side: Side::Buy,
                qty: QtyLots(10),
                offset: oq_types::Offset::Open,
            });
        }
    }

    fn name(&self) -> &str {
        "buy-and-hold"
    }
}

fn main() {
    let ticks = series(MarketShape::trending(2_000), 1);
    let config = RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(10_000),
    );

    let result = run(&config, &mut BuyAndHold { bought: false }, &ticks);

    println!("strategy      {}", result.strategy);
    println!("observations  {}", result.ticks);
    println!("fills         {}", result.fills.len());
    println!("final equity  {} USDT", money(result.final_equity));
    println!("lowest equity {} USDT", money(result.min_equity));
    println!("liquidations  {}", result.liquidations.len());
    println!();
    println!("Holding a position is not a strategy. It is the loop working.");
}
