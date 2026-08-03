#!/usr/bin/env python3
"""Trino's interactive OAuth 2.0 external authentication, end to end.

Requires the `oauth` profile: `./integration-tests/setup.sh --profile oauth`.

**This suite cannot use pyodbc**, which passes `SQL_DRIVER_NOPROMPT`
unconditionally. It loads the driver with ctypes and passes
`SQL_DRIVER_COMPLETE` itself. See "The OAuth 2.0 flow" in `AGENTS.md`.

The browser is `oauth_browser.py`, installed as a PATH-shadowed `xdg-open`
because `open` 5.4.0 runs `xdg-open` first and unconditionally and ignores
`$BROWSER`. See that file for why it always exits 0.

`OAUTH2_LOGINS` in `src/backend.rs` caches one login per
`(secure, host, port, user)` for the life of the process. A scenario expecting a
*failure* takes an entry of its own by naming a distinct `User`. One expecting a
successful connect cannot: Trino refuses a `User` disagreeing with the token as
an impersonation attempt, so those use the identity provider's own user or none
at all and are served from the cache. Reusing a key is also how the cache itself
is asserted.

Usage:
    python3 integration-tests/suites/test_oauth.py [path/to/libstackable_odbc_trino.so]
"""

import ctypes
import json
import os
import shlex
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Results, Stack  # noqa: E402
from odbc_abi import (  # noqa: E402
    SQL_ATTR_ODBC_VERSION,
    SQL_DRIVER_COMPLETE,
    SQL_DRIVER_NOPROMPT,
    SQL_ERROR,
    SQL_HANDLE_DBC,
    SQL_HANDLE_ENV,
    SQL_HANDLE_STMT,
    SQL_NTS,
    SQL_NULL_HANDLE,
    SQL_OV_ODBC3,
    SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO,
    diag_message,
    load,
    sqlstate,
    w,
)

# SQLGetData C type, for reading `current_user` back as text.
SQL_C_CHAR = 1

# SQLSTATE 28000, "invalid authorization specification", which is what every
# failed authentication in this suite must report.
INVALID_AUTH_SPEC = "28000"

R = Results("oauth")

SUITES_DIR = os.path.dirname(os.path.abspath(__file__))

# Long enough for a real login, short enough that a broken browser fails the
# suite in seconds rather than waiting out the driver's 300 second default.
LOGIN_BUDGET_SECONDS = 30
# The abandoned-login scenario. Small, because the assertion is that the budget
# is what ends the wait.
ABANDON_BUDGET_SECONDS = 5

# A name the coordinator's certificate carries, so Jetty selects it on SNI. See
# the note in test_tls.py.
NAMED = "localhost"

IMPOSTOR = "impostor"


class Shim:
    """The fake browser, installed as a PATH-shadowed `xdg-open`.

    Shadowing PATH rather than setting `$BROWSER` is required: `open` 5.4.0
    ignores `$BROWSER` and runs `xdg-open` first, and on a machine with a
    desktop session `xdg-open` dispatches to `gio`, which opens a real browser.

    The driver runs in this process, so `Command::new("xdg-open")` resolves
    against this process's own PATH and inherits this process's environment,
    which is how `reset` selects a mode per scenario.
    """

    def __init__(self, generated_dir):
        self.dir = os.path.join(generated_dir, "oauth-browser")
        self.record = os.path.join(self.dir, "record.jsonl")
        os.makedirs(self.dir, exist_ok=True)

        shim = os.path.join(self.dir, "xdg-open")
        browser = shlex.quote(os.path.join(SUITES_DIR, "oauth_browser.py"))
        with open(shim, "w", encoding="utf-8") as f:
            f.write(
                "#!/usr/bin/env bash\n"
                "# Written by test_oauth.py. `open::that` runs xdg-open first and\n"
                "# unconditionally, so shadowing it on PATH is what makes the test\n"
                "# browser deterministic.\n"
                f'exec python3 {browser} "$@"\n'
            )
        os.chmod(shim, 0o755)

        os.environ["PATH"] = self.dir + os.pathsep + os.environ["PATH"]
        os.environ["ODBC_TEST_OAUTH_RECORD"] = self.record

    def reset(self, mode):
        """Clear the record and select a mode, so launches are per scenario."""
        if os.path.exists(self.record):
            os.remove(self.record)
        os.environ["ODBC_TEST_OAUTH_MODE"] = mode

    def launches(self):
        if not os.path.exists(self.record):
            return []
        with open(self.record, encoding="utf-8") as f:
            return [json.loads(line) for line in f if line.strip()]

    def outcomes(self):
        return [e.get("outcome") for e in self.launches()]

    def failures(self):
        """Launches that reported an error, so a broken browser is named rather
        than surfacing as an unexplained timeout."""
        return [e for e in self.launches() if e.get("outcome") == "error"]


