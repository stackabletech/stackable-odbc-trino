#!/usr/bin/env python3
"""Escape-sequence contract: every capability the driver advertises, executed.

`src/backend/info.rs` states the rule these bitmaps follow:

    a bit may only be set when `translate_escapes`, driven by
    `crate::escape_dialect`, turns that escape into Trino SQL that runs. A bit
    whose name `rewrite_scalar_fn` does not handle reaches the coordinator
    verbatim and fails there.

Until this suite existed nothing checked that against a coordinator. The Rust
side has `untranslatable_escapes_are_never_advertised`, which compares a bitmap
against a list of names, and unit tests that compare the rewriter's output
against expected strings. Neither submits the result to Trino, so an argument
order, a unit spelling or a value conversion could be wrong in both places at
once and agree with itself.

What is checked here:

1. **Every advertised scalar function runs.** The claimed names are parsed out
   of `src/backend/info.rs` rather than transcribed, so a bit added there
   without an entry in `CALLS` below fails this suite instead of shipping
   unchecked.
2. **The rewritten ones return the right value.** A rename cannot be wrong in
   an interesting way, but `DAYOFWEEK` renumbers, `TIMESTAMPDIFF` has an
   argument order, `LOG` is a different function from Trino's `log`, and
   `RAND` drops a seed Trino would read as a bound. Those are asserted on the
   result, not on the absence of an error. `ATAN2` is asserted the same way
   despite having no rewrite, because passing its arguments through in the
   caller's order is a deliberate deviation from the ODBC appendix and the
   inputs are chosen so that the two readings disagree.
3. **Every `SQL_TSI_*` interval the driver advertises** works in both
   `TIMESTAMPADD` and `TIMESTAMPDIFF`, parsed from `TRINO_TIMESTAMP_INTERVALS`
   in `src/backend.rs`.
4. **Every `{fn CONVERT}` target**, parsed from `trino_convert_target` in
   `src/escape_dialect.rs`, because `SQL_CONVERT_FUNCTIONS` reports
   `SQL_FN_CVT_CAST` and a client reading that may send any ODBC type keyword.
5. **The `{d}`, `{t}` and `{ts}` literal escapes**, which have their own
   renderers in `escape_dialect::dialect()`.

Usage:
    uv run --with pyodbc python3 integration-tests/suites/test_escapes.py "<connection-string>"

Requires a running Trino (integration-tests/setup.sh). Needs no compose
profile: every query here is catalog-free.
"""

import os
import re
import sys

import pyodbc

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Results, Stack  # noqa: E402

R = Results("escape sequences")

PROJECT_DIR = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
INFO_RS = os.path.join(PROJECT_DIR, "src", "backend", "info.rs")
BACKEND_RS = os.path.join(PROJECT_DIR, "src", "backend.rs")
DIALECT_RS = os.path.join(PROJECT_DIR, "src", "escape_dialect.rs")

