//! A real strategy, against a real venue, in about thirty lines.
//!
//! ```text
//! OQ_VENUE_KEY=… OQ_VENUE_SECRET=… \
//!   cargo run --release -p oq-live --example grid_live -- --symbol BTCUSDT --minutes 30
//! ```
//!
//! Until this existed the public repository had no way to run a strategy
//! against a venue at all. `oq-trade`'s `observe` sends nothing and its
//! `probe` is a connectivity diagnostic, so anybody wanting a long run
//! had to bring their own strategy *and* reimplement the assembly to do
//! it — which is the duplication `oq_live::run` was extracted to remove.
//!
//! # Why it lives here rather than as an `oq-trade` subcommand
//!
//! The strategy comes from `oq-examples`, which is `publish = false`. A
//! published crate cannot depend on an unpublished one, and nothing that
//! links `oq-live` should inherit a catalogue of teaching material
//! either. So `oq-examples` is a **dev**-dependency and this is an
//! example: the strategy stays in one place, the dependency tree a
//! consumer pulls in is unchanged, and there is still a live binary.
//!
//! # Why the grid
//!
//! Its failure shape is the one that needs a margin model to be visible
//! — short volatility with no stop — so a long run of it exercises the
//! part of this framework that is hardest to exercise any other way.
//!
//! **It is a teaching reference and not a recommendation.** See
//! `oq_examples::classics`. Testnet only unless somebody deliberately
//! says otherwise, and `oq-live`'s own refusals apply: it will not start
//! beside a position it was not told about.

use oq_examples::classics::GridTrader;
use oq_l2feed::venue::Deployment;
use oq_live::run::{RunConfig, run};
use oq_risk::Limits;
use oq_types::{Cash, Nanos, QtyLots, Ratio};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let number = |flag: &str, default: i64| -> i64 {
        value(flag).and_then(|v| v.parse().ok()).unwrap_or(default)
    };

    if args.iter().any(|a| a == "--live" || a == "--mainnet") {
        eprintln!(
            "grid_live: there is no live mode here. This is a teaching strategy, and a\n\
             teaching strategy with a flag for real money is one somebody eventually\n\
             runs with real money. Point `oq-trade` at a deployment deliberately if\n\
             that is what you mean."
        );
        return ExitCode::FAILURE;
    }

    let cfg = RunConfig {
        symbol: value("--symbol").unwrap_or_else(|| "BTCUSDT".to_string()),
        strategy_name: "grid".to_string(),
        deployment: Deployment::Testnet,
        minutes: number("--minutes", 30),
        window_ms: number("--window-ms", 1_000),
        id_prefix: value("--id-prefix").unwrap_or_else(|| "oqgrid".to_string()),
        broker_code: value("--broker-code"),
        adopt_existing: args.iter().any(|a| a == "--adopt-existing"),
        journal: value("--journal"),
        limits: Limits {
            // Deliberately small, and the grid will hit them. That is the
            // point of running it: a strategy that accumulates without a
            // stop is one whose interaction with a cap is worth watching,
            // and a cap set generously enough never to fire would make
            // the run prove nothing.
            max_order_qty: QtyLots(number("--max-qty", 1)),
            max_position_qty: QtyLots(number("--max-position", 8)),
            max_order_notional: Cash(number("--max-notional", 200) * oq_types::CASH_SCALE),
            price_band: Ratio(number("--band-bps", 3_000) * 100_000),
            max_working: 4,
            max_rate: 10,
            rate_window: Nanos(60 * 1_000_000_000),
        },
    };

    // Testnet, and not by a flag: see the refusal above.
    let creds = match oq_gateway::Credentials::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("credentials      FAILED: {e}");
            return ExitCode::FAILURE;
        }
    };
    let venue: Box<dyn oq_gateway::account::Account> = Box::new(oq_gateway::binance::Binance::at(
        oq_gateway::exec::Endpoint::Testnet,
        creds,
    ));

    run(venue, |_| GridTrader::new(), &cfg)
}
