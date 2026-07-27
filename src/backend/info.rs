//! `SQLGetInfo`, `SQLGetTypeInfo` and `SQLGetFunctions` support for the Trino
//! backend: the `get_info` / `get_info_pre_connect` / `get_info_raw`
//! handlers, the exported-function bitmap, the static type-info rows, and the
//! Trino capability bitmaps (`TRINO_*`), several of which are version-gated on
//! the coordinator's reported version.

use stackable_odbc_core::backend::{common_get_info_raw, default_get_info};
use stackable_odbc_core::function_id::FunctionId;
use stackable_odbc_core::types::{
    InfoType, InfoValue, MaxPrecision, MaxScale, SQL_AF_ALL, SQL_AF_AVG, SQL_AF_COUNT,
    SQL_AF_DISTINCT, SQL_AF_MAX, SQL_AF_MIN, SQL_AF_SUM, SQL_AGGREGATE_FUNCTIONS,
    SQL_AT_ADD_COLUMN_SINGLE, SQL_AT_ADD_CONSTRAINT, SQL_AT_DROP_COLUMN, SQL_CL_START,
    SQL_CODE_DATE, SQL_CODE_TIME, SQL_CODE_TIMESTAMP, SQL_CU_DML_STATEMENTS,
    SQL_CU_PRIVILEGE_DEFINITION, SQL_CU_PROCEDURE_INVOCATION, SQL_CU_TABLE_DEFINITION,
    SQL_DATABASE_NAME, SQL_FN_NUM_ABS, SQL_FN_NUM_ACOS, SQL_FN_NUM_ASIN, SQL_FN_NUM_ATAN,
    SQL_FN_NUM_ATAN2, SQL_FN_NUM_CEILING, SQL_FN_NUM_COS, SQL_FN_NUM_DEGREES, SQL_FN_NUM_EXP,
    SQL_FN_NUM_FLOOR, SQL_FN_NUM_LOG, SQL_FN_NUM_LOG10, SQL_FN_NUM_MOD, SQL_FN_NUM_PI,
    SQL_FN_NUM_POWER, SQL_FN_NUM_RADIANS, SQL_FN_NUM_RAND, SQL_FN_NUM_ROUND, SQL_FN_NUM_SIGN,
    SQL_FN_NUM_SIN, SQL_FN_NUM_SQRT, SQL_FN_NUM_TAN, SQL_FN_NUM_TRUNCATE, SQL_FN_STR_CHAR,
    SQL_FN_STR_CONCAT, SQL_FN_STR_LCASE, SQL_FN_STR_LENGTH, SQL_FN_STR_LOCATE_2, SQL_FN_STR_LTRIM,
    SQL_FN_STR_POSITION, SQL_FN_STR_REPLACE, SQL_FN_STR_RTRIM, SQL_FN_STR_SOUNDEX,
    SQL_FN_STR_SUBSTRING, SQL_FN_STR_UCASE, SQL_FN_SYS_DBNAME, SQL_FN_SYS_IFNULL,
    SQL_FN_SYS_USERNAME, SQL_FN_TD_CURDATE, SQL_FN_TD_CURRENT_DATE, SQL_FN_TD_CURRENT_TIME,
    SQL_FN_TD_CURRENT_TIMESTAMP, SQL_FN_TD_CURTIME, SQL_FN_TD_DAYOFMONTH, SQL_FN_TD_DAYOFWEEK,
    SQL_FN_TD_DAYOFYEAR, SQL_FN_TD_EXTRACT, SQL_FN_TD_HOUR, SQL_FN_TD_MINUTE, SQL_FN_TD_MONTH,
    SQL_FN_TD_NOW, SQL_FN_TD_QUARTER, SQL_FN_TD_SECOND, SQL_FN_TD_TIMESTAMPADD,
    SQL_FN_TD_TIMESTAMPDIFF, SQL_FN_TD_WEEK, SQL_FN_TD_YEAR, SQL_GD_ANY_COLUMN, SQL_GD_ANY_ORDER,
    SQL_GD_BOUND, SQL_LIKE_ESCAPE_CLAUSE, SQL_NUMERIC_FUNCTIONS, SQL_OJ_ALL_COMPARISON_OPS,
    SQL_OJ_FULL, SQL_OJ_INNER, SQL_OJ_LEFT, SQL_OJ_NESTED, SQL_OJ_NOT_ORDERED, SQL_OJ_RIGHT,
    SQL_OUTER_JOINS, SQL_SP_BETWEEN, SQL_SP_COMPARISON, SQL_SP_EXISTS, SQL_SP_IN, SQL_SP_ISNOTNULL,
    SQL_SP_ISNULL, SQL_SP_LIKE, SQL_SP_MATCH_FULL, SQL_SP_MATCH_PARTIAL, SQL_SP_MATCH_UNIQUE_FULL,
    SQL_SP_MATCH_UNIQUE_PARTIAL, SQL_SP_OVERLAPS, SQL_SP_QUANTIFIED_COMPARISON, SQL_SP_UNIQUE,
    SQL_SQL92_PREDICATES, SQL_SQL92_RELATIONAL_JOIN_OPERATORS, SQL_SQL92_VALUE_EXPRESSIONS,
    SQL_SRJO_CORRESPONDING_CLAUSE, SQL_SRJO_CROSS_JOIN, SQL_SRJO_EXCEPT_JOIN,
    SQL_SRJO_FULL_OUTER_JOIN, SQL_SRJO_INNER_JOIN, SQL_SRJO_INTERSECT_JOIN,
    SQL_SRJO_LEFT_OUTER_JOIN, SQL_SRJO_RIGHT_OUTER_JOIN, SQL_STRING_FUNCTIONS,
    SQL_SU_DML_STATEMENTS, SQL_SU_PRIVILEGE_DEFINITION, SQL_SU_PROCEDURE_INVOCATION,
    SQL_SU_TABLE_DEFINITION, SQL_SVE_CASE, SQL_SVE_CAST, SQL_SVE_COALESCE, SQL_SVE_NULLIF,
    SQL_SYSTEM_FUNCTIONS, SQL_TC_NONE, SQL_TIMEDATE_FUNCTIONS, SqlDataType, TypeInfoRow,
    catalog_column_size,
};

use super::TrinoBackend;
use super::TrinoConnection;
use super::TrinoError;
use crate::type_conversion::{MAX_FRACTIONAL_SECONDS_PRECISION, TrinoTypeName};

/// Trino's documented maximum DECIMAL precision (and, since Trino ties a
/// DECIMAL's maximum scale to its maximum precision, also its maximum scale).
/// <https://trino.io/docs/current/language/types.html#decimal>
const MAX_DECIMAL_PRECISION: i32 = 38;
const MAX_DECIMAL_SCALE: i16 = 38;

/// Trino wire-format extension for `TIME WITH TIME ZONE` /
/// `TIMESTAMP WITH TIME ZONE` COLUMN_SIZE, layered on top of the ODBC
/// "Column Size" appendix's plain TIME/TIMESTAMP formula. Neither type is an
/// ODBC concise type (ODBC 3.x has no `SQL_TYPE_TIME_WITH_TIMEZONE` /
/// `SQL_TYPE_TIMESTAMP_WITH_TIMEZONE`; those only exist in ODBC 4.0, which no
/// Driver Manager implements), so the appendix defines no row for them: this
/// is Trino's own wire format, not an appendix value, hence living here
/// rather than in `stackable-odbc-core`.
///
/// Both types append a glued numeric offset, e.g. `"13:14:15.123456789012+02:00"`
/// (see `parse_trino_time_with_tz`) / `"...+02:00"` (see
/// `parse_trino_timestamp_tz`), in `type_conversion.rs`.
const TRINO_TZ_OFFSET_SUFFIX_LEN: i32 = 6; // "+HH:MM"
/// `TIMESTAMP WITH TIME ZONE` additionally inserts a space before the offset
/// (`"... HH:MM:SS.ffffffffffff +02:00"`), unlike `TIME WITH TIME ZONE`,
/// which glues the offset directly onto the seconds
/// (`"HH:MM:SS.ffffffffffff+02:00"`); see the two parsers cited above for
/// the exact wire formats this mirrors.
const TRINO_TZ_TIMESTAMP_SPACE_LEN: i32 = 1;