def odbc_connect(lib, conn_str, completion=SQL_DRIVER_COMPLETE):
    """Allocate an environment and connection, then SQLDriverConnectW.

    Returns (ret, env, dbc). The handles come back even on failure, because the
    diagnostic lives on the connection handle.
    """
    env = ctypes.c_void_p()
    lib.SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, ctypes.byref(env))
    lib.SQLSetEnvAttr(env, SQL_ATTR_ODBC_VERSION, ctypes.c_void_p(SQL_OV_ODBC3), 0)
    dbc = ctypes.c_void_p()
    lib.SQLAllocHandle(SQL_HANDLE_DBC, env, ctypes.byref(dbc))

    conn_w, _keep = w(conn_str)
    ret = lib.SQLDriverConnectW(dbc, None, conn_w, SQL_NTS, None, 0, None, completion)
    return ret, env, dbc


def disconnect(lib, env, dbc):
    lib.SQLDisconnect(dbc)
    lib.SQLFreeHandle(SQL_HANDLE_DBC, dbc)
    lib.SQLFreeHandle(SQL_HANDLE_ENV, env)


def diagnostic(lib, dbc):
    """The connection's SQLSTATE and message, for a failure detail."""
    return f"{sqlstate(lib, SQL_HANDLE_DBC, dbc)}: {diag_message(lib, SQL_HANDLE_DBC, dbc)}"


def check_connected(lib, dbc, ret, label):
    """Assert a connect succeeded, naming the diagnostic only when it did not.

    `Results.check` prints whatever detail it is given either way, so passing
    the diagnostic unconditionally decorates every passing line with an empty
    `: :`.
    """
    ok = ret in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO)
    return R.check(label, ok, "" if ok else diagnostic(lib, dbc))


def scalar(lib, dbc, sql):
    """The first column of the first row, as a str, or None."""
    stmt = ctypes.c_void_p()
    lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(stmt))
    try:
        sql_w, _keep = w(sql)
        if lib.SQLExecDirectW(stmt, sql_w, SQL_NTS) not in (
            SQL_SUCCESS,
            SQL_SUCCESS_WITH_INFO,
        ):
            return None
        if lib.SQLFetch(stmt) not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
            return None
        buf = ctypes.create_string_buffer(256)
        ind = ctypes.c_int64(0)
        if lib.SQLGetData(
            stmt,
            1,
            SQL_C_CHAR,
            ctypes.cast(buf, ctypes.c_void_p),
            256,
            ctypes.byref(ind),
        ) not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
            return None
        return buf.value.decode("utf-8", "replace")
    finally:
        lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)


def oauth_conn(stack, **overrides):
    """An `ExternalAuthentication` connection string.

    `User` and `Password` are removed: under an interactive login the identity
    provider decides the user, and the driver leaves `X-Trino-User` off
    entirely. A scenario needing its own login in `OAUTH2_LOGINS` passes a
    `User`, which is part of that cache's key.
    """
    params = {
        "Host": NAMED,
        "User": None,
        "Password": None,
        "ExternalAuthentication": "true",
        "ExternalAuthenticationTimeout": str(LOGIN_BUDGET_SECONDS),
    }
    params.update(overrides)
    return stack.conn_str(**params)


