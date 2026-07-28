//! Catalog metadata for the Trino backend (`tables`, `columns`,
//! `primary_keys`, `foreign_keys`, `statistics`, `special_columns`), built by
//! querying Trino's `information_schema`, plus the private WHERE-clause and
//! ODBC-wildcard helpers those queries share.

use stackable_odbc_core::{
    backend::Backend,
    types::{
        ColumnDescriptor, ColumnValue, ColumnsResultCol, ForeignKeysResultCol, IdentifierType,
        Nullable, PrimaryKeysResultCol, Scope, SqlDataType, TablesResultCol,
        special_columns_columns, statistics_columns,
    },
};

use super::{
    TrinoBackend, TrinoConnection, TrinoError, TrinoStatement, info::trino_bare_type_name,
    query_all_rows,
};
use crate::type_conversion::{trino_type_name_to_sql_type, type_name_precision, type_name_scale};

fn tables_columns() -> Vec<ColumnDescriptor> {
    TablesResultCol::all_descriptors(&TrinoBackend::catalog_result_column_widths())
}

fn columns_result_columns() -> Vec<ColumnDescriptor> {
    ColumnsResultCol::all_descriptors(&TrinoBackend::catalog_result_column_widths())
}

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
fn char_octet_length(sql_type: SqlDataType, ty_precision: Option<i32>) -> ColumnValue {
    let is_character = matches!(
        sql_type,
        SqlDataType::EXT_W_VARCHAR
            | SqlDataType::EXT_W_CHAR
            | SqlDataType::VARCHAR
            | SqlDataType::CHAR
    );

    if is_character {
        return ty_precision
            .and_then(|n| n.checked_mul(BYTES_PER_CHAR))
            .map(ColumnValue::I32)
            .unwrap_or(ColumnValue::Null);
    }
    ColumnValue::Null
}

/// ODBC special mode: return list of distinct catalogs.
///
/// Called when SQLTablesW receives catalog="%" + schema="" + table="".
/// Uses `system.jdbc.catalogs` which works without a default session catalog.
fn tables_list_catalogs(conn: &TrinoConnection) -> Result<TrinoStatement, TrinoError> {
    let sql = "SELECT table_cat FROM system.jdbc.catalogs ORDER BY table_cat";
    let rows = query_all_rows(conn, sql.to_string())?;
    let result_rows = rows
        .into_iter()
        .filter_map(|row| {
            let cat = row.into_json().into_iter().next()?.as_str()?.to_string();
            Some(vec![
                ColumnValue::String(cat),
                ColumnValue::Null,
                ColumnValue::Null,
                ColumnValue::Null,
                ColumnValue::Null,
            ])
        })
        .collect();
    Ok(TrinoStatement {
        pending_sql: None,
        columns: tables_columns(),
        trino_types: Vec::new(),
        batch: result_rows,
        batch_cursor: 0,
        fetch_failed: false,
        next_uri: None,
        query_id: None,
        client: None,
        runtime: None,
        cancel_state: None,
        page_count: 0,
        empty_page_count: 0,
        total_rows_fetched: 0,
        total_fetch_time: std::time::Duration::ZERO,
        total_convert_time: std::time::Duration::ZERO,
    })
}

/// ODBC special mode: return list of distinct schemas.
///
/// Called when SQLTablesW receives schema="%" + catalog="" + table="".
/// Uses `system.jdbc.schemas` which works without a default session catalog.
/// Per the ODBC spec, TABLE_CAT must be NULL in schema-enumeration mode.
fn tables_list_schemas(conn: &TrinoConnection) -> Result<TrinoStatement, TrinoError> {
    let sql = "SELECT table_schem FROM system.jdbc.schemas ORDER BY table_schem";
    let rows = query_all_rows(conn, sql.to_string())?;
    let result_rows = rows
        .into_iter()
        .filter_map(|row| {
            let sch = row.into_json().into_iter().next()?.as_str()?.to_string();
            Some(vec![
                ColumnValue::Null,
                ColumnValue::String(sch),
                ColumnValue::Null,
                ColumnValue::Null,
                ColumnValue::Null,
            ])
        })
        .collect();
    Ok(TrinoStatement {
        pending_sql: None,
        columns: tables_columns(),
        trino_types: Vec::new(),
        batch: result_rows,
        batch_cursor: 0,
        fetch_failed: false,
        next_uri: None,
        query_id: None,
        client: None,
        runtime: None,
        cancel_state: None,
        page_count: 0,
        empty_page_count: 0,
        total_rows_fetched: 0,
        total_fetch_time: std::time::Duration::ZERO,
        total_convert_time: std::time::Duration::ZERO,
    })
}