/// Static type information for Trino's type system.
///
/// Reference: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgettypeinfo-function>
/// Trino types: <https://trino.io/docs/current/language/types.html>
///
/// Every `column_size` value below is computed via `catalog_column_size`
/// (the ODBC "Column Size" appendix formula, evaluated at this data source's
/// maximum supported precision/scale) rather than hand-written; see
/// `stackable_odbc_core::types::column_size` module docs for why `SQLGetTypeInfo`'s
/// COLUMN_SIZE must never be a literal copied from the per-column path (or
/// vice versa).
///
/// Rows are sorted by DATA_TYPE ascending (as signed i16, so ODBC extension
/// types with negative codes sort first), then by TYPE_NAME ascending within
/// an equal DATA_TYPE, per the SQLGetTypeInfo spec's "ordered by DATA_TYPE and
/// then ... TYPE_NAME" requirement. This invariant is asserted directly by
/// `type_info_rows_sorted_by_data_type_then_type_name` below; keep new rows
/// in the correct sorted position rather than appending them.
static TRINO_TYPE_INFO: &[TypeInfoRow] = &[
    // INTERVAL DAY TO SECOND — trino_ty_to_sql_type has no dedicated
    // ODBC interval type for this (see the "String-representable types
    // without a dedicated ODBC type" comment in type_conversion.rs); Trino
    // interval values are rendered as text, so DATA_TYPE matches what is
    // actually reported (EXT_W_VARCHAR), the same as INTERVAL YEAR TO
    // MONTH/JSON/UUID/VARCHAR further down this list. TYPE_NAME is sourced
    // from `TrinoTypeName::IntervalDayToSecond::name()` (not a hardcoded
    // string) so this row and `trino_bare_type_name`'s parser cannot drift
    // apart: without a matching `TrinoTypeName` variant, no real interval
    // column could report this TYPE_NAME (`trino_bare_type_name` would fall
    // through to "VARCHAR" instead); see `every_type_info_row_is_reachable_via_trino_bare_type_name`.
    TypeInfoRow::new(
        TrinoTypeName::IntervalDayToSecond.name(),
        SqlDataType::EXT_W_VARCHAR,
    )
    .with_column_size(catalog_column_size(
        SqlDataType::EXT_W_VARCHAR,
        MaxPrecision(i32::MAX),
        MaxScale(0),
    ))
    .with_literal_affixes(Some("'"), Some("'")),
    // INTERVAL YEAR TO MONTH — same rationale as INTERVAL DAY TO SECOND
    // above, including sourcing TYPE_NAME from `TrinoTypeName::name()`.
    TypeInfoRow::new(
        TrinoTypeName::IntervalYearToMonth.name(),
        SqlDataType::EXT_W_VARCHAR,
    )
    .with_column_size(catalog_column_size(
        SqlDataType::EXT_W_VARCHAR,
        MaxPrecision(i32::MAX),
        MaxScale(0),
    ))
    .with_literal_affixes(Some("'"), Some("'")),
    // JSON — stored/returned as VARCHAR in ODBC context
    TypeInfoRow::new(TrinoTypeName::Json.name(), SqlDataType::EXT_W_VARCHAR)
        .with_column_size(catalog_column_size(
            SqlDataType::EXT_W_VARCHAR,
            MaxPrecision(i32::MAX),
            MaxScale(0),
        ))
        .with_literal_affixes(Some("'"), Some("'"))
        .with_case_sensitive(true),
    // UUID — returned as 36-char VARCHAR string
    TypeInfoRow::new(TrinoTypeName::Uuid.name(), SqlDataType::EXT_W_VARCHAR)
        .with_column_size(catalog_column_size(
            SqlDataType::EXT_W_VARCHAR,
            MaxPrecision(36),
            MaxScale(0),
        ))
        .with_literal_affixes(Some("'"), Some("'")),
    // VARCHAR
    TypeInfoRow::new(TrinoTypeName::Varchar.name(), SqlDataType::EXT_W_VARCHAR)
        .with_column_size(catalog_column_size(
            SqlDataType::EXT_W_VARCHAR,
            MaxPrecision(i32::MAX),
            MaxScale(0),
        ))
        .with_literal_affixes(Some("'"), Some("'"))
        .with_create_params(Some("max length"))
        .with_case_sensitive(true),
    // CHAR
    TypeInfoRow::new(TrinoTypeName::Char.name(), SqlDataType::EXT_W_CHAR)
        .with_column_size(catalog_column_size(
            SqlDataType::EXT_W_CHAR,
            MaxPrecision(u16::MAX as i32),
            MaxScale(0),
        ))
        .with_literal_affixes(Some("'"), Some("'"))
        .with_create_params(Some("length"))
        .with_case_sensitive(true),
    // BOOLEAN
    TypeInfoRow::new(TrinoTypeName::Boolean.name(), SqlDataType::EXT_BIT).with_column_size(
        catalog_column_size(SqlDataType::EXT_BIT, MaxPrecision(0), MaxScale(0)),
    ),
    // TINYINT
    TypeInfoRow::new(TrinoTypeName::TinyInt.name(), SqlDataType::EXT_TINY_INT)
        .with_column_size(catalog_column_size(
            SqlDataType::EXT_TINY_INT,
            MaxPrecision(0),
            MaxScale(0),
        ))
        .with_unsigned(Some(false))
        .with_auto_unique_value(Some(false))
        .with_scale_range(Some(0), Some(0))
        .with_num_prec_radix(Some(10)),
    // BIGINT
    TypeInfoRow::new(TrinoTypeName::BigInt.name(), SqlDataType::EXT_BIG_INT)
        .with_column_size(catalog_column_size(
            SqlDataType::EXT_BIG_INT,
            MaxPrecision(0),
            MaxScale(0),
        ))
        .with_unsigned(Some(false))
        .with_auto_unique_value(Some(false))
        .with_scale_range(Some(0), Some(0))
        .with_num_prec_radix(Some(10)),
    // VARBINARY
    TypeInfoRow::new(
        TrinoTypeName::Varbinary.name(),
        SqlDataType::EXT_LONG_VAR_BINARY,
    )
    .with_column_size(catalog_column_size(
        SqlDataType::EXT_LONG_VAR_BINARY,
        MaxPrecision(i32::MAX),
        MaxScale(0),
    ))
    .with_literal_affixes(Some("X'"), Some("'")),
    // SQL_CHAR (1) — ANSI alias. See the SQL_VARCHAR comment further down
    // this list re TYPE_NAME.
    TypeInfoRow::new("SQL_CHAR", SqlDataType::CHAR)
        .with_column_size(catalog_column_size(
            SqlDataType::CHAR,
            MaxPrecision(u16::MAX as i32),
            MaxScale(0),
        ))
        .with_literal_affixes(Some("'"), Some("'"))
        .with_create_params(Some("length"))
        .with_case_sensitive(true),
    // DECIMAL
    TypeInfoRow::new(TrinoTypeName::Decimal.name(), SqlDataType::DECIMAL)
        .with_column_size(catalog_column_size(
            SqlDataType::DECIMAL,
            MaxPrecision(MAX_DECIMAL_PRECISION),
            MaxScale(MAX_DECIMAL_SCALE),
        ))
        .with_create_params(Some("precision,scale"))
        .with_unsigned(Some(false))
        .with_auto_unique_value(Some(false))
        .with_scale_range(Some(0), Some(MAX_DECIMAL_SCALE))
        .with_num_prec_radix(Some(10)),
    // INTEGER
    TypeInfoRow::new(TrinoTypeName::Integer.name(), SqlDataType::INTEGER)
        .with_column_size(catalog_column_size(
            SqlDataType::INTEGER,
            MaxPrecision(0),
            MaxScale(0),
        ))
        .with_unsigned(Some(false))
        .with_auto_unique_value(Some(false))
        .with_scale_range(Some(0), Some(0))
        .with_num_prec_radix(Some(10)),
    // SMALLINT
    TypeInfoRow::new(TrinoTypeName::SmallInt.name(), SqlDataType::SMALLINT)
        .with_column_size(catalog_column_size(
            SqlDataType::SMALLINT,
            MaxPrecision(0),
            MaxScale(0),
        ))
        .with_unsigned(Some(false))
        .with_auto_unique_value(Some(false))
        .with_scale_range(Some(0), Some(0))
        .with_num_prec_radix(Some(10)),
    // REAL
    TypeInfoRow::new(TrinoTypeName::Real.name(), SqlDataType::REAL)
        .with_column_size(catalog_column_size(
            SqlDataType::REAL,
            MaxPrecision(0),
            MaxScale(0),
        ))
        .with_unsigned(Some(false))
        .with_num_prec_radix(Some(2)),
    // DOUBLE
    TypeInfoRow::new(TrinoTypeName::Double.name(), SqlDataType::DOUBLE)
        .with_column_size(catalog_column_size(
            SqlDataType::DOUBLE,
            MaxPrecision(0),
            MaxScale(0),
        ))
        .with_unsigned(Some(false))
        .with_num_prec_radix(Some(2)),
    // SQL_VARCHAR (12) — ANSI alias needed by pyodbc/Windows DM.
    // When the DM queries SQLGetTypeInfo(SQL_VARCHAR=12), it needs to find a
    // matching row or it refuses to perform type conversions (e.g. bigint→string).
    // TYPE_NAME must differ from the WVARCHAR entry ("VARCHAR") because Power
    // Query builds a record keyed by TYPE_NAME and crashes on duplicates.
    TypeInfoRow::new("SQL_VARCHAR", SqlDataType::VARCHAR)
        .with_column_size(catalog_column_size(
            SqlDataType::VARCHAR,
            MaxPrecision(i32::MAX),
            MaxScale(0),
        ))
        .with_literal_affixes(Some("'"), Some("'"))
        .with_create_params(Some("max length"))
        .with_case_sensitive(true),
    // DATE
    // DATA_TYPE=91 (SQL_TYPE_DATE), SQL_DATA_TYPE=9 (SQL_DATETIME), SQL_DATETIME_SUB=1 (SQL_CODE_DATE)
    TypeInfoRow::new(TrinoTypeName::Date.name(), SqlDataType::DATE)
        .with_column_size(catalog_column_size(
            SqlDataType::DATE,
            MaxPrecision(0),
            MaxScale(0),
        ))
        .with_literal_affixes(Some("DATE '"), Some("'"))
        .with_verbose_type(SqlDataType::DATETIME.0, Some(SQL_CODE_DATE)),
    // TIME
    // DATA_TYPE=92 (SQL_TYPE_TIME), SQL_DATA_TYPE=9 (SQL_DATETIME), SQL_DATETIME_SUB=2 (SQL_CODE_TIME)
    TypeInfoRow::new(TrinoTypeName::Time.name(), SqlDataType::TIME)
        .with_column_size(catalog_column_size(
            SqlDataType::TIME,
            MaxPrecision(0),
            MaxScale(MAX_FRACTIONAL_SECONDS_PRECISION),
        ))
        .with_literal_affixes(Some("TIME '"), Some("'"))
        .with_create_params(Some("precision"))
        .with_scale_range(Some(0), Some(MAX_FRACTIONAL_SECONDS_PRECISION))
        .with_verbose_type(SqlDataType::DATETIME.0, Some(SQL_CODE_TIME)),
    // TIME WITH TIME ZONE — shares DATA_TYPE=92 with plain TIME above
    // (see TrinoTypeName::sql_type); needs its own row so an application
    // that looks up SQLGetTypeInfo by TYPE_NAME (e.g. building a CREATE
    // TABLE statement) can find "TIME WITH TIME ZONE" at all, as it is a
    // distinct, commonly-used Trino type. Grouped immediately after TIME
    // to keep DATA_TYPE=92 rows adjacent per the spec's "ordered by
    // DATA_TYPE" guidance.
    TypeInfoRow::new(TrinoTypeName::TimeWithTimeZone.name(), SqlDataType::TIME)
        .with_column_size(
            catalog_column_size(
                SqlDataType::TIME,
                MaxPrecision(0),
                MaxScale(MAX_FRACTIONAL_SECONDS_PRECISION),
            ) + TRINO_TZ_OFFSET_SUFFIX_LEN,
        )
        .with_literal_affixes(Some("TIME '"), Some("'"))
        .with_create_params(Some("precision"))
        .with_scale_range(Some(0), Some(MAX_FRACTIONAL_SECONDS_PRECISION))
        .with_verbose_type(SqlDataType::DATETIME.0, Some(SQL_CODE_TIME)),
    // TIMESTAMP
    // DATA_TYPE=93 (SQL_TYPE_TIMESTAMP), SQL_DATA_TYPE=9 (SQL_DATETIME), SQL_DATETIME_SUB=3 (SQL_CODE_TIMESTAMP)
    TypeInfoRow::new(TrinoTypeName::Timestamp.name(), SqlDataType::TIMESTAMP)
        .with_column_size(catalog_column_size(
            SqlDataType::TIMESTAMP,
            MaxPrecision(0),
            MaxScale(MAX_FRACTIONAL_SECONDS_PRECISION),
        ))
        .with_literal_affixes(Some("TIMESTAMP '"), Some("'"))
        .with_create_params(Some("precision"))
        .with_scale_range(Some(0), Some(MAX_FRACTIONAL_SECONDS_PRECISION))
        .with_verbose_type(SqlDataType::DATETIME.0, Some(SQL_CODE_TIMESTAMP)),
    // TIMESTAMP WITH TIME ZONE — shares DATA_TYPE=93 with plain
    // TIMESTAMP above; same rationale as TIME WITH TIME ZONE.
    TypeInfoRow::new(
        TrinoTypeName::TimestampWithTimeZone.name(),
        SqlDataType::TIMESTAMP,
    )
    .with_column_size(
        catalog_column_size(
            SqlDataType::TIMESTAMP,
            MaxPrecision(0),
            MaxScale(MAX_FRACTIONAL_SECONDS_PRECISION),
        ) + TRINO_TZ_TIMESTAMP_SPACE_LEN
            + TRINO_TZ_OFFSET_SUFFIX_LEN,
    )
    .with_literal_affixes(Some("TIMESTAMP '"), Some("'"))
    .with_create_params(Some("precision"))
    .with_scale_range(Some(0), Some(MAX_FRACTIONAL_SECONDS_PRECISION))
    .with_verbose_type(SqlDataType::DATETIME.0, Some(SQL_CODE_TIMESTAMP)),
];

/// Connection-independent info lookup. All arms use `_conn` nowhere,
/// so this can be called from unit tests without a live Trino connection.
fn trino_get_info(info_type: InfoType) -> Result<InfoValue, TrinoError> {
    match info_type {
        InfoType::DriverName => return Ok(InfoValue::String("stackable-odbc-trino".into())),
        InfoType::DriverVer => {
            return Ok(InfoValue::String(stackable_odbc_core::driver_version!()));
        }
        InfoType::DbmsName => return Ok(InfoValue::String("Trino".into())),
        // A schema-qualified name (`schema.table`) is usable in DML, in a
        // `CALL schema.procedure()` invocation, in `CREATE`/`ALTER`/`DROP
        // TABLE`, and in `GRANT`/`REVOKE` -- all confirmed against the Trino
        // SQL statement reference. `SQL_SU_INDEX_DEFINITION` is deliberately
        // absent: Trino's grammar has no `CREATE INDEX`/`DROP INDEX`
        // statement at all, so no schema-qualified name is ever usable
        // there. Do not claim `SQL_SU_INDEX_DEFINITION` in place of
        // `SQL_SU_PRIVILEGE_DEFINITION` (both being bit `0x08`/`0x10` of the
        // same nibble makes the swap easy to miss) -- that would overclaim a
        // statement Trino cannot execute and underclaim one it can.
        InfoType::SchemaUsage => {
            return Ok(InfoValue::U32(
                SQL_SU_DML_STATEMENTS
                    | SQL_SU_PROCEDURE_INVOCATION
                    | SQL_SU_TABLE_DEFINITION
                    | SQL_SU_PRIVILEGE_DEFINITION,
            ));
        }
        // Same statement coverage as SQL_SCHEMA_USAGE above, just for a
        // catalog-qualified name (`catalog.schema.table`); Trino resolves
        // both forms through the same qualified-name grammar production, so
        // whatever works schema-qualified also works catalog-qualified.
        InfoType::CatalogUsage => {
            return Ok(InfoValue::U32(
                SQL_CU_DML_STATEMENTS
                    | SQL_CU_PROCEDURE_INVOCATION
                    | SQL_CU_TABLE_DEFINITION
                    | SQL_CU_PRIVILEGE_DEFINITION,
            ));
        }
        // Trino's qualified names read catalog.schema.table -- catalog first.
        InfoType::CatalogLocation => return Ok(InfoValue::U16(SQL_CL_START)),
        // SQL_TXN_CAPABLE is `An SQLUSMALLINT value` per the SQLGetInfo
        // spec, not SQLUINTEGER -- found by the info-type conformance test
        // (`stackable_odbc_core::conformance`). SQL_TC_NONE: this driver does not
        // implement SQLEndTran/manual-commit. Trino does support transactions,
        // so this reports a driver limitation rather than a platform one:
        // see TrinoBackend::end_tran.
        InfoType::TransactionCapable => return Ok(InfoValue::U16(SQL_TC_NONE as u16)),
        // SQL_GD_BLOCK is deliberately not claimed: it means SQLGetData can
        // be called for a row in a block cursor after a bulk fetch, but no
        // driver in this workspace has block cursors -- `SQLSetStmtAttrW`
        // (`stackable-odbc-core/src/ffi/stmt_attr.rs`) rejects any
        // SQL_ATTR_ROW_ARRAY_SIZE other than 1, substituting 1 back with
        // 01S02, so an application can never obtain a multi-row rowset from
        // either backend. SQL_GD_BOUND does hold: `sql_get_data`
        // (`stackable-odbc-core/src/ffi/fetch.rs`) never checks `stmt.bindings` before
        // reading a column, so a column bound via `SQLBindCol` can still be
        // fetched again through `SQLGetData`. AGENTS.md's "always return
        // 0x0F" Windows DM checklist item overclaimed SQL_GD_BLOCK for both
        // drivers; corrected there too.
        InfoType::GetDataExtensions => {
            return Ok(InfoValue::U32(
                SQL_GD_ANY_COLUMN | SQL_GD_ANY_ORDER | SQL_GD_BOUND,
            ));
        }
        // Three `"Y"`/`"N"` info types with no arm in core's
        // `default_get_info`. Without these they reach an application as the
        // empty string, which is not one of the two values the spec defines
        // for any of them.
        //
        // `SQL_MULT_RESULT_SETS`: a Trino statement produces exactly one
        // result set and `SQLMoreResults` never reports another.
        // `SQL_NEED_LONG_DATA_LEN`: bound parameters are interpolated into the
        // SQL as literals (`crate::backend::params`), so no length is ever
        // needed ahead of the value. `SQL_MAX_ROW_SIZE_INCLUDES_LONG`:
        // `SQL_MAX_ROW_SIZE` is `0`, the spec's "no specified limit or the
        // limit is unknown", so there is no maximum for long columns to count
        // against.
        InfoType::MultResultSets | InfoType::NeedLongDataLen | InfoType::MaxRowSizeIncludesLong => {
            return Ok(InfoValue::String("N".into()));
        }
        // SQL_CATALOG_NAME, SQL_NULL_COLLATION, SQL_OJ_CAPABILITIES,
        // SQL_IDENTIFIER_CASE, SQL_DEFAULT_TXN_ISOLATION and
        // SQL_TXN_ISOLATION_OPTION are deliberately *not* answered here. Core
        // derives each from a `Backend` hook (`supports_catalogs`,
        // `null_collation`, `outer_join_capabilities`, `identifier_case`,
        // `default_txn_isolation`,
        // `txn_isolation_options`), and an arm here would shadow the hook for
        // `SQLGetInfo` while the hook still drove `SQLGetConnectAttr` and the
        // `HY024` validation in `sql_set_connect_attr` -- the two answers
        // could then disagree for the same connection. See the hook
        // implementations in `crate::backend`.
        _ => {}
    }

    // Fall through to shared defaults. Core reads the catalog column widths
    // from `TrinoBackend::catalog_result_column_widths` on the type parameter,
    // so the `SQL_MAX_*_NAME_LEN` group cannot disagree with what this backend
    // reports everywhere else.
    default_get_info::<TrinoBackend>(info_type).ok_or_else(|| TrinoError::NotImplemented {
        feature: format!("get_info({info_type:?})"),
    })
}

