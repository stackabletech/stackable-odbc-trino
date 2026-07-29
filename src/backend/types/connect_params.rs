//! `TrinoConnectParams`: the Trino connection settings (host, port, user,
//! password, protocol, catalog, schema, TLS and JWT options) parsed from the
//! generic `stackable-odbc-core` connection-string key/value map.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use stackable_odbc_core::types::{ConnectParams, Redacted};
use trino_rust_client::Tz;
use trino_rust_client::selected_role::{RoleType, SelectedRole};

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
/// Trino session properties, in JDBC's `name:value;name2:value2` form.
pub(crate) const PARAM_SESSION_PROPERTIES: &str = "sessionproperties";
/// Connector-level credentials passed through to the data source, in JDBC's
/// `name:value;name2:value2` form. Carries secrets.
pub(crate) const PARAM_EXTRA_CREDENTIALS: &str = "extracredentials";
/// Scheduling hints, in the same `name:value;name2:value2` form.
pub(crate) const PARAM_RESOURCE_ESTIMATES: &str = "resourceestimates";
/// Default SQL path for resolving unqualified function names.
pub(crate) const PARAM_PATH: &str = "path";
/// Free-form client metadata Trino records against the query.
pub(crate) const PARAM_CLIENT_INFO: &str = "clientinfo";
/// Correlation token Trino records against the query.
pub(crate) const PARAM_TRACE_TOKEN: &str = "tracetoken";
/// IANA time zone the session runs in, sent as `X-Trino-Time-Zone`.
///
/// Trino resolves `current_timestamp`, `TIMESTAMP WITH TIME ZONE` literals and
/// every `AT TIME ZONE` against the session zone, so leaving it unset means
/// those follow whatever zone the coordinator's JVM happens to be in — which is
/// a property of the server, not of the query.
pub(crate) const PARAM_TIME_ZONE: &str = "timezone";
/// Authorisation role per catalog, in the same `name:value;name2:value2` form
/// as the other key-value keys — `Roles={hive:admin;iceberg:ALL}`.
///
/// The value is a role name, or the keyword `ALL` or `NONE`, which is the shape
/// JDBC's `roles` property takes. Trino's own `X-Trino-Role` spelling wraps a
/// name in braces (`ROLE{admin}`), and that would collide with the braces the
/// connection string already needs around a `;`-separated value, so the name is
/// written bare here and [`selected_role`] renders the wire form.
///
/// Roles are what Hive and Iceberg under `sql-standard` security check, and
/// therefore what decides whether `SQLTablePrivileges` returns a row.
pub(crate) const PARAM_ROLES: &str = "roles";
/// User the statements run as, while authentication stays with [`PARAM_USER`].
///
/// JDBC spells it `sessionUser`. Trino sends it as `X-Trino-User`, so the
/// coordinator applies the impersonated user's permissions and records it
/// against the query, while the connection still authenticates as the
/// principal in `User`.
pub(crate) const PARAM_SESSION_USER: &str = "sessionuser";
/// Locale Trino formats locale-dependent values in, sent as
/// `X-Trino-Language`.
pub(crate) const PARAM_LOCALE: &str = "locale";
/// Disable HTTP response compression: `"true"` or `"false"` (default).
pub(crate) const PARAM_DISABLE_COMPRESSION: &str = "disablecompression";
/// How many times a request is attempted before it fails.
pub(crate) const PARAM_MAX_ATTEMPTS: &str = "maxattempts";

/// Separator between the pairs of a key-value connection-string parameter.
///
/// `;` is JDBC's, and it is also the ODBC connection-string separator — so a
/// value using it has to be `{}`-wrapped, which core's parser supports and
/// [`parse_key_value_pairs`] documents. Matching JDBC is worth that: the value
/// an operator already has in a JDBC URL transfers unchanged.
const PAIR_SEPARATOR: char = ';';

/// Separator between a key and its value, JDBC's again.
///
/// `:` rather than `=`, which means a value may contain `=` without escaping.
/// Only the *first* occurrence splits, so a value may contain `:` too — which
/// matters, because a session property value is routinely a URL or a duration.
const KEY_VALUE_SEPARATOR: char = ':';

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

