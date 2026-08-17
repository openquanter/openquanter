//! A venue has to know which deployment it is.
//!
//! The execution side learned this first, for the reason that a string
//! wrong by one character is production. Market data needed it for a
//! different reason: a strategy trading on a test deployment must see
//! that deployment's prices. Pointed at production's, it gets a market
//! it cannot trade in — orders priced against somewhere else, which
//! never fill and never explain why.

use oq_l2feed::venue::{self, Deployment};

#[test]
fn the_same_venue_reaches_two_different_hosts() {
    let live = venue::by_id_at("binance-perp", Deployment::Live).expect("live exists");
    let test = venue::by_id_at("binance-perp", Deployment::Testnet).expect("testnet exists");

    let live_url = live.transport(&live.streams("BTCUSDT")[0]).url;
    let test_url = test.transport(&test.streams("BTCUSDT")[0]).url;

    assert_ne!(live_url, test_url);
    assert!(live_url.contains("fstream.binance.com"), "{live_url}");
    assert!(test_url.contains("stream.binancefuture.com"), "{test_url}");
    // Same topic on both: only the host moves, so a test run subscribes
    // to what a live run would.
    assert!(live_url.ends_with("btcusdt@depth@0ms"), "{live_url}");
    assert!(test_url.ends_with("btcusdt@depth@0ms"), "{test_url}");
}

#[test]
fn the_polled_streams_follow_the_same_deployment() {
    // Mark price decides liquidation. Reading production's while
    // trading a testnet account would misprice every margin
    // calculation in a way that looks like a market that moved.
    let test = venue::by_id_at("binance-perp", Deployment::Testnet).expect("testnet exists");
    let poll = &test.polls("BTCUSDT")[0];
    assert!(
        poll.url.contains("testnet.binancefuture.com"),
        "{}",
        poll.url
    );
}

#[test]
fn a_venue_with_no_test_deployment_refuses_rather_than_answering_with_production() {
    // The worst possible answer to "give me the testnet" is a
    // production endpoint: the caller believes it is testing, and it is
    // not. This venue's simulated trading needs a header this adapter
    // does not send, so it says no.
    assert!(venue::by_id_at("okx-swap", Deployment::Testnet).is_none());
    assert!(venue::by_id_at("okx-swap", Deployment::Live).is_some());
}

#[test]
fn the_plain_registry_still_means_production() {
    // Capture calls this, and its behaviour must not have changed.
    let a = venue::by_id("binance-perp").expect("exists");
    let b = venue::by_id_at("binance-perp", Deployment::Live).expect("exists");
    assert_eq!(
        a.transport(&a.streams("BTCUSDT")[0]).url,
        b.transport(&b.streams("BTCUSDT")[0]).url
    );
}

#[test]
fn an_unknown_venue_is_none_on_every_deployment() {
    for d in [Deployment::Live, Deployment::Testnet] {
        assert!(venue::by_id_at("not-a-venue", d).is_none());
    }
}

#[test]
fn a_read_timeout_keeps_its_kind_so_a_poller_can_tell() {
    // Not reachable without a socket, so this pins the classifier the
    // decision rests on rather than the path itself. A caller polling
    // with a short timeout has to distinguish "nothing this instant"
    // from "the connection is gone"; erasing the kind makes a quiet
    // contract look like a broken one and costs a reconnection per
    // quiet interval.
    use std::io;
    for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::TimedOut] {
        let e = tungstenite::Error::Io(io::Error::new(kind, "timed out"));
        assert!(
            oq_l2feed::ws::is_read_timeout_for_test(&e),
            "{kind:?} is a read timeout"
        );
    }
    let broken = tungstenite::Error::Io(io::Error::new(io::ErrorKind::BrokenPipe, "gone"));
    assert!(
        !oq_l2feed::ws::is_read_timeout_for_test(&broken),
        "a broken pipe is not a timeout"
    );
}
