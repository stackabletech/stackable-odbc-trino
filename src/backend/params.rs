//! Parameter binding for the Trino backend.
//!
//! The Trino REST API has no wire-level parameter binding: `Client::get` takes
//! only a SQL string, and the client's `PREPARE`/`EXECUTE USING` support is
//! fixed at connection-build time so it cannot carry per-statement values.
//! Bound parameters are therefore rendered into the SQL as literals here.
//!
//! Because this builds SQL text from application input, every variant that can
//! carry attacker-controlled characters is either quoted (`String`, `Json`) or
//! validated against a strict grammar (`Decimal`). Numeric and temporal
//! variants are formatted from typed values and cannot inject.

use stackable_odbc_core::types::ColumnValue;

use super::TrinoError;

/// Substitute `?` placeholders in `sql` with the rendered `params`.
///
/// Placeholders inside single-quoted string literals are ignored, matching the
/// placeholder counting in `stackable_odbc_core::ffi::params::count_params`: the two
/// must agree or the parameter indices shift.
pub(super) fn interpolate_params(sql: &str, params: &[ColumnValue]) -> Result<String, TrinoError> {
    let mut out = String::with_capacity(sql.len());
    let mut next = 0usize;
    let mut in_string = false;
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        match (c, in_string) {
            ('\'', false) => {
                in_string = true;
                out.push(c);
            }
            ('\'', true) => {
                out.push(c);
                // An escaped quote ('') stays inside the string literal.
                if chars.peek() == Some(&'\'') {
                    out.push('\'');
                    chars.next();
                } else {
                    in_string = false;
                }
            }
            ('?', false) => {
                let value = params.get(next).ok_or_else(|| TrinoError::General {
                    message: format!(
                        "not enough parameters bound: placeholder {} has no value ({} bound)",
                        next + 1,
                        params.len()
                    ),
                })?;
                out.push_str(&render_literal(value)?);
                next += 1;
            }
            _ => out.push(c),
        }
    }

    if next != params.len() {
        return Err(TrinoError::General {
            message: format!(
                "parameter count mismatch: {} bound, {} placeholders in statement",
                params.len(),
                next
            ),
        });
    }

    Ok(out)
}

