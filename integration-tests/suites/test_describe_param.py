#!/usr/bin/env python3
"""`SQLDescribeParam`, against Trino's own `DESCRIBE INPUT`.

Nothing exercised this against a coordinator. `describe_param.rs` was covered
only by unit tests over its row-to-descriptor conversion, so everything around
that conversion -- the `PREPARE` / `DESCRIBE INPUT` / `DEALLOCATE` round trip,
the per-connection cache, the fixed session-wide statement name, and what
happens when any of it fails -- was unverified.

ctypes rather than pyodbc, because pyodbc exposes no `SQLDescribeParam`. There
is no Driver Manager in the loop either, so what is asserted is the driver's
own answer.

What the call is *for* is the point of the first section. Core's fallback is a
uniform `VARCHAR(SQL_DEFAULT_PARAM_SIZE)` for every parameter, which makes a
client send a number as text and get a type error back. A driver that answered
the fallback for everything would pass any test that only checked for success,
so each probe asserts the specific type Trino inferred.

Usage:
    python3 integration-tests/suites/test_describe_param.py [path/to/lib...so] [conn-str]

Requires a running Trino (integration-tests/setup.sh). No compose profile: the
tpcds and hive catalogs are both in the base stack. Standard library only.
"""

import ctypes
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Results, Stack  # noqa: E402
from odbc_abi import (  # noqa: E402
    SQL_DRIVER_NOPROMPT,
    SQL_HANDLE_DBC,
    SQL_HANDLE_ENV,
    SQL_HANDLE_STMT,
    SQL_NTS,
    SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO,
    SQL_ATTR_ODBC_VERSION,
    SQL_OV_ODBC3,
    load,
    sqlstate,
    w,
)

R = Results("SQLDescribeParam")

# Concise SQL types, from the ODBC spec's own table. Named rather than inlined,
# per the project's rule about spec values.
SQL_CHAR = 1
SQL_INTEGER = 4
SQL_VARCHAR = 12
SQL_DECIMAL = 3
SQL_BIGINT = -5
SQL_WCHAR = -8
SQL_WVARCHAR = -9
SQL_TYPE_DATE = 91

TYPE_NAMES = {
    SQL_CHAR: "SQL_CHAR",
    SQL_INTEGER: "SQL_INTEGER",
    SQL_VARCHAR: "SQL_VARCHAR",
    SQL_DECIMAL: "SQL_DECIMAL",
    SQL_BIGINT: "SQL_BIGINT",
    SQL_WCHAR: "SQL_WCHAR",
    SQL_WVARCHAR: "SQL_WVARCHAR",
    SQL_TYPE_DATE: "SQL_TYPE_DATE",
}

# Connection attribute, for the transaction probe at the end.
SQL_ATTR_AUTOCOMMIT = 102
SQL_AUTOCOMMIT_OFF = 0

# The types core would answer with if the DESCRIBE INPUT round trip did not
# happen. A probe returning this has not proved anything, so the type probes
# assert against it explicitly rather than only against the expected value.
CORE_FALLBACK_TYPES = (SQL_VARCHAR, SQL_WVARCHAR)


def type_name(code):
    return TYPE_NAMES.get(code, str(code))


def describe(lib, stmt, n):
    """`SQLDescribeParam` for parameter `n`, as (ret, type, size, digits, nullable)."""
    dtype = ctypes.c_int16(0)
    size = ctypes.c_uint64(0)
    digits = ctypes.c_int16(0)
    nullable = ctypes.c_int16(0)
    ret = lib.SQLDescribeParam(
        stmt, n,
        ctypes.byref(dtype), ctypes.byref(size),
        ctypes.byref(digits), ctypes.byref(nullable),
    )
    return ret, dtype.value, size.value, digits.value, nullable.value


def prepare(lib, dbc, sql):
    """A fresh statement handle with `sql` prepared on it."""
    stmt = ctypes.c_void_p()
    if lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(stmt)) != SQL_SUCCESS:
        return None
    text, _keep = w(sql)
    ret = lib.SQLPrepareW(stmt, text, SQL_NTS)
    if ret not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
        lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)
        return None
    # `_keep` must outlive the call, and it does: SQLPrepareW copies the text.
    return stmt


