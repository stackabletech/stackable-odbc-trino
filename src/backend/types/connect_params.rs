//! `TrinoConnectParams`: the Trino connection settings (host, port, user,
//! password, protocol, catalog, schema, TLS and JWT options) parsed from the
//! generic `stackable-odbc-core` connection-string key/value map.

use std::collections::HashSet;
use std::time::Duration;

use stackable_odbc_core::types::{ConnectParams, Redacted};

use super::super::TrinoError;

// ---------------------------------------------------------------------------
// Connection string parameter keys (Trino-specific)
// ---------------------------------------------------------------------------

pub(crate) const PARAM_HOST: &str = "host";
pub(crate) const PARAM_PORT: &str = "port";
/// Transport protocol: `"https"` (default) or `"http"`.
pub(crate) const PARAM_PROTOCOL: &str = "protocol";
/// TLS certificate verification: `"true"` (default) or `"false"`.
pub(crate) const PARAM_TLS_VERIFY: &str = "tlsverify";
/// Path to a PEM certificate file for TLS verification.
pub(crate) const PARAM_CERTIFICATE: &str = "certificate";
/// Per-request HTTP timeout in seconds. Default: 30.
pub(crate) const PARAM_QUERY_TIMEOUT: &str = "querytimeout";
/// ODBC-standard alias for [`PARAM_QUERY_TIMEOUT`].
pub(crate) const PARAM_LOGIN_TIMEOUT: &str = "logintimeout";
pub(crate) const PARAM_CATALOG: &str = "catalog";
pub(crate) const PARAM_SCHEMA: &str = "schema";
/// Name this connection reports as Trino's query source.
pub(crate) const PARAM_SOURCE: &str = "source";
/// Comma-separated Trino client tags, which select a resource group.
pub(crate) const PARAM_CLIENT_TAGS: &str = "clienttags";
/// Trino JWT bearer token (sent as `Authorization: Bearer <token>`).
pub(crate) const PARAM_ACCESS_TOKEN: &str = "accesstoken";
/// Alias for [`PARAM_ACCESS_TOKEN`].
pub(crate) const PARAM_TOKEN: &str = "token";

/// Transport used when the connection string names none.
///
/// `https`, so that an unencrypted connection is something an application
/// asked for rather than something it got by staying silent. A coordinator
/// serving plaintext needs an explicit `Protocol=http`.
const DEFAULT_PROTOCOL: &str = "https";
const DEFAULT_QUERY_TIMEOUT_SECS: u64 = 30;

/// Query source reported when the connection string names none.
///
/// Trino shows this in its query history and can route on it, so a default
/// that names the driver is what lets an operator tell this driver's traffic
/// apart. `ClientBuilder::new` would otherwise leave it as the client
/// library's own name, which identifies neither the driver nor the
/// application.
///
/// The Cargo version rides along, `name/version`, matching how HTTP
/// user-agents are conventionally spelled. An operator reading the query log
/// after a rollout can then tell one driver build from another, which is
/// exactly when the question gets asked.
pub(crate) const DEFAULT_SOURCE: &str = concat!("stackable-odbc-trino/", env!("CARGO_PKG_VERSION"));

// ---------------------------------------------------------------------------
// Typed connection parameters
// ---------------------------------------------------------------------------

/// Parsed and validated Trino connection parameters.
#[derive(Debug)]
pub(crate) struct TrinoConnectParams {
    host: String,
    port: u16,
    user: String,
    password: Redacted<Option<String>>,
    access_token: Redacted<Option<String>>,
    secure: bool,
    tls_verify: bool,
    certificate: Option<String>,
    query_timeout: Duration,
    catalog: Option<String>,
    schema: Option<String>,
    source: String,
    client_tags: HashSet<String>,
}

impl TrinoConnectParams {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn password(&self) -> Option<&str> {
        self.password.0.as_deref()
    }

    pub fn access_token(&self) -> Option<&str> {
        self.access_token.0.as_deref()
    }

    pub fn secure(&self) -> bool {
        self.secure
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn client_tags(&self) -> &HashSet<String> {
        &self.client_tags
    }

    pub fn tls_verify(&self) -> bool {
        self.tls_verify
    }

    pub fn certificate(&self) -> Option<&str> {
        self.certificate.as_deref()
    }

    pub fn query_timeout(&self) -> Duration {
        self.query_timeout
    }

    pub fn catalog(&self) -> Option<&str> {
        self.catalog.as_deref()
    }

    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }
}

impl TryFrom<&ConnectParams> for TrinoConnectParams {
    type Error = TrinoError;

