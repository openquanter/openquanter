//! Strict, versioned external history evidence. No inferred completeness.

use std::fmt::Display;
use std::path::Path;
use std::str::FromStr;

use oq_gateway::account::{ExecutionFee, FundingCharge};
use oq_types::{Cash, Side};

use super::{Completeness, CoverageEvidence, HistoryWindow, hash_valid, window};

const HEADER: &str = "openquanter ledger-history evidence v1";

fn validate(e: &CoverageEvidence) -> Result<(), String> {
    if !hash_valid(&e.journal_data_hash)
        || !hash_valid(&e.account_scope_hash)
        || e.since_ms < 0
        || e.since_ms > e.through_ms
        || e.evaluated_ms < e.through_ms
        || e.max_age_ms < 0
    {
        return Err("invalid evidence identity or evaluation policy".into());
    }
    let symbol = &e.fees.symbol;
    if symbol.is_empty()
        || !symbol
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:".contains(&b))
    {
        return Err("invalid evidence symbol".into());
    }
    window(e, &e.fees, symbol)?;
    window(e, &e.funding, symbol)?;
    if e.fees
        .rows
        .iter()
        .any(|r| r.trade_id == 0 || r.time_ms < e.fees.since_ms || r.time_ms > e.fees.through_ms)
        || e.funding
            .rows
            .iter()
            .any(|r| r.time_ms < e.funding.since_ms || r.time_ms > e.funding.through_ms)
    {
        return Err("evidence row lies outside its declared source window".into());
    }
    Ok(())
}

/// Serialize only explicitly complete, well-formed source windows. The
/// native verifier must still establish exact identity-set equality.
pub fn evidence_text(e: &CoverageEvidence) -> Result<String, String> {
    validate(e)?;
    let mut text = format!(
        "{HEADER}\njournal_data_hash={}\naccount_scope_hash={}\nsince_ms={}\nthrough_ms={}\nevaluated_ms={}\nmax_age_ms={}\n",
        e.journal_data_hash,
        e.account_scope_hash,
        e.since_ms,
        e.through_ms,
        e.evaluated_ms,
        e.max_age_ms
    );
    let header = |name: &str,
                  symbol: &str,
                  since: i64,
                  through: i64,
                  observed: i64,
                  count: usize| {
        format!(
            "{name}.symbol={symbol}\n{name}.since_ms={since}\n{name}.through_ms={through}\n{name}.observed_ms={observed}\n{name}.rows={count}\n"
        )
    };
    text.push_str(&header(
        "fees",
        &e.fees.symbol,
        e.fees.since_ms,
        e.fees.through_ms,
        e.fees.observed_ms,
        e.fees.rows.len(),
    ));
    for r in &e.fees.rows {
        text.push_str(&format!(
            "fee={},{},{},{}\n",
            r.trade_id,
            if r.side == Side::Buy { "BUY" } else { "SELL" },
            r.time_ms,
            r.amount.0
        ));
    }
    text.push_str(&header(
        "funding",
        &e.funding.symbol,
        e.funding.since_ms,
        e.funding.through_ms,
        e.funding.observed_ms,
        e.funding.rows.len(),
    ));
    for r in &e.funding.rows {
        text.push_str(&format!("funding={},{},{}\n", r.id, r.time_ms, r.amount.0));
    }
    Ok(text)
}

fn number<T: FromStr + Display>(text: &str) -> Result<T, String> {
    let value = text
        .parse::<T>()
        .map_err(|_| "invalid evidence integer".to_string())?;
    if value.to_string() != text {
        return Err("non-canonical evidence integer".into());
    }
    Ok(value)
}

struct Lines<'a> {
    rows: Vec<&'a str>,
    at: usize,
}
impl<'a> Lines<'a> {
    fn get(&mut self, key: &str) -> Result<&'a str, String> {
        let row = self.rows.get(self.at).ok_or("truncated evidence file")?;
        self.at += 1;
        row.strip_prefix(key)
            .ok_or_else(|| format!("expected evidence field {key}"))
    }
    fn count(&mut self, key: &str) -> Result<usize, String> {
        let count = number(self.get(key)?)?;
        if count > self.rows.len().saturating_sub(self.at) {
            return Err("evidence count exceeds remaining lines".into());
        }
        Ok(count)
    }
    fn source<T>(&mut self, name: &str, scope: &str) -> Result<(HistoryWindow<T>, usize), String> {
        let symbol = self.get(&format!("{name}.symbol="))?.into();
        let since_ms = number(self.get(&format!("{name}.since_ms="))?)?;
        let through_ms = number(self.get(&format!("{name}.through_ms="))?)?;
        let observed_ms = number(self.get(&format!("{name}.observed_ms="))?)?;
        let count = self.count(&format!("{name}.rows="))?;
        Ok((
            HistoryWindow {
                account_scope_hash: scope.into(),
                symbol,
                since_ms,
                through_ms,
                observed_ms,
                completeness: Completeness::Complete,
                rows: Vec::with_capacity(count),
            },
            count,
        ))
    }
}

/// Reject reordered, extra, truncated, non-canonical or unsupported fields.
/// The format explicitly asserts complete as-observed source windows.
pub fn parse_evidence(text: &str) -> Result<CoverageEvidence, String> {
    let mut lines = Lines {
        rows: text.lines().collect(),
        at: 0,
    };
    if !lines.get(HEADER)?.is_empty() {
        return Err("invalid evidence version".into());
    }
    let journal_data_hash = lines.get("journal_data_hash=")?.into();
    let account_scope_hash: String = lines.get("account_scope_hash=")?.into();
    let since_ms = number(lines.get("since_ms=")?)?;
    let through_ms = number(lines.get("through_ms=")?)?;
    let evaluated_ms = number(lines.get("evaluated_ms=")?)?;
    let max_age_ms = number(lines.get("max_age_ms=")?)?;
    let (mut fees, count) = lines.source("fees", &account_scope_hash)?;
    for _ in 0..count {
        let fields: Vec<_> = lines.get("fee=")?.split(',').collect();
        if fields.len() != 4 {
            return Err("invalid fee evidence row".into());
        }
        let side = match fields[1] {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            _ => return Err("invalid fee evidence side".into()),
        };
        fees.rows.push(ExecutionFee {
            trade_id: number(fields[0])?,
            side,
            time_ms: number(fields[2])?,
            amount: Cash(number(fields[3])?),
        });
    }
    let (mut funding, count) = lines.source("funding", &account_scope_hash)?;
    for _ in 0..count {
        let fields: Vec<_> = lines.get("funding=")?.split(',').collect();
        if fields.len() != 3 {
            return Err("invalid funding evidence row".into());
        }
        funding.rows.push(FundingCharge {
            id: number(fields[0])?,
            time_ms: number(fields[1])?,
            amount: Cash(number(fields[2])?),
        });
    }
    if lines.at != lines.rows.len() {
        return Err("extra evidence fields".into());
    }
    let evidence = CoverageEvidence {
        journal_data_hash,
        account_scope_hash,
        since_ms,
        through_ms,
        evaluated_ms,
        max_age_ms,
        fees,
        funding,
    };
    validate(&evidence)?;
    Ok(evidence)
}

/// Create evidence without overwriting a journal or an existing attachment.
pub fn write_evidence(path: impl AsRef<Path>, evidence: &CoverageEvidence) -> Result<(), String> {
    use std::io::Write;
    let text = evidence_text(evidence)?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())
}