# One `{fn ...}` call per advertised capability, and the value it must return.
#
# `None` for an expected value means "assert only that it runs": the result is
# either the clock, the session, or a float whose exact text is not the point.
# Everything the dialect *transforms* carries a value, because that is where a
# translation can be plausible and wrong.
#
# The keys are the `SQL_FN_*` names as `src/backend/info.rs` spells them. A name
# advertised there and missing here is a failure, not a skip.
CALLS = {
    # --- strings ---------------------------------------------------------
    "SQL_FN_STR_CONCAT": ("{fn CONCAT('ab', 'cd')}", "abcd"),
    # Rewritten to the two-argument `ltrim`, because ODBC removes *blanks* and
    # Trino's one-argument form removes every kind of whitespace. The leading
    # tab has to survive; a pass-through would eat it and answer "ab".
    "SQL_FN_STR_LTRIM": ("{fn LTRIM('  ' || chr(9) || 'ab')}", "\tab"),
    # Rewritten to `length(rtrim(x, ' '))`: ODBC counts characters "excluding
    # trailing blanks" and Trino's `length` counts them. The trailing spaces
    # are the discriminator, and a pass-through answers 6.
    "SQL_FN_STR_LENGTH": ("{fn LENGTH('abc   ')}", 3),
    # Renamed to `lower`.
    "SQL_FN_STR_LCASE": ("{fn LCASE('AbC')}", "abc"),
    # Renamed to `upper`.
    "SQL_FN_STR_UCASE": ("{fn UCASE('AbC')}", "ABC"),
    # Rewritten to `position(sub IN str)`. ODBC's argument order is the
    # opposite of Trino's `strpos`, so a wrong rewrite still returns a number.
    "SQL_FN_STR_LOCATE_2": ("{fn LOCATE('cd', 'abcdef')}", 3),
    # Already Trino's syntax, so this proves the escape is left alone.
    "SQL_FN_STR_POSITION": ("{fn POSITION('cd' IN 'abcdef')}", 3),
    "SQL_FN_STR_REPLACE": ("{fn REPLACE('abcabc', 'b', 'X')}", "aXcaXc"),
    # The RTRIM half of the same blanks-versus-whitespace split as LTRIM above.
    "SQL_FN_STR_RTRIM": ("{fn RTRIM('ab' || chr(9) || '  ')}", "ab\t"),
    "SQL_FN_STR_SUBSTRING": ("{fn SUBSTRING('abcdef', 2, 3)}", "bcd"),
    # Renamed to `chr`.
    "SQL_FN_STR_CHAR": ("{fn CHAR(65)}", "A"),
    # Passed through. Trino gained soundex() in 356; see the bitmap's comment.
    "SQL_FN_STR_SOUNDEX": ("{fn SOUNDEX('Robert')}", "R163"),
    # --- numerics --------------------------------------------------------
    "SQL_FN_NUM_ABS": ("{fn ABS(-3)}", 3),
    "SQL_FN_NUM_ACOS": ("{fn ACOS(1)}", 0.0),
    "SQL_FN_NUM_ASIN": ("{fn ASIN(0)}", 0.0),
    "SQL_FN_NUM_ATAN": ("{fn ATAN(0)}", 0.0),
    # Passed through with the arguments in the order the caller wrote them,
    # which is a deliberate deviation from the ODBC appendix; the reasoning is
    # in `rewrite_scalar_fn`, next to the arm ATAN2 deliberately does not have.
    # These inputs discriminate: reading the first argument as y, which is what
    # Trino and every peer implementation do, gives atan(1/2) below, while the
    # appendix's first-argument-is-x reading would give atan(2) = 1.1071487.
    "SQL_FN_NUM_ATAN2": ("{fn ATAN2(1, 2)}", 0.4636476090008061),
    "SQL_FN_NUM_CEILING": ("{fn CEILING(1.2)}", 2),
    "SQL_FN_NUM_COS": ("{fn COS(0)}", 1.0),
    "SQL_FN_NUM_EXP": ("{fn EXP(0)}", 1.0),
    "SQL_FN_NUM_FLOOR": ("{fn FLOOR(1.8)}", 1),
    # ODBC's LOG is the natural logarithm and Trino's `log` is base-b, so this
    # is renamed to `ln`. LOG(1) is 0 either way; the discriminator is that a
    # base-b `log` needs two arguments and would fail to resolve.
    "SQL_FN_NUM_LOG": ("{fn LOG(1)}", 0.0),
    "SQL_FN_NUM_MOD": ("{fn MOD(7, 3)}", 1),
    "SQL_FN_NUM_SIGN": ("{fn SIGN(-5)}", -1),
    "SQL_FN_NUM_SIN": ("{fn SIN(0)}", 0.0),
    "SQL_FN_NUM_SQRT": ("{fn SQRT(9)}", 3.0),
    "SQL_FN_NUM_TAN": ("{fn TAN(0)}", 0.0),
    "SQL_FN_NUM_PI": ("{fn PI()}", None),
    # The seeded form: Trino reads that argument as a bound and would answer an
    # integer in [0, 5). The rewrite drops it, so the result must be a fraction.
    "SQL_FN_NUM_RAND": ("{fn RAND(5)}", None),
    "SQL_FN_NUM_DEGREES": ("{fn DEGREES(0)}", 0.0),
    "SQL_FN_NUM_LOG10": ("{fn LOG10(100)}", 2.0),
    "SQL_FN_NUM_POWER": ("{fn POWER(2, 3)}", 8.0),
    "SQL_FN_NUM_RADIANS": ("{fn RADIANS(0)}", 0.0),
    # Passed through, and the negative digit count is the case worth asserting:
    # ODBC specifies rounding to the left of the decimal point for a negative
    # argument, and Trino's math reference documents that only for `truncate`.
    # It works for round too, so this pins the behaviour the docs omit.
    "SQL_FN_NUM_ROUND": ("{fn ROUND(1234.5, -2)}", 1200.0),
    # Rewritten to scaled arithmetic. Trino's two-argument `truncate` is
    # declared over `decimal` alone, so this has to be a DOUBLE: a decimal
    # literal here would pass against the pass-through that FUNCTION_NOT_FOUNDs
    # on every float column, which is the case ODBC's `numeric_exp` covers.
    "SQL_FN_NUM_TRUNCATE": ("{fn TRUNCATE(CAST(1.99 AS DOUBLE), 1)}", 1.9),
    # --- system ----------------------------------------------------------
    # Both lose their parentheses: Trino takes them as bare SQL-92 keywords.
    "SQL_FN_SYS_USERNAME": ("{fn USERNAME()}", None),
    "SQL_FN_SYS_DBNAME": ("{fn DBNAME()}", None),
    # Renamed to two-argument `coalesce`.
    "SQL_FN_SYS_IFNULL": ("{fn IFNULL(NULL, 'fallback')}", "fallback"),
    # --- date and time ---------------------------------------------------
    "SQL_FN_TD_NOW": ("{fn NOW()}", None),
    "SQL_FN_TD_CURDATE": ("{fn CURDATE()}", None),
    "SQL_FN_TD_CURTIME": ("{fn CURTIME()}", None),
    "SQL_FN_TD_CURRENT_DATE": ("{fn CURRENT_DATE()}", None),
    "SQL_FN_TD_CURRENT_TIME": ("{fn CURRENT_TIME()}", None),
    "SQL_FN_TD_CURRENT_TIMESTAMP": ("{fn CURRENT_TIMESTAMP()}", None),
    "SQL_FN_TD_DAYOFMONTH": ("{fn DAYOFMONTH(DATE '2021-02-03')}", 3),
    # 2021-02-07 is a Sunday, which is 1 in ODBC's numbering and 7 in Trino's
    # ISO one. A rename alone returns 7 here: plausible, and silently wrong.
    "SQL_FN_TD_DAYOFWEEK": ("{fn DAYOFWEEK(DATE '2021-02-07')}", 1),
    "SQL_FN_TD_DAYOFYEAR": ("{fn DAYOFYEAR(DATE '2021-02-03')}", 34),
    "SQL_FN_TD_MONTH": ("{fn MONTH(DATE '2021-02-03')}", 2),
    "SQL_FN_TD_QUARTER": ("{fn QUARTER(DATE '2021-02-03')}", 1),
    # Trino's week() is ISO-numbered, which the bitmap documents as a caveat,
    # so this asserts only that it runs.
    "SQL_FN_TD_WEEK": ("{fn WEEK(DATE '2021-02-03')}", None),
    "SQL_FN_TD_YEAR": ("{fn YEAR(DATE '2021-02-03')}", 2021),
    "SQL_FN_TD_HOUR": ("{fn HOUR(TIMESTAMP '2021-02-03 04:05:06')}", 4),
    "SQL_FN_TD_MINUTE": ("{fn MINUTE(TIMESTAMP '2021-02-03 04:05:06')}", 5),
    "SQL_FN_TD_SECOND": ("{fn SECOND(TIMESTAMP '2021-02-03 04:05:06')}", 6),
    "SQL_FN_TD_EXTRACT": ("{fn EXTRACT(YEAR FROM DATE '2021-02-03')}", 2021),
    # Covered per interval unit below as well; this pins the plain shape.
    "SQL_FN_TD_TIMESTAMPADD":
        ("{fn TIMESTAMPADD(SQL_TSI_DAY, 2, DATE '2021-02-03')}", None),
    # ODBC defines TIMESTAMPDIFF(interval, a, b) as b - a. Reversed, this
    # returns -2 rather than 2, which is exactly as plausible.
    "SQL_FN_TD_TIMESTAMPDIFF":
        ("{fn TIMESTAMPDIFF(SQL_TSI_DAY, DATE '2021-02-03', DATE '2021-02-05')}", 2),
}

