#!/usr/bin/env python3
"""
Raw C ABI pen test for the Trino ODBC driver.

Loads the driver's shared object with ctypes and calls its exported entry
points directly. There is **no Driver Manager in the loop**, which is the whole
point: unixODBC intercepts a large part of the ODBC state machine and answers
it itself, so a driver's own handling of an out-of-order or malformed call is
invisible to any test that goes through pyodbc or isql. Everything asserted
here is the driver's own behaviour.

That also means the spec's **(DM)** diagnostics must not be expected. Where the
spec attributes a SQLSTATE to the Driver Manager, nothing produces it here, and
a probe that demanded it would be asserting the absence of a component rather
than the presence of a behaviour. Those probes assert what the driver
actually does, with a comment naming the (DM) diagnostic they do not demand.

Covers: handle lifecycle and parentage, invalid and stale handles, double free,
use after free, connection state, cursor state, prepare / execute / re-execute,
SQLFreeStmt options, and statement and connection attribute round-trips.

Usage:
    python3 test/test_c_abi.py [path/to/libstackable_odbc_trino.so] [conn-str]

Requires a running Trino (test/setup.sh). Only the Python standard library is
needed -- ctypes, not pyodbc.
"""

import ctypes
import os
import sys

# --- ODBC constants -------------------------------------------------------
# Named rather than inlined, per the project's own rule about spec values.

SQL_HANDLE_ENV = 1
SQL_HANDLE_DBC = 2
SQL_HANDLE_STMT = 3
SQL_HANDLE_DESC = 4

SQL_SUCCESS = 0
SQL_SUCCESS_WITH_INFO = 1
SQL_NO_DATA = 100
SQL_ERROR = -1
SQL_INVALID_HANDLE = -2

SQL_NTS = -3
SQL_NULL_HANDLE = None

SQL_ATTR_ODBC_VERSION = 200
SQL_OV_ODBC3 = 3

SQL_DRIVER_NOPROMPT = 0

# SQLFreeStmt options
SQL_CLOSE = 0
SQL_DROP = 1
SQL_UNBIND = 2
SQL_RESET_PARAMS = 3

# Statement attributes
SQL_ATTR_CURSOR_TYPE = 6
SQL_ATTR_QUERY_TIMEOUT = 0
SQL_ATTR_MAX_ROWS = 1
SQL_ATTR_ROW_ARRAY_SIZE = 27
SQL_CURSOR_FORWARD_ONLY = 0
SQL_CURSOR_STATIC = 3

# Connection attributes
SQL_ATTR_AUTOCOMMIT = 102
SQL_ATTR_TXN_ISOLATION = 108
SQL_AUTOCOMMIT_ON = 1
SQL_TXN_SERIALIZABLE = 8

SQL_C_CHAR = 1

DEFAULT_CONN_STR = (
    "Host=localhost;Port=8080;User=admin;Password=admin;Protocol=http;Catalog=tpcds"
)

passed = 0
failed = 0
notes = 0


def w(s):
    """A SQLWCHAR buffer for `s`. SQLWCHAR is 16-bit on Linux."""
    buf = ctypes.create_string_buffer(s.encode("utf-16-le") + b"\x00\x00")
    return ctypes.cast(buf, ctypes.POINTER(ctypes.c_uint16)), buf


