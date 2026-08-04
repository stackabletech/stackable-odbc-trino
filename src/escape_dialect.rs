//! Trino escape-translation dialect: `"`-quoted identifiers, Trino date/time
//! literals, and the `{fn}` scalar-function remap for the names Trino spells
//! differently from ODBC.
//!
//! Both function hooks correspond exactly to the `SQL_*_FUNCTIONS` bitmaps
//! `src/backend/info.rs` advertises: a bit is advertised only if
//! `{fn NAME(...)}` translates into Trino SQL that runs. A bit without a
//! translation is a capability an application is told it has and cannot use.
//!
//! Two hooks, for two kinds of difference:
//!
//! - [`remap_scalar_fn`] swaps the identifier in front of the parentheses and
//!   never sees the arguments, which is all a spelling difference needs:
//!   `UCASE` → `upper`, `LOG` → `ln`.
//! - [`rewrite_scalar_fn`] receives the whole call and returns its
//!   replacement, for the functions where ODBC and Trino agree on the
//!   capability but not on the syntax: `LOCATE(a, b)` → `position(a IN b)`,
//!   the `CURDATE`/`USERNAME` family → bare keywords with the `()` removed,
//!   `TIMESTAMPADD(SQL_TSI_DAY, ...)` → `date_add('day', ...)` with the unit
//!   re-quoted, `DAYOFWEEK` → an expression converting Trino's ISO day
//!   numbering to ODBC's, `LENGTH`/`LTRIM`/`RTRIM` → the two-argument trims
//!   that take ODBC's "blanks" literally, and `TRUNCATE` → scaled arithmetic,
//!   because Trino's two-argument form is declared over `decimal` alone.
//!
//! Everything else passes through unchanged (`None`). `POSITION` needs no
//! hook, because ODBC spells it `POSITION(exp IN exp)`, which is already
//! Trino's syntax; `NOW`, `MONTH`, `QUARTER`, `WEEK`, `YEAR`, `HOUR`,
//! `MINUTE`, `SECOND`, `EXTRACT` and the numeric and string functions with no
//! arm below agree with Trino on both the name and the signature, verified
//! against <https://trino.io/docs/current/functions/string.html>,
//! <https://trino.io/docs/current/functions/math.html> and
//! <https://trino.io/docs/current/functions/datetime.html>.
//!
//! `ATAN2` is the one name where that agreement covers the spelling but not
//! the ODBC appendix's argument order. It passes through deliberately; the
//! reasoning is recorded at the end of [`rewrite_scalar_fn`].
use stackable_odbc_core::escape::EscapeDialect;

/// Remap an ODBC `{fn NAME(...)}` scalar-function name to Trino's spelling.
/// `None` passes the name through unchanged (same spelling in both).
pub(crate) fn remap_scalar_fn(name: &str) -> Option<&'static str> {
    match name.to_ascii_uppercase().as_str() {
        // SQL_FN_STR_UCASE / SQL_FN_STR_LCASE / SQL_FN_STR_CHAR
        "UCASE" => Some("upper"),
        "LCASE" => Some("lower"),
        "CHAR" => Some("chr"),
        // SQL_FN_NUM_LOG: ODBC's LOG is the natural logarithm, while Trino's
        // own `log(b, x)` is base-b and a different function, so this maps to
        // `ln()` specifically (see the SQL_NUMERIC_FUNCTIONS doc comment in
        // backend/info.rs).
        "LOG" => Some("ln"),
        // SQL_FN_SYS_IFNULL: Trino has no `ifnull`, but two-argument
        // `coalesce(a, b)` is exactly equivalent (same doc comment).
        "IFNULL" => Some("coalesce"),
        // SQL_FN_TD_DAYOFMONTH / SQL_FN_TD_DAYOFYEAR: same semantics as
        // ODBC (1-31 / 1-366), just spelled with underscores in Trino.
        // DAYOFWEEK is NOT remapped here, because its numbering differs; it
        // goes through `rewrite_scalar_fn` instead.
        "DAYOFMONTH" => Some("day_of_month"),
        "DAYOFYEAR" => Some("day_of_year"),
        _ => None,
    }
}

/// ODBC interval keyword → the unit string Trino's `date_add` / `date_diff`
/// take as their first argument.
///
/// `SQL_TSI_FRAC_SECOND` is absent: ODBC defines it as billionths of a second
/// and Trino's finest unit is `millisecond`, which would silently be a
/// million times coarser. That is why `SQL_FN_TSI_FRAC_SECOND` is left out of
/// [`Backend::timedate_add_intervals`](stackable_odbc_core::backend::Backend::timedate_add_intervals)
/// too, and the two must agree.
fn trino_interval_unit(keyword: &str) -> Option<&'static str> {
    match keyword.trim().to_ascii_uppercase().as_str() {
        "SQL_TSI_SECOND" => Some("second"),
        "SQL_TSI_MINUTE" => Some("minute"),
        "SQL_TSI_HOUR" => Some("hour"),
        "SQL_TSI_DAY" => Some("day"),
        "SQL_TSI_WEEK" => Some("week"),
        "SQL_TSI_MONTH" => Some("month"),
        "SQL_TSI_QUARTER" => Some("quarter"),
        "SQL_TSI_YEAR" => Some("year"),
        _ => None,
    }
}

