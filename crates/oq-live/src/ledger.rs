//! Reconstruct recorded account economics, never an inferred strategy run.
//!
//! A missing fee is not zero. Funding totals here cover recorded settlements,
//! not a claim that every settlement or account cash flow was captured.

use std::collections::BTreeSet;
use std::path::Path;

use oq_core::PositionMode;
use oq_engine::Tick;
use oq_journal::Reader;
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, Fee, InstrumentId, PriceTicks, QtyLots, Side, Stamp};

use crate::books::{Booked, Books};
use crate::record::{Record, venue_fee};

/// The latest session in a journal, with uncertainty explicit.
#[derive(Debug)]
pub struct LedgerReport {
    pub symbol: String,
    pub mode: PositionMode,
    pub starting_balance: Cash,
    pub recorded_balance: Cash,
    pub realized: Cash,
    pub recorded_fees: Cash,
    pub funding: Cash,
    pub executions: usize,
    pub missing_fees: usize,
    pub legs: Vec<(&'static str, QtyLots, PriceTicks)>,
    /// Funding coverage is separate from the sum of observed amounts.
    pub funding_coverage: bool,
    /// Original ledger IDs verified in additive funding evidence records.
    pub funding_identified_lines: usize,
    /// Venue lines booked without a historical model amount.
    pub funding_unmodelled_lines: usize,
}

/// Read a checksummed journal through the live books' kernel.
///
/// # Errors
/// Corruption, unknown records, missing context, legacy fee-less fills,
/// duplicate economic events or a changed instrument within one session.
pub fn replay(path: impl AsRef<Path>) -> Result<LedgerReport, String> {
    let journal = Reader::open(path)
        .map_err(|e| e.to_string())?
        .replay()
        .map_err(|e| e.to_string())?;
    if journal.stop != oq_journal::ReplayStop::Clean {
        return Err("ledger journal has an incomplete tail; use forensic replay to inspect its verified prefix".into());
    }
    let mut records = Vec::new();
    for frame in journal.since(0) {
        let record = Record::decode(frame.kind, &frame.payload)
            .ok_or_else(|| format!("undecodable ledger record at sequence {}", frame.seq))?;
        // Venue books do not match or synthesize liquidations from ticks.
        // Keep the verified economic tape small on long capture windows.
        if !matches!(&record, Record::Tick { .. } | Record::Waiting { .. }) {
            records.push(record);
        }
    }
    replay_records(&records)
}

/// The same deterministic fold for callers with already verified records.
///
/// # Errors
/// The same evidence and identity failures as [`replay`].
pub fn replay_records(records: &[Record]) -> Result<LedgerReport, String> {
    let start = records
        .iter()
        .rposition(|r| matches!(r, Record::SessionStart { .. }))
        .ok_or("session identity is missing")?;
    let Record::SessionStart {
        symbol,
        price_scale,
        qty_scale,
        ..
    } = &records[start]
    else {
        unreachable!()
    };
    let mut books = None;
    let mut instrument_id = InstrumentId::new(0);
    let mut initial = Cash::ZERO;
    let mut mode = PositionMode::OneWay;
    let mut executions = 0;
    let mut missing = 0;
    let mut funding = BTreeSet::new();
    let mut funding_ids = BTreeSet::new();
    let mut funding_unmodelled_lines = 0;
    let mut adopted = false;
    for record in &records[start + 1..] {
        match record {
            Record::LedgerStart {
                instrument_id: id,
                instrument,
                tick_cash,
                balance,
                mode: m,
                ..
            } => {
                if books.is_some() {
                    return Err("duplicate ledger context in one session".into());
                }
                if Contract::of(instrument).map(|c| c.tick_cash) != Some(*tick_cash) {
                    return Err("ledger instrument and cash-per-tick-lot disagree".into());
                }
                if instrument.price_scale != *price_scale || instrument.qty_scale != *qty_scale {
                    return Err("session scales and ledger instrument disagree".into());
                }
                books = Some(Books::new(
                    *id,
                    Contract::new(*tick_cash),
                    TierTable::example_btcusdt(),
                    *balance,
                    *m,
                ));
                instrument_id = *id;
                initial = *balance;
                mode = *m;
            }
            Record::Reconciled { at, legs } => {
                let b = books.as_mut().ok_or("adoption precedes ledger context")?;
                if executions != 0 {
                    return Err("position adoption after executions requires a new session".into());
                }
                if adopted {
                    return Err("duplicate adoption in one session".into());
                }
                adopted = true;
                for (sym, leg, qty, entry) in legs {
                    if sym != symbol {
                        return Err("adopted leg belongs to another symbol".into());
                    }
                    let side = if leg.eq_ignore_ascii_case("SHORT") || *qty < 0 {
                        Side::Sell
                    } else {
                        Side::Buy
                    };
                    b.adopt(side, QtyLots(qty.saturating_abs()), PriceTicks(*entry), *at);
                }
            }
            Record::Execution { fill, fee, .. } => {
                if !adopted {
                    return Err("initial positions are unknown; an explicit adoption record is required, even when flat".into());
                }
                if fill.instrument != instrument_id || fill.qty.0 <= 0 || fill.price.0 <= 0 {
                    return Err("execution instrument, price or quantity is invalid".into());
                }
                let b = books.as_mut().ok_or("execution precedes ledger context")?;
                let evidence = venue_fee(*fee);
                if evidence == Fee::Unreadable {
                    missing += 1;
                }
                if !matches!(b.on_venue_fill(fill, evidence), Booked::Applied(_)) {
                    return Err("execution identity is duplicate, stale or unidentifiable".into());
                }
                executions += 1;
            }
            Record::Fill { .. } => {
                return Err(
                    "legacy fill needs fee/instrument enrichment; it cannot certify net accounting"
                        .into(),
                );
            }
            Record::Funding {
                at,
                settled_ms,
                venue,
                ..
            }
            | Record::FundingEvidence {
                at,
                settled_ms,
                venue,
                ..
            } => {
                if let Record::FundingEvidence { lines, .. } = record {
                    let mut total = 0i128;
                    for (id, time_ms, amount) in lines {
                        if !funding_ids.insert(*id) {
                            return Err("duplicate funding ledger identity".into());
                        }
                        if i128::from(*time_ms).abs_diff(i128::from(*settled_ms)) > 60_000 {
                            return Err(
                                "funding ledger line is outside its settlement window".into()
                            );
                        }
                        total += i128::from(amount.0);
                    }
                    if total != i128::from(venue.0) {
                        return Err("funding ledger lines disagree with recorded amount".into());
                    }
                }
                if !funding.insert(*settled_ms) {
                    return Err("duplicate funding settlement".into());
                }
                books
                    .as_mut()
                    .ok_or("funding precedes ledger context")?
                    .on_funding_charged(*venue, *at);
            }
            Record::Tick {
                at,
                seen,
                last,
                bid,
                ask,
                volume,
            } => {
                if let Some(b) = books.as_mut() {
                    b.on_tick(&Tick {
                        stamp: Stamp {
                            exch: *at,
                            local: *seen,
                        },
                        last: *last,
                        high: *last,
                        low: *last,
                        bid: *bid,
                        ask: *ask,
                        volume: *volume,
                    });
                }
            }
            Record::FundingRecovered {
                at,
                since_ms,
                through_ms,
                lines,
            } => {
                if !adopted {
                    return Err("recovered funding precedes explicit initial positions".into());
                }
                if since_ms > through_ms || *through_ms > at.0 / 1_000_000 {
                    return Err("funding recovery range is invalid or lies after receipt".into());
                }
                let b = books
                    .as_mut()
                    .ok_or("funding recovery precedes ledger context")?;
                for (id, time_ms, amount) in lines {
                    if !funding_ids.insert(*id) {
                        return Err("duplicate funding ledger identity".into());
                    }
                    if time_ms < since_ms || time_ms > through_ms {
                        return Err("recovered funding line is outside its history range".into());
                    }
                    b.on_funding_charged(*amount, *at);
                    funding_unmodelled_lines += 1;
                }
            }
            _ => {}
        }
    }
    let b = books.ok_or("ledger context is missing; supply independently identified enrichment")?;
    if !adopted {
        return Err("initial positions are unknown; explicit adoption is required".into());
    }
    let (realized, fees, funding) = b.realized_parts();
    let ctx = b.context(Tick::default(), 0);
    let legs = if mode == PositionMode::Hedge {
        vec![
            ("LONG", ctx.position, ctx.entry),
            ("SHORT", ctx.short_position, ctx.short_entry),
        ]
    } else {
        vec![("NET", ctx.position, ctx.entry)]
    };
    Ok(LedgerReport {
        symbol: symbol.clone(),
        mode,
        starting_balance: initial,
        recorded_balance: initial.add(realized).sub(fees).add(funding),
        realized,
        recorded_fees: fees,
        funding,
        executions,
        missing_fees: missing,
        legs,
        funding_coverage: false,
        funding_identified_lines: funding_ids.len(),
        funding_unmodelled_lines,
    })
}
