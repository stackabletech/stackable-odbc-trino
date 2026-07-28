//! Catalog metadata for the Trino backend: the ten catalog functions
//! (`tables`, `columns`, `primary_keys`, `foreign_keys`, `statistics`,
//! `special_columns`, `table_privileges`, `column_privileges`, `procedures`,
//! `procedure_columns`) plus the `catalogs` / `schemas` / `table_types`
//! enumerations, built by querying Trino's `information_schema` and
//! `system.jdbc`, with the private WHERE-clause and ODBC-wildcard helpers
//! those queries share.
//!
//! Every function here returns rows, never a statement: core converts them to
//! `ColumnValue`s in spec column order, sorts them into the order each
//! function's spec page defines, and serves the result set. That is why none
//! of the queries below carries an `ORDER BY`.
//!
//! Six of the ten return no rows because Trino has nothing to answer them
//! with. Each says which on its own doc comment; `AGENTS.md` has the table.

use std::borrow::Cow;

use stackable_odbc_core::types::{
    ColumnPrivilegeRow, ColumnRow, ForeignKeyRow, IdentifierType, Nullable, PrimaryKeyRow,
    ProcedureColumnRow, ProcedureRow, Scope, SpecialColumnRow, SqlDataType, StatisticsRow,
    TablePrivilegeRow, TableRow,
};

use super::{TrinoConnection, TrinoError, info::trino_bare_type_name, query_all_rows};
use crate::type_conversion::{trino_type_name_to_sql_type, type_name_precision, type_name_scale};

/// Check whether a string contains unescaped ODBC wildcard characters.
///
/// The ODBC spec (§8.3 "Pattern Value Arguments") defines `%` (match any
/// sequence) and `_` (match one character) as wildcards in catalog function
/// arguments such as SQLTablesW and SQLColumnsW.  A backslash escapes
/// wildcards: `\_` means a literal underscore, `\%` means a literal percent.
///
/// This function returns `true` if the string contains at least one
/// **unescaped** wildcard, i.e. a `%` or `_` that is not preceded by `\`.
fn has_odbc_wildcards(s: &str) -> bool {
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        if (bytes[i] == b'%' || bytes[i] == b'_') && (i == 0 || bytes[i - 1] != b'\\') {
            return true;
        }
    }
    false
}

/// Build a SQL filter condition for an `information_schema` query.
///
/// This is part of the **driver-internal** implementation of ODBC catalog
/// functions (SQLTablesW, SQLColumnsW, etc.).  When an ODBC application
/// (or the Power Query Mashup Engine) calls e.g. `SQLColumnsW(catalog,
/// schema, table, column)`, the driver must translate those arguments into
/// a SQL query against Trino's `information_schema` tables.  This function
/// builds individual WHERE conditions for that query.
///
/// The .mez connector is **not** involved here.  The .mez controls how
/// Power Query generates user-facing SQL (via AstVisitor / SqlCapabilities),
/// but ODBC catalog functions are entirely the driver's responsibility.
/// The .mez's `SQLColumns` callback only receives the *result table* after
/// the driver has already queried Trino.
///
/// # Wildcard handling
///
/// The ODBC spec allows catalog function arguments to contain `%` and `_`
/// wildcards, with `\` as the escape character.  ODBC applications encode
/// literal special characters with a backslash: `call\_center` means the
/// exact table name `call_center`, not a single-character wildcard match.
///
/// - **With wildcards** (unescaped `%` or `_`): emit a `LIKE` clause with
///   an explicit `ESCAPE '\'` so Trino honours the backslash convention.
/// - **Without wildcards**: emit an exact `=` match, stripping the escape
///   backslashes so `call\_center` becomes `call_center`.
fn push_filter(conditions: &mut Vec<String>, column: &str, value: &str) {
    let escaped = value.replace('\'', "''");
    if has_odbc_wildcards(&escaped) {
        conditions.push(format!("{column} LIKE '{escaped}' ESCAPE '\\'"));
    } else {
        let literal = escaped.replace("\\_", "_").replace("\\%", "%");
        conditions.push(format!("{column} = '{literal}'"));
    }
}

