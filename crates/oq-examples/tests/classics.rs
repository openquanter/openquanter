//! The catalogue, held to what it claims.
//!
//! These are teaching references, so what has to stay true is not a
//! P&L — it is that each one *does the thing its documentation says*.
//! A classic strategy that silently stopped trading would still print a
//! plausible table, and a reader would learn the framework from a
//! strategy that was not running.

use oq_backtest::{MarginMode, RunConfig, Strategy, run};
use oq_examples::classics::{
    BollingerReversion, DonchianBreakout, DualThrust, GridTrader, MacdTrend, RsiReversion,
    catalogue,
};
use oq_examples::{MarketShape, crash_series, series};
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, InstrumentId};

fn config(balance: i64) -> RunConfig {
    RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(balance),
    )
    .with_margin(MarginMode::Enforced)
}

fn fills<S: Strategy>(mut s: S, ticks: &[oq_engine::Tick]) -> usize {
    run(&config(10_000), &mut s, ticks).fills.len()
}

/// Every one of them trades. A strategy whose indicator never warms up,
/// or whose comparison can never be true, prints an empty row that
/// looks like a quiet market rather than like a bug — and the catalogue
/// would be teaching the framework with something that is not running.
#[test]
fn every_classic_actually_trades() {
    let market = crash_series(11, 3_000, 900, 0.45);
    let counts = [
        ("rsi-reversion", fills(RsiReversion::new(), &market)),
        ("macd-trend", fills(MacdTrend::new(), &market)),
        (
            "bollinger-reversion",
            fills(BollingerReversion::new(), &market),
        ),
        ("donchian-breakout", fills(DonchianBreakout::new(), &market)),
        ("grid", fills(GridTrader::new(), &market)),
        ("dual-thrust", fills(DualThrust::new(), &market)),
    ];
    for (name, n) in counts {
        assert!(n > 0, "{name} made no trades at all");
    }
}

/// And on a calm market too. A breakout system that only fires on a
/// crash is a breakout system whose test data did the work.
#[test]
fn every_classic_trades_on_a_quiet_market_too() {
    let calm = series(MarketShape::calm(4_000), 5);
    for (name, n) in [
        ("rsi-reversion", fills(RsiReversion::new(), &calm)),
        ("macd-trend", fills(MacdTrend::new(), &calm)),
        (
            "bollinger-reversion",
            fills(BollingerReversion::new(), &calm),
        ),
        ("donchian-breakout", fills(DonchianBreakout::new(), &calm)),
        ("grid", fills(GridTrader::new(), &calm)),
        ("dual-thrust", fills(DualThrust::new(), &calm)),
    ] {
        assert!(n > 0, "{name} made no trades on a calm market");
    }
}

/// None of them warms up by trading. An indicator with no reading yet
/// is not a neutral reading, and a strategy that treats it as one trades
/// on the first observation it happens to see — which is a decision
/// nobody made, taken at a price nobody chose.
#[test]
fn none_of_them_trades_before_its_indicator_has_a_reading() {
    // Five observations is fewer than the shortest warm-up in the
    // catalogue, which is Donchian's ten.
    let short = series(MarketShape::calm(5), 3);
    assert_eq!(fills(RsiReversion::new(), &short), 0, "rsi");
    assert_eq!(fills(MacdTrend::new(), &short), 0, "macd");
    assert_eq!(fills(BollingerReversion::new(), &short), 0, "bollinger");
    assert_eq!(fills(DonchianBreakout::new(), &short), 0, "donchian");
    assert_eq!(fills(DualThrust::new(), &short), 0, "dual-thrust");
    // The grid is the exception and says so in its own documentation:
    // its first rung anchors the ladder, so it opens on the first
    // observation by design. Asserted rather than excused, because an
    // exception nobody wrote down is indistinguishable from a defect.
    assert!(
        fills(GridTrader::new(), &short) > 0,
        "the grid anchors immediately"
    );
}

/// The catalogue and the strategies do not drift apart. A row naming a
/// strategy that no longer exists, or a strategy missing from the list,
/// is how a reference becomes wrong while every part of it is right.
#[test]
fn the_catalogue_lists_exactly_what_ships() {
    let listed: Vec<&str> = catalogue().into_iter().map(|c| c.name).collect();
    let shipped = [
        RsiReversion::new().name().to_string(),
        MacdTrend::new().name().to_string(),
        BollingerReversion::new().name().to_string(),
        DonchianBreakout::new().name().to_string(),
        GridTrader::new().name().to_string(),
        DualThrust::new().name().to_string(),
    ];
    assert_eq!(listed.len(), shipped.len());
    for name in &shipped {
        assert!(
            listed.contains(&name.as_str()),
            "{name} ships and is not in the catalogue"
        );
    }
}

/// Every entry says where it breaks, which is the half a reader cannot
/// get from the rules. A catalogue that listed only the rules would be
/// teaching what everybody already has.
#[test]
fn every_entry_names_its_own_weakness() {
    for c in catalogue() {
        assert!(c.weakness.len() > 40, "{}: {:?}", c.name, c.weakness);
        assert!(!c.origin.is_empty(), "{} has no origin", c.name);
        assert!(!c.premise.is_empty(), "{} has no premise", c.name);
    }
}

/// The grid is the one the example singles out, and the reason has to
/// stay true: under leverage the margin-free arm reports a result the
/// account never had.
#[test]
fn the_grid_under_leverage_shows_what_a_margin_free_backtest_hides() {
    let market = crash_series(11, 3_000, 900, 0.45);
    let enforced = run(
        &config(60).with_margin(MarginMode::Enforced),
        &mut GridTrader::new(),
        &market,
    );
    let ignored = run(
        &config(60).with_margin(MarginMode::Ignored),
        &mut GridTrader::new(),
        &market,
    );

    assert!(
        !enforced.liquidations.is_empty(),
        "the venue must have closed this account, or the example's argument is about nothing"
    );
    assert!(
        ignored.min_equity.0 < 0,
        "the margin-free arm must go below zero: that is the account that did not exist"
    );
    assert_ne!(
        enforced.final_equity, ignored.final_equity,
        "the two arms must differ, or there is nothing to read against each other"
    );
}
