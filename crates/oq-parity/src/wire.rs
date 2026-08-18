//! A run, written down, so a baseline can outlive the process that made
//! it.
//!
//! # What was missing
//!
//! `RunManifest` and `RunOutput` existed as types and nothing could
//! store one. Three things followed from that, and they are the reason
//! this module exists rather than being a convenience:
//!
//! - **A baseline could not be kept.** `WHY.md`'s fifth wall is that
//!   nothing can show a past result still holds. The instrument for it —
//!   the identity triple, `BaselineStatus`, `baseline invalidated` — was
//!   built and had nowhere to write its answer, so a parity result lived
//!   as long as one process and then became a memory.
//! - **`oq parity` could not be a subcommand.** `compare` takes two
//!   manifests and two outputs; with no file format there was nothing
//!   for a command line to name.
//! - **An attribution report has nothing to bind to.** `FR-ATTRIB-5`
//!   requires a report bound to a manifest so a third party can
//!   reproduce it. A binding needs something written.
//!
//! # The format, and why it is this one
//!
//! Line-oriented text. A baseline is written once and read years later,
//! possibly by somebody arguing that a result was never reproducible, so
//! it has to be readable without this program — `grep`, `diff`, and
//! `pandas.read_csv` on the fill section all work. A binary format would
//! be smaller and would make the archive depend on a decoder that has to
//! still exist and still agree.
//!
//! **The manifest is inside the file.** This is the whole point of D13:
//! a baseline separated from its identity is a number without an
//! experiment, and two files that must be kept together eventually are
//! not. A reader that finds fills without a manifest is looking at
//! something that cannot be compared, and this module refuses it rather
//! than comparing it.
//!
//! **The file carries a hash of its own body.** Not for tamper
//! resistance — anybody who can edit the file can edit the hash — but
//! because a baseline truncated by a full disk is the realistic failure,
//! and a truncated baseline that compares as "matching for the part that
//! survived" is worse than no baseline at all.
//!
//! # What is deliberately absent
//!
//! No compression and no schema evolution machinery. A run output is
//! kilobytes to a few megabytes and compresses well with any ordinary
//! tool; and the version line is a refusal, not a migration path — a
//! version this build does not know is a file it must not guess at.

use core::fmt::Write as _;

use oq_hash::sha256_hex;
use oq_types::{PriceTicks, QtyLots, Side};

use crate::manifest::RunManifest;
use crate::record::{Fill, Nanos, RunOutput};

/// The format version this build writes and the only one it reads.
pub const VERSION: &str = "1";

/// Why a run could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The file does not say what format it is.
    NoVersion,
    /// Written by a version this build does not read.
    Version(String),
    /// A required manifest field is missing.
    ///
    /// Named rather than reported as "malformed": a run without its data
    /// hash is not a run with a formatting problem, it is a run whose
    /// identity is unknown, and the two need different responses.
    NoIdentity(&'static str),
    /// A line could not be read.
    Line {
        /// One-based line number.
        line: usize,
        /// What was wrong.
        why: String,
    },
    /// The body does not hash to what the file says it should.
    Corrupt {
        /// What the file claims.
        declared: String,
        /// What the body actually hashes to.
        actual: String,
    },
    /// There is a manifest and no fills section at all.
    ///
    /// Distinct from a run that produced no fills, which is a legitimate
    /// and interesting outcome. A file that never got as far as writing
    /// its fills would otherwise read as a run that made no trades.
    Truncated,
}

impl core::fmt::Display for ReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoVersion => write!(f, "the file does not say what format it is"),
            Self::Version(v) => write!(f, "format version {v:?}; this build reads {VERSION}"),
            Self::NoIdentity(what) => write!(
                f,
                "the run has no {what}: its identity is unknown, so nothing can be \
                 concluded by comparing it"
            ),
            Self::Line { line, why } => write!(f, "line {line}: {why}"),
            Self::Corrupt { declared, actual } => write!(
                f,
                "the body hashes to {} and the file declares {}; it was truncated or edited",
                &actual[..16.min(actual.len())],
                &declared[..16.min(declared.len())]
            ),
            Self::Truncated => write!(
                f,
                "the file ends before its fills; a run that made no trades writes \
                 `fills 0`, so this is an incomplete file rather than an empty result"
            ),
        }
    }
}

impl core::error::Error for ReadError {}

/// A run and the identity it was produced under.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// What code, over what data, under what configuration.
    pub manifest: RunManifest,
    /// What it produced.
    pub output: RunOutput,
}

impl Run {
    /// Pair a manifest with an output.
    #[must_use]
    pub const fn new(manifest: RunManifest, output: RunOutput) -> Self {
        Self { manifest, output }
    }

