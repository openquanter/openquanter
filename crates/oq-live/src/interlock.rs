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
//! It also does not reap stale locks. A process that died leaves one
//! behind, and clearing it is a decision — this cannot distinguish a
//! crash from a process that is alive and merely slow, and pid reuse
//! makes the obvious check wrong rather than merely unreliable. The
//! message says which file and what it claims about itself. That is the
//! same bargain `oq-journal`'s writer lock makes, deliberately.

use std::fmt;
use std::io::Write;
use std::path::PathBuf;

/// A held interlock. Released when dropped.
#[derive(Debug)]
pub struct Interlock {
    path: PathBuf,
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
             would tell you reads zero. If that process is gone, remove the \
             file.",
            self.path.display(),
            self.held_by
        )
    }
}

impl std::error::Error for Taken {}

impl Interlock {
    /// Claim `(deployment, symbol, prefix)`, or say who holds it.
    ///
    /// # Errors
    /// [`Taken`] when another live process holds the same triple. An I/O
    /// failure creating the file is also reported as taken rather than
    /// ignored: a lock that could not be established has not been
    /// established, and treating that as success is the one reading that
    /// makes the whole thing decorative.
    pub fn claim(deployment: &str, symbol: &str, prefix: &str) -> Result<Self, Taken> {
        let path = std::env::temp_dir().join(format!(
            "oq-live.{}.{}.{}.lock",
            encode(deployment),
            encode(symbol),
            encode(prefix)
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                let _ = writeln!(
                    f,
                    "pid {} trading {symbol} on {deployment} as {prefix}",
                    std::process::id()
                );
                let _ = f.sync_all();
                Ok(Self { path })
            }
            Err(e) => {
                let held_by = if e.kind() == std::io::ErrorKind::AlreadyExists {
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    let text = text.trim();
                    if text.is_empty() {
                        "nothing about itself".to_string()
                    } else {
                        text.to_string()
                    }
                } else {
                    format!("the lock could not be created: {e}")
                };
                Err(Taken { path, held_by })
            }
        }
    }

    /// The file being held, for a startup banner.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for Interlock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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

    #[test]
    fn a_second_claim_on_the_same_triple_is_refused() {
        let p = unique("same");
        let first = Interlock::claim("testnet", "BTCUSDT", &p).expect("first claim");
        let second = Interlock::claim("testnet", "BTCUSDT", &p);
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
        let first = Interlock::claim("testnet", "BTCUSDT", &p).expect("first claim");
        drop(first);
        let again = Interlock::claim("testnet", "BTCUSDT", &p);
        assert!(
            again.is_ok(),
            "a released interlock still blocked a restart"
        );
    }

    /// Any one of the three differing is a different claim.
    #[test]
    fn the_triple_is_the_key() {
        let p = unique("triple");
        let _a = Interlock::claim("testnet", "BTCUSDT", &p).expect("a");
        let b = Interlock::claim("live", "BTCUSDT", &p);
        let c = Interlock::claim("testnet", "ETHUSDT", &p);
        let d = Interlock::claim("testnet", "BTCUSDT", &format!("{p}-other"));
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
        let a = Interlock::claim("testnet", "BTC/USDT", &p).expect("slash form");
        let b = Interlock::claim("testnet", "BTC-USDT", &p);
        assert!(b.is_ok(), "BTC/USDT and BTC-USDT were merged into one lock");
        assert_ne!(a.path(), b.expect("dash form").path());
    }
}
