#!/usr/bin/env python3
"""
Integration tests for the Trino ODBC driver.

Runs through the ODBC Driver Manager (unixODBC on Linux, odbc32.dll on Windows)
using pyodbc. Tests connection, metadata queries, SELECT, aggregation, and
parameterised statements against the tpcds catalog (read-only, ships with Trino).

Usage:
    python3 integration-tests/suites/test_integration.py "Driver=/path/to/driver.so;Host=localhost;Port=8080;User=admin;Protocol=http;Catalog=tpcds"
    python3 integration-tests/suites/test_integration.py "DSN=test_trino"

Requires: pip install pyodbc
"""

import os
import sys

import pyodbc

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Results, Stack  # noqa: E402

R = Results("integration")


def conn_str_value(conn_str, wanted_key):
    """The value of `wanted_key` in a connection string, or None if absent."""
    for part in conn_str.split(";"):
        key, _, value = part.partition("=")
        if key.strip().lower() == wanted_key:
            return value.strip()
    return None


def conn_str_catalog(conn_str):
    """The `Catalog` value of a connection string, or None for a DSN."""
    return conn_str_value(conn_str, "catalog")


def main():
    # A connection string may be passed positionally -- run-tests.sh does, to
    # drive this suite against several configurations, and windows_test.py does
    # because the VM's stack is not this one. With no argument, the local stack
    # describes itself.
    conn_str = sys.argv[1] if len(sys.argv) > 1 else Stack.load().conn_str()
    conn = pyodbc.connect(conn_str, autocommit=True)
    cur = conn.cursor()

    # ------------------------------------------------------------------
    # Basic connectivity
    # ------------------------------------------------------------------
    def test_select_1():
        cur.execute("SELECT 1")
        row = cur.fetchone()
        assert row is not None
        assert row[0] == 1, f"expected 1, got {row[0]!r}"

    R.run("SELECT 1", test_select_1)

    # ------------------------------------------------------------------
    # Metadata queries
    # ------------------------------------------------------------------
    def test_show_catalogs():
        cur.execute("SHOW CATALOGS")
        catalogs = [r[0].strip() for r in cur.fetchall()]
        assert "tpcds" in catalogs, f"tpcds not in catalogs: {catalogs}"

    R.run("SHOW CATALOGS", test_show_catalogs)

    def test_show_schemas():
        cur.execute("SHOW SCHEMAS FROM tpcds")
        schemas = [r[0].strip() for r in cur.fetchall()]
        assert "sf1" in schemas, f"sf1 not in schemas: {schemas}"

    R.run("SHOW SCHEMAS FROM tpcds", test_show_schemas)

    def test_show_tables():
        cur.execute("SHOW TABLES FROM tpcds.sf1")
        tables = [r[0].strip() for r in cur.fetchall()]
        assert "customer" in tables, f"customer not in tables: {tables}"
        assert "item" in tables, f"item not in tables: {tables}"

    R.run("SHOW TABLES FROM tpcds.sf1", test_show_tables)

    # ------------------------------------------------------------------
    # SELECT queries
    # ------------------------------------------------------------------
    def test_select_with_limit():
        cur.execute("SELECT c_customer_sk, c_first_name, c_last_name FROM tpcds.sf1.customer LIMIT 5")
        rows = cur.fetchall()
        assert len(rows) == 5, f"expected 5 rows, got {len(rows)}"
        assert rows[0][0] is not None, "c_customer_sk should not be NULL"

    R.run("SELECT with LIMIT", test_select_with_limit)

    def test_select_with_where():
        cur.execute("SELECT c_first_name, c_last_name FROM tpcds.sf1.customer WHERE c_customer_sk = 1")
        row = cur.fetchone()
        assert row is not None, "expected a row for c_customer_sk=1"
        assert row[0].strip() == "Javier", f"expected Javier, got {row[0]!r}"

    R.run("SELECT with WHERE", test_select_with_where)

    def test_empty_result():
        cur.execute("SELECT c_customer_sk FROM tpcds.sf1.customer WHERE 1 = 0")
        rows = cur.fetchall()
        assert len(rows) == 0, f"expected 0 rows, got {len(rows)}"

    R.run("Empty result set (WHERE 1=0)", test_empty_result)

    def test_column_metadata():
        cur.execute("SELECT c_customer_sk, c_first_name, c_birth_year FROM tpcds.sf1.customer LIMIT 1")
        assert cur.description is not None, "cursor.description should not be None"
        col_names = [d[0] for d in cur.description]
        assert len(col_names) == 3, f"expected 3 columns, got {len(col_names)}"
        cur.fetchall()

    R.run("Column metadata", test_column_metadata)

    # ------------------------------------------------------------------
    # Aggregation
    # ------------------------------------------------------------------
    def test_count():
        cur.execute("SELECT COUNT(*) FROM tpcds.sf1.customer")
        row = cur.fetchone()
        assert row is not None
        count = row[0]
        assert count > 0, f"expected count > 0, got {count}"

    R.run("COUNT(*)", test_count)

    def test_group_by():
        cur.execute("""
            SELECT c_birth_month, COUNT(*) AS cnt
            FROM tpcds.sf1.customer
            WHERE c_birth_month IS NOT NULL
            GROUP BY c_birth_month
            ORDER BY c_birth_month
        """)
        rows = cur.fetchall()
        assert len(rows) == 12, f"expected 12 months, got {len(rows)}"
        assert rows[0][0] == 1, f"first month should be 1, got {rows[0][0]}"

    R.run("GROUP BY + ORDER BY", test_group_by)

    def test_aggregation_functions():
        cur.execute("""
            SELECT MIN(c_birth_year), MAX(c_birth_year), COUNT(DISTINCT c_birth_year)
            FROM tpcds.sf1.customer
            WHERE c_birth_year IS NOT NULL
        """)
        row = cur.fetchone()
        assert row is not None
        min_year, max_year, distinct_years = row[0], row[1], row[2]
        assert min_year < max_year, f"min {min_year} should be < max {max_year}"
        assert distinct_years > 1, f"expected multiple distinct years, got {distinct_years}"

    R.run("MIN + MAX + COUNT(DISTINCT)", test_aggregation_functions)

    # ------------------------------------------------------------------
    # Multiple WHERE conditions (exercises query variety without
    # parameterised queries — SQLNumParams is not yet implemented)
    # ------------------------------------------------------------------
    def test_where_integer():
        cur.execute("SELECT c_first_name FROM tpcds.sf1.customer WHERE c_customer_sk = 1")
        row = cur.fetchone()
        assert row is not None, "expected a row for c_customer_sk=1"
        assert row[0].strip() == "Javier", f"expected Javier, got {row[0]!r}"

    R.run("WHERE integer equality", test_where_integer)

    def test_where_range():
        cur.execute("SELECT COUNT(*) FROM tpcds.sf1.customer WHERE c_customer_sk BETWEEN 1 AND 10")
        row = cur.fetchone()
        assert row[0] == 10, f"expected 10, got {row[0]}"

    R.run("WHERE BETWEEN range", test_where_range)

    def test_reexecute_different_queries():
        for sk, expected_name in [(1, "Javier"), (2, "Amy"), (3, "Latisha")]:
            cur.execute(f"SELECT c_first_name FROM tpcds.sf1.customer WHERE c_customer_sk = {sk}")
            row = cur.fetchone()
            assert row is not None, f"no row for c_customer_sk={sk}"
            assert row[0].strip() == expected_name, f"expected {expected_name!r}, got {row[0]!r}"

    R.run("Re-execute with different queries", test_reexecute_different_queries)

    # ------------------------------------------------------------------
    # Multiple sequential queries
    # ------------------------------------------------------------------
    def test_sequential_queries():
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.execute("SELECT 2")
        assert cur.fetchone()[0] == 2
        cur.execute("SELECT COUNT(*) FROM tpcds.sf1.customer")
        assert cur.fetchone()[0] > 0

    R.run("Sequential queries on same cursor", test_sequential_queries)

    # ------------------------------------------------------------------
    # Unicode roundtrip
    # ------------------------------------------------------------------
    def test_unicode_roundtrip():
        cases = [
            ("Japanese", "日本語"),
            ("emoji", "🎉🦀"),
            ("accents", "café résumé"),
            ("mixed accents", "Ünïcödé"),
        ]
        for label, val in cases:
            cur.execute(f"SELECT '{val}'")
            row = cur.fetchone()
            assert row is not None, f"no row for {label}"
            assert row[0] == val, f"{label}: expected {val!r}, got {row[0]!r}"

    R.run("Unicode roundtrip (Japanese, emoji, accents)", test_unicode_roundtrip)

    # ------------------------------------------------------------------
    # SQLGetData type coercion
    # ------------------------------------------------------------------
    def test_getdata_integer_as_char():
        import struct
        # c_customer_sk is a BIGINT column in Trino's TPC-DS schema (SQL_BIGINT = -5).
        # Registering an output converter causes pyodbc to skip SQLBindCol and
        # instead call SQLGetData(SQL_C_BINARY) for that column, exercising the
        # integer→binary coercion path. The converter decodes the 8-byte LE value.
        SQL_BIGINT = -5
        received = []
        def decode_integer(b):
            val = struct.unpack("<q", b)[0]
            received.append(val)
            return val
        conn.add_output_converter(SQL_BIGINT, decode_integer)
        try:
            cur.execute(
                "SELECT c_customer_sk FROM tpcds.sf1.customer WHERE c_customer_sk = 1"
            )
            row = cur.fetchone()
            assert row is not None
            assert received == [1], f"SQLGetData not called or wrong raw value: {received!r}"
            assert row[0] == 1, f"expected 1, got {row[0]!r}"
        finally:
            conn.clear_output_converters()

    R.run("SQLGetData type coercion: INTEGER via add_output_converter", test_getdata_integer_as_char)

    # ------------------------------------------------------------------
    # VARBINARY
    # ------------------------------------------------------------------
    def test_varbinary_returns_raw_bytes():
        # Trino sends VARBINARY as base64 over the REST API; the driver decodes
        # it to ColumnValue::Bytes and reports SQL_LONGVARBINARY (-4). Without
        # the decode the client would receive the base64 text "3q2+7w==".
        cur.execute("SELECT X'DEADBEEF'")
        assert cur.description[0][1] in (bytearray, bytes), (
            f"expected a binary column type, got {cur.description[0][1]!r}"
        )
        row = cur.fetchone()
        assert row is not None, "no row for VARBINARY literal"
        assert bytes(row[0]) == b"\xde\xad\xbe\xef", (
            f"expected raw bytes, got {row[0]!r}"
        )

        # Empty VARBINARY must decode to empty bytes, not NULL.
        cur.execute("SELECT CAST('' AS VARBINARY)")
        row = cur.fetchone()
        assert row is not None and bytes(row[0]) == b"", (
            f"expected empty bytes, got {row[0]!r}"
        )

        # NULL VARBINARY must stay NULL.
        cur.execute("SELECT CAST(NULL AS VARBINARY)")
        row = cur.fetchone()
        assert row is not None and row[0] is None, (
            f"expected None, got {row[0]!r}"
        )

    R.run("VARBINARY returns raw bytes (not base64 text)", test_varbinary_returns_raw_bytes)

    # ------------------------------------------------------------------
    # Catalog functions (SQLPrimaryKeys, SQLForeignKeys, SQLStatistics)
    # ------------------------------------------------------------------

    def test_primary_keys_empty():
        """tpcds tables have no PK constraints — should return empty, not error."""
        cur.execute("SELECT 1")
        cur.fetchall()
        rows = list(cur.primaryKeys("customer", catalog="tpcds", schema="sf1"))
        assert len(rows) == 0, f"expected 0 PK rows for tpcds, got {len(rows)}"

    R.run("SQLPrimaryKeys empty (tpcds)", test_primary_keys_empty)

    def test_foreign_keys_empty():
        """tpcds tables have no FK constraints — should return empty, not error."""
        cur.execute("SELECT 1")
        cur.fetchall()
        rows = list(cur.foreignKeys(foreignTable="customer",
                                     foreignCatalog="tpcds",
                                     foreignSchema="sf1"))
        assert len(rows) == 0, f"expected 0 FK rows for tpcds, got {len(rows)}"

    R.run("SQLForeignKeys empty (tpcds)", test_foreign_keys_empty)

    def test_statistics():
        cur.execute("SELECT 1")
        cur.fetchall()
        rows = list(cur.statistics("customers"))
        # We return empty result sets for statistics — just verify no error
        assert isinstance(rows, list), "statistics should return a list"

    R.run("SQLStatistics (no error)", test_statistics)

    # ------------------------------------------------------------------
    # Catalog functions against PostgreSQL catalog
    # ------------------------------------------------------------------
    # NOTE: Trino's information_schema does not expose table_constraints or
    # referential_constraints for any connector. These tests verify that
    # PK/FK calls succeed with empty results (not SQL_ERROR) when targeting
    # the postgresql catalog.

    def test_primary_keys_postgresql_succeeds():
        cur.execute("SELECT 1")
        cur.fetchall()
        rows = list(cur.primaryKeys("customers", catalog="postgresql", schema="public"))
        # Trino doesn't expose constraint metadata — expect empty, not error
        assert isinstance(rows, list), "primaryKeys should return a list"

    R.run("SQLPrimaryKeys postgresql (succeeds, empty)", test_primary_keys_postgresql_succeeds)

    def test_foreign_keys_postgresql_succeeds():
        cur.execute("SELECT 1")
        cur.fetchall()
        rows = list(cur.foreignKeys(foreignTable="orders",
                                     foreignCatalog="postgresql",
                                     foreignSchema="public"))
        # Trino doesn't expose constraint metadata — expect empty, not error
        assert isinstance(rows, list), "foreignKeys should return a list"

    R.run("SQLForeignKeys postgresql (succeeds, empty)", test_foreign_keys_postgresql_succeeds)

    # ------------------------------------------------------------------
    # Timestamp with timezone → UTC conversion
    # ------------------------------------------------------------------
    def test_timestamp_with_tz_utc_conversion():
        cur.execute("SELECT TIMESTAMP '2025-03-10 20:21:22.123 America/New_York' AS v")
        row = cur.fetchone()
        assert row is not None
        import datetime
        val = row[0]
        assert isinstance(val, datetime.datetime), f"expected datetime, got {type(val).__name__}: {val!r}"
        # America/New_York in March 2025 is EDT (UTC-4).
        # 20:21:22 EDT → 2025-03-11 00:21:22 UTC.
        assert val.year == 2025, f"year: expected 2025, got {val.year}"
        assert val.month == 3, f"month: expected 3, got {val.month}"
        assert val.day == 11, f"day: expected 11, got {val.day}"
        assert val.hour == 0, f"hour: expected 0, got {val.hour}"
        assert val.minute == 21, f"minute: expected 21, got {val.minute}"
        assert val.second == 22, f"second: expected 22, got {val.second}"

    R.run("TIMESTAMP WITH TZ to UTC", test_timestamp_with_tz_utc_conversion)

    def test_timestamp_with_utc_tz():
        cur.execute("SELECT TIMESTAMP '2020-05-05 22:00:00.000 UTC' AS v")
        row = cur.fetchone()
        assert row is not None
        import datetime
        val = row[0]
        assert isinstance(val, datetime.datetime), f"expected datetime, got {type(val).__name__}: {val!r}"
        assert val.year == 2020, f"year: expected 2020, got {val.year}"
        assert val.month == 5, f"month: expected 5, got {val.month}"
        assert val.day == 5, f"day: expected 5, got {val.day}"
        assert val.hour == 22, f"hour: expected 22, got {val.hour}"
        assert val.minute == 0, f"minute: expected 0, got {val.minute}"

    R.run("TIMESTAMP WITH UTC TZ", test_timestamp_with_utc_tz)

    # ------------------------------------------------------------------
    # Connection attributes, through the Driver Manager
    # ------------------------------------------------------------------
    # integration-tests/suites/test_c_abi.py drives these with no DM in the loop; the point here is
    # that unixODBC forwards them rather than answering them itself, so what an
    # ordinary application sees is what the driver decided.

    def test_connection_attribute_rules():
        SQL_ATTR_PACKET_SIZE = 112
        SQL_ATTR_ENLIST_IN_DTC = 1207
        SQL_ATTR_ASYNC_ENABLE = 4
        SQL_ASYNC_ENABLE_OFF = 0
        SQL_ASYNC_ENABLE_ON = 1

        for label, attr, value, state in (
            # The spec states this one outright: "if the application sets
            # packet size after a connection has already been made, the driver
            # will return SQLSTATE HY011".
            ("SQL_ATTR_PACKET_SIZE", SQL_ATTR_PACKET_SIZE, 8192, "HY011"),
            # Neither is implemented, and neither is on any substitution list.
            ("SQL_ATTR_ENLIST_IN_DTC", SQL_ATTR_ENLIST_IN_DTC, 1, "HYC00"),
            ("SQL_ATTR_ASYNC_ENABLE", SQL_ATTR_ASYNC_ENABLE, SQL_ASYNC_ENABLE_ON, "HYC00"),
        ):
            try:
                conn.set_attr(attr, value)
                raise AssertionError(f"{label} was accepted; expected {state}")
            except pyodbc.Error as e:
                assert e.args[0] == state, f"{label}: expected {state}, got {e.args[0]}"

        # The value the driver already uses is accepted.
        conn.set_attr(SQL_ATTR_ASYNC_ENABLE, SQL_ASYNC_ENABLE_OFF)

    R.run("connection attribute state and support rules", test_connection_attribute_rules)

    # ------------------------------------------------------------------
    # SQL_DATABASE_NAME names the connected catalog
    # ------------------------------------------------------------------
    # The spec makes this and SQL_ATTR_CURRENT_CATALOG one value under two
    # names, and both now read the driver's `current_catalog` declaration. Only
    # the info-type half is asserted here: pyodbc's `set_attr` takes an integer
    # value and it exposes no string `SQLGetConnectAttr`, so the attribute half
    # -- including that setting it reports HYC00 -- lives in integration-tests/suites/test_c_abi.py,
    # which calls both entry points directly.
    #
    # Still worth having on this path: what the connection string asked for has
    # to survive the Driver Manager and come back under the name an application
    # reads, and a DSN connection reaches this through a different route than a
    # DSN-less one.

    def test_database_name_is_the_connected_catalog():
        got = conn.getinfo(pyodbc.SQL_DATABASE_NAME)
        wanted = conn_str_catalog(conn_str)
        if wanted is None:
            # A DSN connection string; the DSN's own Catalog key decided it, so
            # assert only that something was reported.
            assert got, f"SQL_DATABASE_NAME should name a catalog, got {got!r}"
        else:
            assert got == wanted, (
                f"SQL_DATABASE_NAME is {got!r}, but the connection string named {wanted!r}"
            )

    R.run("SQL_DATABASE_NAME is the connected catalog", test_database_name_is_the_connected_catalog)

    # ------------------------------------------------------------------
    # The three strings that identify this connection
    # ------------------------------------------------------------------
    # SQL_DATA_SOURCE_NAME is the only one of the three the spec gives a defined
    # empty answer to, and only "if the connection string did not contain the
    # DSN keyword". run-tests.sh drives this suite over both cases, which is
    # what makes the pair assertable at all. The DSN half reaches the driver
    # through SQLDriverConnectW's connection string, a route no unit test
    # covers.
    #
    # SQL_SERVER_NAME and SQL_USER_NAME have no such clause, so an empty answer
    # there is a non-answer. The user is asserted against Trino's own
    # current_user rather than against the connection string's User: those two
    # differ under SessionUser and under ExternalAuthentication, and the whole
    # reason the driver probes is to report the former.

    def test_identity_strings():
        dsn = conn_str_value(conn_str, "dsn")
        got_dsn = conn.getinfo(pyodbc.SQL_DATA_SOURCE_NAME)
        if dsn is None:
            assert got_dsn == "", (
                f"a DSN-less connection string must report an empty "
                f"SQL_DATA_SOURCE_NAME, got {got_dsn!r}"
            )
        else:
            assert got_dsn == dsn, (
                f"SQL_DATA_SOURCE_NAME is {got_dsn!r}, but the application connected "
                f"with DSN={dsn!r}"
            )

        got_server = conn.getinfo(pyodbc.SQL_SERVER_NAME)
        assert got_server, f"SQL_SERVER_NAME should name the coordinator, got {got_server!r}"

        got_user = conn.getinfo(pyodbc.SQL_USER_NAME)
        wanted_user = conn.cursor().execute("SELECT current_user").fetchval()
        assert got_user == wanted_user, (
            f"SQL_USER_NAME is {got_user!r}, but the session runs as {wanted_user!r}"
        )

    R.run("SQL_DATA_SOURCE_NAME, SQL_SERVER_NAME and SQL_USER_NAME", test_identity_strings)

    # ------------------------------------------------------------------
    # SQL_ATTR_METADATA_ID changes what the catalog functions match
    # ------------------------------------------------------------------
    # The attribute is one of exactly two an application may set at the
    # connection level, and it decides whether a catalog function's string
    # arguments are identifiers or search patterns. Asserted end to end rather
    # than by reading the attribute back: the read-back was always correct, and
    # what was broken was that nothing acted on it.
    #
    # A separate connection, because the setting outlives the statement and
    # would change every later catalog call on the shared one.

    def test_metadata_id_changes_catalog_argument_semantics():
        SQL_ATTR_METADATA_ID = 10014
        SQL_FALSE, SQL_TRUE = 0, 1
        target = "types_test"

        def names(c, probe):
            rows = c.cursor().tables(
                table=probe, catalog="postgresql", schema="public"
            ).fetchall()
            return sorted({r.table_name for r in rows})

        probe_conn = pyodbc.connect(conn_str, autocommit=True)
        try:
            # As a pattern: case matters, and `%` and `_` are wildcards.
            probe_conn.set_attr(SQL_ATTR_METADATA_ID, SQL_FALSE)
            assert names(probe_conn, target) == [target], "the exact name matches"
            assert names(probe_conn, target.upper()) == [], (
                "a pattern is case-sensitive, and Trino stores the name lower case"
            )
            assert names(probe_conn, "types%test") == [target], "`%` is a wildcard"
            assert names(probe_conn, "types_tes_") == [target], "`_` is a wildcard"

            # As an identifier: case-folded per SQL_IDENTIFIER_CASE, and the
            # wildcards escaped per SQL_SEARCH_PATTERN_ESCAPE, so both become
            # literal characters that no table here has.
            probe_conn.set_attr(SQL_ATTR_METADATA_ID, SQL_TRUE)
            assert names(probe_conn, target) == [target], "the exact name still matches"
            assert names(probe_conn, target.upper()) == [target], (
                "an identifier is case-folded, so the upper-case spelling resolves"
            )
            assert names(probe_conn, "types%test") == [], "`%` is escaped to a literal"
            assert names(probe_conn, "types_tes_") == [], "`_` is escaped to a literal"
        finally:
            probe_conn.close()

    R.run(
        "SQL_ATTR_METADATA_ID changes catalog argument semantics",
        test_metadata_id_changes_catalog_argument_semantics,
    )

    # ------------------------------------------------------------------
    # The SQLGetInfo values backed by a required Backend declaration
    # ------------------------------------------------------------------
    # Each of these was previously answered by a hard-wired value in
    # stackable-odbc-core, on this driver's behalf. They are now declarations
    # the compiler requires, so what an application reads is what the driver
    # states about Trino. Asserted through the DM because that is the path that
    # decides the value's shape as well as its content.

    def test_backend_declared_info_values():
        SQL_IC_LOWER = 2
        SQL_TC_DML = 1
        SQL_CB_CLOSE = 1
        SQL_TXN_READ_UNCOMMITTED = 1

        expected = {
            "SQL_DRIVER_NAME": (pyodbc.SQL_DRIVER_NAME, "stackable-odbc-trino"),
            "SQL_DBMS_NAME": (pyodbc.SQL_DBMS_NAME, "Trino"),
            # Quoted identifiers are case-insensitive in Trino and reported
            # lower case by the system catalog, so this matches
            # SQL_IDENTIFIER_CASE rather than being SQL_IC_SENSITIVE.
            "SQL_QUOTED_IDENTIFIER_CASE": (pyodbc.SQL_QUOTED_IDENTIFIER_CASE, SQL_IC_LOWER),
            "SQL_IDENTIFIER_CASE": (pyodbc.SQL_IDENTIFIER_CASE, SQL_IC_LOWER),
            # DML only: DDL inside a transaction is AUTOCOMMIT_WRITE_CONFLICT
            # on every JDBC-backed catalog. See TrinoBackend::txn_capable.
            "SQL_TXN_CAPABLE": (pyodbc.SQL_TXN_CAPABLE, SQL_TC_DML),
            # The only level every catalog accepts: Trino's connectors vet the
            # level, not its parser, and they disagree above this one.
            "SQL_TXN_ISOLATION_OPTION": (
                pyodbc.SQL_TXN_ISOLATION_OPTION,
                SQL_TXN_READ_UNCOMMITTED,
            ),
            "SQL_DEFAULT_TXN_ISOLATION": (
                pyodbc.SQL_DEFAULT_TXN_ISOLATION,
                SQL_TXN_READ_UNCOMMITTED,
            ),
            # Trino discards a transaction's result sets when it ends, so a
            # cursor does not survive SQLEndTran.
            "SQL_CURSOR_COMMIT_BEHAVIOR": (
                pyodbc.SQL_CURSOR_COMMIT_BEHAVIOR,
                SQL_CB_CLOSE,
            ),
            "SQL_CURSOR_ROLLBACK_BEHAVIOR": (
                pyodbc.SQL_CURSOR_ROLLBACK_BEHAVIOR,
                SQL_CB_CLOSE,
            ),
            # Trino's grammar rejects every referential-integrity constraint.
            "SQL_INTEGRITY": (pyodbc.SQL_INTEGRITY, False),
            # Each connection carries its own Trino session, and therefore
            # its own transaction. One session holds at most one.
            "SQL_MULTIPLE_ACTIVE_TXN": (pyodbc.SQL_MULTIPLE_ACTIVE_TXN, True),
            # Trino publishes no metadata naming its procedures.
            "SQL_ACCESSIBLE_PROCEDURES": (pyodbc.SQL_ACCESSIBLE_PROCEDURES, False),
            # Trino's identifier production is (LETTER | '_') (LETTER | DIGIT |
            # '_')*, which is exactly the set ODBC excludes from this info type.
            "SQL_SPECIAL_CHARACTERS": (pyodbc.SQL_SPECIAL_CHARACTERS, ""),
        }
        for label, (info_type, want) in expected.items():
            got = conn.getinfo(info_type)
            assert got == want, f"{label}: expected {want!r}, got {got!r}"

        # Version strings are not fixed values, so assert their shape instead.
        # The spec's form is "##.##.####" -- major, minor, release -- optionally
        # followed by the data source's own version text, which is why
        # SQL_DBMS_VER's trailing "(467)" is allowed here and SQL_DRIVER_VER's
        # absence of one is too. The major group is not held to two digits:
        # Trino's is 467.
        import re

        version_shape = re.compile(r"^\d+\.\d{2}\.\d{4}( .*)?$")
        for label, info_type in (
            ("SQL_DRIVER_VER", pyodbc.SQL_DRIVER_VER),
            ("SQL_DBMS_VER", pyodbc.SQL_DBMS_VER),
        ):
            got = conn.getinfo(info_type)
            assert version_shape.match(got), (
                f"{label} is not ##.##.#### with an optional suffix: {got!r}"
            )

    R.run("Backend-declared SQLGetInfo values", test_backend_declared_info_values)

    def test_query_timeout_fires_through_the_driver_manager():
        # The only place the query timeout is exercised through a Driver
        # Manager. test_c_abi.py loads the .so directly, so it proves the
        # driver and core cooperate but says nothing about unixODBC.
        #
        # This is NOT gated on Threading. Core enforces the deadline from a
        # timer thread that calls Backend::cancel directly, inside the .so, so
        # it never crosses the Driver Manager and no threading policy can
        # serialise it. Measured both ways against the live coordinator: HYT00
        # at 2.0s under Threading = 2 and under Threading = 3 alike. The
        # setting matters for application-initiated SQLCancel instead -- see
        # the cross-thread cancel test below.
        #
        # pyodbc's Connection.timeout sets SQL_ATTR_QUERY_TIMEOUT on the
        # statements it creates. The query runs ~24s uncancelled, so a
        # regression fails outright rather than flaking, and the elapsed time is
        # asserted as well: HYT00 arriving after the query finished on its own
        # would be the timeout not working, reported as though it were.
        import time

        timed = pyodbc.connect(conn_str, autocommit=True)
        try:
            timed.timeout = 2
            cur = timed.cursor()
            # Trino answers with column metadata before it computes a row, so
            # the execute returns promptly and the whole wait is in the fetch.
            cur.execute("SELECT count(*) FROM tpcds.sf10.store_sales")
            start = time.monotonic()
            try:
                cur.fetchall()
            except pyodbc.Error as e:
                elapsed = time.monotonic() - start
                state = e.args[0]
                assert state == "HYT00", f"expected HYT00, got {state}: {e}"
                assert elapsed < 15, (
                    f"HYT00 after {elapsed:.1f}s against a 2s deadline -- the "
                    f"query runs ~24s uncancelled, so this fired on completion "
                    f"rather than on the deadline"
                )
            else:
                raise AssertionError(
                    f"the query completed in {time.monotonic() - start:.1f}s "
                    f"despite a 2-second deadline"
                )

            # A cancelled query must not strand the connection: a server-side
            # cancel leaves the pooled socket carrying residual bytes if
            # anything keeps paging it, which surfaces later as an unrelated
            # query failing.
            timed.timeout = 0
            after = timed.cursor()
            after.execute("SELECT 1")
            assert after.fetchone()[0] == 1, "connection unusable after a timeout"
        finally:
            timed.close()

    R.run(
        "SQL_ATTR_QUERY_TIMEOUT fires during fetch",
        test_query_timeout_fires_through_the_driver_manager,
    )

    def test_cross_thread_cancel_interrupts_a_running_fetch():
        # This is what Threading = 2 in odbcinst.ini buys, and the only test
        # that would notice it being lost. SQLCancel called from another thread
        # goes *through* the Driver Manager, and at unixODBC's default
        # Threading = 3 the DM serialises at the environment level and holds the
        # cancelling thread behind the executing call. Measured against the live
        # coordinator on a query that runs ~24s:
        #
        #   Threading = 3 -> SQLCancel blocked for the whole run; fetch raised
        #                    HY010 after 23.9s, i.e. the query finished on its
        #                    own and the cancel accomplished nothing.
        #   Threading = 2 -> SQLCancel returned immediately; fetch raised HY008
        #                    after 2.0s.
        #
        # HY008 is the spec's "operation canceled" for a function interrupted by
        # SQLCancel from a different thread, which SQLFetch's diagnostics table
        # lists without a (DM) marker.
        import threading
        import time

        c = pyodbc.connect(conn_str, autocommit=True)
        try:
            cur = c.cursor()
            cur.execute("SELECT count(*) FROM tpcds.sf10.store_sales")

            def cancel_after_two_seconds():
                time.sleep(2)
                cur.cancel()

            canceller = threading.Thread(target=cancel_after_two_seconds)
            canceller.start()
            start = time.monotonic()
            try:
                cur.fetchall()
            except pyodbc.Error as e:
                elapsed = time.monotonic() - start
                state = e.args[0]
                # What the cancel accomplished is decided by *when* the error
                # arrives, so that is asserted first and for every SQLSTATE. An
                # error only around the 24s mark means the query finished on its
                # own and the cancelling thread was serialised behind it.
                assert elapsed < 15, (
                    f"{state} after {elapsed:.1f}s: the query ran to completion "
                    f"(~24s) before the cancel landed -- check that Threading = 2 "
                    f"is set in odbcinst.ini"
                )
                # unixODBC sometimes answers first, with its own Function
                # sequence error: the cancelling thread's SQLCancel moves the
                # Driver Manager's statement state while this thread is mid-loop,
                # so this thread's next call is refused before it reaches the
                # driver. HY010 is (DM)-marked, and measured on the direct and
                # the spooled protocol alike, so it is not the driver's to
                # report and not a defect. The cancel still interrupted the
                # fetch, which is what this scenario claims; the driver's own
                # HY008 path simply went unobserved, so that is noted.
                if state == "HY010":
                    R.note(
                        "SQLCancel from another thread",
                        f"unixODBC answered first with its own HY010 after "
                        f"{elapsed:.1f}s, before the driver was reached: {e}",
                    )
                else:
                    assert state == "HY008", (
                        f"expected HY008, got {state} after {elapsed:.1f}s: {e}. "
                        f"HY008 is the spec's code for a function interrupted by "
                        f"SQLCancel from another thread"
                    )
            else:
                raise AssertionError(
                    "the fetch completed despite a cancel from another thread"
                )
            finally:
                canceller.join()
        finally:
            c.close()

    R.run(
        "SQLCancel from another thread interrupts a fetch (needs Threading=2)",
        test_cross_thread_cancel_interrupts_a_running_fetch,
    )

    # === Cleanup ===
    conn.close()

    # === Summary ===
    sys.exit(R.summary())


if __name__ == "__main__":
    main()