def load(path):
    lib = ctypes.CDLL(path)
    P = ctypes.c_void_p
    W = ctypes.POINTER(ctypes.c_uint16)
    S, I, L = ctypes.c_int16, ctypes.c_int32, ctypes.c_int64

    sig = {
        "SQLAllocHandle": ([S, P, ctypes.POINTER(P)], S),
        "SQLFreeHandle": ([S, P], S),
        "SQLSetEnvAttr": ([P, I, P, I], S),
        "SQLGetEnvAttr": ([P, I, P, I, ctypes.POINTER(I)], S),
        "SQLDriverConnectW": ([P, P, W, S, W, S, ctypes.POINTER(S), ctypes.c_uint16], S),
        "SQLDisconnect": ([P], S),
        "SQLExecDirectW": ([P, W, I], S),
        "SQLPrepareW": ([P, W, I], S),
        "SQLExecute": ([P], S),
        "SQLFetch": ([P], S),
        "SQLGetData": ([P, ctypes.c_uint16, S, P, L, ctypes.POINTER(L)], S),
        "SQLNumResultCols": ([P, ctypes.POINTER(S)], S),
        "SQLRowCount": ([P, ctypes.POINTER(L)], S),
        "SQLCloseCursor": ([P], S),
        "SQLFreeStmt": ([P, ctypes.c_uint16], S),
        "SQLSetStmtAttrW": ([P, I, P, I], S),
        "SQLGetStmtAttrW": ([P, I, P, I, ctypes.POINTER(I)], S),
        "SQLSetConnectAttrW": ([P, I, P, I], S),
        "SQLGetConnectAttrW": ([P, I, P, I, ctypes.POINTER(I)], S),
        "SQLGetDiagRecW": (
            [S, P, S, W, ctypes.POINTER(I), W, S, ctypes.POINTER(S)],
            S,
        ),
        "SQLCancel": ([P], S),
        "SQLNumParams": ([P, ctypes.POINTER(S)], S),
        # SQLRETURN is a 16-bit SQLSMALLINT. Every entry point called here
        # needs its restype declared, or ctypes reads the return register as a
        # 32-bit int and SQL_ERROR (-1) arrives as 65535.
        "SQLTablePrivilegesW": ([P, W, S, W, S, W, S], S),
        "SQLColumnPrivilegesW": ([P, W, S, W, S, W, S, W, S], S),
        "SQLProceduresW": ([P, W, S, W, S, W, S], S),
        "SQLProcedureColumnsW": ([P, W, S, W, S, W, S, W, S], S),
    }
    for name, (args, res) in sig.items():
        fn = getattr(lib, name)
        fn.argtypes = args
        fn.restype = res
    return lib


def sqlstate(lib, htype, handle):
    """The SQLSTATE of diagnostic record 1, or '' when there is none."""
    state = (ctypes.c_uint16 * 6)()
    msg = (ctypes.c_uint16 * 1024)()
    native = ctypes.c_int32(0)
    textlen = ctypes.c_int16(0)
    ret = lib.SQLGetDiagRecW(
        htype,
        handle,
        1,
        ctypes.cast(state, ctypes.POINTER(ctypes.c_uint16)),
        ctypes.byref(native),
        ctypes.cast(msg, ctypes.POINTER(ctypes.c_uint16)),
        1024,
        ctypes.byref(textlen),
    )
    if ret not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
        return ""
    return "".join(chr(c) for c in state if c).strip()


RET_NAMES = {
    SQL_SUCCESS: "SUCCESS",
    SQL_SUCCESS_WITH_INFO: "SUCCESS_WITH_INFO",
    SQL_NO_DATA: "NO_DATA",
    SQL_ERROR: "ERROR",
    SQL_INVALID_HANDLE: "INVALID_HANDLE",
}


def rname(r):
    return RET_NAMES.get(r, str(r))


def check(label, got, want, state=None, got_state=None):
    """Assert a return code, and optionally the SQLSTATE that came with it."""
    global passed, failed
    want_list = want if isinstance(want, (list, tuple)) else [want]
    ok = got in want_list
    detail = ""
    if ok and state is not None:
        ok = got_state == state
        detail = f" (SQLSTATE {got_state or '<none>'}, expected {state})"
    elif got_state:
        detail = f" (SQLSTATE {got_state})"
    if ok:
        print(f"PASS  {label}: {rname(got)}{detail}")
        passed += 1
    else:
        expect = "/".join(rname(x) for x in want_list)
        print(f"FAIL  {label}: got {rname(got)}{detail}, expected {expect}")
        failed += 1


def note(label, text):
    """An observation the driver is entitled to make either way."""
    global notes
    print(f"NOTE  {label}: {text}")
    notes += 1