    /// Render for writing.
    #[must_use]
    pub fn render(&self) -> String {
        let body = self.body();
        let mut out = String::with_capacity(body.len() + 128);
        let _ = writeln!(out, "openquanter-run {VERSION}");
        // The hash comes before the body so a reader can check it while
        // streaming, rather than having to hold the whole file to find a
        // trailer.
        let _ = writeln!(out, "body-sha256 {}", sha256_hex(body.as_bytes()));
        out.push_str(&body);
        out
    }

    /// Everything the hash covers.
    fn body(&self) -> String {
        let mut out = String::new();
        let m = &self.manifest;
        let _ = writeln!(out, "code-commit {}", m.code_commit);
        let _ = writeln!(out, "data-sha256 {}", m.data_hash);
        let _ = writeln!(out, "config-sha256 {}", m.config_hash);
        // Not part of identity, and written last of the manifest fields
        // so that is visible in the file's own ordering.
        let _ = writeln!(out, "label {}", m.label);
        let _ = writeln!(out, "pnl {}", self.output.pnl);
        let _ = writeln!(out, "fills {}", self.output.fills.len());
        // A header for the fill section, so the columns are named in the
        // file rather than in a document somebody has to find.
        let _ = writeln!(out, "# ts symbol side price qty tag");
        for fill in &self.output.fills {
            let _ = writeln!(
                out,
                "fill {} {} {} {} {} {}",
                fill.ts.0,
                fill.symbol,
                match fill.side {
                    Side::Buy => "buy",
                    Side::Sell => "sell",
                },
                fill.price.0,
                fill.qty.0,
                // A tag is optional, and an empty column would be
                // indistinguishable from a tag that is the empty string.
                fill.tag.as_deref().unwrap_or("-"),
            );
        }
        out
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// [`ReadError`] naming what could not be read. Every failure mode
    /// is distinct because they call for different responses: a version
    /// this build does not know needs a different binary, a missing data
    /// hash needs the run repeating, and a body that does not hash needs
    /// the file re-fetched.
    pub fn parse(text: &str) -> Result<Self, ReadError> {
        let mut lines = text.lines().enumerate();

        let (_, first) = lines.next().ok_or(ReadError::NoVersion)?;
        let version = first
            .strip_prefix("openquanter-run ")
            .ok_or(ReadError::NoVersion)?
            .trim();
        if version != VERSION {
            return Err(ReadError::Version(version.to_string()));
        }

        let (n, second) = lines.next().ok_or(ReadError::Truncated)?;
        let declared = second
            .strip_prefix("body-sha256 ")
            .ok_or(ReadError::Line {
                line: n + 1,
                why: "expected body-sha256".to_string(),
            })?
            .trim()
            .to_string();

        // The body is everything after the two header lines, exactly as
        // written — reconstructed by slicing rather than by re-joining,
        // so a trailing newline cannot change the hash.
        let body_start = text
            .match_indices('\n')
            .nth(1)
            .map_or(text.len(), |(i, _)| i + 1);
        let body = &text[body_start..];
        let actual = sha256_hex(body.as_bytes());
        if actual != declared {
            return Err(ReadError::Corrupt { declared, actual });
        }

        let mut code_commit = None;
        let mut data_hash = None;
        let mut config_hash = None;
        let mut label = String::new();
        let mut pnl = None;
        let mut declared_fills = None;
        let mut fills = Vec::new();

        for (i, line) in body.lines().enumerate() {
            let line_no = i + 3;
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fail = |why: &str| ReadError::Line {
                line: line_no,
                why: format!("{why}: {line:?}"),
            };
            let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "code-commit" => code_commit = Some(rest.to_string()),
                "data-sha256" => data_hash = Some(rest.to_string()),
                "config-sha256" => config_hash = Some(rest.to_string()),
                "label" => label = rest.to_string(),
                "pnl" => {
                    pnl = Some(
                        rest.parse::<f64>()
                            .map_err(|_| fail("pnl is not a number"))?,
                    );
                }
                "fills" => {
                    declared_fills = Some(
                        rest.parse::<usize>()
                            .map_err(|_| fail("fill count is not a number"))?,
                    );
                }
                "fill" => fills.push(parse_fill(rest, line_no)?),
                other => {
                    return Err(fail(&format!("unknown field {other:?}")));
                }
            }
        }

        let Some(declared_fills) = declared_fills else {
            return Err(ReadError::Truncated);
        };
        if declared_fills != fills.len() {
            // The hash already catches truncation, but a count that
            // disagrees with the rows is a different defect — a writer
            // bug rather than a transport one — and saying which is
            // worth two lines.
            return Err(ReadError::Line {
                line: 0,
                why: format!(
                    "the file declares {declared_fills} fills and carries {}",
                    fills.len()
                ),
            });
        }

        Ok(Self {
            manifest: RunManifest {
                code_commit: code_commit.ok_or(ReadError::NoIdentity("code commit"))?,
                data_hash: data_hash.ok_or(ReadError::NoIdentity("input data hash"))?,
                config_hash: config_hash.ok_or(ReadError::NoIdentity("configuration hash"))?,
                label,
            },
            output: RunOutput {
                fills,
                pnl: pnl.ok_or(ReadError::NoIdentity("realized P&L"))?,
            },
        })
    }
}