pub(super) fn get_info(
    conn: &TrinoConnection,
    info_type: InfoType,
) -> Result<InfoValue, TrinoError> {
    // Connection-dependent: captured from the coordinator at connect time.
    if info_type == InfoType::DbmsVer {
        return Ok(InfoValue::String(conn.dbms_version.clone()));
    }
    trino_get_info(info_type)
}

pub(super) fn get_info_pre_connect(info_type: InfoType) -> Result<InfoValue, TrinoError> {
    // Before a connection exists there is no server to report a version for.
    // The empty string is the spec's "not available"; returning SQL_ERROR here
    // would corrupt the Windows DM's state (see AGENTS.md).
    if info_type == InfoType::DbmsVer {
        return Ok(InfoValue::String(String::new()));
    }
    trino_get_info(info_type)
}

/// Trino releases at which SQL-92 features this driver reports became available.
/// Sourced from the Trino release notes.
const TRINO_CORRESPONDING_SINCE: u32 = 475;
const TRINO_MATCH_AND_UNIQUE_SINCE: u32 = 482;
const TRINO_OVERLAPS_SINCE: u32 = 483;

/// `SQL_ALTER_TABLE` — the `ALTER TABLE` clauses Trino's grammar accepts,
/// each confirmed against a live coordinator rather than read off the docs:
///
/// | Statement | Result |
/// |---|---|
/// | `ADD COLUMN d varchar` | accepted → `SQL_AT_ADD_COLUMN_SINGLE` |
/// | `ADD COLUMN f integer NOT NULL` | accepted → `SQL_AT_ADD_CONSTRAINT` |
/// | `DROP COLUMN c` | accepted → `SQL_AT_DROP_COLUMN` |
/// | `ADD COLUMN e integer DEFAULT 1` | `SYNTAX_ERROR` |
/// | `DROP COLUMN b CASCADE` / `RESTRICT` | `SYNTAX_ERROR` |
/// | `ALTER COLUMN a SET DEFAULT 1` | `SYNTAX_ERROR` |
/// | `ADD CONSTRAINT pk_a PRIMARY KEY (a)` | `SYNTAX_ERROR` |
///
/// `SQL_AT_DROP_COLUMN` is the ODBC 2.0 flag, used because ODBC 3.0 has no bit
/// for a `DROP COLUMN` without `CASCADE`/`RESTRICT` — the only form Trino has.
/// `SQL_AT_ADD_CONSTRAINT` *is* a live ODBC 3.0 bit (FIPS Transitional level)
/// despite sitting in `sql.h` beside the two deprecated ones.
///
/// None of the four `SQL_AT_CONSTRAINT_*` deferrability bits are claimed:
/// Trino has no `DEFERRABLE`/`INITIALLY DEFERRED` syntax.
pub(super) const TRINO_ALTER_TABLE: u32 =
    SQL_AT_ADD_COLUMN_SINGLE | SQL_AT_ADD_CONSTRAINT | SQL_AT_DROP_COLUMN;

/// `SQL_OJ_CAPABILITIES` — Trino supports `LEFT`, `RIGHT`, `FULL` and `INNER`
/// outer joins, nested outer joins, all comparison operators in the `ON`
/// clause, and does not require the outer-join tables in any particular order.
pub(super) const TRINO_OUTER_JOIN_CAPABILITIES: u32 = SQL_OJ_LEFT
    | SQL_OJ_RIGHT
    | SQL_OJ_FULL
    | SQL_OJ_NESTED
    | SQL_OJ_NOT_ORDERED
    | SQL_OJ_INNER
    | SQL_OJ_ALL_COMPARISON_OPS;

/// `SQL_AGGREGATE_FUNCTIONS` — every ODBC aggregate has a Trino equivalent.
/// `DISTINCT`/`ALL` come from the `setQuantifier` production in Trino's
/// grammar rather than the function reference, which does not spell them out.
/// <https://trino.io/docs/current/functions/aggregate.html>
pub(crate) const TRINO_AGGREGATE_FUNCTIONS: u32 =
    SQL_AF_AVG | SQL_AF_COUNT | SQL_AF_MAX | SQL_AF_MIN | SQL_AF_SUM | SQL_AF_DISTINCT | SQL_AF_ALL;

/// `SQL_SQL92_VALUE_EXPRESSIONS` — all four are present.
/// <https://trino.io/docs/current/functions/conditional.html>,
/// <https://trino.io/docs/current/functions/conversion.html>
pub(crate) const TRINO_SQL92_VALUE_EXPRESSIONS: u32 =
    SQL_SVE_CASE | SQL_SVE_CAST | SQL_SVE_COALESCE | SQL_SVE_NULLIF;

/// `SQL_NUMERIC_FUNCTIONS` — every defined ODBC numeric function has a Trino
/// equivalent except `COT`, which Trino's math reference does not list (its
/// trigonometric set is acos/asin/atan/atan2/cos/cosh/sin/sinh/tan/tanh).
///
/// ODBC's `LOG` is the natural logarithm, so it maps to Trino's `ln()`;
/// Trino's own `log(b, x)` is base-b and is a different function.
/// <https://trino.io/docs/current/functions/math.html>
pub(crate) const TRINO_NUMERIC_FUNCTIONS: u32 = SQL_FN_NUM_ABS
    | SQL_FN_NUM_ACOS
    | SQL_FN_NUM_ASIN
    | SQL_FN_NUM_ATAN
    | SQL_FN_NUM_ATAN2
    | SQL_FN_NUM_CEILING
    | SQL_FN_NUM_COS
    | SQL_FN_NUM_EXP
    | SQL_FN_NUM_FLOOR
    | SQL_FN_NUM_LOG
    | SQL_FN_NUM_MOD
    | SQL_FN_NUM_SIGN
    | SQL_FN_NUM_SIN
    | SQL_FN_NUM_SQRT
    | SQL_FN_NUM_TAN
    | SQL_FN_NUM_PI
    | SQL_FN_NUM_RAND
    | SQL_FN_NUM_DEGREES
    | SQL_FN_NUM_LOG10
    | SQL_FN_NUM_POWER
    | SQL_FN_NUM_RADIANS
    | SQL_FN_NUM_ROUND
    | SQL_FN_NUM_TRUNCATE;

/// Trino's reserved words, as `SQL_KEYWORDS` (89) needs them: the raw list,
/// which core then filters against `ODBC_RESERVED_KEYWORDS`, sorts and joins.
/// Of the 83 below, 22 survive that subtraction.
///
/// Transcribed from <https://trino.io/docs/current/language/reserved.html>
/// rather than read out of the server, because there is nothing to read.
/// Trino has no equivalent of SQLite's `sqlite3_keyword_name`: `system.jdbc`
/// -- the schema backing the JDBC driver's `DatabaseMetaData`, and so the one
/// place such a list would live -- has no keywords table, nor does
/// `system.metadata`, and this driver speaks HTTP so there is no library to
/// ask. Trino's own JDBC driver hardcodes `getSQLKeywords()` for the same
/// reason.
///
/// Deliberately **not** gated on `server_major`, unlike the SQL-92 predicate
/// and join-operator bitmaps. The safe direction is inverted here: over-
/// reporting a keyword only makes an application quote an identifier it need
/// not have, while under-reporting leaves a genuinely reserved word unquoted
/// and the statement fails to parse. So this tracks the newest list rather
/// than the connected server's. The drift is small and additive -- of the
/// twelve sampled against a live 467, eleven were already reserved and only
/// `AUTO` was newer.
pub(crate) const TRINO_RESERVED_KEYWORDS: &[&str] = &[
    "ALTER",
    "AND",
    "AS",
    "AUTO",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CUBE",
    "CURRENT_CATALOG",
    "CURRENT_DATE",
    "CURRENT_PATH",
    "CURRENT_ROLE",
    "CURRENT_SCHEMA",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "DEALLOCATE",
    "DELETE",
    "DESCRIBE",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "ESCAPE",
    "EXCEPT",
    "EXISTS",
    "EXTRACT",
    "FALSE",
    "FOR",
    "FROM",
    "FULL",
    "GROUP",
    "GROUPING",
    "HAVING",
    "IN",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "JSON_ARRAY",
    "JSON_EXISTS",
    "JSON_OBJECT",
    "JSON_QUERY",
    "JSON_TABLE",
    "JSON_VALUE",
    "LEFT",
    "LIKE",
    "LISTAGG",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "NATURAL",
    "NORMALIZE",
    "NOT",
    "NULL",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVERLAPS",
    "PREPARE",
    "RECURSIVE",
    "RIGHT",
    "ROLLUP",
    "SELECT",
    "SKIP",
    "TABLE",
    "THEN",
    "TRIM",
    "TRUE",
    "UESCAPE",
    "UNION",
    "UNNEST",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WITH",
];

/// What the `SQL_*_FUNCTIONS` bitmaps mean here.
///
/// The spec defines them in terms of the ODBC scalar-function escape, not in
/// terms of what the data source can do by some other spelling: "an
/// application can determine which string functions are supported by a driver
/// by calling `SQLGetInfo` with an *information type* of
/// `SQL_STRING_FUNCTIONS`", and what it emits next is `{fn NAME(...)}`. So a
/// bit may only be set when `stackable_odbc_core::escape::translate_escapes`,
/// driven by [`crate::escape_dialect`], turns that escape into Trino SQL that
/// runs. `untranslatable_escapes_are_never_advertised` holds the two in step.
///
/// Twelve of these were dropped once it turned out their escapes reached the
/// coordinator untranslated and failed there, and restored once
/// `EscapeDialect::rewrite_scalar_fn` made the rewrites expressible.
/// `crate::escape_dialect` records what each one becomes.
///
/// `SQL_STRING_FUNCTIONS` — `LCASE` is `lower()`, `UCASE` is `upper()`,
/// `CHAR` is `chr()`; `LOCATE(a, b)` becomes `position(a IN b)`; the rest are
/// spelled identically in Trino.
///
/// `SQL_FN_STR_LOCATE_2` rather than `SQL_FN_STR_LOCATE`: the spec splits the
/// two forms, and only the two-argument one is claimed. ODBC's optional third
/// argument is a start offset, where the third argument of Trino's `strpos()`
/// is an occurrence index, so there is nothing to rewrite it to.
///
/// Deliberately absent: `LEFT`/`RIGHT`/`SPACE`/`INSERT` (no such function);
/// `REPEAT` (Trino's `repeat` is an *array* function); `ASCII` (`codepoint()`
/// requires exactly one character, where ODBC's `ASCII` takes the leftmost
/// character of any string); `DIFFERENCE` (`levenshtein_distance()` is a
/// different metric); the four `*_LENGTH` variants, which Trino does not
/// document.
///
/// `LENGTH` is claimed with one caveat: ODBC defines it as excluding trailing
/// blanks and Trino's `length()` counts them.
/// <https://trino.io/docs/current/functions/string.html>
pub(crate) const TRINO_STRING_FUNCTIONS: u32 = SQL_FN_STR_CONCAT
    | SQL_FN_STR_LTRIM
    | SQL_FN_STR_LENGTH
    | SQL_FN_STR_LCASE
    | SQL_FN_STR_LOCATE_2
    | SQL_FN_STR_POSITION
    | SQL_FN_STR_REPLACE
    | SQL_FN_STR_RTRIM
    | SQL_FN_STR_SUBSTRING
    | SQL_FN_STR_UCASE
    | SQL_FN_STR_CHAR
    | SQL_FN_STR_SOUNDEX;

