//! The same two positions, on one balance and on two.
//!
//! ```text
//! cargo run --release -p oq-examples --example isolated_margin
//! ```
//!
//! Cross margin and isolated margin are usually described as a risk
//! preference. They are also the difference between a kernel that can be
//! sharded and one that cannot, which is why this workspace has a type
//! for each rather than a flag:
//!
//! - **Cross** — one `State` holding several instruments. One balance
//!   behind every position, so a loss on one is paid for by a gain on
//!   another, and the account survives what either alone would not.
//! - **Isolated** — `Shards`, one kernel each. Nothing shared, so a
//!   position that exhausts its own balance is liquidated while the
//!   others carry on. This is the arrangement `FR-CORE-6` describes,
//!   and the reason is not performance: a balance two instruments draw
//!   on *is* shared mutable state.
//!
//! # What this shows
//!
//! Two instruments, opposite directions, one rising. Cross margin nets
//! them and finishes near flat. Isolated margin cannot: the losing side
//! runs out of its own money while the winning side keeps its profit,
//! and the account ends holding a liquidation it did not have to have.
//!
//! Neither is the safer choice in general — that is the point of having
//! both. Isolated bounds what one instrument can cost; cross lets a
//! hedge actually hedge. A strategy that assumes one and runs on the
//! other is mispriced in a way no fill report shows.

use oq_core::{Event, Kernel, Shards, State};
use oq_engine::Tick;
use oq_examples::money;
use oq_margin::{Contract, MarginTier, TierTable};
use oq_types::{Cash, InstrumentId, Offset, OrderId, QtyLots, Ratio, Side, Stamp};

const FIRST: u32 = 1;
const SECOND: u32 = 2;
/// Chosen so the two arrangements separate: half of it does not cover
/// the losing side's drawdown, and all of it does. A larger balance and
/// both survive; a smaller one and neither does. The interesting case
/// is the one in between, and it is narrow — which is itself the point.
const BALANCE: i64 = 60;
const SIZE: i64 = 40;

fn table() -> TierTable {
    TierTable::new(vec![MarginTier {
        max_notional: Cash(i64::MAX),
        rate: Ratio::from_percent(2),
        amount: Cash::ZERO,
    }])
    .expect("single bracket")
}

fn tick(at: i64, price: i64) -> Tick {
    Tick::trades_only(Stamp::synthetic(at), price, price, price)
}

fn buy(instrument: u32, id: u64, at: i64, qty: i64) -> Event {
    Event::Submit {
        instrument: Some(InstrumentId::new(instrument)),
        id: OrderId::new(id),
        side: if qty > 0 { Side::Buy } else { Side::Sell },
        price: None,
        qty: QtyLots(qty.abs()),
        offset: Offset::Open,
        stamp: Stamp::synthetic(at),
    }
}

/// The price path each instrument takes: the first rises, the second is
/// flat, and the position taken on each is opposite.
fn prices(step: i64) -> (i64, i64) {
    (100_000 + step * 2_000, 100_000)
}

fn main() {
    // ---- Cross margin: one account, two holdings ----
    let mut cross = State::new(
        InstrumentId::new(FIRST),
        Contract::new(1_000),
        table(),
        Cash::from_units(BALANCE),
    );
    cross.open_holding(InstrumentId::new(SECOND), Contract::new(1_000), table());
    let mut cross = Kernel::new(cross);

    // ---- Isolated margin: two accounts, half the balance each ----
    let mut isolated = Shards::new(vec![
        State::new(
            InstrumentId::new(FIRST),
            Contract::new(1_000),
            table(),
            Cash::from_units(BALANCE / 2),
        ),
        State::new(
            InstrumentId::new(SECOND),
            Contract::new(1_000),
            table(),
            Cash::from_units(BALANCE / 2),
        ),
    ]);

    let named = |instrument: u32, at: i64, price: i64| Event::Tick {
        instrument: Some(InstrumentId::new(instrument)),
        tick: tick(at, price),
    };

    // Short the instrument that rises, long the one that does not.
    for step in 0..40 {
        let at = (step + 1) * 1_000;
        let (a, b) = prices(step);

        for e in [named(FIRST, at, a), named(SECOND, at, b)] {
            cross.apply(&e);
            isolated.apply(&e);
        }

        if step == 0 {
            for e in [buy(FIRST, 1, at, -SIZE), buy(SECOND, 2, at, SIZE)] {
                cross.apply(&e);
                isolated.apply(&e);
            }
        }
    }

    let cross_equity = cross.summary().equity;
    let first = isolated
        .shard(InstrumentId::new(FIRST))
        .expect("held")
        .summary();
    let second = isolated
        .shard(InstrumentId::new(SECOND))
        .expect("held")
        .summary();

    println!("opening balance   {}", money(Cash::from_units(BALANCE)));
    println!("position          short {SIZE} of one, long {SIZE} of the other");
    println!("move              the short side rises 80%, the long side is flat");
    println!();
    println!("cross margin      one balance behind both");
    println!("  equity          {}", money(cross_equity));
    println!(
        "  position        {:?} and {:?}",
        cross
            .state()
            .holding_of(InstrumentId::new(FIRST))
            .map(|h| h.qty),
        cross
            .state()
            .holding_of(InstrumentId::new(SECOND))
            .map(|h| h.qty),
    );
    println!();
    println!("isolated margin   half the balance behind each");
    println!(
        "  first  equity   {}   position {:?}",
        money(first.equity),
        first.qty
    );
    println!(
        "  second equity   {}   position {:?}",
        money(second.equity),
        second.qty
    );
    println!(
        "  together        {}",
        money(first.equity.add(second.equity))
    );

    println!();
    let liquidated = first.qty == QtyLots::ZERO;
    if liquidated {
        println!(
            "The losing side ran out of its own money and was closed. The \
             winning side kept its position and its profit, and could not \
             lend any of it to the other — which is what isolation means, \
             and is the whole reason a shard shares nothing."
        );
    } else {
        println!(
            "Both sides survived on half the balance each. The arrangements \
             differ in what they permit, not in what they always do — this \
             move was not large enough to separate them, which is itself \
             worth seeing before assuming isolation is always the stricter \
             choice."
        );
    }

    println!();
    println!(
        "Neither is safer in general. Isolation bounds what one instrument \
         can cost; cross lets a hedge actually hedge. A strategy that \
         assumes one and runs on the other is mispriced in a way no fill \
         report shows."
    );
}