/// Parse a `name:value;name2:value2` parameter into a map.
///
/// The form is the Trino JDBC driver's for `sessionProperties` and
/// `extraCredentials`, so a value copied from a JDBC URL works unchanged. It
/// does need `{}` around it in an ODBC connection string, because `;` is what
/// separates one connection-string parameter from the next:
///
/// ```text
/// SessionProperties={query_max_run_time:10m;example.foo:bar}
/// ```
///
/// Without the braces core's parser ends the value at the first `;` and treats
/// the rest as another parameter, which it then discards as unrecognised —
/// silently dropping every property but the first.
///
/// A malformed pair is an error rather than a skip. A dropped session property
/// changes how the query runs, and an operator who mistyped one would otherwise
/// see a plausible result computed under settings they did not ask for.
fn parse_key_value_pairs(key: &str, raw: &str) -> Result<HashMap<String, String>, TrinoError> {
    let mut map = HashMap::new();

    for pair in raw.split(PAIR_SEPARATOR) {
        // Trailing or doubled separators are the shape a hand-edited string
        // takes; they carry no pair and no ambiguity, so they are skipped
        // rather than rejected.
        if pair.trim().is_empty() {
            continue;
        }
        let (name, value) =
            pair.split_once(KEY_VALUE_SEPARATOR)
                .ok_or_else(|| TrinoError::General {
                    message: format!(
                        "invalid value for {key}: {pair:?} is not \
                     \"name{KEY_VALUE_SEPARATOR}value\". Pairs are separated by \
                     {PAIR_SEPARATOR:?}, so the whole value needs {{braces}} in a \
                     connection string"
                    ),
                })?;

        let name = name.trim();
        if name.is_empty() {
            return Err(TrinoError::General {
                message: format!("invalid value for {key}: {pair:?} has an empty name"),
            });
        }
        map.insert(name.to_string(), value.trim().to_string());
    }

    Ok(map)
}

/// A connection-string role value as `X-Trino-Role` spells it.
///
/// `ALL` and `NONE` are Trino's two keywords and are matched case-insensitively,
/// the way every other connection-string value is. Anything else is a role
/// name, which the wire format wraps as `ROLE{name}` — done here rather than
/// asked of the operator, because the braces would have to be escaped past
/// core's connection-string parser to survive.
fn selected_role(value: &str) -> SelectedRole {
    match value.to_ascii_uppercase().as_str() {
        "ALL" => SelectedRole::new(RoleType::All, None),
        "NONE" => SelectedRole::new(RoleType::None, None),
        _ => SelectedRole::new(RoleType::Role, Some(value.to_string())),
    }
}

