#!/usr/bin/env python3
"""Session connection-string keys, checked against what the coordinator saw.

`connect_params.rs` parses 35 keys and unit-tests the parsing of all of them.
What no suite checked was the other half: that a parsed value becomes the right
Trino header and has the effect README.md's table promises. Seventeen keys had
no live coverage at all, so a key could parse perfectly and reach the
coordinator as nothing.

That is not a hypothetical failure mode for this driver. `Certificate` under
`Protocol=http` was accepted and silently discarded, and the connector's `Roles`
sample double-wrapped a value the driver already wraps. Both are the same shape:
the value looks applied and is not.

Three strengths of assertion, in descending order, and each key is covered by
the strongest one Trino makes available:

1. **Observed.** The value comes back out of the session: `current_timezone()`,
   `current_path`, `SHOW SESSION`, `SHOW CURRENT ROLES`,
   `system.runtime.queries.source`, and a locale-dependent `format_datetime`.
2. **Refused.** A deliberately invalid value is rejected by the coordinator or
   the client. A rejection proves the value travelled, which is most of what an
   "observed" check proves, so it covers the keys Trino accepts silently.
3. **Accepted.** The connection succeeds and a query runs. Weak, and recorded
   as a NOTE rather than a PASS, because it cannot tell a working header from a
   discarded one. Only used where Trino offers nothing better.

Usage:
    uv run --with pyodbc python3 integration-tests/suites/test_session_keys.py "<connection-string>"

Requires a running Trino (integration-tests/setup.sh). Needs no compose profile.
The `hive` catalog it reads roles from is part of the base stack.
"""

import os
import sys

import pyodbc

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Results, Stack  # noqa: E402

R = Results("session connection-string keys")


def scalar(conn, sql):
    return conn.cursor().execute(sql).fetchone()[0]


def rows(conn, sql):
    return conn.cursor().execute(sql).fetchall()


def observed(stack, key, value, label, probe, expected):
    """Connect with one key set and assert what the session reports back."""
    try:
        conn = stack.connect(**{key: value})
    except pyodbc.Error as e:
        R.bad(f"{key}={value}", f"the connection was refused: {str(e)[:100]}")
        return
    try:
        got = scalar(conn, probe)
        ok = str(got) == str(expected)
        R.check(
            f"{key}={value} -> {label}",
            ok,
            "" if ok else f"  expected {expected!r}, got {got!r}",
        )
    except pyodbc.Error as e:
        R.bad(f"{key}={value} -> {label}", str(e)[:100])
    finally:
        conn.close()


def refused(stack, key, value, why, *, expect_sqlstate=None):
    """Connect with a deliberately invalid value and assert it is rejected.

    A key Trino accepts silently cannot be observed, but it can be *dis*proved:
    if a bad value is refused, the good value reached the coordinator too.
    """
    try:
        conn = stack.connect(**{key: value})
    except pyodbc.Error as e:
        state = e.args[0] if e.args else ""
        ok = expect_sqlstate is None or state == expect_sqlstate
        R.check(
            f"{key}={value} is refused ({why})",
            ok,
            "" if ok else f"  expected SQLSTATE {expect_sqlstate}, got {state}",
        )
        return
    conn.close()
    R.bad(
        f"{key}={value} is refused ({why})",
        "the connection succeeded, so the value did not reach Trino",
    )


def accepted(stack, key, value, why):
    """Connect with the key set and run a query. The weakest of the three."""
    try:
        conn = stack.connect(**{key: value})
        scalar(conn, "SELECT 1")
        conn.close()
    except pyodbc.Error as e:
        R.bad(f"{key}={value} is accepted", str(e)[:100])
        return
    R.note(
        f"{key}={value}",
        f"connects and queries, but {why}, so this does not prove the header "
        f"was sent",
    )


