#!/usr/bin/env python3
"""
Parse ODBC driver profiling logs and produce a per-query summary table.

The driver emits structured tracing output when ODBC_PROFILING=1:
  - "trino server stats" lines with Trino-side timing per page
  - "query profiling summary" lines with per-query aggregates
  - Span close events with time.busy for trino.submit, trino.fetch_page, etc.

This script reads the log file and produces a summary table showing where
time is spent for each query.

Usage:
    python3 parse_profile.py /tmp/odbc_profile.log
"""

import re
import sys


def parse_kv(line: str) -> dict[str, str]:
    """Extract key=value pairs from a tracing log line.

    Handles both unquoted values (key=123) and quoted values (key="some string").
    """
    pairs = {}
    for m in re.finditer(r'(\w+)=(?:"([^"]*)"|(\S+))', line):
        key = m.group(1)
        val = m.group(2) if m.group(2) is not None else m.group(3)
        pairs[key] = val
    return pairs


class QueryProfile:
    """Accumulates profiling data for a single query."""

    def __init__(self, query_id: str):
        self.query_id = query_id
        self.pages = 0
        self.empty_pages = 0
        self.total_rows = 0
        self.fetch_ms = 0.0
        self.convert_ms = 0.0
        self.trino_elapsed_ms = 0.0
        self.trino_cpu_ms = 0.0
        self.trino_queued_ms = 0.0
        self.trino_rows = 0
        self.trino_bytes = 0
        self.trino_peak_mem = 0
        self.trino_state = ""


def main():
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <log-file>")
        sys.exit(2)

    log_path = sys.argv[1]

    try:
        with open(log_path) as f:
            lines = f.readlines()
    except FileNotFoundError:
        print(f"Error: log file not found: {log_path}")
        sys.exit(1)

    if not lines:
        print("Log file is empty. Did the stress test run with ODBC_PROFILING=1?")
        sys.exit(1)

    # Process lines sequentially. The last "trino server stats" line before a
    # "query profiling summary" line contains the final cumulative Trino stats
    # for that query.
    queries: list[QueryProfile] = []
    last_stats: dict[str, str] = {}

    for line in lines:
        if "trino server stats" in line:
            last_stats = parse_kv(line)

        elif "query profiling summary" in line:
            kv = parse_kv(line)
            qid = kv.get("query_id", "unknown")
            # Clean up the query_id from tracing's Debug format: Some("...") -> ...
            qid = qid.replace("Some(", "").rstrip(")").strip('"')

            q = QueryProfile(qid)
            q.pages = int(kv.get("pages", 0))
            q.empty_pages = int(kv.get("empty_pages", 0))
            q.total_rows = int(kv.get("total_rows", 0))
            q.fetch_ms = float(kv.get("fetch_ms", 0))
            q.convert_ms = float(kv.get("convert_ms", 0))

            # Apply the last seen Trino stats (cumulative final values)
            if last_stats:
                q.trino_elapsed_ms = float(last_stats.get("trino_elapsed_ms", 0))
                q.trino_cpu_ms = float(last_stats.get("trino_cpu_ms", 0))
                q.trino_queued_ms = float(last_stats.get("trino_queued_ms", 0))
                q.trino_rows = int(last_stats.get("trino_rows", 0))
                q.trino_bytes = int(last_stats.get("trino_bytes", 0))
                q.trino_peak_mem = int(last_stats.get("trino_peak_mem", 0))
                q.trino_state = last_stats.get("trino_state", "")

            queries.append(q)
            last_stats = {}

    if not queries:
        print("No query profiling summaries found in the log.")
        print("Ensure the driver was built with the profiling instrumentation")
        print("and ODBC_LOG_LEVEL=info ODBC_PROFILING=1 were set.")
        sys.exit(1)

    # Print summary table.
    hdr = (f"{'Query ID':<24} | {'Pages':>5} | {'Empty':>5} | {'Rows':>8} | "
           f"{'Trino ms':>9} | {'Fetch ms':>9} | {'Cvt ms':>6} | "
           f"{'Overhead':>9} | {'Rows/s':>8} | {'Trino state':<10}")
    sep = "-" * len(hdr)
    print(hdr)
    print(sep)

    total_fetch = 0.0
    total_convert = 0.0
    total_trino = 0.0
    total_rows = 0
    total_empty = 0

    for q in queries:
        overhead_ms = q.fetch_ms - q.trino_elapsed_ms
        if q.fetch_ms > 0:
            rows_per_sec = int(q.total_rows / (q.fetch_ms / 1000.0))
        else:
            rows_per_sec = 0

        display_id = q.query_id[-22:] if len(q.query_id) > 24 else q.query_id

        print(f"{display_id:<24} | {q.pages:>5} | {q.empty_pages:>5} | "
              f"{q.total_rows:>8} | {q.trino_elapsed_ms:>9.0f} | "
              f"{q.fetch_ms:>9.0f} | {q.convert_ms:>6.0f} | "
              f"{overhead_ms:>9.0f} | {rows_per_sec:>8} | {q.trino_state:<10}")

        total_fetch += q.fetch_ms
        total_convert += q.convert_ms
        total_trino += q.trino_elapsed_ms
        total_rows += q.total_rows
        total_empty += q.empty_pages

    print()
    total_overhead = total_fetch - total_trino
    print(f"Total: {len(queries)} queries, {total_rows} rows, "
          f"{total_empty} empty pages across all queries")
    print(f"  Trino server time:  {total_trino:>8.0f} ms")
    print(f"  HTTP fetch time:    {total_fetch:>8.0f} ms")
    print(f"  Row conversion:     {total_convert:>8.0f} ms")
    print(f"  Client overhead:    {total_overhead:>8.0f} ms "
          f"(HTTP round-trip + JSON deserialisation + empty page polling)")
    if total_fetch > 0:
        pct = (total_overhead / total_fetch) * 100
        print(f"  Overhead fraction:  {pct:>7.1f}%")

    # Breakdown by category
    print()
    print("Breakdown:")
    if total_fetch > 0:
        print(f"  Trino execution:    {total_trino / total_fetch * 100:>5.1f}% of fetch time")
        print(f"  Client overhead:    {abs(total_overhead) / total_fetch * 100:>5.1f}% of fetch time"
              f" (HTTP round-trips + JSON deserialisation)")
        print(f"  Row conversion:     {total_convert / total_fetch * 100:>5.1f}% of fetch time"
              f" (row.clone() + json_to_column_value)")
    if total_empty > 0:
        avg_empty = total_empty / len(queries)
        print(f"  Empty pages:        {total_empty} total"
              f" (avg {avg_empty:.1f}/query): Trino REST polling while server works")


if __name__ == "__main__":
    main()
