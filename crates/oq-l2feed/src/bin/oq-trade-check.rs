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
//! ```
//!
//! Exits non-zero when ids are missing or repeated, so it belongs in
//! whatever runs after a capture.

use std::process::ExitCode;

use oq_l2feed::frame::{Kind, decode_all};

fn id_of(payload: &[u8]) -> Option<u64> {
    let i = payload.windows(4).position(|w| w == br#""t":"#)? + 4;
    let digits: Vec<u8> = payload[i..]
        .iter()
        .copied()
        .take_while(u8::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return None;
    }
    core::str::from_utf8(&digits).ok()?.parse().ok()
}

fn main() -> ExitCode {
    let path = std::env::args().nth(1).expect("path");
    let bytes = std::fs::read(&path).expect("read");
    let (records, torn) = decode_all(&bytes).expect("decode");

    let mut ids: Vec<u64> = records
        .iter()
        .filter(|r| r.kind == Kind::Payload)
        .filter_map(|r| id_of(&r.payload))
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
        println!("verdict: NOT A TRADE STREAM — no trade ids found, so nothing here was checked");
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
