//! `SQLDescribeParam` support, answered from Trino's `DESCRIBE INPUT`.
//!
//! Trino describes a prepared statement's parameters, so this driver does not
//! have to fall back to core's generic `VARCHAR(SQL_DEFAULT_PARAM_SIZE)` —
//! the answer that makes a client send a number as text and get a type error
//! back.
//!
//! Reaching it takes three statements, because `DESCRIBE INPUT` names a
//! prepared statement rather than taking SQL:
//!
//! ```text
//! PREPARE <name> FROM <sql>      -- registers it in the session
//! DESCRIBE INPUT <name>          -- one row per parameter: Position, Type
//! DEALLOCATE PREPARE <name>      -- drops it again
//! ```
//!
//! The `PREPARE` must go through the client directly, **never** through the
//! bound-parameter path: `params::interpolate` would replace the statement's
//! own `?` markers with the (absent) parameter values, and Trino would
//! register a statement with no parameters at all. That is not hypothetical —
//! it is what `SQLExecDirect("PREPARE p FROM ... ?")` does today, and it is
//! why `DESCRIBE INPUT` looks empty when driven that way.
//!
//! `DEALLOCATE` is not optional housekeeping. A session's prepared statements
//! ride on **every** subsequent request as an `X-Trino-Prepared-Statement`
//! header, so leaving a large query text registered would grow each later
//! request by that much and eventually breach the coordinator's header limit.

use stackable_odbc_core::types::ParamDescriptor;

use super::{TrinoConnection, TrinoError, query_all_rows};
use crate::type_conversion::{trino_type_name_to_sql_type, type_name_precision, type_name_scale};

/// The parameters of one statement, as `DESCRIBE INPUT` reported them.
#[derive(Debug, Clone)]
pub struct CachedParams {
    /// The SQL these describe, as `describe_param` received it.
    sql: String,
    /// Indexed by position: element `n` describes ODBC parameter `n + 1`.
    params: Vec<ParamDescriptor>,
}

/// The prepared-statement name used to reach `DESCRIBE INPUT`.
///
/// Fixed rather than generated: a fresh name per call would accumulate in the
/// session until `DEALLOCATE` ran, and re-using one bounds the damage to a
/// single entry if a `DEALLOCATE` is ever lost. Prefixed to keep it clear of
/// any name an application prepared itself.
const DESCRIBE_PARAM_STATEMENT: &str = "stackable_odbc_describe_input";

/// Build one descriptor from a Trino type signature.
///
/// Precision and scale come from the signature itself — `char(20)`,
/// `decimal(10,2)` — through the same two helpers `SQLColumns` uses, so a
/// parameter and a column of the same Trino type never disagree. A type
/// carrying neither keeps `ParamDescriptor::new`'s zeroes, which is what the
/// spec has a driver report when the value is not applicable.
fn param_descriptor(type_name: &str) -> ParamDescriptor {
    let mut descriptor = ParamDescriptor::new(trino_type_name_to_sql_type(type_name));
    if let Some(precision) = type_name_precision(type_name).and_then(|p| u64::try_from(p).ok()) {
        descriptor = descriptor.with_parameter_size(precision);
    }
    if let Some(scale) = type_name_scale(type_name).and_then(|s| i16::try_from(s).ok()) {
        descriptor = descriptor.with_decimal_digits(scale);
    }
    descriptor
}

/// Convert `DESCRIBE INPUT`'s rows into one descriptor per parameter.
///
/// Each row is `Position` (0-based) and `Type` (a Trino type signature such as
/// `bigint` or `char(20)`). The result is indexed by position, so element `n`
/// describes ODBC parameter `n + 1`.
///
/// `Position` is read rather than trusted to match row order: indexing by row
/// order would silently misdescribe every parameter if Trino ever reordered
/// them, and a wrong specific type is indistinguishable from a real answer.
fn describe_input_rows_to_params(rows: &[Vec<serde_json::Value>]) -> Vec<ParamDescriptor> {
    let mut by_position: Vec<(usize, ParamDescriptor)> = rows
        .iter()
        .filter_map(|row| {
            let position = usize::try_from(row.first()?.as_i64()?).ok()?;
            let type_name = row.get(1)?.as_str()?;
            Some((position, param_descriptor(type_name)))
        })
        .collect();
    by_position.sort_by_key(|(position, _)| *position);
    by_position.into_iter().map(|(_, param)| param).collect()
}