def scenario_one_login_serves_many_connections(lib, stack, shim):
    """The end-to-end flow, and the login cache.

    Three connections on one key must produce exactly one browser launch. The
    client caches the token in the `Arc<OAuth2State>` behind an `Auth`, and this
    driver builds a `Client` per connection, so without `OAUTH2_LOGINS` a pool
    warming three connections would open three browsers.

    The open connections are handed back so the next scenario can reuse the key
    without a login of its own.
    """
    shim.reset("login")
    opened = []
    for i in range(3):
        t0 = time.monotonic()
        ret, env, dbc = odbc_connect(lib, oauth_conn(stack))
        opened.append((env, dbc))
        ok = check_connected(
            lib,
            dbc,
            ret,
            f"connection {i + 1} of 3 authenticates interactively  "
            f"({time.monotonic() - t0:.1f}s)",
        )
        if not ok:
            for failure in shim.failures():
                R.bad("the browser reported", failure.get("error", ""))
            for env, dbc in opened:
                disconnect(lib, env, dbc)
            return []

    R.check(
        "exactly one browser launch serves three connections",
        len(shim.launches()) == 1,
        f"{len(shim.launches())} launches recorded: {shim.outcomes()}",
    )
    return opened


def scenario_the_token_supplies_the_user(lib, stack, shim, opened):
    """`X-Trino-User` is left off entirely and Trino resolves the user from the
    token. Reusing the previous scenario's key also asserts the login cache: no
    second browser may be launched."""
    if not opened:
        R.skip(
            "the token supplies the session user",
            "the interactive connection did not succeed",
        )
        return
    _env, dbc = opened[0]
    user = scalar(lib, dbc, "SELECT current_user")
    R.check(
        "the session user comes from the token, with no X-Trino-User sent",
        user == stack.get("TRINO_USER"),
        f"current_user is {user!r}, expected {stack.get('TRINO_USER')!r}",
    )
    R.check(
        "reusing the login opened no second browser",
        len(shim.launches()) == 1,
        f"{len(shim.launches())} launches recorded",
    )


def scenario_a_matching_user_is_honoured(lib, stack, shim):
    """A `User` equal to what the identity provider's mapping produces is
    harmless, so naming it must not fail the connection.

    Its own entry in `OAUTH2_LOGINS`, since the key carries the user and the
    first scenario connected with none.
    """
    shim.reset("login")
    ret, env, dbc = odbc_connect(lib, oauth_conn(stack, User=stack.get("TRINO_USER")))
    try:
        if not check_connected(
            lib, dbc, ret, "a User matching the token's identity is accepted"
        ):
            return
        user = scalar(lib, dbc, "SELECT current_user")
        R.check(
            "the session runs as that same user",
            user == stack.get("TRINO_USER"),
            f"current_user is {user!r}",
        )
    finally:
        disconnect(lib, env, dbc)


def scenario_a_disagreeing_user_is_refused(lib, stack, shim):
    """A `User` that disagrees with the token's identity is an impersonation
    request, and Trino refuses it.

    This is the behaviour that made `User` optional under
    `ExternalAuthentication`: Trino takes the session user from the
    authenticated identity when the header is absent, and puts a header that
    disagrees through `checkCanImpersonateUser`. Trino's default system access
    control denies that, so an operator obliged to invent a `User` gets the
    connection refused for their own account.
    """
    shim.reset("login")
    ret, env, dbc = odbc_connect(lib, oauth_conn(stack, User=IMPOSTOR))
    try:
        state = sqlstate(lib, SQL_HANDLE_DBC, dbc)
        message = diag_message(lib, SQL_HANDLE_DBC, dbc)
        refused = ret == SQL_ERROR
        R.check(
            "a User disagreeing with the token is refused",
            refused,
            # Shown only on failure: connecting means the session is running as
            # somebody the application never authenticated as.
            "" if refused else f"returned {ret}, so the impersonation was granted",
        )
        R.check(
            "the refusal reports 28000",
            state == INVALID_AUTH_SPEC,
            f"state is {state!r}: {message}",
        )
        R.check(
            "the diagnostic names the impersonation",
            "impersonate" in message.lower(),
            f"message is {message!r}",
        )
    finally:
        disconnect(lib, env, dbc)


