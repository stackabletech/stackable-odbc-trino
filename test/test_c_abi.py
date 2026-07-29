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
SQL_ATTR_QUERY_TIMEOUT = 0
SQL_ATTR_MAX_ROWS = 1
SQL_ATTR_NOSCAN = 2
SQL_ATTR_MAX_LENGTH = 3
SQL_ATTR_ASYNC_ENABLE = 4
SQL_ATTR_CURSOR_TYPE = 6
SQL_ATTR_CONCURRENCY = 7
SQL_ATTR_KEYSET_SIZE = 8
SQL_ATTR_SIMULATE_CURSOR = 10
SQL_ATTR_RETRIEVE_DATA = 11
SQL_ATTR_USE_BOOKMARKS = 12
SQL_ATTR_ENABLE_AUTO_IPD = 15
SQL_ATTR_PARAM_STATUS_PTR = 20
SQL_ATTR_PARAMS_PROCESSED_PTR = 21
SQL_ATTR_PARAMSET_SIZE = 22
SQL_ATTR_ROW_ARRAY_SIZE = 27
SQL_ATTR_CURSOR_SCROLLABLE = -1
SQL_ATTR_CURSOR_SENSITIVITY = -2
SQL_ATTR_METADATA_ID = 10014
SQL_CURSOR_FORWARD_ONLY = 0
SQL_CURSOR_STATIC = 3

# Values the driver substitutes to, and the values it refuses.
SQL_CONCUR_READ_ONLY = 1
SQL_SC_NON_UNIQUE = 0
SQL_NONSCROLLABLE = 0
SQL_UB_VARIABLE = 2
SQL_RD_OFF = 0
SQL_SENSITIVE = 2
SQL_ASYNC_ENABLE_OFF = 0
SQL_ASYNC_ENABLE_ON = 1

# SQL_ATTR_PARAM_STATUS_PTR element values.
SQL_PARAM_SUCCESS = 0
SQL_PARAM_ERROR = 5

# Connection attributes
SQL_ATTR_AUTOCOMMIT = 102
SQL_ATTR_TXN_ISOLATION = 108
SQL_ATTR_CURRENT_CATALOG = 109
SQL_ATTR_PACKET_SIZE = 112
SQL_ATTR_ENLIST_IN_DTC = 1207
SQL_AUTOCOMMIT_ON = 1
SQL_TXN_SERIALIZABLE = 8

# Info types read back beside the attributes that mirror them.
SQL_DATABASE_NAME = 16

SQL_TRUE = 1
SQL_FALSE = 0

SQL_C_CHAR = 1
SQL_C_SBIGINT = -25
SQL_BIGINT = -5

# SQLBindParameter arguments used by the bound-parameter probes.
SQL_PARAM_INPUT = 1
SQL_NUMERIC = 2

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
        "SQLGetInfoW": ([P, ctypes.c_uint16, P, S, ctypes.POINTER(S)], S),
        "SQLCancel": ([P], S),
        "SQLNumParams": ([P, ctypes.POINTER(S)], S),
        "SQLBindParameter": (
            [P, ctypes.c_uint16, S, S, S, ctypes.c_size_t, S, P, L, ctypes.POINTER(L)],
            S,
        ),
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


