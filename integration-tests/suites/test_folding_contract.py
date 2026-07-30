#!/usr/bin/env python3
"""
Folding contract test: the Power Query connector against the driver and Trino.

The connector tells Power Query how to render SQL -- which constants it can
cast, what a LIMIT clause looks like, which capabilities to assume. None of
that is exercised by any other suite: the pyodbc, C ABI and surface tests drive
the driver directly and never load the `.mez`, and the only documented check on
folding is a human clicking "View Native Query" in Power BI Desktop, one step
at a time. A connector declaration can therefore drift from what the driver
reports or what Trino accepts, and nothing notices.

This checks the two halves that are mechanically checkable:

1. **The Constant visitor's keys.** Power Query looks each one up by
   `typeInfo[TYPE_NAME]`, which is the driver's own `SQLGetTypeInfo` output. A
   key that matches no TYPE_NAME can never fire, so the cast it declares is
   dead. A TYPE_NAME with no key folds nothing -- Power Query evaluates that
   constant locally instead of sending it.

2. **The SQL the connector emits.** Every CAST target it names has to be a type
   Trino actually has, and the LIMIT/OFFSET form its AstVisitor builds has to
   parse. Both are asserted by running them.

Usage:
    uv run --with pyodbc python3 test/test_folding_contract.py "<connection-string>"

Output is PASS / FAIL / NOTE, matching test_c_abi.py. A NOTE is a folding gap:
legal, but it means Power Query falls back to local evaluation for that type.
"""

import os
import re
import sys

import pyodbc

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Results, Stack  # noqa: E402

R = Results("folding contract")

CONNECTOR = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "..", "..", "connector", "StackableTrinoODBC.pq"
)

# A literal of each type, for the CAST the connector would emit. The value only
# has to be castable; the point is whether Trino knows the target type.
SAMPLE = {
    "DATE": "'2020-01-01'",
    "TIMESTAMP": "'2020-01-01 12:00:00'",
    "TIME": "'12:00:00'",
    "BOOLEAN": "true",
}

# TYPE_NAMEs that exist only so the Windows Driver Manager's
# SQLGetTypeInfo(SQL_CHAR=1) / SQLGetTypeInfo(SQL_VARCHAR=12) lookups find a
# row (`DM_COMPAT_ONLY` in src/backend/info.rs). `trino_bare_type_name` never
# returns either for a real column, so Power Query can never look a visitor
# entry up by one, an entry for them would be dead config, and listing them
# as a folding gap misreports a gap that cannot exist.
DM_COMPAT_ONLY = {"SQL_CHAR", "SQL_VARCHAR"}


def check(label, ok, detail=""):
    R.check(f"{label}{detail}", ok)


def note(label, text):
    R.note(label, text)


def parse_constant_visitor(source):
    """Map each Constant-visitor key to the SQL type it casts to.

    Matches the `KEY = each Cast(..., "TYPE")` entries of the AstVisitor's
    Constant record. Parsed from the connector rather than transcribed, so this
    test cannot drift from it.
    """
    visitor = {}
    # The cast target is the last quoted argument of the Cast(...) call, and it
    # is always upper case. Anchoring on that distinguishes it from an earlier
    # quoted argument -- `Cast(Quote(Date.ToText(_, "yyyy-MM-dd")), "DATE")`
    # would otherwise yield the date format instead of the type.
    for key, target in re.findall(
        r'(\w+)\s*=\s*each\s+Cast\(.*?"([A-Z][A-Z ]*)"\s*\)', source
    ):
        visitor[key] = target
    return visitor


def parse_limit_clause(source):
    """Render the row-limiting clause the AstVisitor builds for skip and take.

    Reads the format strings *and* the order the `Text = ...` expression
    concatenates them in, because the order is the whole point: Trino's grammar
    is `OFFSET count LIMIT count` and rejects the reverse.
    """
    formats = {
        name: fmt
        for name, fmt in re.findall(
            r'(limit|offset)\s*=\s*if\b.*?Text\.Format\("([^"]+)"', source
        )
    }
    if "limit" not in formats or "offset" not in formats:
        return None

    order = re.search(r"Text\s*=\s*([a-zA-Z&\s]+?),\s*\n", source)
    if not order:
        return None
    names = [n for n in re.findall(r"\b(limit|offset)\b", order.group(1))]
    if sorted(names) != ["limit", "offset"]:
        return None

    return " ".join(formats[n].strip().replace("#{0}", "2") for n in names)


