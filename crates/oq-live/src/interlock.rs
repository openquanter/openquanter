//! One trading process per account, enforced before anything is sent.
//!
//! # The failure
//!
//! Two processes trading the same symbol on the same account with the
//! same client-id prefix do not merely duplicate each other. They
//! cannot tell each other apart: `IdScheme::owns` is a prefix test, so
//! each one reads the other's resting orders as **its own**. Every
//! consequence follows from that single fact.
//!
//! - `cancel_all` at shutdown withdraws the other process's orders.
//! - Recovery at startup finds orders it never sent, matching its own
//!   prefix, and reconciles against them.
//! - The `foreign_orders` metric — the one that exists to say *this
//!   account is shared* — reads **zero**, because nothing looks foreign.
//!
//! That last one is why this is a lock and not a warning. The
//! instrument that would have told an operator is precisely the
//! instrument this failure blinds.
//!
//! # What is exclusive
//!
//! The triple `(deployment, symbol, id_prefix)` — the three facts that
//! together decide which orders a process will claim. Two runs
//! differing in any of them can coexist: different venues, different
//! contracts, or deliberately partitioned id space.
//!
//! # What this does not do
//!
//! **It is host-local.** Two processes on two machines against one
//! account are not caught here and cannot be: the only authority that
//! could answer is the venue, and it has no notion of which of its
//! clients ought to be running. The `foreign_orders` metric is the
//! instrument for that case, and it works there because a different
//! host is very unlikely to be using the same prefix.
//!
//! # Where it lives, and how it is held
//!
//! In the state directory, beside the reserved order ids it protects, and
//! held as an operating-system lock on an open file for as long as the
//! process lives. It used to be a file created in the shared temporary
//! directory and deleted on exit, which failed three ways: a `/tmp`
//! cleaner or a private `/tmp` let a second process claim the same ids; any
//! user on the host could pre-create the file and stop the trader starting,
//! or plant a link there; and a crash left the file behind, so every
//! restart needed someone to remove it — the step operators end up
//! scripting, at which point the lock protects nothing. A held lock is
//! released by the kernel when its holder dies, however it dies, so none of
//! that can happen. The file itself is left in place: removing it would let
//! a second process lock a new file while the first still holds the old
//! one.

use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

/// A held interlock. Released when dropped, or when the process ends.
#[derive(Debug)]
pub struct Interlock {
    path: PathBuf,
    /// The open, locked file. Dropping it releases the lock.
    _held: File,
}

/// Somebody else is already trading this.
#[derive(Debug)]
pub struct Taken {
    /// The lock file, so an operator can look at it.
    pub path: PathBuf,
    /// Whatever the holder wrote about itself, verbatim.
    pub held_by: String,
}

impl fmt::Display for Taken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "another process is already trading this account: {} says {}. \
             Two processes sharing an id prefix read each other's orders as \
             their own — each will cancel the other's, and the metric that \
             would tell you reads zero. The lock is released when that \
             process ends; stop it rather than removing the file.",
            self.path.display(),
            self.held_by
        )
    }
}

impl std::error::Error for Taken {}

impl Interlock {
    /// Claim `(deployment, symbol, prefix)` in `dir`, or say who holds it.
    ///
    /// `dir` is the process's durable state directory — the one its order
    /// ids are reserved in — and is created owner-only when missing.
    ///
    /// # Errors
    /// [`Taken`] when another live process holds the same triple. An I/O
    /// failure opening or locking the file is also reported as taken rather
    /// than ignored: a lock that could not be established has not been
    /// established, and treating that as success is the one reading that
    /// makes the whole thing decorative.
    pub fn claim(dir: &Path, deployment: &str, symbol: &str, prefix: &str) -> Result<Self, Taken> {
        let path = dir.join(format!(
            "oq-live.{}.{}.{}.lock",
            encode(deployment),
            encode(symbol),
            encode(prefix)
        ));
        let failed = |why: String| Taken {
            path: path.clone(),
            held_by: format!("the lock could not be established: {why}"),
        };
        create_private_dir(dir).map_err(|e| failed(e.to_string()))?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| failed(e.to_string()))?;
        match file.try_lock() {
            Ok(()) => {
                let _ = file.set_len(0);
                let _ = writeln!(
                    file,
                    "pid {} trading {symbol} on {deployment} as {prefix}",
                    std::process::id()
                );
                let _ = file.sync_all();
                Ok(Self { path, _held: file })
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                // Read through the handle already open, not the path: what
                // the holder wrote, and nothing a name could be pointed at.
                let mut text = String::new();
                let _ = file.rewind();
                let _ = Read::by_ref(&mut file).take(1024).read_to_string(&mut text);
                let text = text.trim();
                Err(Taken {
                    held_by: if text.is_empty() {
                        "nothing about itself".to_string()
                    } else {
                        text.to_string()
                    },
                    path,
                })
            }
            Err(std::fs::TryLockError::Error(e)) => Err(failed(e.to_string())),
        }
    }

    /// The file being held, for a startup banner.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// Create `dir` readable by its owner only, if it is missing.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Make a component safe in a filename **without merging two of them.**