/// ODBC type keyword → the Trino type `{fn CONVERT(value, SQL_type)}` casts to.
///
/// The whole set of keywords the spec defines for this escape is covered,
/// because `SQL_CONVERT_FUNCTIONS` reports `SQL_FN_CVT_CAST`: a client reading
/// that bitmap may send any of them, and one without an arm here reaches Trino
/// as a bare identifier and fails with `COLUMN_NOT_FOUND`.
///
/// Two mappings are not the obvious ones, both measured against a live
/// coordinator rather than read off the documentation:
///
/// - `SQL_CHAR` maps to `VARCHAR`, not to Trino's `CHAR`. A bare `CHAR` in
///   Trino is `CHAR(1)`, so `CAST('hello world' AS CHAR)` returns `"h"`, and
///   the escape would truncate every conversion to one character. ODBC's
///   `{fn CONVERT}` carries no length to give `CHAR(n)` instead.
/// - `SQL_FLOAT` maps to `DOUBLE`. ODBC's `SQL_FLOAT` is double precision,
///   and Trino's single-precision type, `REAL`, is ODBC's `SQL_REAL`.
///
/// The `SQL_INTERVAL_*` keywords are absent: a bare `CAST` from an arbitrary
/// expression cannot reach Trino's interval types, so there is nothing honest
/// to rewrite them to. Declining leaves the call on the fallback path instead
/// of casting to something the application did not ask for.
fn trino_convert_target(keyword: &str) -> Option<&'static str> {
    match keyword.trim().to_ascii_uppercase().as_str() {
        "SQL_BIGINT" => Some("BIGINT"),
        "SQL_INTEGER" => Some("INTEGER"),
        "SQL_SMALLINT" => Some("SMALLINT"),
        "SQL_TINYINT" => Some("TINYINT"),
        "SQL_DOUBLE" | "SQL_FLOAT" => Some("DOUBLE"),
        "SQL_REAL" => Some("REAL"),
        "SQL_DECIMAL" | "SQL_NUMERIC" => Some("DECIMAL"),
        "SQL_BIT" => Some("BOOLEAN"),
        "SQL_CHAR" | "SQL_VARCHAR" | "SQL_LONGVARCHAR" | "SQL_WCHAR" | "SQL_WVARCHAR"
        | "SQL_WLONGVARCHAR" => Some("VARCHAR"),
        "SQL_BINARY" | "SQL_VARBINARY" | "SQL_LONGVARBINARY" => Some("VARBINARY"),
        "SQL_DATE" | "SQL_TYPE_DATE" => Some("DATE"),
        "SQL_TIME" | "SQL_TYPE_TIME" => Some("TIME"),
        "SQL_TIMESTAMP" | "SQL_TYPE_TIMESTAMP" => Some("TIMESTAMP"),
        "SQL_GUID" => Some("UUID"),
        _ => None,
    }
}

