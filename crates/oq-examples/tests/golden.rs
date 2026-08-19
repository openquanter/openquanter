//! Golden tests over the examples.
//!
//! The examples print numbers, the documentation quotes them, and a
//! reader who runs the commands expects to see the same thing. That
//! makes the printed values part of the public surface, so they are
//! pinned here.
//!
//! A failure means one of two things and the difference matters:
//!
//! - The engine's behaviour changed. Investigate before updating.
//! - The change was intended. Update the constants *and* the quoted
//!   numbers in the docs, in the same commit.
//!
//! Never relax an assertion to make it pass. These exist precisely to
//! notice when matching, margin or accounting drift.

use oq_backtest::{Context, DeviationReport, Intent, MarginMode, RunConfig, Strategy, run};
use oq_examples::{MarketShape, crash_series, series};
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, InstrumentId, OrderId, QtyLots, Side};

fn config(balance: i64) -> RunConfig {
    RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(balance),
    )
}

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

#[test]
fn hello_produces_the_documented_numbers() {
    let ticks = series(MarketShape::trending(2_000), 1);
    let result = run(&config(10_000), &mut BuyAndHold { bought: false }, &ticks);

    assert_eq!(result.ticks, 2_000);
    assert_eq!(result.fills.len(), 1);
    assert_eq!(result.liquidations.len(), 0);
    assert_eq!(
        result.final_equity,
        Cash(1_086_194_000_000),
        "final equity moved; the README and the example output quote this"
    );
    assert_eq!(result.min_equity, Cash(999_337_900_000));
}

/// The ladder from the flagship example, kept in step with it.
struct MartingaleLadder {
    step: f64,
    base_qty: i64,
    rungs: u32,
    max_rungs: u32,
    next_id: u64,
}

impl MartingaleLadder {
    fn new() -> Self {
        Self {
            step: 0.04,
            base_qty: 4,
            rungs: 0,
            max_rungs: 6,
            next_id: 1,
        }
    }

    fn id(&mut self) -> OrderId {
        let id = OrderId::new(self.next_id);
        self.next_id += 1;
        id
    }
}

impl Strategy for MartingaleLadder {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        if self.rungs == 0 {
            self.rungs = 1;
            let id = self.id();
            out.push(Intent::Market {
                id,
                side: Side::Buy,
                qty: QtyLots(self.base_qty),
                offset: oq_types::Offset::Open,
            });
            return;
        }
        if self.rungs >= self.max_rungs || ctx.position.0 <= 0 {
            return;
        }
        #[allow(clippy::cast_possible_truncation)]
        let trigger = (f64::from(i32::try_from(ctx.entry.0).unwrap_or(i32::MAX))
            * (1.0 - self.step * f64::from(self.rungs))) as i64;
        if ctx.tick.low.0 <= trigger {
            let qty = self.base_qty * (1 << self.rungs);
            self.rungs += 1;
            let id = self.id();
            out.push(Intent::Market {
                id,
                side: Side::Buy,
                qty: QtyLots(qty),
                offset: oq_types::Offset::Open,
            });
        }
    }

    fn name(&self) -> &str {
        "martingale-ladder"
    }
}

#[test]
fn the_margin_free_arm_reports_an_account_that_did_not_survive() {
    // The claim the project is built on, asserted rather than described.
    let ticks = crash_series(11, 400, 200, 0.5);
    let report = DeviationReport::compare(
        &config(2_000).with_margin(MarginMode::Enforced),
        MartingaleLadder::new,
        &ticks,
    );

    assert_eq!(
        report.enforced.liquidations.len(),
        1,
        "the enforced arm must be liquidated, or the example teaches nothing"
    );
    assert_eq!(report.ignored.liquidations.len(), 0);

    assert!(
        report.ignored.min_equity.0 < 0,
        "the margin-free arm must go below zero: {:?}",
        report.ignored.min_equity
    );
    assert!(
        report.ignored.final_equity.0 > report.enforced.final_equity.0 * 100,
        "the overstatement must be large enough to be undeniable: {:?} vs {:?}",
        report.ignored.final_equity,
        report.enforced.final_equity
    );
    assert!(
        !report.margin_free_result_is_honest(),
        "a run whose equity went negative is not an honest result"
    );
    assert!(
        report.fills_after_first_liquidation() > 0,
        "fills placed by a closed account are the concrete evidence"
    );

    // Pinned exactly: the documentation quotes these.
    assert_eq!(report.enforced.final_equity, Cash(6_153_200_000));
    assert_eq!(report.ignored.final_equity, Cash(2_090_811_440_000));
    assert_eq!(report.ignored.min_equity, Cash(-3_030_214_120_000));
}