/// Build the SQL WHERE clause for an information_schema.tables query.
/// Filters are only included if Some and non-empty/non-wildcard.
fn build_tables_where_clause(
    catalog: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
) -> String {
    let mut conditions: Vec<String> = Vec::new();
    if let Some(cat) = catalog.filter(|s| !s.is_empty() && *s != "%") {
        push_filter(&mut conditions, "table_catalog", cat);
    }
    if let Some(sch) = schema.filter(|s| !s.is_empty() && *s != "%") {
        push_filter(&mut conditions, "table_schema", sch);
    }
    if let Some(tbl) = table.filter(|s| !s.is_empty() && *s != "%") {
        push_filter(&mut conditions, "table_name", tbl);
    }
    if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    }
}

/// Build the SQL WHERE clause for an information_schema.columns query.
/// Filters are only included if Some and non-empty/non-wildcard.
fn build_columns_where_clause(
    catalog: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
    column: Option<&str>,
) -> String {
    let mut conditions: Vec<String> = Vec::new();
    if let Some(cat) = catalog.filter(|s| !s.is_empty() && *s != "%") {
        push_filter(&mut conditions, "table_catalog", cat);
    }
    if let Some(sch) = schema.filter(|s| !s.is_empty() && *s != "%") {
        push_filter(&mut conditions, "table_schema", sch);
    }
    if let Some(tbl) = table.filter(|s| !s.is_empty() && *s != "%") {
        push_filter(&mut conditions, "table_name", tbl);
    }
    if let Some(col) = column.filter(|s| !s.is_empty() && *s != "%") {
        push_filter(&mut conditions, "column_name", col);
    }
    if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    }
}

/// Parse precision and scale from a Trino data_type string.
/// E.g. "varchar(100)" → (Some(100), None), "decimal(10,2)" → (Some(10), Some(2)).
fn parse_trino_precision_scale(data_type: &str) -> (Option<i32>, Option<i32>) {
    (type_name_precision(data_type), type_name_scale(data_type))
}

/// Bytes per character used for `CHAR_OCTET_LENGTH`. UTF-16 encodes a character
/// in at most 4 bytes (a surrogate pair).
const BYTES_PER_CHAR: i32 = 4;

/// `NUM_PREC_RADIX` for a numeric column: Trino reports precision for every
/// numeric type in decimal digits, including `REAL` and `DOUBLE`.
const NUM_PREC_RADIX_DECIMAL: i16 = 10;

/// `CHAR_OCTET_LENGTH` for a column: the maximum length in bytes of a
/// character column. All other data types, including binary, return NULL.
///
/// Character columns store their declared length in UTF-16 characters, so it
/// is multiplied by `BYTES_PER_CHAR`; returns NULL when that product does not
/// fit an `i32` (Trino reports unbounded varchar as `varchar(2147483647)` in
/// some connectors, and `2147483647 * 4` overflows).
///
/// `VARBINARY` (→ `SQL_LONGVARBINARY`) is deliberately NOT given a byte-length
/// branch here. Trino's type system carries no length parameter for
/// `varbinary` at all; verified against a live coordinator:
/// `information_schema.columns.data_type` reports the bare string
/// `"varbinary"` for every binary column, with nothing for `type_name_precision`
/// to parse (`TrinoTypeName::Varbinary` has `has_precision_param() == false`
/// and `fixed_precision() == None`). A prior fix added a binary branch here
/// keyed on `ty_precision`, but since that argument is unconditionally `None`
/// for `EXT_LONG_VAR_BINARY`, the branch could never execute: NULL was, and
/// remains, the honest answer for Trino `CHAR_OCTET_LENGTH` on binary
/// columns.
fn char_octet_length(sql_type: SqlDataType, ty_precision: Option<i32>) -> Option<i32> {
    let is_character = matches!(
        sql_type,
        SqlDataType::EXT_W_VARCHAR
            | SqlDataType::EXT_W_CHAR
            | SqlDataType::VARCHAR
            | SqlDataType::CHAR
    );

    if is_character {
        return ty_precision.and_then(|n| n.checked_mul(BYTES_PER_CHAR));
    }
    None
}

/// The catalog names, for `SQLTables`' `SQL_ALL_CATALOGS` enumeration.
///
/// Uses `system.jdbc.catalogs`, which works without a default session catalog —
/// `information_schema` does not, and this is exactly the call an application
/// makes before it has picked one.
pub(super) fn catalogs(conn: &TrinoConnection) -> Result<Vec<String>, TrinoError> {
    tracing::debug!("TrinoBackend::catalogs");

    let sql = "SELECT table_cat FROM system.jdbc.catalogs";
    let rows = query_all_rows(conn, sql.to_string())?;
    Ok(rows
        .into_iter()
        .filter_map(|row| Some(row.into_json().into_iter().next()?.as_str()?.to_string()))
        .collect())
}