/// Split a `{fn ...}` argument list on its top-level commas.
///
/// Core hands the argument text over whole, because only the dialect knows
/// each function's arity and a naive split would corrupt
/// `{fn LOCATE(',', x)}`. So this walks the text with the same awareness core
/// applies to the statement: a comma inside a string literal, a quoted
/// identifier, a comment or a nested parenthesis is not a separator.
fn split_args(args: &str) -> Vec<&str> {
    let bytes: Vec<char> = args.chars().collect();
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut i = 0usize;
    // Byte offsets, so the returned slices borrow from `args` directly.
    let mut char_to_byte = vec![0usize; bytes.len() + 1];
    let mut acc = 0usize;
    for (n, c) in bytes.iter().enumerate() {
        char_to_byte[n] = acc;
        acc += c.len_utf8();
    }
    char_to_byte[bytes.len()] = acc;

    while i < bytes.len() {
        let c = bytes[i];
        if c == '\'' || c == '"' {
            let quote = c;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == quote {
                    // A doubled quote stays inside the literal.
                    if bytes.get(i + 1) == Some(&quote) {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
        } else if c == '-' && bytes.get(i + 1) == Some(&'-') {
            while i < bytes.len() && bytes[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && bytes.get(i + 1) == Some(&'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == '*' && bytes.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else {
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth = depth.saturating_sub(1);
            } else if c == ',' && depth == 0 {
                parts.push(args[char_to_byte[start]..char_to_byte[i]].trim());
                start = i + 1;
            }
            i += 1;
        }
    }
    parts.push(args[char_to_byte[start.min(bytes.len())]..].trim());
    parts
}

/// Rewrite a whole `{fn NAME(args)}` escape into Trino SQL.
///
/// This is for the functions a rename alone cannot reach: ODBC and Trino
/// agree on the capability but not on the argument syntax, the parentheses,
/// or the numbering. Every one of them is advertised in the `SQL_*_FUNCTIONS`
/// bitmaps in `backend/info.rs`, and the two must stay in step: a bit there
/// without an arm here is a claim an application cannot use.
///
/// Returning `None` falls back to [`remap_scalar_fn`] plus verbatim
/// arguments, which is the right answer for every function Trino spells the
/// same way and for a call whose argument count this cannot honour.
pub(crate) fn rewrite_scalar_fn(name: &str, args: &str) -> Option<String> {
    let upper = name.to_ascii_uppercase();
    let parts = split_args(args);
    let empty = args.trim().is_empty();

    match upper.as_str() {
        // SQL_FN_STR_LOCATE_2: `position(substring IN string)` takes ODBC's
        // argument order. Only the two-argument form: ODBC's optional third
        // argument is a *start offset*, where the third argument of Trino's
        // `strpos` is an occurrence index, so there is nothing to rewrite it
        // to. That is why `SQL_FN_STR_LOCATE` (the three-argument form) is not
        // advertised while `SQL_FN_STR_LOCATE_2` is.
        "LOCATE" if parts.len() == 2 => Some(format!("position({} IN {})", parts[0], parts[1])),

        // SQL_FN_STR_LENGTH / SQL_FN_STR_LTRIM / SQL_FN_STR_RTRIM all turn on
        // ODBC's word "blanks", which means the space character and nothing
        // else. Trino reads the same three operations as whitespace-wide, so
        // each needs the trimmed set pinned to a literal space rather than the
        // name passed through:
        //
        // - LENGTH is specified as "the number of characters in string_exp,
        //   excluding trailing blanks", while Trino's `length` counts them.
        //   Measured against a coordinator, `length(CAST('ab' AS char(5)))` is
        //   5 where ODBC asks for 2, and `length('abc   ')` is 6 where ODBC
        //   asks for 3. The gap is not confined to padded `char(n)`: any value
        //   carrying trailing spaces is counted wrong.
        // - LTRIM and RTRIM are specified as removing leading and trailing
        //   *blanks*. Trino's one-argument `ltrim`/`rtrim` remove every kind of
        //   trailing whitespace, so a tab or a newline is eaten from data ODBC
        //   says to keep.
        //
        // The two-argument forms take the exact set, so `rtrim(x, ' ')` trims
        // spaces and preserves a trailing tab. They are NULL-safe, matching the
        // pass-through they replace.
        "LENGTH" if parts.len() == 1 => Some(format!("length(rtrim({}, ' '))", parts[0])),
        "LTRIM" if parts.len() == 1 => Some(format!("ltrim({}, ' ')", parts[0])),
        "RTRIM" if parts.len() == 1 => Some(format!("rtrim({}, ' ')", parts[0])),

        // SQL_FN_TD_CURDATE / CURTIME and the three ODBC 3.x CURRENT_* forms.
        // Trino takes these as bare SQL-92 keywords, so the whole escape,
        // trailing `()` included, has to go. This is what `remap_scalar_fn`
        // could not express.
        "CURDATE" | "CURRENT_DATE" if empty => Some("current_date".into()),
        "CURTIME" | "CURRENT_TIME" if empty => Some("current_time".into()),
        "CURRENT_TIMESTAMP" if empty => Some("current_timestamp".into()),

        // SQL_FN_SYS_USERNAME / SQL_FN_SYS_DBNAME: bare keywords again.
        // `current_catalog` is NULL when the connection set no catalog, which
        // is the honest answer to "which database am I in" in that case.
        "USERNAME" if empty => Some("current_user".into()),
        "DBNAME" if empty => Some("current_catalog".into()),

        // SQL_FN_TD_TIMESTAMPADD / SQL_FN_TD_TIMESTAMPDIFF: ODBC passes the
        // unit as an unquoted keyword and Trino wants a string literal, so
        // the argument has to be re-quoted, not just the name swapped.
        // Argument order matches: ODBC's TIMESTAMPDIFF(interval, ts1, ts2) is
        // ts2 - ts1, and so is Trino's date_diff(unit, ts1, ts2).
        "TIMESTAMPADD" if parts.len() == 3 => {
            let unit = trino_interval_unit(parts[0])?;
            Some(format!("date_add('{unit}', {}, {})", parts[1], parts[2]))
        }
        "TIMESTAMPDIFF" if parts.len() == 3 => {
            let unit = trino_interval_unit(parts[0])?;
            Some(format!("date_diff('{unit}', {}, {})", parts[1], parts[2]))
        }

        // SQL_FN_CVT_CAST: ODBC passes the target as an unquoted `SQL_*`
        // keyword in an argument position, which is neither a Trino type name
        // nor even valid there: `CONVERT(x, SQL_INTEGER)` reaches the server as
        // a two-argument function call and fails resolving `sql_integer` as a
        // column. The whole call has to become a `CAST`.
        "CONVERT" if parts.len() == 2 => {
            let target = trino_convert_target(parts[1]).or_else(|| {
                tracing::warn!(
                    odbc_type = parts[1],
                    "no Trino type for this ODBC CONVERT target; leaving the escape untranslated"
                );
                None
            })?;
            Some(format!("CAST({} AS {target})", parts[0]))
        }

        // SQL_FN_NUM_RAND: ODBC's optional argument is a seed and the result is
        // a float in [0, 1). Trino's `rand(n)` takes a *bound* and returns an
        // integer in [0, n), so passing the call through verbatim answers a
        // different type over a different range: `{fn RAND(5)}` would yield 0-4
        // rather than a fraction. Trino has no seeded generator to rewrite the
        // seed onto, so it is dropped and the zero-argument form emitted, which
        // keeps the type and the range and loses only reproducibility. Silently
        // returning the wrong distribution is the worse of the two.
        "RAND" if parts.len() == 1 && !empty => {
            tracing::warn!(
                seed = parts[0],
                "Trino has no seeded random(); {{fn RAND(seed)}} is translated to \
                 random(), which is not reproducible"
            );
            Some("random()".into())
        }

        // SQL_FN_NUM_TRUNCATE: ODBC's TRUNCATE takes a `numeric_exp`, which the
        // appendix defines as covering SQL_FLOAT, SQL_REAL and SQL_DOUBLE among
        // others, but Trino's two-argument `truncate` is declared over `decimal`
        // alone. `truncate(CAST(1.99 AS DOUBLE), 1)` does not resolve at all: it
        // fails FUNCTION_NOT_FOUND with "Expected: truncate(decimal(p,s), ...)".
        // Passing the call through therefore works for a decimal column and
        // fails outright for a double or real one, which is exactly the claim
        // this module's header says an advertised bit must not make.
        //
        // Scaling by a power of ten reaches the single-argument `truncate`,
        // which Trino does define over double and real, so the rewrite covers
        // the whole numeric domain. See [`rewrite_truncate`] for how the scale
        // factor is chosen, which is what decides the result's type.
        "TRUNCATE" if parts.len() == 2 => Some(rewrite_truncate(parts[0], parts[1])),

        // SQL_FN_TD_DAYOFWEEK: Trino's `day_of_week` is ISO-numbered
        // (1 = Monday .. 7 = Sunday) and ODBC specifies 1 = Sunday ..
        // 7 = Saturday, so the *value* needs converting, not just the name:
        // `(iso % 7) + 1` maps Monday 1 -> 2 and Sunday 7 -> 1.
        // Renaming alone returns a plausible, silently wrong day.
        "DAYOFWEEK" if parts.len() == 1 => Some(format!("((day_of_week({}) % 7) + 1)", parts[0])),

        // SQL_FN_NUM_ATAN2 has no arm on purpose, and the omission is a
        // decision rather than an oversight.
        //
        // ODBC's appendix reads `ATAN2(float_exp1, float_exp2)` as "the
        // arctangent of the x and y coordinates, specified by float_exp1 and
        // float_exp2, respectively", so the literal text puts x first. Trino's
        // `atan2(y, x)` puts y first, and so does every other implementation
        // that was checked: PostgreSQL, MySQL, Oracle, C, Java, Python, and
        // SQL Server's own `ATN2`, whose documented example evaluates
        // `ATN2(129.44, 35.175643)` to 1.30545, which is atan(129.44/35.175643)
        // and therefore first-argument-is-y.
        //
        // Microsoft's engine thus contradicts Microsoft's own appendix, and
        // psqlodbc, which does remap LOG, LENGTH and DAYOFWEEK exactly as this
        // module does, carries ATAN2 in its table only as a commented-out
        // `built_in` and passes it through untouched. Swapping here would make
        // this the single driver in the ecosystem answering the complementary
        // angle, breaking any application ported from another ODBC driver in
        // order to match a sentence no implementation honours.
        //
        // The deviation from the appendix text is therefore intentional and is
        // pinned by a discriminating case in the integration suite, one whose
        // two readings give different non-zero answers.
        _ => None,
    }
}

/// Largest `|d|` that still scales by an integer literal: 10^18 fits in a
/// Trino `bigint`, 10^19 does not.
const TRUNCATE_LITERAL_SCALE_LIMIT: i32 = 18;

/// The body of the `SQL_FN_NUM_TRUNCATE` rewrite: `{fn TRUNCATE(value, digits)}`
/// scaled into the single-argument `truncate` Trino defines over every numeric
/// type.
///
/// ODBC says TRUNCATE "returns values of the same data type as the input
/// parameters", and which scale factor is used decides whether that holds.
/// `power(10, d)` is double-valued, so it drags a decimal or real argument to
/// double. An integer literal does not: Trino promotes `decimal * bigint` to
/// decimal and `real * bigint` to real, so the argument's own type survives the
/// round trip. Measured against a coordinator, `truncate(CAST(1.99 AS
/// DECIMAL(3,2)) * 10) / 10` is an exact `decimal` 1.9 and the same expression
/// over a `real` stays `real`, where the `power` form answers `double` for
/// both. The decimal's scale does widen, because Trino's decimal division adds
/// scale, but the type an application reads from `SQLDescribeCol` is still
/// SQL_DECIMAL and the value is still exact.
///
/// A literal `digits` is therefore scaled by `10^|d|` written out in full, with
/// the sign choosing multiply-then-divide or divide-then-multiply so a negative
/// `d` zeroes digits to the left of the point, as ODBC specifies. `d == 0` needs
/// no scaling at all.
///
/// Anything else falls back to `power`. That covers `digits` given as a column
/// or a parameter marker, which is legal ODBC and cannot be folded here, and
/// `|d| > 18`, where the literal would exceed `bigint`. Those calls widen to
/// double, which stays the better of the two deviations: a widened numeric type
/// is something an application can still read and work with, where the
/// unrewritten call leaves it FUNCTION_NOT_FOUND and nothing at all.
fn rewrite_truncate(value: &str, digits: &str) -> String {
    match digits.trim().parse::<i32>() {
        Ok(0) => format!("truncate({value})"),
        Ok(d) if (1..=TRUNCATE_LITERAL_SCALE_LIMIT).contains(&d) => {
            let scale = 10i64.pow(d as u32);
            format!("(truncate({value} * {scale}) / {scale})")
        }
        Ok(d) if (-TRUNCATE_LITERAL_SCALE_LIMIT..0).contains(&d) => {
            let scale = 10i64.pow(d.unsigned_abs());
            format!("(truncate({value} / {scale}) * {scale})")
        }
        _ => format!("(truncate({value} * power(10, {digits})) / power(10, {digits}))"),
    }
}

fn render_date(x: &str) -> String {
    format!("DATE {x}")
}
fn render_time(x: &str) -> String {
    format!("TIME {x}")
}
fn render_timestamp(x: &str) -> String {
    format!("TIMESTAMP {x}")
}

/// Trino's `EscapeDialect`: `"`-quoted identifiers (Trino's ANSI-standard
/// quoting) and Trino-spelled date/time/timestamp literals.
pub(crate) fn dialect() -> EscapeDialect {
    EscapeDialect::ansi_default()
        .with_identifier_quotes(&[('"', '"')])
        .with_remap_scalar_fn(remap_scalar_fn)
        .with_rewrite_scalar_fn(rewrite_scalar_fn)
        .with_datetime_renderers(render_date, render_time, render_timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `split_args` is the one piece of parsing this crate does that core
    /// declines to: core hands the argument text over whole because only the
    /// dialect knows each function's arity, and warns that splitting naively
    /// would corrupt `{fn LOCATE(',', x)}`. So the comma cases below are the
    /// point of the function, not edge cases around it: a separator that is
    /// really a character inside a literal, an identifier, a comment or a
    /// nested call.
    #[test]
    fn split_args_splits_only_on_top_level_commas() {
        for (input, expected) in [
            // Nothing to split.
            ("", vec![""]),
            ("x", vec!["x"]),
            ("a, b", vec!["a", "b"]),
            // Whitespace around a separator is not part of the argument.
            ("  a  ,  b  ", vec!["a", "b"]),
            // A comma inside a string literal is data. This is core's own
            // example of what a naive split destroys.
            ("','", vec!["','"]),
            ("',', x", vec!["','", "x"]),
            ("'a,b', 'c,d'", vec!["'a,b'", "'c,d'"]),
            // A doubled quote stays inside the literal, so the comma after it
            // is still data.
            ("'it''s, fine', x", vec!["'it''s, fine'", "x"]),
            // Quoted identifiers follow the same rule as string literals.
            ("\"a,b\", c", vec!["\"a,b\"", "c"]),
            (
                "\"say \"\"hi\"\", now\", c",
                vec!["\"say \"\"hi\"\", now\"", "c"],
            ),
            // A nested call's own arguments are not this call's arguments.
            ("f(a, b), c", vec!["f(a, b)", "c"]),
            ("f(g(a, b), c), d", vec!["f(g(a, b), c)", "d"]),
            // Comments can hide a comma too.
            ("a -- one, two\n, b", vec!["a -- one, two", "b"]),
            ("a /* one, two */, b", vec!["a /* one, two */", "b"]),
            // An empty trailing argument is still an argument: the caller
            // checks arity, so this must not silently look like one fewer.
            ("a,", vec!["a", ""]),
            (",a", vec!["", "a"]),
            // Multi-byte characters must not shift the slice boundaries.
            ("'héllo, wörld', x", vec!["'héllo, wörld'", "x"]),
        ] {
            assert_eq!(
                split_args(input),
                expected,
                "split_args({input:?}) did not split on top-level commas only"
            );
        }
    }

    /// An unbalanced or unterminated argument list must not panic or lose
    /// text. Core only offers the hook when the call's parentheses balance,
    /// but the text between them is arbitrary and this must survive it.
    #[test]
    fn split_args_survives_malformed_input() {
        assert_eq!(split_args("'unterminated, x"), vec!["'unterminated, x"]);
        assert_eq!(split_args("\"unterminated, x"), vec!["\"unterminated, x"]);
        assert_eq!(split_args("f(a, b"), vec!["f(a, b"]);
        assert_eq!(split_args("a) , b"), vec!["a)", "b"]);
        assert_eq!(split_args("/* unterminated, x"), vec!["/* unterminated, x"]);
    }

    /// Each rewrite, in the form core hands it over: the name, and the
    /// argument text between the outer parentheses.
    #[test]
    fn rewrites_produce_trinos_spelling() {
        for (name, args, expected) in [
            // The IN keyword ODBC's LOCATE does not have.
            ("LOCATE", "'b', 'ab'", "position('b' IN 'ab')"),
            ("locate", "'b', 'ab'", "position('b' IN 'ab')"),
            // Bare keywords: the escape's own `()` has to disappear.
            ("CURDATE", "", "current_date"),
            ("CURRENT_DATE", "", "current_date"),
            ("CURTIME", "", "current_time"),
            ("CURRENT_TIME", "", "current_time"),
            ("CURRENT_TIMESTAMP", "", "current_timestamp"),
            ("USERNAME", "", "current_user"),
            ("DBNAME", "", "current_catalog"),
            // The interval keyword becomes a quoted unit.
            ("TIMESTAMPADD", "SQL_TSI_DAY, 1, t", "date_add('day', 1, t)"),
            (
                "TIMESTAMPDIFF",
                "SQL_TSI_YEAR, a, b",
                "date_diff('year', a, b)",
            ),
            // Value conversion, not a rename.
            ("DAYOFWEEK", "d", "((day_of_week(d) % 7) + 1)"),
            // The seed is dropped rather than passed through: Trino reads that
            // argument as a bound. See the arm for why losing reproducibility
            // beats answering a different type.
            ("RAND", "5", "random()"),
        ] {
            assert_eq!(
                rewrite_scalar_fn(name, args).as_deref(),
                Some(expected),
                "{{fn {name}({args})}} rewrote wrongly"
            );
        }
    }

    /// `{fn CONVERT(value, SQL_type)}` becomes a `CAST`, which is what
    /// `SQL_CONVERT_FUNCTIONS` reporting `SQL_FN_CVT_CAST` promises.
    ///
    /// Every ODBC type keyword the spec defines for this escape is covered,
    /// because a client reading the bitmap is entitled to send any of them.
    #[test]
    fn convert_becomes_a_cast_for_every_odbc_type_keyword() {
        for (keyword, trino) in [
            ("SQL_BIGINT", "BIGINT"),
            ("SQL_INTEGER", "INTEGER"),
            ("SQL_SMALLINT", "SMALLINT"),
            ("SQL_TINYINT", "TINYINT"),
            ("SQL_DOUBLE", "DOUBLE"),
            ("SQL_FLOAT", "DOUBLE"),
            ("SQL_REAL", "REAL"),
            ("SQL_DECIMAL", "DECIMAL"),
            ("SQL_NUMERIC", "DECIMAL"),
            ("SQL_BIT", "BOOLEAN"),
            ("SQL_CHAR", "VARCHAR"),
            ("SQL_VARCHAR", "VARCHAR"),
            ("SQL_LONGVARCHAR", "VARCHAR"),
            ("SQL_WCHAR", "VARCHAR"),
            ("SQL_WVARCHAR", "VARCHAR"),
            ("SQL_WLONGVARCHAR", "VARCHAR"),
            ("SQL_BINARY", "VARBINARY"),
            ("SQL_VARBINARY", "VARBINARY"),
            ("SQL_LONGVARBINARY", "VARBINARY"),
            ("SQL_DATE", "DATE"),
            ("SQL_TYPE_DATE", "DATE"),
            ("SQL_TIME", "TIME"),
            ("SQL_TYPE_TIME", "TIME"),
            ("SQL_TIMESTAMP", "TIMESTAMP"),
            ("SQL_TYPE_TIMESTAMP", "TIMESTAMP"),
            ("SQL_GUID", "UUID"),
        ] {
            assert_eq!(
                rewrite_scalar_fn("CONVERT", &format!("x, {keyword}")).as_deref(),
                Some(format!("CAST(x AS {trino})").as_str()),
                "{{fn CONVERT(x, {keyword})}} rewrote wrongly"
            );
        }
    }

    /// The keyword is matched case-insensitively and with surrounding space
    /// tolerated, the same as `TIMESTAMPADD`'s interval keyword.
    #[test]
    fn convert_keyword_is_case_and_space_insensitive() {
        assert_eq!(
            rewrite_scalar_fn("CONVERT", "x,  sql_integer ").as_deref(),
            Some("CAST(x AS INTEGER)")
        );
        assert_eq!(
            rewrite_scalar_fn("convert", "x, Sql_Integer").as_deref(),
            Some("CAST(x AS INTEGER)")
        );
    }

    /// `SQL_CHAR` maps to `VARCHAR`, not to Trino's `CHAR`.
    ///
    /// Measured, not assumed: `CAST('hello world' AS CHAR)` returns `"h"` on a
    /// live coordinator, because a bare `CHAR` in Trino is `CHAR(1)`. Mapping
    /// the ODBC keyword to it would silently truncate every conversion to one
    /// character, which is worse than not translating at all.
    #[test]
    fn convert_to_char_does_not_map_to_trinos_truncating_char() {
        let rewritten = rewrite_scalar_fn("CONVERT", "'hello world', SQL_CHAR")
            .expect("SQL_CHAR is a mapped keyword");
        assert!(
            !rewritten.contains("AS CHAR)"),
            "SQL_CHAR must not become Trino's CHAR(1): {rewritten}"
        );
        assert_eq!(rewritten, "CAST('hello world' AS VARCHAR)");
    }

    /// Declining is how a call this cannot honour reaches the fallback path
    /// unchanged, rather than being rewritten into something wrong.
    #[test]
    fn rewrites_decline_what_they_cannot_honour() {
        // ODBC's three-argument LOCATE takes a start offset; the third
        // argument of Trino's strpos() is an occurrence index, so there is
        // nothing to rewrite it to. This is why SQL_FN_STR_LOCATE_2 is
        // advertised and SQL_FN_STR_LOCATE is not.
        assert_eq!(rewrite_scalar_fn("LOCATE", "'b', 'ab', 2"), None);
        // FRAC_SECOND is billionths of a second in ODBC and Trino's finest
        // unit is millisecond, so it must not be silently accepted.
        assert_eq!(
            rewrite_scalar_fn("TIMESTAMPADD", "SQL_TSI_FRAC_SECOND, 1, t"),
            None
        );
        assert_eq!(
            rewrite_scalar_fn("TIMESTAMPADD", "SQL_TSI_NONSENSE, 1, t"),
            None
        );
        // Wrong arity falls through rather than producing malformed SQL.
        assert_eq!(rewrite_scalar_fn("TIMESTAMPADD", "SQL_TSI_DAY, 1"), None);
        assert_eq!(rewrite_scalar_fn("DAYOFWEEK", "a, b"), None);
        // `{fn RAND()}` is already Trino's `rand()`, so only the seeded form
        // needs rewriting; the bare one falls through untouched.
        assert_eq!(rewrite_scalar_fn("RAND", ""), None);
        assert_eq!(rewrite_scalar_fn("RAND", "a, b"), None);
        // The precision forms of CURRENT_TIME/CURRENT_TIMESTAMP pass through
        // instead: Trino accepts `CURRENT_TIMESTAMP(6)` as written.
        assert_eq!(rewrite_scalar_fn("CURRENT_TIMESTAMP", "6"), None);
        // ODBC's "blanks" is the space alone, so all three pin the trimmed set
        // rather than taking Trino's whitespace-wide default.
        assert_eq!(
            rewrite_scalar_fn("LENGTH", "x").as_deref(),
            Some("length(rtrim(x, ' '))")
        );
        assert_eq!(
            rewrite_scalar_fn("LTRIM", "x").as_deref(),
            Some("ltrim(x, ' ')")
        );
        assert_eq!(
            rewrite_scalar_fn("RTRIM", "x").as_deref(),
            Some("rtrim(x, ' ')")
        );
        // Only the one-argument forms ODBC defines.
        assert_eq!(rewrite_scalar_fn("LENGTH", "x, y"), None);
        assert_eq!(rewrite_scalar_fn("RTRIM", "x, y"), None);

        // TRUNCATE scales into the single-argument `truncate`, which Trino
        // defines over double and real; the two-argument one is decimal-only.
        // A literal digit count scales by an integer, which is what keeps a
        // decimal or real argument out of double.
        assert_eq!(
            rewrite_scalar_fn("TRUNCATE", "x, 2").as_deref(),
            Some("(truncate(x * 100) / 100)")
        );
        // A negative one zeroes digits left of the point, so it divides first.
        assert_eq!(
            rewrite_scalar_fn("TRUNCATE", "x, -1").as_deref(),
            Some("(truncate(x / 10) * 10)")
        );
        // Nothing to scale by.
        assert_eq!(
            rewrite_scalar_fn("TRUNCATE", "x, 0").as_deref(),
            Some("truncate(x)")
        );
        // Whitespace around the count is still a literal.
        assert_eq!(
            rewrite_scalar_fn("TRUNCATE", "x,  3 ").as_deref(),
            Some("(truncate(x * 1000) / 1000)")
        );
        // A digit count that is not a literal is legal ODBC and cannot be
        // folded, so it takes the `power` fallback and widens to double.
        assert_eq!(
            rewrite_scalar_fn("TRUNCATE", "x, d").as_deref(),
            Some("(truncate(x * power(10, d)) / power(10, d))")
        );
        assert_eq!(
            rewrite_scalar_fn("TRUNCATE", "x, ?").as_deref(),
            Some("(truncate(x * power(10, ?)) / power(10, ?))")
        );
        // 10^19 exceeds bigint, so the boundary falls back too.
        assert_eq!(
            rewrite_scalar_fn("TRUNCATE", "x, 18").as_deref(),
            Some("(truncate(x * 1000000000000000000) / 1000000000000000000)")
        );
        assert_eq!(
            rewrite_scalar_fn("TRUNCATE", "x, 19").as_deref(),
            Some("(truncate(x * power(10, 19)) / power(10, 19))")
        );
        assert_eq!(
            rewrite_scalar_fn("TRUNCATE", "x, -18").as_deref(),
            Some("(truncate(x / 1000000000000000000) * 1000000000000000000)")
        );
        assert_eq!(
            rewrite_scalar_fn("TRUNCATE", "x, -19").as_deref(),
            Some("(truncate(x * power(10, -19)) / power(10, -19))")
        );
        // ODBC has no one-argument TRUNCATE, so there is nothing to rewrite.
        assert_eq!(rewrite_scalar_fn("TRUNCATE", "x"), None);

        // A function with no rewrite is remap_scalar_fn's business.
        assert_eq!(rewrite_scalar_fn("UCASE", "x"), None);
        // ATAN2 reaches Trino with its arguments in the order the application
        // wrote them, deviating from the ODBC appendix on purpose. Neither hook
        // touches it, so both have to decline; see the comment on the absent
        // arm. Asserting this keeps the deviation a decision under test rather
        // than something a later edit can reverse without noticing.
        assert_eq!(rewrite_scalar_fn("ATAN2", "1, 2"), None);
        assert_eq!(remap_scalar_fn("ATAN2"), None);
        // An ODBC type keyword with no Trino equivalent, and a value that is
        // not a type keyword at all. Guessing a target type would produce a
        // cast the application never asked for.
        assert_eq!(
            rewrite_scalar_fn("CONVERT", "x, SQL_INTERVAL_DAY_TO_SECOND"),
            None
        );
        assert_eq!(rewrite_scalar_fn("CONVERT", "x, NOT_A_TYPE"), None);
        // Wrong arity falls through rather than producing malformed SQL.
        assert_eq!(rewrite_scalar_fn("CONVERT", "x"), None);
        assert_eq!(rewrite_scalar_fn("CONVERT", "x, SQL_INTEGER, y"), None);
    }

    /// `SQL_TIMEDATE_ADD_INTERVALS` / `SQL_TIMEDATE_DIFF_INTERVALS` name the
    /// units `TIMESTAMPADD`/`TIMESTAMPDIFF` accept, so every bit advertised
    /// there must be a unit [`trino_interval_unit`] can rewrite.
    /// Otherwise the driver names an interval whose escape then falls through
    /// untranslated.
    ///
    /// `FRAC_SECOND` is the one that must stay out: ODBC defines it as
    /// billionths of a second, Trino's finest unit is `millisecond`, and
    /// mapping one to the other would be a factor of a million out.
    #[test]
    fn advertised_intervals_are_all_rewritable() {
        use stackable_odbc_core::types::{
            SQL_FN_TSI_DAY, SQL_FN_TSI_FRAC_SECOND, SQL_FN_TSI_HOUR, SQL_FN_TSI_MINUTE,
            SQL_FN_TSI_MONTH, SQL_FN_TSI_QUARTER, SQL_FN_TSI_SECOND, SQL_FN_TSI_WEEK,
            SQL_FN_TSI_YEAR,
        };

        let advertised = crate::backend::TRINO_TIMESTAMP_INTERVALS;
        for (flag, keyword) in [
            (SQL_FN_TSI_SECOND, "SQL_TSI_SECOND"),
            (SQL_FN_TSI_MINUTE, "SQL_TSI_MINUTE"),
            (SQL_FN_TSI_HOUR, "SQL_TSI_HOUR"),
            (SQL_FN_TSI_DAY, "SQL_TSI_DAY"),
            (SQL_FN_TSI_WEEK, "SQL_TSI_WEEK"),
            (SQL_FN_TSI_MONTH, "SQL_TSI_MONTH"),
            (SQL_FN_TSI_QUARTER, "SQL_TSI_QUARTER"),
            (SQL_FN_TSI_YEAR, "SQL_TSI_YEAR"),
        ] {
            assert_ne!(
                advertised & flag,
                0,
                "{keyword} is rewritable but unclaimed"
            );
            assert!(
                trino_interval_unit(keyword).is_some(),
                "{keyword} is claimed but has no Trino unit"
            );
        }

        assert_eq!(advertised & SQL_FN_TSI_FRAC_SECOND, 0);
        assert_eq!(trino_interval_unit("SQL_TSI_FRAC_SECOND"), None);
    }

    /// A comma inside a literal must survive the whole rewrite, not just
    /// `split_args` in isolation: this is core's stated worst case.
    #[test]
    fn rewrite_preserves_a_comma_inside_a_literal() {
        assert_eq!(
            rewrite_scalar_fn("LOCATE", "',', x").as_deref(),
            Some("position(',' IN x)")
        );
    }

    #[test]
    fn ucase_maps_to_upper() {
        assert_eq!(remap_scalar_fn("UCASE"), Some("upper"));
        assert_eq!(remap_scalar_fn("ucase"), Some("upper"));
    }

    #[test]
    fn lcase_maps_to_lower() {
        assert_eq!(remap_scalar_fn("LCASE"), Some("lower"));
    }

    #[test]
    fn char_maps_to_chr() {
        assert_eq!(remap_scalar_fn("CHAR"), Some("chr"));
    }

    #[test]
    fn log_maps_to_ln() {
        assert_eq!(remap_scalar_fn("LOG"), Some("ln"));
    }

    #[test]
    fn ifnull_maps_to_coalesce() {
        assert_eq!(remap_scalar_fn("IFNULL"), Some("coalesce"));
    }

    #[test]
    fn dayofmonth_maps_to_day_of_month() {
        assert_eq!(remap_scalar_fn("DAYOFMONTH"), Some("day_of_month"));
    }

    #[test]
    fn dayofyear_maps_to_day_of_year() {
        assert_eq!(remap_scalar_fn("DAYOFYEAR"), Some("day_of_year"));
    }

    #[test]
    fn abs_passes_through() {
        assert_eq!(remap_scalar_fn("ABS"), None);
    }

    #[test]
    fn ceiling_passes_through() {
        assert_eq!(remap_scalar_fn("CEILING"), None);
    }

    #[test]
    fn concat_passes_through() {
        assert_eq!(remap_scalar_fn("CONCAT"), None);
    }

    #[test]
    fn substring_passes_through() {
        assert_eq!(remap_scalar_fn("SUBSTRING"), None);
    }

    #[test]
    fn now_passes_through() {
        assert_eq!(remap_scalar_fn("NOW"), None);
    }

    // NOT remapped despite being advertised; see the module doc.
    #[test]
    fn locate_not_remapped() {
        assert_eq!(remap_scalar_fn("LOCATE"), None);
    }

    #[test]
    fn dayofweek_not_remapped() {
        assert_eq!(remap_scalar_fn("DAYOFWEEK"), None);
    }

    #[test]
    fn curdate_not_remapped() {
        assert_eq!(remap_scalar_fn("CURDATE"), None);
    }

    #[test]
    fn timestampadd_not_remapped() {
        assert_eq!(remap_scalar_fn("TIMESTAMPADD"), None);
    }

    #[test]
    fn username_not_remapped() {
        assert_eq!(remap_scalar_fn("USERNAME"), None);
    }

    #[test]
    fn date_literal_is_trino_form() {
        assert_eq!(render_date("'2020-01-01'"), "DATE '2020-01-01'");
    }

    #[test]
    fn time_literal_is_trino_form() {
        assert_eq!(render_time("'10:00:00'"), "TIME '10:00:00'");
    }

    #[test]
    fn timestamp_literal_is_trino_form() {
        assert_eq!(
            render_timestamp("'2020-01-01 00:00:00'"),
            "TIMESTAMP '2020-01-01 00:00:00'"
        );
    }

    #[test]
    fn dialect_uses_double_quote_identifiers() {
        assert_eq!(dialect().identifier_quotes(), &[('"', '"')]);
    }

    #[test]
    fn end_to_end_fn_and_date_translate() {
        let out = stackable_odbc_core::escape::translate_escapes(
            "SELECT {fn UCASE(name)} FROM t WHERE d = {d '2020-01-01'}",
            &dialect(),
        )
        .unwrap();
        assert_eq!(out, "SELECT upper(name) FROM t WHERE d = DATE '2020-01-01'");
    }
}
