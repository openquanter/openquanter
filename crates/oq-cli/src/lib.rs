//! One name to remember.
//!
//! The tools were written one at a time and each got its own binary,
//! which is right for how they are built and wrong for how they are
//! found. A reader who has just cloned this repository has no way to
//! learn that `oq-book-check` exists, and nine names is nine chances to
//! remember the wrong one.
//!
//! # Why this dispatches rather than absorbs
//!
//! `oq` finds the tool on `PATH` and executes it, replacing itself.
//! It does not link the crates and re-implement their argument parsing.
//! Two reasons, and the second is the one that matters:
//!
//! - Absorbing them would give this crate every dependency any tool has
//!   — a TLS stack, a websocket client — so installing the launcher
//!   would build the venue clients whether or not you ever speak to a
//!   venue.
//! - A wrapper that re-parses arguments is a wrapper that drifts. Every
//!   flag would exist twice and one copy would fall behind, and the
//!   failure is silent: the tool does something other than what the
//!   documented flag says.
//!
//! So this crate has no dependencies at all, including on the tools it
//! launches, and the budget check pins that at zero. What it adds is a
//! list, a lookup, and an error message that names what is missing.

/// Three commands the plan asked for that are deliberately absent, and
/// why. Kept in the source rather than only in the roadmap, because this
/// is where someone looks after typing `oq backtest` and getting nothing.
///
/// - **`backtest` and `sweep`.** A strategy is compiled Rust. Running an
///   arbitrary one from a command line needs a plugin or scripting
///   boundary that does not exist, and running only the bundled ones
///   would be `cargo run --example hello` with fewer options — which the
///   quickstart already says. The subcommands would be a worse spelling
///   of something that works.
/// - **`parity`.** Comparing two runs needs both runs in a file, and a
///   run's output has no serialised format yet. Inventing one here would
///   fix the format at the command line rather than where the
///   attribution work will need it.
pub const ABSENT: &[(&str, &str)] = &[
    (
        "backtest",
        "a strategy is compiled Rust; use `cargo run --example`",
    ),
    ("sweep", "same reason as backtest"),
];

/// The tools `oq` knows about, with what each is for.
///
/// Ordered as a reader meets them: capture something, check it, convert
/// it, then trade. The order is the reason this is a list rather than a
/// map — it is documentation first and a lookup table second.
pub const TOOLS: &[(&str, &str)] = &[
    (
        "capture",
        "record a venue's streams to an archive, verbatim",
    ),
    (
        "book-check",
        "replay an archive into an order book and report breaks",
    ),
    (
        "trade-check",
        "follow a venue's own trade ids to prove nothing was missed",
    ),
    ("merge", "reconcile two archives of the same window"),
    (
        "resequence",
        "put an archive damaged by two writers back in venue order",
    ),
    (
        "ingest",
        "convert an archive into the tick format a backtest reads",
    ),
    (
        "data",
        "characterise a tick file before a backtest trusts it",
    ),
    (
        "parity",
        "compare a run against a baseline, and say when the baseline expired",
    ),
    (
        "recon",
        "read a live account and say whether it matches expectations",
    ),
    (
        "order-check",
        "prove the order path works, against a testnet",
    ),
    ("trade", "run a strategy against a venue"),
    (
        "replay",
        "read back what a live run decided, from its journal",
    ),
];

/// The binary implementing `name`, if it is one of ours.
#[must_use]
pub fn binary_for(name: &str) -> Option<String> {
    TOOLS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(n, _)| format!("oq-{n}"))
}

/// The crate that ships `name`, for an error message that can be acted
/// on.
///
/// A launcher that reports "not found" and stops leaves the reader to
/// discover that the tools ship inside three crates rather than nine.
#[must_use]
pub fn crate_for(name: &str) -> Option<&'static str> {
    Some(match name {
        "capture" | "book-check" | "trade-check" | "merge" | "resequence" => "oq-l2feed",
        "ingest" => "oq-ingest",
        "parity" => "oq-parity",
        "recon" | "order-check" => "oq-gateway",
        "data" => "oq-data",
        "trade" | "replay" => "oq-live",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_names_the_crate_that_ships_it() {
        // A missing entry would produce "install it from None", which is
        // worse than no suggestion: it reads as a bug rather than as a
        // gap.
        for (name, _) in TOOLS {
            assert!(crate_for(name).is_some(), "{name} has no crate");
            assert!(binary_for(name).is_some(), "{name} has no binary");
        }
    }

    #[test]
    fn a_tool_we_do_not_ship_is_not_invented() {
        assert_eq!(binary_for("mine-bitcoin"), None);
        assert_eq!(crate_for("mine-bitcoin"), None);
    }

    #[test]
    fn the_binary_name_is_the_subcommand_with_one_prefix() {
        // The mapping is mechanical on purpose: a table of exceptions is
        // a table someone has to keep, and this one would be all
        // exceptions and no rule.
        assert_eq!(binary_for("capture").as_deref(), Some("oq-capture"));
        assert_eq!(binary_for("book-check").as_deref(), Some("oq-book-check"));
    }

    #[test]
    fn the_list_has_no_duplicates() {
        let names: std::collections::HashSet<_> = TOOLS.iter().map(|(n, _)| n).collect();
        assert_eq!(names.len(), TOOLS.len());
    }

    #[test]
    fn every_tool_says_what_it_is_for() {
        // The list is documentation first. An entry without a description
        // is an entry that only helps someone who already knew.
        for (name, what) in TOOLS {
            assert!(what.len() > 20, "{name}: {what:?} is too short to help");
            assert!(
                !what.starts_with(name),
                "{name}: a description that repeats the name says nothing"
            );
        }
    }
}

#[cfg(test)]
mod absent_tests {
    use super::*;

    #[test]
    fn an_absent_command_is_named_rather_than_merely_missing() {
        // Someone typing `oq backtest` read a plan that mentions it. "No
        // such tool" sends them looking for a typo; naming it and saying
        // why sends them to the thing that works.
        for (name, why) in ABSENT {
            assert!(
                binary_for(name).is_none(),
                "{name} is both present and absent"
            );
            assert!(why.len() > 15, "{name}: {why:?} does not explain anything");
        }
    }

    #[test]
    fn nothing_is_both_shipped_and_absent() {
        let shipped: std::collections::HashSet<_> = TOOLS.iter().map(|(n, _)| *n).collect();
        for (name, _) in ABSENT {
            assert!(!shipped.contains(name), "{name} is in both lists");
        }
    }

    #[test]
    fn the_new_tools_name_their_crates_too() {
        for name in ["data", "replay"] {
            assert!(crate_for(name).is_some(), "{name} has no crate");
            assert!(binary_for(name).is_some(), "{name} has no binary");
        }
        assert_eq!(crate_for("data"), Some("oq-data"));
        assert_eq!(crate_for("replay"), Some("oq-live"));
    }
}