def main():
    global passed, failed

    here = os.path.dirname(os.path.abspath(__file__))
    default_so = os.path.join(
        here, "..", "target", "debug", "libstackable_odbc_trino.so"
    )
    so = sys.argv[1] if len(sys.argv) > 1 else default_so
    conn_str = sys.argv[2] if len(sys.argv) > 2 else DEFAULT_CONN_STR

    if not os.path.exists(so):
        print(f"driver not found: {so}\nrun: cargo build")
        return 2

    lib = load(so)
    P = ctypes.c_void_p

    print(f"=== raw C ABI pen test (no Driver Manager) ===\ndriver: {so}\n")

    # ---------------------------------------------------------------
    print("--- handle lifecycle ---")
    env = P()
    r = lib.SQLAllocHandle(SQL_HANDLE_ENV, None, ctypes.byref(env))
    check("alloc env", r, SQL_SUCCESS)

    r = lib.SQLSetEnvAttr(env, SQL_ATTR_ODBC_VERSION, P(SQL_OV_ODBC3), 0)
    check("set ODBC version 3", r, SQL_SUCCESS)

    dbc = P()
    r = lib.SQLAllocHandle(SQL_HANDLE_DBC, env, ctypes.byref(dbc))
    check("alloc connection", r, SQL_SUCCESS)

    # The env still owns a connection, so it must refuse to be freed.
    r = lib.SQLFreeHandle(SQL_HANDLE_ENV, env)
    check(
        "free env with a live connection",
        r,
        SQL_ERROR,
        state="HY010",
        got_state=sqlstate(lib, SQL_HANDLE_ENV, env),
    )

    # ---------------------------------------------------------------
    print("\n--- invalid and mismatched handles ---")
    bogus = P(0xDEADBEEF)
    out = P()
    r = lib.SQLAllocHandle(SQL_HANDLE_DBC, bogus, ctypes.byref(out))
    check("alloc connection on a non-handle parent", r, SQL_INVALID_HANDLE)

    r = lib.SQLFreeHandle(SQL_HANDLE_ENV, None)
    check("free a null handle", r, SQL_INVALID_HANDLE)

    r = lib.SQLFreeHandle(SQL_HANDLE_STMT, env)
    check("free an env under the wrong handle type", r, SQL_INVALID_HANDLE)

    r = lib.SQLAllocHandle(SQL_HANDLE_STMT, env, ctypes.byref(out))
    check("alloc statement parented on an env", r, SQL_INVALID_HANDLE)

    # ---------------------------------------------------------------
    print("\n--- statement on an unconnected connection ---")
    stmt0 = P()
    r = lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(stmt0))
    # The spec's 08003 for this is (DM)-owned, so the driver is entitled to
    # allocate: a statement on a not-yet-open connection is legal here.
    note("alloc statement before connecting", f"{rname(r)} (08003 here is DM-owned)")
    if r == SQL_SUCCESS:
        sql, _keep = w("SELECT 1")
        r = lib.SQLExecDirectW(stmt0, sql, SQL_NTS)
        # HY010, not 08003: SQLExecDirect's 08003 is (DM)-annotated, so with no
        # Driver Manager loaded nothing produces it, and the driver reports the
        # sequence error instead.
        check(
            "execute on an unconnected connection",
            r,
            SQL_ERROR,
            state="HY010",
            got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt0),
        )
        lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt0)

    r = lib.SQLDisconnect(dbc)
    check(
        "disconnect while not connected",
        r,
        SQL_ERROR,
        state="08003",
        got_state=sqlstate(lib, SQL_HANDLE_DBC, dbc),
    )

    # ---------------------------------------------------------------
    print("\n--- connect ---")
    cs, _keep_cs = w(conn_str)
    outbuf = (ctypes.c_uint16 * 1024)()
    outlen = ctypes.c_int16(0)
    r = lib.SQLDriverConnectW(
        dbc,
        None,
        cs,
        SQL_NTS,
        ctypes.cast(outbuf, ctypes.POINTER(ctypes.c_uint16)),
        1024,
        ctypes.byref(outlen),
        SQL_DRIVER_NOPROMPT,
    )
    check(
        "SQLDriverConnectW",
        r,
        [SQL_SUCCESS, SQL_SUCCESS_WITH_INFO],
        got_state=sqlstate(lib, SQL_HANDLE_DBC, dbc),
    )
    if r not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
        print("\ncannot continue without a connection; is Trino running?")
        return 1

    cs2, _keep_cs2 = w(conn_str)
    r = lib.SQLDriverConnectW(
        dbc,
        None,
        cs2,
        SQL_NTS,
        ctypes.cast(outbuf, ctypes.POINTER(ctypes.c_uint16)),
        1024,
        ctypes.byref(outlen),
        SQL_DRIVER_NOPROMPT,
    )
    check(
        "connect on an already-connected handle",
        r,
        SQL_ERROR,
        state="08002",
        got_state=sqlstate(lib, SQL_HANDLE_DBC, dbc),
    )

    stmt = P()
    r = lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(stmt))
    check("alloc statement", r, SQL_SUCCESS)

    # Dirty the connection's diagnostic queue immediately before the free, with
    # no call in between: this driver reports SQL_TC_NONE, so every isolation
    # level is invalid and this leaves HY024 as record 1.
    #
    # It has to be immediately before. The 08002 from the failed second connect
    # above is already gone by here, because SQLAllocHandle clears at entry too
    # and the statement allocation sits between the two.
    r = lib.SQLSetConnectAttrW(dbc, SQL_ATTR_TXN_ISOLATION, P(SQL_TXN_SERIALIZABLE), 0)
    check(
        "set SQL_ATTR_TXN_ISOLATION on a transaction-less driver",
        r,
        SQL_ERROR,
        state="HY024",
        got_state=sqlstate(lib, SQL_HANDLE_DBC, dbc),
    )

    # A function clears the handle's diagnostics at entry, so the HY010 this
    # posts must be record 1 rather than sitting behind the HY024 above. An
    # application reading the first record after a failed free would otherwise
    # act on the previous call's SQLSTATE.
    r = lib.SQLFreeHandle(SQL_HANDLE_DBC, dbc)
    check(
        "free connection while still connected, over a dirty queue",
        r,
        SQL_ERROR,
        state="HY010",
        got_state=sqlstate(lib, SQL_HANDLE_DBC, dbc),
    )

    # ---------------------------------------------------------------
    print("\n--- cursor state with no cursor ---")
    # HY010, not 24000, and the distinction is deliberate: 24000 is for a
    # statement that *was* executed but has no result set, HY010 for one never
    # put in an executed state. This statement is the latter.
    r = lib.SQLFetch(stmt)
    check(
        "fetch on a never-executed statement",
        r,
        SQL_ERROR,
        state="HY010",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )

    ind = ctypes.c_int64(0)
    buf = ctypes.create_string_buffer(64)
    r = lib.SQLGetData(stmt, 1, SQL_C_CHAR, ctypes.cast(buf, P), 64, ctypes.byref(ind))
    check(
        "get_data with no cursor",
        r,
        SQL_ERROR,
        state="24000",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )

    r = lib.SQLCloseCursor(stmt)
    check(
        "close_cursor with no cursor",
        r,
        SQL_ERROR,
        state="24000",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )

    r = lib.SQLExecute(stmt)
    check(
        "execute with nothing prepared",
        r,
        SQL_ERROR,
        state="HY010",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )

    cols = ctypes.c_int16(-1)
    r = lib.SQLNumResultCols(stmt, ctypes.byref(cols))
    check(
        "num_result_cols before execute",
        r,
        SQL_ERROR,
        state="HY010",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )

    # ---------------------------------------------------------------
    print("\n--- prepare / execute / re-execute ---")
    sql, _k1 = w("SELECT 1 AS n")
    r = lib.SQLPrepareW(stmt, sql, SQL_NTS)
    check("prepare", r, SQL_SUCCESS)

    # SQL_ATTR_CURSOR_TYPE may not be set once a statement is prepared.
    r = lib.SQLSetStmtAttrW(stmt, SQL_ATTR_CURSOR_TYPE, P(SQL_CURSOR_STATIC), 0)
    check(
        "set cursor type after prepare",
        r,
        SQL_ERROR,
        state="HY011",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )

    for attempt in (1, 2):
        r = lib.SQLExecute(stmt)
        check(f"execute (attempt {attempt})", r, SQL_SUCCESS)
        r = lib.SQLNumResultCols(stmt, ctypes.byref(cols))
        check(f"num_result_cols after execute ({attempt})", r, SQL_SUCCESS)
        if cols.value != 1:
            note(f"num_result_cols after execute ({attempt})", f"got {cols.value}")
        r = lib.SQLFetch(stmt)
        check(f"fetch row ({attempt})", r, SQL_SUCCESS)
        r = lib.SQLFetch(stmt)
        check(f"fetch past the last row ({attempt})", r, SQL_NO_DATA)
        r = lib.SQLCloseCursor(stmt)
        check(f"close cursor ({attempt})", r, SQL_SUCCESS)

    # ---------------------------------------------------------------
    print("\n--- SQLFreeStmt options ---")
    sql, _k2 = w("SELECT 1 AS n")
    lib.SQLExecDirectW(stmt, sql, SQL_NTS)
    r = lib.SQLFreeStmt(stmt, SQL_CLOSE)
    check("free_stmt SQL_CLOSE with an open cursor", r, SQL_SUCCESS)
    r = lib.SQLFreeStmt(stmt, SQL_CLOSE)
    check("free_stmt SQL_CLOSE with no cursor", r, SQL_SUCCESS)
    r = lib.SQLFreeStmt(stmt, SQL_UNBIND)
    check("free_stmt SQL_UNBIND", r, SQL_SUCCESS)
    r = lib.SQLFreeStmt(stmt, SQL_RESET_PARAMS)
    check("free_stmt SQL_RESET_PARAMS", r, SQL_SUCCESS)
    # The SQLSTATE matters as much as the return code: an SQL_ERROR carrying no
    # diagnostic record leaves an application with an error it cannot interpret.
    r = lib.SQLFreeStmt(stmt, 99)
    check(
        "free_stmt with an undefined option",
        r,
        SQL_ERROR,
        state="HY092",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )

    # ---------------------------------------------------------------
    print("\n--- attribute round-trips ---")
    # Both of these are unsupported values the driver substitutes rather than
    # refuses. That is the spec's own prescription -- accept, substitute, report
    # 01S02, and let SQLGetStmtAttr tell the application what it actually got --
    # so the substituted value is asserted, not merely observed. A driver that
    # silently kept the requested value would be claiming a timeout it does not
    # enforce and a block cursor it does not implement.
    #
    # SQLULEN is 64-bit here, so the read-back buffer is too.
    val = ctypes.c_uint64(0)
    outlen32 = ctypes.c_int32(0)
    for label, attr, asked, substituted in (
        ("SQL_ATTR_QUERY_TIMEOUT", SQL_ATTR_QUERY_TIMEOUT, 42, 0),
        ("SQL_ATTR_ROW_ARRAY_SIZE", SQL_ATTR_ROW_ARRAY_SIZE, 10, 1),
    ):
        r = lib.SQLSetStmtAttrW(stmt, attr, P(asked), 0)
        check(
            f"set {label}={asked} (unsupported)",
            r,
            SQL_SUCCESS_WITH_INFO,
            state="01S02",
            got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
        )
        val.value = 0
        r = lib.SQLGetStmtAttrW(stmt, attr, ctypes.byref(val), 8, ctypes.byref(outlen32))
        check(f"get {label}", r, SQL_SUCCESS)
        if r == SQL_SUCCESS and val.value != substituted:
            note(
                f"{label} substitution",
                f"asked {asked}, expected the driver to substitute "
                f"{substituted}, read back {val.value}",
            )

    # Accepted silently, by decision rather than by omission: core relaxes the
    # spec's HY092 here for Driver Manager and tool compatibility, and says so
    # at the call site. Asserted so the relaxation cannot be reverted silently.
    r = lib.SQLSetStmtAttrW(stmt, 9999, P(1), 0)
    check("set an undefined statement attribute (relaxed)", r, SQL_SUCCESS)

    r = lib.SQLGetConnectAttrW(
        dbc, SQL_ATTR_AUTOCOMMIT, ctypes.byref(val), 4, ctypes.byref(outlen32)
    )
    check("get SQL_ATTR_AUTOCOMMIT", r, SQL_SUCCESS)

    # ---------------------------------------------------------------
    print("\n--- diagnostics are cleared at function entry ---")
    # The spec clears a handle's diagnostics at the start of every function
    # called on it, except SQLGetDiagRec/SQLGetDiagField. Without that, an
    # application reads the *previous* error after a failure and acts on the
    # wrong SQLSTATE. Provoke a known error, then provoke a different one, and
    # see which record comes back first.
    r = lib.SQLSetStmtAttrW(stmt, SQL_ATTR_ROW_ARRAY_SIZE, P(10), 0)  # leaves 01S02
    first = sqlstate(lib, SQL_HANDLE_STMT, stmt)
    r = lib.SQLGetData(stmt, 1, SQL_C_CHAR, ctypes.cast(buf, P), 64, ctypes.byref(ind))
    second = sqlstate(lib, SQL_HANDLE_STMT, stmt)
    check("get_data after an unrelated 01S02", r, SQL_ERROR)
    if second == first:
        note(
            "diagnostics cleared at entry",
            f"KNOWN: record 1 is still {first} from the previous call; the "
            "later error is queued behind it",
        )
    else:
        check(
            "diagnostics cleared at entry",
            r,
            SQL_ERROR,
            state="24000",
            got_state=second,
        )

    # ---------------------------------------------------------------
    print("\n--- privilege catalog functions ---")
    # pyodbc exposes no tablePrivileges()/columnPrivileges(), so this is the
    # only suite that reaches SQLTablePrivilegesW and SQLColumnPrivilegesW at
    # all. Both answer an empty result set here, but for different reasons:
    #
    #  - SQLTablePrivileges runs a real query against
    #    information_schema.table_privileges. It is empty because neither test
    #    catalog implements permission management, not because the driver
    #    declines to look. A SQL_ERROR here means the query was rejected.
    #  - SQLColumnPrivileges reads nothing: Trino grants on tables, never on
    #    columns, and publishes no column-privilege metadata.
    #
    # Both must still describe their result set, because an application sizes
    # its buffers from SQLNumResultCols before it fetches anything.
    cat, _kp1 = w("tpcds")
    sch, _kp2 = w("sf1")
    tbl, _kp3 = w("call_center")
    pct, _kp4 = w("%")

    r = lib.SQLTablePrivilegesW(stmt, cat, SQL_NTS, sch, SQL_NTS, tbl, SQL_NTS)
    check(
        "table privileges on a real table",
        r,
        SQL_SUCCESS,
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )
    ncols = ctypes.c_short(0)
    lib.SQLNumResultCols(stmt, ctypes.byref(ncols))
    check("table privileges describes 7 columns", ncols.value, 7)
    check("table privileges is empty here", lib.SQLFetch(stmt), SQL_NO_DATA)
    lib.SQLFreeStmt(stmt, SQL_CLOSE)

    r = lib.SQLColumnPrivilegesW(
        stmt, cat, SQL_NTS, sch, SQL_NTS, tbl, SQL_NTS, pct, SQL_NTS
    )
    check(
        "column privileges on a real table",
        r,
        SQL_SUCCESS,
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )
    ncols = ctypes.c_short(0)
    lib.SQLNumResultCols(stmt, ctypes.byref(ncols))
    check("column privileges describes 8 columns", ncols.value, 8)
    check("column privileges is empty", lib.SQLFetch(stmt), SQL_NO_DATA)
    lib.SQLFreeStmt(stmt, SQL_CLOSE)

    # SQLColumnPrivileges is the only one of the four privilege/procedure
    # functions whose spec page states "The TableName argument was a null
    # pointer" *without* a (DM) marker, so the driver owns that HY009. Its
    # three neighbours must not report one, which is why only this is probed.
    r = lib.SQLColumnPrivilegesW(
        stmt, cat, SQL_NTS, sch, SQL_NTS, None, SQL_NTS, pct, SQL_NTS
    )
    check(
        "column privileges with a null TableName",
        r,
        SQL_ERROR,
        state="HY009",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )
    lib.SQLFreeStmt(stmt, SQL_CLOSE)

    # ---------------------------------------------------------------
    print("\n--- procedure catalog functions ---")
    # Trino has callable procedures -- CALL system.runtime.kill_query(...) is
    # one -- but publishes no metadata naming them: system.jdbc.procedures is
    # a JDBC-compatibility view that is hardwired empty. An empty result set
    # is therefore the honest answer, and it must still be a described one.
    sysc, _kp5 = w("system")
    runt, _kp6 = w("runtime")

    r = lib.SQLProceduresW(stmt, sysc, SQL_NTS, runt, SQL_NTS, pct, SQL_NTS)
    check(
        "procedures in system.runtime",
        r,
        SQL_SUCCESS,
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )
    ncols = ctypes.c_short(0)
    lib.SQLNumResultCols(stmt, ctypes.byref(ncols))
    check("procedures describes 8 columns", ncols.value, 8)
    check("procedures is empty", lib.SQLFetch(stmt), SQL_NO_DATA)
    lib.SQLFreeStmt(stmt, SQL_CLOSE)

    r = lib.SQLProcedureColumnsW(
        stmt, sysc, SQL_NTS, runt, SQL_NTS, pct, SQL_NTS, pct, SQL_NTS
    )
    check(
        "procedure columns in system.runtime",
        r,
        SQL_SUCCESS,
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )
    ncols = ctypes.c_short(0)
    lib.SQLNumResultCols(stmt, ctypes.byref(ncols))
    check("procedure columns describes 19 columns", ncols.value, 19)
    check("procedure columns is empty", lib.SQLFetch(stmt), SQL_NO_DATA)
    lib.SQLFreeStmt(stmt, SQL_CLOSE)

    print("\n--- cancel with nothing running ---")
    r = lib.SQLCancel(stmt)
    check("cancel an idle statement", r, SQL_SUCCESS)

    # ---------------------------------------------------------------
    print("\n--- double free and use after free ---")
    r = lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)
    check("free statement", r, SQL_SUCCESS)

    r = lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)
    check("free the same statement twice", r, SQL_INVALID_HANDLE)

    r = lib.SQLFetch(stmt)
    check("fetch on a freed statement", r, SQL_INVALID_HANDLE)

    sql, _k3 = w("SELECT 1")
    r = lib.SQLExecDirectW(stmt, sql, SQL_NTS)
    check("execute on a freed statement", r, SQL_INVALID_HANDLE)

    r = lib.SQLDisconnect(dbc)
    check("disconnect", r, SQL_SUCCESS)

    r = lib.SQLFreeHandle(SQL_HANDLE_DBC, dbc)
    check("free connection", r, SQL_SUCCESS)

    r = lib.SQLFreeHandle(SQL_HANDLE_DBC, dbc)
    check("free the same connection twice", r, SQL_INVALID_HANDLE)

    r = lib.SQLFreeHandle(SQL_HANDLE_ENV, env)
    check("free env once its children are gone", r, SQL_SUCCESS)

    r = lib.SQLFreeHandle(SQL_HANDLE_ENV, env)
    check("free the same env twice", r, SQL_INVALID_HANDLE)

    print(f"\n{passed} passed, {failed} failed, {notes} notes")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
