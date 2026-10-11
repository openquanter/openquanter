//! Closed-window fee/funding coverage, independently of model attribution.
//!
//! Producers must establish account scope, retention and terminal pagination.
//! A successful legacy `Vec` history query is not a complete-window assertion.
//! This verifier checks the assertion against every recorded identity; it does
//! not authenticate an account statement or reconstruct model inventory.

use std::collections::BTreeMap;

use oq_gateway::account::{ExecutionFee, FundingCharge};
use oq_parity::{BaselineStatus, RunManifest};
use oq_types::Fee;

use crate::record::{Record, venue_fee};

/// Why a producer cannot certify the whole closed interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completeness {
    /// Only after retention and terminal pagination have been established.
    Complete,
    Incomplete(String),
}

/// Original rows plus the explicit interval and scope the source certifies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryWindow<T> {
    /// Opaque SHA-256 of the externally established venue/account scope.
    /// Never put account identifiers or credentials in public fixtures.
    pub account_scope_hash: String,
    pub symbol: String,
    pub since_ms: i64,
    pub through_ms: i64,
    /// Source observation boundary, injected by the producer.
    pub observed_ms: i64,
    pub completeness: Completeness,
    pub rows: Vec<T>,
}

impl<T> HistoryWindow<T> {
    /// Attach a producer-certified retained interval to an externally
    /// established account scope. This hash is supplied by the consumer,
    /// not authenticated by the adapter's row response.
    #[must_use]
    pub fn from_bounded(
        history: oq_gateway::account::BoundedHistory<T>,
        account_scope_hash: String,
    ) -> Self {
        Self {
            account_scope_hash,
            symbol: history.symbol,
            since_ms: history.since_ms,
            through_ms: history.through_ms,
            observed_ms: history.observed_ms,
            completeness: Completeness::Complete,
            rows: history.rows,
        }
    }
}

/// Evidence attachment to one immutable native journal snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageEvidence {
    pub journal_data_hash: String,
    pub account_scope_hash: String,
    pub since_ms: i64,
    pub through_ms: i64,
    pub evaluated_ms: i64,
    pub max_age_ms: i64,
    pub fees: HistoryWindow<ExecutionFee>,
    pub funding: HistoryWindow<FundingCharge>,
}

/// Exact set equality, not a matching sum or an empty pending cohort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCoverage {
    pub fee_executions: usize,
    pub funding_lines: usize,
    /// Identity of all original rows, source boundaries and journal binding.
    pub manifest: RunManifest,
}

const SCOPE: &str = "openquanter native-ledger coverage v1";

impl VerifiedCoverage {
    /// Refuse foreign, unknown-code or invalidated coverage baselines.
    pub fn verify_baseline(&self, expected: &RunManifest) -> Result<BaselineStatus, String> {
        let known = |code: &str| {
            matches!(code.len(), 40 | 64) && code.bytes().all(|byte| byte.is_ascii_hexdigit())
        };
        if expected.label != SCOPE
            || !known(&expected.code_commit)
            || !known(&self.manifest.code_commit)
        {
            return Err("coverage baseline needs the same scope and known code identity".into());
        }
        let status = expected.compare(&self.manifest);
        if matches!(status, BaselineStatus::Invalidated { .. }) {
            return Err("coverage baseline is invalidated by changed evidence or policy".into());
        }
        Ok(status)
    }
}

