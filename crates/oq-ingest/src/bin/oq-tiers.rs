//! `oq-tiers` — run one strategy over one archive at two fidelity tiers.
//!
//! ```text
//! oq-tiers --archive /data/binance-perp/BTCUSDT --day 2026-08-19
//! ```
//!
//! The `book_tiers` example makes this comparison on a **generated**
//! book, which is consistent with its prices rather than measured beside
//! them. This makes it on the archive: the same depth the venue
//! published, replayed into the matcher that reads it.
//!
//! It answers one question. **How many of a resting strategy's backtest
//! fills would the queue never have reached?** L0 fills a resting order
//! the moment the price arrives; the venue fills it when everything
//! ahead of it has traded. The gap is not a haircut on the return -- it
//! is trades that did not happen, and a P&L computed from them is about
//! a market that was not there.
//!
//! # What it is not
//!
//! Not a strategy worth running. The one below quotes one lot a tick
//! under the market and re-prices as it moves, which is the simplest
//! thing that makes the queue decide the outcome. Its P&L is not a
//! finding; the fill counts either side of it are.
//!
//! # What it has measured
//!
//! Ten hours spread across four days of captured BTCUSDT, 1.3 million
//! depth updates, none refused by the sequencing check: a bid resting
//! at the touch filled **13,972 times at L0 and 3,140 at L2**. No hour
//! kept more than 44.6% of L0's fills or fewer than 12.5%.
//!
//! The spread matters as much as the total. A backtest's error here is
//! not a constant to subtract; it depends on how thick the book was that
//! hour, which is exactly the thing a tick file does not carry.
//!
//! # The queue here still only depletes on trades
//!
//! No MBP feed can tell a cancellation ahead of you from one behind, so
//! the queue shrinks only when something trades at that price. Real
//! queues also shorten when the people in front give up, so every wait
//! here is an upper bound and every fill count a lower one. The
//! direction is deliberate: an order fills here no earlier than it would
//! have in life.

use std::path::PathBuf;
use std::process::ExitCode;

use oq_backtest::{
    Context, Intent, Observation, RunConfig, RunResult, Strategy, Tier, run_observations,
};
use oq_engine::{Delay, Impact, Latency, Policy, QueueAhead};
use oq_ingest::batches::{hours, load_hour};
use oq_ingest::{Aggregator, Report, Source, fold_into_observations};
use oq_l2feed::depth::Scales;
use oq_margin::{Contract, MarginTier, TierTable};
use oq_types::{Cash, InstrumentId, Offset, OrderId, PriceTicks, QtyLots, Ratio, Side};

const USAGE: &str = "\
oq-tiers — run one strategy over one archive at two fidelity tiers

USAGE:
    oq-tiers --archive <DIR> --day <YYYY-MM-DD> [OPTIONS]

OPTIONS:
    --archive <DIR>      Instrument directory, e.g. .../binance-perp/BTCUSDT
    --day <DATE>         UTC day to run
    --window-ms <N>      Tick window in milliseconds [default: 1000]
    --venue <NAME>       Venue whose instrument table to use [default: binance-perp]
    --symbol <SYMBOL>    Override the symbol inferred from the archive path
    --help
";

/// Quotes one lot at the best bid, re-pricing as it moves.
///
/// A maker on purpose: a taker would show almost no queue effect and
/// the comparison would look like a rounding difference.
///
/// **At the touch, not a tick under it.** A price the book does not
/// display has nothing resting on it, so an order joining it is first
/// in the queue -- a true reading, and one that makes L2 answer as L0
/// and measures nothing. Real quoting joins the touch and takes its
/// place in the line, which is where the tiers disagree.
struct RestingBid {
    next_id: u64,
    working: Option<(OrderId, i64)>,
}