/// The schema names, for `SQLTables`' `SQL_ALL_SCHEMAS` enumeration.
///
/// Uses `system.jdbc.schemas` for the same reason as [`catalogs`]. Core NULLs
/// out every column but `TABLE_SCHEM`, which is what the spec requires of this
/// enumeration.
pub(super) fn schemas(conn: &TrinoConnection) -> Result<Vec<String>, TrinoError> {
    tracing::debug!("TrinoBackend::schemas");

    let sql = "SELECT table_schem FROM system.jdbc.schemas";
    let rows = query_all_rows(conn, sql.to_string())?;
    Ok(rows
        .into_iter()
        .filter_map(|row| Some(row.into_json().into_iter().next()?.as_str()?.to_string()))
        .collect())
}

/// The table types Trino has, for `SQLTables`' `SQL_ALL_TABLE_TYPES`
/// enumeration.
///
/// These are the two `information_schema.tables.table_type` values [`tables`]
/// maps and reports; anything else is dropped there, so listing more here
/// would advertise a type no row can ever carry.
pub(super) fn table_types() -> Vec<Cow<'static, str>> {
    vec![
        Cow::Borrowed(TABLE_TYPE_TABLE),
        Cow::Borrowed(TABLE_TYPE_VIEW),
    ]
}

/// ODBC's `TABLE_TYPE` for Trino's `BASE TABLE`.
const TABLE_TYPE_TABLE: &str = "TABLE";

/// ODBC's `TABLE_TYPE` for Trino's `VIEW`.
const TABLE_TYPE_VIEW: &str = "VIEW";

pub(super) fn tables(
    conn: &TrinoConnection,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
    table_types: &[String],
) -> Result<Vec<TableRow>, TrinoError> {
    tracing::debug!(catalog, schema, table, ?table_types, "TrinoBackend::tables");

    // No ORDER BY: core sorts the result set into the spec's order. The three
    // `SQL_ALL_*` enumerations never reach here either — core serves them from
    // `catalogs`, `schemas` and `table_types` above.
    let sql = format!(
        "SELECT table_catalog, table_schema, table_name, table_type \
         FROM information_schema.tables{}",
        build_tables_where_clause(catalog, schema, table)
    );

    let rows = query_all_rows(conn, sql)?;

    // Core split and unquoted the value list; an empty slice is "no filter".
    // The spec has applications supply table types in upper case, so folding
    // here is tolerance for those that do not, not a requirement.
    let allowed_types: Vec<String> = table_types.iter().map(|t| t.to_uppercase()).collect();

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let mut vals = row.into_json().into_iter();
            let cat_val = vals.next()?;
            let sch_val = vals.next()?;
            let name = vals.next()?.as_str()?.to_string();
            let raw_type = vals.next()?.as_str()?.to_uppercase();
            // Trino reports "BASE TABLE"; ODBC expects "TABLE".
            let odbc_type = match raw_type.as_str() {
                "BASE TABLE" => TABLE_TYPE_TABLE,
                "VIEW" => TABLE_TYPE_VIEW,
                other => {
                    tracing::warn!(other, "unknown table_type from Trino, skipping row");
                    return None;
                }
            };

            if !allowed_types.is_empty() && !allowed_types.iter().any(|a| a == odbc_type) {
                return None;
            }

            Some(TableRow {
                catalog: cat_val.as_str().map(str::to_string),
                schema: sch_val.as_str().map(str::to_string),
                name: Some(name),
                table_type: Some(odbc_type.to_string()),
                remarks: None,
            })
        })
        .collect())
}