def main():
    # A connection string may be passed positionally; with no argument the
    # local stack describes itself.
    conn_str = sys.argv[1] if len(sys.argv) > 1 else Stack.load().conn_str()

    with open(CONNECTOR, encoding="utf-8") as f:
        source = f.read()

    visitor = parse_constant_visitor(source)
    if not visitor:
        print("FAIL  could not parse the Constant visitor out of the connector")
        return 1

    conn = pyodbc.connect(conn_str, autocommit=True)
    cur = conn.cursor()
    type_names = {r[0] for r in cur.getTypeInfo().fetchall()}

    print(f"=== folding contract ===\nconnector: {os.path.normpath(CONNECTOR)}")
    print(f"visitor entries: {len(visitor)}, driver TYPE_NAMEs: {len(type_names)}\n")

    # ------------------------------------------------------------------
    print("--- every Constant visitor key is a TYPE_NAME the driver reports ---")
    # Power Query looks the key up by TYPE_NAME. One that matches nothing is
    # dead config, and dead config hides the absence of the entry that should
    # have been there -- which is how a Postgres-derived key list survives in a
    # Trino connector.
    for key in sorted(visitor):
        check(
            f"visitor key {key!r} is a driver TYPE_NAME",
            key in type_names,
            "" if key in type_names else f"  (casts to {visitor[key]}, can never fire)",
        )

    # ------------------------------------------------------------------
    print("\n--- every CAST target the connector emits is a Trino type ---")
    for key in sorted(visitor):
        target = visitor[key]
        sample = SAMPLE.get(target, "1")
        sql = f"SELECT CAST({sample} AS {target})"
        try:
            cur.execute(sql).fetchall()
            check(f"CAST(... AS {target}) for key {key}", True)
        except pyodbc.Error as e:
            check(f"CAST(... AS {target}) for key {key}", False, f"  {str(e)[:90]}")

    # ------------------------------------------------------------------
    print("\n--- the row-limiting clause the AstVisitor builds parses ---")
    rendered = parse_limit_clause(source)
    check("AstVisitor's LimitClause could be parsed", rendered is not None)
    if rendered:
        # Skip 2 of 1..5 and take 2, so the rows themselves prove the clause
        # was applied in the intended sense and not merely accepted.
        sql = f"SELECT x FROM (VALUES 1,2,3,4,5) t(x) ORDER BY x {rendered}"
        try:
            rows = [r[0] for r in cur.execute(sql).fetchall()]
            check(f"{rendered!r} skips 2 and takes 2", rows == [3, 4], f"  (got {rows})")
        except pyodbc.Error as e:
            check(f"{rendered!r} executes", False, f"  {str(e)[:90]}")

    # ------------------------------------------------------------------
    print("\n--- declared SqlCapabilities match what Trino does ---")
    # SupportsDerivedTable = true: Power Query wraps folded queries in a
    # subselect, so this has to hold or every folded query breaks.
    if "SupportsDerivedTable = true" in source:
        try:
            cur.execute("SELECT count(*) FROM (SELECT 1 AS x) s").fetchall()
            check("SupportsDerivedTable = true", True)
        except pyodbc.Error as e:
            check("SupportsDerivedTable = true", False, f"  {str(e)[:90]}")

    # SupportsTop = false: Trino has no TOP, so the declaration is honest only
    # if TOP really is rejected. A driver that started accepting it would make
    # the connector needlessly emit LIMIT.
    if "SupportsTop = false" in source:
        try:
            cur.execute("SELECT TOP 1 x FROM (VALUES 1,2) t(x)").fetchall()
            check("SupportsTop = false", False, "  Trino accepted TOP after all")
        except pyodbc.Error:
            check("SupportsTop = false", True, "  (Trino rejects TOP, as declared)")

    # ------------------------------------------------------------------
    print("\n--- the connector does not disable what the driver supports ---")
    # The connector's own rule is that an override is for what the driver gets
    # *wrong*, because it silently wins and cannot be corrected by fixing the
    # driver. Setting `Config_UseParameterBindings = false` declares
    # `SQL_API_SQLBINDPARAMETER = false`, which contradicts the driver:
    # `get_functions` lists `BindParameter`, and test_sql_surface.py exercises
    # parameters in every clause that takes one.
    #
    # Asserted on the flag rather than on the string `SQL_API_SQLBINDPARAMETER
    # = false`, which also appears in the branch the flag makes unreachable.
    bindings_on = re.search(r"Config_UseParameterBindings\s*=\s*true", source) is not None
    check(
        "Config_UseParameterBindings leaves SQLBindParameter enabled",
        bindings_on,
        "" if bindings_on else "  (set false, which disables a function the driver declares)",
    )

    # ------------------------------------------------------------------
    print("\n--- driver types with no Constant visitor entry ---")
    # Not a failure: an absent key makes Power Query evaluate that constant
    # locally rather than fold it. It is still worth naming, because a missing
    # entry is invisible from the connector alone -- nothing there lists the
    # types it does not handle.
    unhandled = sorted(type_names - set(visitor) - DM_COMPAT_ONLY)
    if unhandled:
        note(
            "constants that do not fold",
            f"{len(unhandled)} driver TYPE_NAMEs have no visitor entry: "
            + ", ".join(unhandled),
        )
    else:
        check("every driver TYPE_NAME has a visitor entry", True)

    cur.close()
    conn.close()

    return R.summary()


if __name__ == "__main__":
    sys.exit(main())