/// Render a single bound value as a Trino SQL literal.
fn render_literal(value: &ColumnValue) -> Result<String, TrinoError> {
    let unsupported = |what: &str| TrinoError::General {
        message: format!("{what} is not supported as a bound parameter"),
    };

    Ok(match value {
        ColumnValue::Null => "NULL".to_string(),
        ColumnValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),

        ColumnValue::I8(v) => v.to_string(),
        ColumnValue::I16(v) => v.to_string(),
        ColumnValue::I32(v) => v.to_string(),
        ColumnValue::I64(v) => v.to_string(),

        // NaN and the infinities have no Trino literal form.
        ColumnValue::F32(v) => {
            if !v.is_finite() {
                return Err(unsupported("non-finite REAL"));
            }
            format!("REAL '{v}'")
        }
        ColumnValue::F64(v) => {
            if !v.is_finite() {
                return Err(unsupported("non-finite DOUBLE"));
            }
            format!("DOUBLE '{v}'")
        }

        ColumnValue::String(s) => quote_string(s),
        // Trino parses JSON from a string literal, so the same quoting applies.
        ColumnValue::Json(s) => format!("JSON {}", quote_string(s)),

        // Decimal is carried as text to preserve precision, so it is the one
        // numeric variant that could smuggle SQL. Validate before emitting.
        ColumnValue::Decimal(s) => {
            if !is_plain_decimal(s) {
                return Err(TrinoError::General {
                    message: format!("invalid DECIMAL parameter value: {s:?}"),
                });
            }
            format!("DECIMAL '{s}'")
        }

        ColumnValue::Bytes(b) => {
            use std::fmt::Write;
            let mut hex = String::with_capacity(b.len() * 2);
            for byte in b {
                let _ = write!(hex, "{byte:02X}");
            }
            format!("X'{hex}'")
        }
        ColumnValue::Guid(b) => format!(
            "UUID '{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}'",
            b[0],
            b[1],
            b[2],
            b[3],
            b[4],
            b[5],
            b[6],
            b[7],
            b[8],
            b[9],
            b[10],
            b[11],
            b[12],
            b[13],
            b[14],
            b[15]
        ),

        ColumnValue::Date { year, month, day } => {
            format!("DATE '{year:04}-{month:02}-{day:02}'")
        }
        ColumnValue::Time {
            hour,
            minute,
            second,
            fraction,
        } => format!(
            "TIME '{hour:02}:{minute:02}:{second:02}.{:03}'",
            fraction / 1_000_000
        ),
        ColumnValue::Timestamp {
            year,
            month,
            day,
            hour,
            minute,
            second,
            fraction,
        } => format!(
            "TIMESTAMP '{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{:03}'",
            fraction / 1_000_000
        ),
        ColumnValue::TimestampTz {
            year,
            month,
            day,
            hour,
            minute,
            second,
            fraction,
            timezone_offset_minutes,
        } => {
            let sign = if *timezone_offset_minutes < 0 {
                '-'
            } else {
                '+'
            };
            let abs = timezone_offset_minutes.abs();
            format!(
                "TIMESTAMP '{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{:03} {sign}{:02}:{:02}'",
                fraction / 1_000_000,
                abs / 60,
                abs % 60
            )
        }

        // Constructing these safely requires per-element type context that the
        // ODBC bind API does not provide.
        ColumnValue::Array(_) => return Err(unsupported("ARRAY")),
        ColumnValue::Map(_) => return Err(unsupported("MAP")),
        ColumnValue::Row(_) => return Err(unsupported("ROW")),
        ColumnValue::IntervalYearMonth { .. } => return Err(unsupported("INTERVAL YEAR TO MONTH")),
        ColumnValue::IntervalDayTime { .. } => return Err(unsupported("INTERVAL DAY TO SECOND")),

        // ColumnValue is #[non_exhaustive]. A variant added upstream must fail
        // loudly here rather than be rendered by a guess: this function emits
        // SQL text, so an unreviewed rendering is an injection risk. The value
        // itself is deliberately not included in the message.
        _ => return Err(unsupported("this parameter type")),
    })
}