/// Describe parameter `parameter_number` (1-based) of `sql`.
///
/// `Ok(None)` for anything that cannot be answered — a statement Trino
/// declines to prepare, a parameter past the end, a failed round trip. Core
/// then reports its documented uniform guess, which is a better outcome than
/// failing `SQLDescribeParam` outright or inventing a specific type.
pub(super) fn describe_param(
    conn: &TrinoConnection,
    sql: &str,
    parameter_number: u16,
) -> Result<Option<ParamDescriptor>, TrinoError> {
    tracing::debug!(%sql, parameter_number, "TrinoBackend::describe_param");

    // Parameter numbers are 1-based; core rejects 0 before reaching a
    // backend, so this is belt and braces rather than a live path.
    let Some(index) = usize::from(parameter_number).checked_sub(1) else {
        return Ok(None);
    };

    let mut cache = conn
        .describe_param_cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if cache.as_ref().is_none_or(|cached| cached.sql != sql) {
        match fetch_input_params(conn, sql) {
            Ok(params) => {
                *cache = Some(CachedParams {
                    sql: sql.to_string(),
                    params,
                });
            }
            Err(e) => {
                // Not an error path for the application: Trino declines to
                // prepare plenty of legitimate statements, and core's fallback
                // is a usable answer.
                tracing::debug!(%sql, error = %e, "DESCRIBE INPUT failed; leaving the parameter to core's default");
                return Ok(None);
            }
        }
    }

    Ok(cache
        .as_ref()
        .and_then(|cached| cached.params.get(index))
        .copied())
}

/// Run a statement that returns no result set, discarding its update count.
fn execute_statement(conn: &TrinoConnection, sql: String) -> Result<(), TrinoError> {
    conn.runtime
        .block_on(conn.client.execute(sql))
        .map_err(super::map_trino_error)?;
    Ok(())
}

/// Run the `PREPARE` / `DESCRIBE INPUT` / `DEALLOCATE` round trip.
///
/// `DEALLOCATE` runs whether or not the describe succeeded, so a failure
/// cannot leave the statement registered on the session.
fn fetch_input_params(
    conn: &TrinoConnection,
    sql: &str,
) -> Result<Vec<ParamDescriptor>, TrinoError> {
    // `PREPARE` and `DEALLOCATE` produce no result set, so they go through
    // `Client::execute`; `query_all_rows` deserialises rows and fails on a
    // statement that declares no columns.
    execute_statement(
        conn,
        format!("PREPARE {DESCRIBE_PARAM_STATEMENT} FROM {sql}"),
    )?;

    let described = query_all_rows(conn, format!("DESCRIBE INPUT {DESCRIBE_PARAM_STATEMENT}"));

    if let Err(e) = execute_statement(
        conn,
        format!("DEALLOCATE PREPARE {DESCRIBE_PARAM_STATEMENT}"),
    ) {
        // Worth a warning rather than a failure: the describe itself may have
        // succeeded, and the cost of a leaked entry is a larger header on
        // later requests, not a wrong answer.
        tracing::warn!(error = %e, "could not deallocate the describe-input statement");
    }

    let rows: Vec<Vec<serde_json::Value>> = described?
        .into_iter()
        .map(|row| row.into_json().into_iter().collect())
        .collect();

    Ok(describe_input_rows_to_params(&rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stackable_odbc_core::types::SqlDataType;

    /// `DESCRIBE INPUT` returns one row per parameter: `Position` (0-based)
    /// and `Type` (a Trino type signature).
    fn row(position: i64, ty: &str) -> Vec<serde_json::Value> {
        vec![serde_json::json!(position), serde_json::json!(ty)]
    }

    #[test]
    fn describe_input_rows_become_one_descriptor_per_parameter_in_position_order() {
        let params = describe_input_rows_to_params(&[row(0, "bigint"), row(1, "char(20)")]);

        assert_eq!(params.len(), 2);
        assert_eq!(params[0].data_type(), SqlDataType::EXT_BIG_INT);
        assert_eq!(params[1].data_type(), SqlDataType::EXT_W_CHAR);
    }

    #[test]
    fn parameters_are_ordered_by_position_not_by_row_order() {
        // Indexing by row order would describe parameter 1 as the char and
        // parameter 2 as the bigint. A wrong specific type is worse than no
        // answer, because an application cannot tell it apart from a real one.
        let params = describe_input_rows_to_params(&[row(1, "char(20)"), row(0, "bigint")]);

        assert_eq!(params.len(), 2);
        assert_eq!(params[0].data_type(), SqlDataType::EXT_BIG_INT);
        assert_eq!(params[1].data_type(), SqlDataType::EXT_W_CHAR);
    }

    #[test]
    fn a_parametric_type_carries_its_precision_and_scale() {
        // Sizing a buffer is what SQLDescribeParam is for, so the type alone
        // is not enough: char(20) must say 20, and decimal(10,2) must say
        // both 10 and 2.
        let params = describe_input_rows_to_params(&[row(0, "char(20)"), row(1, "decimal(10,2)")]);

        assert_eq!(params[0].parameter_size(), 20);
        assert_eq!(params[0].decimal_digits(), 0);
        assert_eq!(params[1].parameter_size(), 10);
        assert_eq!(params[1].decimal_digits(), 2);
    }
}
