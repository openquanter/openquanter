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
use oq_parity::{BaselineStatus, RunManifest};
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
    /// Unknown-fee executions whose original commission was recovered.
    pub recovered_fee_executions: usize,
    pub legs: Vec<(&'static str, QtyLots, PriceTicks)>,
    /// Funding coverage is separate from the sum of observed amounts.
    pub funding_coverage: bool,
    /// Original ledger IDs verified in additive funding evidence records.
    pub funding_identified_lines: usize,
    /// Venue lines booked without a historical model amount.
    pub funding_unmodelled_lines: usize,
    /// Latest durable uncertainty; `None` is legacy/unrecorded, not complete.
    pub funding_obligations: Option<crate::funding::FundingObligations>,
}

/// Identity-bound recorded accounting. Coverage is an independent result
/// inside `accounting`, never inferred from a matching manifest.
#[derive(Debug)]
pub struct BoundLedgerReport {
    pub accounting: LedgerReport,
    pub manifest: RunManifest,
    pub baseline: Option<BaselineStatus>,
}

const MANIFEST_SCOPE: &str = "openquanter native-ledger manifest v1";

fn code_known(code: &str) -> bool {
    matches!(code.len(), 40 | 64) && code.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl BoundLedgerReport {
    #[must_use]
    pub fn code_identity_known(&self) -> bool {
        code_known(&self.manifest.code_commit)
    }
}

/// Read a checksummed journal through the live books' kernel.
///
/// # Errors
/// Corruption, unknown records, missing context, legacy fee-less fills,
/// duplicate economic events or a changed instrument within one session.
pub fn replay(path: impl AsRef<Path>) -> Result<LedgerReport, String> {
    let reader = Reader::open(path).map_err(|e| e.to_string())?;
    replay_records(&decode_snapshot(&reader)?)
}

fn decode_snapshot(reader: &Reader) -> Result<Vec<Record>, String> {
    let journal = reader.replay().map_err(|e| e.to_string())?;
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
    Ok(records)
}

/// Hash and decode one immutable snapshot, binding the actual effective
/// native accounting context. Code changes are named; changed input or
/// context invalidates comparison. Unknown code cannot verify a baseline.
pub fn replay_bound(
    path: impl AsRef<Path>,
    code_commit: &str,
    expected: Option<&RunManifest>,
) -> Result<BoundLedgerReport, String> {
    let reader = Reader::open(path).map_err(|e| e.to_string())?;
    bound_snapshot(&reader, code_commit, expected)
}

fn bound_snapshot(
    reader: &Reader,
    code_commit: &str,
    expected: Option<&RunManifest>,
) -> Result<BoundLedgerReport, String> {
    let records = decode_snapshot(reader)?;
    let accounting = replay_records(&records)?;
    let start = records
        .iter()
        .rposition(|r| matches!(r, Record::SessionStart { .. }))
        .ok_or("session identity is missing")?;
    let context = records[start + 1..]
        .iter()
        .find(|r| matches!(r, Record::LedgerStart { .. }))
        .ok_or("journal has no native ledger context")?;
    let mut config = MANIFEST_SCOPE.as_bytes().to_vec();
    config.push(0);
    config.extend(context.encode());
    let revision = if code_known(code_commit) {
        code_commit.to_ascii_lowercase()
    } else {
        "unknown".into()
    };
    let manifest = RunManifest::from_content(revision, reader.snapshot(), &config, MANIFEST_SCOPE);
    let baseline = if let Some(expected) = expected {
        if expected.label != MANIFEST_SCOPE {
            return Err("manifest describes a different replay scope".into());
        }
        if !code_known(&expected.code_commit) || !code_known(&manifest.code_commit) {
            return Err("manifest verification needs a known build code revision".into());
        }
        let status = expected.compare(&manifest);
        if let BaselineStatus::Invalidated { changed } = &status {
            return Err(format!(
                "manifest invalidated: {}",
                changed
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Some(status)
    } else {
        None
    };
    Ok(BoundLedgerReport {
        accounting,
        manifest,
        baseline,
    })
}

/// Recorded accounting plus independent, closed-window fee/funding evidence.
/// Coverage does not supply model inventory or certify a G12 residual.
#[derive(Debug)]
pub struct CoveredLedgerReport {
    pub native: BoundLedgerReport,
    pub coverage: crate::ledger_coverage::VerifiedCoverage,
}

/// Bind native replay and coverage to the same immutable reader snapshot.
/// The evidence producer must explicitly establish account scope, retention
/// and complete pagination; legacy history vectors cannot do this alone.
pub fn replay_with_history(
    path: impl AsRef<Path>,
    code_commit: &str,
    evidence: &crate::ledger_coverage::CoverageEvidence,
) -> Result<CoveredLedgerReport, String> {
    let reader = Reader::open(path).map_err(|e| e.to_string())?;
    let mut native = bound_snapshot(&reader, code_commit, None)?;
    let coverage =
        crate::ledger_coverage::verify(&decode_snapshot(&reader)?, &native.manifest, evidence)?;
    native.accounting.funding_coverage = true;
    Ok(CoveredLedgerReport { native, coverage })
}

/// A strict, scope-named sidecar for the existing RunManifest identity.
pub fn manifest_text(manifest: &RunManifest) -> Result<String, String> {
    if manifest.label != MANIFEST_SCOPE
        || !(code_known(&manifest.code_commit) || manifest.code_commit == "unknown")
        || ![&manifest.data_hash, &manifest.config_hash]
            .into_iter()
            .all(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err("invalid native ledger manifest identity or scope".into());
    }
    Ok(format!(
        "{MANIFEST_SCOPE}\ncode_commit={}\ndata_hash={}\nconfig_hash={}\n",
        manifest.code_commit, manifest.data_hash, manifest.config_hash
    ))
}

/// Reject ambiguous, duplicate, extra or partial sidecar fields.
pub fn parse_manifest(text: &str) -> Result<RunManifest, String> {
    let lines: Vec<_> = text.lines().collect();
    if lines.len() != 4 || lines[0] != MANIFEST_SCOPE {
        return Err("invalid native ledger manifest header or field count".into());
    }
    let get = |index: usize, key: &str| {
        lines[index]
            .strip_prefix(key)
            .map(str::to_string)
            .ok_or_else(|| "invalid native ledger manifest field".to_string())
    };
    let manifest = RunManifest {
        code_commit: get(1, "code_commit=")?.to_ascii_lowercase(),
        data_hash: get(2, "data_hash=")?.to_ascii_lowercase(),
        config_hash: get(3, "config_hash=")?.to_ascii_lowercase(),
        label: MANIFEST_SCOPE.into(),
    };
    manifest_text(&manifest)?;
    Ok(manifest)
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
    let mut funding_state: Option<(oq_types::Nanos, crate::funding::FundingObligations)> = None;
    let mut adopted = false;
    let session = &records[start + 1..];
    let mut recovered_fees = std::collections::BTreeMap::new();
    for (i, record) in session.iter().enumerate() {
        if let Record::FeeEvidence {
            at,
            source_ms,
            fill,
            amount,
        } = record
        {
            if *at < fill.stamp.local || *source_ms > at.0 / 1_000_000 {
                return Err(
                    "fee evidence precedes execution receipt or comes from the future".into(),
                );
            }
            let Some(index) = session[..i].iter().position(
                |r| matches!(r, Record::Execution { fill: original, .. } if original == fill),
            ) else {
                return Err("fee evidence does not bind a prior complete execution".into());
            };
            let Record::Execution { fee, .. } = &session[index] else {
                unreachable!()
            };
            if venue_fee(*fee) != Fee::Unreadable {
                return Err("fee evidence cannot overwrite an already reported commission".into());
            }
            if recovered_fees.insert(index, *amount).is_some() {
                return Err("duplicate fee recovery for one execution".into());
            }
        }
    }
    for (record_index, record) in session.iter().enumerate() {
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
                let evidence = recovered_fees
                    .get(&record_index)
                    .map_or_else(|| venue_fee(*fee), |amount| Fee::Reported(*amount));
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
            Record::FundingState { at, obligations } => {
                let now_ms = at.0 / 1_000_000;
                if obligations.pending_ms.windows(2).any(|p| p[0] >= p[1])
                    || obligations.pending_ms.iter().any(|t| *t > now_ms)
                {
                    return Err(
                        "funding pending timestamps are duplicate, unordered or after receipt"
                            .into(),
                    );
                }
                for (i, gap) in obligations.schedule_gaps.iter().enumerate() {
                    if gap
                        .until_ms
                        .is_some_and(|until| until < gap.since_ms || until > now_ms)
                        || gap.since_ms > now_ms.saturating_add(1)
                        || i > 0
                            && obligations.schedule_gaps[i - 1]
                                .until_ms
                                .is_none_or(|until| until >= gap.since_ms)
                    {
                        return Err("funding schedule gap boundaries are invalid".into());
                    }
                }
                if let Some((previous_at, previous)) = &funding_state {
                    if at < previous_at {
                        return Err("funding state receive time moved backwards".into());
                    }
                    if previous.problem.is_some() && obligations.problem.is_none() {
                        return Err("funding uncertainty was silently cleared".into());
                    }
                    for pending in &previous.pending_ms {
                        if !obligations.pending_ms.contains(pending) && !funding.contains(pending) {
                            return Err("funding pending obligation disappeared without a booked settlement".into());
                        }
                    }
                    for previous_gap in &previous.schedule_gaps {
                        let Some(gap) = obligations
                            .schedule_gaps
                            .iter()
                            .find(|gap| gap.since_ms == previous_gap.since_ms)
                        else {
                            return Err("funding schedule gap obligation disappeared".into());
                        };
                        if previous_gap.until_ms.is_some() && previous_gap.until_ms != gap.until_ms
                        {
                            return Err("funding schedule gap history was rewritten".into());
                        }
                    }
                }
                funding_state = Some((*at, obligations.clone()));
            }
            _ => {}
        }
    }
    let b = books.ok_or("ledger context is missing; supply independently identified enrichment")?;
    if !adopted {
        return Err("initial positions are unknown; explicit adoption is required".into());
    }
    let funding_obligations = funding_state.map(|(_, mut obligations)| {
        obligations
            .pending_ms
            .retain(|pending| !funding.contains(pending));
        obligations
    });
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
        recovered_fee_executions: recovered_fees.len(),
        legs,
        funding_coverage: false,
        funding_identified_lines: funding_ids.len(),
        funding_unmodelled_lines,
        funding_obligations,
    })
}
