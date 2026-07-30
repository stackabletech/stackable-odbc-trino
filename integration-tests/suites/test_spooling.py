#!/usr/bin/env python3
"""Trino's spooled protocol, through the driver's `Encoding` key.

Requires the `spooling` profile for the scenarios that need the coordinator to
spool: `./integration-tests/setup.sh --profile spooling`. The fallback scenario
is the other way round and runs when the profile is *off*, since a coordinator
with no spooling manager is what most deployments are.

Measured thresholds this suite depends on (2026-07-30, against the profile's
`initial-segment-size=16kB` / `max-segment-size=64kB` and Trino's default
inlining of the first 1000 rows or 128kB):

    SELECT 1                    -> 1 inline segment, 0 spooled
    900 rows of customer        -> 1 inline segment, 0 spooled
    20,000 rows of customer     -> 2 inline, 25 spooled

So a suite querying under ~1000 rows passes without a single segment being
fetched from object storage, which is why the scenarios below read the driver's
log rather than trusting a row count.
"""

import os
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Results, Stack  # noqa: E402

# The client logs this once per *remote* segment, in
# `trino-rust-client/src/spooling/fetcher.rs`. An inline segment never produces
# one, which is exactly the distinction the thresholds above turn on.
SEGMENT_LOG_LINE = "Successfully fetched remote spooled segment"

# 20,000 rows is over the inlining threshold with room to spare: measured at 25
# spooled segments. Ordered so two runs' results are comparable row by row.
BIG_QUERY = (
    "SELECT c_customer_sk, c_customer_id, c_first_name, c_last_name "
    "FROM tpcds.sf1.customer ORDER BY c_customer_sk LIMIT 20000"
)
BIG_QUERY_ROWS = 20000


def scenario(results, label, fn):
    """Run one scenario, recording an exception as a single failure.

    `Results.run` is not used because each scenario does its own `check`
    accounting, and a wrapper PASS printed beside an inner FAIL reads as though
    something passed. The elapsed time is still printed: a suite that slows down
    is a finding."""
    start = time.monotonic()
    try:
        fn()
    except Exception as e:  # noqa: BLE001
        results.bad(label, f"raised after {time.monotonic() - start:.1f}s: {e}")
    else:
        print(f"      {label}: {time.monotonic() - start:.1f}s")


def run_query(stack, log_path, sql, **overrides):
    """Run `sql` on a fresh connection with the driver logging to `log_path`,
    and return (rows, log text).

    The log environment is set before pyodbc loads the driver, because core
    reads it once when the library is loaded."""
    os.environ["ODBC_LOG_LEVEL"] = "info"
    os.environ["ODBC_LOG_FILE"] = log_path
    import pyodbc

    conn = pyodbc.connect(stack.conn_str(**overrides), autocommit=True)
    try:
        cur = conn.cursor()
        cur.execute(sql)
        rows = [tuple(r) for r in cur.fetchall()]
    finally:
        conn.close()

    log = ""
    if os.path.exists(log_path):
        with open(log_path, encoding="utf-8", errors="replace") as f:
            log = f.read()
    return rows, log


def spooled_matches_direct(stack, results):
    """A spooled result set equals the inline one, and segments were really
    fetched. Without the log check this would pass on an inlined result and
    prove nothing."""
    with tempfile.TemporaryDirectory() as d:
        direct, direct_log = run_query(stack, os.path.join(d, "direct.log"), BIG_QUERY)
        spooled, spooled_log = run_query(
            stack, os.path.join(d, "spooled.log"), BIG_QUERY, Encoding="json+zstd"
        )

    results.check(
        "spooled row count matches direct",
        len(spooled) == len(direct) == BIG_QUERY_ROWS,
        f"direct={len(direct)} spooled={len(spooled)}",
    )
    results.check("spooled rows are identical to direct", spooled == direct)
    fetched = spooled_log.count(SEGMENT_LOG_LINE)
    results.check(
        "the driver fetched remote spooled segments",
        fetched > 0,
        f"{fetched} segment fetches logged; 0 would mean the result was inlined "
        f"and this scenario proved nothing",
    )
    results.check(
        "the direct run fetched no segment",
        direct_log.count(SEGMENT_LOG_LINE) == 0,
        "no Encoding key was set, so nothing may be spooled",
    )


