//! Stream identity and capture software identity.
//!
//! Both exist so the things that travel together stay together: a
//! manifest that named its stream in five loose arguments would sooner
//! or later be built with the symbol and the venue swapped.

use std::path::{Path, PathBuf};

use crate::day::UtcDay;

/// Which stream a capture covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamId {
    /// Venue identifier, e.g. the exchange name.
    pub venue: String,
    /// Instrument symbol.
    pub symbol: String,
    /// Stream name, e.g. `depth`.
    pub stream: String,
}

impl StreamId {
    /// A stream identifier.
    #[must_use]
    pub fn new(
        venue: impl Into<String>,
        symbol: impl Into<String>,
        stream: impl Into<String>,
    ) -> Self {
        Self {
            venue: venue.into(),
            symbol: symbol.into(),
            stream: stream.into(),
        }
    }

    /// Directory this stream's files live in, under `root`.
    #[must_use]
    pub fn directory(&self, root: &Path) -> PathBuf {
        root.join(&self.venue).join(&self.symbol).join(&self.stream)
    }

    /// Path of the capture file for `day`.
    #[must_use]
    pub fn file_for(&self, root: &Path, day: UtcDay) -> PathBuf {
        self.directory(root).join(format!("{day}.oqcap"))
    }

    /// Path of the manifest for `day`.
    #[must_use]
    pub fn manifest_for(&self, root: &Path, day: UtcDay) -> PathBuf {
        self.directory(root).join(format!("{day}.manifest.json"))
    }
}

/// Which build produced a capture.
///
/// Archived alongside the data because "which version wrote this" is
/// unanswerable later otherwise, and it is the first question asked when
/// an archive looks wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Software {
    /// Human-readable version.
    pub version: String,
    /// Commit the capture software was built from.
    pub commit: String,
}

impl Software {
    /// Capture software identity.
    #[must_use]
    pub fn new(version: impl Into<String>, commit: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            commit: commit.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_the_documented_layout() {
        let stream = StreamId::new("venue", "SYM", "depth");
        let root = Path::new("/archive/raw");
        assert!(
            stream
                .file_for(root, UtcDay(20_000))
                .ends_with("venue/SYM/depth/2024-10-04.oqcap")
        );
        assert!(
            stream
                .manifest_for(root, UtcDay(20_000))
                .ends_with("venue/SYM/depth/2024-10-04.manifest.json")
        );
    }
}
