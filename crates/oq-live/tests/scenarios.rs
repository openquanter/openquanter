//! The corpus, applied to the things that claim to survive it.
//!
//! Each component here documents a behaviour for something going wrong.
//! Those behaviours are tested one case at a time, by a test that builds
//! the one case it is about. This drives them through the whole corpus
//! instead, which is where the twentieth occurrence and the combination
//! nobody wrote a test for live.
//!
//! A failure here prints the scenario name and its seed, and that pair is
//! enough to reproduce it anywhere.

use oq_gateway::OrderUpdate;
use oq_live::Book;
use oq_sim::{Fault, Scenario, corpus, distort};

fn fill(client_id: &str, trade_id: i64) -> OrderUpdate {
    OrderUpdate {
        symbol: "ETHUSDT".into(),
        client_id: client_id.into(),
        venue_id: trade_id,
        status: "FILLED".into(),
        last_qty: "0.001".into(),
        cumulative_qty: "0.001".into(),
        last_price: "3000.00".into(),
        side: "BUY".to_string(),
        position_side: "BOTH".into(),
        maker: false,
        trade_id: Some(trade_id),
        event_ms: trade_id,
    }
}

/// Fills with distinct trade ids, as a venue would report them.
fn stream(n: i64) -> Vec<OrderUpdate> {
    (1..=n).map(|i| fill(&format!("oq-{i}"), i)).collect()
}

#[test]
fn no_scenario_makes_the_book_count_a_fill_twice() {
    // The invariant that has to hold under every distortion: a fill is
    // applied once. Duplicates are the fault the deduplication table
    // exists for, and reorder and drop must not let one through by
    // arriving in an order the table did not expect.
    let events = stream(30);
    for s in corpus() {
        let mut book = Book::owning("oq");
        let mut applied = 0_u64;
        for u in distort(&s, &events) {
            if book.apply(&u) {
                applied += 1;
            }
        }
        let distinct: std::collections::HashSet<i64> = distort(&s, &events)
            .iter()
            .filter_map(|u| u.trade_id)
            .collect();
        assert_eq!(
            applied as usize,
            distinct.len(),
            "{} (seed {:#x}): applied {applied} of {} distinct trade ids — {}",
            s.name,
            s.seed,
            distinct.len(),
            s.about
        );
    }
}

#[test]
fn duplicates_are_counted_wherever_the_corpus_puts_them() {
    // A redelivery must be noticed, not merely survived. A book that
    // silently dropped them would pass the test above and hide a stream
    // that is redelivering steadily.
    let events = stream(30);
    let s = corpus()
        .into_iter()
        .find(|s| s.name == "redelivery-storm")
        .expect("the corpus has one");
    let mut book = Book::owning("oq");
    for u in distort(&s, &events) {
        book.apply(&u);
    }
    assert!(
        book.duplicates() > 0,
        "{} (seed {:#x}) inserted duplicates and none were counted",
        s.name,
        s.seed
    );
}

#[test]
fn a_gap_loses_events_and_the_book_does_not_invent_them() {
    // The honest outcome. A book that reported the missing fills would be
    // reporting a position the account does not have.
    let events = stream(30);
    let s = corpus()
        .into_iter()
        .find(|s| s.name == "feed-gap")
        .expect("the corpus has one");
    let received = distort(&s, &events);
    assert!(received.len() < events.len(), "the gap removed nothing");

    let mut book = Book::owning("oq");
    let mut applied = 0;
    for u in &received {
        if book.apply(u) {
            applied += 1;
        }
    }
    assert_eq!(
        applied,
        received.len(),
        "{} (seed {:#x}): what arrived is what was applied",
        s.name,
        s.seed
    );
}

#[test]
fn another_systems_events_are_refused_under_every_scenario() {
    // The ownership filter has to hold under distortion too: a reorder
    // that put a foreign event first must not make it look like ours.
    let mut events = stream(20);
    for i in 0..5 {
        events.push(fill(&format!("x-someone-else-{i}"), 1000 + i));
    }
    for s in corpus() {
        let mut book = Book::owning("oq");
        for u in distort(&s, &events) {
            book.apply(&u);
        }
        let foreign_present = distort(&s, &events)
            .iter()
            .filter(|u| !u.client_id.starts_with("oq"))
            .count();
        assert_eq!(
            book.foreign() as usize,
            foreign_present,
            "{} (seed {:#x}): {} foreign events present, {} refused",
            s.name,
            s.seed,
            foreign_present,
            book.foreign()
        );
    }
}

#[test]
fn a_scenario_that_truncates_everything_is_still_survivable() {
    // Disconnect at position zero. A component that needed at least one
    // event would fail here, and the start of a session is exactly when a
    // connection is most likely to drop.
    let events = stream(10);
    let s = Scenario {
        name: "immediate-disconnect",
        about: "the socket ends before anything arrives at all",
        seed: 3,
        faults: vec![Fault::Disconnect, Fault::Disconnect, Fault::Disconnect],
    };
    let mut book = Book::owning("oq");
    for u in distort(&s, &events) {
        book.apply(&u);
    }
    assert_eq!(book.working(), 0);
    assert_eq!(book.duplicates(), 0);
}