#[test]
fn the_generated_market_is_stable_across_runs_and_machines() {
    // Everything above depends on this. If the generator ever changes,
    // every golden number here is meaningless rather than merely wrong.
    let ticks = crash_series(11, 400, 200, 0.5);
    assert_eq!(ticks.len(), 800);
    assert_eq!(ticks[0].last.0, 5_999_752);
    assert_eq!(ticks[799].last.0, 4_990_551);
    assert_eq!(
        ticks.iter().map(|t| t.last.0).min(),
        Some(2_958_398),
        "the low of the crash"
    );
}

/// A two-average crossover, held here rather than imported so these
/// numbers do not move when a teaching example is reworded.
///
/// The strategies pinned above hold one position for a whole run or add
/// to a losing one. Neither exercises realized profit over many round
/// trips, which is the accounting most strategies actually depend on.
struct Cross {
    fast: usize,
    slow: usize,
    hist: Vec<f64>,
    long: bool,
    next_id: u64,
}

impl Cross {
    const fn new() -> Self {
        Self {
            fast: 20,
            slow: 100,
            hist: Vec::new(),
            long: false,
            next_id: 0,
        }
    }
}

impl Strategy for Cross {
    fn name(&self) -> &str {
        "golden-cross"
    }

    #[allow(clippy::cast_precision_loss)]
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        self.hist.push(ctx.tick.last.0 as f64);
        if self.hist.len() < self.slow {
            return;
        }
        if self.hist.len() > self.slow * 2 {
            self.hist.drain(..self.slow);
        }
        let mean =
            |n: usize| -> f64 { self.hist[self.hist.len() - n..].iter().sum::<f64>() / n as f64 };
        let want = mean(self.fast) > mean(self.slow);
        if want == self.long {
            return;
        }
        self.long = want;
        self.next_id += 1;
        out.push(Intent::Market {
            id: OrderId::new(self.next_id),
            side: if want { Side::Buy } else { Side::Sell },
            qty: QtyLots(1),
            offset: if ctx.position.0 == 0 {
                oq_types::Offset::Open
            } else {
                oq_types::Offset::Close
            },
        });
    }
}

#[test]
fn a_crossover_over_a_trending_market_produces_exactly_these_numbers() {
    let ticks = series(MarketShape::trending(4_000), 42);

    // The market first, because if the generator moved, every number
    // below moved with it and one failure is easier to read than four.
    assert_eq!(ticks.len(), 4_000);
    assert_eq!(ticks[0].last.0, 6_001_106);
    assert_eq!(ticks[3_999].last.0, 7_606_004);
    assert_eq!(
        ticks.iter().map(|t| t.last.0).sum::<i64>(),
        27_068_389_777,
        "the whole path, not only its endpoints"
    );

    let result = run(
        &config(10_000).with_margin(MarginMode::Enforced),
        &mut Cross::new(),
        &ticks,
    );

    assert_eq!(result.ticks, 4_000);
    assert_eq!(result.fills.len(), 11, "fills");
    assert_eq!(result.liquidations.len(), 0, "liquidations");
    assert_eq!(result.final_equity, Cash(1_015_299_940_000), "final equity");
    assert_eq!(result.realized, Cash(15_270_260_000), "realized");
    assert_eq!(result.min_equity, Cash(999_930_460_000), "lowest equity");
    assert_eq!(
        result.fills.iter().map(|f| f.price.0).sum::<i64>(),
        72_861_962,
        "the fill prices themselves, not only how many there were"
    );
}

