//! The one HTTP client every venue adapter uses.
//!
//! Built in one place because the settings that matter here are safety
//! settings, and seven copies of a builder are seven places to forget one.
//!
//! - **No redirects.** Every venue authenticates with headers of its own
//!   — `X-MBX-APIKEY`, `OK-ACCESS-KEY` and `OK-ACCESS-PASSPHRASE`, Kraken's
//!   `APIKey` — and a redirect keeps every header but `Authorization` and
//!   `Cookie`. Following one would send the key, and for OKX two of its
//!   three secrets, to whatever host the 3xx named. A venue's API does not
//!   redirect, so one arriving is a fault to report, not a path to take;
//!   and a POST followed as a GET would read some other page's answer as
//!   the outcome of an order.
//! - **HTTPS only.** Every base URL is a hard-coded `https://` constant; this
//!   makes a plain-text request an error even if one ever was not.
//! - **A response is read, not raised.** A refusal's body says why, and the
//!   error variant would carry only the status.
//! - **Bounded.** A request that never answers ends, and is reported.

use std::time::Duration;

/// How long one request may take end to end.
///
/// Generous, because the alternative is worse: a read that times out is
/// indistinguishable from one that failed. Measured on the link this runs
/// over, the same request took 0.7 s and 4.4 s a second apart.
pub const TIMEOUT: Duration = Duration::from_secs(45);

/// A client for a venue's REST API.
#[must_use]
pub fn venue_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .max_redirects(0)
        .https_only(true)
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings, read back from the client every adapter holds.
    #[test]
    fn the_venue_client_follows_no_redirect_and_speaks_only_https() {
        let agent = venue_agent();
        assert_eq!(agent.config().max_redirects(), 0);
        assert!(agent.config().https_only());
    }

    /// A plain-text request is refused before it is sent, whatever the
    /// address.
    #[test]
    fn a_plain_text_request_is_refused() {
        let err = venue_agent()
            .get("http://127.0.0.1:1/")
            .call()
            .expect_err("refused");
        assert!(
            err.to_string().to_lowercase().contains("https"),
            "refused for being plain text, not for the port: {err}"
        );
    }
}
