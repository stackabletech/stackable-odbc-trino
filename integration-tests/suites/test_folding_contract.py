#!/usr/bin/env python3
"""
Folding contract test: the Power Query connector against the driver and Trino.

The connector tells Power Query how to render SQL: which constants it can cast,
what a LIMIT clause looks like, which capabilities to assume. No other suite
exercises any of that. The pyodbc, C ABI and surface tests drive the driver
directly and never load the `.mez`, and the only other check on folding is a
human clicking "View Native Query" in Power BI Desktop, one step at a time. A
connector declaration can therefore drift from what the driver reports or what
Trino accepts, and nothing notices.

This checks the three halves that are mechanically checkable:

1. **The Constant visitor's keys.** Power Query looks each one up by
   `typeInfo[TYPE_NAME]`, which is the driver's own `SQLGetTypeInfo` output. A
   key that matches no TYPE_NAME can never fire, so the cast it declares is
   dead. A TYPE_NAME with no key folds nothing, so Power Query evaluates that
   constant locally instead of sending it.

2. **The SQL the connector emits.** Every CAST target it names has to be a type
   Trino has, and the LIMIT/OFFSET form its AstVisitor builds has to
   parse. Both are asserted by running them.

3. **The temporal format strings.** A visitor entry for a date, time or
   timestamp renders the value through a .NET custom format string before
   quoting it, and that string is not checked by anything else here: a target
   type can be valid while the literal handed to it is not. `.sssssss` looks
   plausible and is seven seconds fields rather than a fractional second,
   because in a custom format string `s` is the second and `f` is the fraction.
   The formats are translated and the rendered literal is sent to Trino.

Usage:
    uv run --with pyodbc python3 integration-tests/suites/test_folding_contract.py "<connection-string>"

Requires a running Trino (integration-tests/setup.sh) and `pip install pyodbc`,
normally through `uv run --with pyodbc`. Needs no compose profile: every query
here runs against the base stack.

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
    # quoted argument. `Cast(Quote(Date.ToText(_, "yyyy-MM-dd")), "DATE")`
    # would otherwise yield the date format instead of the type.
    for key, target in re.findall(
        r'(\w+)\s*=\s*each\s+Cast\(.*?"([A-Z][A-Z ]*)"\s*\)', source
    ):
        visitor[key] = target
    return visitor


def parse_temporal_formats(source):
    """Map each Constant-visitor key to the .NET format string it renders with.

    Only the entries that quote a rendered value have one:
    `Cast(Quote(DateTime.ToText(_, "<format>")), "TIMESTAMP")`.
    """
    return dict(
        re.findall(
            r'(\w+)\s*=\s*each\s+Cast\(\s*Quote\(\s*(?:Date|DateTime|Time)'
            r'\.ToText\(\s*_\s*,\s*"([^"]+)"',
            source,
        )
    )


# The .NET custom date/time specifiers the connector is allowed to use, longest
# first so `mm` is matched before `m` would be. Anything else is rejected rather
# than guessed at: an unrecognised specifier is exactly the defect this looks
# for, and silently passing it through would render it as a literal.
NET_SPECIFIERS = [
    ("yyyy", "%Y"),
    ("MM", "%m"),
    ("dd", "%d"),
    ("HH", "%H"),
    ("mm", "%M"),
    ("ss", "%S"),
]

# The instant every rendered literal describes, chosen so each field is
# distinct: a format that swapped month for minute, or seconds for the
# fraction, produces a different string rather than an accidentally equal one.
SAMPLE_INSTANT = {
    "%Y": "2021",
    "%m": "02",
    "%d": "03",
    "%H": "04",
    "%M": "05",
    "%S": "06",
}
SAMPLE_FRACTION = "1234567"


def render_net_format(fmt):
    """Render `fmt` for `SAMPLE_INSTANT`, or raise ValueError if it cannot be.

    A run of `f`/`F` is the fractional second, rendered to the width asked for.
    A run of any other letter that is not a known specifier is an error: `sss`
    is not a wider seconds field, it is a mistake for `fff`.
    """
    out = []
    i = 0
    while i < len(fmt):
        ch = fmt[i]
        # The run of `ch` starting at `i`, measured on the remaining suffix.
        run = len(fmt[i:]) - len(fmt[i:].lstrip(ch))
        if ch in "fF":
            if run > 7:
                raise ValueError(f"fractional-seconds run of {run} exceeds .NET's 7 digits")
            out.append(SAMPLE_FRACTION[:run].ljust(run, "0"))
            i += run
            continue
        for token, strf in NET_SPECIFIERS:
            if fmt.startswith(token, i):
                # A longer run of the same letter is not the specifier: `sssssss`
                # starts with `ss` but is not a wider seconds field, and reading
                # it as one would hide exactly the defect this looks for.
                if run != len(token):
                    raise ValueError(
                        f"{ch!r} repeated {run} times is not a specifier; "
                        f"{token!r} is the field, and 'f' is the fractional second"
                    )
                out.append(SAMPLE_INSTANT[strf])
                i += run
                break
        else:
            if ch.isalpha():
                raise ValueError(f"unrecognised format specifier {ch!r}")
            out.append(ch)
            i += 1
    return "".join(out)


def parse_limit_clause(source):
    """Render the row-limiting clause the AstVisitor builds for skip and take.

    Reads the format strings *and* the order the `Text = ...` expression
    concatenates them in. Trino's grammar is `OFFSET count LIMIT count` and
    rejects the reverse, so only the order proves the clause is usable.
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
    # have been there. That is how a Postgres-derived key list survives in a
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
    print("\n--- the temporal literals the Constant visitor renders parse ---")
    # A valid CAST target with an unrenderable literal folds a filter into SQL
    # Trino rejects, or worse, into one it accepts with the wrong instant. The
    # target check above cannot see this: it substitutes its own SAMPLE literal.
    formats = parse_temporal_formats(source)
    check(
        "the temporal visitor entries declare a format string",
        len(formats) >= 3,
        "" if len(formats) >= 3 else f"  (parsed {sorted(formats)})",
    )
    for key in sorted(formats):
        target = visitor.get(key, key)
        try:
            literal = render_net_format(formats[key])
        except ValueError as e:
            check(f"{key} format {formats[key]!r} is renderable", False, f"  {e}")
            continue
        sql = f"SELECT CAST('{literal}' AS {target})"
        try:
            got = cur.execute(sql).fetchone()[0]
            # Round-tripped, not merely accepted: Trino parses a great many
            # malformed-looking strings, and the failure that matters is a
            # literal that lands on a different instant.
            rendered_back = str(got)
            fields_present = all(
                v in rendered_back for v in ("2021", "02", "03", "04", "05", "06")
                if v in literal
            )
            check(
                f"{key} renders {literal!r}, which CASTs to {target}",
                fields_present,
                "" if fields_present else f"  (Trino read it back as {rendered_back!r})",
            )
        except pyodbc.Error as e:
            check(
                f"{key} renders {literal!r}, which CASTs to {target}",
                False,
                f"  {str(e)[:90]}",
            )

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
    # locally rather than fold it. It is named because a missing entry is
    # invisible from the connector alone: nothing there lists the types it does
    # not handle.
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