/// ODBC special mode: return the supported table types.
///
/// Called when SQLTablesW receives table_type="%" + catalog="" + schema="" + table="".
pub(super) fn tables_list_table_types() -> Result<TrinoStatement, TrinoError> {
    let rows = vec![
        vec![
            ColumnValue::Null,
            ColumnValue::Null,
            ColumnValue::Null,
            ColumnValue::String("TABLE".into()),
            ColumnValue::Null,
        ],
        vec![
            ColumnValue::Null,
            ColumnValue::Null,
            ColumnValue::Null,
            ColumnValue::String("VIEW".into()),
            ColumnValue::Null,
        ],
    ];
    Ok(TrinoStatement {
        pending_sql: None,
        columns: tables_columns(),
        trino_types: Vec::new(),
        batch: rows,
        batch_cursor: 0,
        fetch_failed: false,
        next_uri: None,
        query_id: None,
        client: None,
        runtime: None,
        cancel_state: None,
        page_count: 0,
        empty_page_count: 0,
        total_rows_fetched: 0,
        total_fetch_time: std::time::Duration::ZERO,
        total_convert_time: std::time::Duration::ZERO,
    })
}

pub(super) fn tables(
    conn: &TrinoConnection,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
    table_type: Option<&str>,
) -> Result<TrinoStatement, TrinoError> {
    tracing::debug!(catalog, schema, table, table_type, "TrinoBackend::tables");

    // ODBC special enumeration modes (ODBC spec §SQLTables):
    // These are used by tools like PowerBI to browse the catalog tree.
    //
    // • catalog="%" + schema="" + table="" → return list of distinct catalogs.
    // • schema="%" + catalog="" + table="" → return list of distinct schemas.
    // • table_type="%" + catalog="" + schema="" + table="" → return list of table types.
    if catalog == Some("%") && schema == Some("") && table == Some("") {
        return tables_list_catalogs(conn);
    }
    if schema == Some("%") && catalog == Some("") && table == Some("") {
        return tables_list_schemas(conn);
    }
    if table_type == Some("%") && catalog == Some("") && schema == Some("") && table == Some("") {
        return tables_list_table_types();
    }

    let sql = format!(
        "SELECT table_catalog, table_schema, table_name, table_type \
         FROM information_schema.tables{} \
         ORDER BY table_type, table_catalog, table_schema, table_name",
        build_tables_where_clause(catalog, schema, table)
    );

    let rows = query_all_rows(conn, sql)?;

    // table_type filter: comma-separated list like "'TABLE','VIEW'"
    let allowed_types: Option<Vec<String>> = table_type.filter(|s| !s.is_empty()).map(|tt| {
        tt.split(',')
            .map(|s| s.trim().trim_matches('\'').to_uppercase())
            .collect()
    });

    let result_rows: Vec<Vec<ColumnValue>> = rows
        .into_iter()
        .filter_map(|row| {
            let mut vals = row.into_json().into_iter();
            let cat_val = vals.next()?;
            let sch_val = vals.next()?;
            let name = vals.next()?.as_str()?.to_string();
            let raw_type = vals.next()?.as_str()?.to_uppercase();
            // Trino reports "BASE TABLE"; ODBC expects "TABLE".
            let odbc_type = match raw_type.as_str() {
                "BASE TABLE" => "TABLE",
                "VIEW" => "VIEW",
                other => {
                    tracing::warn!(other, "unknown table_type from Trino, skipping row");
                    return None;
                }
            };

            if allowed_types
                .as_ref()
                .is_some_and(|allowed| !allowed.iter().any(|a| a == odbc_type))
            {
                return None;
            }

            Some(vec![
                if cat_val.is_null() {
                    ColumnValue::Null
                } else {
                    ColumnValue::String(cat_val.as_str()?.to_string())
                },
                if sch_val.is_null() {
                    ColumnValue::Null
                } else {
                    ColumnValue::String(sch_val.as_str()?.to_string())
                },
                ColumnValue::String(name),
                ColumnValue::String(odbc_type.to_string()),
                ColumnValue::Null, // REMARKS
            ])
        })
        .collect();

    Ok(TrinoStatement {
        pending_sql: None,
        columns: tables_columns(),
        trino_types: Vec::new(),
        batch: result_rows,
        batch_cursor: 0,
        fetch_failed: false,
        next_uri: None,
        query_id: None,
        client: None,
        runtime: None,
        cancel_state: None,
        page_count: 0,
        empty_page_count: 0,
        total_rows_fetched: 0,
        total_fetch_time: std::time::Duration::ZERO,
        total_convert_time: std::time::Duration::ZERO,
    })
}