pub(super) fn columns(
    conn: &TrinoConnection,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
    column: Option<&str>,
) -> Result<Vec<ColumnRow>, TrinoError> {
    tracing::debug!(catalog, schema, table, column, "TrinoBackend::columns");

    // 0-based column positions in the SELECT list below.
    // Must stay in sync with the query.
    #[derive(Clone, Copy)]
    enum QueryCol {
        TableCatalog = 0,
        TableSchema = 1,
        TableName = 2,
        ColumnName = 3,
        OrdinalPosition = 4,
        ColumnDefault = 5,
        IsNullable = 6,
        DataType = 7,
    }
    impl QueryCol {
        fn idx(self) -> usize {
            self as usize
        }
    }

    // Note: Trino's information_schema.columns does not expose
    // character_maximum_length, numeric_precision, or numeric_scale.
    // Precision and scale are derived from the data_type string instead
    // (e.g. "varchar(100)" → precision 100, "decimal(10,2)" → prec 10 scale 2).
    //
    // No ORDER BY: core sorts the result set into the spec's order
    // (TABLE_CAT, TABLE_SCHEM, TABLE_NAME, ORDINAL_POSITION).
    let sql = format!(
        "SELECT table_catalog, table_schema, table_name, column_name, \
                ordinal_position, column_default, is_nullable, data_type \
         FROM information_schema.columns{}",
        build_columns_where_clause(catalog, schema, table, column)
    );

    let rows = query_all_rows(conn, sql)?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let vals: Vec<serde_json::Value> = row.into_json().into_iter().collect();
            let get_str = |qc: QueryCol| -> Option<String> {
                vals.get(qc.idx())?.as_str().map(str::to_string)
            };
            let get_i32 = |qc: QueryCol| -> Option<i32> {
                vals.get(qc.idx())?
                    .as_i64()
                    .and_then(|v| i32::try_from(v).ok())
            };

            let table_cat = get_str(QueryCol::TableCatalog);
            let table_sch = get_str(QueryCol::TableSchema);
            let table_name = match get_str(QueryCol::TableName) {
                Some(s) => s,
                None => {
                    tracing::warn!("table_name was not a string, skipping row");
                    return None;
                }
            };
            let col_name = match get_str(QueryCol::ColumnName) {
                Some(s) => s,
                None => {
                    tracing::warn!("column_name was not a string, skipping row");
                    return None;
                }
            };
            let ordinal = match get_i32(QueryCol::OrdinalPosition) {
                Some(n) => n,
                None => {
                    tracing::warn!("ordinal_position was not an integer, skipping row");
                    return None;
                }
            };
            let col_def = get_str(QueryCol::ColumnDefault);
            let is_null_str = get_str(QueryCol::IsNullable).unwrap_or_default();
            let data_type_raw = get_str(QueryCol::DataType).unwrap_or_default();

            let sql_type = trino_type_name_to_sql_type(&data_type_raw);

            // TYPE_NAME in SQLColumnsW must match SQLGetTypeInfo's TYPE_NAME
            // (uppercase base name without parameters, see
            // `trino_bare_type_name`'s doc comment for the spec citation).
            // Also used, identically, for SQL_DESC_TYPE_NAME in execute.rs,
            // so the two never disagree.
            let data_type = trino_bare_type_name(&data_type_raw, sql_type);
            let nullable = if is_null_str.eq_ignore_ascii_case("YES") {
                Nullable::SqlNullable
            } else if is_null_str.eq_ignore_ascii_case("NO") {
                Nullable::SqlNoNulls
            } else {
                Nullable::SqlNullableUnknown
            };

            let is_numeric = matches!(
                sql_type,
                SqlDataType::EXT_TINY_INT
                    | SqlDataType::SMALLINT
                    | SqlDataType::INTEGER
                    | SqlDataType::EXT_BIG_INT
                    | SqlDataType::REAL
                    | SqlDataType::DOUBLE
                    | SqlDataType::DECIMAL
            );
            // Precision/scale are parsed from the data_type string since
            // Trino's information_schema.columns lacks dedicated columns.
            let (ty_precision, ty_scale) = parse_trino_precision_scale(&data_type_raw);
            // COLUMN_SIZE is reported for every type `type_name_precision`
            // can resolve a value for: parametric types (VARCHAR/CHAR/
            // DECIMAL, read from the type string) and fixed-precision types
            // (the integer/float/boolean/date/time family, via
            // `TrinoTypeName::fixed_precision`). Gating on `ty_precision`
            // directly, rather than re-deriving a separate list of
            // "types with a size" here, means a type only needs to be
            // taught to `fixed_precision`/`has_precision_param` once; no
            // second enumeration to keep in sync (see: this exact class of
            // bug for DATE/TIME/TIMESTAMP/BOOLEAN, and CHAR_OCTET_LENGTH's
            // separate VARBINARY miss below).
            let col_size = ty_precision;
            let num_prec_radix = if is_numeric {
                Some(NUM_PREC_RADIX_DECIMAL)
            } else {
                None
            };
            // DECIMAL_DIGITS. Spec (SQLColumns): "The total number of
            // significant digits to the right of the decimal point. For
            // SQL_TYPE_TIME and SQL_TYPE_TIMESTAMP, this column contains the
            // number of digits in the fractional seconds component. ... NULL
            // is returned for data types where DECIMAL_DIGITS is not
            // applicable." So this is meaningful for DECIMAL/NUMERIC *and*
            // TIME/TIMESTAMP (integer/floating-point types report NULL);
            // `ty_scale` already carries the right quantity for both cases
            // (see `type_name_scale`), this just needs to stop discarding it
            // for the datetime types.
            let decimal_digits = if matches!(
                sql_type,
                SqlDataType::DECIMAL | SqlDataType::TIME | SqlDataType::TIMESTAMP
            ) {
                ty_scale.and_then(|s| i16::try_from(s).ok())
            } else {
                None
            };
            let char_octet = char_octet_length(sql_type, ty_precision);

            Some(ColumnRow {
                catalog: table_cat,
                schema: table_sch,
                table_name,
                column_name: col_name,
                data_type: sql_type.0,
                type_name: data_type,
                column_size: col_size,
                buffer_length: None,
                decimal_digits,
                num_prec_radix,
                nullable: nullable.into(),
                remarks: None,
                column_def: col_def,
                sql_data_type: sql_type.0,
                sql_datetime_sub: None,
                char_octet_length: char_octet,
                ordinal_position: ordinal,
                is_nullable: Some(nullable.as_is_nullable_str().to_string()),
            })
        })
        .collect())
}