impl Strategy for RestingBid {
    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        // A window with no quote cannot be joined. Falling back to the
        // last trade would quote into a price the book may not show,
        // which is first-in-queue by construction and measures nothing.
        if ctx.tick.bid.0 <= 0 {
            return;
        }
        let want = ctx.tick.bid.0;
        match self.working {
            Some((id, at)) if at != want => {
                out.push(Intent::Cancel(id));
                self.working = None;
            }
            Some(_) => {}
            None => {
                let id = OrderId(self.next_id);
                self.next_id += 1;
                out.push(Intent::Limit {
                    id,
                    side: Side::Buy,
                    price: PriceTicks(want),
                    qty: QtyLots(1),
                    offset: Offset::Open,
                });
                self.working = Some((id, want));
            }
        }
    }

    fn on_fill(&mut self, _f: &oq_types::Fill, _c: &Context, _o: &mut Vec<Intent>) {
        self.working = None;
    }

    fn name(&self) -> &str {
        "resting-bid"
    }
}

fn strategy() -> RestingBid {
    RestingBid {
        next_id: 1,
        working: None,
    }
}

fn config(instrument: InstrumentId, tier: Tier) -> RunConfig {
    let table = TierTable::new(vec![MarginTier {
        max_notional: Cash(i64::MAX),
        rate: Ratio::from_percent(1),
        amount: Cash::ZERO,
    }])
    .expect("single bracket");
    RunConfig::new(
        instrument,
        Contract::new(10_000),
        table,
        Cash::from_units(10_000_000),
    )
    .at_tier(tier)
}