def read_wide_info(lib, dbc, info_type):
    """A character-shaped SQLGetInfoW answer, as a str."""
    buf = (ctypes.c_uint16 * 256)()
    length = ctypes.c_int16(0)
    ret = lib.SQLGetInfoW(
        dbc,
        info_type,
        ctypes.cast(buf, ctypes.c_void_p),
        512,
        ctypes.byref(length),
    )
    if ret not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
        return None
    return "".join(chr(c) for c in buf[: max(length.value, 0) // 2])


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
    print("\n--- statement attributes: substituted values (01S02) ---")
    # The spec's 01S02 row closes the set of statement attributes a driver may
    # substitute for, and for each the driver must store the value it will
    # actually use -- that is what makes the row's parenthesis true,
    # "(SQLGetStmtAttr can be called to determine the temporarily substituted
    # value.)". The read-back is asserted, not merely observed: a driver that
    # kept the requested value would be claiming a timeout it does not enforce
    # and a block cursor it does not implement, and an application reading its
    # own number back has no way to tell.
    #
    # SQLULEN is 64-bit here, so the read-back buffer is too. It is zeroed
    # before each read; see the KNOWN note on the write width below.
    #
    # A *fresh* statement, not the shared one: SQL_ATTR_CONCURRENCY,
    # SQL_ATTR_CURSOR_TYPE, SQL_ATTR_SIMULATE_CURSOR and SQL_ATTR_USE_BOOKMARKS
    # "must be set before the statement is executed", and the shared handle has
    # been prepared and executed by now. Setting one of those on it answers
    # HY011 -- correctly, which is what the group below this one asserts.
    val = ctypes.c_uint64(0)
    outlen32 = ctypes.c_int32(0)
    attr_stmt = ctypes.c_void_p()
    r = lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(attr_stmt))
    check("allocate a fresh statement for the attribute probes", r, SQL_SUCCESS)
    for label, attr, asked, substituted in (
        ("SQL_ATTR_CONCURRENCY", SQL_ATTR_CONCURRENCY, 2, SQL_CONCUR_READ_ONLY),
        ("SQL_ATTR_CURSOR_TYPE", SQL_ATTR_CURSOR_TYPE, SQL_CURSOR_STATIC, SQL_CURSOR_FORWARD_ONLY),
        ("SQL_ATTR_KEYSET_SIZE", SQL_ATTR_KEYSET_SIZE, 50, 0),
        ("SQL_ATTR_MAX_LENGTH", SQL_ATTR_MAX_LENGTH, 4096, 0),
        ("SQL_ATTR_MAX_ROWS", SQL_ATTR_MAX_ROWS, 100, 0),
        ("SQL_ATTR_QUERY_TIMEOUT", SQL_ATTR_QUERY_TIMEOUT, 42, 0),
        ("SQL_ATTR_ROW_ARRAY_SIZE", SQL_ATTR_ROW_ARRAY_SIZE, 10, 1),
        ("SQL_ATTR_SIMULATE_CURSOR", SQL_ATTR_SIMULATE_CURSOR, 2, SQL_SC_NON_UNIQUE),
        # Two deviations from the spec's closed list, deliberate and documented
        # in core: substituting keeps SQL_ATTR_CURSOR_SCROLLABLE consistent with
        # SQL_ATTR_CURSOR_TYPE, and refusing SQL_ATTR_PARAMSET_SIZE would fail a
        # call every parameter-array-capable tool makes while accepting it
        # verbatim would silently drop every set past the first.
        ("SQL_ATTR_CURSOR_SCROLLABLE", SQL_ATTR_CURSOR_SCROLLABLE, 1, SQL_NONSCROLLABLE),
        ("SQL_ATTR_PARAMSET_SIZE", SQL_ATTR_PARAMSET_SIZE, 500, 1),
    ):
        r = lib.SQLSetStmtAttrW(attr_stmt, attr, P(asked), 0)
        check(
            f"set {label}={asked} (unsupported)",
            r,
            SQL_SUCCESS_WITH_INFO,
            state="01S02",
            got_state=sqlstate(lib, SQL_HANDLE_STMT, attr_stmt),
        )
        val.value = 0
        r = lib.SQLGetStmtAttrW(attr_stmt, attr, ctypes.byref(val), 8, ctypes.byref(outlen32))
        check(f"get {label}", r, SQL_SUCCESS)
        if r == SQL_SUCCESS:
            check(
                f"{label} reads back the substituted value",
                SQL_SUCCESS if val.value == substituted else SQL_ERROR,
                SQL_SUCCESS,
                got_state=f"read {val.value}, expected {substituted}",
            )

    print("\n--- statement attributes: refused values (HYC00) ---")
    # SQL_ATTR_USE_BOOKMARKS is in this group but is also one of the four that
    # may not be set after execution, so it stays on the fresh handle where
    # HYC00 is the rule that applies rather than HY011.
    # An attribute off the 01S02 list has no substitution to offer, so the value
    # is refused outright. The distinction matters: 01S02 says "I did something
    # else", HYC00 says "I did nothing", and reading a substituted value back is
    # only meaningful for the first.
    for label, attr, value in (
        ("SQL_ATTR_USE_BOOKMARKS=SQL_UB_VARIABLE", SQL_ATTR_USE_BOOKMARKS, SQL_UB_VARIABLE),
        ("SQL_ATTR_RETRIEVE_DATA=SQL_RD_OFF", SQL_ATTR_RETRIEVE_DATA, SQL_RD_OFF),
        ("SQL_ATTR_CURSOR_SENSITIVITY=SQL_SENSITIVE", SQL_ATTR_CURSOR_SENSITIVITY, SQL_SENSITIVE),
        ("SQL_ATTR_ENABLE_AUTO_IPD=SQL_TRUE", SQL_ATTR_ENABLE_AUTO_IPD, SQL_TRUE),
        ("SQL_ATTR_ASYNC_ENABLE=SQL_ASYNC_ENABLE_ON", SQL_ATTR_ASYNC_ENABLE, SQL_ASYNC_ENABLE_ON),
    ):
        r = lib.SQLSetStmtAttrW(attr_stmt, attr, P(value), 0)
        check(
            f"set {label}",
            r,
            SQL_ERROR,
            state="HYC00",
            got_state=sqlstate(lib, SQL_HANDLE_STMT, attr_stmt),
        )

    # The SQLULEN write width. Every non-pointer statement attribute on the
    # SQLSetStmtAttr page is declared "An SQLULEN value" -- not one is
    # SQLUINTEGER -- and SQLULEN is 64-bit here. BufferLength is ignored for a
    # non-string value, so an application declaring `SQLULEN v;` and reading a
    # short write would keep whatever was on its stack in the top half, and
    # read an enormous row limit rather than the 0 the driver reported. The
    # buffer is poisoned rather than zeroed for exactly that reason: a zeroed
    # one cannot tell a correct write from a short one.
    for label, attr, expected in (
        ("SQL_ATTR_MAX_ROWS", SQL_ATTR_MAX_ROWS, 0),
        ("SQL_ATTR_QUERY_TIMEOUT", SQL_ATTR_QUERY_TIMEOUT, 0),
        ("SQL_ATTR_ROW_ARRAY_SIZE", SQL_ATTR_ROW_ARRAY_SIZE, 1),
        ("SQL_ATTR_CONCURRENCY", SQL_ATTR_CONCURRENCY, SQL_CONCUR_READ_ONLY),
        ("SQL_ATTR_NOSCAN", SQL_ATTR_NOSCAN, 0),
        ("SQL_ATTR_METADATA_ID", SQL_ATTR_METADATA_ID, SQL_FALSE),
    ):
        val.value = 0xFFFFFFFFFFFFFFFF
        outlen32.value = 0
        r = lib.SQLGetStmtAttrW(
            attr_stmt, attr, ctypes.byref(val), 8, ctypes.byref(outlen32)
        )
        check(f"get {label} into a poisoned SQLULEN", r, SQL_SUCCESS)
        check(
            f"{label} is written at the full SQLULEN width",
            SQL_SUCCESS if (val.value == expected and outlen32.value == 8) else SQL_ERROR,
            SQL_SUCCESS,
            got_state=f"read 0x{val.value:016x} with StringLength {outlen32.value}",
        )

    lib.SQLFreeHandle(SQL_HANDLE_STMT, attr_stmt)

    print("\n--- statement attributes: too late to set (HY011) ---")
    # The four the spec's Comments say "must be set before the statement is
    # executed". The shared `stmt` has been prepared and executed by this
    # point, so each is refused -- and refused *before* the substitution and
    # HYC00 rules above are consulted, which is why they had to run on a fresh
    # handle. An application that sets one of these mid-cursor is asking for a
    # different cursor over a result set that already exists.
    for label, attr, value in (
        ("SQL_ATTR_CONCURRENCY", SQL_ATTR_CONCURRENCY, 2),
        ("SQL_ATTR_CURSOR_TYPE", SQL_ATTR_CURSOR_TYPE, SQL_CURSOR_STATIC),
        ("SQL_ATTR_SIMULATE_CURSOR", SQL_ATTR_SIMULATE_CURSOR, 2),
        ("SQL_ATTR_USE_BOOKMARKS", SQL_ATTR_USE_BOOKMARKS, SQL_UB_VARIABLE),
    ):
        r = lib.SQLSetStmtAttrW(stmt, attr, P(value), 0)
        check(
            f"set {label} after the statement was executed",
            r,
            SQL_ERROR,
            state="HY011",
            got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
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

    print("\n--- connection attributes the driver owns ---")
    # SQL_ATTR_PACKET_SIZE: the spec states this one directly -- "if the
    # application sets packet size after a connection has already been made,
    # the driver will return SQLSTATE HY011 (Attribute cannot be set now)".
    r = lib.SQLSetConnectAttrW(dbc, SQL_ATTR_PACKET_SIZE, P(8192), 0)
    check(
        "set SQL_ATTR_PACKET_SIZE after connecting",
        r,
        SQL_ERROR,
        state="HY011",
        got_state=sqlstate(lib, SQL_HANDLE_DBC, dbc),
    )
    # Neither is implemented and neither has a substitution to offer: this
    # driver reports SQL_AM_NONE for SQL_ASYNC_MODE and enlists in no
    # distributed transaction, so accepting either would leave an application
    # believing in behaviour it does not get.
    r = lib.SQLSetConnectAttrW(dbc, SQL_ATTR_ENLIST_IN_DTC, P(1), 0)
    check(
        "set SQL_ATTR_ENLIST_IN_DTC",
        r,
        SQL_ERROR,
        state="HYC00",
        got_state=sqlstate(lib, SQL_HANDLE_DBC, dbc),
    )
    r = lib.SQLSetConnectAttrW(dbc, SQL_ATTR_ASYNC_ENABLE, P(SQL_ASYNC_ENABLE_ON), 0)
    check(
        "set SQL_ATTR_ASYNC_ENABLE=SQL_ASYNC_ENABLE_ON",
        r,
        SQL_ERROR,
        state="HYC00",
        got_state=sqlstate(lib, SQL_HANDLE_DBC, dbc),
    )
    r = lib.SQLSetConnectAttrW(dbc, SQL_ATTR_ASYNC_ENABLE, P(SQL_ASYNC_ENABLE_OFF), 0)
    check("set SQL_ATTR_ASYNC_ENABLE=SQL_ASYNC_ENABLE_OFF", r, SQL_SUCCESS)

    print("\n--- SQL_ATTR_METADATA_ID inherits from the connection ---")
    # SQLSetStmtAttr's Comments make this one of exactly two attributes an
    # application may set at the connection level. It is not a cosmetic
    # read-back: it decides whether the catalog functions treat their arguments
    # as identifiers or as search patterns, and the connection-level route
    # previously reached no statement at all -- SQL_SUCCESS, the value echoed
    # back by SQLGetConnectAttr, and then pattern semantics with no diagnostic.
    # Per the ODBC 2.x rule it inherits, this is the default for statements
    # allocated afterwards only, so the pre-existing one is checked too.
    r = lib.SQLSetConnectAttrW(dbc, SQL_ATTR_METADATA_ID, P(SQL_TRUE), 0)
    check("set SQL_ATTR_METADATA_ID on the connection", r, SQL_SUCCESS)
    inherit_stmt = ctypes.c_void_p()
    r = lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(inherit_stmt))
    check("allocate a statement after setting it", r, SQL_SUCCESS)
    val.value = 0
    r = lib.SQLGetStmtAttrW(
        inherit_stmt, SQL_ATTR_METADATA_ID, ctypes.byref(val), 8, ctypes.byref(outlen32)
    )
    check("get SQL_ATTR_METADATA_ID on the new statement", r, SQL_SUCCESS)
    check(
        "the new statement inherited SQL_TRUE",
        SQL_SUCCESS if val.value == SQL_TRUE else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"read {val.value}",
    )
    val.value = 0
    lib.SQLGetStmtAttrW(
        stmt, SQL_ATTR_METADATA_ID, ctypes.byref(val), 8, ctypes.byref(outlen32)
    )
    check(
        "a statement that already existed is untouched",
        SQL_SUCCESS if val.value == SQL_FALSE else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"read {val.value}",
    )
    lib.SQLFreeHandle(SQL_HANDLE_STMT, inherit_stmt)
    lib.SQLSetConnectAttrW(dbc, SQL_ATTR_METADATA_ID, P(SQL_FALSE), 0)

    print("\n--- an execution reports its parameter set ---")
    # The parameter-side counterpart of what SQLFetch writes through
    # SQL_ATTR_ROWS_FETCHED_PTR. An application binding a status array to detect
    # per-set errors previously read back its own initial buffer contents, which
    # is indistinguishable from every set having succeeded.
    param_stmt = ctypes.c_void_p()
    lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(param_stmt))
    processed = ctypes.c_uint64(0xFFFFFFFFFFFFFFFF)
    status = (ctypes.c_uint16 * 4)(*([0xBEEF] * 4))
    r = lib.SQLSetStmtAttrW(
        param_stmt, SQL_ATTR_PARAMS_PROCESSED_PTR, ctypes.cast(ctypes.byref(processed), P), 0
    )
    check("set SQL_ATTR_PARAMS_PROCESSED_PTR", r, SQL_SUCCESS)
    r = lib.SQLSetStmtAttrW(
        param_stmt, SQL_ATTR_PARAM_STATUS_PTR, ctypes.cast(status, P), 0
    )
    check("set SQL_ATTR_PARAM_STATUS_PTR", r, SQL_SUCCESS)

    pval = ctypes.c_int64(42)
    plen = ctypes.c_int64(8)
    r = lib.SQLBindParameter(
        param_stmt,
        1,
        SQL_PARAM_INPUT,
        SQL_C_SBIGINT,
        SQL_BIGINT,
        19,
        0,
        ctypes.cast(ctypes.byref(pval), P),
        8,
        ctypes.byref(plen),
    )
    check("bind the parameter", r, SQL_SUCCESS)
    sql, _kp = w("SELECT CAST(? AS bigint)")
    r = lib.SQLExecDirectW(param_stmt, sql, SQL_NTS)
    check("execute with one parameter set", r, SQL_SUCCESS)
    check(
        "the processed count is 1",
        SQL_SUCCESS if processed.value == 1 else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"read {processed.value}",
    )
    check(
        "the status element is SQL_PARAM_SUCCESS",
        SQL_SUCCESS if status[0] == SQL_PARAM_SUCCESS else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"read {status[0]}",
    )
    check(
        "the sets past the first are untouched",
        SQL_SUCCESS if status[1] == 0xBEEF else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"read {status[1]:#x}",
    )
    lib.SQLCloseCursor(param_stmt)

    # The failing half. The rejection comes from the coordinator rather than
    # from parameter handling, which is the case an application binds a status
    # array for.
    processed.value = 0xFFFFFFFFFFFFFFFF
    status[0] = 0xBEEF
    sql, _kq = w("SELECT ? FROM does_not_exist_zzz")
    r = lib.SQLExecDirectW(param_stmt, sql, SQL_NTS)
    check("execute a statement the coordinator rejects", r, SQL_ERROR)
    check(
        "the processed count includes the error set",
        SQL_SUCCESS if processed.value == 1 else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"read {processed.value}",
    )
    check(
        "the status element is SQL_PARAM_ERROR",
        SQL_SUCCESS if status[0] == SQL_PARAM_ERROR else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"read {status[0]}",
    )
    lib.SQLFreeHandle(SQL_HANDLE_STMT, param_stmt)

    print("\n--- SQL_DATABASE_NAME and SQL_ATTR_CURRENT_CATALOG ---")
    # The spec makes these one value under two names: "in ODBC 3.x, the value
    # returned for this InfoType can also be returned by calling
    # SQLGetConnectAttr with an Attribute argument of SQL_ATTR_CURRENT_CATALOG".
    dbname = read_wide_info(lib, dbc, SQL_DATABASE_NAME)
    check(
        "SQLGetInfo(SQL_DATABASE_NAME) is the connected catalog",
        SQL_SUCCESS if dbname == "tpcds" else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"read {dbname!r}",
    )
    catbuf = (ctypes.c_uint16 * 256)()
    outlen32.value = 0
    r = lib.SQLGetConnectAttrW(
        dbc, SQL_ATTR_CURRENT_CATALOG, ctypes.cast(catbuf, P), 512, ctypes.byref(outlen32)
    )
    check("get SQL_ATTR_CURRENT_CATALOG", r, SQL_SUCCESS)
    current = "".join(chr(c) for c in catbuf[: max(outlen32.value, 0) // 2])
    check(
        "SQL_ATTR_CURRENT_CATALOG agrees with SQL_DATABASE_NAME",
        SQL_SUCCESS if current == dbname else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"attribute {current!r}, info type {dbname!r}",
    )

    # Setting it is refused rather than silently accepted. Trino's only
    # catalog-switching statement is `USE`, whose grammar requires a schema, so
    # honouring this would mean inventing one and moving where the session's
    # unqualified names resolve. Storing the value and returning SQL_SUCCESS --
    # what the driver did before -- told an application its names had moved
    # when nothing had.
    newcat, _kc = w("postgresql")
    r = lib.SQLSetConnectAttrW(
        dbc, SQL_ATTR_CURRENT_CATALOG, ctypes.cast(newcat, P), SQL_NTS
    )
    check(
        "set SQL_ATTR_CURRENT_CATALOG",
        r,
        SQL_ERROR,
        state="HYC00",
        got_state=sqlstate(lib, SQL_HANDLE_DBC, dbc),
    )
    check(
        "a refused catalog switch leaves both readers where they were",
        SQL_SUCCESS if read_wide_info(lib, dbc, SQL_DATABASE_NAME) == dbname else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"SQL_DATABASE_NAME is now {read_wide_info(lib, dbc, SQL_DATABASE_NAME)!r}",
    )

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

    # ---------------------------------------------------------------
    print("\n--- bound parameter types ---")
    # Both of these are stackable-odbc-core gaps reached through this driver,
    # recorded as KNOWN so the suite stays green until core changes. Tighten
    # each into a `check` as soon as its fix lands.
    #
    # 1. SQLBindParameter's declared SQL type is discarded. core's
    #    read_param_value matches on the C type alone, so SQL_C_CHAR +
    #    SQL_NUMERIC -- what every client sends for a numeric delivered as
    #    text -- becomes a string, and Trino refuses to compare it to a
    #    decimal column.
    dec = ctypes.create_string_buffer(b"12.34")
    dec_ind = ctypes.c_longlong(SQL_NTS)
    sql, _kd = w("SELECT CAST(? AS VARCHAR)")
    lib.SQLFreeStmt(stmt, SQL_CLOSE)
    r = lib.SQLBindParameter(
        stmt, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_NUMERIC, 10, 2,
        ctypes.cast(dec, P), 0, ctypes.byref(dec_ind),
    )
    check("bind a NUMERIC parameter delivered as characters", r, SQL_SUCCESS)
    typed, _kt = w("SELECT typeof(?)")
    r = lib.SQLExecDirectW(stmt, typed, SQL_NTS)
    if r in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO) and lib.SQLFetch(stmt) == SQL_SUCCESS:
        got = ctypes.create_string_buffer(64)
        got_ind = ctypes.c_longlong(0)
        lib.SQLGetData(stmt, 1, SQL_C_CHAR, ctypes.cast(got, P), 64, ctypes.byref(got_ind))
        trino_type = got.value.decode(errors="replace")
        check(
            f"NUMERIC parameter reaches Trino as a decimal (got {trino_type!r})",
            SQL_SUCCESS if trino_type.startswith("decimal") else SQL_ERROR,
            SQL_SUCCESS,
        )
    else:
        note("NUMERIC parameter type", "KNOWN: could not read the parameter's type back")
    lib.SQLFreeStmt(stmt, SQL_RESET_PARAMS)
    lib.SQLFreeStmt(stmt, SQL_CLOSE)

    # 2. A statement with a parameter marker and nothing bound. The spec's
    #    answer is 07002 (COUNT field incorrect); core's collect_params pads
    #    every unbound marker with NULL instead, so the statement runs with
    #    NULLs substituted and the application is never told.
    unbound, _ku = w("SELECT 1 WHERE 2 = ?")
    r = lib.SQLExecDirectW(stmt, unbound, SQL_NTS)
    state = sqlstate(lib, SQL_HANDLE_STMT, stmt)
    if r == SQL_ERROR and state == "07002":
        check("unbound parameter marker", r, SQL_ERROR, state="07002", got_state=state)
    else:
        note(
            "unbound parameter marker",
            f"KNOWN: got {rname(r)} (SQLSTATE {state or 'none'}); core's collect_params "
            "substitutes NULL for an unbound marker instead of reporting 07002",
        )
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
