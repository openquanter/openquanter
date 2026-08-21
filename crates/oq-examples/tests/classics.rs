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

// ---------------------------------------------------------------------
// The grid's ladder
// ---------------------------------------------------------------------
//
// `GridTrader` is the only strategy in the catalogue that carries state
// derived from its own orders, and the only one wired to a venue
// (`oq-live`'s `grid_live` example). Both facts point at the same risk:
// a refused rung must not move the ladder. These drive the strategy
// directly rather than through `run`, because the simulated matcher
// fills market orders immediately and so cannot express a refusal.

use oq_backtest::{Context, Intent};
use oq_types::{Fill, Liquidity, Offset, OrderId, PriceTicks, QtyLots, Side, Stamp, TradeId};

fn at(price: i64, position: i64) -> Context {
    Context {
        instrument: oq_types::InstrumentId::new(1),
        tick: oq_engine::Tick {
            last: PriceTicks(price),
            ..oq_engine::Tick::default()
        },
        position: QtyLots(position),
        entry: PriceTicks(0),
        short_position: QtyLots(0),
        short_entry: PriceTicks(0),
        equity: Cash::from_units(10_000),
        working: 0,
    }
}

fn filled(order: OrderId, price: i64, side: Side) -> Fill {
    Fill {
        stamp: Stamp::default(),
        instrument: InstrumentId::new(1),
        order,
        trade: TradeId(order.0),
        side,
        offset: Offset::Open,
        price: PriceTicks(price),
        qty: QtyLots(1),
        liquidity: Liquidity::Taker,
    }
}

fn asked(out: &[Intent]) -> Option<OrderId> {
    out.iter().find_map(|i| match i {
        Intent::Market { id, .. } | Intent::Limit { id, .. } => Some(*id),
        _ => None,
    })
}

/// A refused rung leaves the ladder where it was, and the same
/// condition places it again.
///
/// The failure this guards against is silent: an optimistic grid
/// advances its anchor on submission, so after a refusal it waits for a
/// step down from a rung it never bought. Price would have to fall
/// twice as far before it acted, and further for every later refusal —
/// a strategy that looks like it is running and is not.
#[test]
fn refused_rung_does_not_move_the_ladder() {
    let mut grid = GridTrader::new();
    let mut out = Vec::new();

    // The anchoring rung, refused.
    grid.on_tick(&at(100_000, 0), &mut out);
    let first = asked(&out).expect("the grid anchors on its first observation");
    grid.on_placed(first, false);

    // Same observation, no position: it must ask again.
    out.clear();
    grid.on_tick(&at(100_000, 0), &mut out);
    let second = asked(&out).expect("a refused anchor is retried, not skipped");
    assert_ne!(first, second, "a retry is a new order, not a resubmission");
}

/// The ladder anchors on the price that was paid, not on the tick that
/// produced the order.
///
/// Anchoring on the trigger price would fold every rung's slippage into
/// the grid's geometry, where it stops being visible as a cost and
/// starts being visible as a strategy that trades at slightly wrong
/// levels — which is the harder bug to ever notice.
#[test]
fn the_ladder_anchors_on_the_fill_price() {
    let mut grid = GridTrader::new();
    let mut out = Vec::new();

    grid.on_tick(&at(100_000, 0), &mut out);
    let first = asked(&out).expect("the grid anchors on its first observation");
    // Asked at 100_000, paid 101_000 — a full step of slippage.
    grid.on_fill(
        &filled(first, 101_000, Side::Buy),
        &at(101_000, 1),
        &mut out,
    );

    // Anchored on the fill, the next rung is a step below 101_000, so
    // it triggers at 100_495. Anchored on the price that *triggered*
    // the order it would be a step below 100_000, or 99_500. The two
    // answers differ across (99_500, 100_495] and nowhere else, which
    // is the only interval worth probing: outside it both anchorings
    // agree and a passing assertion would prove nothing.
    out.clear();
    grid.on_tick(&at(100_000, 1), &mut out);
    assert!(
        asked(&out).is_some(),
        "100_000 is a step below the fill at 101_000; only a ladder still \
         anchored on the trigger price would sit here doing nothing"
    );
}

/// One rung in flight at a time.
///
/// The entry condition stays true until the rung fills, so a grid
/// ticking faster than the venue answers would place one per tick — the
/// position grows by however long the round trip took, which is a
/// number the strategy never approved.
#[test]
fn only_one_rung_is_outstanding() {
    let mut grid = GridTrader::new();
    let mut out = Vec::new();

    grid.on_tick(&at(100_000, 0), &mut out);
    assert!(asked(&out).is_some());

    for _ in 0..20 {
        out.clear();
        grid.on_tick(&at(100_000, 0), &mut out);
        assert!(
            out.is_empty(),
            "a second rung was placed while the first was unanswered"
        );
    }
}
