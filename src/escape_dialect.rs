//! Trino escape-translation dialect: `"`-quoted identifiers, Trino date/time
//! literals, and the `{fn}` scalar-function remap for the names Trino spells
//! differently from ODBC.
//!
//! Both function hooks are traceable to the `SQL_*_FUNCTIONS` bitmaps
//! `src/backend/info.rs` advertises for Trino, and the three are kept in exact
//! correspondence: a bit is advertised only if `{fn NAME(...)}` translates
//! into Trino SQL that runs. A bit without a translation is a capability an
//! application is told it has and then cannot use.
//!
//! There are two hooks because there are two kinds of difference:
//!
//! - [`remap_scalar_fn`] swaps the identifier in front of the parentheses and
//!   never sees the arguments. That is all a pure spelling difference needs:
//!   `UCASE` → `upper`, `LOG` → `ln`.
//! - [`rewrite_scalar_fn`] receives the whole call and returns its
//!   replacement, for the functions where ODBC and Trino agree on the
//!   capability but not on the syntax: `LOCATE(a, b)` → `position(a IN b)`,
//!   the `CURDATE`/`USERNAME` family → bare keywords with the `()` removed,
//!   `TIMESTAMPADD(SQL_TSI_DAY, ...)` → `date_add('day', ...)` with the unit
//!   re-quoted, and `DAYOFWEEK` → an expression converting Trino's ISO day
//!   numbering to ODBC's.
//!
//! `POSITION` needs neither: ODBC spells it `POSITION(exp IN exp)`, which is
//! already Trino's syntax, so the escape passes through untouched.
//!
//! `NOW`, `MONTH`, `QUARTER`, `WEEK`, `YEAR`, `HOUR`, `MINUTE`, `SECOND`,
//! `EXTRACT` and the numeric/string functions not listed as arms below are
//! already spelled identically in Trino (verified against
//! <https://trino.io/docs/current/functions/string.html>,
//! <https://trino.io/docs/current/functions/math.html>, and
//! <https://trino.io/docs/current/functions/datetime.html>), so they pass
//! through unchanged (`None`).
use stackable_odbc_core::escape::EscapeDialect;