def scenario_an_abandoned_login_times_out(lib, stack, shim):
    """A login nobody completes ends at `ExternalAuthenticationTimeout`, with
    `28000`.

    The elapsed time is asserted as well as the SQLSTATE: a failure arriving
    after some other budget expired would be the timeout not working, reported
    as though it were.
    """
    shim.reset("noop")
    t0 = time.monotonic()
    ret, env, dbc = odbc_connect(
        lib,
        oauth_conn(
            stack,
            User="abandoned",
            ExternalAuthenticationTimeout=str(ABANDON_BUDGET_SECONDS),
        ),
    )
    elapsed = time.monotonic() - t0
    try:
        state = sqlstate(lib, SQL_HANDLE_DBC, dbc)
        failed = ret == SQL_ERROR
        R.check(
            f"an abandoned login fails the connection  ({elapsed:.1f}s)",
            failed,
            "" if failed else f"returned {ret} with state {state!r}",
        )
        R.check(
            "an abandoned login reports 28000",
            state == INVALID_AUTH_SPEC,
            f"state is {state!r}: {diag_message(lib, SQL_HANDLE_DBC, dbc)}",
        )
        R.check(
            "the login budget is what ended the wait",
            ABANDON_BUDGET_SECONDS - 1 <= elapsed <= ABANDON_BUDGET_SECONDS + 15,
            f"{elapsed:.1f}s against a {ABANDON_BUDGET_SECONDS}s budget",
        )
        presented = shim.outcomes() == ["presented"]
        R.check(
            "the browser was launched and presented the URL",
            presented,
            "" if presented else f"records: {shim.launches()}",
        )
    finally:
        disconnect(lib, env, dbc)


def scenario_a_refused_login_reports_28000(lib, stack, shim):
    """A login the identity provider refuses fails at once, not at the budget.

    Elapsed time is what separates the two failures: a refusal the coordinator
    ignored would still end the connection, just at
    `ExternalAuthenticationTimeout`, and the SQLSTATE alone cannot tell them
    apart.
    """
    shim.reset("deny")
    t0 = time.monotonic()
    ret, env, dbc = odbc_connect(lib, oauth_conn(stack, User="refused"))
    elapsed = time.monotonic() - t0
    try:
        denied = shim.outcomes() == ["denied"]
        if not R.check(
            "the browser delivered a refusal",
            denied,
            "" if denied else f"records: {shim.launches()}",
        ):
            return
        state = sqlstate(lib, SQL_HANDLE_DBC, dbc)
        failed = ret == SQL_ERROR
        R.check(
            f"a refused login fails the connection  ({elapsed:.1f}s)",
            failed,
            "" if failed else f"returned {ret} with state {state!r}",
        )
        R.check(
            "a refused login reports 28000",
            state == INVALID_AUTH_SPEC,
            f"state is {state!r}: {diag_message(lib, SQL_HANDLE_DBC, dbc)}",
        )
        prompt = elapsed < LOGIN_BUDGET_SECONDS - 5
        R.check(
            "the refusal ended the login rather than the budget",
            prompt,
            ""
            if prompt
            else f"{elapsed:.1f}s against a {LOGIN_BUDGET_SECONDS}s budget, so the "
            "coordinator waited the flow out instead of failing it",
        )
    finally:
        disconnect(lib, env, dbc)