/// 0-based positions in `table_privileges`' SELECT list. Must stay in sync
/// with the query in [`table_privileges`].
#[derive(Clone, Copy)]
enum PrivilegeCol {
    TableCatalog = 0,
    TableSchema = 1,
    TableName = 2,
    Grantor = 3,
    Grantee = 4,
    PrivilegeType = 5,
    IsGrantable = 6,
}

/// Convert one `information_schema.table_privileges` row to a
/// [`TablePrivilegeRow`], or drop it if a column the spec marks not-NULL is
/// missing or is not a string.
///
/// Split out from [`table_privileges`] because it is the only part of that
/// function a live coordinator in this project's test stack cannot exercise:
/// neither test catalog implements permission management, so the query is
/// always empty there (`GRANT` on either answers `NOT_SUPPORTED`). The unit
/// tests feed it the rows a coordinator with `sql-standard` security would
/// return.
fn table_privilege_row(vals: &[serde_json::Value]) -> Option<TablePrivilegeRow> {
    let get = |col: PrivilegeCol| -> Option<String> {
        vals.get(col as usize)?.as_str().map(str::to_string)
    };

    // TABLE_NAME, GRANTEE and PRIVILEGE are the three columns the spec marks
    // "not NULL"; a row missing one is not a row an application can use.
    let table_name = get(PrivilegeCol::TableName)?;
    let grantee = get(PrivilegeCol::Grantee)?;
    let privilege = get(PrivilegeCol::PrivilegeType)?;

    Some(TablePrivilegeRow {
        catalog: get(PrivilegeCol::TableCatalog),
        schema: get(PrivilegeCol::TableSchema),
        table_name,
        // Nullable in both directions: Trino leaves `grantor` NULL for a
        // privilege nobody explicitly granted, and ODBC's GRANTOR is nullable.
        grantor: get(PrivilegeCol::Grantor),
        grantee,
        privilege,
        // Trino spells this 'YES'/'NO', which is exactly ODBC's vocabulary for
        // IS_GRANTABLE, so it passes through unmapped.
        is_grantable: get(PrivilegeCol::IsGrantable),
    })
}