def probe_types(lib, dbc, label, sql, expected):
    """Prepare `sql` and assert each parameter's described type.

    `expected` is a list of (sql_type, minimum_size) per parameter, in order.
    """
    stmt = prepare(lib, dbc, sql)
    if stmt is None:
        R.bad(label, "the statement could not be prepared")
        return

    count = ctypes.c_int16(0)
    lib.SQLNumParams(stmt, ctypes.byref(count))
    R.check(
        f"{label}: SQLNumParams",
        count.value == len(expected),
        "" if count.value == len(expected) else f"  expected {len(expected)}, got {count.value}",
    )

    for i, (want_type, want_min_size) in enumerate(expected, start=1):
        ret, dtype, size, digits, _nullable = describe(lib, stmt, i)
        if ret not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
            R.bad(
                f"{label}: parameter {i}",
                f"SQLDescribeParam failed, {sqlstate(lib, SQL_HANDLE_STMT, stmt)}",
            )
            continue
        ok = dtype == want_type
        R.check(
            f"{label}: parameter {i} is {type_name(want_type)}",
            ok,
            "" if ok else f"  got {type_name(dtype)} (size {size}, digits {digits})",
        )
        # The whole point of the round trip: core's fallback is one uniform
        # character type, so a driver answering that has described nothing.
        if want_type not in CORE_FALLBACK_TYPES:
            R.check(
                f"{label}: parameter {i} is not core's generic fallback",
                dtype not in CORE_FALLBACK_TYPES,
                "" if dtype not in CORE_FALLBACK_TYPES
                else "  DESCRIBE INPUT did not happen, or its answer was discarded",
            )
        if want_min_size is not None:
            R.check(
                f"{label}: parameter {i} carries a size",
                size >= want_min_size,
                "" if size >= want_min_size else f"  expected >= {want_min_size}, got {size}",
            )

    lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)


