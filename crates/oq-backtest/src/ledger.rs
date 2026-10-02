//! The trial ledger on disk.
//!
//! [`crate::sweep`] deflates by every configuration tried on a question,
//! and that count has to survive the program that ran the sweep. This is
//! the file that carries it: load before a sweep, save after, and the
//! next sweep — tomorrow's, or the one after an abandoned run — starts
//! from the full count instead of from zero.
//!
//! One ledger per research question: a strategy family on a body of
//! data. Sharing one across unrelated questions would deflate each by
//! the other's search; starting a fresh one to escape the count is the
//! thing it exists to make visible, since the file is where the count
//! went.

use std::io::Write as _;
use std::path::Path;

use oq_stats::TrialRegistry;

/// Read the ledger at `path`, or an empty registry when there is none yet.
///
/// # Errors
///
/// The file exists but cannot be read, or does not parse. A ledger that
/// cannot be read is an error and not an empty registry: reading it as
/// empty would reset the count the next save writes back.
pub fn load(path: &Path) -> Result<TrialRegistry, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            TrialRegistry::from_ledger(&text).map_err(|e| format!("{}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TrialRegistry::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Write `registry` to `path`, replacing it whole.
///
/// Written beside it and renamed over it, so an interrupted save leaves
/// the previous ledger rather than half of a new one.
///
/// # Errors
///
/// Any I/O error writing, syncing or renaming the file.
pub fn save(path: &Path, registry: &TrialRegistry) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(registry.to_ledger().as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_stats::trials::Trial;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("oq-ledger-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    #[test]
    fn a_missing_ledger_is_an_empty_one_and_a_saved_one_reads_back() {
        let d = dir("roundtrip");
        let path = d.join("question.trials");
        let mut registry = load(&path).expect("absent is empty");
        assert!(registry.is_empty());
        registry.record(Trial {
            id: "a".into(),
            sharpe: 0.2,
            n_observations: 40,
            skewness: 0.0,
            kurtosis: 3.0,
        });
        registry.record_unscored("b");
        save(&path, &registry).expect("saves");
        assert_eq!(load(&path).expect("reads"), registry);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unreadable_ledger_is_an_error_not_a_reset() {
        let d = dir("corrupt");
        let path = d.join("question.trials");
        std::fs::write(&path, "openquanter-trials 1\ntrial nonsense\n").expect("writes");
        let err = load(&path).expect_err("refused");
        assert!(err.contains("line 2"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