/// Return the table-level privileges on the matching tables.
///
/// Trino models these: every catalog has an `information_schema.table_privileges`
/// whose columns line up with ODBC's, and its own JDBC driver reads the same
/// table for `DatabaseMetaData.getTablePrivileges()`.
///
/// It is populated from the connector's permission management, so it is
/// non-empty only for connectors that implement it — Hive and Iceberg under
/// `sql-standard` security, say. A connector without it answers with zero rows
/// rather than an error, which is why this queries unconditionally instead of
/// gating on the catalog. Both catalogs in this project's test stack are in
/// that group, so the integration tests can assert the call's success and
/// shape but never a row; [`table_privilege_row`] carries the unit tests that
/// cover the conversion itself.
///
/// Note that Trino's `information_schema` is synthesised by Trino, not passed
/// through to the underlying database: a `GRANT` issued directly in PostgreSQL
/// is visible in PostgreSQL's own `information_schema.table_privileges` and
/// **not** in the `postgresql` catalog's, because the base JDBC connector
/// implements no permission management. Verified against the test stack.
pub(super) fn table_privileges(
    conn: &TrinoConnection,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
) -> Result<Vec<TablePrivilegeRow>, TrinoError> {
    tracing::debug!(catalog, schema, table, "TrinoBackend::table_privileges");

    // No ORDER BY: core sorts by TABLE_CAT, TABLE_SCHEM, TABLE_NAME,
    // PRIVILEGE, GRANTEE — note that PRIVILEGE outranks GRANTEE here.
    let sql = format!(
        "SELECT table_catalog, table_schema, table_name, grantor, grantee, \
                privilege_type, is_grantable \
         FROM information_schema.table_privileges{}",
        build_tables_where_clause(catalog, schema, table)
    );

    let rows = query_all_rows(conn, sql)?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let vals: Vec<serde_json::Value> = row.into_json().into_iter().collect();
            let converted = table_privilege_row(&vals);
            if converted.is_none() {
                tracing::warn!("table_privileges row missing a non-NULL column, skipping");
            }
            converted
        })
        .collect())
}

/// Return the column-level privileges on a single table.
///
/// Trino exposes no column-level privilege metadata at all: there is no
/// `information_schema.column_privileges` in any catalog, and no `system.jdbc`
/// equivalent. Privileges in Trino are granted on a table, never on a column,
/// so there is nothing to narrow [`table_privileges`] down with either.
///
/// Core defaults this to no rows, but it is stated here so the reason is
/// recorded and the call is logged like every other backend method.
pub(super) fn column_privileges(
    _conn: &TrinoConnection,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
    column: Option<&str>,
) -> Result<Vec<ColumnPrivilegeRow>, TrinoError> {
    tracing::debug!(
        catalog,
        schema,
        table,
        column,
        "TrinoBackend::column_privileges (empty — Trino grants on tables, not columns)"
    );
    Ok(Vec::new())
}

/// Return the stored procedures matching the given filters.
///
/// Trino has procedures — `CALL system.runtime.kill_query(...)` is one, and
/// calling an unregistered name answers `PROCEDURE_NOT_FOUND` — but it
/// publishes no metadata describing them. `system.jdbc.procedures` exists for
/// JDBC compatibility and is hardwired empty (verified against a live
/// coordinator: `system.runtime.kill_query` is callable while that table
/// returns zero rows), and `system.metadata` has no procedures table.
///
/// This is consistent with the `SQL_ACCESSIBLE_PROCEDURES` = `"N"` this driver
/// already reports from `info`.
pub(super) fn procedures(
    _conn: &TrinoConnection,
    catalog: Option<&str>,
    schema: Option<&str>,
    proc_name: Option<&str>,
) -> Result<Vec<ProcedureRow>, TrinoError> {
    tracing::debug!(
        catalog,
        schema,
        proc_name,
        "TrinoBackend::procedures (empty — Trino publishes no procedure metadata)"
    );
    Ok(Vec::new())
}

/// Return the parameters and result-set columns of the matching procedures.
///
/// Empty for the same reason as [`procedures`]: `system.jdbc.procedure_columns`
/// is the matching hardwired-empty JDBC compatibility view. A driver that
/// cannot name a procedure cannot describe its parameters either.
pub(super) fn procedure_columns(
    _conn: &TrinoConnection,
    catalog: Option<&str>,
    schema: Option<&str>,
    proc_name: Option<&str>,
    column: Option<&str>,
) -> Result<Vec<ProcedureColumnRow>, TrinoError> {
    tracing::debug!(
        catalog,
        schema,
        proc_name,
        column,
        "TrinoBackend::procedure_columns (empty — Trino publishes no procedure metadata)"
    );
    Ok(Vec::new())
}

