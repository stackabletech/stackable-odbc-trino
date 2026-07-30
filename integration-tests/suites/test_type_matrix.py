#!/usr/bin/env python3
"""
Type-transform fuzz for the Trino ODBC driver.

Drives every (Trino value, C data type) pair through `SQLGetData` on the raw C
ABI and checks the outcome against invariants rather than against a transcribed
copy of the ODBC conversion matrix. Transcribing the matrix would mostly test
the transcription; the invariants below are the properties whose violation is
an actual defect, and they hold for every cell of it.

    1. The call returns. No pair may crash, abort or hang the process.
    2. A failure carries a SQLSTATE. `SQL_ERROR` with no diagnostic record
       leaves an application with an error it cannot interpret.
    3. NULL is reported as NULL. `SQL_NULL_DATA` in the indicator, for every
       target type, whatever the source type is.
    4. A value that does not fit reports 22003, not a truncated number.
    5. Text that is not a number reports 22018, not a zero.
    6. A successful conversion round-trips. Where the value is checkable as
       text, what comes back is what went in.

Covers the paths the previous session left unfuzzed: the full type-transform
matrix, NULL and the IEEE specials per type, integer boundary values, and
trailing semicolons.

Usage:
    python3 integration-tests/suites/test_type_matrix.py [path/to/driver.so] [conn-str]

Requires a running Trino (integration-tests/setup.sh). Standard library only -- ctypes, no
pyodbc and no uv, so its output survives being redirected to a file.
"""

import ctypes
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Results, Stack  # noqa: E402
from test_c_abi import (  # noqa: E402
    SQL_ATTR_ODBC_VERSION,
    SQL_DRIVER_NOPROMPT,
    SQL_ERROR,
    SQL_HANDLE_DBC,
    SQL_HANDLE_ENV,
    SQL_HANDLE_STMT,
    SQL_NTS,
    SQL_OV_ODBC3,
    SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO,
    load,
    sqlstate,
    w,
)

P = ctypes.c_void_p

# C data types, from odbc_sys::CDataType.
C_CHAR = 1
C_WCHAR = -8
C_BIT = -7
C_STINYINT = -26
C_SSHORT = -15
C_SLONG = -16
C_SBIGINT = -25
C_FLOAT = 7
C_DOUBLE = 8
C_BINARY = -2
C_TYPE_DATE = 91
C_TYPE_TIME = 92
C_TYPE_TIMESTAMP = 93

C_TYPES = [
    ("SQL_C_CHAR", C_CHAR),
    ("SQL_C_WCHAR", C_WCHAR),
    ("SQL_C_BIT", C_BIT),
    ("SQL_C_STINYINT", C_STINYINT),
    ("SQL_C_SSHORT", C_SSHORT),
    ("SQL_C_SLONG", C_SLONG),
    ("SQL_C_SBIGINT", C_SBIGINT),
    ("SQL_C_FLOAT", C_FLOAT),
    ("SQL_C_DOUBLE", C_DOUBLE),
    ("SQL_C_BINARY", C_BINARY),
    ("SQL_C_TYPE_DATE", C_TYPE_DATE),
    ("SQL_C_TYPE_TIME", C_TYPE_TIME),
    ("SQL_C_TYPE_TIMESTAMP", C_TYPE_TIMESTAMP),
]

SQL_NULL_DATA = -1

# Spec SQLSTATEs this fuzz reasons about.
STATE_OUT_OF_RANGE = "22003"  # Numeric value out of range
STATE_BAD_CAST = "22018"  # Invalid character value for cast
STATE_TRUNCATED = "01004"  # String data, right truncated
STATE_RESTRICTED = "07006"  # Restricted data type attribute violation

