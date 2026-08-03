#!/usr/bin/env python3
"""
BI stress tests for the Trino ODBC driver.

Exercises query patterns typical of PowerBI and similar BI tools: multi-table
JOINs, UNIONs, subqueries, CTEs, window functions, large result sets, and
wide rows. All queries are read-only against the tpcds sf1 catalogue.

Usage:
    python3 integration-tests/perf/test_stress.py "Driver=/path/to/driver.so;Host=localhost;Port=8080;User=admin;Protocol=http;Catalog=tpcds"
    python3 integration-tests/perf/test_stress.py "DSN=test_trino"

Requires a running Trino (integration-tests/setup.sh) and `pip install pyodbc`.
Needs no compose profile: the tpcds catalog is in the base stack.
"""

import os
import sys

import pyodbc

sys.path.insert(
    0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "suites")
)

from harness import Results, Stack  # noqa: E402

R = Results("stress")


def main():
    # A connection string may be passed positionally; with no argument the
    # local stack describes itself.
    conn_str = sys.argv[1] if len(sys.argv) > 1 else Stack.load().conn_str()
    conn = pyodbc.connect(conn_str, autocommit=True)
    cur = conn.cursor()

    # ------------------------------------------------------------------
    # 1. Multi-table JOINs
    # ------------------------------------------------------------------

    def test_two_table_join():
        cur.execute("""
            SELECT c.c_first_name, c.c_last_name, SUM(ss.ss_net_paid) AS total_spend
            FROM tpcds.sf1.customer c
            JOIN tpcds.sf1.store_sales ss ON c.c_customer_sk = ss.ss_customer_sk
            GROUP BY c.c_first_name, c.c_last_name
            ORDER BY total_spend DESC
            LIMIT 10
        """)
        rows = cur.fetchall()
        assert len(rows) == 10, f"expected 10 rows, got {len(rows)}"
        for row in rows:
            assert row[2] is not None and row[2] > 0, f"total_spend should be > 0, got {row[2]!r}"

    R.run("Two-table INNER JOIN with aggregation", test_two_table_join)

    def test_three_table_star_join():
        cur.execute("""
            SELECT c.c_first_name, i.i_product_name, SUM(ss.ss_quantity) AS total_qty
            FROM tpcds.sf1.store_sales ss
            JOIN tpcds.sf1.customer c ON ss.ss_customer_sk = c.c_customer_sk
            JOIN tpcds.sf1.item i ON ss.ss_item_sk = i.i_item_sk
            GROUP BY c.c_first_name, i.i_product_name
            ORDER BY total_qty DESC
            LIMIT 10
        """)
        rows = cur.fetchall()
        assert len(rows) == 10, f"expected 10 rows, got {len(rows)}"
        # TPC-DS data can have NULLs in name columns; the test exercises the
        # three-table JOIN path, not NULL handling (that's test 5a).
        for row in rows:
            assert row[2] is not None, "total_qty should not be NULL"

    R.run("Three-table star-schema JOIN", test_three_table_star_join)

    def test_left_join_nulls():
        cur.execute("""
            SELECT c.c_customer_sk, c.c_first_name, ws.ws_order_number
            FROM tpcds.sf1.customer c
            LEFT JOIN tpcds.sf1.web_sales ws ON c.c_customer_sk = ws.ws_bill_customer_sk
            WHERE c.c_customer_sk BETWEEN 1 AND 20
            ORDER BY c.c_customer_sk
        """)
        rows = cur.fetchall()
        assert len(rows) >= 20, f"expected >= 20 rows, got {len(rows)}"
        has_null = any(row[2] is None for row in rows)
        assert has_null, "expected at least one NULL ws_order_number from LEFT JOIN"

    R.run("LEFT JOIN producing NULLs", test_left_join_nulls)

    # ------------------------------------------------------------------
    # 2. Subqueries and CTEs
    # ------------------------------------------------------------------

    def test_correlated_subquery():
        cur.execute("""
            SELECT c_customer_sk, c_first_name
            FROM tpcds.sf1.customer c
            WHERE c_customer_sk IN (
                SELECT ss_customer_sk
                FROM tpcds.sf1.store_sales
                WHERE ss_net_paid > 100
            )
            LIMIT 10
        """)
        rows = cur.fetchall()
        assert len(rows) == 10, f"expected 10 rows, got {len(rows)}"
        for row in rows:
            assert row[0] > 0, f"c_customer_sk should be > 0, got {row[0]}"

    R.run("Correlated subquery", test_correlated_subquery)

    def test_cte():
        cur.execute("""
            WITH top_items AS (
                SELECT ss_item_sk, SUM(ss_quantity) AS total_qty
                FROM tpcds.sf1.store_sales
                GROUP BY ss_item_sk
                ORDER BY total_qty DESC
                LIMIT 5
            )
            SELECT i.i_product_name, t.total_qty
            FROM top_items t
            JOIN tpcds.sf1.item i ON t.ss_item_sk = i.i_item_sk
        """)
        rows = cur.fetchall()
        assert len(rows) == 5, f"expected 5 rows, got {len(rows)}"
        for row in rows:
            assert row[1] is not None and row[1] > 0, f"total_qty should be > 0, got {row[1]!r}"

    R.run("CTE / WITH clause", test_cte)

    # ------------------------------------------------------------------
    # 3. UNION
    # ------------------------------------------------------------------

    def test_union_all():
        cur.execute("""
            SELECT 'store' AS channel, ss_customer_sk AS customer_sk, ss_net_paid AS amount
            FROM tpcds.sf1.store_sales
            WHERE ss_customer_sk BETWEEN 1 AND 5
            UNION ALL
            SELECT 'web', ws_bill_customer_sk, ws_net_paid
            FROM tpcds.sf1.web_sales
            WHERE ws_bill_customer_sk BETWEEN 1 AND 5
        """)
        rows = cur.fetchall()
        assert len(rows) > 0, "expected rows from UNION ALL"
        channels = {row[0].strip() for row in rows}
        assert "store" in channels, f"expected 'store' channel, got {channels}"
        assert "web" in channels, f"expected 'web' channel, got {channels}"

    R.run("UNION ALL across sales channels", test_union_all)

    def test_union_dedup():
        cur.execute("""
            SELECT ss_customer_sk AS customer_sk
            FROM tpcds.sf1.store_sales WHERE ss_customer_sk BETWEEN 1 AND 10
            UNION
            SELECT ws_bill_customer_sk
            FROM tpcds.sf1.web_sales WHERE ws_bill_customer_sk BETWEEN 1 AND 10
        """)
        rows = cur.fetchall()
        assert len(rows) <= 10, f"expected <= 10 deduped rows, got {len(rows)}"
        for row in rows:
            assert 1 <= row[0] <= 10, f"customer_sk {row[0]} out of range [1, 10]"

    R.run("UNION with dedup", test_union_dedup)

    # ------------------------------------------------------------------
    # 4. Large result sets
    # ------------------------------------------------------------------

    def test_large_result_set():
        cur.execute("""
            SELECT c_customer_sk, c_first_name, c_last_name, c_birth_year
            FROM tpcds.sf1.customer
            WHERE c_customer_sk <= 15000
        """)
        rows = cur.fetchall()
        assert len(rows) >= 10000, f"expected >= 10000 rows, got {len(rows)}"

    R.run("Fetch 10,000+ rows", test_large_result_set)

    def test_wide_result_set():
        cur.execute("""
            SELECT
                c.c_customer_sk, c.c_first_name, c.c_last_name,
                c.c_birth_year, c.c_birth_month, c.c_birth_country,
                c.c_email_address, c.c_login,
                ss.ss_quantity, ss.ss_net_paid, ss.ss_net_profit
            FROM tpcds.sf1.store_sales ss
            JOIN tpcds.sf1.customer c ON ss.ss_customer_sk = c.c_customer_sk
            WHERE c.c_customer_sk = 1
        """)
        rows = cur.fetchall()
        assert len(rows) >= 1, f"expected >= 1 row, got {len(rows)}"
        assert cur.description is not None
        col_names = [d[0] for d in cur.description]
        assert len(col_names) == 11, f"expected 11 columns, got {len(col_names)}"

    R.run("Wide result set (11 columns)", test_wide_result_set)

    # ------------------------------------------------------------------
    # 5. NULL and edge cases
    # ------------------------------------------------------------------

    def test_nulls_in_various_positions():
        cur.execute("""
            SELECT c_customer_sk, c_email_address, c_birth_country, c_login
            FROM tpcds.sf1.customer
            WHERE c_customer_sk BETWEEN 1 AND 50
        """)
        rows = cur.fetchall()
        assert len(rows) >= 50, f"expected >= 50 rows, got {len(rows)}"
        has_none = any(
            any(col is None for col in row[1:])
            for row in rows
        )
        assert has_none, "expected at least one NULL in nullable columns"

    R.run("NULLs in various column positions", test_nulls_in_various_positions)

    def test_empty_result_set():
        cur.execute("SELECT c_customer_sk FROM tpcds.sf1.customer WHERE 1 = 0")
        rows = cur.fetchall()
        assert len(rows) == 0, f"expected 0 rows, got {len(rows)}"

    R.run("Empty result set", test_empty_result_set)

    # ------------------------------------------------------------------
    # 6. Complex aggregation
    # ------------------------------------------------------------------

    def test_group_by_having_on_join():
        cur.execute("""
            SELECT c.c_first_name, COUNT(*) AS purchase_count, SUM(ss.ss_net_paid) AS total
            FROM tpcds.sf1.store_sales ss
            JOIN tpcds.sf1.customer c ON ss.ss_customer_sk = c.c_customer_sk
            GROUP BY c.c_first_name
            HAVING COUNT(*) > 100
            ORDER BY total DESC
            LIMIT 10
        """)
        rows = cur.fetchall()
        assert len(rows) == 10, f"expected 10 rows, got {len(rows)}"
        for row in rows:
            assert row[1] > 100, f"purchase_count should be > 100, got {row[1]}"
            assert row[2] is not None and row[2] > 0, f"total should be > 0, got {row[2]!r}"

    R.run("GROUP BY + HAVING on a JOIN", test_group_by_having_on_join)

    def test_window_function():
        cur.execute("""
            SELECT c_customer_sk, c_first_name, c_birth_year,
                ROW_NUMBER() OVER (PARTITION BY c_birth_year ORDER BY c_customer_sk) AS rn
            FROM tpcds.sf1.customer
            WHERE c_birth_year IS NOT NULL AND c_customer_sk <= 1000
        """)
        rows = cur.fetchall()
        assert len(rows) > 0, "expected rows"
        for row in rows:
            assert row[3] is not None and row[3] > 0, f"rn should be a positive integer, got {row[3]!r}"

    R.run("Window function (ROW_NUMBER OVER PARTITION BY)", test_window_function)

    # === Cleanup ===
    conn.close()

    # === Summary ===
    sys.exit(R.summary())


if __name__ == "__main__":
    main()