# The bitmaps whose names are checked, and the Rust constant each is built from.
BITMAPS = {
    "SQL_STRING_FUNCTIONS": "TRINO_STRING_FUNCTIONS",
    "SQL_NUMERIC_FUNCTIONS": "TRINO_NUMERIC_FUNCTIONS",
    "SQL_SYSTEM_FUNCTIONS": "TRINO_SYSTEM_FUNCTIONS",
    "SQL_TIMEDATE_FUNCTIONS": "TRINO_TIMEDATE_FUNCTIONS",
}

def escape_keyword(bitmap_name):
    """The `{fn}` keyword for an interval whose *bit* is named `bitmap_name`.

    ODBC names the bit `SQL_FN_TSI_DAY` and the escape keyword `SQL_TSI_DAY`,
    so the two differ by the `FN_`. Passing the bit's name through unchanged
    reaches Trino as a bare identifier and fails with `COLUMN_NOT_FOUND`, which
    is the same failure `escape_dialect.rs` describes for an unhandled name.
    """
    return bitmap_name.replace("SQL_FN_TSI_", "SQL_TSI_")


# One `TIMESTAMPADD` / `TIMESTAMPDIFF` probe per interval keyword, as
# (added-to-2021-02-03T04:05:06, difference between two values one unit apart).
# The pair proves the unit reached Trino as the right word: `date_add('day',...)`
# and `date_add('hour',...)` both run, and only the result tells them apart.
INTERVAL_PROBES = {
    "SQL_FN_TSI_SECOND": ("TIMESTAMP '2021-02-03 04:05:06'", "TIMESTAMP '2021-02-03 04:05:07'"),
    "SQL_FN_TSI_MINUTE": ("TIMESTAMP '2021-02-03 04:05:06'", "TIMESTAMP '2021-02-03 04:06:06'"),
    "SQL_FN_TSI_HOUR": ("TIMESTAMP '2021-02-03 04:05:06'", "TIMESTAMP '2021-02-03 05:05:06'"),
    "SQL_FN_TSI_DAY": ("TIMESTAMP '2021-02-03 04:05:06'", "TIMESTAMP '2021-02-04 04:05:06'"),
    "SQL_FN_TSI_WEEK": ("TIMESTAMP '2021-02-03 04:05:06'", "TIMESTAMP '2021-02-10 04:05:06'"),
    "SQL_FN_TSI_MONTH": ("TIMESTAMP '2021-02-03 04:05:06'", "TIMESTAMP '2021-03-03 04:05:06'"),
    "SQL_FN_TSI_QUARTER": ("TIMESTAMP '2021-02-03 04:05:06'", "TIMESTAMP '2021-05-03 04:05:06'"),
    "SQL_FN_TSI_YEAR": ("TIMESTAMP '2021-02-03 04:05:06'", "TIMESTAMP '2022-02-03 04:05:06'"),
}