/// Wrap `s` in single quotes, doubling any embedded quote.
///
/// Backslash is not an escape character in Trino string literals (standard SQL
/// semantics), so doubling the quote is sufficient and complete.
fn quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push('\'');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// True when `s` is a bare decimal number: optional sign, digits, optional
/// fractional part. Deliberately strict: anything else is rejected rather
/// than escaped, because a DECIMAL literal cannot be quoted.
fn is_plain_decimal(s: &str) -> bool {
    let body = s
        .strip_prefix('-')
        .or_else(|| s.strip_prefix('+'))
        .unwrap_or(s);
    if body.is_empty() {
        return false;
    }
    match body.split_once('.') {
        None => body.bytes().all(|b| b.is_ascii_digit()),
        Some((int, frac)) => {
            !int.is_empty()
                && !frac.is_empty()
                && int.bytes().all(|b| b.is_ascii_digit())
                && frac.bytes().all(|b| b.is_ascii_digit())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interp(sql: &str, params: &[ColumnValue]) -> String {
        interpolate_params(sql, params).expect("interpolation succeeded")
    }

    #[test]
    fn integer_parameter_is_substituted() {
        assert_eq!(
            interp("SELECT * FROM t WHERE id = ?", &[ColumnValue::I64(42)]),
            "SELECT * FROM t WHERE id = 42"
        );
    }

    #[test]
    fn multiple_parameters_substitute_in_order() {
        assert_eq!(
            interp(
                "SELECT ? , ?",
                &[ColumnValue::I32(1), ColumnValue::String("a".into())]
            ),
            "SELECT 1 , 'a'"
        );
    }

    #[test]
    fn null_renders_as_null_keyword() {
        assert_eq!(interp("SELECT ?", &[ColumnValue::Null]), "SELECT NULL");
    }

    #[test]
    fn string_quotes_are_doubled() {
        assert_eq!(
            interp("SELECT ?", &[ColumnValue::String("O'Brien".into())]),
            "SELECT 'O''Brien'"
        );
    }

    #[test]
    fn sql_injection_attempt_stays_inside_the_literal() {
        // The classic payload must end up as data, not as statement structure.
        let evil = "'; DROP TABLE users; --";
        let out = interp(
            "SELECT * FROM t WHERE name = ?",
            &[ColumnValue::String(evil.into())],
        );
        assert_eq!(
            out,
            "SELECT * FROM t WHERE name = '''; DROP TABLE users; --'"
        );
        // Exactly one quoted literal: quote count must be even.
        assert_eq!(out.matches('\'').count() % 2, 0);
    }

    #[test]
    fn placeholder_inside_string_literal_is_not_substituted() {
        // Matches count_params, which also ignores `?` inside a literal.
        assert_eq!(
            interp("SELECT '?' , ?", &[ColumnValue::I32(7)]),
            "SELECT '?' , 7"
        );
    }

    #[test]
    fn escaped_quote_inside_literal_is_preserved() {
        assert_eq!(
            interp("SELECT 'it''s ?' , ?", &[ColumnValue::I32(1)]),
            "SELECT 'it''s ?' , 1"
        );
    }

    #[test]
    fn too_few_parameters_is_an_error() {
        let err = interpolate_params("SELECT ?, ?", &[ColumnValue::I32(1)]).unwrap_err();
        assert!(
            format!("{err:?}").contains("not enough parameters"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn too_many_parameters_is_an_error() {
        let err = interpolate_params("SELECT ?", &[ColumnValue::I32(1), ColumnValue::I32(2)])
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("parameter count mismatch"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn decimal_rejects_non_numeric_text() {
        let err = interpolate_params(
            "SELECT ?",
            &[ColumnValue::Decimal("1'; DROP TABLE t; --".into())],
        )
        .unwrap_err();
        assert!(
            format!("{err:?}").contains("invalid DECIMAL"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn decimal_accepts_plain_numbers() {
        assert_eq!(
            interp("SELECT ?", &[ColumnValue::Decimal("-12.345".into())]),
            "SELECT DECIMAL '-12.345'"
        );
    }

    #[test]
    fn bytes_render_as_hex_literal() {
        assert_eq!(
            interp("SELECT ?", &[ColumnValue::Bytes(vec![0xDE, 0xAD, 0x00])]),
            "SELECT X'DEAD00'"
        );
    }

    #[test]
    fn bool_renders_as_keyword() {
        assert_eq!(
            interp(
                "SELECT ?, ?",
                &[ColumnValue::Bool(true), ColumnValue::Bool(false)]
            ),
            "SELECT TRUE, FALSE"
        );
    }

    #[test]
    fn date_and_timestamp_render_with_type_prefix() {
        assert_eq!(
            interp(
                "SELECT ?",
                &[ColumnValue::Date {
                    year: 2026,
                    month: 7,
                    day: 4
                }]
            ),
            "SELECT DATE '2026-07-04'"
        );
        assert_eq!(
            interp(
                "SELECT ?",
                &[ColumnValue::Timestamp {
                    year: 2026,
                    month: 7,
                    day: 4,
                    hour: 1,
                    minute: 2,
                    second: 3,
                    fraction: 500_000_000
                }]
            ),
            "SELECT TIMESTAMP '2026-07-04 01:02:03.500'"
        );
    }

    #[test]
    fn non_finite_floats_are_rejected() {
        assert!(interpolate_params("SELECT ?", &[ColumnValue::F64(f64::NAN)]).is_err());
        assert!(interpolate_params("SELECT ?", &[ColumnValue::F64(f64::INFINITY)]).is_err());
    }

    #[test]
    fn unsupported_composite_types_are_rejected() {
        let err = interpolate_params("SELECT ?", &[ColumnValue::Array(vec![])]).unwrap_err();
        assert!(
            format!("{err:?}").contains("ARRAY"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn statement_without_placeholders_is_unchanged() {
        assert_eq!(interp("SELECT 1", &[]), "SELECT 1");
    }
}