/// Return primary key columns for the given table.
///
/// Trino has no concept of primary keys at the engine level: no connector
/// exposes `information_schema.table_constraints` or `key_column_usage`.
/// The official Trino JDBC driver returns an empty result set from
/// `DatabaseMetaData.getPrimaryKeys()` using `WHERE false`.
///
/// We match that behavior: no rows, which core serves as `SQL_SUCCESS` with
/// the spec's 6-column schema. This allows BI tools (PowerBI, Tableau) to
/// proceed with schema discovery without error.
///
/// Ref: <https://github.com/trinodb/trino/issues/22408>
pub(super) fn primary_keys(
    _conn: &TrinoConnection,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
) -> Result<Vec<PrimaryKeyRow>, TrinoError> {
    tracing::debug!(
        catalog,
        schema,
        table,
        "TrinoBackend::primary_keys (empty — Trino has no PK metadata)"
    );
    Ok(Vec::new())
}

/// Return foreign key relationships.
///
/// Trino has no concept of foreign keys, same limitation as primary keys.
/// No connector exposes `information_schema.referential_constraints`.
///
/// Ref: <https://github.com/trinodb/trino/issues/22408>
pub(super) fn foreign_keys(
    _conn: &TrinoConnection,
    pk_catalog: Option<&str>,
    pk_schema: Option<&str>,
    pk_table: Option<&str>,
    fk_catalog: Option<&str>,
    fk_schema: Option<&str>,
    fk_table: Option<&str>,
) -> Result<Vec<ForeignKeyRow>, TrinoError> {
    tracing::debug!(
        pk_catalog,
        pk_schema,
        pk_table,
        fk_catalog,
        fk_schema,
        fk_table,
        "TrinoBackend::foreign_keys (empty — Trino has no FK metadata)"
    );
    Ok(Vec::new())
}

/// Return index statistics for a table.
///
/// Trino exposes no cross-connector index or cardinality metadata: there is no
/// engine-level equivalent of `SQLStatistics`, and index shape is a per-connector
/// physical detail Trino deliberately hides. Rather than leave the trait default,
/// this reports the deliberate empty result explicitly, matching `primary_keys`
/// and `foreign_keys`.
pub(super) fn statistics(
    _conn: &TrinoConnection,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
    _unique_only: bool,
) -> Result<Vec<StatisticsRow>, TrinoError> {
    tracing::debug!(
        catalog,
        schema,
        table,
        "TrinoBackend::statistics (empty — Trino exposes no index metadata)"
    );
    Ok(Vec::new())
}

