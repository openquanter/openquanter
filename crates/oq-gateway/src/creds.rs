//! API credentials.
//!
//! Read from the process's environment — systemd credentials first, then
//! environment variables — never from a config file this repository could
//! accidentally track, and never printed. `Debug` is implemented by
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

    /// Read `OQ_VENUE_KEY`, `OQ_VENUE_SECRET` and, if present,
    /// `OQ_VENUE_PASSPHRASE`.
    ///
    /// Each is taken from a file of that name in `$CREDENTIALS_DIRECTORY`
    /// when there is one — where systemd's `LoadCredential=` puts them —
    /// and otherwise from the environment variable. The file is the one
    /// to use in a deployment: an environment variable is readable in
    /// `/proc/<pid>/environ` by anything running as the same user and is
    /// inherited by every child, while a credential is readable by this
    /// service alone and by nothing it starts.
    ///
    /// # Errors
    /// Names whichever value is missing. Failing here is preferable to
    /// signing with an empty secret, which the venue rejects with a
    /// message about the signature rather than about the configuration.
    pub fn from_env() -> Result<Self, String> {
        let dir = std::env::var_os("CREDENTIALS_DIRECTORY").map(std::path::PathBuf::from);
        Self::resolve(dir.as_deref(), |name| std::env::var(name).ok())
    }

    /// [`Credentials::from_env`] with the environment passed in, so it
    /// can be tested without writing to the process environment.
    fn resolve(
        dir: Option<&std::path::Path>,
        var: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let read = |name: &str| -> Result<Option<String>, String> {
            if let Some(dir) = dir {
                let path = dir.join(name);
                match std::fs::read_to_string(&path) {
                    // A credential file ends with the newline whoever
                    // wrote it left; signing with it fails as a bad
                    // signature, not as a bad file.
                    Ok(v) => return Ok(Some(v.trim_end_matches(['\n', '\r']).to_string())),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    // Present and unreadable is a broken deployment, not
                    // a cue to go looking somewhere else.
                    Err(e) => return Err(format!("{}: {e}", path.display())),
                }
            }
            Ok(var(name))
        };
        let missing = |name: &str| {
            format!(
                "{name} is not set: neither a credential in $CREDENTIALS_DIRECTORY nor an environment variable"
            )
        };
        let key = read("OQ_VENUE_KEY")?.ok_or_else(|| missing("OQ_VENUE_KEY"))?;
        let secret = read("OQ_VENUE_SECRET")?.ok_or_else(|| missing("OQ_VENUE_SECRET"))?;
        let creds = Self::checked(key, secret)?;
        // Absent rather than blank when unset: a venue with no
        // passphrase must not be handed an empty one, and a venue that
        // needs one must fail naming it rather than naming a signature.
        match read("OQ_VENUE_PASSPHRASE")? {
            Some(p) => creds.with_passphrase(p),
            None => Ok(creds),
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

    fn cred_dir(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oq-creds-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        for (file, value) in files {
            std::fs::write(dir.join(file), value).expect("write");
        }
        dir
    }

    /// A systemd credential wins over an environment variable, and loses
    /// its trailing newline.
    #[test]
    fn a_credential_file_is_read_before_the_environment() {
        let dir = cred_dir(
            "files",
            &[
                ("OQ_VENUE_KEY", "filekey\n"),
                ("OQ_VENUE_SECRET", "filesecret\n"),
            ],
        );
        let c = Credentials::resolve(Some(&dir), |_| Some("envvalue".into())).expect("resolves");
        assert_eq!(c.key(), "filekey");
        assert_eq!(c.secret, "filesecret");
        assert_eq!(
            c.passphrase(),
            Some("envvalue"),
            "each value falls back on its own"
        );
        let c = Credentials::resolve(Some(&dir), |_| None).expect("resolves");
        assert!(
            c.passphrase().is_none(),
            "no file and no variable is no passphrase"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without a credential directory, or without a file in it, the
    /// environment variable is used — a developer's shell, unchanged.
    #[test]
    fn the_environment_is_the_fallback() {
        let dir = cred_dir("partial", &[("OQ_VENUE_KEY", "filekey")]);
        let env = |n: &str| (n == "OQ_VENUE_SECRET").then(|| "envsecret".to_string());
        let c = Credentials::resolve(Some(&dir), env).expect("resolves");
        assert_eq!((c.key(), c.secret.as_str()), ("filekey", "envsecret"));

        let c = Credentials::resolve(None, |n: &str| Some(format!("{n}-v"))).expect("resolves");
        assert_eq!(c.key(), "OQ_VENUE_KEY-v");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_value_is_named_with_both_places_it_was_looked_for() {
        let err = Credentials::resolve(None, |_| None).expect_err("nothing set");
        assert!(
            err.contains("OQ_VENUE_KEY") && err.contains("CREDENTIALS_DIRECTORY"),
            "{err}"
        );
    }

    /// A credential that exists and cannot be read stops the start; it
    /// does not fall through to whatever the environment holds.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_credential_is_an_error_not_a_fallback() {
        let dir = cred_dir("unreadable", &[]);
        // A directory where the file should be: present, and not readable
        // as one, whoever runs the test.
        std::fs::create_dir(dir.join("OQ_VENUE_KEY")).expect("dir");
        let err = Credentials::resolve(Some(&dir), |_| Some("env".into())).expect_err("refused");
        assert!(err.contains("OQ_VENUE_KEY"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