fn parse_fill(rest: &str, line: usize) -> Result<Fill, ReadError> {
    let fail = |why: &str| ReadError::Line {
        line,
        why: format!("{why}: {rest:?}"),
    };
    let mut parts = rest.split(' ');
    let ts = parts
        .next()
        .ok_or_else(|| fail("no timestamp"))?
        .parse::<i64>()
        .map_err(|_| fail("timestamp is not a number"))?;
    let symbol = parts.next().ok_or_else(|| fail("no symbol"))?.to_string();
    let side = match parts.next() {
        Some("buy") => Side::Buy,
        Some("sell") => Side::Sell,
        _ => return Err(fail("side must be buy or sell")),
    };
    let price = parts
        .next()
        .ok_or_else(|| fail("no price"))?
        .parse::<i64>()
        .map_err(|_| fail("price is not an integer number of ticks"))?;
    let qty = parts
        .next()
        .ok_or_else(|| fail("no quantity"))?
        .parse::<i64>()
        .map_err(|_| fail("quantity is not an integer number of lots"))?;
    let tag = match parts.next() {
        None | Some("-") => None,
        Some(t) => Some(t.to_string()),
    };
    Ok(Fill {
        ts: Nanos(ts),
        symbol,
        side,
        price: PriceTicks(price),
        qty: QtyLots(qty),
        tag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> Run {
        Run::new(
            RunManifest::from_content("abc123", b"the ticks", b"the config", "L0"),
            RunOutput::new(
                vec![
                    Fill::new(
                        1_700_000_000_000_000_000,
                        "BTCUSDT",
                        Side::Buy,
                        6_000_000,
                        5,
                    ),
                    Fill::new(
                        1_700_000_060_000_000_000,
                        "BTCUSDT",
                        Side::Sell,
                        6_010_000,
                        5,
                    )
                    .with_tag("exit"),
                ],
                123.456,
            ),
        )
    }

    /// The claim: a baseline written today is the same baseline when
    /// read back, or every conclusion drawn from it is about something
    /// else.
    #[test]
    fn a_run_survives_the_round_trip() {
        let original = run();
        let back = Run::parse(&original.render()).expect("readable");
        assert_eq!(back, original);
    }

    /// A run that made no trades is a real and interesting outcome — a
    /// strategy whose filter never opened — and must not be confused
    /// with a file that never finished writing.
    #[test]
    fn a_run_with_no_fills_round_trips_as_a_run_with_no_fills() {
        let r = Run::new(
            RunManifest::from_content("c", b"d", b"g", ""),
            RunOutput::new(Vec::new(), 0.0),
        );
        let back = Run::parse(&r.render()).expect("readable");
        assert_eq!(back.output.fills.len(), 0);
        assert_eq!(back, r);
    }

    /// The failure this format exists to make impossible. A baseline
    /// truncated by a full disk, compared as "matching for the part that
    /// survived", is worse than no baseline at all.
    #[test]
    fn a_truncated_file_is_refused_rather_than_compared() {
        let text = run().render();
        for cut in [text.len() / 2, text.len() - 20, text.len() - 1] {
            let err = Run::parse(&text[..cut]).expect_err("truncated");
            assert!(
                matches!(err, ReadError::Corrupt { .. } | ReadError::Truncated),
                "a truncation at {cut} must be refused, got {err:?}"
            );
        }
    }

    /// And an edited one. Not tamper resistance — anybody who can edit
    /// the file can edit the hash — but a P&L quietly corrected in a
    /// text editor is the realistic way a baseline stops describing the
    /// run that produced it.
    #[test]
    fn an_edited_body_is_refused() {
        let text = run().render().replace("pnl 123.456", "pnl 999.999");
        assert!(matches!(Run::parse(&text), Err(ReadError::Corrupt { .. })));
    }

    /// The identity triple is the point of D13. A run missing any part
    /// of it cannot be compared, and the error says which part rather
    /// than calling the file malformed — a run with no data hash is not
    /// a formatting problem, it is a run whose experiment is unknown.
    #[test]
    fn a_run_without_its_identity_is_refused_by_element() {
        for (field, expected) in [
            ("code-commit", "code commit"),
            ("data-sha256", "input data hash"),
            ("config-sha256", "configuration hash"),
        ] {
            let r = run();
            // Rebuild the file without one identity line, re-hashing so
            // the failure is the missing field and not the checksum.
            let body: String = r
                .body()
                .lines()
                .filter(|l| !l.starts_with(field))
                .map(|l| format!("{l}\n"))
                .collect();
            let text = format!(
                "openquanter-run {VERSION}\nbody-sha256 {}\n{body}",
                sha256_hex(body.as_bytes())
            );
            match Run::parse(&text) {
                Err(ReadError::NoIdentity(what)) => assert_eq!(what, expected),
                other => panic!("dropping {field} gave {other:?}"),
            }
        }
    }

    /// A version this build does not know is a file it must not guess
    /// at. The alternative — reading what it recognises and ignoring the
    /// rest — is how a baseline written by a newer engine gets compared
    /// against an older one and reports a regression that is a format
    /// difference.
    #[test]
    fn an_unknown_version_is_refused_rather_than_partially_read() {
        let text = run().render().replacen(
            &format!("openquanter-run {VERSION}"),
            "openquanter-run 99",
            1,
        );
        assert_eq!(Run::parse(&text), Err(ReadError::Version("99".to_string())));
        assert_eq!(Run::parse("some other file\n"), Err(ReadError::NoVersion));
        assert_eq!(Run::parse(""), Err(ReadError::NoVersion));
    }

    /// A count that disagrees with the rows is a writer bug rather than
    /// a transport one, and the hash would not catch it — the file is
    /// internally consistent and wrong.
    #[test]
    fn a_fill_count_that_disagrees_with_the_rows_is_caught() {
        let r = run();
        let body = r.body().replace("fills 2", "fills 7");
        let text = format!(
            "openquanter-run {VERSION}\nbody-sha256 {}\n{body}",
            sha256_hex(body.as_bytes())
        );
        match Run::parse(&text) {
            Err(ReadError::Line { why, .. }) => {
                assert!(
                    why.contains("declares 7") && why.contains("carries 2"),
                    "{why}"
                );
            }
            other => panic!("expected a count mismatch, got {other:?}"),
        }
    }

    /// A tag that is absent and a tag that is the empty string are
    /// different, and a column left blank could not tell them apart.
    #[test]
    fn an_absent_tag_and_an_empty_one_stay_different() {
        let r = Run::new(
            RunManifest::from_content("c", b"d", b"g", ""),
            RunOutput::new(
                vec![
                    Fill::new(1, "X", Side::Buy, 1, 1),
                    Fill::new(2, "X", Side::Buy, 1, 1).with_tag("t"),
                ],
                0.0,
            ),
        );
        let back = Run::parse(&r.render()).expect("readable");
        assert_eq!(back.output.fills[0].tag, None);
        assert_eq!(back.output.fills[1].tag.as_deref(), Some("t"));
    }

    /// The fill section is columns of plain text, so the archive is
    /// readable by tools that have never heard of this program — which
    /// is the reason the format is text at all.
    #[test]
    fn the_fill_section_is_readable_without_this_program() {
        let text = run().render();
        let rows: Vec<&str> = text.lines().filter(|l| l.starts_with("fill ")).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], "fill 1700000000000000000 BTCUSDT buy 6000000 5 -");
        assert!(
            text.contains("# ts symbol side price qty tag"),
            "the columns must be named in the file"
        );
    }

    /// Writing is deterministic: the same run twice is the same bytes,
    /// or a baseline committed to a repository churns on every rebuild
    /// and stops being diffable.
    #[test]
    fn rendering_is_deterministic() {
        assert_eq!(run().render(), run().render());
    }

    /// The end-to-end reason this exists: a baseline read from a file
    /// drives the same comparison an in-memory one does, including the
    /// `invalidated` verdict when the data moved.
    #[test]
    fn a_baseline_read_from_a_file_still_invalidates_when_the_data_moved() {
        let baseline = Run::parse(&run().render()).expect("readable");

        let same_data = Run::new(
            RunManifest::from_content("def456", b"the ticks", b"the config", "L0"),
            RunOutput::new(baseline.output.fills.clone(), 123.456),
        );
        let report = crate::diff::compare(
            &baseline.manifest,
            &baseline.output,
            &same_data.manifest,
            &same_data.output,
        );
        assert!(
            report.baseline_status.permits_behavioral_conclusions(),
            "only the code moved: {:?}",
            report.baseline_status
        );

        let other_data = Run::new(
            RunManifest::from_content("abc123", b"different ticks", b"the config", "L0"),
            RunOutput::new(baseline.output.fills.clone(), 123.456),
        );
        let report = crate::diff::compare(
            &baseline.manifest,
            &baseline.output,
            &other_data.manifest,
            &other_data.output,
        );
        assert!(
            !report.baseline_status.permits_behavioral_conclusions(),
            "the data moved, so nothing about the engine can be concluded"
        );
    }
}
