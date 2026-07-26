//! Trino escape-translation dialect: `"`-quoted identifiers, Trino date/time
//! literals, and the `{fn}` scalar-function remap for the names Trino spells
//! differently from ODBC.
//!
//! The remap table is traceable to the `SQL_*_FUNCTIONS` bitmaps
//! `crates/stackable-odbc-trino/src/backend/info.rs` advertises for Trino.
//! Every arm below corresponds to one advertised `SQL_FN_*`
//! bit whose ODBC name Trino spells differently *and* for which a bare
//! name substitution (`stackable_odbc_core::escape` only ever swaps the identifier in
//! front of the parentheses, it does not rewrite argument syntax or values)
//! still produces valid, semantically equivalent Trino SQL.
//!
//! Advertised names that are NOT remapped here, and why a name-only swap
//! would be wrong for them:
//!
//! - `LOCATE`, `POSITION` (`SQL_FN_STR_LOCATE`, `SQL_FN_STR_POSITION`): Trino's
//!   equivalent is `position(substring IN string)`, a special reserved-word
//!   syntax with an `IN` keyword, not a comma-separated argument list. Renaming
//!   just the identifier would produce `position(a, b)`, which Trino's grammar
//!   rejects (it does not accept a plain function-call form for `position`).
//! - `CURDATE`, `CURTIME` (`SQL_FN_TD_CURDATE`, `SQL_FN_TD_CURTIME`) and
//!   `CURRENT_DATE`, `CURRENT_TIME`, `CURRENT_TIMESTAMP`: Trino's
//!   `current_date` / `current_time` / `current_timestamp` are bare SQL-92
//!   keywords taken *without* parentheses. The ODBC escape always includes
//!   `()` (e.g. `{fn CURDATE()}`), and the translator appends whatever
//!   follows the name verbatim, so renaming still leaves a trailing `()`
//!   that Trino's grammar does not accept for these keywords.
//! - `TIMESTAMPADD`, `TIMESTAMPDIFF` (`SQL_FN_TD_TIMESTAMPADD`,
//!   `SQL_FN_TD_TIMESTAMPDIFF`): Trino's `date_add(unit, value, ts)` /
//!   `date_diff(unit, ts1, ts2)` take `unit` as a string literal (`'day'`),
//!   while ODBC's first argument is an unquoted interval keyword
//!   (`SQL_TSI_DAY`). A name-only rename cannot re-quote that argument.
//! - `USERNAME`, `DBNAME` (`SQL_FN_SYS_USERNAME`, `SQL_FN_SYS_DBNAME`):
//!   Trino's `current_user` / `current_catalog` are bare keywords without
//!   parentheses; same problem as `CURDATE`/`CURTIME` above.
//! - `DAYOFWEEK` (`SQL_FN_TD_DAYOFWEEK`): Trino's `day_of_week()` is
//!   ISO-numbered (1 = Monday), while ODBC's `DAYOFWEEK` is 1 = Sunday (see
//!   the `SQL_TIMEDATE_FUNCTIONS` doc comment in `backend/info.rs`). Remapping
//!   the name would silently return a *different, wrong* day number rather
//!   than failing loudly, worse than leaving it untranslated.
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
        // DAYOFWEEK is deliberately NOT remapped here; see the module doc
        // comment above.
        "DAYOFMONTH" => Some("day_of_month"),
        "DAYOFYEAR" => Some("day_of_year"),
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
    EscapeDialect {
        identifier_quotes: &[('"', '"')],
        remap_scalar_fn,
        render_date,
        render_time,
        render_timestamp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(dialect().identifier_quotes, &[('"', '"')]);
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