/// `SQL_SYSTEM_FUNCTIONS` — `USERNAME` is the bare `current_user` keyword,
/// `DBNAME` the bare `current_catalog`, and `IFNULL` is `coalesce(a, b)`
/// (Trino documents no `ifnull`/`nvl`, but two-argument `coalesce` is exactly
/// equivalent). The first two need the escape's `()` removed, which is
/// [`crate::escape_dialect::rewrite_scalar_fn`]'s job.
/// <https://trino.io/docs/current/functions/session.html>,
/// <https://trino.io/docs/current/functions/conditional.html>
pub(crate) const TRINO_SYSTEM_FUNCTIONS: u32 =
    SQL_FN_SYS_USERNAME | SQL_FN_SYS_DBNAME | SQL_FN_SYS_IFNULL;

/// `SQL_TIMEDATE_FUNCTIONS` — the names Trino spells identically, plus the
/// rewritten ones: `CURDATE`/`CURTIME` and the three ODBC 3.x `CURRENT_*`
/// forms become bare keywords, `TIMESTAMPADD`/`TIMESTAMPDIFF` become
/// `date_add`/`date_diff` with the unit re-quoted, and `DAYOFWEEK` becomes an
/// expression converting Trino's ISO numbering to ODBC's.
///
/// `EXTRACT` needs no rewrite: ODBC's `EXTRACT(field FROM source)` is already
/// Trino's syntax, so the escape passes through untouched.
///
/// One caveat: Trino's `week()` is ISO week numbering, so a client that
/// trusts the ODBC convention can be off by one.
///
/// Deliberately absent: `DAYNAME` and `MONTHNAME`, which Trino has no
/// function for -- only `format_datetime()` with a pattern.
/// <https://trino.io/docs/current/functions/datetime.html>
pub(crate) const TRINO_TIMEDATE_FUNCTIONS: u32 = SQL_FN_TD_NOW
    | SQL_FN_TD_CURDATE
    | SQL_FN_TD_CURTIME
    | SQL_FN_TD_CURRENT_DATE
    | SQL_FN_TD_CURRENT_TIME
    | SQL_FN_TD_CURRENT_TIMESTAMP
    | SQL_FN_TD_DAYOFMONTH
    | SQL_FN_TD_DAYOFWEEK
    | SQL_FN_TD_DAYOFYEAR
    | SQL_FN_TD_MONTH
    | SQL_FN_TD_QUARTER
    | SQL_FN_TD_WEEK
    | SQL_FN_TD_YEAR
    | SQL_FN_TD_HOUR
    | SQL_FN_TD_MINUTE
    | SQL_FN_TD_SECOND
    | SQL_FN_TD_TIMESTAMPADD
    | SQL_FN_TD_TIMESTAMPDIFF
    | SQL_FN_TD_EXTRACT;

/// `SQL_SQL92_PREDICATES` for a coordinator of major version `server_major`.
///
/// `MATCH` and `UNIQUE` arrived in Trino 482 and `OVERLAPS` in 483, so a
/// server older than that must not claim them -- a BI tool that folds an
/// unsupported predicate gets a parse error, which is worse than not folding.
/// `server_major` is `0` when the version probe failed, which gates all three
/// off.
///
/// <https://trino.io/docs/current/functions/comparison.html>
fn sql92_predicates(server_major: u32) -> u32 {
    let mut predicates = SQL_SP_EXISTS
        | SQL_SP_ISNOTNULL
        | SQL_SP_ISNULL
        | SQL_SP_LIKE
        | SQL_SP_IN
        | SQL_SP_BETWEEN
        | SQL_SP_COMPARISON
        | SQL_SP_QUANTIFIED_COMPARISON;

    if server_major >= TRINO_MATCH_AND_UNIQUE_SINCE {
        predicates |= SQL_SP_MATCH_FULL
            | SQL_SP_MATCH_PARTIAL
            | SQL_SP_MATCH_UNIQUE_FULL
            | SQL_SP_MATCH_UNIQUE_PARTIAL
            | SQL_SP_UNIQUE;
    }
    if server_major >= TRINO_OVERLAPS_SINCE {
        predicates |= SQL_SP_OVERLAPS;
    }
    predicates
}

/// `SQL_SQL92_RELATIONAL_JOIN_OPERATORS` for a coordinator of major version
/// `server_major`.
///
/// `CORRESPONDING` on a set operation arrived in Trino 475. `UNION JOIN` has
/// no production in Trino's grammar at any version, so it is never claimed.
///
/// `NATURAL JOIN` is deliberately *not* claimed, despite being grammatically
/// present (`SqlBase.g4` accepts it) -- a live Trino 467 coordinator rejects
/// it at analysis time with `NOT_SUPPORTED: Natural join not supported`, so
/// grammar acceptance alone overstates capability. Confirmed against a live
/// coordinator.
///
/// <https://trino.io/docs/current/sql/select.html>
fn sql92_join_operators(server_major: u32) -> u32 {
    let mut operators = SQL_SRJO_CROSS_JOIN
        | SQL_SRJO_EXCEPT_JOIN
        | SQL_SRJO_FULL_OUTER_JOIN
        | SQL_SRJO_INNER_JOIN
        | SQL_SRJO_INTERSECT_JOIN
        | SQL_SRJO_LEFT_OUTER_JOIN
        | SQL_SRJO_RIGHT_OUTER_JOIN;

    if server_major >= TRINO_CORRESPONDING_SINCE {
        operators |= SQL_SRJO_CORRESPONDING_CLAUSE;
    }
    operators
}

pub(super) fn get_info_raw(
    conn: &TrinoConnection,
    info_type: u16,
) -> Option<Result<InfoValue, TrinoError>> {
    // Power BI reads these capability info types to decide which operations
    // can be folded to SQL. Each one is a genuine `odbc_sys::InfoType`
    // variant (odbc-sys 0.31), but this driver still matches on the raw
    // `u16` here rather than the typed `InfoType` in `trino_get_info`,
    // because `get_info_raw` is the dispatch stage that runs before the
    // Driver-Manager-safe default and unconditionally wins for these types
    // (see `info_type_default_response` in `stackable-odbc-core/src/ffi/info.rs`).
    //
    // The scalar-function bitmaps describe Trino *equivalents*, not literal
    // ODBC escape-sequence support in the naive sense: `SQLExecDirectW` /
    // `SQLPrepareW` / `SQLNativeSqlW` do translate `{fn NAME(...)}` escapes
    // (`stackable_odbc_core::escape::translate_escapes`, driven by
    // `TrinoBackend::escape_dialect()` -- see `crate::escape_dialect`), so
    // `{fn ABS(x)}` becomes `ABS(x)` and succeeds, and names Trino spells
    // differently are remapped (`UCASE`->`upper`, `LOG`->`ln`,
    // `IFNULL`->`coalesce`, ...). A handful of advertised functions need an
    // argument-syntax change that a bare name substitution cannot make --
    // `LOCATE`, `CURDATE`/`CURTIME`, `TIMESTAMPADD`/`TIMESTAMPDIFF`,
    // `USERNAME`/`DBNAME`, `DAYOFWEEK` -- and are deliberately left
    // untranslated; see the `crate::escape_dialect` module doc comment for
    // why each one can't be fixed by renaming alone.
    match info_type {
        SQL_AGGREGATE_FUNCTIONS => Some(Ok(InfoValue::U32(TRINO_AGGREGATE_FUNCTIONS))),
        SQL_SQL92_PREDICATES => Some(Ok(InfoValue::U32(sql92_predicates(conn.server_major)))),
        SQL_SQL92_RELATIONAL_JOIN_OPERATORS => {
            Some(Ok(InfoValue::U32(sql92_join_operators(conn.server_major))))
        }
        SQL_SQL92_VALUE_EXPRESSIONS => Some(Ok(InfoValue::U32(TRINO_SQL92_VALUE_EXPRESSIONS))),
        SQL_NUMERIC_FUNCTIONS => Some(Ok(InfoValue::U32(TRINO_NUMERIC_FUNCTIONS))),
        SQL_STRING_FUNCTIONS => Some(Ok(InfoValue::U32(TRINO_STRING_FUNCTIONS))),
        SQL_SYSTEM_FUNCTIONS => Some(Ok(InfoValue::U32(TRINO_SYSTEM_FUNCTIONS))),
        SQL_TIMEDATE_FUNCTIONS => Some(Ok(InfoValue::U32(TRINO_TIMEDATE_FUNCTIONS))),
        // Trino supports LIKE ... ESCAPE and full outer joins.
        SQL_LIKE_ESCAPE_CLAUSE => Some(Ok(InfoValue::String("Y".into()))),
        SQL_OUTER_JOINS => Some(Ok(InfoValue::String("Y".into()))),
        // The catalog this connection was opened against. Answered here
        // rather than left to `common_get_info_raw`, whose shared answer is
        // the empty string -- correct only for a backend that cannot name its
        // current database. The empty string still stands for a connection
        // that named no catalog, which is the spec's "not available".
        SQL_DATABASE_NAME => Some(Ok(InfoValue::String(
            conn.catalog.clone().unwrap_or_default(),
        ))),
        _ => common_get_info_raw::<TrinoBackend>(info_type).map(Ok),
    }
}

/// The functions this driver reports as supported through `SQLGetFunctions`.
///
/// Together with [`TRINO_WITHHELD_FUNCTIONS`] this partitions
/// `CORE_EXPORTED_FUNCTIONS` exactly, which
/// `every_core_exported_function_is_advertised_or_withheld` asserts. The list
/// is opt-in rather than `CORE_EXPORTED_FUNCTIONS` minus the withheld set,
/// and the direction matters: core exporting a symbol answers "does this entry
/// point exist?", not "does this driver implement the function behind it?".
/// Deriving the list would make a newly exported core function advertised
/// without anyone deciding it works, and the Windows Driver Manager builds its
/// dispatch table from this answer.
const TRINO_ADVERTISED_FUNCTIONS: &[FunctionId] = &[
    FunctionId::AllocHandle,
    FunctionId::FreeHandle,
    FunctionId::Connect,
    FunctionId::DriverConnect,
    FunctionId::Disconnect,
    FunctionId::GetInfo,
    FunctionId::GetFunctions,
    FunctionId::GetDiagRec,
    FunctionId::ExecDirect,
    FunctionId::Prepare,
    FunctionId::Execute,
    FunctionId::Fetch,
    FunctionId::GetData,
    FunctionId::NumResultCols,
    FunctionId::DescribeCol,
    FunctionId::ColAttribute,
    FunctionId::RowCount,
    FunctionId::CloseCursor,
    FunctionId::FreeStmt,
    FunctionId::MoreResults,
    FunctionId::Tables,
    FunctionId::Columns,
    FunctionId::GetTypeInfo,
    // Attribute and diagnostic functions: the Windows DM uses these
    // heavily. Missing from the 3.x bitmap causes NULL dispatch crashes.
    FunctionId::SetEnvAttr,
    FunctionId::GetEnvAttr,
    FunctionId::SetConnectAttr,
    FunctionId::GetConnectAttr,
    FunctionId::SetStmtAttr,
    FunctionId::GetStmtAttr,
    FunctionId::GetDiagField,
    FunctionId::BindCol,
    FunctionId::Cancel,
    FunctionId::EndTran,
    FunctionId::FetchScroll,
    FunctionId::BindParameter,
    FunctionId::NativeSql,
    FunctionId::NumParams,
    FunctionId::PrimaryKeys,
    FunctionId::ForeignKeys,
    FunctionId::Statistics,
    FunctionId::SpecialColumns,
    FunctionId::Procedures,
    FunctionId::ProcedureColumns,
    FunctionId::GetCursorName,
    FunctionId::SetCursorName,
    FunctionId::ColumnPrivileges,
    FunctionId::TablePrivileges,
    FunctionId::DescribeParam,
    // Data-at-execution (fully implemented in stackable-odbc-core) and the remaining
    // exported entry points that delegate to a real implementation. Listed so
    // the Windows DM 3.x dispatch bitmap has no gaps.
    FunctionId::ParamData,
    FunctionId::PutData,
    FunctionId::BrowseConnect,
    FunctionId::BulkOperations,
    FunctionId::SetPos,
];

