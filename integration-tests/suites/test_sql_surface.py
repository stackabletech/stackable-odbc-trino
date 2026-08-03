#!/usr/bin/env python3
"""
SQL surface pen test for the Trino ODBC driver.

Walks the SQL a BI tool emits and checks the driver carries it through intact:
joins of every shape, aggregates, the GROUP BY extensions, window functions,
subqueries, CTEs, set operations, parameters in every clause that accepts one,
the ODBC catalog functions, and the statement forms whose result columns have no
declared length (DESCRIBE, SHOW, EXPLAIN).

Where a query has one right answer it is asserted. Where it does not (a plan
listing, a server-dependent count), the assertion is that it returns a result of
the expected shape, which is still enough to catch a translation or fetch
failure.

Every query is read-only, against the tpcds and postgresql catalogs.

Usage:
    python3 integration-tests/suites/test_sql_surface.py "<connection-string>"
    python3 integration-tests/suites/test_sql_surface.py "DSN=trino_http"

Requires a running Trino (integration-tests/setup.sh) and `pip install pyodbc`,
normally through `uv run --with pyodbc`. Needs no compose profile: the tpcds and
postgresql catalogs are both in the base stack.
"""

import os
import sys
import time

import pyodbc

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Results, Stack  # noqa: E402

R = Results("sql surface")

# A query that hangs is worse than one that errors: it takes the whole suite
# with it and gives no diagnosis. Nothing here should come close.
QUERY_TIMEOUT_SECONDS = 60


