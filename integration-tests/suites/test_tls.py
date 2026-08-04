#!/usr/bin/env python3
"""TLS verification modes and mutual TLS, against the test CA.

Drives `TlsVerify`, `SSLVerification`, `Certificate` and `ClientCertificate`
against a real coordinator. Their parsing is unit-tested in
`src/backend/types/connect_params.rs`; this is where the TLS behaviour itself is
covered.

What each mode is checked for:

    full   chain verified, and the name checked
    ca     chain verified, the name not checked
    none   nothing verified

Chain verification is checked in all three by pointing `Certificate` at
`keycloak.crt`, a leaf signed by the same CA and therefore not a trust anchor:
`full` and `ca` must refuse it, `none` must not care.

**The name half of `ca` is not covered here, and cannot be.** Doing so needs the
coordinator to present its own certificate under a name that certificate does
not carry, and Jetty will not: it selects on SNI, and for any SNI it cannot
match it serves the self-signed `CN=<node.environment>` certificate Trino
generates for internal communication. Connecting by IP address is worse, since
TLS sends no SNI for an IP literal and the fallback certificate is served every
time. Measured against this stack:

    SNI=localhost           -> CN=localhost  (the CA-signed leaf)
    SNI=trino               -> CN=localhost  (DNS:trino is in the SAN)
    SNI=nosuchname.example  -> CN=test       (Trino's internal certificate)
    no SNI, by IP           -> CN=test

Neither way of removing the second certificate works, both measured against
the live stack, so do not retry them:

    * removing `internal-communication.https.required` stops the certificate
      being generated, and stops Trino starting:
      "NullPointerException: internalUri is null". With no plaintext listener
      that setting is what supplies the internal URI.
    * pointing `internal-communication.https.keystore.path` at the CA-signed
      keystore, with `node.internal-address=trino` so the name resolves,
      removes the second certificate and leaves every query stuck in RUNNING.

Closing this needs a TLS endpoint that serves one fixed certificate. The
Keycloak the `oauth` profile brings in is a candidate, but the driver only ever
speaks to Trino, so it would test rustls rather than the driver.

Requires a running Trino with the certificates `integration-tests/setup.sh`
generates, and `pip install pyodbc`. Needs no compose profile.

Usage:
    uv run --with pyodbc python3 integration-tests/suites/test_tls.py
"""

import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Results, Stack  # noqa: E402

R = Results("tls")

# A name the coordinator's certificate carries, so Jetty selects it on SNI. The
# stack states which one: `localhost` on the host, and the name mapped into the
# VM's hosts file when this runs on Windows. An address cannot stand in, because
# TLS sends no SNI for an IP literal.
NAMED = "localhost"


def connects(stack, **overrides):
    """Try a connection and a trivial query. Returns (ok, message, seconds)."""
    import pyodbc

    t0 = time.monotonic()
    try:
        conn = pyodbc.connect(stack.conn_str(**overrides), autocommit=True)
        try:
            cur = conn.cursor()
            cur.execute("SELECT 1")
            return cur.fetchone()[0] == 1, "", time.monotonic() - t0
        finally:
            conn.close()
    except Exception as e:
        return False, str(e).replace("\n", " ")[:150], time.monotonic() - t0


def expect_ok(stack, label, **overrides):
    ok, msg, secs = connects(stack, **overrides)
    R.check(f"{label}  ({secs:.1f}s)", ok, msg)


def expect_refused(stack, label, why, **overrides):
    ok, _msg, secs = connects(stack, **overrides)
    R.check(f"{label}  ({secs:.1f}s)", not ok, why if ok else "")


