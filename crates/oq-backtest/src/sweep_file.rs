//! A sweep's result as a file: what a console reads to show a parameter
//! search and whether it can be trusted.
//!
//! A strategy is compiled Rust, so a sweep runs in the caller's program
//! and there is no `oq sweep` to hold its output. This is the other half:
//! the caller writes [`render`] to a `.sweep` file, and anything that
//! reads it — the console's sweeps page — gets every configuration's
//! outcome **and** the overfitting statistics beside them, including the
//! reasons a statistic could not be computed. A table of winners without
//! the deflated Sharpe and the PBO next to it is the thing the sweep
//! exists to refuse.
//!
//! Line-oriented, one fact per line, tab-separated where a line has
//! several fields, so it can be read by eye and diffed:
//!
//! ```text
//! openquanter-sweep 1
//! label ma-cross calm
//! equity-every 100
//! thresholds 0.35 0.95 0
//! deflated-sharpe 0.41
//! pbo 0.25 16 0.3 0.12 0.8          # pbo, splits, P(loss), median OOS Sharpe, slope
//! logits -1.2 0.4 …
//! refusal <sentence>
//! config <label>\t<fills>\t<realized>\t<fees>\t<final equity>\t<min equity>\t<liquidations>\t<sharpe or ->
//! unscorable <label>
//! lookahead <label>\t<sentence>
//! ```
//!
//! A statistic that could not be computed is written `<name> - <reason>`.

use core::fmt::Write as _;

use crate::sweep::{SweepReport, Thresholds, returns};

/// The format this build writes and reads.
pub const VERSION: &str = "1";

/// Cash as the file writes it: account currency, not the fixed-point unit.
#[allow(clippy::cast_precision_loss)]
fn money(c: oq_types::Cash) -> f64 {
    c.0 as f64 / oq_types::CASH_SCALE as f64
}

/// Render a sweep, judged against `thresholds`.
#[must_use]
pub fn render(label: &str, report: &SweepReport, thresholds: Thresholds) -> String {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let mut out = String::new();
    let _ = writeln!(out, "openquanter-sweep {VERSION}");
    let _ = writeln!(out, "label {}", clean(label));
    let _ = writeln!(out, "equity-every {}", report.equity_every);
    let _ = writeln!(
        out,
        "thresholds {} {} {}",
        thresholds.max_pbo, thresholds.min_deflated_sharpe, thresholds.min_degradation_slope
    );
    match &report.deflated_sharpe {
        Ok(d) => {
            let _ = writeln!(out, "deflated-sharpe {d}");
        }
        Err(why) => {
            let _ = writeln!(out, "deflated-sharpe - {}", clean(why));
        }
    }
    match &report.pbo {
        Ok(p) => {
            let _ = writeln!(
                out,
                "pbo {} {} {} {} {}",
                p.pbo,
                p.n_splits,
                p.probability_of_loss,
                p.median_oos_sharpe,
                p.performance_degradation
            );
            let logits: Vec<String> = p.logits.iter().map(ToString::to_string).collect();
            let _ = writeln!(out, "logits {}", logits.join(" "));
        }
        Err(why) => {
            let _ = writeln!(out, "pbo - {}", clean(why));
        }
    }
    for refusal in report.refusals(thresholds) {
        let _ = writeln!(out, "refusal {}", clean(&refusal.to_string()));
    }
    for (label, result) in &report.results {
        let sharpe = oq_stats::Moments::from_returns(&returns(&result.equity_curve))
            .map(|m| m.sharpe_ratio())
            .map_or_else(|_| "-".to_string(), |s| s.to_string());
        let _ = writeln!(
            out,
            "config {}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            clean(label),
            result.fills.len(),
            money(result.realized),
            money(result.fees_paid),
            money(result.final_equity),
            money(result.min_equity),
            result.liquidations.len(),
            sharpe
        );
    }
    for label in &report.unscorable {
        let _ = writeln!(out, "unscorable {}", clean(label));
    }
    if let Some((label, check)) = &report.lookahead {
        let verdict = if check.clean() {
            format!("clean over {} signal(s)", check.signals)
        } else {
            format!(
                "{} divergence(s) in {} checked",
                check.divergences.len(),
                check.checked
            )
        };
        let _ = writeln!(out, "lookahead {}\t{verdict}", clean(label));
    }
    out
}

/// One configuration's row.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigRow {
    pub label: String,
    pub fills: usize,
    pub realized: f64,
    pub fees: f64,
    pub final_equity: f64,
    pub min_equity: f64,
    pub liquidations: usize,
    /// `None` when there were too few returns to score.
    pub sharpe: Option<f64>,
}

/// The probability of backtest overfitting and what came with it.
#[derive(Debug, Clone, PartialEq)]
pub struct Pbo {
    pub pbo: f64,
    pub splits: usize,
    pub probability_of_loss: f64,
    pub median_oos_sharpe: f64,
    pub degradation: f64,
    pub logits: Vec<f64>,
}

/// A sweep file, read back.
#[derive(Debug, Clone, PartialEq)]
pub struct SweepFile {
    pub label: String,
    pub equity_every: usize,
    /// `(max pbo, min deflated sharpe, min degradation slope)`.
    pub thresholds: (f64, f64, f64),
    pub deflated_sharpe: Result<f64, String>,
    pub pbo: Result<Pbo, String>,
    /// Why it must not be deployed; empty when nothing refused it.
    pub refusals: Vec<String>,
    pub configs: Vec<ConfigRow>,
    pub unscorable: Vec<String>,
    pub lookahead: Option<(String, String)>,
}