def main():
    # No connection-string argument, unlike most suites: every check here varies
    # one key against an otherwise identical connection, so it has to build the
    # strings rather than be handed one. `Stack` is what does that.
    stack = Stack.load()

    print("=== session connection-string keys ===\n")

    # ------------------------------------------------------------------
    print("--- observed: the value comes back out of the session ---")

    # Trino resolves current_timestamp, TIMESTAMP WITH TIME ZONE literals and
    # every AT TIME ZONE against the session zone. Unset, those follow the
    # coordinator's JVM, which is a property of the server rather than of the
    # query, so a TimeZone that did not apply is hours of silent error.
    observed(stack, "TimeZone", "Europe/Berlin", "current_timezone()",
             "SELECT current_timezone()", "Europe/Berlin")

    # The default SQL path is empty, so this cannot pass by accident.
    observed(stack, "Path", "system.builtin", "current_path",
             "SELECT current_path", "system.builtin")

    # Trino's query history shows this, and resource-group rules route on it.
    observed(stack, "Source", "odbc-session-key-suite",
             "system.runtime.queries.source",
             "SELECT source FROM system.runtime.queries "
             "WHERE source = 'odbc-session-key-suite' LIMIT 1",
             "odbc-session-key-suite")

    # `X-Trino-Language`. Observable because month names are locale-dependent:
    # the default renders 'February' and de-DE renders 'Februar'. Without this
    # the key was parsed, sent, and never once shown to do anything.
    observed(stack, "Locale", "de-DE", "a German month name",
             "SELECT format_datetime(DATE '2021-02-03', 'MMMM')", "Februar")

    # The braces are the connection-string escaping README.md devotes a section
    # to, so this exercises the whole path: core unwraps them, the parser splits
    # on ';' and ':', and the property reaches the session.
    try:
        conn = stack.connect(SessionProperties="{query_max_run_time:10m}")
        session = {r[0]: (r[1], r[2]) for r in rows(conn, "SHOW SESSION")}
        value, default = session.get("query_max_run_time", (None, None))
        R.check(
            "SessionProperties={query_max_run_time:10m} -> SHOW SESSION",
            value == "10m" and default != "10m",
            "" if value == "10m" else f"  value={value!r} default={default!r}",
        )
        conn.close()
    except pyodbc.Error as e:
        R.bad("SessionProperties reaches SHOW SESSION", str(e)[:100])

    # Roles are what Hive and Iceberg under sql-standard security check, and so
    # what decides whether SQLTablePrivileges returns a row. The name is written
    # bare here: connect_params.rs renders Trino's own ROLE{...} spelling.
    try:
        conn = stack.connect(Roles="{hive:admin}")
        current = {r[0] for r in rows(conn, "SHOW CURRENT ROLES FROM hive")}
        R.check(
            "Roles={hive:admin} -> SHOW CURRENT ROLES",
            "admin" in current,
            "" if "admin" in current else f"  got {sorted(current)}",
        )
        conn.close()
    except pyodbc.Error as e:
        R.bad("Roles reaches SHOW CURRENT ROLES", str(e)[:100])

    # ------------------------------------------------------------------
    print("\n--- refused: an invalid value is rejected, so a valid one travels ---")

    # Trino validates the property name, so a bogus one proves the map is sent
    # rather than dropped.
    refused(stack, "SessionProperties", "{not_a_real_property:1}",
            "INVALID_SESSION_PROPERTY")

    # The coordinator answers 400 for an unknown estimate name. ResourceEstimates
    # has no positive probe: a scheduling hint has no reading in the session.
    refused(stack, "ResourceEstimates", "{NOT_A_REAL_ESTIMATE:1h}",
            "the coordinator rejects the estimate name")

    # A role that does not exist is an authorisation failure, which is 28000
    # rather than a connect failure. That the SQLSTATE is specific is part of
    # the claim: the driver routes it through map_trino_error.
    refused(stack, "Roles", "{hive:no_such_role}",
            "the role does not exist", expect_sqlstate="28000")

    # Not Trino's rejection but the client's, and worth pinning: reqwest appends
    # rather than replaces, so a header the client already manages would be sent
    # twice. connect_params.rs documents this as the reason the value is refused
    # when the client is built.
    refused(stack, "ExtraHeaders", "{X-Trino-User:someone}",
            "the client manages that header")

    # ------------------------------------------------------------------
    print("\n--- accepted: the connection works, which is all Trino exposes ---")

    # A benign extra header, the case a gateway needs. Trino ignores unknown
    # headers, so there is nothing to read back; the refusal above is what
    # proves the map is delivered.
    accepted(stack, "ExtraHeaders", "{X-Odbc-Probe:yes}",
             "Trino ignores headers it does not know")
    accepted(stack, "ExtraCredentials", "{probe.token:abc123}",
             "the credential is forwarded to a connector, and neither test "
             "catalog reads one")
    accepted(stack, "ClientCapabilities", "ODBC_PROBE",
             "Trino ignores capabilities it does not know")
    accepted(stack, "ClientTags", "bi,adhoc",
             "tags select a resource group, and the test stack configures none")
    accepted(stack, "ClientInfo", "odbc-session-key-suite",
             "Trino records it against the query but publishes no column for it")
    accepted(stack, "TraceToken", "odbc-probe-1",
             "Trino records it against the query but publishes no column for it")
    accepted(stack, "DisableCompression", "true",
             "the effect is a request header the Driver Manager does not expose")
    accepted(stack, "MaxAttempts", "3",
             "a retry budget shows only under a fault this stack cannot inject")

    # ------------------------------------------------------------------
    print("\n--- not covered here ---")
    R.skip(
        "AccessToken",
        "needs a bearer token; test_oauth.py obtains one under the 'oauth' "
        "profile and is where that belongs",
    )
    R.skip(
        "Proxy, ProxyUser, ProxyPassword",
        "needs an HTTP proxy in the compose stack; the parser's own rules "
        "(userinfo refused, the two credentials required together, neither "
        "without Proxy) are unit-tested in connect_params.rs",
    )

    return R.summary()


if __name__ == "__main__":
    sys.exit(main())
