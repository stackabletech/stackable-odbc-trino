#!/usr/bin/env python3
"""ODBC manual-commit transactions, through unixODBC.

Needs no profile: the `hive` catalog is in the base stack.

Every scenario that writes names `hive`. It is the only connector Trino ships
that accepts a write outside autocommit. The coordinator gates that on the SPI's
`Connector.isSingleStatementWritesOnly()`, and `tpcds` and `postgresql` answer
`AUTOCOMMIT_WRITE_CONFLICT`. See the hive catalog section in `AGENTS.md`.

Three measured Trino behaviours shape what is asserted here, and each is the
reason a scenario looks the way it does rather than the obvious way:

  - **Any statement error aborts the whole transaction**, and Trino then
    refuses everything including `COMMIT`. So the failed-statement scenario
    expects `SQLEndTran(SQL_COMMIT)` to *fail*, and expects the connection to
    keep working afterwards.
  - **Two inserts into the same unpartitioned Hive table in one transaction
    fail** (`Inserting into an unpartitioned table that were added, altered,
    or inserted into in the same transaction is not supported`). The
    multi-statement scenario therefore writes to two tables, which is also the
    stronger atomicity claim.
  - **A table written in a transaction cannot be read back before the commit**
    (`NOT_SUPPORTED: Cannot read from a table ... that was modified within
    transaction`). Row counts are taken from a second connection, after the
    transaction ends.
"""

import os
import sys
import time
import uuid

import pyodbc

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Results, Stack  # noqa: E402

# pyodbc enables ODBC connection pooling by default, and a pooled connection is
# handed back to the application without the driver being reconnected, so it
# arrives still carrying whatever commit mode the previous borrower left on it.
# Measured here: a dozen pyodbc connections produced two `TrinoBackend::connect`
# calls and no `disconnect` at all, and a `CREATE TABLE` on a "fresh" connection
# ran inside the previous borrower's manual-commit mode and was discarded,
# reporting success.
#
# Turned off so this suite measures the driver rather than the Driver Manager's
# pooling. The hazard itself is real and is recorded in AGENTS.md.
pyodbc.pooling = False

SCHEMA = "hive.tx"

# pyodbc exposes neither of these, so they are spelled out rather than taken
# from it: `SQL_ATTR_TXN_ISOLATION` and the level the driver must refuse.
SQL_ATTR_TXN_ISOLATION = 108
SQL_TXN_SERIALIZABLE = 8


def scenario(results, label, fn):
    """Run one scenario, recording an exception as a single failure.

    `Results.run` is not used because each scenario does its own `check`
    accounting, and a wrapper PASS printed beside an inner FAIL reads as though
    something passed."""
    start = time.monotonic()
    try:
        fn()
    except Exception as e:  # noqa: BLE001
        results.bad(label, f"raised after {time.monotonic() - start:.1f}s: {e}")
    else:
        print(f"      {label}: {time.monotonic() - start:.1f}s")


def unique_table(prefix):
    """A table name of this run's own, so a suite left half-finished by an
    earlier failure cannot make the next run pass or fail for the wrong
    reason."""
    return f"{SCHEMA}.{prefix}_{uuid.uuid4().hex[:8]}"


def make_table(conn, table):
    conn.cursor().execute(f"CREATE TABLE {table} (id integer)")


def drop_table(conn, table):
    try:
        conn.cursor().execute(f"DROP TABLE IF EXISTS {table}")
    except Exception:  # noqa: BLE001
        # Cleanup only. A failure here must not mask the scenario's own result.
        pass


def count_rows(stack, table):
    """Count from a *fresh* connection, which is what makes a commit or a
    rollback observable rather than merely reported."""
    with stack.connect() as conn:
        return conn.cursor().execute(f"SELECT count(*) FROM {table}").fetchone()[0]


def a_rollback_discards_a_write(stack, results):
    table = unique_table("rollback")
    with stack.connect() as setup:
        make_table(setup, table)
    try:
        conn = stack.connect()
        conn.autocommit = False
        conn.cursor().execute(f"INSERT INTO {table} VALUES (1)")
        conn.rollback()
        conn.close()

        results.check(
            "a rolled-back insert is not there",
            count_rows(stack, table) == 0,
            f"count is {count_rows(stack, table)}",
        )
    finally:
        with stack.connect() as cleanup:
            drop_table(cleanup, table)


def a_commit_publishes_a_write(stack, results):
    table = unique_table("commit")
    with stack.connect() as setup:
        make_table(setup, table)
    try:
        conn = stack.connect()
        conn.autocommit = False
        conn.cursor().execute(f"INSERT INTO {table} VALUES (1)")
        conn.commit()
        conn.close()

        results.check(
            "a committed insert is visible to another connection",
            count_rows(stack, table) == 1,
            f"count is {count_rows(stack, table)}",
        )
    finally:
        with stack.connect() as cleanup:
            drop_table(cleanup, table)


def a_commit_spanning_two_tables_is_atomic(stack, results):
    """Two tables, not two inserts into one: Hive refuses a second insert into
    the same unpartitioned table inside one transaction."""
    first, second = unique_table("atomic_a"), unique_table("atomic_b")
    with stack.connect() as setup:
        make_table(setup, first)
        make_table(setup, second)
    try:
        conn = stack.connect()
        conn.autocommit = False
        conn.cursor().execute(f"INSERT INTO {first} VALUES (1)")
        conn.cursor().execute(f"INSERT INTO {second} VALUES (2)")
        conn.commit()
        conn.close()

        results.check(
            "both tables carry the commit",
            count_rows(stack, first) == 1 and count_rows(stack, second) == 1,
            f"{count_rows(stack, first)} and {count_rows(stack, second)}",
        )
    finally:
        with stack.connect() as cleanup:
            drop_table(cleanup, first)
            drop_table(cleanup, second)