    fn try_from(params: &ConnectParams) -> Result<Self, TrinoError> {
        let host = params
            .get(PARAM_HOST)
            .ok_or_else(|| TrinoError::MissingParam {
                name: PARAM_HOST.into(),
            })?;
        let port_str = params
            .get(PARAM_PORT)
            .ok_or_else(|| TrinoError::MissingParam {
                name: PARAM_PORT.into(),
            })?;
        let port: u16 = port_str.parse().map_err(|_| TrinoError::General {
            message: format!("invalid port: {port_str}"),
        })?;
        let user = params.user().map_err(|_| TrinoError::MissingParam {
            name: "user".into(),
        })?;
        let password = params.password().map(str::to_string);
        let access_token = params
            .get(PARAM_ACCESS_TOKEN)
            .or_else(|| params.get(PARAM_TOKEN))
            .map(str::to_string);
        // Matched case-insensitively and validated: an unrecognised or
        // differently-cased value must not silently fall back to plaintext,
        // because the password is only sent over a secure transport.
        let secure = match params.get(PARAM_PROTOCOL).unwrap_or(DEFAULT_PROTOCOL) {
            v if v.eq_ignore_ascii_case("https") => true,
            v if v.eq_ignore_ascii_case("http") => false,
            v => {
                return Err(TrinoError::General {
                    message: format!(
                        "invalid value for {PARAM_PROTOCOL}: {v:?}, expected \"http\" or \"https\""
                    ),
                });
            }
        };
        let tls_verify = match params.get(PARAM_TLS_VERIFY) {
            None => true,
            Some(v) if v.eq_ignore_ascii_case("true") => true,
            Some(v) if v.eq_ignore_ascii_case("false") => false,
            Some(v) => {
                return Err(TrinoError::General {
                    message: format!(
                        "invalid value for {PARAM_TLS_VERIFY}: {v:?}, expected \"true\" or \"false\""
                    ),
                });
            }
        };
        let certificate = params.get(PARAM_CERTIFICATE).map(str::to_string);
        let query_timeout_secs: u64 = match params
            .get(PARAM_QUERY_TIMEOUT)
            .or_else(|| params.get(PARAM_LOGIN_TIMEOUT))
        {
            None => DEFAULT_QUERY_TIMEOUT_SECS,
            Some(v) => v.parse().unwrap_or_else(|_| {
                tracing::warn!(
                    "invalid query timeout {:?}, using default {}s",
                    v,
                    DEFAULT_QUERY_TIMEOUT_SECS
                );
                DEFAULT_QUERY_TIMEOUT_SECS
            }),
        };

        Ok(TrinoConnectParams {
            host: host.to_string(),
            port,
            user: user.to_string(),
            password: Redacted(password),
            access_token: Redacted(access_token),
            secure,
            tls_verify,
            certificate,
            query_timeout: Duration::from_secs(query_timeout_secs),
            catalog: params.get(PARAM_CATALOG).map(str::to_string),
            schema: params.get(PARAM_SCHEMA).map(str::to_string),
            source: params
                .get(PARAM_SOURCE)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(DEFAULT_SOURCE)
                .to_string(),
            // Trino matches resource-group selectors against whole tags, so
            // surrounding space would make " bi" miss a rule written for "bi".
            // An empty element is dropped rather than sent as an empty tag.
            client_tags: params
                .get(PARAM_CLIENT_TAGS)
                .map(|raw| {
                    raw.split(',')
                        .map(str::trim)
                        .filter(|tag| !tag.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> TrinoConnectParams {
        let params = ConnectParams::parse(s).unwrap();
        TrinoConnectParams::try_from(&params).unwrap()
    }

    fn parse_err(s: &str) -> TrinoError {
        let params = ConnectParams::parse(s).unwrap();
        TrinoConnectParams::try_from(&params).unwrap_err()
    }

    #[test]
    fn source_defaults_to_the_driver_name_and_version() {
        // Trino's query history shows this. Left unset, every query from this
        // driver is indistinguishable from any other client's -- and without
        // the version, one driver build is indistinguishable from another,
        // which is what an operator needs when a regression appears in the
        // query log after a rollout.
        let p = parse("Host=localhost;Port=8080;User=admin");
        assert_eq!(
            p.source(),
            format!("stackable-odbc-trino/{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn source_can_be_overridden() {
        let p = parse("Host=localhost;Port=8080;User=admin;Source=powerbi");
        assert_eq!(p.source(), "powerbi");
    }

    #[test]
    fn client_tags_are_split_on_commas_and_trimmed() {
        // Trino selects a resource group from these, so " bi , adhoc " has to
        // reach the coordinator as two clean tags, not one padded string.
        let p = parse("Host=localhost;Port=8080;User=admin;ClientTags= bi , adhoc ");
        let mut tags: Vec<&str> = p.client_tags().iter().map(String::as_str).collect();
        tags.sort_unstable();
        assert_eq!(tags, vec!["adhoc", "bi"]);
    }

    #[test]
    fn client_tags_are_empty_when_unset() {
        let p = parse("Host=localhost;Port=8080;User=admin");
        assert!(p.client_tags().is_empty());
    }

    #[test]
    fn tls_verify_defaults_to_true() {
        let p = parse("Host=localhost;Port=8080;User=admin");
        assert!(p.tls_verify());
    }

    #[test]
    fn tls_verify_true_accepted() {
        let p = parse("Host=localhost;Port=8080;User=admin;TlsVerify=true");
        assert!(p.tls_verify());
    }

    #[test]
    fn tls_verify_false_accepted() {
        let p = parse("Host=localhost;Port=8080;User=admin;TlsVerify=false");
        assert!(!p.tls_verify());
    }

    #[test]
    fn tls_verify_case_insensitive() {
        let p = parse("Host=localhost;Port=8080;User=admin;TlsVerify=True");
        assert!(p.tls_verify());
        let p = parse("Host=localhost;Port=8080;User=admin;TlsVerify=FALSE");
        assert!(!p.tls_verify());
    }

    #[test]
    fn tls_verify_invalid_value_returns_error() {
        let err = parse_err("Host=localhost;Port=8080;User=admin;TlsVerify=yes");
        assert!(
            matches!(err, TrinoError::General { ref message } if message.contains("tlsverify")),
            "expected error mentioning tlsverify, got: {err:?}"
        );
    }

    #[test]
    fn protocol_defaults_to_https() {
        // The safe direction for an omitted value: an unencrypted connection
        // should be something an application asked for, not something it got
        // by saying nothing. A plaintext coordinator needs `Protocol=http`.
        let p = parse("Host=localhost;Port=8080;User=admin");
        assert!(p.secure());
    }

    #[test]
    fn protocol_http_opts_out_of_tls() {
        let p = parse("Host=localhost;Port=8080;User=admin;Protocol=http");
        assert!(!p.secure());
    }

    #[test]
    fn protocol_https_accepted() {
        let p = parse("Host=localhost;Port=8080;User=admin;Protocol=https");
        assert!(p.secure());
    }

    #[test]
    fn protocol_case_insensitive() {
        // A case variant must not silently downgrade the connection to
        // plaintext: the password is only sent when the transport is secure.
        for s in ["HTTPS", "Https", "hTTps"] {
            let p = parse(&format!("Host=localhost;Port=8080;User=admin;Protocol={s}"));
            assert!(p.secure(), "Protocol={s} was not treated as secure");
        }
        let p = parse("Host=localhost;Port=8080;User=admin;Protocol=HTTP");
        assert!(!p.secure());
    }

    #[test]
    fn protocol_invalid_value_returns_error() {
        let err = parse_err("Host=localhost;Port=8080;User=admin;Protocol=ftp");
        assert!(
            matches!(err, TrinoError::General { ref message } if message.contains("protocol")),
            "expected error mentioning protocol, got: {err:?}"
        );
    }

    #[test]
    fn debug_redacts_password() {
        let p = parse("Host=localhost;Port=8080;User=admin;Password=s3cr3t");
        let debug_str = format!("{p:?}");
        assert!(
            !debug_str.contains("s3cr3t"),
            "password must be redacted: {debug_str}"
        );
        assert!(
            debug_str.contains("*****"),
            "expected ***** in: {debug_str}"
        );
        assert!(
            debug_str.contains("localhost"),
            "host should be visible: {debug_str}"
        );
    }

    #[test]
    fn debug_no_password_shows_none() {
        let p = parse("Host=localhost;Port=8080;User=admin");
        let debug_str = format!("{p:?}");
        // Redacted<Option<String>> with None still prints as "*****": field is always hidden
        assert!(
            !debug_str.contains("localhost\")"),
            "redacted field should not leak: {debug_str}"
        );
    }

    #[test]
    fn access_token_parsed_from_accesstoken_key() {
        let p = parse("Host=h;Port=8080;User=u;Protocol=https;AccessToken=abc.def.ghi");
        assert_eq!(p.access_token(), Some("abc.def.ghi"));
    }

    #[test]
    fn access_token_parsed_from_token_alias() {
        let p = parse("Host=h;Port=8080;User=u;Protocol=https;Token=abc.def.ghi");
        assert_eq!(p.access_token(), Some("abc.def.ghi"));
    }

    #[test]
    fn access_token_absent_is_none() {
        let p = parse("Host=h;Port=8080;User=u");
        assert_eq!(p.access_token(), None);
    }

    #[test]
    fn debug_redacts_access_token() {
        let p = parse("Host=h;Port=8080;User=u;Protocol=https;AccessToken=s3cr3t.jwt");
        let s = format!("{p:?}");
        assert!(!s.contains("s3cr3t"), "token must be redacted: {s}");
    }
}