pub(super) fn columns(
    conn: &TrinoConnection,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
    column: Option<&str>,
) -> Result<TrinoStatement, TrinoError> {
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
    let sql = format!(
        "SELECT table_catalog, table_schema, table_name, column_name, \
                ordinal_position, column_default, is_nullable, data_type \
         FROM information_schema.columns{} \
         ORDER BY table_catalog, table_schema, table_name, ordinal_position",
        build_columns_where_clause(catalog, schema, table, column)
    );

    let rows = query_all_rows(conn, sql)?;

    let result_rows: Vec<Vec<ColumnValue>> = rows
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

            let table_cat = get_str(QueryCol::TableCatalog)
                .map(ColumnValue::String)
                .unwrap_or(ColumnValue::Null);
            let table_sch = get_str(QueryCol::TableSchema)
                .map(ColumnValue::String)
                .unwrap_or(ColumnValue::Null);
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
            let col_def = get_str(QueryCol::ColumnDefault)
                .map(ColumnValue::String)
                .unwrap_or(ColumnValue::Null);
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
            let col_size = ty_precision
                .map(ColumnValue::I32)
                .unwrap_or(ColumnValue::Null);
            let num_prec_radix = if is_numeric {
                ColumnValue::I16(10)
            } else {
                ColumnValue::Null
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
                ty_scale
                    .and_then(|s| i16::try_from(s).ok())
                    .map(ColumnValue::I16)
                    .unwrap_or(ColumnValue::Null)
            } else {
                ColumnValue::Null
            };
            let char_octet = char_octet_length(sql_type, ty_precision);

            Some(vec![
                table_cat,                                                      //  1 TABLE_CAT
                table_sch,                                                      //  2 TABLE_SCHEM
                ColumnValue::String(table_name),                                //  3 TABLE_NAME
                ColumnValue::String(col_name),                                  //  4 COLUMN_NAME
                ColumnValue::I16(sql_type.0),                                   //  5 DATA_TYPE
                ColumnValue::String(data_type),                                 //  6 TYPE_NAME
                col_size,                                                       //  7 COLUMN_SIZE
                ColumnValue::Null,                                              //  8 BUFFER_LENGTH
                decimal_digits,                                                 //  9 DECIMAL_DIGITS
                num_prec_radix,                                                 // 10 NUM_PREC_RADIX
                ColumnValue::I16(nullable.into()),                              // 11 NULLABLE
                ColumnValue::Null,                                              // 12 REMARKS
                col_def,                                                        // 13 COLUMN_DEF
                ColumnValue::I16(sql_type.0),                                   // 14 SQL_DATA_TYPE
                ColumnValue::Null,         // 15 SQL_DATETIME_SUB
                char_octet,                // 16 CHAR_OCTET_LENGTH
                ColumnValue::I32(ordinal), // 17 ORDINAL_POSITION
                ColumnValue::String(nullable.as_is_nullable_str().to_string()), // 18 IS_NULLABLE
            ])
        })
        .collect();

    Ok(TrinoStatement {
        pending_sql: None,
        columns: columns_result_columns(),
        trino_types: Vec::new(),
        batch: result_rows,
        batch_cursor: 0,
        fetch_failed: false,
        next_uri: None,
        query_id: None,
        client: None,
        runtime: None,
        cancel_state: None,
        page_count: 0,
        empty_page_count: 0,
        total_rows_fetched: 0,
        total_fetch_time: std::time::Duration::ZERO,
        total_convert_time: std::time::Duration::ZERO,
    })
}