/// Remap an ODBC `{fn NAME(...)}` scalar-function name to Trino's spelling.
/// `None` passes the name through unchanged (same spelling in both).
pub(crate) fn remap_scalar_fn(name: &str) -> Option<&'static str> {
    match name.to_ascii_uppercase().as_str() {
        // SQL_FN_STR_UCASE / SQL_FN_STR_LCASE / SQL_FN_STR_CHAR
        "UCASE" => Some("upper"),
        "LCASE" => Some("lower"),
        "CHAR" => Some("chr"),
        // SQL_FN_NUM_LOG — ODBC's LOG is the natural logarithm; Trino's own
        // `log(b, x)` is base-b and a different function, so this maps to
        // `ln()` specifically (see the SQL_NUMERIC_FUNCTIONS doc comment in
        // backend/info.rs).
        "LOG" => Some("ln"),
        // SQL_FN_SYS_IFNULL — Trino has no `ifnull`, but two-argument
        // `coalesce(a, b)` is exactly equivalent (same doc comment).
        "IFNULL" => Some("coalesce"),
        // SQL_FN_TD_DAYOFMONTH / SQL_FN_TD_DAYOFYEAR — same semantics as
        // ODBC (1-31 / 1-366), just spelled with underscores in Trino.
        // DAYOFWEEK is deliberately NOT remapped here: its numbering differs,
        // so it goes through `rewrite_scalar_fn` instead.
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
/// The keywords are the ones the spec defines for the conversion escape, and
/// the whole set is covered because `SQL_CONVERT_FUNCTIONS` reports
/// `SQL_FN_CVT_CAST`: a client reading that bitmap may send any of them, and
/// one without an arm here reaches Trino as a bare identifier and fails with
/// `COLUMN_NOT_FOUND`.
///
/// Two mappings are worth their own note, both measured against a live
/// coordinator rather than read off the documentation:
///
/// - `SQL_CHAR` maps to `VARCHAR`, not to Trino's `CHAR`. A bare `CHAR` in
///   Trino is `CHAR(1)`, so `CAST('hello world' AS CHAR)` returns `"h"`, and the
///   escape would silently truncate every conversion to one character. ODBC's
///   `{fn CONVERT}` carries no length to give `CHAR(n)` instead.
/// - `SQL_FLOAT` maps to `DOUBLE`. ODBC's `SQL_FLOAT` is double precision;
///   Trino's `REAL` is the single-precision type, which is `SQL_REAL`.
///
/// The `SQL_INTERVAL_*` keywords are deliberately absent. Trino's interval
/// types cannot be reached by a bare `CAST` from an arbitrary expression, so
/// there is nothing honest to rewrite them to, and declining leaves the call
/// on the fallback path instead of casting to something the application did
/// not ask for.
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
/// Core hands the argument text over whole, deliberately: only the dialect
/// knows each function's arity, and a naive split would corrupt
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
        // SQL_FN_STR_LOCATE_2 — `position(substring IN string)` takes ODBC's
        // argument order. Only the two-argument form: ODBC's optional third
        // argument is a *start offset*, where the third argument of Trino's
        // `strpos` is an occurrence index, so there is nothing to rewrite it
        // to. That is why `SQL_FN_STR_LOCATE` (the three-argument form) is not
        // advertised while `SQL_FN_STR_LOCATE_2` is.
        "LOCATE" if parts.len() == 2 => Some(format!("position({} IN {})", parts[0], parts[1])),

        // SQL_FN_TD_CURDATE / CURTIME and the three ODBC 3.x CURRENT_* forms.
        // Trino takes these as bare SQL-92 keywords, so the whole escape,
        // trailing `()` included, has to go. This is what `remap_scalar_fn`
        // could not express.
        "CURDATE" | "CURRENT_DATE" if empty => Some("current_date".into()),
        "CURTIME" | "CURRENT_TIME" if empty => Some("current_time".into()),
        "CURRENT_TIMESTAMP" if empty => Some("current_timestamp".into()),

        // SQL_FN_SYS_USERNAME / SQL_FN_SYS_DBNAME — bare keywords again.
        // `current_catalog` is NULL when the connection set no catalog, which
        // is the honest answer to "which database am I in" in that case.
        "USERNAME" if empty => Some("current_user".into()),
        "DBNAME" if empty => Some("current_catalog".into()),

        // SQL_FN_TD_TIMESTAMPADD / SQL_FN_TD_TIMESTAMPDIFF — ODBC passes the
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

        // SQL_FN_CVT_CAST — ODBC passes the target as an unquoted `SQL_*`
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

        // SQL_FN_TD_DAYOFWEEK — Trino's `day_of_week` is ISO-numbered
        // (1 = Monday .. 7 = Sunday) and ODBC specifies 1 = Sunday ..
        // 7 = Saturday, so the *value* needs converting, not just the name:
        // `(iso % 7) + 1` maps Monday 1 -> 2 and Sunday 7 -> 1.
        // Renaming alone would have returned a plausible, silently wrong day.
        "DAYOFWEEK" if parts.len() == 1 => Some(format!("((day_of_week({}) % 7) + 1)", parts[0])),

        _ => None,
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
    /// deliberately declined to: core hands the argument text over whole
    /// because only the dialect knows each function's arity, and warns that
    /// splitting naively would corrupt `{fn LOCATE(',', x)}`. So the comma
    /// cases below are the point of the function, not edge cases around it:
    /// a separator that is really a character inside a literal, an
    /// identifier, a comment or a nested call.
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
        // The precision forms of CURRENT_TIME/CURRENT_TIMESTAMP pass through
        // instead: Trino accepts `CURRENT_TIMESTAMP(6)` as written.
        assert_eq!(rewrite_scalar_fn("CURRENT_TIMESTAMP", "6"), None);
        // A function with no rewrite is remap_scalar_fn's business.
        assert_eq!(rewrite_scalar_fn("UCASE", "x"), None);
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
    /// there must be a unit [`trino_interval_unit`] can actually rewrite.
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

    // Deliberately NOT remapped despite being advertised; see module doc.
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