/// Return the optimal row-identifier or row-version columns for a table.
///
/// Trino has no rowid, no row-version column, and no engine-level unique-key
/// metadata to derive an optimal identifier from, so there is nothing to report.
/// Like `statistics`, this returns the deliberate empty result explicitly rather
/// than via the trait default.
#[allow(clippy::too_many_arguments)]
pub(super) fn special_columns(
    _conn: &TrinoConnection,
    _identifier_type: IdentifierType,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
    _scope: Scope,
    _nullable: Nullable,
) -> Result<Vec<SpecialColumnRow>, TrinoError> {
    tracing::debug!(
        catalog,
        schema,
        table,
        "TrinoBackend::special_columns (empty — Trino has no rowid/row-version metadata)"
    );
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_octet_length_does_not_overflow() {
        // Trino reports unbounded varchar as varchar(2147483647) in some
        // connectors; 2147483647 * BYTES_PER_CHAR wraps to -4 in release,
        // panics in debug.
        assert_eq!(
            char_octet_length(SqlDataType::EXT_W_VARCHAR, Some(i32::MAX)),
            None
        );
    }

    #[test]
    fn char_octet_length_is_null_for_binary_columns() {
        // Trino's varbinary carries no length anywhere in its type system:
        // `information_schema.columns.data_type` is the bare string
        // "varbinary" for every binary column (verified against a live
        // coordinator), so the real call path can never produce a `Some`
        // `ty_precision` for EXT_LONG_VAR_BINARY. NULL is the honest,
        // reachable answer; this pins it rather than feeding an impossible
        // `Some(_)` input.
        assert_eq!(
            char_octet_length(SqlDataType::EXT_LONG_VAR_BINARY, None),
            None
        );
    }

    #[test]
    fn char_octet_length_is_four_times_size_for_character_columns() {
        assert_eq!(
            char_octet_length(SqlDataType::EXT_W_VARCHAR, Some(50)),
            Some(50 * BYTES_PER_CHAR)
        );
    }

    #[test]
    fn char_octet_length_is_null_for_non_character_non_binary_columns() {
        // INTEGER is neither character nor binary data.
        assert_eq!(char_octet_length(SqlDataType::INTEGER, Some(10)), None);
    }

    #[test]
    fn char_octet_length_is_null_without_a_declared_length() {
        assert_eq!(char_octet_length(SqlDataType::EXT_W_VARCHAR, None), None);
    }

    /// The row a coordinator with `sql-standard` security returns. Neither
    /// catalog in this project's test stack implements permission management,
    /// so this shape cannot be obtained from the live server the integration
    /// tests run against — it is transcribed from
    /// `information_schema.table_privileges`' column list, which was read off
    /// the running coordinator.
    fn privilege_json(grantor: serde_json::Value) -> Vec<serde_json::Value> {
        use serde_json::json;
        vec![
            json!("tpcds"),       // table_catalog
            json!("sf1"),         // table_schema
            json!("call_center"), // table_name
            grantor,              // grantor
            json!("alice"),       // grantee
            json!("SELECT"),      // privilege_type
            json!("NO"),          // is_grantable
        ]
    }

    #[test]
    fn table_privilege_row_maps_every_column_to_its_spec_position() {
        let row = table_privilege_row(&privilege_json(serde_json::json!("admin"))).unwrap();
        assert_eq!(row.catalog.as_deref(), Some("tpcds"));
        assert_eq!(row.schema.as_deref(), Some("sf1"));
        assert_eq!(row.table_name, "call_center");
        assert_eq!(row.grantor.as_deref(), Some("admin"));
        assert_eq!(row.grantee, "alice");
        assert_eq!(row.privilege, "SELECT");
        assert_eq!(row.is_grantable.as_deref(), Some("NO"));
    }

    #[test]
    fn table_privilege_row_keeps_a_null_grantor_null() {
        // ODBC's GRANTOR is nullable and Trino leaves it NULL for a privilege
        // nobody explicitly granted, so this must not become the string
        // "null" or an empty string.
        let row = table_privilege_row(&privilege_json(serde_json::Value::Null)).unwrap();
        assert_eq!(row.grantor, None);
    }

    #[test]
    fn table_privilege_row_drops_a_row_missing_a_non_null_column() {
        // TABLE_NAME, GRANTEE and PRIVILEGE are the spec's not-NULL columns.
        // A row without one cannot be described to an application, and
        // core would serve it as an empty string rather than an error.
        for missing in [
            PrivilegeCol::TableName,
            PrivilegeCol::Grantee,
            PrivilegeCol::PrivilegeType,
        ] {
            let mut vals = privilege_json(serde_json::json!("admin"));
            vals[missing as usize] = serde_json::Value::Null;
            assert!(
                table_privilege_row(&vals).is_none(),
                "a NULL in column {} must drop the row",
                missing as usize
            );
        }
    }

    #[test]
    fn table_privilege_row_drops_a_short_row() {
        // A truncated row must not panic on the index.
        assert!(table_privilege_row(&[]).is_none());
        assert!(table_privilege_row(&privilege_json(serde_json::Value::Null)[..3]).is_none());
    }

    #[test]
    fn push_filter_escaped_percent_is_literal_exact_match() {
        // `foo\%` is a literal '%', emitted as `=` (not LIKE).
        let mut conds = Vec::new();
        push_filter(&mut conds, "table_name", "foo\\%");
        assert_eq!(conds, vec!["table_name = 'foo%'".to_string()]);
    }

    #[test]
    fn push_filter_escaped_underscore_is_literal_exact_match() {
        let mut conds = Vec::new();
        push_filter(&mut conds, "table_name", "call\\_center");
        assert_eq!(conds, vec!["table_name = 'call_center'".to_string()]);
    }

    #[test]
    fn push_filter_unescaped_wildcard_is_like() {
        let mut conds = Vec::new();
        push_filter(&mut conds, "table_name", "call%");
        assert_eq!(
            conds,
            vec!["table_name LIKE 'call%' ESCAPE '\\'".to_string()]
        );
    }
}
