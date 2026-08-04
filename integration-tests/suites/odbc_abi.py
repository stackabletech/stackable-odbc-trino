#!/usr/bin/env python3
"""The raw ODBC C ABI, for the suites that call it without a Driver Manager.

`test_c_abi.py` and `test_oauth.py` both load a shared object with ctypes and
call the exported entry points directly. This module is the plumbing they share:
the wide-string helper, the signature declarations, and the diagnostic readers.

`load` takes a path, so it serves the driver's own `.so` and unixODBC's
`libodbc.so.2` equally. `SQLRETURN` is a 16-bit `SQLSMALLINT`, and an undeclared
function leaves ctypes reading the return register as a 32-bit int, where
`SQL_ERROR` arrives as 65535 and every comparison against -1 silently fails.
That is why every entry point either suite calls is declared here.
"""

import ctypes

# --- handle types ---
SQL_HANDLE_ENV = 1
SQL_HANDLE_DBC = 2
SQL_HANDLE_STMT = 3
SQL_HANDLE_DESC = 4

# --- return codes ---
SQL_SUCCESS = 0
SQL_SUCCESS_WITH_INFO = 1
SQL_NO_DATA = 100
SQL_ERROR = -1
SQL_INVALID_HANDLE = -2

SQL_NTS = -3
SQL_NULL_HANDLE = None

SQL_ATTR_ODBC_VERSION = 200
SQL_OV_ODBC3 = 3

# --- SQLDriverConnect DriverCompletion ---
# Only NOPROMPT forbids the driver from prompting; the other three permit it.
# pyodbc passes NOPROMPT unconditionally, which is why an interactive
# authentication test cannot go through pyodbc at all.
SQL_DRIVER_NOPROMPT = 0
SQL_DRIVER_COMPLETE = 1
SQL_DRIVER_PROMPT = 2
SQL_DRIVER_COMPLETE_REQUIRED = 3


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
        # ParameterSizePtr is a SQLULEN, which is 64-bit on this platform;
        # declaring it 32-bit would read half of it and half of the next slot.
        # pyodbc exposes no equivalent, so ctypes is the only way to reach it.
        "SQLDescribeParam": (
            [
                P,
                ctypes.c_uint16,
                ctypes.POINTER(S),
                ctypes.POINTER(ctypes.c_uint64),
                ctypes.POINTER(S),
                ctypes.POINTER(S),
            ],
            S,
        ),
        "SQLBindParameter": (
            [P, ctypes.c_uint16, S, S, S, ctypes.c_size_t, S, P, L, ctypes.POINTER(L)],
            S,
        ),
        # SQLRETURN is a 16-bit SQLSMALLINT: an undeclared function leaves
        # ctypes reading a 32-bit register, where SQL_ERROR arrives as 65535.
        "SQLEndTran": ([S, P, S], S),
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


def _diag_record(lib, htype, handle):
    """Diagnostic record 1 as (sqlstate, message), or ('', '') when absent."""
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
        return "", ""
    return (
        "".join(chr(c) for c in state if c).strip(),
        "".join(chr(c) for c in msg[: max(textlen.value, 0)]),
    )


def sqlstate(lib, htype, handle):
    """The SQLSTATE of diagnostic record 1, or '' when there is none."""
    return _diag_record(lib, htype, handle)[0]


def diag_message(lib, htype, handle):
    """The message text of diagnostic record 1, or '' when there is none.

    A SQLSTATE alone is not enough to diagnose a failed connect: the SQLSTATE
    says what kind of failure it was, and the message says which of several
    connection-string problems produced it.
    """
    return _diag_record(lib, htype, handle)[1]


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