/// Parse a `"true"` / `"false"` parameter, case-insensitively.
///
/// Rejects anything else rather than defaulting: every boolean here turns a
/// protection or a behaviour off, and a typo silently reading as "leave it on"
/// is the failure mode `TlsVerify` already guards against.
fn parse_bool(key: &str, raw: &str) -> Result<bool, TrinoError> {
    match raw {
        v if v.eq_ignore_ascii_case("true") => Ok(true),
        v if v.eq_ignore_ascii_case("false") => Ok(false),
        v => Err(TrinoError::General {
            message: format!("invalid value for {key}: {v:?}, expected \"true\" or \"false\""),
        }),
    }
}

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
    session_properties: HashMap<String, String>,
    /// Redacted for the same reason as `password`: these are credentials the
    /// connection forwards to a connector, and `Debug` on this struct reaches
    /// the log.
    extra_credentials: Redacted<HashMap<String, String>>,
    resource_estimates: HashMap<String, String>,
    path: Option<String>,
    client_info: Option<String>,
    trace_token: Option<String>,
    session_user: Option<String>,
    locale: Option<String>,
    roles: HashMap<String, SelectedRole>,
    time_zone: Option<Tz>,
    compression_disabled: bool,
    max_attempts: Option<usize>,
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

    pub fn session_properties(&self) -> &HashMap<String, String> {
        &self.session_properties
    }

    pub fn extra_credentials(&self) -> &HashMap<String, String> {
        &self.extra_credentials.0
    }

    pub fn resource_estimates(&self) -> &HashMap<String, String> {
        &self.resource_estimates
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    pub fn client_info(&self) -> Option<&str> {
        self.client_info.as_deref()
    }

    pub fn trace_token(&self) -> Option<&str> {
        self.trace_token.as_deref()
    }

    /// The user statements run as, when it differs from the authenticating one.
    pub fn session_user(&self) -> Option<&str> {
        self.session_user.as_deref()
    }

    pub fn locale(&self) -> Option<&str> {
        self.locale.as_deref()
    }

    /// Authorisation role per catalog. Empty leaves the coordinator's default,
    /// which for `sql-standard` security is no role at all.
    pub fn roles(&self) -> &HashMap<String, SelectedRole> {
        &self.roles
    }

    /// `None` leaves the coordinator's own zone in force.
    pub fn time_zone(&self) -> Option<Tz> {
        self.time_zone
    }

    pub fn compression_disabled(&self) -> bool {
        self.compression_disabled
    }

    /// `None` leaves `trino-rust-client`'s own retry budget in place, which is
    /// the honest default: the driver has no better number than the client's.
    pub fn max_attempts(&self) -> Option<usize> {
        self.max_attempts
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

        let pairs = |key: &'static str| match params.get(key) {
            None => Ok(HashMap::new()),
            Some(raw) => parse_key_value_pairs(key, raw),
        };
        let session_properties = pairs(PARAM_SESSION_PROPERTIES)?;
        let extra_credentials = pairs(PARAM_EXTRA_CREDENTIALS)?;
        let resource_estimates = pairs(PARAM_RESOURCE_ESTIMATES)?;
        let roles = pairs(PARAM_ROLES)?
            .into_iter()
            .map(|(catalog, role)| (catalog, selected_role(&role)))
            .collect();

        // Rejected rather than ignored. A zone the operator mistyped would
        // otherwise leave every `current_timestamp` and `AT TIME ZONE` on the
        // coordinator's own zone, which is a plausible-looking answer that is
        // silently hours out.
        let time_zone = match params.get(PARAM_TIME_ZONE) {
            None => None,
            Some(raw) => Some(raw.parse::<Tz>().map_err(|_| TrinoError::General {
                message: format!(
                    "invalid value for {PARAM_TIME_ZONE}: {raw:?} is not an IANA \
                     time zone name, such as \"Europe/Berlin\" or \"UTC\""
                ),
            })?),
        };

        let compression_disabled = match params.get(PARAM_DISABLE_COMPRESSION) {
            None => false,
            Some(v) => parse_bool(PARAM_DISABLE_COMPRESSION, v)?,
        };

        // Rejected rather than defaulted, unlike `QueryTimeout` above. That one
        // predates this and warns for compatibility; a new key is better off
        // telling the operator the value never took effect, since a retry
        // budget silently reverting to the client's default is invisible until
        // a flaky network makes it matter.
        let max_attempts = match params.get(PARAM_MAX_ATTEMPTS) {
            None => None,
            Some(v) => match v.parse::<usize>() {
                // Zero attempts would mean "never send the request", which is
                // not a budget an application can have meant.
                Ok(0) | Err(_) => {
                    return Err(TrinoError::General {
                        message: format!(
                            "invalid value for {PARAM_MAX_ATTEMPTS}: {v:?}, \
                             expected a positive integer"
                        ),
                    });
                }
                Ok(n) => Some(n),
            },
        };
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
            session_properties,
            extra_credentials: Redacted(extra_credentials),
            resource_estimates,
            path: params.get(PARAM_PATH).map(str::to_string),
            client_info: params.get(PARAM_CLIENT_INFO).map(str::to_string),
            trace_token: params.get(PARAM_TRACE_TOKEN).map(str::to_string),
            session_user: params.get(PARAM_SESSION_USER).map(str::to_string),
            locale: params.get(PARAM_LOCALE).map(str::to_string),
            roles,
            time_zone,
            compression_disabled,
            max_attempts,
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

    /// The `{}` wrapping is not optional and not cosmetic: `;` separates one
    /// connection-string parameter from the next, so an unbraced multi-pair
    /// value is truncated at the first `;` by core's parser before this code
    /// ever sees it. Both halves are asserted here so the requirement is
    /// recorded as a behaviour rather than only in prose.
    #[test]
    fn session_properties_take_jdbcs_form_inside_braces() {
        let p = parse(
            "Host=localhost;Port=8080;User=admin;\
             SessionProperties={query_max_run_time:10m;example.foo:bar}",
        );
        assert_eq!(
            p.session_properties()
                .get("query_max_run_time")
                .map(String::as_str),
            Some("10m")
        );
        assert_eq!(
            p.session_properties()
                .get("example.foo")
                .map(String::as_str),
            Some("bar")
        );
    }

    #[test]
    fn session_properties_unbraced_keep_only_the_first_pair() {
        let p = parse(
            "Host=localhost;Port=8080;User=admin;\
             SessionProperties=query_max_run_time:10m;example.foo:bar",
        );
        assert_eq!(
            p.session_properties().len(),
            1,
            "core's parser ends the value at the first ';', so the rest is a \
             separate (unrecognised) parameter -- this is why braces are required"
        );
    }

    /// Only the *first* separator splits, so a value may contain `:` — which
    /// is not a corner case: `http://…` and `10:00` are ordinary property
    /// values.
    #[test]
    fn a_property_value_may_contain_the_separator() {
        let p = parse(
            "Host=localhost;Port=8080;User=admin;\
             SessionProperties={exchange.base-directories:s3://bucket/path}",
        );
        assert_eq!(
            p.session_properties()
                .get("exchange.base-directories")
                .map(String::as_str),
            Some("s3://bucket/path")
        );
    }

    /// A dropped property changes how the query runs, so a typo has to fail
    /// the connection rather than produce a plausible answer computed under
    /// settings nobody asked for.
    #[test]
    fn a_malformed_pair_is_rejected() {
        let err =
            parse_err("Host=localhost;Port=8080;User=admin;SessionProperties={query_max_run_time}");
        let message = err.to_string();
        assert!(
            message.contains("sessionproperties") && message.contains("query_max_run_time"),
            "the error must name the key and the offending pair: {message}"
        );
    }

    #[test]
    fn an_empty_property_name_is_rejected() {
        let err = parse_err("Host=localhost;Port=8080;User=admin;SessionProperties={:orphan}");
        assert!(err.to_string().contains("empty name"), "{err}");
    }

    #[test]
    fn key_value_parameters_are_empty_when_unset() {
        let p = parse("Host=localhost;Port=8080;User=admin");
        assert!(p.session_properties().is_empty());
        assert!(p.extra_credentials().is_empty());
        assert!(p.resource_estimates().is_empty());
    }

    #[test]
    fn extra_credentials_and_resource_estimates_use_the_same_form() {
        let p = parse(
            "Host=localhost;Port=8080;User=admin;\
             ExtraCredentials={s3.token:abc123;kerberos:xyz};\
             ResourceEstimates={EXECUTION_TIME:1h}",
        );
        assert_eq!(p.extra_credentials().len(), 2);
        assert_eq!(
            p.extra_credentials().get("s3.token").map(String::as_str),
            Some("abc123")
        );
        assert_eq!(
            p.resource_estimates()
                .get("EXECUTION_TIME")
                .map(String::as_str),
            Some("1h")
        );
    }

    /// `Debug` on this struct reaches the log, and these are credentials being
    /// forwarded to a connector.
    #[test]
    fn extra_credentials_are_redacted_in_debug() {
        let p = parse("Host=localhost;Port=8080;User=admin;ExtraCredentials={s3.token:hunter2}");
        let rendered = format!("{p:?}");
        assert!(
            !rendered.contains("hunter2"),
            "the credential leaked into Debug output: {rendered}"
        );
    }

    #[test]
    fn the_plain_string_keys_round_trip() {
        let p = parse(
            "Host=localhost;Port=8080;User=admin;\
             Path=system.builtin;ClientInfo=dashboard-7;TraceToken=abc-123",
        );
        assert_eq!(p.path(), Some("system.builtin"));
        assert_eq!(p.client_info(), Some("dashboard-7"));
        assert_eq!(p.trace_token(), Some("abc-123"));
    }

    /// Impersonation is a second user alongside the authenticating one, not a
    /// replacement for it: `User` still carries the credentials.
    #[test]
    fn session_user_is_separate_from_the_authenticating_user() {
        let p = parse("Host=localhost;Port=8080;User=svc_bi;SessionUser=alice");
        assert_eq!(p.user(), "svc_bi");
        assert_eq!(p.session_user(), Some("alice"));
    }

    #[test]
    fn session_user_and_locale_are_unset_by_default() {
        let p = parse("Host=localhost;Port=8080;User=admin");
        assert_eq!(p.session_user(), None);
        assert_eq!(p.locale(), None);
    }

    #[test]
    fn locale_round_trips() {
        let p = parse("Host=localhost;Port=8080;User=admin;Locale=de-DE");
        assert_eq!(p.locale(), Some("de-DE"));
    }

    /// A bare name is a role name, and the two keywords are keywords. The
    /// rendered form is what goes on the wire as `X-Trino-Role`, which is why
    /// it is asserted rather than the enum: the braces are the part an operator
    /// must not have to write.
    #[test]
    fn roles_render_the_wire_form_from_bare_names_and_keywords() {
        let p = parse("Host=localhost;Port=8080;User=admin;Roles={hive:admin;iceberg:ALL;pg:none}");
        assert_eq!(p.roles()["hive"].to_string(), "ROLE{admin}");
        assert_eq!(p.roles()["iceberg"].to_string(), "ALL");
        // Matched case-insensitively, like every other connection-string value.
        assert_eq!(p.roles()["pg"].to_string(), "NONE");
    }

    #[test]
    fn time_zone_takes_an_iana_name() {
        let p = parse("Host=localhost;Port=8080;User=admin;TimeZone=Europe/Berlin");
        assert_eq!(
            p.time_zone().map(|tz| tz.to_string()),
            Some("Europe/Berlin".to_string())
        );
        assert_eq!(
            parse("Host=localhost;Port=8080;User=admin").time_zone(),
            None
        );
    }

    /// A mistyped zone leaves every `current_timestamp` on the coordinator's
    /// own, which is a wrong answer that looks right.
    #[test]
    fn an_unknown_time_zone_is_rejected() {
        let err = parse_err("Host=localhost;Port=8080;User=admin;TimeZone=Europe/Berlim");
        assert!(
            err.to_string().contains("IANA"),
            "the message must say what a valid value looks like: {err}"
        );
    }

    #[test]
    fn roles_are_empty_by_default() {
        assert!(
            parse("Host=localhost;Port=8080;User=admin")
                .roles()
                .is_empty()
        );
    }

    /// The same malformed-pair rule as the other key-value keys: a dropped role
    /// is a silently different permission set.
    #[test]
    fn a_role_without_a_catalog_is_rejected() {
        let err = parse_err("Host=localhost;Port=8080;User=admin;Roles={hive:admin;bogus}");
        assert!(
            err.to_string().contains("roles"),
            "the message must name the key: {err}"
        );
    }

    #[test]
    fn compression_is_enabled_unless_disabled() {
        assert!(!parse("Host=localhost;Port=8080;User=admin").compression_disabled());
        assert!(
            parse("Host=localhost;Port=8080;User=admin;DisableCompression=TRUE")
                .compression_disabled()
        );
        assert!(
            parse_err("Host=localhost;Port=8080;User=admin;DisableCompression=yes")
                .to_string()
                .contains("disablecompression")
        );
    }

    /// `None` leaves the client's own budget alone, which is different from
    /// any number this driver could pick.
    #[test]
    fn max_attempts_is_unset_by_default_and_must_be_positive() {
        assert_eq!(
            parse("Host=localhost;Port=8080;User=admin").max_attempts(),
            None
        );
        assert_eq!(
            parse("Host=localhost;Port=8080;User=admin;MaxAttempts=5").max_attempts(),
            Some(5)
        );
        for bad in ["0", "-1", "many"] {
            let err = parse_err(&format!(
                "Host=localhost;Port=8080;User=admin;MaxAttempts={bad}"
            ));
            assert!(
                err.to_string().contains("maxattempts"),
                "MaxAttempts={bad} must be rejected by name: {err}"
            );
        }
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