/// FNV-1a, matching `oq-ingest` so the two agree on an instrument id.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let value = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    let (Some(archive), Some(day)) = (value("--archive"), value("--day")) else {
        eprintln!("oq-tiers: --archive and --day are required\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    let archive = PathBuf::from(archive);
    let venue_id = value("--venue").unwrap_or_else(|| "binance-perp".to_string());
    let Some(symbol) = value("--symbol").or_else(|| {
        archive
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
    }) else {
        eprintln!(
            "oq-tiers: cannot tell which symbol {} holds; pass --symbol",
            archive.display()
        );
        return ExitCode::FAILURE;
    };
    let Some(venue) = oq_l2feed::venue::by_id(&venue_id) else {
        eprintln!("oq-tiers: unknown venue {venue_id:?}");
        return ExitCode::FAILURE;
    };
    let Some(instrument) = venue.instrument(&symbol) else {
        eprintln!(
            "oq-tiers: no instrument definition for {symbol:?} on {venue_id}. \
             Quoting precision cannot be guessed: a wrong scale rescales every \
             price without failing, so this stops rather than assume one."
        );
        return ExitCode::FAILURE;
    };
    let scales = Scales {
        price: u32::from(instrument.price_scale),
        qty: u32::from(instrument.qty_scale),
    };
    let window_ms: i64 = value("--window-ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);

    let hours = hours(&archive, &day);
    if hours.is_empty() {
        eprintln!("oq-tiers: nothing to run under {}", archive.display());
        return ExitCode::FAILURE;
    }

    // Built once and replayed twice, so the two runs differ in the
    // matcher and in nothing else. It costs the memory of a day's
    // observations; the alternative is converting twice and hoping the
    // two conversions agreed.
    let mut agg = match Aggregator::new(window_ms * 1_000_000) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("oq-tiers: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut report = Report::default();
    let mut stream: Vec<Observation> = Vec::new();
    let mut seeded = false;
    for hour in &hours {
        let loaded = match load_hour(&archive, &day, hour) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("oq-tiers: {e}");
                return ExitCode::FAILURE;
            }
        };
        if loaded.is_empty() {
            continue;
        }
        let sources: Vec<Source<'_>> = loaded
            .iter()
            .map(|b| Source {
                records: &b.records,
                stream: b.stream,
            })
            .collect();
        for obs in fold_into_observations(venue.as_ref(), &sources, scales, &mut agg, &mut report) {
            // Seed the book from the first update that can be placed.
            //
            // The archive holds no REST snapshot -- capture records the
            // socket, and the snapshot is a separate request nobody
            // made. Bootstrapping from the first update is what
            // `oq-book-check` does to prove an archive reconstructs, and
            // it has a cost this run inherits: every level that existed
            // before the capture began is invisible, so the queue reads
            // **shorter** than it was until trading has rebuilt the
            // levels. That understates the queue, which overstates the
            // fills -- the same direction L0 errs in, just less of it.
            if !seeded && let Observation::Depth(u) = &obs {
                stream.push(Observation::Snapshot {
                    update_id: u.first_id.saturating_sub(1),
                    bids: Vec::new(),
                    asks: Vec::new(),
                });
                seeded = true;
            }
            stream.push(obs);
        }
    }
    stream.extend(agg.flush().into_iter().map(Observation::Tick));

    let ticks = stream
        .iter()
        .filter(|o| matches!(o, Observation::Tick(_)))
        .count();
    let updates = stream.len() - ticks;
    println!("archive       {}", archive.display());
    println!("day           {day}");
    println!("window        {window_ms} ms");
    println!("ticks         {ticks}");
    println!("depth updates {updates}");
    if ticks == 0 {
        eprintln!("\noq-tiers: no ticks; the trade stream may be missing");
        return ExitCode::FAILURE;
    }

    let id = InstrumentId::new(
        u32::try_from(fnv1a(format!("{venue_id}:{symbol}").as_bytes()) & 0xffff_ffff).unwrap_or(1),
    );
    // The policy is transparent: latency and impact off, queue unnamed.
    // Anything else would move both runs and obscure the one difference
    // being measured.
    let policy = Policy {
        queue: QueueAhead::None,
        latency: Latency {
            entry: Delay::Fixed(oq_types::Nanos(0)),
            response: Delay::Fixed(oq_types::Nanos(0)),
        },
        impact: Impact { coefficient: 0 },
    };

    // L0 is given the ticks only. Handing it depth would be counted as
    // ignored and say nothing new; the point of the arm is the matcher.
    let ticks_only: Vec<Observation> = stream
        .iter()
        .filter(|o| matches!(o, Observation::Tick(_)))
        .cloned()
        .collect();
    let l0 = run_observations(&config(id, Tier::L0), &mut strategy(), ticks_only);
    let l2 = run_observations(&config(id, Tier::L2(policy)), &mut strategy(), stream);

    println!();
    println!(
        "{:<12} {:>9} {:>9} {:>16}",
        "", "fills", "of L0", "final equity"
    );
    row("L0", &l0, l0.fills.len());
    row("L2", &l2, l0.fills.len());

    println!();
    println!(
        "depth applied {}   refused {}   ignored {}",
        l2.depth_applied, l2.depth_refused, l2.depth_unused
    );
    println!(
        "  the book was bootstrapped from the first update, not a REST \
         snapshot, so levels resting before the capture began are absent \
         and the queue reads shorter than it was early on"
    );
    if l2.depth_refused > 0 {
        println!(
            "  refusals are sequence breaks: messages were lost, and the book \
             was left as it was rather than guessing. A queue measured after \
             one reads shorter than it was."
        );
    }

    println!();
    let lost = l0.fills.len().saturating_sub(l2.fills.len());
    if l0.fills.is_empty() {
        println!("L0 filled nothing, so there is nothing to compare.");
    } else {
        println!(
            "{lost} of L0's {} fills did not survive the queue — {:.1}%. Those \
             are not fills worth less; they are trades the queue never reached.",
            l0.fills.len(),
            100.0 * lost as f64 / l0.fills.len() as f64
        );
    }
    ExitCode::SUCCESS
}

fn row(label: &str, r: &RunResult, baseline: usize) {
    let share = if baseline == 0 {
        0.0
    } else {
        100.0 * r.fills.len() as f64 / baseline as f64
    };
    println!(
        "{label:<12} {:>9} {:>8.1}% {:>16}",
        r.fills.len(),
        share,
        r.final_equity.0 as f64 / 100.0,
    );
}
