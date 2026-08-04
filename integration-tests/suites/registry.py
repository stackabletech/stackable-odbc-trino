#!/usr/bin/env python3
"""The suite registry: one list of suites, read by both runners.

`scripts/run-tests.sh` and `windows/windows_test.py` each carried their own
idea of what the suites are. The Linux side grew to eleven entries while the
Windows side stayed at one, and nothing in either file said that was a
decision rather than an oversight. `SUITES` below is now the only list, and a
suite that does not run on Windows has to say why in its own entry.

What the registry holds is facts about a suite, not the command that runs it.
The two runners invoke Python differently and cannot share a command string:
Linux runs `uv run --with pyodbc python3 <path>` in a shell, while Windows runs
an absolute interpreter over WinRM with the driver's logging environment set
and a log-retrieval step afterwards. Each renders its own invocation from
`argv` and `pyodbc`.

The four-configuration connect matrix is deliberately *not* shared. Linux
crosses DSN names out of the generated `odbc.ini` with a DSN-less string;
Windows crosses a registry-registered DSN and connects by address for the
unverified cases, so that no SNI is sent. They are two different matrices that
happen to have the same four labels. `LINUX_CONFIGS` is here because this file
renders the Linux commands; the Windows configurations live in
`windows/windows_test.py`.

Run this file to see what the Linux runner will execute:

    python3 integration-tests/suites/registry.py --bash
"""

import os
import shlex
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Stack  # noqa: E402

TEST_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PROJECT_DIR = os.path.dirname(TEST_DIR)


class Suite:
    """One entry in the registry.

    `profile`  the compose profile the suite needs, or "" for the base stack.
    `argv`     how the suite takes its configuration:
                 "conn"        a connection string
                 "driver+conn" the driver's library path, then a connection string
                 "driver"      the driver's library path
                 "none"        nothing; it reads `stack.env` itself
    `pyodbc`   whether the suite imports pyodbc. The ctypes suites do not, and
               running them under `uv run --with pyodbc` would hide a missing
               dependency behind an installed one.
    `matrix`   run once per connect configuration rather than once.
    `windows`  True to run on the Windows VM, or the reason it does not. A
               string here is printed as a SKIP, because an unrun suite must
               never be indistinguishable from a passing one.
    `deploy`   repo-relative files the suite reads, beyond its own script and
               what every suite gets. Only the Windows runner uses this.
    """

    def __init__(self, name, script, *, profile="", argv="conn", pyodbc=True,
                 matrix=False, windows=True, deploy=()):
        self.name = name
        self.script = script
        self.profile = profile
        self.argv = argv
        self.pyodbc = pyodbc
        self.matrix = matrix
        self.windows = windows
        self.deploy = deploy

    @property
    def runs_on_windows(self):
        return self.windows is True

    @property
    def windows_skip_reason(self):
        return "" if self.windows is True else self.windows