#[test]
fn the_same_run_twice_is_the_same_run() {
    // The generator's stability is pinned above; this is the engine's.
    // Every golden in this file assumes it and none of them check it.
    let ticks = series(MarketShape::calm(2_000), 7);
    let cfg = config(10_000).with_margin(MarginMode::Enforced);
    let a = run(&cfg, &mut Cross::new(), &ticks);
    let b = run(&cfg, &mut Cross::new(), &ticks);

    assert_eq!(a.final_equity, b.final_equity);
    assert_eq!(a.realized, b.realized);
    assert_eq!(
        a.fills.iter().map(|f| f.price.0).collect::<Vec<_>>(),
        b.fills.iter().map(|f| f.price.0).collect::<Vec<_>>()
    );
}

#[test]
fn a_generated_market_survives_the_tick_format() {
    // A golden taken in memory says nothing about one taken from a file
    // unless the file gives back what went into it.
    let ticks = series(MarketShape::calm(1_000), 3);
    let stream = oq_data::TickStream::new(1, ticks.clone()).expect("valid stream");
    let back = oq_data::TickStream::from_bytes(&stream.encode()).expect("decode");
    assert_eq!(back.ticks(), ticks.as_slice());
}

/// Print every pinned number, so updating a golden is a measurement
/// rather than a guess.
///
/// ```text
/// cargo test -p oq-examples --test golden -- --ignored --nocapture
/// ```
///
/// Ignored by default because it asserts nothing. It exists because the
/// first draft of the tests above had four of its numbers written from
/// memory and three of them were wrong — a golden guessed at guards
/// nothing and costs an afternoon.
#[test]
#[ignore = "prints the goldens rather than checking them"]
fn print_goldens() {
    let t = series(MarketShape::trending(4_000), 42);
    println!(
        "market  first {}  last {}  sum {}",
        t[0].last.0,
        t[3_999].last.0,
        t.iter().map(|x| x.last.0).sum::<i64>()
    );

    let r = run(
        &config(10_000).with_margin(MarginMode::Enforced),
        &mut Cross::new(),
        &t,
    );
    println!(
        "cross   fills {}  liq {}  final {}  realized {}  min {}  price_sum {}",
        r.fills.len(),
        r.liquidations.len(),
        r.final_equity.0,
        r.realized.0,
        r.min_equity.0,
        r.fills.iter().map(|f| f.price.0).sum::<i64>()
    );
}

// ---------------------------------------------------------------------
// The classics catalogue
// ---------------------------------------------------------------------
//
// These were not pinned when the catalogue shipped, and it cost exactly
// what this file exists to prevent. Changing `GridTrader` to move its
// ladder on fills rather than on submission — a correctness fix — moved
// the grid's levered result from 4.06 to 4.46 and its margin-free arm
// from −513.74 to −508.12. Both numbers are quoted in QUICKSTART, in two
// languages, and nothing failed. The whole suite passed and the
// documentation was simply wrong from that commit onward.
//
// So the catalogue is pinned on the same terms as everything else here:
// a failure means either the behaviour drifted or the change was
// intended, and an intended change updates these constants **and** the
// documentation in one commit.

use oq_examples::classics::{
    BollingerReversion, DonchianBreakout, DualThrust, GridTrader, MacdTrend, RsiReversion,
};