fn num<T: core::str::FromStr>(s: Option<&str>, what: &str, n: usize) -> Result<T, String> {
    s.and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("line {n}: {what} is not a number"))
}

impl SweepFile {
    /// Read a rendered sweep.
    ///
    /// # Errors
    /// The version line is missing or another, or a line does not parse
    /// — named by its number.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines().enumerate();
        match lines.next() {
            Some((_, l)) if l.trim() == format!("openquanter-sweep {VERSION}") => {}
            Some((_, l)) => return Err(format!("not a sweep file this build reads: {l:?}")),
            None => return Err("empty".into()),
        }
        let mut f = Self {
            label: String::new(),
            equity_every: 0,
            thresholds: (0.0, 0.0, 0.0),
            deflated_sharpe: Err("not recorded".into()),
            pbo: Err("not recorded".into()),
            refusals: Vec::new(),
            configs: Vec::new(),
            unscorable: Vec::new(),
            lookahead: None,
        };
        let mut logits = Vec::new();
        for (i, line) in lines {
            let n = i + 1;
            let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "label" => f.label = rest.to_string(),
                "equity-every" => f.equity_every = num(Some(rest), "equity-every", n)?,
                "thresholds" => {
                    let mut p = rest.split(' ');
                    f.thresholds = (
                        num(p.next(), "max pbo", n)?,
                        num(p.next(), "min deflated sharpe", n)?,
                        num(p.next(), "min slope", n)?,
                    );
                }
                "deflated-sharpe" => {
                    f.deflated_sharpe = match rest.strip_prefix("- ") {
                        Some(why) => Err(why.to_string()),
                        None => Ok(num(Some(rest), "deflated sharpe", n)?),
                    };
                }
                "pbo" => {
                    f.pbo = match rest.strip_prefix("- ") {
                        Some(why) => Err(why.to_string()),
                        None => {
                            let mut p = rest.split(' ');
                            Ok(Pbo {
                                pbo: num(p.next(), "pbo", n)?,
                                splits: num(p.next(), "splits", n)?,
                                probability_of_loss: num(p.next(), "probability of loss", n)?,
                                median_oos_sharpe: num(p.next(), "median oos sharpe", n)?,
                                degradation: num(p.next(), "degradation", n)?,
                                logits: Vec::new(),
                            })
                        }
                    };
                }
                "logits" => {
                    logits = rest
                        .split(' ')
                        .filter(|s| !s.is_empty())
                        .map(|s| num(Some(s), "logit", n))
                        .collect::<Result<_, _>>()?;
                }
                "refusal" => f.refusals.push(rest.to_string()),
                "unscorable" => f.unscorable.push(rest.to_string()),
                "lookahead" => {
                    let (l, v) = rest.split_once('\t').unwrap_or((rest, ""));
                    f.lookahead = Some((l.to_string(), v.to_string()));
                }
                "config" => {
                    let p: Vec<&str> = rest.split('\t').collect();
                    if p.len() != 8 {
                        return Err(format!(
                            "line {n}: a config row has 8 fields, this has {}",
                            p.len()
                        ));
                    }
                    f.configs.push(ConfigRow {
                        label: p[0].to_string(),
                        fills: num(Some(p[1]), "fills", n)?,
                        realized: num(Some(p[2]), "realized", n)?,
                        fees: num(Some(p[3]), "fees", n)?,
                        final_equity: num(Some(p[4]), "final equity", n)?,
                        min_equity: num(Some(p[5]), "min equity", n)?,
                        liquidations: num(Some(p[6]), "liquidations", n)?,
                        sharpe: if p[7] == "-" {
                            None
                        } else {
                            Some(num(Some(p[7]), "sharpe", n)?)
                        },
                    });
                }
                "" => {}
                other => return Err(format!("line {n}: unknown line {other:?}")),
            }
        }
        if let Ok(p) = &mut f.pbo {
            p.logits = logits;
        }
        Ok(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rendered_sweep_reads_back_whole_including_what_could_not_be_computed() {
        let report = SweepReport {
            results: Vec::new(),
            equity_every: 50,
            deflated_sharpe: Ok(0.42),
            pbo: Err("fewer than two configurations scored".into()),
            unscorable: vec!["fast=1\tslow=2".into()],
            lookahead: None,
        };
        let text = render("demo", &report, Thresholds::default());
        let f = SweepFile::parse(&text).expect("reads");
        assert_eq!(f.label, "demo");
        assert_eq!(f.equity_every, 50);
        assert_eq!(f.deflated_sharpe, Ok(0.42));
        assert_eq!(f.pbo, Err("fewer than two configurations scored".into()));
        assert_eq!(
            f.unscorable,
            vec!["fast=1 slow=2".to_string()],
            "a tab cannot break a line"
        );
        assert!(
            !f.refusals.is_empty(),
            "an unscored PBO and a low DSR are refusals: {text}"
        );
        assert!(SweepFile::parse("openquanter-run 1\n").is_err());
        assert!(SweepFile::parse("openquanter-sweep 1\nconfig a\tb\n").is_err());
    }
}
