//! `oq-trade-check` — prove a captured trade stream is complete.
//!
//! A hash proves that what was stored is what was received. It cannot
//! prove that everything sent arrived, and those are different claims:
//! a capture that silently dropped a third of the trades would pass
//! every integrity check in the pipeline.
//!
//! Trade ids answer it. The venue assigns them in order, so an unbroken
//! run means nothing was missed and no id twice means nothing was
//! recorded twice. Depth streams carry their own chain — `U`, `u` and
//! `pu` — which `oq-book-check` follows; trades have no such chain, and
//! until this existed their completeness was assumed rather than shown.
//!
//! ```text
//! oq-trade-check archive/binance-perp/BTCUSDT/trade/2026-08-16/11.oqcap
//! oq-trade-check --venue okx-swap archive/okx-swap/BTCUSDT/trade/2026-08-16.oqcap
//! ```
//!
//! The venue reads its own ids: one writes a bare `"t":12345`, the other
//! a quoted `"tradeId":"12345"`, and a reader written for either finds
//! none at all in the other. That is not an error — it is an empty
//! check, and an empty check here reports the same shape as a passing
//! one. The venue is taken from the archive path when it is not given,
//! because that path already names it.
//!
//! Exits non-zero when ids are missing or repeated, so it belongs in
//! whatever runs after a capture.

use std::process::ExitCode;

use oq_l2feed::frame::{Kind, decode_all};

/// The venue segment of an archive path: `<root>/<venue>/<symbol>/<stream>/…`.
///
/// Inferred rather than required because every archive path holds it,
/// and a flag that is nearly always the same as something already on the
/// command line is a flag that gets forgotten and then defaulted wrongly.
fn venue_from_path(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let stream_idx = parts.iter().rposition(|p| {
        matches!(
            *p,
            "depth" | "bookTicker" | "trade" | "forceOrder" | "markPrice" | "fundingRate"
        )
    })?;
    parts
        .get(stream_idx.checked_sub(2)?)
        .map(|s| (*s).to_string())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let Some(path) = args.iter().find(|a| !a.starts_with("--")).cloned() else {
        eprintln!(
            "oq-trade-check: missing path\n\nUSAGE:\n    oq-trade-check [--venue <NAME>] <FILE.oqcap>"
        );
        return ExitCode::FAILURE;
    };

    let venue_id = flag("--venue")
        .or_else(|| venue_from_path(&path))
        .unwrap_or_else(|| "binance-perp".to_string());
    let Some(venue) = oq_l2feed::venue::by_id(&venue_id) else {
        eprintln!(
            "oq-trade-check: unknown venue {venue_id:?}; known: {}",
            oq_l2feed::venue::known_ids().join(", ")
        );
        return ExitCode::FAILURE;
    };
    println!("venue           {venue_id}");

    let bytes = oq_l2feed::archive::read(&path).expect("read");
    let (records, torn) = decode_all(&bytes).expect("decode");

    let mut ids: Vec<u64> = records
        .iter()
        .filter(|r| r.kind == Kind::Payload)
        .flat_map(|r| venue.trade_ids(&r.payload))
        .collect();
    let total = ids.len();
    ids.sort_unstable();

    let dupes = ids.windows(2).filter(|w| w[0] == w[1]).count();
    let gaps: Vec<(u64, u64)> = ids
        .windows(2)
        .filter(|w| w[1] > w[0] + 1)
        .map(|w| (w[0], w[1]))
        .collect();
    let missing: u64 = gaps.iter().map(|(a, b)| b - a - 1).sum();
    let span = ids.last().unwrap_or(&0) - ids.first().unwrap_or(&0) + 1;

    println!("records         {}  (torn {torn})", records.len());
    println!("with a trade id {total}");
    println!(
        "id range        {} .. {}",
        ids.first().unwrap_or(&0),
        ids.last().unwrap_or(&0)
    );
    println!("ids in range    {span}");
    println!("received / due  {total} / {span}");
    println!("repeated ids    {dupes}");
    println!("gaps in ids     {}", gaps.len());
    println!("trades missing  {missing}");

    // The order flow, which the same records carry and nothing read
    // until now. It is here rather than in its own command because the
    // file is already decoded and the venue already parsed: a second
    // command would re-read the archive to answer a question this one
    // has the bytes for.
    // Any scale parses the side correctly: the aggressor is a flag, not
    // a number, so a wrong precision would rescale a price this does not
    // read rather than change which way the trade went.
    let scales = oq_l2feed::depth::Scales::default();
    let mut flow: Vec<oq_stats::Aggressor> = Vec::new();
    let mut stamps: Vec<i64> = Vec::new();
    for r in records.iter().filter(|r| r.kind == Kind::Payload) {
        let Some(t) = venue.parse_trade(&r.payload, scales) else {
            continue;
        };
        let side = match t.aggressor {
            Some(oq_types::Side::Buy) => oq_stats::Aggressor::Buy,
            Some(oq_types::Side::Sell) => oq_stats::Aggressor::Sell,
            None => continue,
        };
        flow.push(side);
        stamps.push(r.exch_ts);
    }

    println!();
    if flow.len() < total {
        println!(
            "with a side    {} of {total} — the rest carry no aggressor",
            flow.len()
        );
    }

    // Raw trades first: this is what a queue faces, one at a time.
    match oq_stats::OrderFlow::measure(&flow, 20) {
        Ok(f) => print!("trades — {}", f.render()),
        Err(e) => println!("trade flow      not measurable: {e}"),
    }

    // Then collapsed to orders, which is the quantity the literature's
    // coefficient is over. One order crossing several resting ones
    // produces several trades on the same side in the same millisecond,
    // and counting those separately reports one decision as a run.
    println!();
    let orders = oq_stats::as_orders(&flow, &stamps);
    println!(
        "collapsed      {} trades into {} orders ({:.1} trades each)",
        flow.len(),
        orders.len(),
        flow.len() as f64 / orders.len().max(1) as f64
    );
    match oq_stats::OrderFlow::measure(&orders, 20) {
        Ok(f) => print!("orders — {}", f.render()),
        Err(e) => println!("order flow      not measurable: {e}"),
    }
    for (a, b) in gaps.iter().take(10) {
        println!("    gap {a} -> {b}, {} missing", b - a - 1);
    }

    // Nothing to check is not the same as nothing wrong. Pointed at a
    // depth file — which carries no trade ids at all — the arithmetic
    // above finds zero gaps among zero ids and would call it complete,
    // which is the worst answer available: an empty check wearing the
    // shape of a passing one.
    if total == 0 {
        println!();
        println!(
            "verdict: NOT A TRADE STREAM — no trade ids found for {venue_id}, so nothing \
             here was checked (wrong file, or the wrong --venue for it)"
        );
        return ExitCode::FAILURE;
    }

    if missing > 0 || dupes > 0 {
        println!();
        println!("verdict: INCOMPLETE — {missing} missing, {dupes} repeated");
        return ExitCode::FAILURE;
    }
    println!();
    println!("verdict: COMPLETE — every id the venue issued in this range is present, once");
    ExitCode::SUCCESS
}