# (label, Trino expression, expected text when read as SQL_C_CHAR or None)
#
# Boundary values are the exact limits of each Trino integer type, because an
# off-by-one in a narrowing conversion shows up nowhere else.
VALUES = [
    # "1"/"0", not "true"/"false": a Trino BOOLEAN is described as SQL_BIT
    # (verified through SQLDescribeCol), and the ODBC conversion matrix renders
    # SQL_BIT as the character "1" or "0". Reading the Trino spelling back would
    # mean the driver was not honouring the type it advertises.
    ("boolean true", "CAST(true AS BOOLEAN)", "1"),
    ("boolean false", "CAST(false AS BOOLEAN)", "0"),
    ("tinyint min", "CAST(-128 AS TINYINT)", "-128"),
    ("tinyint max", "CAST(127 AS TINYINT)", "127"),
    ("smallint min", "CAST(-32768 AS SMALLINT)", "-32768"),
    ("smallint max", "CAST(32767 AS SMALLINT)", "32767"),
    ("integer min", "CAST(-2147483648 AS INTEGER)", "-2147483648"),
    ("integer max", "CAST(2147483647 AS INTEGER)", "2147483647"),
    ("bigint min", "CAST(-9223372036854775808 AS BIGINT)", "-9223372036854775808"),
    ("bigint max", "CAST(9223372036854775807 AS BIGINT)", "9223372036854775807"),
    ("bigint zero", "CAST(0 AS BIGINT)", "0"),
    ("real", "CAST(1.5 AS REAL)", None),
    ("real nan", "CAST(nan() AS REAL)", None),
    ("real inf", "CAST(infinity() AS REAL)", None),
    ("real -inf", "CAST(-infinity() AS REAL)", None),
    ("double", "CAST(1.5 AS DOUBLE)", None),
    ("double nan", "CAST(nan() AS DOUBLE)", None),
    ("double inf", "CAST(infinity() AS DOUBLE)", None),
    ("double -inf", "CAST(-infinity() AS DOUBLE)", None),
    ("decimal", "CAST(123.45 AS DECIMAL(10,2))", "123.45"),
    ("decimal negative", "CAST(-123.45 AS DECIMAL(10,2))", "-123.45"),
    ("varchar text", "CAST('hello' AS VARCHAR)", "hello"),
    ("varchar numeric text", "CAST('42' AS VARCHAR)", "42"),
    ("varchar empty", "CAST('' AS VARCHAR)", ""),
    ("varchar overflowing bigint", "CAST('99999999999999999999' AS VARCHAR)", None),
    ("char(5)", "CAST('ab' AS CHAR(5))", None),
    ("varbinary", "CAST('a' AS VARBINARY)", None),
    ("date", "CAST('2020-02-03' AS DATE)", "2020-02-03"),
    ("time", "CAST('04:05:06' AS TIME)", None),
    ("timestamp", "CAST('2020-02-03 04:05:06' AS TIMESTAMP)", None),
    ("timestamp tz", "CAST('2020-02-03 04:05:06 UTC' AS TIMESTAMP WITH TIME ZONE)", None),
    ("uuid", "CAST('12151fd2-7586-11e9-8f9e-2a86e4085a59' AS UUID)", None),
    ("json", "CAST('{\"a\":1}' AS JSON)", None),
    ("interval day", "INTERVAL '2' DAY", None),
    ("interval year", "INTERVAL '2' YEAR", None),
    ("array", "ARRAY[1,2,3]", None),
    ("row", "CAST(ROW(1,'a') AS ROW(x INTEGER, y VARCHAR))", None),
]

# Every value above, as its own NULL. NULL must be reported as NULL for every
# target type -- a driver that reports a NULL as 0 or "" corrupts data silently.
NULL_VALUES = [
    ("null boolean", "CAST(NULL AS BOOLEAN)"),
    ("null tinyint", "CAST(NULL AS TINYINT)"),
    ("null integer", "CAST(NULL AS INTEGER)"),
    ("null bigint", "CAST(NULL AS BIGINT)"),
    ("null double", "CAST(NULL AS DOUBLE)"),
    ("null real", "CAST(NULL AS REAL)"),
    ("null decimal", "CAST(NULL AS DECIMAL(10,2))"),
    ("null varchar", "CAST(NULL AS VARCHAR)"),
    ("null varbinary", "CAST(NULL AS VARBINARY)"),
    ("null date", "CAST(NULL AS DATE)"),
    ("null time", "CAST(NULL AS TIME)"),
    ("null timestamp", "CAST(NULL AS TIMESTAMP)"),
    ("null uuid", "CAST(NULL AS UUID)"),
    ("null json", "CAST(NULL AS JSON)"),
]