/// The functions core exports an entry point for that this driver deliberately
/// does not advertise, each with the reason.
///
/// Reporting one of these supported is not merely optimistic: `SQLGetFunctions`
/// is what the Windows Driver Manager builds its dispatch table from, and an
/// application that reads the bitmap will call what it finds there.
///
/// The reasons live here rather than in the test that checks the partition,
/// because the decision is the part worth reading.
///
/// Nothing reads this at runtime, and that is the point: `get_functions`
/// returns [`TRINO_ADVERTISED_FUNCTIONS`] directly rather than subtracting this
/// from `CORE_EXPORTED_FUNCTIONS`, so that a function core adds later is
/// advertised only once someone says it works. What consumes this is
/// `every_core_exported_function_is_advertised_or_withheld`, which is what
/// turns "someone says so" into a build failure. `#[cfg(test)]` would compile
/// it out of the driver, but it would also file the reasoning under test
/// scaffolding, which is the opposite of why it is written down.
// `allow` rather than `expect`: the lib is compiled both as a library, where
// this is dead, and as a test target, where the partition test reads it -- so
// an expectation would go unfulfilled in one of the two and fail the build.
#[allow(dead_code)]
const TRINO_WITHHELD_FUNCTIONS: &[(FunctionId, &str)] = &[
    (
        FunctionId::AllocConnect,
        "ODBC 2.x, superseded by SQLAllocHandle",
    ),
    (
        FunctionId::AllocEnv,
        "ODBC 2.x, superseded by SQLAllocHandle",
    ),
    (
        FunctionId::AllocStmt,
        "ODBC 2.x, superseded by SQLAllocHandle",
    ),
    (FunctionId::Error, "ODBC 2.x, superseded by SQLGetDiagRec"),
    (
        FunctionId::ExtendedFetch,
        "ODBC 2.x, superseded by SQLFetchScroll",
    ),
    (
        FunctionId::FreeConnect,
        "ODBC 2.x, superseded by SQLFreeHandle",
    ),
    (FunctionId::FreeEnv, "ODBC 2.x, superseded by SQLFreeHandle"),
    (
        FunctionId::GetConnectOption,
        "ODBC 2.x, superseded by SQLGetConnectAttr",
    ),
    (
        FunctionId::GetStmtOption,
        "ODBC 2.x, superseded by SQLGetStmtAttr",
    ),
    (
        FunctionId::SetConnectOption,
        "ODBC 2.x, superseded by SQLSetConnectAttr",
    ),
    (
        FunctionId::SetStmtOption,
        "ODBC 2.x, superseded by SQLSetStmtAttr",
    ),
    (
        FunctionId::SetScrollOptions,
        "ODBC 2.x, superseded by the SQL_ATTR_CURSOR_* statement attributes",
    ),
    (FunctionId::Transact, "ODBC 2.x, superseded by SQLEndTran"),
    // The descriptor-field functions need a descriptor handle to work on.
    // Core allocates one for SQLGetStmtAttr(SQL_ATTR_APP_ROW_DESC) and friends,
    // but nothing accepts SQL_HANDLE_DESC, so there is no handle an application
    // could pass to these.
    (
        FunctionId::GetDescField,
        "requires a descriptor handle; SQL_HANDLE_DESC is not accepted",
    ),
    (
        FunctionId::SetDescField,
        "requires a descriptor handle; SQL_HANDLE_DESC is not accepted",
    ),
    (
        FunctionId::SetDescRec,
        "requires a descriptor handle; SQL_HANDLE_DESC is not accepted",
    ),
];

pub(super) fn get_functions() -> &'static [FunctionId] {
    TRINO_ADVERTISED_FUNCTIONS
}

pub(super) fn get_type_info() -> &'static [TypeInfoRow] {
    TRINO_TYPE_INFO
}

/// Bare, uppercase data-source-dependent type name for a column, shared by
/// `SQL_DESC_TYPE_NAME` (`SQLColAttributeW`, via `execute.rs`) and
/// `SQLColumns.TYPE_NAME` (`metadata.rs`), so the two never disagree, and so
/// that name always matches a row in [`TRINO_TYPE_INFO`] (the same table
/// `SQLGetTypeInfo` returns via [`get_type_info`]).
///
/// Spec (`SQL_DESC_TYPE_NAME`): "Data source-dependent data type name; for
/// example, "CHAR", "VARCHAR", "MONEY", "LONG VARBINARY", or "CHAR ( ) FOR
/// BIT DATA"." (`SQLColumns.TYPE_NAME` is worded identically, modulo a typo
/// in the truncated "LONG VARBINAR" example.) Every example in both spec
/// pages is a bare name, not a parameterised declaration ("CHAR ( )" is an
/// empty placeholder for a length, not a filled-in one), so `native` (a
/// declaration like `"varchar(50)"`) is never returned verbatim; the caller
/// (`execute.rs`) separately carries the declared length via
/// `type_name_precision`/`type_name_scale`, so stripping it from the name
/// here does not lose it.
///
/// `native` is Trino's own type-name string
/// (`information_schema.columns.data_type`, or a query column's
/// `Column::ty`) and `sql_type` is the `SqlDataType` the caller already
/// computed for it. Parsing `native` via [`TrinoTypeName`] is tried first
/// because several ODBC types share a `DATA_TYPE` with a distinctly-named
/// sibling row (`TIME` vs. `TIME WITH TIME ZONE`; `TIMESTAMP` vs. `TIMESTAMP
/// WITH TIME ZONE`): a reverse lookup from `sql_type` alone cannot recover
/// which one a column actually is, only the native string can. When parsing
/// fails, the function falls back to a deliberately chosen canonical name
/// for `sql_type`, rather than "whichever `TRINO_TYPE_INFO` row happens to sort first under
/// this DATA_TYPE" (which would be `INTERVAL DAY TO SECOND`, an alphabetical
/// accident of that table's required DATA_TYPE/TYPE_NAME sort order (see
/// its doc comment), misnaming every compound type: ARRAY, MAP, ROW,
/// TUPLE, `ipaddress`, and anything Trino reports that this driver has no
/// dedicated `TrinoTypeName` variant for). Every one of those is rendered
/// as `EXT_W_VARCHAR` text by `trino_ty_to_sql_type` (see its own doc
/// comment), so `VARCHAR` is the honest canonical name for that DATA_TYPE,
/// chosen deliberately below, not read off table order.
pub(super) fn trino_bare_type_name(native: &str, sql_type: SqlDataType) -> String {
    if let Some(ty) = TrinoTypeName::parse(native) {
        return ty.name().to_string();
    }
    if sql_type == SqlDataType::EXT_W_VARCHAR {
        return TrinoTypeName::Varchar.name().to_string();
    }
    // No native string reaches this arm today with any other `sql_type`
    // (see `trino_bare_type_name_returns_the_expected_name` below, which
    // pins every known fallback input to the `EXT_W_VARCHAR` arm above);
    // this driver has no established canonical name for any other
    // DATA_TYPE reaching here. Spec (`SQL_DESC_TYPE_NAME`): "If the type is
    // unknown, an empty string is returned."
    tracing::warn!(
        native,
        ?sql_type,
        "trino_bare_type_name: no canonical name established for this SqlDataType; \
         reporting TYPE_NAME as empty string per spec"
    );
    String::new()
}

#[cfg(test)]
mod tests {

    /// Fixed-size types: the "Column Size" appendix formula for these takes
    /// no backend-specific parameter, so the row's value must equal the
    /// formula applied to *the row's own* `data_type`. Deriving the expected
    /// value from `row.data_type` rather than repeating the table's own
    /// arguments is what makes this catch a row built with the wrong
    /// `SqlDataType`, the one way two drivers could disagree on a value the
    /// spec defines as backend-independent.
    ///
    /// This replaces a cross-driver test crate that compared the two drivers'
    /// tables directly. That crate had to link both drivers into one binary,
    /// which duplicates every `extern "system"` ODBC export (see the note in
    /// this crate's Cargo.toml), and it pinned expected sizes as literals,
    /// the very pattern deriving from the formula exists to remove.
    #[test]
    fn fixed_size_type_info_rows_use_the_backend_independent_formula() {
        // Arguments are ignored by the formula for every type listed here;
        // any value proves the point, so use deliberately absurd ones.
        const IGNORED_PRECISION: MaxPrecision = MaxPrecision(-1);
        const IGNORED_SCALE: MaxScale = MaxScale(-1);

        const BACKEND_INDEPENDENT: &[SqlDataType] = &[
            SqlDataType::EXT_BIT,
            SqlDataType::EXT_TINY_INT,
            SqlDataType::SMALLINT,
            SqlDataType::INTEGER,
            SqlDataType::EXT_BIG_INT,
            SqlDataType::REAL,
            SqlDataType::DOUBLE,
            SqlDataType::DATE,
        ];

        for row in TRINO_TYPE_INFO {
            if !BACKEND_INDEPENDENT.contains(&row.data_type) {
                continue;
            }
            let expected = catalog_column_size(row.data_type, IGNORED_PRECISION, IGNORED_SCALE);
            assert_eq!(
                row.column_size, expected,
                "{} (DATA_TYPE {:?}): COLUMN_SIZE is {} but the \
                 backend-independent appendix formula for that DATA_TYPE \
                 gives {} — the row is built from a different SqlDataType \
                 than it reports",
                row.type_name, row.data_type, row.column_size, expected
            );
        }
    }
    use super::*;
    use stackable_odbc_core::types::{
        DEFAULT_IDENTIFIER_LEN, InfoType, InfoValue, SQL_AM_NONE, SQL_CA1_NEXT, SQL_CB_PRESERVE,
        SQL_DRIVER_ODBC_VER_STRING, SQL_FN_CVT_CAST, SQL_FN_STR_LOCATE, SQL_FN_STR_POSITION,
        SQL_FN_SYS_DBNAME, SQL_FN_SYS_USERNAME, SQL_FN_TD_CURDATE, SQL_FN_TD_CURRENT_DATE,
        SQL_FN_TD_CURRENT_TIME, SQL_FN_TD_CURRENT_TIMESTAMP, SQL_FN_TD_CURTIME,
        SQL_FN_TD_DAYOFWEEK, SQL_FN_TD_TIMESTAMPADD, SQL_FN_TD_TIMESTAMPDIFF,
        SQL_GB_GROUP_BY_CONTAINS_SELECT, SQL_GD_ANY_COLUMN, SQL_GD_ANY_ORDER, SQL_GD_BOUND,
        SQL_IC_LOWER, SQL_INSENSITIVE, SQL_MAX_CURSOR_NAME_LEN, SQL_NC_END, SQL_OIC_CORE,
        SQL_SO_FORWARD_ONLY, SQL_SQ_COMPARISON, SQL_SQ_CORRELATED_SUBQUERIES, SQL_SQ_EXISTS,
        SQL_SQ_IN, SQL_SQ_QUANTIFIED, SQL_U_UNION, SQL_U_UNION_ALL,
    };

    enum Expected {
        Str(&'static str),
        U16(u16),
        U32(u32),
    }

    #[rustfmt::skip]
    const EXPECTED: &[(InfoType, Expected)] = &[
        // --- String values ---
        (InfoType::DriverName,                    Expected::Str("stackable-odbc-trino")),
        (InfoType::DbmsName,                      Expected::Str("Trino")),
        (InfoType::DriverOdbcVer,                 Expected::Str(SQL_DRIVER_ODBC_VER_STRING)),
        (InfoType::SearchPatternEscape,            Expected::Str("\\")),
        (InfoType::IdentifierQuoteChar,            Expected::Str("\"")),
        (InfoType::CatalogTerm,                   Expected::Str("catalog")),
        (InfoType::SchemaTerm,                    Expected::Str("schema")),
        (InfoType::CatalogNameSeparator,           Expected::Str(".")),
        (InfoType::ColumnAlias,                   Expected::Str("Y")),
        (InfoType::OrderByColumnsInSelect,         Expected::Str("N")),
        (InfoType::CatalogName,                   Expected::Str("Y")),
        (InfoType::DataSourceName,                Expected::Str("")),
        (InfoType::ServerName,                    Expected::Str("")),
        (InfoType::UserName,                      Expected::Str("")),
        (InfoType::DataSourceReadOnly,             Expected::Str("N")),
        // "N": Trino can filter information_schema by privilege, but only
        // when the deployment configures access control -- see
        // TrinoBackend::accessible_tables.
        (InfoType::AccessibleTables,              Expected::Str("N")),
        (InfoType::AccessibleProcedures,          Expected::Str("N")),
        (InfoType::Integrity,                     Expected::Str("N")),
        (InfoType::SpecialCharacters,             Expected::Str("")),
        (InfoType::XopenCliYear,                  Expected::Str("1995")),
        (InfoType::CollationSeq,                  Expected::Str("")),
        (InfoType::DescribeParameter,             Expected::Str("Y")),
        // --- U16 values ---
        (InfoType::GroupBy,                       Expected::U16(SQL_GB_GROUP_BY_CONTAINS_SELECT)),
        (InfoType::MaxDriverConnections,          Expected::U16(0)),
        (InfoType::MaxConcurrentActivities,       Expected::U16(0)),
        (InfoType::ConcatNullBehavior,            Expected::U16(0)),
        // Derived by stackable-odbc-core from Backend::cursor_commit_behavior,
        // which this driver leaves at CursorBehavior::Preserve: Trino reports
        // SQL_TC_NONE, so no transaction ever closes a cursor.
        (InfoType::CursorCommitBehaviour,         Expected::U16(SQL_CB_PRESERVE)),
        (InfoType::IdentifierCase,                Expected::U16(SQL_IC_LOWER)),
        (InfoType::MaxColumnNameLen,              Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::MaxCursorNameLen,              Expected::U16(SQL_MAX_CURSOR_NAME_LEN)),
        (InfoType::MaxSchemaNameLen,              Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::MaxCatalogNameLen,             Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::MaxTableNameLen,               Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::NullCollation,                 Expected::U16(SQL_NC_END)),
        (InfoType::MaxColumnsInGroupBy,           Expected::U16(0)),
        (InfoType::MaxColumnsInIndex,             Expected::U16(0)),
        (InfoType::MaxColumnsInOrderBy,           Expected::U16(0)),
        (InfoType::MaxColumnsInSelect,            Expected::U16(0)),
        (InfoType::MaxColumnsInTable,             Expected::U16(0)),
        (InfoType::MaxTablesInSelect,             Expected::U16(0)),
        (InfoType::MaxUserNameLen,                Expected::U16(0)),
        (InfoType::ActiveEnvironments,            Expected::U16(0)),
        (InfoType::MaxIdentifierLen,              Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::CatalogLocation,               Expected::U16(SQL_CL_START)),
        // TransactionCapable is SQLUSMALLINT per spec, not SQLUINTEGER -- see
        // the matching comment on its arm in trino_get_info.
        (InfoType::TransactionCapable,            Expected::U16(SQL_TC_NONE as u16)),
        // --- U32 values ---
        // CursorSensitivity is SQLUINTEGER per spec, not SQLUSMALLINT -- see
        // the matching comment in stackable-odbc-core's default_get_info.
        (InfoType::CursorSensitivity,             Expected::U32(SQL_INSENSITIVE as u32)),
        (InfoType::Subqueries,                    Expected::U32(SQL_SQ_COMPARISON | SQL_SQ_EXISTS | SQL_SQ_IN | SQL_SQ_QUANTIFIED | SQL_SQ_CORRELATED_SUBQUERIES)),
        (InfoType::UnionStatement,                Expected::U32(SQL_U_UNION | SQL_U_UNION_ALL)),
        (InfoType::DefaultTxnIsolation,           Expected::U32(0)),
        (InfoType::ScrollOptions,                 Expected::U32(SQL_SO_FORWARD_ONLY)),
        (InfoType::ConvertFunctions,              Expected::U32(SQL_FN_CVT_CAST)),
        (InfoType::TransactionIsolationProtocol,  Expected::U32(0)),
        (InfoType::AlterTable,                    Expected::U32(SQL_AT_ADD_COLUMN_SINGLE | SQL_AT_ADD_CONSTRAINT | SQL_AT_DROP_COLUMN)),
        (InfoType::MaxIndexSize,                  Expected::U32(0)),
        (InfoType::MaxRowSize,                    Expected::U32(0)),
        (InfoType::MaxStatementLen,               Expected::U32(0)),
        (InfoType::OuterJoinCapabilities,         Expected::U32(SQL_OJ_LEFT | SQL_OJ_RIGHT | SQL_OJ_FULL | SQL_OJ_NESTED | SQL_OJ_NOT_ORDERED | SQL_OJ_INNER | SQL_OJ_ALL_COMPARISON_OPS)),
        (InfoType::SqlConformance,                Expected::U32(0)),
        (InfoType::OdbcInterfaceConformance,      Expected::U32(SQL_OIC_CORE)),
        (InfoType::AsyncMode,                     Expected::U32(SQL_AM_NONE)),
        (InfoType::AsyncDbcFunctions,             Expected::U32(0)),
        (InfoType::SchemaUsage,                   Expected::U32(SQL_SU_DML_STATEMENTS | SQL_SU_PROCEDURE_INVOCATION | SQL_SU_TABLE_DEFINITION | SQL_SU_PRIVILEGE_DEFINITION)),
        (InfoType::CatalogUsage,                  Expected::U32(SQL_CU_DML_STATEMENTS | SQL_CU_PROCEDURE_INVOCATION | SQL_CU_TABLE_DEFINITION | SQL_CU_PRIVILEGE_DEFINITION)),
        (InfoType::GetDataExtensions,             Expected::U32(SQL_GD_ANY_COLUMN | SQL_GD_ANY_ORDER | SQL_GD_BOUND)),
        (InfoType::DynamicCursorAttributes1,      Expected::U32(0)),
        (InfoType::DynamicCursorAttributes2,      Expected::U32(0)),
        (InfoType::ForwardOnlyCursorAttributes1,  Expected::U32(SQL_CA1_NEXT)),
        (InfoType::ForwardOnlyCursorAttributes2,  Expected::U32(0)),
        (InfoType::KeysetCursorAttributes1,       Expected::U32(0)),
        (InfoType::KeysetCursorAttributes2,       Expected::U32(0)),
        (InfoType::StaticCursorAttributes1,       Expected::U32(0)),
        (InfoType::StaticCursorAttributes2,       Expected::U32(0)),
    ];