def main():
    stack = Stack.load()
    driver = sys.argv[1] if len(sys.argv) > 1 else stack.get("DRIVER_PATH")
    conn_str = sys.argv[2] if len(sys.argv) > 2 else stack.conn_str()

    lib = load(driver)
    env = ctypes.c_void_p()
    lib.SQLAllocHandle(SQL_HANDLE_ENV, None, ctypes.byref(env))
    lib.SQLSetEnvAttr(env, SQL_ATTR_ODBC_VERSION, ctypes.c_void_p(SQL_OV_ODBC3), 0)
    dbc = ctypes.c_void_p()
    lib.SQLAllocHandle(SQL_HANDLE_DBC, env, ctypes.byref(dbc))

    cs, _keep = w(conn_str)
    outbuf = (ctypes.c_uint16 * 1024)()
    outlen = ctypes.c_int16(0)
    ret = lib.SQLDriverConnectW(
        dbc, None, cs, SQL_NTS,
        ctypes.cast(outbuf, ctypes.POINTER(ctypes.c_uint16)), 1024,
        ctypes.byref(outlen), SQL_DRIVER_NOPROMPT,
    )
    if ret not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
        print(f"cannot connect ({sqlstate(lib, SQL_HANDLE_DBC, dbc)}); is Trino running?")
        return 1

    print("=== SQLDescribeParam ===\n")

    # ------------------------------------------------------------------
    print("--- the type Trino inferred, not core's uniform guess ---")
    # tpcds.sf1.customer types, read from the catalog: c_customer_sk is bigint,
    # c_customer_id is char(16), c_birth_year is integer.
    probe_types(
        lib, dbc, "three columns of different types",
        "SELECT 1 FROM tpcds.sf1.customer "
        "WHERE c_customer_sk = ? AND c_customer_id = ? AND c_birth_year = ?",
        [(SQL_BIGINT, None), (SQL_WCHAR, 16), (SQL_INTEGER, None)],
    )
    # The case the CHANGELOG names: "a filter on a `decimal` column keeps its
    # type". i_current_price is decimal(7,2).
    probe_types(
        lib, dbc, "a decimal column",
        "SELECT 1 FROM tpcds.sf1.item WHERE i_current_price = ?",
        [(SQL_DECIMAL, 7)],
    )
    probe_types(
        lib, dbc, "a date column",
        "SELECT 1 FROM tpcds.sf1.date_dim WHERE d_date = ?",
        [(SQL_TYPE_DATE, None)],
    )

    # ------------------------------------------------------------------
    print("\n--- the trailing statement terminator ---")
    # Trino's grammar has no terminator, so `PREPARE x FROM SELECT ... ;` is a
    # syntax error. `exec_direct` has always stripped it; `describe_param` wraps
    # the same SQL in a PREPARE and did not, so a statement an application could
    # prepare and run was one this could not describe.
    probe_types(
        lib, dbc, "a statement ending in a semicolon",
        "SELECT 1 FROM tpcds.sf1.customer WHERE c_customer_sk = ? ;",
        [(SQL_BIGINT, None)],
    )

    # ------------------------------------------------------------------
    print("\n--- the per-connection cache ---")
    # `Backend::describe_param` receives no statement handle, so core calls it
    # once per parameter. Without the cache a ten-parameter statement would cost
    # ten round trips. Asserted by describing the same statement's parameters
    # repeatedly and requiring a stable answer: a cache keyed on the wrong thing
    # would answer parameter 1's type for parameter 2.
    stmt = prepare(
        lib, dbc,
        "SELECT 1 FROM tpcds.sf1.customer WHERE c_customer_sk = ? AND c_birth_year = ?",
    )
    if stmt is None:
        R.bad("cache: prepare", "the statement could not be prepared")
    else:
        seen = []
        for _ in range(3):
            for i in (1, 2):
                seen.append(describe(lib, stmt, i)[1])
        R.check(
            "repeated describes answer the same types in the same order",
            seen == [SQL_BIGINT, SQL_INTEGER] * 3,
            "" if seen == [SQL_BIGINT, SQL_INTEGER] * 3
            else f"  got {[type_name(t) for t in seen]}",
        )
        lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)

    # A different statement on the same connection must not be served the first
    # one's answer: the cache is keyed on the SQL text.
    probe_types(
        lib, dbc, "a second statement is not served the first one's cache",
        "SELECT 1 FROM tpcds.sf1.item WHERE i_current_price = ?",
        [(SQL_DECIMAL, 7)],
    )

    # ------------------------------------------------------------------
    print("\n--- a statement Trino cannot prepare ---")
    # Outside a transaction this degrades rather than failing: Trino declines to
    # prepare plenty of legitimate statements, and core's fallback is usable for
    # a call that only sizes a buffer.
    stmt = prepare(lib, dbc, "SELECT 1 FROM no_such_catalog.s.t WHERE x = ?")
    if stmt is None:
        R.note("an unpreparable statement", "SQLPrepare itself refused it, so there is nothing to describe")
    else:
        ret, dtype, _size, _digits, _nullable = describe(lib, stmt, 1)
        R.check(
            "an unpreparable statement falls back rather than failing",
            ret in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO),
            "" if ret in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO)
            else f"  {sqlstate(lib, SQL_HANDLE_STMT, stmt)}",
        )
        if ret in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
            R.check(
                "and the fallback is core's uniform character type",
                dtype in CORE_FALLBACK_TYPES,
                "" if dtype in CORE_FALLBACK_TYPES else f"  got {type_name(dtype)}",
            )
        lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)

    # ------------------------------------------------------------------
    print("\n--- inside a transaction, the same failure is reported ---")
    # Trino carries the transaction id in a session header, so the driver's own
    # PREPARE joins whatever the application has open, and a statement error
    # aborts the whole transaction. Falling back silently there would report
    # success from SQLDescribeParam while the application's transaction had just
    # been killed by a round trip it did not make and cannot see.
    lib.SQLSetConnectAttrW(
        dbc, SQL_ATTR_AUTOCOMMIT, ctypes.c_void_p(SQL_AUTOCOMMIT_OFF), 0
    )
    # Open the transaction with a statement that works.
    opener = ctypes.c_void_p()
    lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(opener))
    sql, _k = w("SELECT 1")
    lib.SQLExecDirectW(opener, sql, SQL_NTS)
    lib.SQLFreeHandle(SQL_HANDLE_STMT, opener)

    stmt = prepare(lib, dbc, "SELECT 1 FROM no_such_catalog.s.t WHERE x = ?")
    if stmt is None:
        R.note("unpreparable inside a transaction", "SQLPrepare itself refused it")
    else:
        ret, _dtype, _size, _digits, _nullable = describe(lib, stmt, 1)
        R.check(
            "an unpreparable statement inside a transaction reports the failure",
            ret not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO),
            "" if ret not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO)
            else "  reported success, so the aborted transaction went unmentioned",
        )
        lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)

    lib.SQLEndTran(SQL_HANDLE_DBC, dbc, 1)  # SQL_ROLLBACK, to free the session

    lib.SQLDisconnect(dbc)
    lib.SQLFreeHandle(SQL_HANDLE_DBC, dbc)
    lib.SQLFreeHandle(SQL_HANDLE_ENV, env)
    return R.summary()


if __name__ == "__main__":
    sys.exit(main())