# Statements whose terminator or comment placement the parser has to survive.
TERMINATORS = [
    ("plain", "SELECT 1 AS n"),
    ("one semicolon", "SELECT 1 AS n;"),
    ("semicolon and spaces", "SELECT 1 AS n ;   "),
    ("two semicolons", "SELECT 1 AS n;;"),
    ("semicolon and newline", "SELECT 1 AS n;\n"),
    ("semicolon in a literal", "SELECT ';' AS n"),
    ("semicolon in a literal, then terminator", "SELECT ';' AS n;"),
    ("semicolon in a quoted identifier", 'SELECT 1 AS "a;b"'),
    ("semicolon inside a line comment", "SELECT 1 AS n -- ;"),
    ("semicolon inside a block comment", "SELECT 1 AS n /* ; */"),
]

R = Results("type matrix")
violations = []


# These write R's counters directly rather than going through ok()/bad():
# the suite prints per *violation*, not per check. 37 values against 13 C
# types is 481 PASS lines nobody reads.
def fail(kind, detail):
    R.failed += 1
    violations.append(f"{kind}: {detail}")


def ok():
    R.passed += 1


class Driver:
    def __init__(self, so, conn_str):
        self.lib = load(so)
        self.env = P()
        self.lib.SQLAllocHandle(SQL_HANDLE_ENV, None, ctypes.byref(self.env))
        self.lib.SQLSetEnvAttr(self.env, SQL_ATTR_ODBC_VERSION, P(SQL_OV_ODBC3), 0)
        self.dbc = P()
        self.lib.SQLAllocHandle(SQL_HANDLE_DBC, self.env, ctypes.byref(self.dbc))
        cs, self._keep = w(conn_str)
        ob = (ctypes.c_uint16 * 1024)()
        ol = ctypes.c_int16(0)
        r = self.lib.SQLDriverConnectW(
            self.dbc, None, cs, SQL_NTS,
            ctypes.cast(ob, ctypes.POINTER(ctypes.c_uint16)), 1024,
            ctypes.byref(ol), SQL_DRIVER_NOPROMPT,
        )
        if r not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
            raise SystemExit("could not connect; is Trino running? (integration-tests/setup.sh)")

    def fetch_as(self, expr, c_type):
        """Run `SELECT <expr>` and read column 1 as `c_type`.

        Returns (ret, sqlstate, indicator, raw_bytes).
        """
        lib = self.lib
        stmt = P()
        lib.SQLAllocHandle(SQL_HANDLE_STMT, self.dbc, ctypes.byref(stmt))
        try:
            sql, _k = w(f"SELECT {expr}")
            r = lib.SQLExecDirectW(stmt, sql, SQL_NTS)
            if r not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
                return ("EXEC", sqlstate(lib, SQL_HANDLE_STMT, stmt), None, None)
            r = lib.SQLFetch(stmt)
            if r not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
                return ("FETCH", sqlstate(lib, SQL_HANDLE_STMT, stmt), None, None)
            buf = ctypes.create_string_buffer(512)
            ind = ctypes.c_int64(0)
            r = lib.SQLGetData(
                stmt, 1, c_type, ctypes.cast(buf, P), 512, ctypes.byref(ind)
            )
            return (r, sqlstate(lib, SQL_HANDLE_STMT, stmt), ind.value, buf.raw)
        finally:
            lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)

    def close(self):
        self.lib.SQLDisconnect(self.dbc)
        self.lib.SQLFreeHandle(SQL_HANDLE_DBC, self.dbc)
        self.lib.SQLFreeHandle(SQL_HANDLE_ENV, self.env)


def i32_max_as_u64():
    """The largest column size a driver may honestly report as a number.

    `i32::MAX` is the established "unbounded but reportable" convention;
    anything above it means a signed sentinel was written into an unsigned
    out-parameter and wrapped.
    """
    return 2**31 - 1