def a_failed_statement_aborts_the_transaction(stack, results):
    """Trino aborts the whole transaction on any statement error and then
    refuses the commit, so the driver rolls back and reports the failure.

    Reporting success would tell an application its writes landed when they
    were discarded, and the connection has to survive: the SQLSTATE is `25S03`
    precisely so the Driver Manager does not suspend it."""
    table = unique_table("aborted")
    with stack.connect() as setup:
        make_table(setup, table)
    try:
        conn = stack.connect()
        conn.autocommit = False
        conn.cursor().execute(f"INSERT INTO {table} VALUES (1)")

        try:
            conn.cursor().execute("SELECT 1/0").fetchall()
            results.bad("division by zero fails", "it succeeded")
            return
        except Exception:  # noqa: BLE001
            pass

        committed = None
        try:
            conn.commit()
            committed = True
        except Exception as e:  # noqa: BLE001
            committed = False
            state = getattr(e, "args", ["", ""])[0]
            results.check(
                "committing an aborted transaction reports 25S03",
                state == "25S03",
                f"SQLSTATE {state}",
            )
        if committed:
            results.bad(
                "committing an aborted transaction fails",
                "it reported success, so the application believes writes landed",
            )

        results.check(
            "the discarded insert is not there",
            count_rows(stack, table) == 0,
            f"count is {count_rows(stack, table)}",
        )

        # Why the driver rolls back rather than leaving the session wedged:
        # Trino refuses every statement on an aborted transaction.
        conn.autocommit = True
        value = conn.cursor().execute("SELECT 1").fetchone()[0]
        results.check("the connection still works", value == 1, f"got {value}")
        conn.close()
    finally:
        with stack.connect() as cleanup:
            drop_table(cleanup, table)


def a_commit_closes_an_open_cursor(stack, results):
    """`SQL_CURSOR_COMMIT_BEHAVIOR` is `SQL_CB_CLOSE`, from the application's
    side. Trino discards a transaction's result sets when it ends, so fetching
    on afterwards must not quietly return more rows."""
    conn = stack.connect()
    conn.autocommit = False
    cursor = conn.cursor()
    cursor.execute("SELECT c_customer_sk FROM tpcds.sf1.customer ORDER BY 1")
    first = cursor.fetchone()
    results.check("the cursor produced a row before the commit", first is not None)

    conn.commit()

    try:
        row = cursor.fetchone()
    except Exception as e:  # noqa: BLE001
        results.ok("fetching after the commit is refused", f"{type(e).__name__}")
    else:
        results.check(
            "the cursor was closed by the commit",
            row is None,
            "it returned another row, so the cursor outlived its transaction",
        )
    conn.autocommit = True
    conn.close()


def autocommit_is_the_default(stack, results):
    """No explicit transaction, and the write is visible elsewhere with no
    commit, which is what ODBC's default commit mode means."""
    table = unique_table("autocommit")
    with stack.connect() as setup:
        make_table(setup, table)
    try:
        with stack.connect() as conn:
            conn.cursor().execute(f"INSERT INTO {table} VALUES (1)")
        results.check(
            "an autocommit write needs no commit",
            count_rows(stack, table) == 1,
            f"count is {count_rows(stack, table)}",
        )
    finally:
        with stack.connect() as cleanup:
            drop_table(cleanup, table)


def an_unsupported_isolation_level_is_refused_by_the_driver(stack, results):
    """`SQL_TXN_ISOLATION_OPTION` advertises only `SQL_TXN_READ_UNCOMMITTED`,
    so core rejects the rest with `HY024` before they reach the wire.

    The alternative would be advertising Trino's whole grammar and letting the
    *connector* reject the level on the first statement that touches a catalog,
    which reaches the application as a mysterious failed query rather than as a
    refused attribute."""
    conn = stack.connect()
    try:
        conn.set_attr(SQL_ATTR_TXN_ISOLATION, SQL_TXN_SERIALIZABLE)
    except Exception as e:  # noqa: BLE001
        state = getattr(e, "args", ["", ""])[0]
        results.check(
            "SQL_TXN_SERIALIZABLE is refused with HY024",
            state == "HY024",
            f"SQLSTATE {state}",
        )
    else:
        results.bad(
            "SQL_TXN_SERIALIZABLE is refused",
            "it was accepted, but no Trino connector honours it",
        )
    finally:
        conn.close()


def main():
    stack = Stack.load()
    results = Results("transactions")

    for label, fn in (
        ("a rollback discards a write", a_rollback_discards_a_write),
        ("a commit publishes a write", a_commit_publishes_a_write),
        ("a commit spanning two tables is atomic", a_commit_spanning_two_tables_is_atomic),
        ("a failed statement aborts the transaction", a_failed_statement_aborts_the_transaction),
        ("a commit closes an open cursor", a_commit_closes_an_open_cursor),
        ("autocommit is the default", autocommit_is_the_default),
        (
            "an unsupported isolation level is refused",
            an_unsupported_isolation_level_is_refused_by_the_driver,
        ),
    ):
        scenario(results, label, lambda fn=fn: fn(stack, results))

    sys.exit(results.summary())


if __name__ == "__main__":
    main()
