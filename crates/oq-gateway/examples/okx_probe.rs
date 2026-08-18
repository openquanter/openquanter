//! What can be checked against OKX without an account.
//!
//! ```text
//! cargo run --release -p oq-gateway --example okx_probe
//! ```
//!
//! Instrument listings and mark prices are public, so the half of the
//! adapter that reads them can be proved against the real venue by
//! anyone, with no credentials and nothing at risk. This runs that half
//! and prints what it got.
//!
//! It is an example rather than a test because it needs the internet,
//! and a test suite that fails when a café's wifi drops is a test suite
//! people learn to ignore. The parsing itself is tested offline against
//! a listing captured from this endpoint.
//!
//! # What it does not prove
//!
//! Anything signed: placing an order, cancelling one, reading an
//! account, or the private stream. Those need an OKX account and demo
//! trading, and until somebody runs them the order path on this venue is
//! written rather than working.

use oq_gateway::Credentials;
use oq_gateway::exec::{Endpoint, decimal};
use oq_gateway::okx::Okx;

fn main() {
    // Public endpoints are unsigned, so these are placeholders and never
    // leave the process. A reader should not have to find credentials to
    // run the part that does not need them.
    let creds = Credentials::new("public", "unsigned")
        .with_passphrase("unused")
        .expect("a placeholder triple");
    let okx = Okx::at(Endpoint::Live, creds);

    println!("okx listings, read from the venue rather than from a table");
    println!(
        "  {:<16} {:>12}  {:>8}  {:>8}  {:>8}  {:>12}",
        "instrument", "contract", "tick", "lot", "min", "mark"
    );

    let mut failures = 0;
    for id in ["BTC-USDT-SWAP", "ETH-USDT-SWAP", "SOL-USDT-SWAP"] {
        let listing = match okx.listing(id) {
            Ok(l) => l,
            Err(e) => {
                println!("  {id:<16} FAILED: {e}");
                failures += 1;
                continue;
            }
        };
        let mark = match okx.mark_price(id, listing.price_scale) {
            Ok(p) => decimal(p.0, listing.price_scale),
            Err(e) => {
                failures += 1;
                format!("FAILED: {e}")
            }
        };
        println!(
            "  {id:<16} {:>12}  {:>8}  {:>8}  {:>8}  {:>12}",
            decimal(listing.contract_value, 8),
            decimal(listing.price_tick, listing.price_scale),
            decimal(listing.lot_size, listing.size_scale),
            decimal(listing.min_size, listing.size_scale),
            mark,
        );
    }

    println!();
    println!("Three contracts, three different sizes: a contract is 0.01 BTC, 0.1 ETH");
    println!("and 1 SOL. An order sized in coins and sent as `sz` would be out by a");
    println!("hundred, ten, and not at all — which is why one of those is the bug that");
    println!("gets found late.");

    if failures > 0 {
        println!();
        println!("{failures} read(s) failed. If the network is fine, the venue changed a");
        println!("field name and the adapter needs to be told.");
        std::process::exit(1);
    }
}