SUITES = [
    Suite("integration", "test_integration.py", matrix=True),
    Suite(
        "harness unit tests", "test_harness.py", argv="none", pyodbc=False,
        windows="it tests harness.py, which is platform-independent Python "
                "and reaches neither the driver nor a Driver Manager",
    ),
    Suite("sql surface", "test_sql_surface.py"),
    # Parses the advertised capability bitmaps out of the driver's own source
    # and executes one `{fn ...}` per bit, so the rule info.rs states -- a bit
    # is set only when the escape becomes Trino SQL that runs -- is checked
    # against a coordinator rather than against a list of names.
    Suite(
        "escape sequences", "test_escapes.py",
        deploy=(
            "src/backend/info.rs",
            "src/backend.rs",
            "src/escape_dialect.rs",
        ),
    ),
    # argv="none": every check varies one connection-string key against an
    # otherwise identical connection, so it builds its own strings from
    # stack.env rather than taking one.
    Suite("session keys", "test_session_keys.py", argv="none"),
    Suite(
        "folding contract", "test_folding_contract.py",
        # It parses the connector's Constant visitor out of the .pq source, so
        # the connector travels with it.
        deploy=("connector/StackableTrinoODBC.pq",),
    ),
    Suite(
        "tls", "test_tls.py", argv="none",
        # keycloak.crt is a leaf signed by the same CA, used as a trust anchor
        # that signed nothing; client.pem is the mutual-TLS identity. ca.crt is
        # deployed for every suite.
        deploy=(
            "integration-tests/generated/certs/keycloak.crt",
            "integration-tests/generated/certs/client.pem",
        ),
    ),
    Suite("raw C ABI", "test_c_abi.py", argv="driver+conn", pyodbc=False),
    # ctypes, because pyodbc exposes no SQLDescribeParam at all: the call it
    # covers is reachable only through the C ABI.
    Suite("describe param", "test_describe_param.py", argv="driver+conn", pyodbc=False),
    Suite("type matrix", "test_type_matrix.py", argv="driver+conn", pyodbc=False),
    # No required profile: with `spooling` active it drives the spooled
    # protocol, and without it asserts the fallback a coordinator with no
    # spooling manager produces. Both are real assertions, so neither stack
    # state is a blind spot.
    Suite("spooling", "test_spooling.py", argv="none"),
    # No required profile either: the hive catalog it writes to is in the base
    # stack, because a file metastore costs no container.
    Suite("transactions", "test_transactions.py", argv="none"),
    # ctypes rather than pyodbc, and no connection string: pyodbc passes
    # SQL_DRIVER_NOPROMPT unconditionally, which core reads as forbidding the
    # prompt an interactive login needs, so this suite builds its own.
    Suite(
        "oauth", "test_oauth.py", profile="oauth", argv="driver", pyodbc=False,
        # The browser login itself does work through the Windows Driver
        # Manager, measured by hand against this stack. What is missing is
        # unattended: the suite's Driver Manager scenario loads libodbc.so.2 by
        # name, and Keycloak's frontchannel issuer is https://localhost:8444,
        # which inside the VM is the VM. Both are fixable; neither is done.
        windows="needs an odbc32.dll branch for the Driver Manager scenario, "
                "and a Keycloak issuer the VM resolves to the host",
    ),
]

# The Linux connect matrix. The DSN names are the ones scripts/gen-odbc-config.sh
# writes into the generated odbc.ini.
LINUX_CONFIGS = [
    ("DSN-less, verified TLS", lambda s: s.conn_str()),
    ("DSN-less, TlsVerify=false",
     lambda s: s.conn_str(TlsVerify="false", Certificate=None)),
    ("DSN, verified TLS", lambda s: s.dsn("trino_https")),
    ("DSN, TlsVerify=false", lambda s: s.dsn("trino_https_verify_false")),
]


def linux_entries(stack):
    """Yield (name, profile, command) for every suite, matrix expanded."""
    driver = stack.get("DRIVER_PATH")
    for suite in SUITES:
        if suite.matrix:
            for label, build in LINUX_CONFIGS:
                yield (
                    f"{suite.name} ({label})",
                    suite.profile,
                    _linux_command(suite, driver, build(stack)),
                )
        else:
            yield (
                suite.name,
                suite.profile,
                _linux_command(suite, driver, stack.conn_str()),
            )


def suite_argv(suite, driver, conn):
    """The suite's own arguments, for a runner that renders its interpreter."""
    if suite.argv == "conn":
        return [conn]
    if suite.argv == "driver+conn":
        return [driver, conn]
    if suite.argv == "driver":
        return [driver]
    if suite.argv == "none":
        return []
    raise ValueError(f"{suite.name}: unknown argv kind {suite.argv!r}")


def _linux_command(suite, driver, conn):
    interpreter = (
        ["uv", "run", "--with", "pyodbc", "python3"] if suite.pyodbc else ["python3"]
    )
    parts = interpreter + [os.path.join(TEST_DIR, "suites", suite.script)]
    parts += suite_argv(suite, driver, conn)
    return " ".join(shlex.quote(p) for p in parts)


def main():
    if len(sys.argv) != 2 or sys.argv[1] != "--bash":
        print("usage: registry.py --bash", file=sys.stderr)
        return 2
    stack = Stack.load(os.environ.get("STACK_ENV"))
    for name, profile, command in linux_entries(stack):
        print(f"{name}|{profile}|{command}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
