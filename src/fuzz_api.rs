//! The entry points the fuzz targets in `fuzz/` drive.
//!
//! The three surfaces worth fuzzing here are private modules, and two of them
//! expose their interesting functions as `pub(crate)`. Rather than widen each
//! module and each item, this one module re-exports exactly what the targets
//! call, so the fuzzed surface is a list someone can read in one screen.
//!
//! Behind the default-off `fuzzing` feature, so none of this reaches the
//! shipped `cdylib`. Nothing here is part of the driver's supported API: it
//! exists to let an out-of-tree crate reach in, and it may change with the
//! internals it wraps.
//!
//! What each target covers, and why it is here rather than in
//! `stackable-odbc-core`:
//!
//! - [`json_to_column_value`]: core fuzzes `write_column_value`, which turns a
//!   `ColumnValue` into a caller's buffer. Nothing fuzzes the step before it,
//!   where a coordinator's JSON becomes that `ColumnValue`, and that step is
//!   this crate's own dozen-odd temporal and interval parsers.
//! - [`type_name_precision`] and its neighbours: parse Trino type signatures
//!   (`decimal(10,2)`, `timestamp(6) with time zone`) that arrive as text in
//!   `DESCRIBE INPUT` rows and `information_schema` queries.
//! - [`translate_escapes`]: core owns the ODBC escape parser and fuzzes it
//!   against its own dialects; this crate owns the Trino dialect. Only the
//!   composition exercises `escape_dialect::split_args`, so only this repo can
//!   fuzz it.
//! - [`trino_connect_params`]: core's `ConnectParams::parse` splits the
//!   connection string; the per-key value parsing on top of it is this
//!   crate's.

use serde_json::Value;
use stackable_odbc_core::types::ColumnValue;
use trino_rust_client::TrinoTy;

pub use crate::type_conversion::{
    json_to_column_value, trino_type_name_to_sql_type, type_name_precision, type_name_scale,
};

/// Convert one coordinator JSON value under its declared Trino type.
///
/// A thin alias for [`json_to_column_value`], kept so a target can name the
/// whole read-path conversion without importing the type-name helpers.
pub fn json_value(val: Value, ty: &TrinoTy) -> ColumnValue {
    json_to_column_value(val, ty)
}

/// Run core's ODBC escape translator with this driver's dialect.
///
/// Returns `None` where the translator reports an error, which is a normal
/// outcome for malformed input and not the property under test. The property
/// is that neither the parser nor the dialect callbacks panic.
pub fn translate_escapes(sql: &str) -> Option<String> {
    stackable_odbc_core::escape::translate_escapes(sql, &crate::escape_dialect::dialect()).ok()
}

/// Parse a connection string the way `SQLDriverConnectW` does: core splits it
/// into keys, then this driver reads the values it knows.
///
/// The error is rendered to a `String` rather than returned as-is, so the
/// target exercises the `Display` formatting too. Error formatting is its own
/// panic surface: several arms interpolate the offending value.
pub fn trino_connect_params(connection_string: &str) -> Result<(), String> {
    use crate::backend::types::connect_params::TrinoConnectParams;

    let params = stackable_odbc_core::types::ConnectParams::parse(connection_string)
        .map_err(|e| e.to_string())?;
    TrinoConnectParams::try_from(&params)
        .map(|_| ())
        .map_err(|e| e.to_string())
}
