#!/usr/bin/env python3
"""
Power BI slicer semantics: does the filter Power BI folds select the value it showed?

A Power BI DirectQuery slicer shows each value the driver delivers, and when one is
picked it folds a filter built from that shown value. The filter only works if Trino,
evaluating it, arrives back at the same value. Whether it does depends on the
column's type, and for several types it silently does not: the filter runs, finds no
rows, and the report shows nothing, with no error anywhere.

This reproduces Power BI's filters through the driver and checks the row count. The
templates below are copied verbatim from the SQL Power BI Desktop sent to Trino on
2026-10-07 (DirectQuery, the Stackable connector, `hive.tx.interval_test`); the
expected count is computed here from the fetched rows, so no comparison inside Trino
is trusted to define the answer.

Everything runs twice, in the server's default session time zone and in
`Europe/Berlin`, because a timestamp-with-time-zone filter depends on it.

Usage:
    uv run --with pyodbc python3 integration-tests/suites/test_pbi_slicer_semantics.py "<connection-string>"

Requires a running Trino (integration-tests/setup.sh), whose seed-hive.sh creates the
`hive.tx.interval_test` view this reads. Needs no compose profile.
"""

import os
import sys

import pyodbc

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Results, Stack  # noqa: E402

R = Results("pbi slicer semantics")

VIEW = "hive.tx.interval_test"


def text(v):
    return v.replace("'", "''")


def fraction7(v):
    """Power BI renders seconds with seven fractional digits (100 ns ticks)."""
    return f"{v.microsecond:06d}0"


# column -> the WHERE clause Power BI folded for a slicer value `v`, as captured.
TEMPLATES = {
    "col_varchar": lambda v: f"\"col_varchar\" = CAST('{text(v)}' as VARCHAR)",
    "col_date": lambda v: f"\"col_date\" = CAST('{v:%Y-%m-%d}' as DATE)",
    "col_timestamp": lambda v: (
        f"\"col_timestamp\" = CAST('{v:%Y-%m-%d %H:%M:%S}.{fraction7(v)}' as TIMESTAMP)"
    ),
    "col_interval_ds": lambda v: (
        f"cast(\"col_interval_ds\" as VARCHAR) = CAST('{text(v)}' as VARCHAR)"
    ),
    "col_interval_ym": lambda v: (
        f"cast(\"col_interval_ym\" as VARCHAR) = CAST('{text(v)}' as VARCHAR)"
    ),
    # Power BI anchors a time of day on its base date, 1899-12-30, and folds
    # `cast("col_time" as TIMESTAMP) = CAST('1899-12-30 hh:mm:ss' as TIMESTAMP)`.
    # Trino's cast uses the current date, as ODBC's conversion tables do, so
    # that filter can never match. The connector therefore withholds
    # SQL_CVT_TIMESTAMP from SQL_CONVERT_TIME and Power BI refuses the fold with
    # a visible error; test_folding_contract.py checks that. Only "(Blank)" is
    # checked here.
    "col_time": None,
    "col_timestamptz": lambda v: (
        f"\"col_timestamptz\" = CAST('{v:%Y-%m-%d %H:%M:%S}.{fraction7(v)}' as TIMESTAMP)"
    ),
}


def check_zone(conn_str, zone_label):
    print(f"\n--- session time zone: {zone_label} ---")
    conn = pyodbc.connect(conn_str, autocommit=True)
    cur = conn.cursor()
    for col, template in TEMPLATES.items():
        pairs = cur.execute(f"SELECT id, {col} FROM {VIEW}").fetchall()
        values = []
        for _, x in pairs:
            if x is not None and x not in values:
                values.append(x)
        if template is None:
            values = []
        for v in values:
            expected = sum(1 for _, x in pairs if x == v)
            where = template(v)
            label = f"[{zone_label}] {col} = {v!r} selects its rows"
            try:
                got = cur.execute(f"SELECT count(*) FROM {VIEW} WHERE {where}").fetchone()[0]
                R.check(label, got == expected, "" if got == expected
                        else f"got {got}, expected {expected} (WHERE {where})")
            except pyodbc.Error as e:
                R.check(label, False, f"{str(e)[:120]} (WHERE {where})")
        # A slicer's "(Blank)" folds to `is null`; that already works and must keep working.
        nulls = sum(1 for _, x in pairs if x is None)
        got = cur.execute(f"SELECT count(*) FROM {VIEW} WHERE \"{col}\" is null").fetchone()[0]
        R.check(f"[{zone_label}] {col} (Blank) selects its rows", got == nulls,
                "" if got == nulls else f"got {got}, expected {nulls}")
    cur.close()
    conn.close()


def main():
    conn_str = sys.argv[1] if len(sys.argv) > 1 else Stack.load().conn_str()
    print(f"=== pbi slicer semantics ===\nview: {VIEW}")
    check_zone(conn_str, "server default")
    check_zone(conn_str + ";TimeZone=Europe/Berlin", "Europe/Berlin")
    return R.summary()


if __name__ == "__main__":
    sys.exit(main())
