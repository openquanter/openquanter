//! Parity: does this run still do what the reference run did?
//!
//! The measuring instrument, built before the thing it measures. A port,
//! a refactor, a fidelity-tier change, or a second implementation of the
//! same strategy in another language all need the same answer — not
//! "are the results close" but "which fill was the first to differ, and
//! by how much".
//!
//! Two rules shape the design:
//!
//! 1. **Exact where exactness is possible.** Prices and quantities are
//!    fixed-point integers and are compared exactly. Tolerance applies
//!    only to derived monetary values.
//! 2. **A stale baseline is stale, not violated.** Every run carries the
//!    identity triple (code commit, input data hash, configuration
//!    hash). If data or configuration moved, the report says so and
//!    concludes nothing about behavior. See [`manifest`].

pub mod attribution;
pub mod diff;
pub mod manifest;
pub mod markout;
pub mod record;
pub mod wire;

pub use diff::{Difference, FieldDifference, ParityReport, compare};
pub use manifest::{BaselineStatus, IdentityElement, RunManifest};
pub use record::{Fill, Nanos, RunOutput};

/// The first `chars` characters of `s`: how a hash or commit is shown
/// to a person.
///
/// By character, not byte. Every value shortened this way was read from
/// a file, and a byte slice through a multi-byte character panics —
/// which turned a corrupt run file into a crash while reporting it as
/// corrupt.
#[must_use]
pub fn abbreviate(s: &str, chars: usize) -> &str {
    s.char_indices().nth(chars).map_or(s, |(end, _)| &s[..end])
}

#[cfg(test)]
mod tests {
    use super::abbreviate;

    #[test]
    fn abbreviation_counts_characters_and_never_splits_one() {
        assert_eq!(abbreviate("0123456789abcdef0123", 12), "0123456789ab");
        assert_eq!(abbreviate("short", 12), "short");
        assert_eq!(abbreviate("", 12), "");
        // Each of these is three bytes: byte 12 is inside the fifth.
        assert_eq!(abbreviate("哈希哈希哈希", 4), "哈希哈希");
        assert_eq!(abbreviate("ab哈希", 3), "ab哈");
    }
}