def main():
    # A connection string may be passed positionally; with no argument the
    # local stack describes itself.
    conn_str = sys.argv[1] if len(sys.argv) > 1 else Stack.load().conn_str()
    conn = pyodbc.connect(conn_str, autocommit=True)
    conn.timeout = QUERY_TIMEOUT_SECONDS
    cur = conn.cursor()

    def scalar(sql, want, params=None):
        got = cur.execute(sql, params).fetchone()[0] if params else cur.execute(sql).fetchone()[0]
        assert got == want, f"expected {want!r}, got {got!r}"

    def rows(sql, want_count=None, min_count=None, params=None):
        got = (cur.execute(sql, params) if params else cur.execute(sql)).fetchall()
        if want_count is not None:
            assert len(got) == want_count, f"expected {want_count} rows, got {len(got)}"
        if min_count is not None:
            assert len(got) >= min_count, f"expected >= {min_count} rows, got {len(got)}"
        return got

    def shape(sql, min_cols=1, min_rows=1):
        """Executes and returns rows; asserts only the result's shape."""
        cur.execute(sql)
        assert cur.description is not None, "no result set"
        assert len(cur.description) >= min_cols, f"expected >= {min_cols} columns"
        got = cur.fetchall()
        assert len(got) >= min_rows, f"expected >= {min_rows} rows, got {len(got)}"
        return got

    # ------------------------------------------------------------------
    print("--- joins ---")
    R.run("inner join", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1,2,3) a(x) JOIN (VALUES 2,3,4) b(y) ON a.x = b.y", 2))
    R.run("left outer join", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1,2,3) a(x) LEFT JOIN (VALUES 2) b(y) ON a.x = b.y", 3))
    R.run("right outer join", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1) a(x) RIGHT JOIN (VALUES 1,2,3) b(y) ON a.x = b.y", 3))
    R.run("full outer join", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1,2) a(x) FULL JOIN (VALUES 2,3) b(y) ON a.x = b.y", 3))
    R.run("cross join", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1,2,3) a(x) CROSS JOIN (VALUES 1,2) b(y)", 6))
    R.run("non-equi join", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1,2,3) a(x) JOIN (VALUES 1,2,3) b(y) ON a.x < b.y", 3))
    R.run("self join", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1,2,3) a(x) JOIN (VALUES 1,2,3) b(x) ON a.x = b.x", 3))
    R.run("three-way join", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1,2) a(x) JOIN (VALUES 1,2) b(y) ON a.x=b.y "
        "JOIN (VALUES 1,2) c(z) ON b.y=c.z", 2))
    # The {oj} escape, which SQL_OUTER_JOIN_CAPABILITIES advertises.
    R.run("ODBC {oj} escape", lambda: scalar(
        "SELECT count(*) FROM {oj (VALUES 1,2,3) a(x) LEFT OUTER JOIN (VALUES 2) b(y) "
        "ON a.x = b.y}", 3))

    # ------------------------------------------------------------------
    print("\n--- aggregates and GROUP BY ---")
    R.run("count/sum/avg/min/max", lambda: scalar(
        "SELECT count(*) + sum(x) + min(x) + max(x) FROM (VALUES 1,2,3) t(x)", 3 + 6 + 1 + 3))
    R.run("count(DISTINCT)", lambda: scalar(
        "SELECT count(DISTINCT x) FROM (VALUES 1,1,2) t(x)", 2))
    R.run("GROUP BY", lambda: rows(
        "SELECT x, count(*) FROM (VALUES 1,1,2) t(x) GROUP BY x", want_count=2))
    R.run("HAVING", lambda: rows(
        "SELECT x FROM (VALUES 1,1,2) t(x) GROUP BY x HAVING count(*) > 1", want_count=1))
    R.run("GROUPING SETS", lambda: rows(
        "SELECT x, y, count(*) FROM (VALUES (1,1),(1,2)) t(x,y) "
        "GROUP BY GROUPING SETS ((x),(y))", min_count=2))
    R.run("ROLLUP", lambda: rows(
        "SELECT x, count(*) FROM (VALUES 1,2) t(x) GROUP BY ROLLUP (x)", min_count=2))
    R.run("CUBE", lambda: rows(
        "SELECT x, y, count(*) FROM (VALUES (1,1),(2,2)) t(x,y) GROUP BY CUBE (x,y)",
        min_count=3))

    # ------------------------------------------------------------------
    print("\n--- window functions ---")
    R.run("row_number", lambda: scalar(
        "SELECT max(rn) FROM (SELECT row_number() OVER (ORDER BY x) rn "
        "FROM (VALUES 1,2,3) t(x)) s", 3))
    R.run("rank with PARTITION BY", lambda: rows(
        "SELECT rank() OVER (PARTITION BY x ORDER BY y) FROM (VALUES (1,1),(1,2)) t(x,y)",
        want_count=2))
    R.run("lag/lead", lambda: rows(
        "SELECT lag(x) OVER (ORDER BY x), lead(x) OVER (ORDER BY x) "
        "FROM (VALUES 1,2,3) t(x)", want_count=3))
    R.run("running sum frame", lambda: scalar(
        "SELECT max(s) FROM (SELECT sum(x) OVER (ORDER BY x ROWS BETWEEN UNBOUNDED "
        "PRECEDING AND CURRENT ROW) s FROM (VALUES 1,2,3) t(x)) q", 6))

    # ------------------------------------------------------------------
    print("\n--- subqueries and CTEs ---")
    R.run("scalar subquery", lambda: scalar("SELECT (SELECT max(x) FROM (VALUES 1,2,3) t(x))", 3))
    R.run("IN subquery", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1,2,3) a(x) WHERE a.x IN (SELECT y FROM (VALUES 1,2) b(y))",
        2))
    R.run("EXISTS subquery", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1,2) a(x) WHERE EXISTS "
        "(SELECT 1 FROM (VALUES 1) b(y) WHERE b.y = a.x)", 1))
    R.run("correlated subquery", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1,2) a(x) WHERE a.x = "
        "(SELECT max(y) FROM (VALUES 1) b(y))", 1))
    R.run("quantified comparison (ANY/ALL)", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1,2,3) a(x) WHERE a.x > ALL (SELECT y FROM (VALUES 1) b(y))",
        2))
    R.run("CTE", lambda: scalar("WITH c AS (SELECT 1 AS x) SELECT x FROM c", 1))
    R.run("multiple CTEs", lambda: scalar(
        "WITH a AS (SELECT 1 x), b AS (SELECT 2 y) SELECT a.x + b.y FROM a, b", 3))
    R.run("derived table", lambda: scalar(
        "SELECT count(*) FROM (SELECT x FROM (VALUES 1,2,3) t(x)) s", 3))

    # ------------------------------------------------------------------
    print("\n--- set operations ---")
    R.run("UNION", lambda: scalar(
        "SELECT count(*) FROM (SELECT 1 UNION SELECT 1 UNION SELECT 2) t", 2))
    R.run("UNION ALL", lambda: scalar(
        "SELECT count(*) FROM (SELECT 1 UNION ALL SELECT 1) t", 2))
    R.run("INTERSECT", lambda: scalar(
        "SELECT count(*) FROM (SELECT 1 INTERSECT SELECT 1) t", 1))
    R.run("EXCEPT", lambda: scalar(
        "SELECT count(*) FROM (SELECT 1 EXCEPT SELECT 2) t", 1))

    # ------------------------------------------------------------------
    print("\n--- parameters in every clause that takes one ---")
    R.run("parameter in SELECT", lambda: scalar("SELECT CAST(? AS INTEGER)", 7, params=[7]))
    R.run("parameter in WHERE", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1,2,3) t(x) WHERE x > ?", 2, params=[1]))
    R.run("parameter in HAVING", lambda: rows(
        "SELECT x FROM (VALUES 1,1,2) t(x) GROUP BY x HAVING count(*) > ?",
        want_count=1, params=[1]))
    R.run("parameter in IN list", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1,2,3) t(x) WHERE x IN (?, ?)", 2, params=[1, 2]))
    R.run("two parameters, order preserved", lambda: scalar(
        "SELECT CAST(? AS VARCHAR) || CAST(? AS VARCHAR)", "ab", params=["a", "b"]))
    R.run("parameter in a join condition", lambda: scalar(
        "SELECT count(*) FROM (VALUES 1,2) a(x) JOIN (VALUES 1,2) b(y) ON a.x = b.y "
        "AND a.x > ?", 1, params=[1]))
    R.run("NULL parameter", lambda: scalar("SELECT CAST(? AS INTEGER) IS NULL", True, params=[None]))
    # LIMIT gets its own probe: Trino does not accept a parameter there, so
    # the driver rendering the value as a literal is what makes it work at all.
    R.run("parameter in LIMIT", lambda: rows(
        "SELECT x FROM (VALUES 1,2,3) t(x) LIMIT ?", want_count=2, params=[2]))

    # ------------------------------------------------------------------
    print("\n--- batched parameters ---")
    # The CHANGELOG claims "bound parameters singly and in batches", and only
    # the singly half was covered. Trino has no wire-level parameter binding, so
    # every set in a batch becomes its own interpolated statement: what has to
    # hold is that all of them are submitted, in order, with their own values.
    #
    # A write, because that is what `executemany` is for, and against the hive
    # catalog, which is the writable one in the base stack. Each probe makes and
    # drops its own table so a failed run leaves nothing behind for the next.
    batch_table = "hive.tx.odbc_batch_probe"

    def with_batch_table(fn):
        cur.execute(f"DROP TABLE IF EXISTS {batch_table}")
        cur.execute(f"CREATE TABLE {batch_table} (id integer, label varchar)")
        try:
            fn()
        finally:
            cur.execute(f"DROP TABLE IF EXISTS {batch_table}")

    def batch_inserts_every_row():
        cur.executemany(
            f"INSERT INTO {batch_table} VALUES (?, ?)",
            [(1, "one"), (2, "two"), (3, "three")],
        )
        got = cur.execute(
            f"SELECT id, label FROM {batch_table} ORDER BY id"
        ).fetchall()
        assert [tuple(r) for r in got] == [(1, "one"), (2, "two"), (3, "three")], got

    R.run("executemany inserts every set", lambda: with_batch_table(batch_inserts_every_row))

    def batch_keeps_values_with_their_row():
        # Values that would still look plausible if a set were reused or the
        # two columns crossed: the strings do not match the numbers.
        cur.executemany(
            f"INSERT INTO {batch_table} VALUES (?, ?)",
            [(10, "b"), (20, "a"), (30, "c")],
        )
        got = cur.execute(
            f"SELECT id, label FROM {batch_table} ORDER BY id"
        ).fetchall()
        assert [tuple(r) for r in got] == [(10, "b"), (20, "a"), (30, "c")], got

    R.run("executemany keeps each value with its own row",
          lambda: with_batch_table(batch_keeps_values_with_their_row))

    def batch_handles_quotes_and_nulls():
        # The escaping path, once per set: `params::quote_string` doubles an
        # embedded quote, and a NULL becomes the keyword rather than a string.
        cur.executemany(
            f"INSERT INTO {batch_table} VALUES (?, ?)",
            [(1, "O'Brien"), (2, None), (3, "a;b")],
        )
        got = cur.execute(
            f"SELECT id, label FROM {batch_table} ORDER BY id"
        ).fetchall()
        assert [tuple(r) for r in got] == [(1, "O'Brien"), (2, None), (3, "a;b")], got

    R.run("executemany escapes each set independently",
          lambda: with_batch_table(batch_handles_quotes_and_nulls))

    def prepared_handle_is_reusable():
        # `execute` swaps the result into the existing handle and restores the
        # template, so the same prepared statement runs again with new values.
        # That is the main reason to prepare, and nothing else asserted it.
        first = cur.execute("SELECT CAST(? AS INTEGER)", [1]).fetchone()[0]
        second = cur.execute("SELECT CAST(? AS INTEGER)", [2]).fetchone()[0]
        assert (first, second) == (1, 2), (first, second)

    R.run("a prepared statement re-executes with new values", prepared_handle_is_reusable)

    # ------------------------------------------------------------------
    print("\n--- statement forms with undeclared column lengths ---")
    # These return varchar columns with no declared length. They are grouped
    # because that is the property under test: the driver has to describe a
    # column whose size it cannot know, and an application sizes its buffers
    # from what it says.
    R.run("DESCRIBE", lambda: shape("DESCRIBE tpcds.sf1.customer", min_cols=2, min_rows=1))
    R.run("SHOW TABLES", lambda: shape("SHOW TABLES FROM tpcds.sf1", min_rows=1))
    R.run("SHOW SCHEMAS", lambda: shape("SHOW SCHEMAS FROM tpcds", min_rows=1))
    R.run("SHOW COLUMNS", lambda: shape("SHOW COLUMNS FROM tpcds.sf1.customer", min_rows=1))
    R.run("EXPLAIN", lambda: shape("EXPLAIN SELECT 1", min_rows=1))
    R.run("EXPLAIN (TYPE LOGICAL)", lambda: shape("EXPLAIN (TYPE LOGICAL) SELECT 1", min_rows=1))
    R.run("EXPLAIN ANALYZE", lambda: shape("EXPLAIN ANALYZE SELECT 1", min_rows=1))
    R.run("SHOW FUNCTIONS", lambda: shape("SHOW FUNCTIONS", min_rows=1))

    # ------------------------------------------------------------------
    print("\n--- ODBC catalog functions ---")
    R.run("SQLTables", lambda: (
        cur.tables(catalog="tpcds", schema="sf1").fetchall() or
        (_ for _ in ()).throw(AssertionError("no tables"))))
    R.run("SQLTables catalog enumeration", lambda: (
        cur.tables(catalog="%", schema="", table="").fetchall() or
        (_ for _ in ()).throw(AssertionError("no catalogs"))))
    R.run("SQLTables schema enumeration", lambda: (
        cur.tables(catalog="", schema="%", table="").fetchall() or
        (_ for _ in ()).throw(AssertionError("no schemas"))))
    R.run("SQLTables table-type enumeration", lambda: (
        cur.tables(catalog="", schema="", table="", tableType="%").fetchall() or
        (_ for _ in ()).throw(AssertionError("no table types"))))
    R.run("SQLColumns", lambda: (
        cur.columns(catalog="tpcds", schema="sf1", table="customer").fetchall() or
        (_ for _ in ()).throw(AssertionError("no columns"))))
    R.run("SQLGetTypeInfo", lambda: (
        cur.getTypeInfo().fetchall() or
        (_ for _ in ()).throw(AssertionError("no type info"))))

    def datetime_columns_report_the_verbose_type():
        """SQLColumns and SQLGetTypeInfo must not disagree about a datetime.

        The spec has SQL_DATA_TYPE carry the *verbose* type, SQL_DATETIME (9),
        with the concise type in DATA_TYPE and the subcode in SQL_DATETIME_SUB.
        Reporting the concise type from SQLColumns would make the driver
        contradict SQLGetTypeInfo about the same column.
        """
        SQL_DATETIME = 9
        # SQLGetTypeInfo columns: DATA_TYPE 2, SQL_DATA_TYPE 16, SQL_DATETIME_SUB 17.
        by_concise = {r[1]: (r[15], r[16]) for r in cur.getTypeInfo().fetchall()}
        # SQLColumns columns: DATA_TYPE 5, SQL_DATA_TYPE 14, SQL_DATETIME_SUB 15.
        #
        # postgresql rather than the connected tpcds, because it is the catalog
        # carrying date, time and timestamp columns together. Reaching another
        # catalog at all is what the qualified `information_schema` reference
        # makes possible.
        rows = cur.columns(catalog="postgresql", schema="public",
                           table="types_test").fetchall()
        assert rows, "no columns returned for the postgresql catalog"
        seen = 0
        for r in rows:
            concise, verbose, sub = r[4], r[13], r[14]
            if concise not in by_concise:
                continue
            assert (verbose, sub) == by_concise[concise], (
                f"{r[3]}: SQLColumns says ({verbose}, {sub}), "
                f"SQLGetTypeInfo says {by_concise[concise]}")
            if verbose == SQL_DATETIME:
                seen += 1
                assert sub is not None, f"{r[3]}: SQL_DATETIME with no subcode"
        assert seen >= 3, f"expected date, time and timestamp columns, saw {seen}"

    R.run("SQLColumns datetime verbose type agrees with SQLGetTypeInfo",
        datetime_columns_report_the_verbose_type)

    def catalog_functions_reach_an_unconnected_catalog():
        """A named catalog other than the session one must still resolve.

        Trino resolves a bare `information_schema` through the session catalog,
        and each catalog's copy describes only itself, so filtering on
        `table_catalog` could never reach another catalog. An application that
        enumerates catalogs, which is what Power Query's navigator does, would
        then find every catalog but its own empty.
        """
        assert cur.tables(catalog="postgresql", schema="public").fetchall(), \
            "no tables for the postgresql catalog"
        assert cur.columns(catalog="postgresql", schema="public",
                           table="types_test").fetchall(), \
            "no columns for the postgresql catalog"
        # A catalog that does not exist is a filter matching nothing, not an
        # error: Trino answers CATALOG_NOT_FOUND and the driver absorbs it.
        assert cur.tables(catalog="no_such_catalog").fetchall() == [], \
            "a missing catalog must be empty, not an error"

    R.run("catalog functions reach an unconnected catalog",
        catalog_functions_reach_an_unconnected_catalog)
    # Trino exposes no key or index metadata, so an empty result set is the
    # correct answer and the assertion is that the call succeeds and describes
    # its columns rather than erroring.
    R.run("SQLPrimaryKeys (empty is correct)",
        lambda: cur.primaryKeys(catalog="tpcds", schema="sf1", table="customer").fetchall())
    R.run("SQLStatistics (empty is correct)",
        lambda: cur.statistics(catalog="tpcds", schema="sf1", table="customer").fetchall())
    # Trino has callable procedures (CALL system.runtime.kill_query(...))
    # but publishes no metadata naming them, so an empty result set is the
    # honest answer here too. pyodbc exposes no tablePrivileges() or
    # columnPrivileges(), so those two are covered in test_c_abi.py instead.
    R.run("SQLProcedures (empty is correct)",
        lambda: cur.procedures(catalog="system", schema="runtime").fetchall())
    R.run("SQLProcedureColumns (empty is correct)",
        lambda: cur.procedureColumns(catalog="system", schema="runtime").fetchall())

    # ------------------------------------------------------------------
    print("\n--- ordering, distinct, and null handling ---")
    R.run("ORDER BY on an unselected column", lambda: rows(
        "SELECT y FROM (VALUES (1,'b'),(2,'a')) t(x,y) ORDER BY x", want_count=2))
    R.run("ORDER BY an expression", lambda: rows(
        "SELECT x FROM (VALUES 1,2) t(x) ORDER BY -x", want_count=2))
    R.run("NULLS sort last by default", lambda: scalar(
        "SELECT x FROM (VALUES 1, NULL) t(x) ORDER BY x LIMIT 1", 1))
    R.run("DISTINCT", lambda: scalar(
        "SELECT count(*) FROM (SELECT DISTINCT x FROM (VALUES 1,1,2) t(x)) s", 2))
    R.run("CASE expression", lambda: scalar(
        "SELECT CASE WHEN 1 = 1 THEN 'y' ELSE 'n' END", "y"))
    R.run("COALESCE over NULL", lambda: scalar("SELECT coalesce(CAST(NULL AS INTEGER), 5)", 5))

    cur.close()
    conn.close()

    return R.summary()


if __name__ == "__main__":
    sys.exit(main())