def an_inline_segment_decodes(stack, results):
    """With an encoding set, a small result arrives as one *inline* segment.
    That is a different decode path from a remote fetch, and the one every short
    query takes once spooling is on."""
    with tempfile.TemporaryDirectory() as d:
        rows, log = run_query(
            stack, os.path.join(d, "inline.log"), "SELECT 1", Encoding="json+zstd"
        )

    results.check("an inline segment decodes", rows == [(1,)], f"got {rows}")
    results.check(
        "no remote segment was fetched for a one-row result",
        log.count(SEGMENT_LOG_LINE) == 0,
        "Trino inlines the first 1000 rows, so a remote fetch here would mean "
        "the inlining threshold moved",
    )


def every_encoding_agrees(stack, results):
    """The three encodings are three wire formats for one result."""
    with tempfile.TemporaryDirectory() as d:
        baseline, _ = run_query(stack, os.path.join(d, "direct.log"), BIG_QUERY)
        for encoding in ("json", "json+zstd", "json+lz4"):
            log_name = encoding.replace("+", "_") + ".log"
            rows, log = run_query(
                stack, os.path.join(d, log_name), BIG_QUERY, Encoding=encoding
            )
            results.check(
                f"{encoding} returns the direct result",
                rows == baseline,
                f"{len(rows)} rows, {log.count(SEGMENT_LOG_LINE)} segments fetched",
            )


def a_coordinator_without_spooling_falls_back(stack, results):
    """Most coordinators have no spooling manager configured. The header is then
    ignored and the rows arrive inline, so setting `Encoding` must not fail the
    query."""
    with tempfile.TemporaryDirectory() as d:
        rows, log = run_query(
            stack, os.path.join(d, "fallback.log"), BIG_QUERY, Encoding="json+zstd"
        )

    results.check(
        "rows arrive inline when the coordinator does not spool",
        len(rows) == BIG_QUERY_ROWS,
        f"{len(rows)} rows",
    )
    results.check(
        "no segment was fetched",
        log.count(SEGMENT_LOG_LINE) == 0,
        "a segment fetch here would mean the coordinator did spool, so this run "
        "is not the fallback case it claims to be",
    )


def cancel_during_a_spooled_fetch_reports_hy008(stack, results):
    """SQLCancel from another thread, while the fetch is downloading segments.

    HY008 is the spec's code for a function interrupted by SQLCancel, and it
    needs `Threading = 2` in odbcinst.ini, which setup.sh writes."""
    import threading
    import time

    import pyodbc

    conn = pyodbc.connect(stack.conn_str(Encoding="json+zstd"), autocommit=True)
    try:
        cur = conn.cursor()
        cur.execute(
            "SELECT c_customer_sk, c_customer_id, c_first_name, c_last_name "
            "FROM tpcds.sf10.customer ORDER BY c_customer_sk"
        )

        def cancel_after_two_seconds():
            time.sleep(2)
            cur.cancel()

        canceller = threading.Thread(target=cancel_after_two_seconds)
        canceller.start()
        start = time.monotonic()
        try:
            cur.fetchall()
            results.bad(
                "cancel during a spooled fetch reports HY008",
                "the fetch completed; the query was too small to still be "
                "running after 2s",
            )
        except pyodbc.Error as e:
            elapsed = time.monotonic() - start
            state = e.args[0]
            results.check(
                "cancel during a spooled fetch reports HY008",
                state == "HY008",
                f"got {state} after {elapsed:.1f}s: {e}",
            )
        finally:
            canceller.join()
    finally:
        conn.close()


def main():
    stack = Stack.load()
    results = Results("spooling")
    absent = "profile 'spooling' is not active (setup.sh --profile spooling)"

    if stack.has_profile("spooling"):
        scenario(
            results,
            "spooled result equals direct",
            lambda: spooled_matches_direct(stack, results),
        )
        scenario(
            results,
            "inline segment decodes",
            lambda: an_inline_segment_decodes(stack, results),
        )
        scenario(
            results, "every encoding agrees", lambda: every_encoding_agrees(stack, results)
        )
        scenario(
            results,
            "cancel during a spooled fetch",
            lambda: cancel_during_a_spooled_fetch_reports_hy008(stack, results),
        )
        results.skip(
            "coordinator without spooling falls back",
            "the 'spooling' profile IS active, so this coordinator spools; run "
            "setup.sh without --profile spooling for the fallback case",
        )
    else:
        for label in (
            "spooled result equals direct",
            "inline segment decodes",
            "every encoding agrees",
            "cancel during a spooled fetch",
        ):
            results.skip(label, absent)
        scenario(
            results,
            "coordinator without spooling falls back",
            lambda: a_coordinator_without_spooling_falls_back(stack, results),
        )

    sys.exit(results.summary())


if __name__ == "__main__":
    main()