def as_text(raw, c_type):
    if c_type == C_WCHAR:
        u = ctypes.cast(raw, ctypes.POINTER(ctypes.c_uint16))
        out = []
        for i in range(256):
            if u[i] == 0:
                break
            out.append(chr(u[i]))
        return "".join(out)
    return raw.split(b"\x00", 1)[0].decode("utf-8", "replace")


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    so = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        here, "..", "..", "target", "debug", "libstackable_odbc_trino.so"
    )
    conn_str = sys.argv[2] if len(sys.argv) > 2 else Stack.load().conn_str()
    if not os.path.exists(so):
        print(f"driver not found: {so}\nrun: cargo build")
        return 2

    d = Driver(so, conn_str)
    print("=== type-transform fuzz ===\n")

    # -- invariants 1, 2 and 6 over the full matrix -------------------
    print(f"--- {len(VALUES)} values x {len(C_TYPES)} C types ---")
    for label, expr, want_text in VALUES:
        for cname, ctype in C_TYPES:
            ret, state, ind, raw = d.fetch_as(expr, ctype)
            cell = f"{label} -> {cname}"

            if ret in ("EXEC", "FETCH"):
                fail("query failed", f"{cell}: {ret} {state}")
                continue

            # Invariant 2: a failure must carry a SQLSTATE.
            if ret == SQL_ERROR and not state:
                fail("error with no SQLSTATE", cell)
                continue

            if ret in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
                # Invariant 6: a successful text conversion round-trips.
                if want_text is not None and ctype in (C_CHAR, C_WCHAR):
                    got = as_text(raw, ctype)
                    if got != want_text:
                        fail(
                            "round-trip mismatch",
                            f"{cell}: sent {want_text!r}, read {got!r}",
                        )
                        continue
            ok()

    # -- invariant 4: overflow is 22003, not a truncated number -------
    print("\n--- integer overflow must report 22003 ---")
    OVERFLOW = [
        ("bigint max -> SQL_C_SLONG", "CAST(9223372036854775807 AS BIGINT)", C_SLONG),
        ("bigint max -> SQL_C_SSHORT", "CAST(9223372036854775807 AS BIGINT)", C_SSHORT),
        ("bigint max -> SQL_C_STINYINT", "CAST(9223372036854775807 AS BIGINT)", C_STINYINT),
        ("integer max -> SQL_C_SSHORT", "CAST(2147483647 AS INTEGER)", C_SSHORT),
        ("integer max -> SQL_C_STINYINT", "CAST(2147483647 AS INTEGER)", C_STINYINT),
        ("smallint max -> SQL_C_STINYINT", "CAST(32767 AS SMALLINT)", C_STINYINT),
        ("bigint min -> SQL_C_SLONG", "CAST(-9223372036854775808 AS BIGINT)", C_SLONG),
    ]
    for cell, expr, ctype in OVERFLOW:
        ret, state, ind, raw = d.fetch_as(expr, ctype)
        if ret in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
            fail(
                "silent overflow",
                f"{cell}: succeeded where the value cannot fit; expected {STATE_OUT_OF_RANGE}",
            )
        elif state != STATE_OUT_OF_RANGE:
            fail("wrong overflow SQLSTATE", f"{cell}: {state or '<none>'} "
                 f"(expected {STATE_OUT_OF_RANGE})")
        else:
            ok()

    # -- invariant 5: non-numeric text is 22018, not zero -------------
    print("\n--- non-numeric text must report 22018 ---")
    for cname, ctype in [
        ("SQL_C_SLONG", C_SLONG),
        ("SQL_C_SBIGINT", C_SBIGINT),
        ("SQL_C_DOUBLE", C_DOUBLE),
        ("SQL_C_SSHORT", C_SSHORT),
    ]:
        cell = f"varchar 'abc' -> {cname}"
        ret, state, ind, raw = d.fetch_as("CAST('abc' AS VARCHAR)", ctype)
        if ret in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
            fail("silent bad cast", f"{cell}: succeeded; expected {STATE_BAD_CAST}")
        elif state != STATE_BAD_CAST:
            fail("wrong bad-cast SQLSTATE",
                 f"{cell}: {state or '<none>'} (expected {STATE_BAD_CAST})")
        else:
            ok()

    # -- invariant 3: NULL is NULL for every target type --------------
    print(f"\n--- {len(NULL_VALUES)} NULLs x {len(C_TYPES)} C types ---")
    for label, expr in NULL_VALUES:
        for cname, ctype in C_TYPES:
            cell = f"{label} -> {cname}"
            ret, state, ind, raw = d.fetch_as(expr, ctype)
            if ret in ("EXEC", "FETCH"):
                fail("query failed", f"{cell}: {ret} {state}")
            elif ret in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
                if ind != SQL_NULL_DATA:
                    fail(
                        "NULL not reported as NULL",
                        f"{cell}: indicator {ind}, expected SQL_NULL_DATA ({SQL_NULL_DATA})",
                    )
                else:
                    ok()
            elif not state:
                fail("error with no SQLSTATE", cell)
            else:
                # Refusing the conversion outright is legitimate; reporting the
                # NULL as data is not, and that is what the branch above checks.
                ok()

    # -- the IEEE specials as text ------------------------------------
    # Their spelling is Trino's and Java's, not Rust's Display: an application
    # reading a DOUBLE as text must not see `inf` where every other Trino client
    # says `Infinity`, and a value that round-trips through two clients should
    # not change spelling on the way.
    print("\n--- IEEE specials render with Trino's spelling ---")
    for expr, want in [
        ("CAST(infinity() AS DOUBLE)", "Infinity"),
        ("CAST(-infinity() AS DOUBLE)", "-Infinity"),
        ("CAST(nan() AS DOUBLE)", "NaN"),
        ("CAST(infinity() AS REAL)", "Infinity"),
    ]:
        ret, state, ind, raw = d.fetch_as(expr, C_CHAR)
        if ret not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
            fail("special not readable as text", f"{expr}: {state or ret}")
            continue
        got = as_text(raw, C_CHAR)
        if got != want:
            fail("wrong spelling", f"{expr}: read {got!r}, expected {want!r}")
        else:
            ok()

    # -- an undeterminable column size is 0, not a wrapped -1 ----------
    # SQLDescribeCol's ColumnSizePtr is a SQLULEN, and the spec's answer for a
    # size the driver cannot determine is 0. Reporting SQL_NO_TOTAL there
    # instead surfaced as 18,446,744,073,709,551,612, which an application
    # sizing a buffer from would try to allocate.
    print("\n--- undeterminable column sizes report 0 ---")
    lib = d.lib
    lib.SQLDescribeColW.argtypes = [
        P, ctypes.c_uint16, ctypes.POINTER(ctypes.c_uint16), ctypes.c_int16,
        ctypes.POINTER(ctypes.c_int16), ctypes.POINTER(ctypes.c_int16),
        ctypes.POINTER(ctypes.c_uint64), ctypes.POINTER(ctypes.c_int16),
        ctypes.POINTER(ctypes.c_int16),
    ]
    lib.SQLDescribeColW.restype = ctypes.c_int16
    for stmt_sql in ("DESCRIBE tpcds.sf1.customer", "SHOW TABLES FROM tpcds.sf1"):
        stmt = P(); lib.SQLAllocHandle(SQL_HANDLE_STMT, d.dbc, ctypes.byref(stmt))
        wsql, _k = w(stmt_sql)
        if lib.SQLExecDirectW(stmt, wsql, SQL_NTS) in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
            nm = (ctypes.c_uint16 * 64)(); nl = ctypes.c_int16(0); dt = ctypes.c_int16(0)
            sz = ctypes.c_uint64(0); dd = ctypes.c_int16(0); nu = ctypes.c_int16(0)
            lib.SQLDescribeColW(
                stmt, 1, ctypes.cast(nm, ctypes.POINTER(ctypes.c_uint16)), 64,
                ctypes.byref(nl), ctypes.byref(dt), ctypes.byref(sz),
                ctypes.byref(dd), ctypes.byref(nu),
            )
            if sz.value > i32_max_as_u64():
                fail(
                    "absurd column size",
                    f"{stmt_sql}: SQLDescribeCol reported {sz.value:,}",
                )
            else:
                ok()
        else:
            fail("query failed", stmt_sql)
        lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)

    # -- trailing semicolons and comment placement --------------------
    print(f"\n--- {len(TERMINATORS)} statement terminator forms ---")
    for label, sql in TERMINATORS:
        stmt = P()
        d.lib.SQLAllocHandle(SQL_HANDLE_STMT, d.dbc, ctypes.byref(stmt))
        wsql, _k = w(sql)
        ret = d.lib.SQLExecDirectW(stmt, wsql, SQL_NTS)
        state = sqlstate(d.lib, SQL_HANDLE_STMT, stmt)
        if ret in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
            ok()
        else:
            fail("terminator rejected", f"{label}: {sql!r} -> {state or '<none>'}")
        d.lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)

    d.close()

    if violations:
        print("\nviolations:")
        seen = set()
        for v in violations:
            if v not in seen:
                seen.add(v)
                print(f"  {v}")
    return R.summary()


if __name__ == "__main__":
    sys.exit(main())
