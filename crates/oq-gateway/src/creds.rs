//! API credentials.
//!
//! Read from the environment, never from a config file this repository
//! could accidentally track, and never printed. `Debug` is implemented by
//! hand so that a struct containing credentials cannot leak them into a
//! log line — the derived one would print the secret in full, and the
//! places that print a whole request on failure are exactly the places
//! that matter.

use core::fmt;

/// A key pair for a venue account.
#[derive(Clone)]
pub struct Credentials {
    key: String,
    secret: String,
    /// A third secret some venues require alongside the pair.
    ///
    /// Optional because most do not have one, and `None` rather than an
    /// empty string because "this venue has no passphrase" and "the
    /// passphrase is blank" are different states — the second is a
    /// misconfiguration and must not be silently signed with.
    passphrase: Option<String>,
}

impl Credentials {
    /// Build from an explicit pair.
    #[must_use]
    pub fn new(key: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            secret: secret.into(),
            passphrase: None,
        }
    }

    /// Attach the third secret a venue such as OKX requires.
    ///
    /// # Errors
    /// When the passphrase is blank. A venue signs successfully with a
    /// blank one and then refuses the request for a reason that names
    /// the signature, which is an hour spent looking in the wrong place.
    pub fn with_passphrase(mut self, passphrase: impl Into<String>) -> Result<Self, String> {
        let p = passphrase.into();
        if p.trim().is_empty() {
            return Err("the passphrase must not be empty".to_string());
        }
        self.passphrase = Some(p);
        Ok(self)
    }

    /// The third secret, when the venue has one.
    #[must_use]
    pub(crate) fn passphrase(&self) -> Option<&str> {
        self.passphrase.as_deref()
    }

    /// Read from `OQ_VENUE_KEY` and `OQ_VENUE_SECRET`.
    ///
    /// # Errors
    /// Names whichever variable is missing. Failing here is preferable to
    /// signing with an empty secret, which the venue rejects with a
    /// message about the signature rather than about the configuration.
    pub fn from_env() -> Result<Self, String> {
        let key =
            std::env::var("OQ_VENUE_KEY").map_err(|_| "OQ_VENUE_KEY is not set".to_string())?;
        let secret = std::env::var("OQ_VENUE_SECRET")
            .map_err(|_| "OQ_VENUE_SECRET is not set".to_string())?;
        let creds = Self::checked(key, secret)?;
        // Absent rather than blank when unset: a venue with no
        // passphrase must not be handed an empty one, and a venue that
        // needs one must fail naming it rather than naming a signature.
        match std::env::var("OQ_VENUE_PASSPHRASE") {
            Ok(p) => creds.with_passphrase(p),
            Err(_) => Ok(creds),
        }
    }

    /// Validate a pair before it is used to sign anything.
    ///
    /// Separate from [`Credentials::from_env`] so the rule can be tested
    /// without writing to the process environment, which is racy across
    /// parallel tests and unsafe to do at all under the 2024 edition.
    ///
    /// # Errors
    /// When either half is blank.
    pub fn checked(key: String, secret: String) -> Result<Self, String> {
        if key.trim().is_empty() || secret.trim().is_empty() {
            return Err("OQ_VENUE_KEY and OQ_VENUE_SECRET must not be empty".to_string());
        }
        Ok(Self::new(key, secret))
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    #[must_use]
    pub(crate) fn secret_bytes(&self) -> &[u8] {
        self.secret.as_bytes()
    }
}

impl fmt::Debug for Credentials {
    /// Shows enough of the key to tell two accounts apart, and nothing of
    /// the secret. A derived `Debug` would print both, and the first time
    /// that mattered would be in a log somebody had already shared.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let head: String = self.key.chars().take(6).collect();
        write!(f, "Credentials {{ key: {head}…, secret: <redacted> }}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_secret() {
        let c = Credentials::new("keykeykeykey", "supersecretvalue");
        let shown = format!("{c:?}");
        assert!(
            !shown.contains("supersecretvalue"),
            "secret leaked: {shown}"
        );
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn debug_shows_enough_key_to_identify_the_account() {
        let c = Credentials::new("abcdef0123456789", "s");
        assert!(format!("{c:?}").contains("abcdef"));
    }

    /// Signing with a blank secret produces a signature the venue
    /// rejects, and the error it returns talks about signatures rather
    /// than about configuration — an hour spent looking in the wrong
    /// place.
    /// A venue that wants three secrets and is given two blank-padded
    /// ones signs correctly and is refused, and its message talks about
    /// the signature.
    #[test]
    fn a_blank_passphrase_is_refused_rather_than_signed_with() {
        let c = Credentials::new("k", "s");
        assert!(c.passphrase().is_none(), "absent, not empty");
        assert!(Credentials::new("k", "s").with_passphrase("  ").is_err());
        let ok = Credentials::new("k", "s")
            .with_passphrase("phrase")
            .expect("valid");
        assert_eq!(ok.passphrase(), Some("phrase"));
    }

    #[test]
    fn debug_never_shows_the_passphrase() {
        let c = Credentials::new("keykeykey", "sec")
            .with_passphrase("thepassphrase")
            .expect("valid");
        assert!(!format!("{c:?}").contains("thepassphrase"));
    }

    #[test]
    fn a_blank_half_is_refused_rather_than_used() {
        assert!(Credentials::checked("k".into(), "   ".into()).is_err());
        assert!(Credentials::checked("".into(), "s".into()).is_err());
        assert!(Credentials::checked("k".into(), "s".into()).is_ok());
    }
}