# A value each ODBC CONVERT target can be reached from. `{fn CONVERT}` carries
# no length, so a character target has to come from something short.
CONVERT_SOURCES = {
    "SQL_BIGINT": "1", "SQL_INTEGER": "1", "SQL_SMALLINT": "1", "SQL_TINYINT": "1",
    "SQL_DOUBLE": "1", "SQL_FLOAT": "1", "SQL_REAL": "1",
    "SQL_DECIMAL": "1", "SQL_NUMERIC": "1",
    "SQL_BIT": "true",
    "SQL_CHAR": "'ab'", "SQL_VARCHAR": "'ab'", "SQL_LONGVARCHAR": "'ab'",
    "SQL_WCHAR": "'ab'", "SQL_WVARCHAR": "'ab'", "SQL_WLONGVARCHAR": "'ab'",
    "SQL_BINARY": "'ab'", "SQL_VARBINARY": "'ab'", "SQL_LONGVARBINARY": "'ab'",
    "SQL_DATE": "'2021-02-03'", "SQL_TYPE_DATE": "'2021-02-03'",
    "SQL_TIME": "'04:05:06'", "SQL_TYPE_TIME": "'04:05:06'",
    "SQL_TIMESTAMP": "'2021-02-03 04:05:06'",
    "SQL_TYPE_TIMESTAMP": "'2021-02-03 04:05:06'",
    "SQL_GUID": "'12151fd2-7586-11e9-8f9e-2a86e4085a59'",
}