def scenario_the_driver_manager_forwards_the_completion(stack, shim):
    """unixODBC passes a non-NOPROMPT *DriverCompletion* through to the driver.

    Every other scenario here loads the driver directly, so nothing else would
    notice a Driver Manager that flattened the argument. This one goes through
    `libodbc.so.2`, which is the path `isql`, Power BI and Excel take. It takes
    no driver path: the connection string carries `Driver`, and the Driver
    Manager is what loads it.

    **The connect succeeding is the whole proof, and no browser launch is
    required for it.** `resolve_auth` refuses `ExternalAuthentication` on the
    prompting flag alone, before `oauth2_auth` is ever consulted, so a connection
    the Driver Manager had marked `SQL_DRIVER_NOPROMPT` would fail here even with
    a token already cached. Which is just as well: unixODBC loads the same `.so`
    this process already has open, so `OAUTH2_LOGINS` is shared with every
    scenario above and a login this identity has performed is served from cache.
    Naming a fresh `User` to force a login is not the way out, because a `User`
    disagreeing with the token is refused as an impersonation attempt.
    """
    try:
        dm = load("libodbc.so.2")
    except OSError as e:
        R.skip(
            "unixODBC forwards the DriverCompletion", f"libodbc.so.2 not loadable: {e}"
        )
        return

    shim.reset("login")
    ret, env, dbc = odbc_connect(dm, oauth_conn(stack))
    try:
        if not check_connected(
            dm, dbc, ret, "unixODBC forwards a non-NOPROMPT DriverCompletion"
        ):
            return
        user = scalar(dm, dbc, "SELECT current_user")
        R.check(
            "the Driver Manager path resolves the same session user",
            user == stack.get("TRINO_USER"),
            f"current_user is {user!r}",
        )
    finally:
        disconnect(dm, env, dbc)


def scenario_noprompt_is_refused(lib, stack, shim):
    """`SQL_DRIVER_NOPROMPT` forbids the prompt an interactive login needs, so
    the connection is refused before any network I/O.

    Needs no identity provider, and belongs beside its opposite: the two
    together are what say the *DriverCompletion* gate is the thing deciding.
    This is also the reason the suite cannot use pyodbc, which passes this value
    unconditionally.
    """
    shim.reset("login")
    ret, env, dbc = odbc_connect(
        lib, oauth_conn(stack, User="noprompt"), completion=SQL_DRIVER_NOPROMPT
    )
    try:
        state = sqlstate(lib, SQL_HANDLE_DBC, dbc)
        message = diag_message(lib, SQL_HANDLE_DBC, dbc)
        refused = ret == SQL_ERROR
        R.check(
            "ExternalAuthentication under SQL_DRIVER_NOPROMPT is refused",
            refused,
            "" if refused else f"returned {ret}",
        )
        R.check(
            "the refusal reports 28000",
            state == INVALID_AUTH_SPEC,
            f"state is {state!r}: {message}",
        )
        R.check(
            "the diagnostic names SQL_DRIVER_NOPROMPT",
            "NOPROMPT" in message,
            f"message is {message!r}",
        )
        no_browser = shim.launches() == []
        R.check(
            "no browser was launched",
            no_browser,
            "" if no_browser else f"records: {shim.launches()}",
        )
    finally:
        disconnect(lib, env, dbc)


def main():
    stack = Stack.load()
    driver = sys.argv[1] if len(sys.argv) > 1 else stack.get("DRIVER_PATH")
    if not stack.has_profile("oauth"):
        R.skip(
            "the whole suite",
            "profile 'oauth' is not active (setup.sh --profile oauth)",
        )
        return R.summary()

    lib = load(driver)
    shim = Shim(os.path.join(os.path.dirname(SUITES_DIR), "generated"))

    print("\n--- the interactive flow, and the login cache ---")
    opened = scenario_one_login_serves_many_connections(lib, stack, shim)
    scenario_the_token_supplies_the_user(lib, stack, shim, opened)
    for env, dbc in opened:
        disconnect(lib, env, dbc)

    print("\n--- the X-Trino-User trap ---")
    scenario_a_matching_user_is_honoured(lib, stack, shim)
    scenario_a_disagreeing_user_is_refused(lib, stack, shim)

    print("\n--- a login that does not succeed ---")
    scenario_an_abandoned_login_times_out(lib, stack, shim)
    scenario_a_refused_login_reports_28000(lib, stack, shim)

    print("\n--- who is allowed to prompt ---")
    scenario_noprompt_is_refused(lib, stack, shim)
    scenario_the_driver_manager_forwards_the_completion(stack, shim)

    return R.summary()


if __name__ == "__main__":
    sys.exit(main())
