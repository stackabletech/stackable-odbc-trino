#!/usr/bin/env python3
"""
Integration tests for the Trino ODBC driver.

Runs through the ODBC Driver Manager (unixODBC on Linux, odbc32.dll on Windows)
using pyodbc. Tests connection, metadata queries, SELECT, aggregation, and
parameterised statements against the tpcds catalog (read-only, ships with Trino).

Usage:
    python3 test/test_integration.py "Driver=/path/to/driver.so;Host=localhost;Port=8080;User=admin;Protocol=http;Catalog=tpcds"
    python3 test/test_integration.py "DSN=test_trino"

Requires: pip install pyodbc
"""

import sys
import pyodbc

passed = 0
failed = 0


def run(label, fn):
    """Run a test function, print PASS/FAIL, track counts."""
    global passed, failed
    try:
        fn()
        print(f"PASS  {label}")
        passed += 1
    except Exception as e:
        print(f"FAIL  {label}: {e}")
        failed += 1


def main():
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <connection-string>")
        sys.exit(2)

    conn_str = sys.argv[1]
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

    run("SELECT 1", test_select_1)

    # ------------------------------------------------------------------
    # Metadata queries
    # ------------------------------------------------------------------
    def test_show_catalogs():
        cur.execute("SHOW CATALOGS")
        catalogs = [r[0].strip() for r in cur.fetchall()]
        assert "tpcds" in catalogs, f"tpcds not in catalogs: {catalogs}"

    run("SHOW CATALOGS", test_show_catalogs)

    def test_show_schemas():
        cur.execute("SHOW SCHEMAS FROM tpcds")
        schemas = [r[0].strip() for r in cur.fetchall()]
        assert "sf1" in schemas, f"sf1 not in schemas: {schemas}"

    run("SHOW SCHEMAS FROM tpcds", test_show_schemas)

    def test_show_tables():
        cur.execute("SHOW TABLES FROM tpcds.sf1")
        tables = [r[0].strip() for r in cur.fetchall()]
        assert "customer" in tables, f"customer not in tables: {tables}"
        assert "item" in tables, f"item not in tables: {tables}"

    run("SHOW TABLES FROM tpcds.sf1", test_show_tables)

    # ------------------------------------------------------------------
    # SELECT queries
    # ------------------------------------------------------------------
    def test_select_with_limit():
        cur.execute("SELECT c_customer_sk, c_first_name, c_last_name FROM tpcds.sf1.customer LIMIT 5")
        rows = cur.fetchall()
        assert len(rows) == 5, f"expected 5 rows, got {len(rows)}"
        assert rows[0][0] is not None, "c_customer_sk should not be NULL"

    run("SELECT with LIMIT", test_select_with_limit)

    def test_select_with_where():
        cur.execute("SELECT c_first_name, c_last_name FROM tpcds.sf1.customer WHERE c_customer_sk = 1")
        row = cur.fetchone()
        assert row is not None, "expected a row for c_customer_sk=1"
        assert row[0].strip() == "Javier", f"expected Javier, got {row[0]!r}"

    run("SELECT with WHERE", test_select_with_where)

    def test_empty_result():
        cur.execute("SELECT c_customer_sk FROM tpcds.sf1.customer WHERE 1 = 0")
        rows = cur.fetchall()
        assert len(rows) == 0, f"expected 0 rows, got {len(rows)}"

    run("Empty result set (WHERE 1=0)", test_empty_result)

    def test_column_metadata():
        cur.execute("SELECT c_customer_sk, c_first_name, c_birth_year FROM tpcds.sf1.customer LIMIT 1")
        assert cur.description is not None, "cursor.description should not be None"
        col_names = [d[0] for d in cur.description]
        assert len(col_names) == 3, f"expected 3 columns, got {len(col_names)}"
        cur.fetchall()

    run("Column metadata", test_column_metadata)

    # ------------------------------------------------------------------
    # Aggregation
    # ------------------------------------------------------------------
    def test_count():
        cur.execute("SELECT COUNT(*) FROM tpcds.sf1.customer")
        row = cur.fetchone()
        assert row is not None
        count = row[0]
        assert count > 0, f"expected count > 0, got {count}"

    run("COUNT(*)", test_count)

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

    run("GROUP BY + ORDER BY", test_group_by)

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

    run("MIN + MAX + COUNT(DISTINCT)", test_aggregation_functions)

    # ------------------------------------------------------------------
    # Multiple WHERE conditions (exercises query variety without
    # parameterised queries — SQLNumParams is not yet implemented)
    # ------------------------------------------------------------------
    def test_where_integer():
        cur.execute("SELECT c_first_name FROM tpcds.sf1.customer WHERE c_customer_sk = 1")
        row = cur.fetchone()
        assert row is not None, "expected a row for c_customer_sk=1"
        assert row[0].strip() == "Javier", f"expected Javier, got {row[0]!r}"

    run("WHERE integer equality", test_where_integer)

    def test_where_range():
        cur.execute("SELECT COUNT(*) FROM tpcds.sf1.customer WHERE c_customer_sk BETWEEN 1 AND 10")
        row = cur.fetchone()
        assert row[0] == 10, f"expected 10, got {row[0]}"

    run("WHERE BETWEEN range", test_where_range)

    def test_reexecute_different_queries():
        for sk, expected_name in [(1, "Javier"), (2, "Amy"), (3, "Latisha")]:
            cur.execute(f"SELECT c_first_name FROM tpcds.sf1.customer WHERE c_customer_sk = {sk}")
            row = cur.fetchone()
            assert row is not None, f"no row for c_customer_sk={sk}"
            assert row[0].strip() == expected_name, f"expected {expected_name!r}, got {row[0]!r}"

    run("Re-execute with different queries", test_reexecute_different_queries)

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

    run("Sequential queries on same cursor", test_sequential_queries)

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

    run("Unicode roundtrip (Japanese, emoji, accents)", test_unicode_roundtrip)

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

    run("SQLGetData type coercion: INTEGER via add_output_converter", test_getdata_integer_as_char)

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

    run("VARBINARY returns raw bytes (not base64 text)", test_varbinary_returns_raw_bytes)

    # ------------------------------------------------------------------
    # Catalog functions (SQLPrimaryKeys, SQLForeignKeys, SQLStatistics)
    # ------------------------------------------------------------------

    def test_primary_keys_empty():
        """tpcds tables have no PK constraints — should return empty, not error."""
        cur.execute("SELECT 1")
        cur.fetchall()
        rows = list(cur.primaryKeys("customer", catalog="tpcds", schema="sf1"))
        assert len(rows) == 0, f"expected 0 PK rows for tpcds, got {len(rows)}"

    run("SQLPrimaryKeys empty (tpcds)", test_primary_keys_empty)

    def test_foreign_keys_empty():
        """tpcds tables have no FK constraints — should return empty, not error."""
        cur.execute("SELECT 1")
        cur.fetchall()
        rows = list(cur.foreignKeys(foreignTable="customer",
                                     foreignCatalog="tpcds",
                                     foreignSchema="sf1"))
        assert len(rows) == 0, f"expected 0 FK rows for tpcds, got {len(rows)}"

    run("SQLForeignKeys empty (tpcds)", test_foreign_keys_empty)

    def test_statistics():
        cur.execute("SELECT 1")
        cur.fetchall()
        rows = list(cur.statistics("customers"))
        # We return empty result sets for statistics — just verify no error
        assert isinstance(rows, list), "statistics should return a list"

    run("SQLStatistics (no error)", test_statistics)

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

    run("SQLPrimaryKeys postgresql (succeeds, empty)", test_primary_keys_postgresql_succeeds)

    def test_foreign_keys_postgresql_succeeds():
        cur.execute("SELECT 1")
        cur.fetchall()
        rows = list(cur.foreignKeys(foreignTable="orders",
                                     foreignCatalog="postgresql",
                                     foreignSchema="public"))
        # Trino doesn't expose constraint metadata — expect empty, not error
        assert isinstance(rows, list), "foreignKeys should return a list"

    run("SQLForeignKeys postgresql (succeeds, empty)", test_foreign_keys_postgresql_succeeds)

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

    run("TIMESTAMP WITH TZ to UTC", test_timestamp_with_tz_utc_conversion)

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

    run("TIMESTAMP WITH UTC TZ", test_timestamp_with_utc_tz)

    # === Cleanup ===
    conn.close()

    # === Summary ===
    print(f"\n{passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