def read(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


def advertised_names(source, constant):
    """The `SQL_FN_*` names OR'd together into `constant`.

    Parsed from the driver's own source, so a capability added to the bitmap
    without a probe here fails this suite rather than shipping unchecked. That
    is the whole point: a hand-copied list would drift silently, which is what
    happened to the bitmap's own documentation.
    """
    m = re.search(
        rf"const {constant}: u32 =(.*?);", source, re.DOTALL
    )
    if not m:
        return []
    return re.findall(r"\b(SQL_FN_[A-Z0-9_]+)\b", m.group(1))


def convert_targets(source):
    """The ODBC type keywords `trino_convert_target` maps, with their targets."""
    m = re.search(
        r"fn trino_convert_target\(.*?\n\}", source, re.DOTALL
    )
    if not m:
        return {}
    targets = {}
    for keywords, trino in re.findall(
        r'((?:"SQL_[A-Z_]+"\s*\|?\s*)+)=>\s*Some\("([A-Z ]+)"\)', m.group(0)
    ):
        for kw in re.findall(r'"(SQL_[A-Z_]+)"', keywords):
            targets[kw] = trino
    return targets


def scalar(cur, sql):
    return cur.execute(f"SELECT {sql}").fetchone()[0]


def matches(got, expected):
    """Compare loosely enough for Trino's numeric types, exactly otherwise.

    A Trino DECIMAL arrives as `decimal.Decimal` and a DOUBLE as `float`, and
    which one a scalar expression yields is Trino's business, not the escape's.
    """
    if expected is None:
        return True
    if isinstance(expected, float) or isinstance(expected, int):
        try:
            return abs(float(got) - float(expected)) < 1e-9
        except (TypeError, ValueError):
            return False
    return str(got) == str(expected)


def main():
    conn_str = sys.argv[1] if len(sys.argv) > 1 else Stack.load().conn_str()

    info_src = read(INFO_RS)
    backend_src = read(BACKEND_RS)
    dialect_src = read(DIALECT_RS)

    conn = pyodbc.connect(conn_str, autocommit=True)
    cur = conn.cursor()

    print("=== escape sequences ===")

    # ------------------------------------------------------------------
    print("\n--- every advertised scalar function has a probe ---")
    # Checked before anything runs. A bit added to a bitmap without an entry in
    # CALLS is a capability nothing here executes, and a suite that quietly
    # skipped it would report a clean run over an unchecked claim.
    claimed = []
    for info_type, constant in BITMAPS.items():
        names = advertised_names(info_src, constant)
        R.check(
            f"{constant} parsed out of info.rs",
            bool(names),
            "" if names else "  (the suite cannot see what the driver claims)",
        )
        claimed.extend(names)
    unprobed = sorted(set(claimed) - set(CALLS))
    R.check(
        "every advertised name has a probe in CALLS",
        not unprobed,
        "" if not unprobed else f"  add one for: {', '.join(unprobed)}",
    )
    stale = sorted(set(CALLS) - set(claimed))
    R.check(
        "every probe in CALLS is still advertised",
        not stale,
        "" if not stale else f"  no longer in a bitmap: {', '.join(stale)}",
    )

    # ------------------------------------------------------------------
    print(f"\n--- {len(claimed)} advertised scalar functions execute ---")
    for name in sorted(set(claimed) & set(CALLS)):
        call, expected = CALLS[name]
        try:
            got = scalar(cur, call)
        except pyodbc.Error as e:
            R.bad(f"{name}: {call}", str(e)[:110])
            continue
        R.check(
            f"{name}: {call}",
            matches(got, expected),
            "" if matches(got, expected) else f"  expected {expected!r}, got {got!r}",
        )

    # The two whose value is a range rather than a constant.
    try:
        pi = float(scalar(cur, "{fn PI()}"))
        ok = abs(pi - 3.14159265358979) < 1e-9
        R.check("{fn PI()} is pi", ok, "" if ok else f"  (got {pi})")
    except (pyodbc.Error, TypeError, ValueError) as e:
        R.bad("{fn PI()} is pi", str(e)[:110])
    try:
        # The seed is dropped, so this must be a fraction in [0, 1). Trino
        # reading the 5 as a bound would give an integer in [0, 5).
        rand = float(scalar(cur, "{fn RAND(5)}"))
        R.check(
            "{fn RAND(seed)} returns a fraction, not an integer in [0, seed)",
            0.0 <= rand < 1.0,
            "" if 0.0 <= rand < 1.0 else f"  (got {rand})",
        )
    except (pyodbc.Error, TypeError, ValueError) as e:
        R.bad("{fn RAND(seed)} returns a fraction", str(e)[:110])

    # ------------------------------------------------------------------
    print("\n--- every advertised interval unit works in both directions ---")
    intervals = advertised_names(backend_src, "TRINO_TIMESTAMP_INTERVALS")
    R.check("TRINO_TIMESTAMP_INTERVALS parsed out of backend.rs", bool(intervals))
    unprobed = sorted(set(intervals) - set(INTERVAL_PROBES))
    R.check(
        "every advertised interval has a probe",
        not unprobed,
        "" if not unprobed else f"  add one for: {', '.join(unprobed)}",
    )
    for bit in intervals:
        if bit not in INTERVAL_PROBES:
            continue
        unit = escape_keyword(bit)
        base, one_later = INTERVAL_PROBES[bit]
        # ODBC defines TIMESTAMPDIFF(interval, a, b) as b - a, so this is +1.
        # A reversed argument order returns -1, which no error would reveal.
        try:
            diff = scalar(cur, f"{{fn TIMESTAMPDIFF({unit}, {base}, {one_later})}}")
            R.check(
                f"TIMESTAMPDIFF({unit}) counts b - a",
                int(diff) == 1,
                "" if int(diff) == 1 else f"  expected 1, got {diff}",
            )
        except (pyodbc.Error, TypeError, ValueError) as e:
            R.bad(f"TIMESTAMPDIFF({unit})", str(e)[:110])
        # And adding one unit to the base reaches the later value. This is what
        # proves the unit reached Trino as the right word: `date_add('day',...)`
        # and `date_add('hour',...)` both run, and only the result separates them.
        try:
            added = scalar(cur, f"{{fn TIMESTAMPADD({unit}, 1, {base})}}")
            expected = scalar(cur, one_later)
            R.check(
                f"TIMESTAMPADD({unit}) advances by one unit",
                str(added) == str(expected),
                "" if str(added) == str(expected) else f"  expected {expected}, got {added}",
            )
        except (pyodbc.Error, TypeError, ValueError) as e:
            R.bad(f"TIMESTAMPADD({unit})", str(e)[:110])

    # ------------------------------------------------------------------
    print("\n--- every {fn CONVERT} target casts ---")
    # SQL_CONVERT_FUNCTIONS reports SQL_FN_CVT_CAST, so a client may send any
    # ODBC type keyword. One with no arm reaches Trino as a bare identifier and
    # fails with COLUMN_NOT_FOUND, which is what the dialect's own comment says.
    targets = convert_targets(dialect_src)
    R.check(
        "trino_convert_target parsed out of escape_dialect.rs",
        len(targets) > 15,
        "" if len(targets) > 15 else f"  (parsed {sorted(targets)})",
    )
    missing_source = sorted(set(targets) - set(CONVERT_SOURCES))
    R.check(
        "every CONVERT target has a source value to cast from",
        not missing_source,
        "" if not missing_source else f"  add one for: {', '.join(missing_source)}",
    )
    for keyword in sorted(targets):
        if keyword not in CONVERT_SOURCES:
            continue
        call = f"{{fn CONVERT({CONVERT_SOURCES[keyword]}, {keyword})}}"
        try:
            scalar(cur, call)
            R.ok(f"{call} -> {targets[keyword]}")
        except pyodbc.Error as e:
            R.bad(f"{call} -> {targets[keyword]}", str(e)[:110])

    # ------------------------------------------------------------------
    print("\n--- {fn TRUNCATE} keeps the argument's type ---")
    # Trino's two-argument `truncate` is decimal-only, so the escape scales into
    # the single-argument form. ODBC requires TRUNCATE to return "the same data
    # type as the input parameters", which holds only because the scale factor
    # is an integer literal: `power(10, d)` is double-valued and would drag
    # decimal and real to double. Asserted on `typeof`, because the value alone
    # cannot tell the two rewrites apart.
    for source, want_type in [
        ("CAST(1.99 AS DECIMAL(3,2))", "decimal"),
        ("CAST(1.99 AS DOUBLE)", "double"),
        ("CAST(1.99 AS REAL)", "real"),
    ]:
        for digits in (1, 0, -1):
            call = f"{{fn TRUNCATE({source}, {digits})}}"
            try:
                got = scalar(cur, f"typeof(({call}))")
                ok = str(got).startswith(want_type)
                R.check(
                    f"{call} -> {want_type}",
                    ok,
                    "" if ok else f"  (got {got!r}, so the scale factor widened it)",
                )
            except pyodbc.Error as e:
                R.bad(f"{call} -> {want_type}", str(e)[:110])

    # ------------------------------------------------------------------
    print("\n--- the {d} {t} {ts} literal escapes ---")
    # Rendered by `render_date`/`render_time`/`render_timestamp` in
    # escape_dialect.rs, which prefix Trino's own type keyword. Asserted on the
    # value, so a renderer that produced a parseable literal for the wrong
    # instant is caught.
    for call, expected in [
        ("{d '2021-02-03'}", "2021-02-03"),
        ("{t '04:05:06'}", "04:05:06"),
        ("{ts '2021-02-03 04:05:06'}", "2021-02-03 04:05:06"),
    ]:
        try:
            got = scalar(cur, call)
            ok = str(got).startswith(expected)
            R.check(f"{call}", ok, "" if ok else f"  (got {got!r})")
        except pyodbc.Error as e:
            R.bad(f"{call}", str(e)[:110])

    cur.close()
    conn.close()
    return R.summary()


if __name__ == "__main__":
    sys.exit(main())
