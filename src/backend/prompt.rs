//! Presenting an interactive login URL to the user.
//!
//! Trino's OAuth 2.0 external authentication answers the first request with a
//! login URL a human has to visit, and then issues the token once they have.
//! Core decides *whether* this connect may prompt at all: that is
//! `SQLDriverConnect`'s *DriverCompletion*, and core hands back a
//! [`Prompter`] only when the answer is yes. This module is the *how*, and the
//! only place the `open` dependency is used.

use std::sync::Arc;

use stackable_odbc_core::{errors::OdbcError, prompt::Prompter};
use trino_rust_client::{auth::RedirectHandler, error::Error as ClientError};

/// Shows a login URL by logging it and opening the system browser.
pub(crate) struct BrowserPrompter;

impl Prompter for BrowserPrompter {
    /// Logs the URL, then makes a best-effort attempt to open a browser.
    ///
    /// The log comes first and happens unconditionally, because it is the only
    /// channel that always survives: a Driver Manager discards whatever the
    /// driver writes to stderr, so `ODBC_LOG_FILE` / `ODBC_LOG_LEVEL` are what
    /// make the URL reachable at all under `isql`, Power BI or Excel.
    ///
    /// A failed browser launch is **not** an error. There may be no display, no
    /// browser, or no permission to start one, and the flow can still be
    /// completed by opening the logged URL by hand, because the client polls
    /// for the token rather than waiting on this call. Reporting `IM008` here
    /// would fail a connect that was still perfectly able to succeed.
    fn present_url(&self, url: &str) -> Result<(), OdbcError> {
        tracing::info!(%url, "OAuth2 login required: open this URL to authenticate");
        if let Err(e) = open::that(url) {
            tracing::warn!(
                %e,
                "could not open a browser; the login URL logged above has to be opened manually"
            );
        }
        Ok(())
    }
}

/// Adapts core's [`Prompter`] to the client's [`RedirectHandler`].
///
/// The client owns the OAuth 2.0 flow and calls its handler once per login;
/// core owns the decision that a login may be shown at all. This is the seam
/// between them, and it exists so the `Prompter` core handed back is the object
/// actually used, not an equivalent this driver rebuilt for itself.
pub(crate) struct ClientRedirect(Arc<dyn Prompter>);

impl ClientRedirect {
    /// Wrap the [`Prompter`] core handed back, so the client's login URL
    /// reaches it.
    pub(crate) fn new(prompter: Arc<dyn Prompter>) -> Self {
        Self(prompter)
    }
}

impl RedirectHandler for ClientRedirect {
    fn redirect(&self, url: &str) -> Result<(), ClientError> {
        self.0
            .present_url(url)
            .map_err(|e| ClientError::OAuth2(format!("could not present the login URL: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use stackable_odbc_core::types::SqlState;

    use super::*;

    /// Records what it was asked to present, so the adapter can be tested
    /// without a browser.
    struct Recording(Mutex<Vec<String>>);

    impl Prompter for Recording {
        fn present_url(&self, url: &str) -> Result<(), OdbcError> {
            match self.0.lock() {
                Ok(mut seen) => seen.push(url.to_string()),
                Err(_) => {
                    return Err(OdbcError::general(
                        "recording prompter poisoned",
                        SqlState::general_error(),
                    ));
                }
            }
            Ok(())
        }
    }

    struct Failing;

    impl Prompter for Failing {
        fn present_url(&self, _url: &str) -> Result<(), OdbcError> {
            Err(OdbcError::general("no display", SqlState::general_error()))
        }
    }

    #[test]
    fn the_adapter_passes_the_url_through_to_the_prompter() {
        let recording = Arc::new(Recording(Mutex::new(Vec::new())));
        let adapter = ClientRedirect::new(recording.clone());

        assert!(
            adapter
                .redirect("https://trino.example.com/oauth2/token/init/abc")
                .is_ok()
        );

        let seen = recording.0.lock().expect("not poisoned");
        assert_eq!(
            seen.as_slice(),
            ["https://trino.example.com/oauth2/token/init/abc"]
        );
    }

    /// A prompter that cannot show anything must reach the client as an OAuth2
    /// failure, not be swallowed: the client is what turns it into a failed
    /// login rather than an indefinite poll.
    #[test]
    fn a_prompter_failure_becomes_a_client_oauth2_error() {
        let adapter = ClientRedirect::new(Arc::new(Failing));

        let err = adapter
            .redirect("https://trino.example.com/oauth2/token/init/abc")
            .expect_err("a failing prompter must not report success");

        assert!(
            matches!(&err, ClientError::OAuth2(m) if m.contains("no display")),
            "expected the cause to survive into the client error, got {err:?}"
        );
    }
}