    #[test]
    fn get_info_snapshot() {
        for (info_type, expected) in EXPECTED {
            let actual = trino_get_info(*info_type)
                .unwrap_or_else(|e| panic!("get_info returned error for {info_type:?}: {e:?}"));
            match (expected, &actual) {
                (Expected::Str(s), InfoValue::String(v)) => {
                    assert_eq!(v.as_str(), *s, "wrong value for {info_type:?}")
                }
                (Expected::U16(n), InfoValue::U16(v)) => {
                    assert_eq!(v, n, "wrong value for {info_type:?}")
                }
                (Expected::U32(n), InfoValue::U32(v)) => {
                    assert_eq!(v, n, "wrong value for {info_type:?}")
                }
                _ => panic!("type mismatch for {info_type:?}: got {actual:?}"),
            }
        }
    }

    /// SQL_DRIVER_VER is derived from Cargo.toml, so it cannot be asserted
    /// against a hard-coded literal, which would drift from the crate
    /// version. Assert the spec's shape instead.
    #[test]
    fn driver_ver_is_well_formed() {
        let InfoValue::String(v) = trino_get_info(InfoType::DriverVer).unwrap() else {
            panic!("expected String for DriverVer");
        };
        let parts: Vec<&str> = v.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "SQL_DRIVER_VER must be ##.##.####, got {v:?}"
        );
        assert!(
            parts[0].len() >= 2 && parts[1].len() >= 2 && parts[2].len() >= 4,
            "SQL_DRIVER_VER field widths wrong: {v:?}"
        );
        assert!(
            parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())),
            "SQL_DRIVER_VER must be all digits and dots: {v:?}"
        );
    }

    #[test]
    fn every_reportable_type_has_a_type_info_row() {
        // A driver reporting a type via SQLColumns/SQLDescribeCol
        // that has no corresponding SQLGetTypeInfo row is unusable to
        // applications that consult the type list to decide how to bind.
        //
        // A hand-copied list of declared type strings would fail to catch a
        // *new* `TrinoTypeName` variant/mapping arm that yields a
        // `SqlDataType` with no row, since nothing forces the hand-copied
        // list to grow when the enum does. Iterating
        // `TrinoTypeName::ALL_VARIANTS` instead closes that gap: it is
        // paired with `TrinoTypeName::assert_all_variants_listed`, an
        // exhaustive match with no wildcard arm, so adding a variant to the
        // enum without adding it to `ALL_VARIANTS` is a compile error, not a
        // silently-passing test.
        for ty in TrinoTypeName::ALL_VARIANTS {
            TrinoTypeName::assert_all_variants_listed(ty);
            let reported = ty.sql_type();
            assert!(
                TRINO_TYPE_INFO.iter().any(|row| row.data_type == reported),
                "TrinoTypeName::{ty:?} is reported as {reported:?}, which has no \
                 SQLGetTypeInfo row"
            );
        }

        // Residual gap: types outside the closed `TrinoTypeName` enum
        // (compound/niche types this driver recognises only by string, via
        // `trino_type_name_to_sql_type`'s fallback, not by a dedicated
        // variant) are not covered by the exhaustiveness check above. All of
        // them fall back to SQL_WVARCHAR today (already covered by
        // `TrinoTypeName::Varchar` in the loop above), so this loop adds no
        // additional row coverage; it exists only to pin the concrete
        // fallback inputs the previous hand-written list checked, in case
        // that default ever changes.
        for decl in ["ipaddress", "array(integer)", "row(x integer, y varchar)"] {
            let reported = crate::type_conversion::trino_type_name_to_sql_type(decl);
            assert!(
                TRINO_TYPE_INFO.iter().any(|row| row.data_type == reported),
                "declared type {decl:?} is reported as {reported:?}, \
                 which has no SQLGetTypeInfo row"
            );
        }
    }

    #[test]
    fn trino_bare_type_name_returns_the_expected_name() {
        // The invariant: SQL_DESC_TYPE_NAME
        // (`execute.rs`) and SQLColumns.TYPE_NAME (`metadata.rs`) both call
        // `trino_bare_type_name`, so pin here that its result always matches
        // a row's TYPE_NAME for that same DATA_TYPE, including the native
        // parameterised forms ("varchar(50)", not "VARCHAR")
        // and the WITH TIME ZONE variants, which share a DATA_TYPE with
        // their plain counterpart but must resolve to their own distinct row.
        //
        // This asserts the *exact* expected name, not just that *some*
        // row shares the DATA_TYPE; a membership-only check is what lets
        // the "first row wins" fallback bug through: `("row(x integer)",
        // EXT_W_VARCHAR)` matches *a* row ("INTERVAL DAY TO SECOND") without
        // matching the *right* one, yet still passes.
        let cases = [
            ("varchar(50)", SqlDataType::EXT_W_VARCHAR, "VARCHAR"),
            ("char(10)", SqlDataType::EXT_W_CHAR, "CHAR"),
            ("decimal(10,2)", SqlDataType::DECIMAL, "DECIMAL"),
            ("bigint", SqlDataType::EXT_BIG_INT, "BIGINT"),
            ("time", SqlDataType::TIME, "TIME"),
            (
                "time with time zone",
                SqlDataType::TIME,
                "TIME WITH TIME ZONE",
            ),
            ("timestamp(3)", SqlDataType::TIMESTAMP, "TIMESTAMP"),
            (
                "timestamp(3) with time zone",
                SqlDataType::TIMESTAMP,
                "TIMESTAMP WITH TIME ZONE",
            ),
            // Compound / unmodelled types: `TrinoTypeName::parse` returns
            // `None` for every one of these, so they exercise the canonical
            // `EXT_W_VARCHAR` fallback, which must resolve to "VARCHAR"
            // rather than to "INTERVAL DAY TO SECOND" by table-sort accident.
            ("row(x integer)", SqlDataType::EXT_W_VARCHAR, "VARCHAR"),
            ("array(integer)", SqlDataType::EXT_W_VARCHAR, "VARCHAR"),
            (
                "map(varchar, integer)",
                SqlDataType::EXT_W_VARCHAR,
                "VARCHAR",
            ),
            ("ipaddress", SqlDataType::EXT_W_VARCHAR, "VARCHAR"),
            // INTERVAL types: unlike the compound/unmodelled cases just
            // above, these *are* modelled (`TrinoTypeName::IntervalDayToSecond`/
            // `IntervalYearToMonth`), so they must resolve to their own
            // name, not fall through to the VARCHAR fallback.
            (
                "interval day to second",
                SqlDataType::EXT_W_VARCHAR,
                "INTERVAL DAY TO SECOND",
            ),
            (
                "interval year to month",
                SqlDataType::EXT_W_VARCHAR,
                "INTERVAL YEAR TO MONTH",
            ),
        ];
        for (native, sql_type, expected_name) in cases {
            let name = trino_bare_type_name(native, sql_type);
            assert_eq!(
                name, expected_name,
                "trino_bare_type_name({native:?}, {sql_type:?}) returned an unexpected name"
            );
            assert!(
                TRINO_TYPE_INFO
                    .iter()
                    .any(|row| row.type_name == name && row.data_type == sql_type),
                "trino_bare_type_name({native:?}, {sql_type:?}) returned {name:?}, which is \
                 not a matching SQLGetTypeInfo row"
            );
        }
    }

    #[test]
    fn type_info_rows_have_unique_data_types_per_name() {
        let mut seen = std::collections::HashSet::new();
        for row in TRINO_TYPE_INFO {
            assert!(
                seen.insert(row.type_name),
                "duplicate type_name in TRINO_TYPE_INFO: {}",
                row.type_name
            );
        }
    }

    #[test]
    fn with_time_zone_and_interval_type_names_have_dedicated_rows() {
        // WITH TIME ZONE and INTERVAL types share a DATA_TYPE with
        // their plain/text counterparts, so without a row of their own an
        // application looking up SQLGetTypeInfo by TYPE_NAME (e.g. to build
        // a CREATE TABLE statement) could not find "TIME WITH TIME ZONE" or
        // "INTERVAL DAY TO SECOND" at all; the data_type-presence check in
        // `every_reportable_type_has_a_type_info_row` above does not catch
        // this, since these types are already reported under a shared
        // DATA_TYPE (TIME/TIMESTAMP/VARCHAR); this test guards the distinct
        // TYPE_NAME entries specifically.
        for name in [
            "TIME WITH TIME ZONE",
            "TIMESTAMP WITH TIME ZONE",
            "INTERVAL DAY TO SECOND",
            "INTERVAL YEAR TO MONTH",
        ] {
            assert!(
                TRINO_TYPE_INFO.iter().any(|row| row.type_name == name),
                "missing SQLGetTypeInfo row for {name:?}"
            );
        }
    }

    #[test]
    fn every_type_info_row_is_reachable_via_trino_bare_type_name() {
        // Inverse of `every_reportable_type_has_a_type_info_row` above: that
        // test guards that every `TrinoTypeName` variant maps to *some* row;
        // this one guards the opposite direction: that every
        // `TRINO_TYPE_INFO` row's TYPE_NAME can actually be *produced* by
        // `trino_bare_type_name` for a real column, not merely advertised in
        // the catalog. A row that fails this check reintroduces the interval
        // bug: an application enumerating SQLGetTypeInfo would see a type
        // advertised that no real column can ever claim, because
        // `trino_bare_type_name` has no path that returns it.
        //
        // Reachability is checked by feeding the row's own (lowercased)
        // TYPE_NAME back in as the native Trino type-name string; this
        // works for every row derived from a `TrinoTypeName` variant because
        // `TrinoTypeName::name()` and `TrinoTypeName::parse()` are exact
        // case-insensitive inverses of one another (pinned per-variant by
        // the `parse_*` tests elsewhere in this module and in
        // `type_conversion.rs`).
        //
        // Exceptions: SQL_CHAR (1) and SQL_VARCHAR (12) are ANSI-alias rows
        // present *only* so the Windows DM/pyodbc can find a match when
        // querying `SQLGetTypeInfo` by those legacy ANSI type codes (see
        // their own doc comments in `TRINO_TYPE_INFO` above).
        // `trino_bare_type_name` never returns "SQL_CHAR"/"SQL_VARCHAR" for
        // any real column: every text-affinity column resolves to "CHAR"
        // or "VARCHAR" (the WCHAR-based rows) instead, so these two names
        // are unreachable by design, not by omission.
        const DM_COMPAT_ONLY: &[&str] = &["SQL_CHAR", "SQL_VARCHAR"];

        for row in TRINO_TYPE_INFO {
            if DM_COMPAT_ONLY.contains(&row.type_name) {
                continue;
            }
            let native = row.type_name.to_lowercase();
            let produced = trino_bare_type_name(&native, row.data_type);
            assert_eq!(
                produced, row.type_name,
                "TRINO_TYPE_INFO row {:?} (DATA_TYPE={:?}) is not reachable via \
                 trino_bare_type_name (got {produced:?} instead) — no real column can \
                 ever be reported under this TYPE_NAME",
                row.type_name, row.data_type
            );
        }
    }

    // NOTE: do not assert that the WITH TIME ZONE rows' catalog COLUMN_SIZE
    // equals `TrinoTypeName::fixed_precision()`, the query path's per-column
    // precision. That conflates two quantities that must not be equated:
    // SQLGetTypeInfo's COLUMN_SIZE ("the maximum column size the server
    // supports") and a column's own reported precision are different
    // quantities by spec and are *not* supposed to agree once a data
    // source's maximum exceeds what a single driver-internal struct can
    // carry. These values are covered instead by:
    // - the spec-table test in `stackable_odbc_core::types::column_size` (asserts the
    //   shared formula against the ODBC appendix directly, not against this
    //   driver's own other numbers);
    // - `catalog_column_size_matches_max_fractional_seconds_precision_formula`
    //   below (pins the four temporal rows' actual values against the
    //   formula + the live-verified Trino maximum);
    // - `fixed_size_type_info_rows_use_the_backend_independent_formula` (checks
    //   the fixed-size rows against the shared `catalog_column_size` formula).

    #[test]
    fn catalog_column_size_matches_max_fractional_seconds_precision_formula() {
        // Regression pin: TIME WITH TIME
        // ZONE/TIMESTAMP WITH TIME ZONE's COLUMN_SIZE and MAXIMUM_SCALE must
        // reflect Trino's real, live-verified maximum fractional-seconds
        // precision (12), not an arbitrary smaller value, and the four
        // temporal rows must be internally consistent (same MAXIMUM_SCALE
        // for the plain and WITH TIME ZONE variant of the same base type).
        let time = find_row(TrinoTypeName::Time.name());
        let time_tz = find_row(TrinoTypeName::TimeWithTimeZone.name());
        let timestamp = find_row(TrinoTypeName::Timestamp.name());
        let timestamp_tz = find_row(TrinoTypeName::TimestampWithTimeZone.name());

        assert_eq!(time.column_size, 21); // 9 + 12
        assert_eq!(time_tz.column_size, 27); // 9 + 12 + 6 ("+HH:MM")
        assert_eq!(timestamp.column_size, 32); // 20 + 12
        assert_eq!(timestamp_tz.column_size, 39); // 20 + 12 + 1 (space) + 6

        for row in [time, time_tz, timestamp, timestamp_tz] {
            assert_eq!(
                row.maximum_scale,
                Some(MAX_FRACTIONAL_SECONDS_PRECISION),
                "{:?} MAXIMUM_SCALE must equal Trino's real maximum",
                row.type_name
            );
        }
    }

    fn find_row(type_name: &str) -> &'static TypeInfoRow {
        TRINO_TYPE_INFO
            .iter()
            .find(|row| row.type_name == type_name)
            .unwrap_or_else(|| panic!("no SQLGetTypeInfo row for {type_name:?}"))
    }

    #[test]
    fn type_info_rows_sorted_by_data_type_then_type_name() {
        // Spec (SQLGetTypeInfo): "ordered by DATA_TYPE and then ... TYPE_NAME,
        // both ascending." DATA_TYPE is a signed i16 (negative for ODBC
        // extension types), so the comparison must not treat it as unsigned.
        // This walks adjacent pairs rather than asserting a fixed sequence,
        // so it keeps holding as rows are added or reordered.
        for pair in TRINO_TYPE_INFO.windows(2) {
            let (prev, next) = (&pair[0], &pair[1]);
            assert!(
                prev.data_type.0 <= next.data_type.0,
                "TRINO_TYPE_INFO not sorted by DATA_TYPE: {:?} (DATA_TYPE={}) \
                 appears before {:?} (DATA_TYPE={})",
                prev.type_name,
                prev.data_type.0,
                next.type_name,
                next.data_type.0
            );
            if prev.data_type == next.data_type {
                assert!(
                    prev.type_name <= next.type_name,
                    "rows sharing DATA_TYPE={} not sorted by TYPE_NAME: {:?} appears \
                     before {:?}",
                    prev.data_type.0,
                    prev.type_name,
                    next.type_name
                );
            }
        }
    }

    /// `driver_version!()` must resolve `SQL_DRIVER_VER` from *this* crate's
    /// `CARGO_PKG_VERSION` at the macro's call site, not from `stackable-odbc-core`'s
    /// version at `stackable-odbc-core`'s compile time.
    ///
    /// This test can actually catch that regression because this crate's version
    /// differs from `stackable-odbc-core`'s (see this crate's `Cargo.toml`): if the macro
    /// wrongly resolved against `stackable-odbc-core`, the returned string would not match
    /// the one recomputed here from this crate's own `CARGO_PKG_VERSION`. A
    /// driver crate whose version happened to equal `stackable-odbc-core`'s could not
    /// distinguish the two, so the guarantee is asserted here where the versions
    /// genuinely diverge.
    #[test]
    fn driver_version_tracks_the_crate_version() {
        let (major, minor, release) =
            stackable_odbc_core::types::parse_dotted_version(env!("CARGO_PKG_VERSION"))
                .expect("Cargo always supplies a parseable package version");
        assert_eq!(
            stackable_odbc_core::driver_version!(),
            stackable_odbc_core::types::format_odbc_version(major, minor, release)
        );
    }

    use stackable_odbc_core::types::*;

    /// Trino 467 predates MATCH (482), UNIQUE (482) and OVERLAPS (483), so a
    /// server that old must not claim them; it must still claim BETWEEN,
    /// COMPARISON and QUANTIFIED_COMPARISON, which Trino has always had.
    #[test]
    fn sql92_predicates_for_an_old_server() {
        assert_eq!(
            sql92_predicates(467),
            SQL_SP_EXISTS
                | SQL_SP_ISNOTNULL
                | SQL_SP_ISNULL
                | SQL_SP_LIKE
                | SQL_SP_IN
                | SQL_SP_BETWEEN
                | SQL_SP_COMPARISON
                | SQL_SP_QUANTIFIED_COMPARISON
        );
    }

    #[test]
    fn sql92_predicates_gain_match_and_unique_at_482() {
        let before = sql92_predicates(481);
        let after = sql92_predicates(482);
        assert_eq!(before & SQL_SP_MATCH_FULL, 0);
        assert_eq!(
            after & (SQL_SP_MATCH_FULL | SQL_SP_MATCH_PARTIAL | SQL_SP_UNIQUE),
            SQL_SP_MATCH_FULL | SQL_SP_MATCH_PARTIAL | SQL_SP_UNIQUE
        );
        assert_eq!(
            after & SQL_SP_OVERLAPS,
            0,
            "OVERLAPS arrives at 483, not 482"
        );
    }

    #[test]
    fn sql92_predicates_gain_overlaps_at_483() {
        assert_eq!(sql92_predicates(482) & SQL_SP_OVERLAPS, 0);
        assert_eq!(sql92_predicates(483) & SQL_SP_OVERLAPS, SQL_SP_OVERLAPS);
    }

    /// A failed version probe leaves server_major at 0. Every version-gated
    /// flag must be off, so the driver understates rather than overstates.
    #[test]
    fn an_unknown_server_version_claims_no_gated_features() {
        let p = sql92_predicates(0);
        assert_eq!(
            p & (SQL_SP_MATCH_FULL
                | SQL_SP_MATCH_PARTIAL
                | SQL_SP_MATCH_UNIQUE_FULL
                | SQL_SP_MATCH_UNIQUE_PARTIAL
                | SQL_SP_OVERLAPS
                | SQL_SP_UNIQUE),
            0
        );
        assert_eq!(sql92_join_operators(0) & SQL_SRJO_CORRESPONDING_CLAUSE, 0);
    }

    /// CORRESPONDING arrives at 475; UNION JOIN does not exist in Trino at
    /// any version and must never be claimed.
    ///
    /// NATURAL JOIN is also never claimed, at any version: it is accepted by
    /// Trino's grammar but rejected at analysis time (live-verified against
    /// Trino 467: `NOT_SUPPORTED: Natural join not supported`) -- see
    /// `sql92_join_operators`'s doc comment.
    #[test]
    fn sql92_join_operators_track_the_server_version() {
        assert_eq!(sql92_join_operators(474) & SQL_SRJO_CORRESPONDING_CLAUSE, 0);
        assert_eq!(
            sql92_join_operators(475) & SQL_SRJO_CORRESPONDING_CLAUSE,
            SQL_SRJO_CORRESPONDING_CLAUSE
        );
        for v in [467, 475, 483] {
            assert_eq!(
                sql92_join_operators(v) & SQL_SRJO_UNION_JOIN,
                0,
                "Trino has no UNION JOIN at any version"
            );
            assert_eq!(
                sql92_join_operators(v) & SQL_SRJO_NATURAL_JOIN,
                0,
                "Trino rejects NATURAL JOIN at analysis time (live-verified against 467)"
            );
        }
    }

    /// Only defined SQL_FN_NUM_* flags may be set (no bit outside the range,
    /// such as bit 24), and COT must not be claimed — Trino has no cot().
    #[test]
    fn numeric_functions_claim_only_defined_flags_trino_has() {
        let all_defined = SQL_FN_NUM_ABS
            | SQL_FN_NUM_ACOS
            | SQL_FN_NUM_ASIN
            | SQL_FN_NUM_ATAN
            | SQL_FN_NUM_ATAN2
            | SQL_FN_NUM_CEILING
            | SQL_FN_NUM_COS
            | SQL_FN_NUM_COT
            | SQL_FN_NUM_EXP
            | SQL_FN_NUM_FLOOR
            | SQL_FN_NUM_LOG
            | SQL_FN_NUM_MOD
            | SQL_FN_NUM_SIGN
            | SQL_FN_NUM_SIN
            | SQL_FN_NUM_SQRT
            | SQL_FN_NUM_TAN
            | SQL_FN_NUM_PI
            | SQL_FN_NUM_RAND
            | SQL_FN_NUM_DEGREES
            | SQL_FN_NUM_LOG10
            | SQL_FN_NUM_POWER
            | SQL_FN_NUM_RADIANS
            | SQL_FN_NUM_ROUND
            | SQL_FN_NUM_TRUNCATE;
        assert_eq!(
            TRINO_NUMERIC_FUNCTIONS & !all_defined,
            0,
            "a bit outside the defined SQL_FN_NUM_* range is set"
        );
        assert_eq!(
            TRINO_NUMERIC_FUNCTIONS & SQL_FN_NUM_COT,
            0,
            "Trino has no cot() function"
        );
        assert_eq!(
            TRINO_NUMERIC_FUNCTIONS,
            all_defined & !SQL_FN_NUM_COT,
            "Trino supports every defined numeric function except COT"
        );
    }

    /// RIGHT and ASCII must not be claimed (Trino lacks them); LTRIM and
    /// RTRIM must be (Trino has them).
    #[test]
    fn string_functions_match_trinos_documented_set() {
        assert_eq!(
            TRINO_STRING_FUNCTIONS,
            SQL_FN_STR_CONCAT
                | SQL_FN_STR_LTRIM
                | SQL_FN_STR_LENGTH
                | SQL_FN_STR_LCASE
                | SQL_FN_STR_LOCATE_2
                | SQL_FN_STR_POSITION
                | SQL_FN_STR_REPLACE
                | SQL_FN_STR_RTRIM
                | SQL_FN_STR_SUBSTRING
                | SQL_FN_STR_UCASE
                | SQL_FN_STR_CHAR
                | SQL_FN_STR_SOUNDEX
        );
        for absent in [
            SQL_FN_STR_RIGHT,
            SQL_FN_STR_LEFT,
            SQL_FN_STR_ASCII,
            SQL_FN_STR_REPEAT,
            SQL_FN_STR_INSERT,
            SQL_FN_STR_DIFFERENCE,
            SQL_FN_STR_SPACE,
            SQL_FN_STR_LOCATE,
            SQL_FN_STR_BIT_LENGTH,
            SQL_FN_STR_CHAR_LENGTH,
            SQL_FN_STR_CHARACTER_LENGTH,
            SQL_FN_STR_OCTET_LENGTH,
        ] {
            assert_eq!(TRINO_STRING_FUNCTIONS & absent, 0);
        }
    }

    /// All three of USERNAME, DBNAME and IFNULL, each under its correct flag
    /// (IFNULL is a distinct flag from DBNAME's 0x02). The first two are
    /// rewrites rather than remaps; see
    /// [`every_advertised_rewrite_has_a_translation`].
    #[test]
    fn system_functions_include_all_three_equivalents() {
        assert_eq!(
            TRINO_SYSTEM_FUNCTIONS,
            SQL_FN_SYS_USERNAME | SQL_FN_SYS_DBNAME | SQL_FN_SYS_IFNULL
        );
    }

    /// The invariant the `SQL_*_FUNCTIONS` bitmaps exist to keep: a bit is
    /// advertised only when `{fn NAME(...)}` survives translation into Trino
    /// SQL that runs.
    ///
    /// Every name below needs an argument-syntax change that
    /// `EscapeDialect::remap_scalar_fn` cannot make -- it only swaps the
    /// identifier in front of the parentheses. Each was advertised without a
    /// translation once, and a client that read the bitmap and emitted the
    /// escape got `FUNCTION_NOT_FOUND: \'curdate\'`,
    /// `COLUMN_NOT_FOUND: \'sql_tsi_day\'` and the like. They are advertised
    /// again only because `rewrite_scalar_fn` now handles each one, so this
    /// asserts both halves together: the bit is set *and* the rewrite exists.
    ///
    /// `DAYOFWEEK` is the one where the rewrite matters most. Trino has
    /// `day_of_week()`, so a rename would have succeeded -- and returned a
    /// silently wrong, ISO-numbered day.
    #[test]
    fn every_advertised_rewrite_has_a_translation() {
        for (bitmap, name, flag, args) in [
            (
                TRINO_STRING_FUNCTIONS,
                "LOCATE",
                SQL_FN_STR_LOCATE_2,
                "\'b\', \'ab\'",
            ),
            (TRINO_SYSTEM_FUNCTIONS, "USERNAME", SQL_FN_SYS_USERNAME, ""),
            (TRINO_SYSTEM_FUNCTIONS, "DBNAME", SQL_FN_SYS_DBNAME, ""),
            (TRINO_TIMEDATE_FUNCTIONS, "CURDATE", SQL_FN_TD_CURDATE, ""),
            (TRINO_TIMEDATE_FUNCTIONS, "CURTIME", SQL_FN_TD_CURTIME, ""),
            (
                TRINO_TIMEDATE_FUNCTIONS,
                "CURRENT_DATE",
                SQL_FN_TD_CURRENT_DATE,
                "",
            ),
            (
                TRINO_TIMEDATE_FUNCTIONS,
                "CURRENT_TIME",
                SQL_FN_TD_CURRENT_TIME,
                "",
            ),
            (
                TRINO_TIMEDATE_FUNCTIONS,
                "CURRENT_TIMESTAMP",
                SQL_FN_TD_CURRENT_TIMESTAMP,
                "",
            ),
            (
                TRINO_TIMEDATE_FUNCTIONS,
                "TIMESTAMPADD",
                SQL_FN_TD_TIMESTAMPADD,
                "SQL_TSI_DAY, 1, t",
            ),
            (
                TRINO_TIMEDATE_FUNCTIONS,
                "TIMESTAMPDIFF",
                SQL_FN_TD_TIMESTAMPDIFF,
                "SQL_TSI_DAY, a, b",
            ),
            (
                TRINO_TIMEDATE_FUNCTIONS,
                "DAYOFWEEK",
                SQL_FN_TD_DAYOFWEEK,
                "d",
            ),
        ] {
            assert_ne!(
                bitmap & flag,
                0,
                "{name} has a rewrite but is not advertised"
            );
            assert!(
                crate::escape_dialect::rewrite_scalar_fn(name, args).is_some(),
                "{name} is advertised but `{{fn {name}({args})}}` has no rewrite"
            );
        }
    }

    /// `POSITION` is advertised with no translation at all, and that is
    /// correct: ODBC spells it `POSITION(exp IN exp)`, which is already
    /// Trino\'s syntax, so the escape passes through untouched.
    ///
    /// `SQL_FN_STR_LOCATE` -- the *three*-argument form -- must stay
    /// unadvertised. ODBC\'s third argument is a start offset and the third
    /// argument of Trino\'s `strpos()` is an occurrence index, so the rewrite
    /// declines it; advertising the bit would promise a call that then falls
    /// through untranslated.
    #[test]
    fn locate_advertises_only_the_two_argument_form() {
        assert_ne!(TRINO_STRING_FUNCTIONS & SQL_FN_STR_POSITION, 0);
        assert_eq!(
            crate::escape_dialect::rewrite_scalar_fn("POSITION", "\'b\' IN \'ab\'"),
            None,
            "POSITION needs no rewrite -- ODBC already spells it Trino\'s way"
        );

        assert_eq!(TRINO_STRING_FUNCTIONS & SQL_FN_STR_LOCATE, 0);
        assert_eq!(
            crate::escape_dialect::rewrite_scalar_fn("LOCATE", "\'b\', \'ab\', 2"),
            None,
            "the three-argument LOCATE has no Trino equivalent"
        );
    }

    /// NOW, every DAYOF*, EXTRACT, the field extractors, all three ODBC 3.x
    /// CURRENT_* flags and both TIMESTAMP* flags must be claimed.
    #[test]
    fn timedate_functions_match_trinos_documented_set() {
        assert_eq!(
            TRINO_TIMEDATE_FUNCTIONS,
            SQL_FN_TD_NOW
                | SQL_FN_TD_CURDATE
                | SQL_FN_TD_CURTIME
                | SQL_FN_TD_CURRENT_DATE
                | SQL_FN_TD_CURRENT_TIME
                | SQL_FN_TD_CURRENT_TIMESTAMP
                | SQL_FN_TD_DAYOFMONTH
                | SQL_FN_TD_DAYOFWEEK
                | SQL_FN_TD_DAYOFYEAR
                | SQL_FN_TD_MONTH
                | SQL_FN_TD_QUARTER
                | SQL_FN_TD_WEEK
                | SQL_FN_TD_YEAR
                | SQL_FN_TD_HOUR
                | SQL_FN_TD_MINUTE
                | SQL_FN_TD_SECOND
                | SQL_FN_TD_TIMESTAMPADD
                | SQL_FN_TD_TIMESTAMPDIFF
                | SQL_FN_TD_EXTRACT
        );
        assert_eq!(TRINO_TIMEDATE_FUNCTIONS & SQL_FN_TD_DAYNAME, 0);
        assert_eq!(TRINO_TIMEDATE_FUNCTIONS & SQL_FN_TD_MONTHNAME, 0);
    }

    /// Unchanged by this pass, but pinned so the reconstruction is checked:
    /// the old comment named COUNT_DISTINCT and EVERY, which are not ODBC
    /// flags, so the value was only right by coincidence.
    ///
    /// Both the named-OR assertion and the raw-hex assertion are deliberate,
    /// and the pairing is why this test is worth having. They catch different
    /// mistakes: the OR checks *which* flags were selected, the hex checks
    /// that the flag constants themselves carry the values `sqlext.h` gives
    /// them. Either alone would pass a transcription error in the other. This
    /// is the one place the plan's "no spec literals in tests" rule is
    /// deliberately set aside, because the literal *is* the independent
    /// check.
    #[test]
    fn aggregate_and_value_expression_bitmaps_are_unchanged() {
        assert_eq!(
            TRINO_AGGREGATE_FUNCTIONS,
            SQL_AF_AVG
                | SQL_AF_COUNT
                | SQL_AF_MAX
                | SQL_AF_MIN
                | SQL_AF_SUM
                | SQL_AF_DISTINCT
                | SQL_AF_ALL
        );
        assert_eq!(TRINO_AGGREGATE_FUNCTIONS, 0x7F);
        assert_eq!(
            TRINO_SQL92_VALUE_EXPRESSIONS,
            SQL_SVE_CASE | SQL_SVE_CAST | SQL_SVE_COALESCE | SQL_SVE_NULLIF
        );
        assert_eq!(TRINO_SQL92_VALUE_EXPRESSIONS, 0x0F);
    }

    /// `SQL_KEYWORDS` is Trino's reserved words *minus* the ones ODBC already
    /// reserves, so this asserts the value an application receives rather than
    /// the raw list the hook returns -- the subtraction is core's, and getting
    /// it wrong in either direction is what the info type exists to prevent.
    ///
    /// The expected string is written out rather than recomputed from
    /// `TRINO_RESERVED_KEYWORDS`, which would just restate the implementation.
    #[test]
    fn sql_keywords_excludes_the_words_odbc_already_reserves() {
        use stackable_odbc_core::backend::Backend;
        use stackable_odbc_core::types::ODBC_RESERVED_KEYWORDS;

        let reported: Vec<&str> = TrinoBackend::keywords()
            .iter()
            .filter(|k| {
                !ODBC_RESERVED_KEYWORDS
                    .iter()
                    .any(|r| r.eq_ignore_ascii_case(k))
            })
            .copied()
            .collect();

        assert_eq!(
            reported.join(","),
            "AUTO,CUBE,CURRENT_CATALOG,CURRENT_PATH,CURRENT_ROLE,CURRENT_SCHEMA,\
             GROUPING,JSON_ARRAY,JSON_EXISTS,JSON_OBJECT,JSON_QUERY,JSON_TABLE,\
             JSON_VALUE,LISTAGG,LOCALTIME,LOCALTIMESTAMP,NORMALIZE,RECURSIVE,\
             ROLLUP,SKIP,UESCAPE,UNNEST"
        );

        // The words Trino shares with ODBC must not be reported: an
        // application already knows SELECT is reserved.
        for shared in ["SELECT", "FROM", "WHERE", "JOIN", "CREATE"] {
            assert!(
                TRINO_RESERVED_KEYWORDS.contains(&shared),
                "{shared} should be in the raw list"
            );
            assert!(
                !reported.contains(&shared),
                "{shared} is an ODBC reserved word and must be subtracted out"
            );
        }
    }

    /// A duplicate would be reported twice, and a lowercase entry would slip
    /// past a case-sensitive reader of the value even though core's own
    /// subtraction is case-insensitive.
    #[test]
    fn trino_reserved_keywords_are_unique_sorted_and_upper_case() {
        let mut sorted = TRINO_RESERVED_KEYWORDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            TRINO_RESERVED_KEYWORDS,
            &sorted[..],
            "the list should stay sorted so it can be diffed against the docs"
        );

        let mut deduped = sorted.clone();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            TRINO_RESERVED_KEYWORDS.len(),
            "duplicate entry"
        );

        for k in TRINO_RESERVED_KEYWORDS {
            assert_eq!(*k, k.to_ascii_uppercase(), "{k} should be upper case");
        }
    }

    #[test]
    fn get_functions_advertises_data_at_execution() {
        let f = get_functions();
        assert!(f.contains(&FunctionId::ParamData), "SQLParamData missing");
        assert!(f.contains(&FunctionId::PutData), "SQLPutData missing");
    }

    /// `SQLGetFunctions` is what the Windows Driver Manager builds its dispatch
    /// table from, so claiming a function core does not export hands it a null
    /// pointer to call.
    #[test]
    fn no_advertised_function_is_one_core_does_not_export() {
        use stackable_odbc_core::function_id::CORE_EXPORTED_FUNCTIONS;

        for id in get_functions() {
            assert!(
                CORE_EXPORTED_FUNCTIONS.contains(id),
                "{id:?} is advertised but core generates no entry point for it"
            );
        }
    }

    /// The two lists together are this driver's answer to "do you support this
    /// function?" for every entry point core exports. A function in neither has
    /// no answer, and that is exactly what a newly exported core function looks
    /// like -- so this failing is the point at which someone decides whether
    /// the driver implements it, rather than it going unadvertised unnoticed.
    ///
    /// Mirrors core's own `every_function_id_is_declared_exported_or_not`, one
    /// level up.
    #[test]
    fn every_core_exported_function_is_advertised_or_withheld() {
        use stackable_odbc_core::function_id::CORE_EXPORTED_FUNCTIONS;

        for id in CORE_EXPORTED_FUNCTIONS {
            let advertised = TRINO_ADVERTISED_FUNCTIONS.contains(id);
            let withheld = TRINO_WITHHELD_FUNCTIONS.iter().any(|(w, _)| w == id);
            assert!(
                advertised ^ withheld,
                "{id:?} must appear in exactly one of TRINO_ADVERTISED_FUNCTIONS \
                 (advertised={advertised}) and TRINO_WITHHELD_FUNCTIONS \
                 (withheld={withheld})"
            );
        }

        // Catches an entry in either list that core does not export at all,
        // which the loop above cannot see because it iterates core's list.
        assert_eq!(
            TRINO_ADVERTISED_FUNCTIONS.len() + TRINO_WITHHELD_FUNCTIONS.len(),
            CORE_EXPORTED_FUNCTIONS.len(),
            "the two lists must partition CORE_EXPORTED_FUNCTIONS exactly"
        );
    }

    /// Every withheld entry carries a reason, because the reason is the whole
    /// value of recording the decision.
    #[test]
    fn every_withheld_function_says_why() {
        for (id, reason) in TRINO_WITHHELD_FUNCTIONS {
            assert!(
                !reason.trim().is_empty(),
                "{id:?} is withheld with no reason"
            );
        }
    }

    #[test]
    fn get_functions_has_no_duplicates() {
        let f = get_functions();
        let ids: Vec<u16> = f.iter().map(|id| *id as u16).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            ids.len(),
            "duplicate FunctionId in get_functions"
        );
    }
}