def main():
    global NAMED

    stack = Stack.load()
    NAMED = stack.get("TRINO_HOST", NAMED)
    ca = stack.get("CA_CERT")
    client_pem = stack.get("CLIENT_PEM")
    # A leaf signed by the same CA, so it is not itself a trust anchor and
    # cannot have signed the coordinator's certificate. Note this cannot be
    # client.pem: that file carries the CA in its chain, so it *is* a valid
    # anchor and a connection using it rightly succeeds.
    unrelated = os.path.join(os.path.dirname(ca), "keycloak.crt")

    print("\n--- each mode accepts the coordinator's own certificate ---")
    expect_ok(stack, "full verifies chain and name", Host=NAMED, TlsVerify="full")
    expect_ok(stack, "full is the default when TlsVerify is unset", Host=NAMED)
    expect_ok(stack, "ca verifies the chain", Host=NAMED, TlsVerify="ca")
    expect_ok(
        stack, "none verifies nothing", Host=NAMED, TlsVerify="none", Certificate=None
    )

    print("\n--- chain verification, against a certificate that signed nothing ---")
    expect_refused(
        stack,
        "full refuses a trust anchor that did not sign the chain",
        "connected with a leaf certificate as the trust anchor",
        Host=NAMED,
        TlsVerify="full",
        Certificate=unrelated,
    )
    expect_refused(
        stack,
        "ca refuses a trust anchor that did not sign the chain",
        "connected with a leaf certificate as the trust anchor; ca skips the "
        "name check, never the chain",
        Host=NAMED,
        TlsVerify="ca",
        Certificate=unrelated,
    )
    expect_ok(
        stack,
        "none accepts a trust anchor that signed nothing",
        Host=NAMED,
        TlsVerify="none",
        Certificate=unrelated,
    )

    R.note(
        "ca ignoring the name",
        "not covered: Jetty serves Trino's internal self-signed certificate for "
        "any SNI it cannot match, so the coordinator's own certificate cannot be "
        "reached under a name it does not carry. See this file's docstring.",
    )

    print("\n--- SSLVerification is an alias, and both keys take both vocabularies ---")
    expect_ok(
        stack,
        "SSLVerification=true means full",
        Host=NAMED,
        TlsVerify=None,
        SSLVerification="true",
    )
    expect_ok(
        stack,
        "SSLVerification=ca takes the TlsVerify vocabulary",
        Host=NAMED,
        TlsVerify=None,
        SSLVerification="ca",
    )
    expect_ok(
        stack,
        "TlsVerify=none takes the SSLVerification vocabulary",
        Host=NAMED,
        TlsVerify="none",
        Certificate=None,
    )
    expect_ok(
        stack,
        "both keys set to the same mode is accepted",
        Host=NAMED,
        TlsVerify="full",
        SSLVerification="full",
    )

    print("\n--- refused before a socket is opened ---")
    expect_refused(
        stack,
        "ca without Certificate is refused",
        "connected; rustls can only skip the name check when the trust store is "
        "given explicitly, so this cannot be honoured",
        Host=NAMED,
        TlsVerify="ca",
        Certificate=None,
    )
    expect_refused(
        stack,
        "TlsVerify and SSLVerification disagreeing is refused",
        "connected; one of the two was silently preferred, for a setting whose "
        "failure mode is an unauthenticated connection",
        Host=NAMED,
        TlsVerify="full",
        SSLVerification="none",
    )
    expect_refused(
        stack,
        "an unknown verification word is refused",
        "connected; a typo must not read as leaving verification on or off",
        Host=NAMED,
        TlsVerify="yes",
    )

    print("\n--- mutual TLS ---")
    if not os.path.exists(client_pem):
        R.skip(
            "mutual TLS",
            f"{client_pem} is missing; run ./integration-tests/setup.sh",
        )
    else:
        # The coordinator authenticates the certificate's CN as a Trino user,
        # so this must succeed with no Password at all.
        expect_ok(
            stack,
            "ClientCertificate authenticates with no Password",
            Host=NAMED,
            ClientCertificate=client_pem,
            Password=None,
        )
        expect_ok(
            stack,
            "ClientCertificate alongside a Password is accepted",
            Host=NAMED,
            ClientCertificate=client_pem,
        )
        expect_ok(
            stack,
            "ClientCertificate is independent of the verification mode",
            Host=NAMED,
            TlsVerify="ca",
            ClientCertificate=client_pem,
            Password=None,
        )
        expect_refused(
            stack,
            "a missing ClientCertificate file is refused",
            "connected without the certificate file existing",
            Host=NAMED,
            ClientCertificate="/nonexistent/client.pem",
            Password=None,
        )
        expect_refused(
            stack,
            "a ClientCertificate carrying no private key is refused",
            "connected with a file holding a certificate and no key",
            Host=NAMED,
            ClientCertificate=ca,
            Password=None,
        )

    return R.summary()


if __name__ == "__main__":
    sys.exit(main())