/// The example's own configuration, duplicated deliberately.
///
/// Importing it would make this test agree with the example by
/// construction, and a golden that cannot disagree with the thing it
/// pins is decoration. The one thing that must match is the margin
/// tracking, because a run without it reports no margin at all.
fn arms<S: Strategy, F: Fn() -> S>(
    build: F,
    ticks: &[oq_engine::Tick],
    balance: i64,
) -> (f64, f64) {
    let base = RunConfig::new(
        InstrumentId::new(1),
        Contract::new(10_000),
        TierTable::example_btcusdt(),
        Cash::from_units(balance),
    )
    .tracking_margin();
    let enforced = run(
        &base.clone().with_margin(MarginMode::Enforced),
        &mut build(),
        ticks,
    );
    let ignored = run(&base.with_margin(MarginMode::Ignored), &mut build(), ticks);
    (
        enforced.final_equity.0 as f64 / 100_000_000.0,
        ignored.final_equity.0 as f64 / 100_000_000.0,
    )
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.005
}

/// The numbers QUICKSTART quotes, to the cent it prints them at.
///
/// The grid specifically: it is the row the documentation singles out,
/// and the only levered run in the catalogue where the two arms differ
/// by three orders of magnitude.
#[test]
fn the_grid_levered_matches_what_the_documentation_quotes() {
    let market = crash_series(11, 3_000, 900, 0.45);
    let (enforced, free) = arms(GridTrader::new, &market, 60);
    assert!(
        close(enforced, 4.46),
        "grid, margin enforced: expected 4.46, got {enforced:.2}"
    );
    assert!(
        close(free, -508.12),
        "grid, margin-free: expected -508.12, got {free:.2}"
    );
}

/// Every levered row, so a change to any strategy is caught by the
/// strategy it changed rather than by whichever one the documentation
/// happened to quote.
#[test]
fn every_levered_row_is_pinned() {
    let market = crash_series(11, 3_000, 900, 0.45);
    let rows: [(&str, (f64, f64)); 6] = [
        ("rsi-reversion", (1.19, -213.73)),
        ("macd-trend", (1.21, -1.88)),
        ("bollinger-reversion", (1.33, -214.20)),
        ("donchian-breakout", (245.19, 245.19)),
        ("grid", (4.46, -508.12)),
        ("dual-thrust", (248.72, 248.72)),
    ];
    let actual = [
        arms(RsiReversion::new, &market, 60),
        arms(MacdTrend::new, &market, 60),
        arms(BollingerReversion::new, &market, 60),
        arms(DonchianBreakout::new, &market, 60),
        arms(GridTrader::new, &market, 60),
        arms(DualThrust::new, &market, 60),
    ];
    for ((name, (want_e, want_f)), (got_e, got_f)) in rows.iter().zip(actual) {
        assert!(
            close(*want_e, got_e),
            "{name}, margin enforced: expected {want_e:.2}, got {got_e:.2}"
        );
        assert!(
            close(*want_f, got_f),
            "{name}, margin-free: expected {want_f:.2}, got {got_f:.2}"
        );
    }
}

/// The catalogue's headline finding, as a property rather than a
/// number: unlevered, the two arms agree for every strategy.
///
/// Pinned separately from the values because it is the sentence the
/// documentation actually makes — *a margin model is invisible until
/// leverage is real* — and it would survive every constant above
/// changing. If it ever fails, the claim is wrong rather than stale.
#[test]
fn unlevered_the_two_arms_agree_for_all_six() {
    let market = crash_series(11, 3_000, 900, 0.45);
    let all = [
        ("rsi-reversion", arms(RsiReversion::new, &market, 10_000)),
        ("macd-trend", arms(MacdTrend::new, &market, 10_000)),
        (
            "bollinger-reversion",
            arms(BollingerReversion::new, &market, 10_000),
        ),
        (
            "donchian-breakout",
            arms(DonchianBreakout::new, &market, 10_000),
        ),
        ("grid", arms(GridTrader::new, &market, 10_000)),
        ("dual-thrust", arms(DualThrust::new, &market, 10_000)),
    ];
    for (name, (enforced, free)) in all {
        assert!(
            close(enforced, free),
            "{name} unlevered: the arms parted, {enforced:.2} vs {free:.2}. \
             Either a strategy now liquidates without leverage, or the claim \
             that a margin model is invisible until leverage is real is wrong."
        );
    }
}