/// A completed statement carrying only the given column schema and no rows.
///
/// Trino cannot answer several ODBC catalog functions (keys, index statistics,
/// special columns), and the spec's defined response for "the data source does
/// not expose this" is `SQL_SUCCESS` with the correct schema and zero rows, not
/// an error. This builds exactly that.
fn empty_result(columns: Vec<ColumnDescriptor>) -> TrinoStatement {
    TrinoStatement {
        pending_sql: None,
        columns,
        trino_types: Vec::new(),
        batch: vec![],
        batch_cursor: 0,
        fetch_failed: false,
        next_uri: None,
        query_id: None,
        client: None,
        runtime: None,
        cancel_state: None,
        page_count: 0,
        empty_page_count: 0,
        total_rows_fetched: 0,
        total_fetch_time: std::time::Duration::ZERO,
        total_convert_time: std::time::Duration::ZERO,
    }
}

fn primary_keys_columns() -> Vec<ColumnDescriptor> {
    PrimaryKeysResultCol::all_descriptors(&TrinoBackend::catalog_result_column_widths())
}

/// Return primary key columns for the given table.
///
/// Trino has no concept of primary keys at the engine level: no connector
/// exposes `information_schema.table_constraints` or `key_column_usage`.
/// The official Trino JDBC driver returns an empty result set from
/// `DatabaseMetaData.getPrimaryKeys()` using `WHERE false`.
///
/// We match that behavior: return SQL_SUCCESS with an empty result set and
/// the correct 6-column schema. This allows BI tools (PowerBI, Tableau) to
/// proceed with schema discovery without error.
///
/// Ref: <https://github.com/trinodb/trino/issues/22408>
pub(super) fn primary_keys(
    _conn: &TrinoConnection,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
) -> Result<TrinoStatement, TrinoError> {
    tracing::debug!(
        catalog,
        schema,
        table,
        "TrinoBackend::primary_keys (empty — Trino has no PK metadata)"
    );
    Ok(empty_result(primary_keys_columns()))
}

fn foreign_keys_columns() -> Vec<ColumnDescriptor> {
    ForeignKeysResultCol::all_descriptors(&TrinoBackend::catalog_result_column_widths())
}

/// Return foreign key relationships.
///
/// Trino has no concept of foreign keys, same limitation as primary keys.
/// No connector exposes `information_schema.referential_constraints`.
/// Return SQL_SUCCESS with an empty result set and the correct 14-column schema.
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
) -> Result<TrinoStatement, TrinoError> {
    tracing::debug!(
        pk_catalog,
        pk_schema,
        pk_table,
        fk_catalog,
        fk_schema,
        fk_table,
        "TrinoBackend::foreign_keys (empty — Trino has no FK metadata)"
    );
    Ok(empty_result(foreign_keys_columns()))
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
) -> Result<TrinoStatement, TrinoError> {
    tracing::debug!(
        catalog,
        schema,
        table,
        "TrinoBackend::statistics (empty — Trino exposes no index metadata)"
    );
    Ok(empty_result(statistics_columns(
        &TrinoBackend::catalog_result_column_widths(),
    )))
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
) -> Result<TrinoStatement, TrinoError> {
    tracing::debug!(
        catalog,
        schema,
        table,
        "TrinoBackend::special_columns (empty — Trino has no rowid/row-version metadata)"
    );
    Ok(empty_result(special_columns_columns(
        &TrinoBackend::catalog_result_column_widths(),
    )))
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
            ColumnValue::Null
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
            ColumnValue::Null
        );
    }

    #[test]
    fn char_octet_length_is_four_times_size_for_character_columns() {
        assert_eq!(
            char_octet_length(SqlDataType::EXT_W_VARCHAR, Some(50)),
            ColumnValue::I32(50 * BYTES_PER_CHAR)
        );
    }

    #[test]
    fn char_octet_length_is_null_for_non_character_non_binary_columns() {
        // INTEGER is neither character nor binary data.
        assert_eq!(
            char_octet_length(SqlDataType::INTEGER, Some(10)),
            ColumnValue::Null
        );
    }

    #[test]
    fn char_octet_length_is_null_without_a_declared_length() {
        assert_eq!(
            char_octet_length(SqlDataType::EXT_W_VARCHAR, None),
            ColumnValue::Null
        );
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