fn hash_valid(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn window<T>(
    evidence: &CoverageEvidence,
    source: &HistoryWindow<T>,
    symbol: &str,
) -> Result<(), String> {
    if source.account_scope_hash != evidence.account_scope_hash || source.symbol != symbol {
        return Err("history account/symbol scope differs from the evidence attachment".into());
    }
    if source.since_ms > evidence.since_ms
        || source.through_ms < evidence.through_ms
        || source.since_ms > source.through_ms
        || source.since_ms < 0
        || source.observed_ms < source.through_ms
        || source.observed_ms > evidence.evaluated_ms
        || evidence.evaluated_ms - source.observed_ms > evidence.max_age_ms
    {
        return Err("history interval is incomplete, future or stale".into());
    }
    if let Completeness::Incomplete(reason) = &source.completeness {
        return Err(format!(
            "history producer did not establish completeness: {reason}"
        ));
    }
    Ok(())
}

fn insert<K: Ord, V: PartialEq>(map: &mut BTreeMap<K, V>, key: K, row: V) -> Result<(), String> {
    if map.get(&key).is_some_and(|prior| *prior != row) {
        return Err("history identity has conflicting evidence".into());
    }
    map.insert(key, row);
    Ok(())
}

/// Verify independently obtained fee and funding rows against the latest
/// native session. No partial success or zero substitution is returned.
/// The caller must supply the manifest of the very same verified records.
/// Prefer [`crate::ledger::replay_with_history`] for filesystem inputs.
pub fn verify(
    records: &[Record],
    native: &RunManifest,
    evidence: &CoverageEvidence,
) -> Result<VerifiedCoverage, String> {
    let accounting = crate::ledger::replay_records(records)?;
    if native.label != "openquanter native-ledger manifest v1"
        || !hash_valid(&evidence.journal_data_hash)
        || evidence.journal_data_hash != native.data_hash
        || !hash_valid(&evidence.account_scope_hash)
        || evidence.since_ms < 0
        || evidence.through_ms < evidence.since_ms
        || evidence.evaluated_ms < evidence.through_ms
        || evidence.max_age_ms < 0
    {
        return Err("invalid coverage interval, scope or journal binding".into());
    }
    window(evidence, &evidence.fees, &accounting.symbol)?;
    window(evidence, &evidence.funding, &accounting.symbol)?;
    let in_window = |time: i64| time >= evidence.since_ms && time <= evidence.through_ms;
    let start = records
        .iter()
        .rposition(|record| matches!(record, Record::SessionStart { .. }))
        .ok_or("missing session")?;
    let session = &records[start + 1..];
    let boundary = session
        .iter()
        .find_map(|record| match record {
            Record::LedgerStart { at, .. } => Some(at.0 / 1_000_000),
            _ => None,
        })
        .ok_or("missing ledger boundary")?;
    if boundary != evidence.since_ms {
        return Err("coverage must start at the native ledger boundary".into());
    }

    let mut fees = BTreeMap::new();
    for row in &evidence.fees.rows {
        if row.trade_id == 0
            || row.time_ms < evidence.fees.since_ms
            || row.time_ms > evidence.fees.through_ms
        {
            return Err("fee history row is invalid or outside its declared interval".into());
        }
        insert(
            &mut fees,
            (row.trade_id, row.side == oq_types::Side::Buy),
            *row,
        )?;
    }
    fees.retain(|_, row| in_window(row.time_ms));
    let mut funding = BTreeMap::new();
    for row in &evidence.funding.rows {
        if row.time_ms < evidence.funding.since_ms || row.time_ms > evidence.funding.through_ms {
            return Err("funding history row is outside its declared interval".into());
        }
        insert(&mut funding, row.id, *row)?;
    }
    funding.retain(|_, row| in_window(row.time_ms));

    let mut recovered = BTreeMap::new();
    for record in session {
        if let Record::FeeEvidence {
            at,
            fill,
            amount,
            source_ms,
        } = record
        {
            if at.0 / 1_000_000 > evidence.evaluated_ms {
                return Err("fee evidence receipt lies after coverage evaluation".into());
            }
            recovered.insert(
                (fill.trade.0, fill.side == oq_types::Side::Buy),
                (*amount, *source_ms),
            );
        }
    }
    let mut recorded_fees = BTreeMap::new();
    let mut recorded_funding = BTreeMap::new();
    for record in session {
        match record {
            Record::Execution { fill, fee, .. } => {
                let time_ms = fill.stamp.exch.0 / 1_000_000;
                if fill.stamp.exch.0 < 0
                    || fill.stamp.local.0 < 0
                    || !in_window(time_ms)
                    || fill.stamp.local.0 / 1_000_000 > evidence.evaluated_ms
                {
                    return Err("execution lies outside the coverage/receipt boundary".into());
                }
                let key = (fill.trade.0, fill.side == oq_types::Side::Buy);
                let amount = match venue_fee(*fee) {
                    Fee::Reported(amount) => amount,
                    _ => {
                        let (amount, source_ms) = recovered
                            .get(&key)
                            .ok_or("recorded execution still has unknown fees")?;
                        if *source_ms != time_ms {
                            return Err(
                                "recovered fee source timestamp differs from execution".into()
                            );
                        }
                        *amount
                    }
                };
                recorded_fees.insert(
                    key,
                    ExecutionFee {
                        trade_id: fill.trade.0,
                        side: fill.side,
                        time_ms,
                        amount,
                    },
                );
            }
            Record::Funding { .. } => {
                return Err("legacy funding aggregate has no original identity coverage".into());
            }
            Record::FundingEvidence { at, lines, .. }
            | Record::FundingRecovered { at, lines, .. } => {
                if at.0 / 1_000_000 > evidence.evaluated_ms {
                    return Err("funding receipt lies after coverage evaluation".into());
                }
                for (id, time_ms, amount) in lines {
                    if !in_window(*time_ms) || *time_ms > at.0 / 1_000_000 {
                        return Err("recorded funding lies outside the coverage interval".into());
                    }
                    recorded_funding.insert(
                        *id,
                        FundingCharge {
                            id: *id,
                            time_ms: *time_ms,
                            amount: *amount,
                        },
                    );
                }
            }
            _ => {}
        }
    }
    if fees != recorded_fees {
        return Err(
            "fee history and journal do not cover the same identities, times and amounts".into(),
        );
    }
    if funding != recorded_funding {
        return Err(
            "funding history and journal do not cover the same identities, times and amounts"
                .into(),
        );
    }

    // Canonical row order makes stable overlapping pages the same evidence.
    // Source metadata and all rows (even outside the requested subwindow)
    // are bound; the journal hash alone never certifies their contents.
    let mut canonical_fees = evidence.fees.clone();
    canonical_fees
        .rows
        .sort_by_key(|row| (row.trade_id, row.side == oq_types::Side::Buy));
    canonical_fees.rows.dedup();
    let mut canonical_funding = evidence.funding.clone();
    canonical_funding.rows.sort_by_key(|row| row.id);
    canonical_funding.rows.dedup();
    let input = format!(
        "journal={};context={};fees={canonical_fees:?};funding={canonical_funding:?}",
        native.data_hash, native.config_hash
    );
    let config = format!(
        "{SCOPE};scope={};since={};through={};evaluated={};max_age={}",
        evidence.account_scope_hash,
        evidence.since_ms,
        evidence.through_ms,
        evidence.evaluated_ms,
        evidence.max_age_ms
    );
    Ok(VerifiedCoverage {
        fee_executions: recorded_fees.len(),
        funding_lines: recorded_funding.len(),
        manifest: RunManifest::from_content(
            &native.code_commit,
            input.as_bytes(),
            config.as_bytes(),
            SCOPE,
        ),
    })
}