///
/// Sanitising by replacing every awkward character with `_` would map
/// `BTC/USDT` and `BTC-USDT` onto one lock, and a lock shared by two
/// different contracts refuses a run that should have been allowed.
/// Percent-encoding is injective, so distinct inputs stay distinct.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(tag: &str) -> String {
        format!("test-{}-{tag}", std::process::id())
    }

    fn dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oq-interlock-{}-{tag}", std::process::id()))
    }

    #[test]
    fn a_second_claim_on_the_same_triple_is_refused() {
        let p = unique("same");
        let first = Interlock::claim(&dir(&p), "testnet", "BTCUSDT", &p).expect("first claim");
        let second = Interlock::claim(&dir(&p), "testnet", "BTCUSDT", &p);
        assert!(second.is_err(), "two processes claimed one account");
        let taken = second.unwrap_err();
        assert!(
            taken.held_by.contains("BTCUSDT"),
            "the refusal should say what is held: {}",
            taken.held_by
        );
        drop(first);
    }

    /// Releasing must actually release. A lock that outlived its holder
    /// would make every restart need manual intervention, and an
    /// operator who has to `rm` a file to restart will eventually script
    /// the `rm` — at which point the lock protects nothing.
    #[test]
    fn releasing_lets_the_next_run_start() {
        let p = unique("release");
        let first = Interlock::claim(&dir(&p), "testnet", "BTCUSDT", &p).expect("first claim");
        drop(first);
        let again = Interlock::claim(&dir(&p), "testnet", "BTCUSDT", &p);
        assert!(
            again.is_ok(),
            "a released interlock still blocked a restart"
        );
    }

    /// Any one of the three differing is a different claim.
    #[test]
    fn the_triple_is_the_key() {
        let p = unique("triple");
        let _a = Interlock::claim(&dir(&p), "testnet", "BTCUSDT", &p).expect("a");
        let b = Interlock::claim(&dir(&p), "live", "BTCUSDT", &p);
        let c = Interlock::claim(&dir(&p), "testnet", "ETHUSDT", &p);
        let d = Interlock::claim(&dir(&p), "testnet", "BTCUSDT", &format!("{p}-other"));
        assert!(b.is_ok(), "a different deployment was refused");
        assert!(c.is_ok(), "a different symbol was refused");
        assert!(d.is_ok(), "a different id prefix was refused");
    }

    /// Two symbols that sanitise to the same string must not share a
    /// lock. A false collision refuses a run that was legitimate, and
    /// the operator has no way to see why.
    #[test]
    fn symbols_that_differ_only_in_punctuation_do_not_collide() {
        let p = unique("punct");
        let a = Interlock::claim(&dir(&p), "testnet", "BTC/USDT", &p).expect("slash form");
        let b = Interlock::claim(&dir(&p), "testnet", "BTC-USDT", &p);
        assert!(b.is_ok(), "BTC/USDT and BTC-USDT were merged into one lock");
        assert_ne!(a.path(), b.expect("dash form").path());
    }
}

#[cfg(test)]
mod held_by_the_kernel {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oq-interlock-k-{}-{tag}", std::process::id()))
    }

    /// A file left behind by a process that died is not a lock. Before,
    /// it was: every crash meant a restart refused until someone removed
    /// it by hand.
    #[test]
    fn a_file_left_by_a_crashed_process_does_not_block_a_restart() {
        let d = dir("stale");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("oq-live.testnet.BTCUSDT.stale.lock"),
            "pid 1 trading BTCUSDT on testnet as stale\n",
        )
        .unwrap();
        let held = Interlock::claim(&d, "testnet", "BTCUSDT", "stale").expect("not held by anyone");
        let text = std::fs::read_to_string(held.path()).unwrap();
        assert!(text.contains(&std::process::id().to_string()), "{text}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The lock lives where it is asked to, never in the shared
    /// temporary directory, and the directory is its owner's alone.
    #[test]
    fn the_lock_lives_in_the_state_directory() {
        let d = dir("where").join("oq-live");
        let _ = std::fs::remove_dir_all(dir("where"));
        let held = Interlock::claim(&d, "testnet", "BTCUSDT", "where").expect("claimed");
        assert_eq!(held.path().parent(), Some(d.as_path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{mode:o}");
        }
        let _ = std::fs::remove_dir_all(dir("where"));
    }

    /// Releasing does not remove the file: removing it would let a second
    /// process lock a fresh file while the first still held the old one.
    #[test]
    fn releasing_leaves_the_file_and_frees_the_lock() {
        let d = dir("keep");
        let _ = std::fs::remove_dir_all(&d);
        let held = Interlock::claim(&d, "testnet", "BTCUSDT", "keep").expect("claimed");
        let path = held.path().to_path_buf();
        drop(held);
        assert!(path.exists());
        assert!(Interlock::claim(&d, "testnet", "BTCUSDT", "keep").is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }
}
