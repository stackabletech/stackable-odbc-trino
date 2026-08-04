//! Conversion of Trino's JSON-encoded row values into `stackable-odbc-core`'s
//! [`ColumnValue`], and of bound parameter values back into the JSON literals
//! Trino's REST protocol expects. Temporal types are handled via `chrono`.

use chrono::Datelike as _;
use chrono::Timelike as _;
use serde_json::Value;
use stackable_odbc_core::types::{
    ColumnValue, Interval, NANOS_PER_DAY, NANOS_PER_HOUR, NANOS_PER_MINUTE, NANOS_PER_SECOND,
    PRECISION_UNDETERMINABLE, SqlDataType, column_size,
};
use trino_rust_client::{TrinoFloat, TrinoInt, TrinoTy};

/// This driver's declared maximum fractional-seconds precision for TIME,
/// TIME WITH TIME ZONE, TIMESTAMP, and TIMESTAMP WITH TIME ZONE. The single
/// source of truth for every `SQLGetTypeInfo` COLUMN_SIZE/MAXIMUM_SCALE value
/// for these four types (see `backend/info.rs`). Live-verified against Trino
/// 467: `CAST(current_time AS time(13))` errors ("Unknown type: time(13)"),
/// while `time(12)`/`timestamp(12)` (with or without time zone) succeed.
pub(crate) const MAX_FRACTIONAL_SECONDS_PRECISION: i16 = 12;

/// Fractional-seconds scale assumed for TIME, TIME WITH TIME ZONE, TIMESTAMP
/// and TIMESTAMP WITH TIME ZONE when only a `TrinoTy` value is available, with
/// no `information_schema` type-name string to read a declared scale from.
/// Those four `TrinoTy` variants carry no precision parameter (a
/// `trino-rust-client` limitation; contrast `TrinoTy::Decimal(p, s)`, which
/// does), so there is no per-column scale to read here.
///
/// 3 (milliseconds), not 0: that is Trino's default declared precision for a
/// column created without an explicit one, where the ANSI SQL default is 0.
/// `time_with_fraction_keeps_milliseconds_via_get_data_string` in
/// `ffi_integration_tests.rs` confirms it for TIME, and TIMESTAMP behaves the
/// same way.
///
/// One constant covers both TIME and TIMESTAMP. Splitting it in two would gain
/// nothing, because TIME's fraction survives the text conversions exactly as
/// TIMESTAMP's does.
///
/// A column declaring another scale is reported at 3 on this path. The
/// declared scale reaches the driver only in the type-name string, which
/// `type_name_scale` reads wherever the caller has one.
const DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME: i16 = 3;

/// Trino built-in type names as an enum, eliminating hardcoded strings across
/// `type_conversion.rs` and `backend/info.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrinoTypeName {
    Boolean,
    TinyInt,
    SmallInt,
    Integer,
    BigInt,
    Real,
    Double,
    Decimal,
    Varchar,
    Char,
    Varbinary,
    Date,
    Time,
    TimeWithTimeZone,
    Timestamp,
    TimestampWithTimeZone,
    Json,
    Uuid,
    IntervalDayToSecond,
    IntervalYearToMonth,
}

impl TrinoTypeName {
    /// Uppercase display name for ODBC type-info tables.
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::Boolean => "BOOLEAN",
            Self::TinyInt => "TINYINT",
            Self::SmallInt => "SMALLINT",
            Self::Integer => "INTEGER",
            Self::BigInt => "BIGINT",
            Self::Real => "REAL",
            Self::Double => "DOUBLE",
            Self::Decimal => "DECIMAL",
            Self::Varchar => "VARCHAR",
            Self::Char => "CHAR",
            Self::Varbinary => "VARBINARY",
            Self::Date => "DATE",
            Self::Time => "TIME",
            Self::TimeWithTimeZone => "TIME WITH TIME ZONE",
            Self::Timestamp => "TIMESTAMP",
            Self::TimestampWithTimeZone => "TIMESTAMP WITH TIME ZONE",
            Self::Json => "JSON",
            Self::Uuid => "UUID",
            Self::IntervalDayToSecond => "INTERVAL DAY TO SECOND",
            Self::IntervalYearToMonth => "INTERVAL YEAR TO MONTH",
        }
    }

    /// Parse from an `information_schema` type name string.
    ///
    /// Removes any parenthesised precision/scale argument (e.g.
    /// `"varchar(255)"` → `"varchar"`) before matching. Unlike a naive
    /// truncation at the first `(`, this preserves any suffix that follows
    /// the argument: Trino serialises time-zone-aware types with the
    /// argument in the middle, e.g. `"timestamp(3) with time zone"`, so
    /// truncating at `(` would destroy the ` with time zone` suffix and
    /// silently match the wrong (non-TZ) variant instead of failing to
    /// parse. Returns `None` for unknown or compound types.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        let base = strip_precision_param(name);
        match base.as_str() {
            "boolean" => Some(Self::Boolean),
            "tinyint" => Some(Self::TinyInt),
            "smallint" => Some(Self::SmallInt),
            "integer" | "int" => Some(Self::Integer),
            "bigint" => Some(Self::BigInt),
            "real" => Some(Self::Real),
            "double" | "double precision" => Some(Self::Double),
            "decimal" | "numeric" => Some(Self::Decimal),
            "varchar" | "character varying" | "string" => Some(Self::Varchar),
            "char" => Some(Self::Char),
            "varbinary" => Some(Self::Varbinary),
            "date" => Some(Self::Date),
            "time" => Some(Self::Time),
            "time with time zone" => Some(Self::TimeWithTimeZone),
            "timestamp" => Some(Self::Timestamp),
            "timestamp with time zone" => Some(Self::TimestampWithTimeZone),
            "json" => Some(Self::Json),
            "uuid" => Some(Self::Uuid),
            "interval day to second" => Some(Self::IntervalDayToSecond),
            "interval year to month" => Some(Self::IntervalYearToMonth),
            _ => None,
        }
    }

    /// The ODBC SQL data type for this Trino type, as returned by `SQLColumns`.
    pub(crate) fn sql_type(&self) -> SqlDataType {
        match self {
            Self::Boolean => SqlDataType::EXT_BIT,
            Self::TinyInt => SqlDataType::EXT_TINY_INT,
            Self::SmallInt => SqlDataType::SMALLINT,
            Self::Integer => SqlDataType::INTEGER,
            Self::BigInt => SqlDataType::EXT_BIG_INT,
            Self::Real => SqlDataType::REAL,
            Self::Double => SqlDataType::DOUBLE,
            Self::Decimal => SqlDataType::DECIMAL,
            Self::Varchar => SqlDataType::EXT_W_VARCHAR,
            // `information_schema` spells a fixed-length column `char(n)`, and
            // `SQLColumns` reports it as EXT_W_CHAR.
            //
            // So does the query path: `backend::execute` prefers
            // `TrinoTypeName::parse` over `trino_ty_to_sql_type` precisely so
            // the two agree, and `char(n)` parses. `trino_ty_to_sql_type`'s own
            // `TrinoTy::Char(_) => EXT_W_VARCHAR` arm is the fallback for a
            // signature this parser cannot read, not the ordinary result-column
            // route.
            Self::Char => SqlDataType::EXT_W_CHAR,
            Self::Varbinary => SqlDataType::EXT_LONG_VAR_BINARY,
            Self::Date => SqlDataType::DATE,
            Self::Time | Self::TimeWithTimeZone => SqlDataType::TIME,
            Self::Timestamp | Self::TimestampWithTimeZone => SqlDataType::TIMESTAMP,
            Self::Json | Self::Uuid => SqlDataType::EXT_W_VARCHAR,
            // Same rationale as Json/Uuid: `odbc-sys` has no concrete
            // SQL_INTERVAL_* `SqlDataType` (only the legacy verbose
            // `EXT_TIME_OR_INTERVAL` code, which this driver does not use),
            // and `trino_ty_to_sql_type` already renders both interval
            // types as text for the same reason (see its own INTERVAL arm),
            // so EXT_W_VARCHAR is the honest type here too.
            Self::IntervalDayToSecond | Self::IntervalYearToMonth => SqlDataType::EXT_W_VARCHAR,
        }
    }

    /// Fixed column precision for non-parametric types.
    ///
    /// Returns `None` for types whose precision is encoded in the type string
    /// (`Varchar(n)`, `Char(n)`, `Decimal(p,s)`).
    pub(crate) fn fixed_precision(&self) -> Option<i32> {
        match self {
            Self::TinyInt => Some(column_size(SqlDataType::EXT_TINY_INT, 0, 0)),
            Self::SmallInt => Some(column_size(SqlDataType::SMALLINT, 0, 0)),
            Self::Integer => Some(column_size(SqlDataType::INTEGER, 0, 0)),
            Self::BigInt => Some(column_size(SqlDataType::EXT_BIG_INT, 0, 0)),
            Self::Real => Some(column_size(SqlDataType::REAL, 0, 0)),
            Self::Double => Some(column_size(SqlDataType::DOUBLE, 0, 0)),
            Self::Boolean => Some(column_size(SqlDataType::EXT_BIT, 0, 0)),
            Self::Date => Some(column_size(SqlDataType::DATE, 0, 0)),
            // HH:MM:SS.mmm = 12 chars (9 + scale 3, see
            // DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME, Trino's actual
            // default declared precision). TimeWithTimeZone is also 12, not
            // HH:MM:SS.mmm+HH:MM (18): `parse_trino_time_with_tz` applies the
            // offset and normalises to UTC (matching TIMESTAMP WITH TIME
            // ZONE), so the offset never survives into SQL_TIME_STRUCT;
            // only the fractional seconds do (preserved via `ColumnValue::
            // Time`'s `fraction` field, delivered through SQL_C_CHAR/WCHAR
            // text conversions). Keep the two as distinct arms, matching
            // `trino_ty_precision`: merging them makes one variant borrow the
            // other's size, which the query path (`execute.rs`) then reports
            // for a column that does not have it.
            Self::Time => Some(column_size(
                SqlDataType::TIME,
                0,
                DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME,
            )),
            Self::TimeWithTimeZone => Some(column_size(
                SqlDataType::TIME,
                0,
                DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME,
            )),
            // YYYY-MM-DD HH:MM:SS.mmm = 23 chars (20 + scale 3, see
            // DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME). TimestampWithTimeZone
            // is also 23, not YYYY-MM-DD HH:MM:SS.mmm+HH:MM (29):
            // `parse_trino_timestamp_tz` applies the offset and normalises to
            // UTC, so it doesn't survive into SQL_TIMESTAMP_STRUCT (which has
            // no zone field either); only YYYY-MM-DD HH:MM:SS.mmm is
            // delivered. Kept as a distinct match arm (not merged with
            // `Timestamp`) to match `trino_ty_precision`, for the same reason
            // as `Time`/`TimeWithTimeZone` above.
            Self::Timestamp => Some(column_size(
                SqlDataType::TIMESTAMP,
                0,
                DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME,
            )),
            Self::TimestampWithTimeZone => Some(column_size(
                SqlDataType::TIMESTAMP,
                0,
                DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME,
            )),
            Self::Varchar
            | Self::Char
            | Self::Decimal
            | Self::Varbinary
            | Self::Json
            | Self::Uuid
            | Self::IntervalDayToSecond
            | Self::IntervalYearToMonth => None,
        }
    }

    /// Whether the type string carries a precision: `varchar(n)`, `char(n)`
    /// or `decimal(p,s)`.
    pub(crate) fn has_precision_param(&self) -> bool {
        matches!(self, Self::Varchar | Self::Char | Self::Decimal)
    }

    /// Whether the type string carries a scale, which only `decimal(p,s)`
    /// does.
    pub(crate) fn has_scale_param(&self) -> bool {
        matches!(self, Self::Decimal)
    }

    /// Whether this type's single parenthesised type-string argument is a
    /// fractional-seconds *scale*, not a precision.
    ///
    /// `true` for `Time`, `TimeWithTimeZone`, `Timestamp` and
    /// `TimestampWithTimeZone`, and a separate question from
    /// [`Self::has_precision_param`] and [`Self::has_scale_param`].
    /// `decimal(p,s)`, `varchar(n)` and `char(n)` carry a precision as their
    /// first argument, while Trino's temporal types spell a scale as their
    /// only one: `timestamp(6)` declares 6 fractional-second digits, not a
    /// precision of 6.
    ///
    /// Reading that argument as `COLUMN_SIZE`/`SQL_DESC_LENGTH` reports
    /// `timestamp(6)` at `6` instead of `26`, which is `20 + s` per the ODBC
    /// "Column Size" appendix. `type_name_precision` and `type_name_scale`
    /// below are where the distinction is applied.
    pub(crate) fn has_temporal_scale_param(&self) -> bool {
        matches!(
            self,
            Self::Time | Self::TimeWithTimeZone | Self::Timestamp | Self::TimestampWithTimeZone
        )
    }

    /// Every variant of this enum, for a completeness test
    /// (`every_reportable_type_has_a_type_info_row` in `backend/info.rs`)
    /// that must cover every native type name this driver explicitly
    /// recognises, rather than a hand-copied list of declared type strings
    /// that can silently omit a variant added later.
    ///
    /// `assert_all_variants_listed` below is an exhaustive `match` with no
    /// wildcard arm, so adding a variant to `TrinoTypeName` without also
    /// adding it here fails to compile.
    #[cfg(test)]
    pub(crate) const ALL_VARIANTS: &'static [TrinoTypeName] = &[
        Self::Boolean,
        Self::TinyInt,
        Self::SmallInt,
        Self::Integer,
        Self::BigInt,
        Self::Real,
        Self::Double,
        Self::Decimal,
        Self::Varchar,
        Self::Char,
        Self::Varbinary,
        Self::Date,
        Self::Time,
        Self::TimeWithTimeZone,
        Self::Timestamp,
        Self::TimestampWithTimeZone,
        Self::Json,
        Self::Uuid,
        Self::IntervalDayToSecond,
        Self::IntervalYearToMonth,
    ];

    /// Compile-time proof that [`Self::ALL_VARIANTS`] is exhaustive: this
    /// match has no wildcard arm, so it fails to compile the moment a new
    /// variant is added to `TrinoTypeName` without being listed here (and,
    /// per the doc comment above, in `ALL_VARIANTS`).
    #[cfg(test)]
    pub(crate) const fn assert_all_variants_listed(v: &Self) {
        match v {
            Self::Boolean
            | Self::TinyInt
            | Self::SmallInt
            | Self::Integer
            | Self::BigInt
            | Self::Real
            | Self::Double
            | Self::Decimal
            | Self::Varchar
            | Self::Char
            | Self::Varbinary
            | Self::Date
            | Self::Time
            | Self::TimeWithTimeZone
            | Self::Timestamp
            | Self::TimestampWithTimeZone
            | Self::Json
            | Self::Uuid
            | Self::IntervalDayToSecond
            | Self::IntervalYearToMonth => {}
        }
    }
}

/// Map a Trino column type to an ODBC SQL data type.
pub fn trino_ty_to_sql_type(column_type: &TrinoTy) -> SqlDataType {
    match column_type {
        TrinoTy::TrinoInt(TrinoInt::I64) => SqlDataType::EXT_BIG_INT,
        TrinoTy::TrinoInt(TrinoInt::I32) => SqlDataType::INTEGER,
        TrinoTy::TrinoInt(TrinoInt::I16) => SqlDataType::SMALLINT,
        TrinoTy::TrinoInt(TrinoInt::I8) => SqlDataType::EXT_TINY_INT,
        TrinoTy::TrinoFloat(TrinoFloat::F64) => SqlDataType::DOUBLE,
        TrinoTy::TrinoFloat(TrinoFloat::F32) => SqlDataType::REAL,
        TrinoTy::Boolean => SqlDataType::EXT_BIT,
        // `Char(_)` lands on EXT_W_VARCHAR rather than EXT_W_CHAR, and only
        // reaches an application when `TrinoTypeName::parse` could not read the
        // column's own signature: every caller tries that first, and it answers
        // EXT_W_CHAR for a `char(n)`. Widening is the safe direction for a type
        // this driver could not identify.
        TrinoTy::Varchar | TrinoTy::Char(_) => SqlDataType::EXT_W_VARCHAR,
        TrinoTy::Date => SqlDataType::DATE,
        TrinoTy::Time | TrinoTy::TimeWithTimeZone => SqlDataType::TIME,
        TrinoTy::Timestamp | TrinoTy::TimestampWithTimeZone => SqlDataType::TIMESTAMP,
        TrinoTy::Decimal(_, _) => SqlDataType::DECIMAL,
        // String-representable types without a dedicated ODBC type
        TrinoTy::Uuid | TrinoTy::Json | TrinoTy::IpAddress => SqlDataType::EXT_W_VARCHAR,
        TrinoTy::IntervalYearToMonth | TrinoTy::IntervalDayToSecond => SqlDataType::EXT_W_VARCHAR,
        // VARBINARY: Trino sends base64 text over the REST API; `json_to_column_value`
        // decodes it to ColumnValue::Bytes. SQL_LONGVARBINARY (-4) is chosen to match
        // what `SQLGetTypeInfo` (backend/info.rs) and the catalog path
        // (`TrinoTypeName::Varbinary`) already report, so all three agree.
        TrinoTy::VarBinary => SqlDataType::EXT_LONG_VAR_BINARY,
        // Compound types: ODBC 3.x has no SQL type for them (SQL_ARRAY / SQL_ROW
        // exist only in ODBC 4.0, which no Driver Manager implements). The values
        // keep their structure as ColumnValue::Array / Map / Row and are rendered
        // to text by `column_value_to_string` at write time, using Trino's own
        // display form (`[1, 2]`, `{k=v}`, `(a, b)`) rather than JSON.
        TrinoTy::Array(_) | TrinoTy::Map(_, _) | TrinoTy::Row(_) | TrinoTy::Tuple(_) => {
            tracing::warn!(column_type = ?column_type, "compound Trino type mapped to SQL_WVARCHAR (rendered as text at write time)");
            SqlDataType::EXT_W_VARCHAR
        }
        TrinoTy::Option(inner) => trino_ty_to_sql_type(inner),
        TrinoTy::Unknown => {
            tracing::warn!("unknown Trino type mapped to SQL_WVARCHAR");
            SqlDataType::EXT_W_VARCHAR
        }
    }
}

/// Narrow a [`column_size`] or [`TrinoTypeName::fixed_precision`] result to
/// the `u32` [`trino_ty_precision`] returns.
///
/// No SQL type routed through here produces a negative or overflowing value,
/// so the fallback exists to keep the function panic-free rather than to rely
/// on that invariant silently.
fn precision_as_u32(n: i32) -> u32 {
    u32::try_from(n).unwrap_or_else(|_| {
        tracing::warn!(
            value = n,
            "column size formula produced a value outside u32 range"
        );
        0
    })
}

/// Map a Trino column type to a display precision (number of digits/chars).
pub fn trino_ty_precision(ty: &TrinoTy) -> u32 {
    match ty {
        TrinoTy::TrinoInt(TrinoInt::I8) => {
            precision_as_u32(column_size(SqlDataType::EXT_TINY_INT, 0, 0))
        }
        TrinoTy::TrinoInt(TrinoInt::I16) => {
            precision_as_u32(column_size(SqlDataType::SMALLINT, 0, 0))
        }
        TrinoTy::TrinoInt(TrinoInt::I32) => {
            precision_as_u32(column_size(SqlDataType::INTEGER, 0, 0))
        }
        TrinoTy::TrinoInt(TrinoInt::I64) => {
            precision_as_u32(column_size(SqlDataType::EXT_BIG_INT, 0, 0))
        }
        TrinoTy::TrinoFloat(TrinoFloat::F32) => {
            precision_as_u32(column_size(SqlDataType::REAL, 0, 0))
        }
        TrinoTy::TrinoFloat(TrinoFloat::F64) => {
            precision_as_u32(column_size(SqlDataType::DOUBLE, 0, 0))
        }
        // Char(n) is a character type: column_size passes precision straight
        // through, so this is `n` itself, routed through the shared formula
        // rather than cast directly for consistency with every other arm here.
        TrinoTy::Char(n) => precision_as_u32(column_size(
            SqlDataType::EXT_W_CHAR,
            i32::try_from(*n).unwrap_or(i32::MAX),
            0,
        )),
        TrinoTy::Boolean => precision_as_u32(column_size(SqlDataType::EXT_BIT, 0, 0)),
        TrinoTy::Date => precision_as_u32(column_size(SqlDataType::DATE, 0, 0)),
        // HH:MM:SS.mmm (scale 3, see DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME).
        TrinoTy::Time => precision_as_u32(column_size(
            SqlDataType::TIME,
            0,
            DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME,
        )),
        // Also HH:MM:SS.mmm, not HH:MM:SS.mmm+HH:MM: the offset is applied
        // and the value normalised to UTC (see `parse_trino_time_with_tz`),
        // so only the fractional seconds reach the application, through the
        // text conversions. SQL_TIME_STRUCT has no fraction field of its own.
        TrinoTy::TimeWithTimeZone => precision_as_u32(column_size(
            SqlDataType::TIME,
            0,
            DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME,
        )),
        // YYYY-MM-DD HH:MM:SS.mmm (scale 3, see
        // DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME).
        TrinoTy::Timestamp => precision_as_u32(column_size(
            SqlDataType::TIMESTAMP,
            0,
            DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME,
        )),
        // Also YYYY-MM-DD HH:MM:SS.mmm, not ...+HH:MM: the offset is applied
        // and the value normalised to UTC (see `parse_trino_timestamp_tz`),
        // so only YYYY-MM-DD HH:MM:SS.mmm reaches SQL_TIMESTAMP_STRUCT.
        TrinoTy::TimestampWithTimeZone => precision_as_u32(column_size(
            SqlDataType::TIMESTAMP,
            0,
            DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME,
        )),
        TrinoTy::Decimal(p, _) => precision_as_u32(column_size(
            SqlDataType::DECIMAL,
            i32::try_from(*p).unwrap_or(i32::MAX),
            0,
        )),
        TrinoTy::Option(inner) => trino_ty_precision(inner),
        // Every other type (Varchar, VarBinary, Uuid, Json, the two
        // intervals, Array, Map, Row, Tuple, Unknown) is rendered as
        // unbounded text by this driver, with no declared length to read.
        // `backend/execute.rs` falls back to this function only for a column
        // with no `information_schema` type-name string, such as a computed
        // `SELECT CAST(x AS VARCHAR)` with no catalog entry.
        //
        // The ODBC "Column Size" and "Display Size" appendices cover exactly
        // this in a footnote: "If the driver cannot determine the column or
        // parameter length for a variable type, it returns SQL_NO_TOTAL."
        // `PRECISION_UNDETERMINABLE` is core's sentinel for it, which the
        // numeric `SQLColAttributeW` and `SQLDescribeCol` outputs recognise
        // and substitute `SQL_NO_TOTAL` for (see `resolve_precision_isize`
        // and `resolve_precision_ulen`).
        //
        // The two wrong answers here are 0 and `i32::MAX`. 0 under-reports a
        // column that can hold arbitrarily long text and truncates
        // `SQLGetData(SQL_C_WCHAR)` reads, which
        // `metadata_sized_wchar_round_trip_covers_representative_types` in
        // `ffi_integration_tests.rs` pins. 2147483647 is not an allocatable
        // buffer size, and it carries a different meaning already:
        // `SQLGetTypeInfo`'s VARCHAR, VARBINARY, JSON and INTERVAL rows
        // (`backend/info.rs`) report that literal number for "unbounded but
        // known", which must not be reinterpreted as "undeterminable".
        _ => PRECISION_UNDETERMINABLE,
    }
}

/// Return the decimal scale for a Trino column type (0 for non-decimal types).
pub fn trino_ty_scale(ty: &TrinoTy) -> i16 {
    match ty {
        TrinoTy::Decimal(_, s) => *s as i16,
        TrinoTy::Option(inner) => trino_ty_scale(inner),
        _ => 0,
    }
}

/// Remove a parenthesised precision/scale argument from the middle of a type
/// string, preserving whatever comes before and after it, and lowercasing
/// the result for case-insensitive matching.
///
/// `"timestamp(3) with time zone"` → `"timestamp with time zone"`
/// `"varchar(255)"` → `"varchar"`
/// `"timestamp with time zone"` (no argument) → `"timestamp with time zone"`
///
/// Not a truncation at the first `(`: Trino's time-zone-aware types carry the
/// argument *before* a suffix (`" with time zone"`), so truncating there would
/// discard the suffix and let the caller misidentify the type.
fn strip_precision_param(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    let Some(start) = lower.find('(') else {
        return lower;
    };
    let Some(end) = lower[start..].find(')') else {
        return lower;
    };
    let end = start + end;

    let prefix = lower[..start].trim_end();
    let suffix = lower[end + 1..].trim();

    if suffix.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix} {suffix}")
    }
}

/// Extract the first numeric parameter from a type string like `"varchar(100)"` or `"decimal(10,2)"`.
fn parse_precision_param(name: &str) -> Option<i32> {
    let start = name.find('(')?;
    let end = name.rfind(')')?;
    name[start + 1..end].split(',').next()?.trim().parse().ok()
}

/// Extract the second numeric parameter from a type string like `"decimal(10,2)"`.
fn parse_scale_param(name: &str) -> Option<i32> {
    let start = name.find('(')?;
    let end = name.rfind(')')?;
    let mut parts = name[start + 1..end].split(',');
    parts.next()?;
    parts.next()?.trim().parse().ok()
}

/// Parse the fractional-seconds *scale* from a temporal type-name string's
/// sole parenthesised argument (`"timestamp(6)"` -> 6,
/// `"time(6) with time zone"` -> 6), defaulting to
/// [`DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME`] when there is no argument at
/// all (a bare `"timestamp"`) or it does not fit `i16`.
///
/// It reuses [`parse_precision_param`]'s parenthesis extraction, since the
/// argument sits in the same textual position, and keeps its own name because
/// the quantity is different: for `Time`, `TimeWithTimeZone`, `Timestamp` and
/// `TimestampWithTimeZone` that argument is a *scale*, never a precision. See
/// [`TrinoTypeName::has_temporal_scale_param`] for what conflating the two
/// reports.
fn temporal_scale_param(name: &str) -> i16 {
    parse_precision_param(name)
        .and_then(|s| i16::try_from(s).ok())
        .unwrap_or(DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME)
}

/// Extract ODBC column precision (`COLUMN_SIZE`/`SQL_DESC_LENGTH`) from a
/// Trino type name string (as returned by `information_schema.columns.data_type`).
///
/// For parametric types (`varchar(n)`, `char(n)`, `decimal(p,s)`), the value is
/// read from the type string. For fixed-size types, the canonical precision is
/// returned directly. Returns `None` for types with no meaningful precision.
///
/// For `Time`/`TimeWithTimeZone`/`Timestamp`/`TimestampWithTimeZone`, the
/// declared fractional-seconds scale is parsed from the type string (falling
/// back to [`DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME`] only when no
/// parenthesised argument is present) and fed through [`column_size`], the
/// ODBC "Column Size" appendix's character-length formula (`9 + s` for TIME,
/// `20 + s` for TIMESTAMP), rather than returned as-is. The parenthesised
/// argument for these types *is* the scale, not a precision (see
/// [`TrinoTypeName::has_temporal_scale_param`]); do not return it directly
/// here, as that collapses `SQL_DESC_LENGTH`/`COLUMN_SIZE`
/// for `timestamp(6)` to `6` instead of the correct `26`.
pub fn type_name_precision(name: &str) -> Option<i32> {
    let ty = TrinoTypeName::parse(name)?;
    if ty.has_precision_param() {
        parse_precision_param(name)
    } else if ty.has_temporal_scale_param() {
        let scale = temporal_scale_param(name);
        Some(column_size(ty.sql_type(), 0, scale))
    } else {
        ty.fixed_precision()
    }
}

/// Extract ODBC decimal digits (`SQL_DESC_PRECISION` for datetime types,
/// `SQL_DESC_SCALE` for `DECIMAL`/`NUMERIC`) from a Trino type name string.
///
/// Returns `Some(scale)` for `decimal`/`numeric` types with an explicit scale
/// parameter, and for `Time`/`TimeWithTimeZone`/`Timestamp`/
/// `TimestampWithTimeZone` the declared fractional-seconds scale parsed from
/// the type string (see [`type_name_precision`]'s doc comment for why this is
/// a distinct quantity from that function's `COLUMN_SIZE`/`SQL_DESC_LENGTH`
/// result, even though both are ultimately derived from the same
/// parenthesised argument). `None` for every other type.
pub fn type_name_scale(name: &str) -> Option<i32> {
    let ty = TrinoTypeName::parse(name)?;
    if ty.has_scale_param() {
        parse_scale_param(name)
    } else if ty.has_temporal_scale_param() {
        Some(i32::from(temporal_scale_param(name)))
    } else {
        None
    }
}

/// Map a Trino type name string (from `information_schema.columns.data_type`)
/// to an ODBC `SqlDataType`. Strips parametric suffixes like `varchar(100)` or
/// `timestamp(3)` before matching. Unknown types fall back to `EXT_W_VARCHAR`.
pub fn trino_type_name_to_sql_type(name: &str) -> SqlDataType {
    TrinoTypeName::parse(name)
        .map(|ty| ty.sql_type())
        .unwrap_or(SqlDataType::EXT_W_VARCHAR)
}

/// Parse a Trino date string `"YYYY-MM-DD"` into a [`ColumnValue::Date`].
///
/// Returns `None` if the string is malformed.
fn parse_trino_date(s: &str) -> Option<ColumnValue> {
    let (year, month, day) = parse_ymd(s)?;
    Some(ColumnValue::Date { year, month, day })
}

/// Split a Trino `YYYY-MM-DD` date into its three fields.
///
/// Shared by [`parse_trino_date`] and [`parse_trino_timestamp`], which have to
/// agree: both read back values this driver itself emitted, through the same
/// `year4` renderer in `backend::params`, so a year one accepts and the other
/// rejects is a round trip that works for `DATE` and silently degrades a
/// `TIMESTAMP` to text.
///
/// Trino renders a year before 1 CE with a leading `-`, which is the same
/// character that separates the fields, so the sign is taken off before the
/// split rather than left for `splitn` to read as an empty year.
///
/// A year that does not fit `SQL_DATE_STRUCT`'s signed 16-bit field (Trino goes
/// to 5881580, and renders those with a leading `+`) returns `None`, and the
/// caller keeps the value as text: truncating it would report a different year
/// as though it were the real one.
fn parse_ymd(s: &str) -> Option<(i16, u16, u16)> {
    let (negative, rest) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let mut parts = rest.splitn(3, '-');
    let magnitude: i16 = parts.next()?.parse().ok()?;
    // `checked_neg`, because `i16::MIN` has no positive counterpart and a bare
    // `-` on it would wrap back to itself.
    let year = if negative {
        magnitude.checked_neg()?
    } else {
        magnitude
    };
    let month: u16 = parts.next()?.parse().ok()?;
    let day: u16 = parts.next()?.parse().ok()?;
    Some((year, month, day))
}

/// Convert Trino's decimal fractional-seconds text (up to 12 digits, i.e.
/// picoseconds) into nanoseconds, the unit `ColumnValue::Time`/`Timestamp`
/// both use. Left-aligned zero-padding/truncation to 9 digits. The sole
/// helper for this: `parse_trino_time`/`parse_trino_time_with_tz`/
/// `parse_trino_timestamp` all call it rather than each repeating the same
/// padding logic.
fn parse_fraction_nanos(frac: &str) -> u32 {
    let padded = format!("{frac:0<9}");
    // `padded.get(..9)` rather than `padded[..9]`: `frac` is expected to be
    // ASCII digits from Trino's own wire format, but if malformed input ever
    // contains a multi-byte character, byte index 9 may not land on a UTF-8
    // char boundary, and indexing (unlike `get`) panics rather than
    // returning `None`: the workspace denies `panic` outside tests, so
    // this must not be able to. Falling back to 0 matches what an
    // unparseable numeric fragment already does below.
    padded.get(..9).and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// Parse a Trino time string `"HH:MM:SS[.fraction][ TZ]"` into a [`ColumnValue::Time`].
///
/// The timezone suffix is discarded (a bare `TIME` has no offset semantics
/// beyond what `parse_trino_time_with_tz` applies). The fractional-seconds
/// part is converted to nanoseconds and kept: `SQL_TIME_STRUCT` cannot carry
/// it, but the string rendering used for `SQL_C_CHAR`/`SQL_C_WCHAR` targets
/// can, so dropping it here would lose it before the driver even knows which
/// C type the caller wants.
/// Returns `None` if the string is malformed.
fn parse_trino_time(s: &str) -> Option<ColumnValue> {
    // Strip optional timezone suffix: "13:14:15.123 UTC" → "13:14:15.123"
    let s = s.split_whitespace().next()?;
    let mut parts = s.splitn(3, ':');
    let hour: u16 = parts.next()?.parse().ok()?;
    let minute: u16 = parts.next()?.parse().ok()?;
    let sec_part = parts.next()?;
    let mut sf = sec_part.splitn(2, '.');
    let second: u16 = sf.next()?.parse().ok()?;
    let fraction = sf.next().map(parse_fraction_nanos).unwrap_or(0);
    Some(ColumnValue::Time {
        hour,
        minute,
        second,
        fraction,
    })
}

/// Parse `TIME WITH TIME ZONE` and normalise to UTC.
///
/// `SQL_TIME_STRUCT` has no timezone field, so the offset cannot be carried.
/// Applying it and returning UTC matches what `TIMESTAMP WITH TIME ZONE`
/// already does; do not discard the offset silently, or the two "with time
/// zone" types behave inconsistently.
///
/// Trino renders the zone two ways: a space-separated name (`"13:14:15.000
/// UTC"`) or a glued numeric offset (`"13:14:15+02:00"`, `"13:14:15-05:30"`).
/// Both are handled. A named zone other than UTC is date-dependent (DST) and
/// a bare TIME has no date to resolve it against, so it is treated as UTC
/// (offset 0) with a `tracing::warn!` rather than silently guessing; this
/// only affects the rare case of a non-UTC named zone, since Trino's own
/// numeric-offset rendering is the common form.
/// Returns `None` if the string is malformed.
fn parse_trino_time_with_tz(s: &str) -> Option<ColumnValue> {
    let t = s.trim();

    // A space-separated named zone, e.g. "13:14:15.000 UTC".
    if let Some((time_part, zone)) = t.rsplit_once(' ') {
        let offset_minutes = if zone.eq_ignore_ascii_case("UTC") {
            0
        } else {
            // A named zone's offset is date-dependent and a TIME has no date.
            // Treat it as UTC and say so rather than silently guessing.
            tracing::warn!(
                zone = %zone,
                "TIME WITH TIME ZONE carries a named zone; offset cannot be \
                 resolved without a date, treating as UTC"
            );
            0
        };
        return shift_time(time_part, offset_minutes);
    }

    // A glued numeric offset, e.g. "13:14:15+02:00" or "13:14:15-05:30".
    // The time-of-day portion (HH:MM:SS[.f]) is only ever digits, colons and
    // a dot, never '+' or '-', so the *rightmost* '+'/'-' in
    // the string is unambiguously the offset sign.
    let sign_pos = t.rfind(['+', '-'])?;
    let (time_part, offset_part) = t.split_at(sign_pos);
    let sign = if offset_part.starts_with('-') { -1 } else { 1 };
    let offset_body = &offset_part[1..];
    let mut op = offset_body.splitn(2, ':');
    let oh: i32 = op.next()?.parse().ok()?;
    let om: i32 = op.next().unwrap_or("0").parse().ok()?;
    shift_time(time_part, sign * (oh * 60 + om))
}

/// Shift `HH:MM:SS[.f]` by `offset_minutes`, wrapping within the day.
///
/// A `TIME` has no date to carry an overflow into, so the result wraps
/// within `[0, 24h)`. `rem_euclid` (not `%`) is used because a negative
/// `total` (the offset exceeds the time-of-day, e.g. `"01:00:00+02:00"`)
/// must wrap forward to the previous day's minutes-past-midnight, not
/// produce a negative remainder.
///
/// The offset is always a whole number of minutes, so it cannot shift the
/// fractional-seconds part: that is carried through unchanged, the same way
/// `parse_trino_time` keeps it.
fn shift_time(time_part: &str, offset_minutes: i32) -> Option<ColumnValue> {
    let mut parts = time_part.trim().splitn(3, ':');
    let hour: i32 = parts.next()?.parse().ok()?;
    let minute: i32 = parts.next()?.parse().ok()?;
    let sec_part = parts.next()?;
    let mut sf = sec_part.splitn(2, '.');
    let second: i32 = sf.next()?.parse().ok()?;
    let fraction = sf.next().map(parse_fraction_nanos).unwrap_or(0);

    let total = hour * 60 + minute - offset_minutes;
    let wrapped = total.rem_euclid(24 * 60);

    Some(ColumnValue::Time {
        hour: u16::try_from(wrapped / 60).ok()?,
        minute: u16::try_from(wrapped % 60).ok()?,
        second: u16::try_from(second).ok()?,
        fraction,
    })
}

/// Decode Trino's base64 VARBINARY payload into [`ColumnValue::Bytes`].
///
/// Returns `None` if the string is not valid standard-alphabet base64. Callers
/// do not log the raw payload, since binary column contents may be sensitive.
fn base64_decode(s: &str) -> Option<ColumnValue> {
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s)
        .ok()
        .map(ColumnValue::Bytes)
}

/// Parse a Trino timestamp string `"YYYY-MM-DD HH:MM:SS[.fraction][ TZ]"` into a
/// [`ColumnValue::Timestamp`].
///
/// The fractional-seconds part is converted to nanoseconds stored in
/// `fraction` via [`parse_fraction_nanos`]. The optional timezone suffix is
/// discarded (Trino normalises timestamps to UTC before serialising them
/// when the column type includes a time zone).
///
/// Returns `None` if the string is malformed.
fn parse_trino_timestamp(s: &str) -> Option<ColumnValue> {
    // Split on the first space to separate date from time+tz.
    let (date_str, rest) = s.split_once(' ')?;
    // Strip optional timezone: take only the first token.
    let time_str = rest.split_whitespace().next()?;

    // Through `parse_ymd` rather than a local `splitn`, so a year before 1 CE
    // reads here exactly as it does in `parse_trino_date`. `backend::params`
    // renders a bound `SQL_TIMESTAMP_STRUCT` with the same `year4` it uses for
    // a `SQL_DATE_STRUCT`, so a bare split left `-0001-01-01 00:00:00` with an
    // empty first field and degraded the whole value to text.
    let (year, month, day) = parse_ymd(date_str)?;

    let mut tp = time_str.splitn(3, ':');
    let hour: u16 = tp.next()?.parse().ok()?;
    let minute: u16 = tp.next()?.parse().ok()?;
    let sec_frac = tp.next()?;
    let mut sf = sec_frac.splitn(2, '.');
    let second: u16 = sf.next()?.parse().ok()?;
    let fraction = sf.next().map(parse_fraction_nanos).unwrap_or(0);

    Some(ColumnValue::Timestamp {
        year,
        month,
        day,
        hour,
        minute,
        second,
        fraction,
    })
}

/// Parse a Trino INTERVAL YEAR TO MONTH string "Y-M" into years and months.
///
/// The sign prefixes the whole interval, not just the year component: Trino
/// serialises a negative interval as `"-Y-M"`. Parsing the leading `-` once
/// (rather than relying on `i32::parse` to see it on the first token) and
/// applying it to both fields keeps the two in agreement: a split
/// representation must not let one field be negative while the other is
/// positive.
fn parse_interval_year_month(s: &str) -> Option<ColumnValue> {
    let t = s.trim();
    let (negative, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t),
    };

    let mut parts = body.splitn(2, '-');
    let years: i32 = parts.next()?.trim().parse().ok()?;
    let months: i32 = parts.next()?.trim().parse().ok()?;

    let (years, months) = if negative {
        (years.checked_neg()?, months.checked_neg()?)
    } else {
        (years, months)
    };

    Some(ColumnValue::IntervalYearMonth {
        years,
        months,
        // Trino has one year-month interval type and it carries both fields, so
        // the precision is always the two-field form. The narrower
        // `Interval::Year` and `Interval::Month` have no Trino column type to
        // come from.
        precision: Interval::YearToMonth,
    })
}

/// Parse Trino's `INTERVAL DAY TO SECOND` text, e.g. `"-2 03:04:05.678"`.
///
/// The sign prefixes the whole interval, not just the day component.
fn parse_interval_day_time(s: &str) -> Option<ColumnValue> {
    let t = s.trim();
    let (negative, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t),
    };

    let mut outer = body.splitn(2, ' ');
    let days: i64 = outer.next()?.trim().parse().ok()?;
    let time_part = outer.next()?.trim();

    let mut tp = time_part.splitn(3, ':');
    let h: i64 = tp.next()?.parse().ok()?;
    let m: i64 = tp.next()?.parse().ok()?;
    let sec_part = tp.next()?;
    let (sec_text, frac_text) = match sec_part.split_once('.') {
        Some((sec, frac)) => (sec, frac),
        None => (sec_part, ""),
    };
    let sec: i64 = sec_text.parse().ok()?;
    // `ColumnValue::IntervalDayTime` counts nanoseconds, so the fraction is kept
    // whole. Trino renders this type with three fractional digits, its own
    // storage being a millisecond count, but `parse_fraction_nanos` reads
    // whatever arrives on the same "pad right, then take nine" rule the temporal
    // parsers use, so a shorter fragment like "5" is read as 500ms rather than
    // 5ns and a longer one does not have to be special-cased here.
    let frac_nanos = i128::from(parse_fraction_nanos(frac_text));

    let magnitude = i128::from(days)
        .checked_mul(NANOS_PER_DAY)?
        .checked_add(i128::from(h) * NANOS_PER_HOUR)?
        .checked_add(i128::from(m) * NANOS_PER_MINUTE)?
        .checked_add(i128::from(sec) * NANOS_PER_SECOND)?
        .checked_add(frac_nanos)?;

    Some(ColumnValue::IntervalDayTime {
        total_nanoseconds: if negative { -magnitude } else { magnitude },
        // Trino has one day-time interval type and it spans all four fields, so
        // the precision is always the widest form.
        precision: Interval::DayToSecond,
    })
}

/// Parse a Trino TIMESTAMP WITH TIME ZONE string and convert to UTC.
///
/// Trino REST API format: `"YYYY-MM-DD HH:MM:SS.fraction TIMEZONE"` where
/// TIMEZONE is either a numeric offset (`+05:30`, `-08:00`) or a named IANA
/// zone (`UTC`, `America/New_York`, `Europe/Berlin`).
///
/// Returns `ColumnValue::Timestamp` with UTC-converted fields, matching the
/// official Trino ODBC driver's behaviour. The timezone information is consumed
/// during conversion: `SQL_TIMESTAMP_STRUCT` has no timezone field.
fn parse_trino_timestamp_tz(s: &str) -> Option<ColumnValue> {
    let last_space = s.rfind(' ')?;
    let datetime_part = &s[..last_space];
    let tz_part = s[last_space + 1..].trim();

    let base = parse_trino_timestamp(datetime_part)?;
    let (year, month, day, hour, minute, second, fraction) = match base {
        ColumnValue::Timestamp {
            year,
            month,
            day,
            hour,
            minute,
            second,
            fraction,
        } => (year, month, day, hour, minute, second, fraction),
        _ => return None,
    };

    use chrono::{NaiveDate, NaiveDateTime, NaiveTime, TimeZone as _};
    use chrono_tz::Tz;

    let ndt = NaiveDate::from_ymd_opt(year.into(), month.into(), day.into()).and_then(|d| {
        NaiveTime::from_hms_nano_opt(hour.into(), minute.into(), second.into(), fraction)
            .map(|t| NaiveDateTime::new(d, t))
    })?;

    // Numeric offsets (+05:30, -08:00): subtract the offset directly to get
    // UTC. Named zones (UTC, America/New_York, CET): resolve via chrono-tz
    // which handles DST rules: the same named zone maps to different UTC
    // offsets depending on the date.
    let utc_ndt = if let Some(rest) = tz_part.strip_prefix('+') {
        ndt - parse_numeric_offset(1, rest)?
    } else if let Some(rest) = tz_part.strip_prefix('-') {
        ndt - parse_numeric_offset(-1, rest)?
    } else {
        let tz: Tz = tz_part.parse().ok().or_else(|| {
            tracing::warn!(
                "parse_trino_timestamp_tz: unrecognised timezone {:?}",
                tz_part
            );
            None
        })?;
        // earliest() picks the first valid mapping when a local time is
        // ambiguous (DST fall-back). Returns None for gap times (spring-forward).
        tz.from_local_datetime(&ndt)
            .earliest()
            .or_else(|| {
                tracing::warn!(
                    "parse_trino_timestamp_tz: ambiguous or invalid local time {:?} in zone {:?}",
                    ndt,
                    tz_part
                );
                None
            })?
            .with_timezone(&chrono::Utc)
            .naive_utc()
    };

    // Fraction (sub-second nanoseconds) is preserved as-is: UTC conversion
    // only shifts hours/minutes/seconds, never sub-second precision.
    Some(ColumnValue::Timestamp {
        year: i16::try_from(utc_ndt.date().year()).ok()?,
        month: utc_ndt.date().month() as u16,
        day: utc_ndt.date().day() as u16,
        hour: utc_ndt.time().hour() as u16,
        minute: utc_ndt.time().minute() as u16,
        second: utc_ndt.time().second() as u16,
        fraction,
    })
}

/// Parse a numeric timezone offset string `"HH:MM"` or `"HH"` into a
/// `chrono::TimeDelta`, applying the given sign (1 or -1).
fn parse_numeric_offset(sign: i32, hhmm: &str) -> Option<chrono::TimeDelta> {
    let mut parts = hhmm.splitn(2, ':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let total_seconds = i64::from(sign) * (h * 3600 + m * 60);
    chrono::TimeDelta::try_seconds(total_seconds)
}

/// The `f64` Trino means by a string-valued float, or `None` for any other
/// string.
///
/// JSON has no literal for the IEEE specials, so Trino sends them as strings:
/// a `DOUBLE` or `REAL` column carrying one arrives as `"NaN"`, `"Infinity"` or
/// `"-Infinity"` rather than as a number. Read off the wire, not from the
/// documentation. See `ieee_specials_are_read_as_floats_not_strings`.
///
/// The spellings are matched exactly rather than case-insensitively: these are
/// the three Trino emits, and accepting looser forms would mean silently
/// turning some other data source's text into a number.
fn trino_special_float(s: &str) -> Option<f64> {
    match s {
        "NaN" => Some(f64::NAN),
        "Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        _ => None,
    }
}

/// The text of a JSON value, without re-encoding a string as JSON.
///
/// `Value::to_string()` renders `Value::String("abc")` as `"\"abc\""`, quote
/// characters and all. Every fallback arm in [`json_to_column_value`] hands its
/// result to the application as data, so a value that failed to convert must
/// arrive as the text Trino sent, not as its JSON encoding.
fn json_as_text(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Convert a JSON value from Trino to an ODBC ColumnValue, guided by the column type.
pub fn json_to_column_value(val: Value, ty: &TrinoTy) -> ColumnValue {
    if val.is_null() {
        return ColumnValue::Null;
    }
    match ty {
        TrinoTy::TrinoInt(TrinoInt::I64) => val
            .as_i64()
            .or_else(|| val.as_str().and_then(|s| s.parse().ok()))
            .map(ColumnValue::I64)
            .unwrap_or_else(|| ColumnValue::String(json_as_text(&val))),
        TrinoTy::TrinoInt(TrinoInt::I32) => match val.as_i64() {
            Some(n) => i32::try_from(n).map(ColumnValue::I32).unwrap_or_else(|_| {
                tracing::warn!(
                    value = %val,
                    declared_type = ?ty,
                    "value does not fit the declared INTEGER column; returning it as text"
                );
                ColumnValue::String(json_as_text(&val))
            }),
            None => ColumnValue::String(json_as_text(&val)),
        },
        TrinoTy::TrinoInt(TrinoInt::I16) => match val.as_i64() {
            Some(n) => i16::try_from(n).map(ColumnValue::I16).unwrap_or_else(|_| {
                tracing::warn!(
                    value = %val,
                    declared_type = ?ty,
                    "value does not fit the declared SMALLINT column; returning it as text"
                );
                ColumnValue::String(json_as_text(&val))
            }),
            None => ColumnValue::String(json_as_text(&val)),
        },
        TrinoTy::TrinoInt(TrinoInt::I8) => match val.as_i64() {
            Some(n) => i8::try_from(n).map(ColumnValue::I8).unwrap_or_else(|_| {
                tracing::warn!(
                    value = %val,
                    declared_type = ?ty,
                    "value does not fit the declared TINYINT column; returning it as text"
                );
                ColumnValue::String(json_as_text(&val))
            }),
            None => ColumnValue::String(json_as_text(&val)),
        },
        TrinoTy::TrinoFloat(TrinoFloat::F64) => val
            .as_f64()
            .or_else(|| val.as_str().and_then(trino_special_float))
            .map(ColumnValue::F64)
            .unwrap_or_else(|| ColumnValue::String(json_as_text(&val))),
        TrinoTy::TrinoFloat(TrinoFloat::F32) => val
            .as_f64()
            .or_else(|| val.as_str().and_then(trino_special_float))
            .map(|f| {
                let n = f as f32;
                if n.is_finite() || !f.is_finite() {
                    ColumnValue::F32(n)
                } else {
                    tracing::warn!(
                        value = %f,
                        declared_type = ?ty,
                        "value overflows the declared REAL column; returning it as text"
                    );
                    ColumnValue::String(json_as_text(&val))
                }
            })
            .unwrap_or_else(|| ColumnValue::String(json_as_text(&val))),
        TrinoTy::Boolean => val
            .as_bool()
            .map(ColumnValue::Bool)
            .unwrap_or_else(|| ColumnValue::String(json_as_text(&val))),
        // Date / time types: Trino serialises these as ISO-8601 strings.
        TrinoTy::Date => {
            if let Value::String(ref s) = val {
                parse_trino_date(s).unwrap_or_else(|| {
                    tracing::warn!(raw = s, "failed to parse Trino DATE string");
                    ColumnValue::String(s.clone())
                })
            } else {
                ColumnValue::String(val.to_string())
            }
        }
        TrinoTy::Time => {
            if let Value::String(ref s) = val {
                parse_trino_time(s).unwrap_or_else(|| {
                    tracing::warn!(raw = s, "failed to parse Trino TIME string");
                    ColumnValue::String(s.clone())
                })
            } else {
                ColumnValue::String(val.to_string())
            }
        }
        // TIME WITH TIME ZONE: parse and normalise to UTC, mirroring
        // TIMESTAMP WITH TIME ZONE. See `parse_trino_time_with_tz` for why
        // this must not share `parse_trino_time`, which silently discards
        // the offset instead of applying it.
        TrinoTy::TimeWithTimeZone => {
            if let Value::String(ref s) = val {
                parse_trino_time_with_tz(s).unwrap_or_else(|| {
                    tracing::warn!(raw = s, "failed to parse Trino TIME WITH TIME ZONE string");
                    ColumnValue::String(s.clone())
                })
            } else {
                ColumnValue::String(val.to_string())
            }
        }
        // TIMESTAMP (no TZ): parse date/time fields directly, no UTC conversion.
        TrinoTy::Timestamp => {
            if let Value::String(ref s) = val {
                parse_trino_timestamp(s).unwrap_or_else(|| {
                    tracing::warn!(raw = s, "failed to parse Trino TIMESTAMP string");
                    ColumnValue::String(s.clone())
                })
            } else {
                ColumnValue::String(val.to_string())
            }
        }
        // TIMESTAMP WITH TIME ZONE: parse and convert to UTC via chrono-tz.
        // Trino sends named zones (UTC, America/New_York, CET) or numeric
        // offsets (+05:30, -08:00); the column type in the REST API metadata
        // determines which parser is called, not the string content.
        TrinoTy::TimestampWithTimeZone => {
            if let Value::String(ref s) = val {
                parse_trino_timestamp_tz(s).unwrap_or_else(|| {
                    tracing::warn!(
                        raw = s,
                        "failed to parse Trino TIMESTAMP WITH TIME ZONE string"
                    );
                    ColumnValue::String(s.clone())
                })
            } else {
                ColumnValue::String(val.to_string())
            }
        }
        TrinoTy::Decimal(_, _) => match val {
            Value::String(s) => ColumnValue::Decimal(s),
            other => ColumnValue::Decimal(other.to_string()),
        },
        TrinoTy::Json => match val {
            Value::String(s) => ColumnValue::Json(s),
            other => ColumnValue::Json(other.to_string()),
        },
        TrinoTy::IntervalYearToMonth => {
            if let Value::String(ref s) = val {
                parse_interval_year_month(s).unwrap_or_else(|| {
                    tracing::warn!(raw = s, "failed to parse Trino INTERVAL YEAR TO MONTH");
                    ColumnValue::String(s.clone())
                })
            } else {
                ColumnValue::String(val.to_string())
            }
        }
        TrinoTy::IntervalDayToSecond => {
            if let Value::String(ref s) = val {
                parse_interval_day_time(s).unwrap_or_else(|| {
                    tracing::warn!(raw = s, "failed to parse Trino INTERVAL DAY TO SECOND");
                    ColumnValue::String(s.clone())
                })
            } else {
                ColumnValue::String(val.to_string())
            }
        }
        // VARBINARY arrives as a base64-encoded string in the REST API payload.
        TrinoTy::VarBinary => {
            if let Value::String(ref s) = val {
                base64_decode(s).unwrap_or_else(|| {
                    tracing::warn!("failed to base64-decode Trino VARBINARY");
                    ColumnValue::String(s.clone())
                })
            } else {
                ColumnValue::String(val.to_string())
            }
        }
        TrinoTy::Array(inner_ty) => {
            if let Value::Array(items) = val {
                let vals = items
                    .into_iter()
                    .map(|v| json_to_column_value(v, inner_ty))
                    .collect();
                ColumnValue::Array(vals)
            } else {
                ColumnValue::String(val.to_string())
            }
        }
        TrinoTy::Map(key_ty, val_ty) => {
            if let Value::Object(map) = val {
                let pairs = map
                    .into_iter()
                    .map(|(k, v)| {
                        let key_col = json_to_column_value(Value::String(k), key_ty);
                        let val_col = json_to_column_value(v, val_ty);
                        (key_col, val_col)
                    })
                    .collect();
                ColumnValue::Map(pairs)
            } else {
                ColumnValue::String(val.to_string())
            }
        }
        TrinoTy::Row(fields) => {
            if let Value::Array(items) = val {
                let vals = items
                    .into_iter()
                    .zip(fields.iter())
                    .map(|(v, (_name, ty))| json_to_column_value(v, ty))
                    .collect();
                ColumnValue::Row(vals)
            } else {
                ColumnValue::String(val.to_string())
            }
        }
        TrinoTy::Tuple(fields) => {
            if let Value::Array(items) = val {
                let vals = items
                    .into_iter()
                    .zip(fields.iter())
                    .map(|(v, ty)| json_to_column_value(v, ty))
                    .collect();
                ColumnValue::Row(vals)
            } else {
                ColumnValue::String(val.to_string())
            }
        }
        // Nullable wrapper: delegate to the inner type (null already handled above)
        TrinoTy::Option(inner) => json_to_column_value(val, inner),
        _ => match val {
            Value::String(s) => ColumnValue::String(s),
            other => ColumnValue::String(other.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bigint_maps_to_ext_big_int() {
        assert_eq!(
            trino_ty_to_sql_type(&TrinoTy::TrinoInt(TrinoInt::I64)),
            SqlDataType::EXT_BIG_INT
        );
    }

    #[test]
    fn integer_maps_to_integer() {
        assert_eq!(
            trino_ty_to_sql_type(&TrinoTy::TrinoInt(TrinoInt::I32)),
            SqlDataType::INTEGER
        );
    }

    #[test]
    fn varchar_maps_to_ext_w_varchar() {
        assert_eq!(
            trino_ty_to_sql_type(&TrinoTy::Varchar),
            SqlDataType::EXT_W_VARCHAR
        );
    }

    #[test]
    fn varbinary_maps_to_ext_long_var_binary() {
        assert_eq!(
            trino_ty_to_sql_type(&TrinoTy::VarBinary),
            SqlDataType::EXT_LONG_VAR_BINARY
        );
    }

    #[test]
    fn varbinary_decodes_base64_to_bytes() {
        // base64("\xDE\xAD\xBE\xEF") == "3q2+7w=="
        let val = Value::String("3q2+7w==".to_string());
        assert_eq!(
            json_to_column_value(val, &TrinoTy::VarBinary),
            ColumnValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])
        );
    }

    #[test]
    fn varbinary_empty_decodes_to_empty_bytes() {
        let val = Value::String(String::new());
        assert_eq!(
            json_to_column_value(val, &TrinoTy::VarBinary),
            ColumnValue::Bytes(Vec::new())
        );
    }

    #[test]
    fn varbinary_invalid_base64_falls_back_to_string() {
        let val = Value::String("not!valid!base64".to_string());
        assert_eq!(
            json_to_column_value(val, &TrinoTy::VarBinary),
            ColumnValue::String("not!valid!base64".to_string())
        );
    }

    #[test]
    fn varbinary_null_maps_to_null() {
        assert_eq!(
            json_to_column_value(Value::Null, &TrinoTy::VarBinary),
            ColumnValue::Null
        );
    }

    #[test]
    fn boolean_maps_to_ext_bit() {
        assert_eq!(
            trino_ty_to_sql_type(&TrinoTy::Boolean),
            SqlDataType::EXT_BIT
        );
    }

    #[test]
    fn bigint_precision_is_19() {
        assert_eq!(trino_ty_precision(&TrinoTy::TrinoInt(TrinoInt::I64)), 19);
    }

    #[test]
    fn boolean_precision_is_1() {
        assert_eq!(trino_ty_precision(&TrinoTy::Boolean), 1);
    }

    #[test]
    fn varchar_precision_is_undeterminable_not_zero() {
        // A precision of 0 here would under-report the length of a column
        // that can legitimately hold arbitrarily long text, producing a
        // zero-sized SQL_DESC_DISPLAY_SIZE for a computed VARCHAR expression
        // with no catalog entry. `i32::MAX` is also wrong: not an allocatable
        // buffer size, and it collides with the different "unbounded but
        // known" convention `SQLGetTypeInfo` uses. See the fallback arm's doc
        // comment in `trino_ty_precision`.
        assert_eq!(
            trino_ty_precision(&TrinoTy::Varchar),
            PRECISION_UNDETERMINABLE
        );
    }

    #[test]
    fn unknown_precision_is_undeterminable_not_zero() {
        assert_eq!(
            trino_ty_precision(&TrinoTy::Unknown),
            PRECISION_UNDETERMINABLE
        );
    }

    #[test]
    fn json_null_returns_column_null() {
        assert_eq!(
            json_to_column_value(Value::Null, &TrinoTy::Varchar),
            ColumnValue::Null
        );
    }

    #[test]
    fn json_string_returns_column_string() {
        assert_eq!(
            json_to_column_value(Value::String("hello".into()), &TrinoTy::Varchar),
            ColumnValue::String("hello".into())
        );
    }

    #[test]
    fn json_number_bigint_returns_i64() {
        assert_eq!(
            json_to_column_value(serde_json::json!(42), &TrinoTy::TrinoInt(TrinoInt::I64)),
            ColumnValue::I64(42)
        );
    }

    // --- json_to_column_value: checked narrowing ---

    #[test]
    fn out_of_range_integer_for_declared_type_is_an_error_not_a_wrap() {
        // Server declared INTEGER but sent a value that does not fit i32.
        let val = json_to_column_value(
            serde_json::json!(4_294_967_296i64),
            &TrinoTy::TrinoInt(TrinoInt::I32),
        );
        // Out-of-range falls back to the text representation, matching what
        // the I64 arm already does for a non-integer JSON value.
        assert_eq!(val, ColumnValue::String("4294967296".to_string()));
    }

    #[test]
    fn in_range_integer_still_converts() {
        let val = json_to_column_value(serde_json::json!(42i64), &TrinoTy::TrinoInt(TrinoInt::I32));
        assert_eq!(val, ColumnValue::I32(42));
    }

    #[test]
    fn out_of_range_i16_falls_back_to_text() {
        let val = json_to_column_value(
            serde_json::json!(70_000i64),
            &TrinoTy::TrinoInt(TrinoInt::I16),
        );
        assert_eq!(val, ColumnValue::String("70000".to_string()));
    }

    #[test]
    fn out_of_range_i8_falls_back_to_text() {
        let val = json_to_column_value(serde_json::json!(200i64), &TrinoTy::TrinoInt(TrinoInt::I8));
        assert_eq!(val, ColumnValue::String("200".to_string()));
    }

    #[test]
    fn negative_out_of_range_integer_falls_back_to_text() {
        let val = json_to_column_value(
            serde_json::json!(-2_147_483_649i64),
            &TrinoTy::TrinoInt(TrinoInt::I32),
        );
        assert_eq!(val, ColumnValue::String("-2147483649".to_string()));
    }

    #[test]
    fn out_of_range_float_for_real_is_not_infinity() {
        let val = json_to_column_value(
            serde_json::json!(1e300f64),
            &TrinoTy::TrinoFloat(TrinoFloat::F32),
        );
        // Not "1e300": serde_json renders the exponent with an explicit sign
        // ("1e+300"), which is what `json_as_text` produces for a non-string
        // value.
        assert_eq!(val, ColumnValue::String("1e+300".to_string()));
    }

    /// Trino encodes the three IEEE specials as JSON *strings*, not numbers
    /// (`["NaN", "Infinity", "-Infinity"]`, confirmed off the wire), because
    /// JSON has no literal for them. Without an arm for a string-valued float
    /// column they fall through to `ColumnValue::String`, and core then
    /// refuses `String -> Double` with `22018`, leaving them unreadable.
    #[test]
    fn ieee_specials_are_read_as_floats_not_strings() {
        for (raw, want) in [
            ("NaN", f64::NAN),
            ("Infinity", f64::INFINITY),
            ("-Infinity", f64::NEG_INFINITY),
        ] {
            let val = json_to_column_value(
                serde_json::json!(raw),
                &TrinoTy::TrinoFloat(TrinoFloat::F64),
            );
            match val {
                ColumnValue::F64(got) => assert!(
                    (got.is_nan() && want.is_nan()) || got == want,
                    "DOUBLE {raw:?} became {got}, expected {want}"
                ),
                other => panic!("DOUBLE {raw:?} did not convert to a float: {other:?}"),
            }
        }
    }

    /// The same three, for `REAL`. The F32 arm reads them through the same
    /// `as_f64()`, which returns `None` for a string, so both arms need the
    /// string case.
    #[test]
    fn ieee_specials_are_read_as_floats_for_real_too() {
        for (raw, want) in [
            ("NaN", f32::NAN),
            ("Infinity", f32::INFINITY),
            ("-Infinity", f32::NEG_INFINITY),
        ] {
            let val = json_to_column_value(
                serde_json::json!(raw),
                &TrinoTy::TrinoFloat(TrinoFloat::F32),
            );
            match val {
                ColumnValue::F32(got) => assert!(
                    (got.is_nan() && want.is_nan()) || got == want,
                    "REAL {raw:?} became {got}, expected {want}"
                ),
                other => panic!("REAL {raw:?} did not convert to a float: {other:?}"),
            }
        }
    }

    /// A string Trino sends for a float column that is *not* one of the three
    /// specials falls back to text, as the text itself rather than as its JSON
    /// encoding.
    ///
    /// The fallback goes through `json_as_text` for that reason.
    /// `Value::to_string()` on a `Value::String` re-adds the quote characters
    /// and would yield `"\"abc\""`, giving an application reading the column as
    /// text two literal quote marks it never sent.
    #[test]
    fn unparseable_float_string_falls_back_without_json_quotes() {
        let val = json_to_column_value(
            serde_json::json!("abc"),
            &TrinoTy::TrinoFloat(TrinoFloat::F64),
        );
        assert_eq!(val, ColumnValue::String("abc".to_string()));
    }

    #[test]
    fn in_range_float_for_real_still_converts() {
        let val = json_to_column_value(
            serde_json::json!(3.5f64),
            &TrinoTy::TrinoFloat(TrinoFloat::F32),
        );
        assert_eq!(val, ColumnValue::F32(3.5));
    }

    // Note: there is no test for "source f64 already infinite" (e.g. Trino
    // sending a value that parses to f64::INFINITY) because serde_json::Value
    // cannot represent a non-finite number at all: `Value::from(f64::INFINITY)`
    // collapses to `Value::Null` (confirmed via `Number::from_f64` returning
    // `None`), and parsing the literal `"1e400"` errors with "number out of
    // range" rather than producing an infinite f64. The `!f.is_finite()`
    // guard in the F32 arm below is therefore defensive/unreachable through
    // this entry point, but is kept since it costs nothing
    // and documents the intent (an already-infinite source is not the
    // overflow the guard targets).

    #[test]
    fn json_bool_returns_column_bool() {
        assert_eq!(
            json_to_column_value(Value::Bool(true), &TrinoTy::Boolean),
            ColumnValue::Bool(true)
        );
    }

    #[test]
    fn type_name_varchar_maps_to_wvarchar() {
        assert_eq!(
            trino_type_name_to_sql_type("varchar"),
            SqlDataType::EXT_W_VARCHAR
        );
    }

    #[test]
    fn type_name_integer_maps_to_integer() {
        assert_eq!(trino_type_name_to_sql_type("integer"), SqlDataType::INTEGER);
    }

    #[test]
    fn type_name_bigint_maps_to_bigint() {
        assert_eq!(
            trino_type_name_to_sql_type("bigint"),
            SqlDataType::EXT_BIG_INT
        );
    }

    #[test]
    fn type_name_boolean_maps_to_bit() {
        assert_eq!(trino_type_name_to_sql_type("boolean"), SqlDataType::EXT_BIT);
    }

    #[test]
    fn type_name_double_maps_to_double() {
        assert_eq!(trino_type_name_to_sql_type("double"), SqlDataType::DOUBLE);
    }

    #[test]
    fn type_name_varchar_with_param_maps_to_wvarchar() {
        // Verifies parametric suffix stripping: "varchar(255)" → "varchar"
        assert_eq!(
            trino_type_name_to_sql_type("varchar(255)"),
            SqlDataType::EXT_W_VARCHAR
        );
    }

    #[test]
    fn type_name_decimal_maps_to_decimal() {
        assert_eq!(
            trino_type_name_to_sql_type("decimal(10,2)"),
            SqlDataType::DECIMAL
        );
    }

    #[test]
    fn type_name_date_maps_to_date() {
        assert_eq!(trino_type_name_to_sql_type("date"), SqlDataType::DATE);
    }

    #[test]
    fn type_name_timestamp_maps_to_timestamp() {
        assert_eq!(
            trino_type_name_to_sql_type("timestamp(3)"),
            SqlDataType::TIMESTAMP
        );
    }

    #[test]
    fn type_name_unknown_maps_to_wvarchar() {
        assert_eq!(
            trino_type_name_to_sql_type("row(x integer, y varchar)"),
            SqlDataType::EXT_W_VARCHAR
        );
    }

    #[test]
    fn type_name_path_recovers_varchar_length() {
        // The query path must agree with the catalog path, which already
        // parses the length out of the type-name string.
        assert_eq!(type_name_precision("varchar(50)"), Some(50));
        assert_eq!(type_name_precision("decimal(10,2)"), Some(10));
        assert_eq!(type_name_scale("decimal(10,2)"), Some(2));
    }

    // --- TrinoTypeName::parse: precision-argument-in-the-middle ---
    //
    // `TrinoTypeName::parse` must not truncate at the first `(`: that would
    // destroy the ` with time zone` suffix on `"timestamp(3) with time
    // zone"` / `"time(3) with time zone"` and cause them to silently parse
    // as the plain (non-TZ) variant instead of failing to parse, making
    // the query path (`execute.rs`) report the wrong precision because its
    // `unwrap_or_else` fallback to `trino_ty_precision` never fires.

    #[test]
    fn parse_timestamp_without_param() {
        assert!(matches!(
            TrinoTypeName::parse("timestamp"),
            Some(TrinoTypeName::Timestamp)
        ));
    }

    #[test]
    fn parse_timestamp_with_param() {
        assert!(matches!(
            TrinoTypeName::parse("timestamp(3)"),
            Some(TrinoTypeName::Timestamp)
        ));
    }

    #[test]
    fn parse_timestamp_with_time_zone_no_param() {
        assert!(matches!(
            TrinoTypeName::parse("timestamp with time zone"),
            Some(TrinoTypeName::TimestampWithTimeZone)
        ));
    }

    #[test]
    fn parse_timestamp_with_time_zone_and_param() {
        assert!(matches!(
            TrinoTypeName::parse("timestamp(3) with time zone"),
            Some(TrinoTypeName::TimestampWithTimeZone)
        ));
    }

    #[test]
    fn parse_time_without_param() {
        assert!(matches!(
            TrinoTypeName::parse("time"),
            Some(TrinoTypeName::Time)
        ));
    }

    #[test]
    fn parse_time_with_param() {
        assert!(matches!(
            TrinoTypeName::parse("time(3)"),
            Some(TrinoTypeName::Time)
        ));
    }

    #[test]
    fn parse_time_with_time_zone_no_param() {
        assert!(matches!(
            TrinoTypeName::parse("time with time zone"),
            Some(TrinoTypeName::TimeWithTimeZone)
        ));
    }

    #[test]
    fn parse_time_with_time_zone_and_param() {
        assert!(matches!(
            TrinoTypeName::parse("time(3) with time zone"),
            Some(TrinoTypeName::TimeWithTimeZone)
        ));
    }

    #[test]
    fn parse_varchar_with_param_unchanged() {
        assert!(matches!(
            TrinoTypeName::parse("varchar"),
            Some(TrinoTypeName::Varchar)
        ));
        assert!(matches!(
            TrinoTypeName::parse("varchar(50)"),
            Some(TrinoTypeName::Varchar)
        ));
    }

    #[test]
    fn parse_char_with_param_unchanged() {
        assert!(matches!(
            TrinoTypeName::parse("char(10)"),
            Some(TrinoTypeName::Char)
        ));
    }

    #[test]
    fn parse_decimal_with_param_unchanged() {
        assert!(matches!(
            TrinoTypeName::parse("decimal(10,2)"),
            Some(TrinoTypeName::Decimal)
        ));
    }

    // --- TrinoTypeName::parse: INTERVAL types ---
    //
    // `trino_type_info` (backend/info.rs) advertises "INTERVAL DAY TO SECOND"
    // and "INTERVAL YEAR TO MONTH" rows, so `TrinoTypeName::parse` needs a
    // variant for each. Without one, `trino_bare_type_name` falls through to
    // the EXT_W_VARCHAR/"VARCHAR" fallback and no interval column can be
    // reported under the name its own SQLGetTypeInfo row advertises. This
    // matches how `Json` and `Uuid` are handled, for the same reason.

    #[test]
    fn parse_interval_day_to_second() {
        assert!(matches!(
            TrinoTypeName::parse("interval day to second"),
            Some(TrinoTypeName::IntervalDayToSecond)
        ));
    }

    #[test]
    fn parse_interval_year_to_month() {
        assert!(matches!(
            TrinoTypeName::parse("interval year to month"),
            Some(TrinoTypeName::IntervalYearToMonth)
        ));
    }

    #[test]
    fn parse_varchar_param_still_recovers_length() {
        // Confirms the argument-in-the-middle handling does not break the
        // `type_name_precision` / `type_name_scale` extraction for types
        // whose argument sits at the end of the string.
        assert_eq!(type_name_precision("varchar(50)"), Some(50));
        assert_eq!(type_name_precision("char(10)"), Some(10));
        assert_eq!(type_name_precision("decimal(10,2)"), Some(10));
        assert_eq!(type_name_scale("decimal(10,2)"), Some(2));
    }

    /// `TIMESTAMP WITH TIME ZONE` and plain `TIMESTAMP` report the same
    /// precision (23, `YYYY-MM-DD HH:MM:SS.mmm`) because the offset is applied
    /// and discarded rather than carried in the string (see
    /// `parse_trino_timestamp_tz`), so this test does not distinguish a
    /// mis-parse (`TrinoTypeName::parse` silently matching the wrong non-TZ
    /// variant for parenthesised time-zone types) from the correct value.
    /// That distinction is covered separately by
    /// `parse_timestamp_with_time_zone_and_param`, which asserts
    /// `TrinoTypeName::parse` resolves to the `TimestampWithTimeZone`
    /// variant, not `Timestamp`. What this test pins is
    /// `type_name_precision`'s output for this input string, which is what
    /// `execute.rs` reports:
    /// `type_name_precision(&native_name).unwrap_or_else(|| trino_ty_precision(&ty))`.
    #[test]
    fn query_path_timestamp_with_time_zone_precision_is_23() {
        assert_eq!(type_name_precision("timestamp(3) with time zone"), Some(23));
    }

    /// `TIME WITH TIME ZONE` and plain `TIME` report the same precision
    /// (12, `HH:MM:SS.mmm`, 9 + `DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME`)
    /// because the offset is applied and discarded rather than carried in the
    /// string (see `parse_trino_time_with_tz`), so this test does not
    /// distinguish a mis-parse from the correct value, the same as
    /// `..._timestamp_..._23` above; that
    /// distinction is covered separately by
    /// `parse_time_with_time_zone_and_param`, which asserts `TrinoTypeName::parse`
    /// resolves to the `TimeWithTimeZone` variant, not `Time`. What this test
    /// pins is `type_name_precision`'s output for this input string.
    #[test]
    fn query_path_time_with_time_zone_precision_is_12() {
        assert_eq!(type_name_precision("time(3) with time zone"), Some(12));
    }

    // --- type_name_precision/type_name_scale: declared temporal scale ---
    //
    // The tests above (`..._precision_is_23`/`..._is_12`) use scale 3, which
    // happens to equal `DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME`, so they
    // cannot distinguish "the declared scale was read from the type string"
    // from "the fallback default was used and happened to match". These
    // tests use scale 6 specifically to rule that out.

    #[test]
    fn timestamp_6_reports_column_size_26_not_the_bare_scale() {
        // 20 + 6 = 26 (ODBC "Column Size" appendix formula), not `6`:
        // treating the parenthesised argument as if it were the column size
        // directly reports the bare scale, which is wrong.
        assert_eq!(type_name_precision("timestamp(6)"), Some(26));
    }

    #[test]
    fn timestamp_6_reports_decimal_digits_6() {
        // SQL_DESC_PRECISION/decimal digits for a datetime type is the
        // fractional-seconds scale itself, not the column size (the
        // companion quantity `type_name_precision` above must NOT collapse
        // into).
        assert_eq!(type_name_scale("timestamp(6)"), Some(6));
    }

    #[test]
    fn timestamp_with_time_zone_6_reports_column_size_26() {
        assert_eq!(type_name_precision("timestamp(6) with time zone"), Some(26));
        assert_eq!(type_name_scale("timestamp(6) with time zone"), Some(6));
    }

    #[test]
    fn time_6_reports_column_size_15_not_the_bare_scale() {
        // 9 + 6 = 15, not `6`.
        assert_eq!(type_name_precision("time(6)"), Some(15));
        assert_eq!(type_name_scale("time(6)"), Some(6));
    }

    #[test]
    fn time_with_time_zone_6_reports_column_size_15() {
        assert_eq!(type_name_precision("time(6) with time zone"), Some(15));
        assert_eq!(type_name_scale("time(6) with time zone"), Some(6));
    }

    #[test]
    fn timestamp_0_reports_column_size_19_and_scale_0() {
        // scale 0 is a real, explicit declaration (no fractional seconds at
        // all), distinct from "no argument present at all" below; both must
        // still produce the correct formula output (19 = 20 + 0, not 20).
        assert_eq!(type_name_precision("timestamp(0)"), Some(19));
        assert_eq!(type_name_scale("timestamp(0)"), Some(0));
    }

    #[test]
    fn bare_timestamp_with_no_type_name_argument_falls_back_to_the_default_scale() {
        // No parenthesised argument at all (e.g. a bare "timestamp" string):
        // `DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME` (3) is the only
        // reasonable fallback here, matching `TrinoTypeName::fixed_precision`'s
        // behaviour for the no-type-name-string case.
        assert_eq!(type_name_precision("timestamp"), Some(23));
        assert_eq!(type_name_scale("timestamp"), Some(3));
    }

    // --- trino_ty_to_sql_type: new type coverage ---

    #[test]
    fn decimal_maps_to_decimal() {
        assert_eq!(
            trino_ty_to_sql_type(&TrinoTy::Decimal(10, 2)),
            SqlDataType::DECIMAL
        );
    }

    #[test]
    fn time_maps_to_time() {
        assert_eq!(trino_ty_to_sql_type(&TrinoTy::Time), SqlDataType::TIME);
    }

    #[test]
    fn time_with_tz_maps_to_time() {
        assert_eq!(
            trino_ty_to_sql_type(&TrinoTy::TimeWithTimeZone),
            SqlDataType::TIME
        );
    }

    #[test]
    fn timestamp_with_tz_maps_to_timestamp() {
        assert_eq!(
            trino_ty_to_sql_type(&TrinoTy::TimestampWithTimeZone),
            SqlDataType::TIMESTAMP
        );
    }

    #[test]
    fn uuid_maps_to_wvarchar() {
        assert_eq!(
            trino_ty_to_sql_type(&TrinoTy::Uuid),
            SqlDataType::EXT_W_VARCHAR
        );
    }

    #[test]
    fn json_maps_to_wvarchar() {
        assert_eq!(
            trino_ty_to_sql_type(&TrinoTy::Json),
            SqlDataType::EXT_W_VARCHAR
        );
    }

    #[test]
    fn array_maps_to_wvarchar() {
        assert_eq!(
            trino_ty_to_sql_type(&TrinoTy::Array(Box::new(TrinoTy::TrinoInt(TrinoInt::I64)))),
            SqlDataType::EXT_W_VARCHAR
        );
    }

    #[test]
    fn option_delegates_to_inner_type() {
        assert_eq!(
            trino_ty_to_sql_type(&TrinoTy::Option(Box::new(TrinoTy::TrinoInt(TrinoInt::I64)))),
            SqlDataType::EXT_BIG_INT
        );
    }

    // --- trino_ty_precision: new type coverage ---

    #[test]
    fn decimal_precision_extracted() {
        assert_eq!(trino_ty_precision(&TrinoTy::Decimal(10, 2)), 10);
    }

    #[test]
    fn time_precision_is_12() {
        // 9 + 3 (DEFAULT_TEMPORAL_SCALE_WITHOUT_TYPE_NAME, Trino's actual
        // default declared precision) = 12, HH:MM:SS.mmm.
        assert_eq!(trino_ty_precision(&TrinoTy::Time), 12);
    }

    #[test]
    fn time_with_tz_precision_is_12() {
        // Not 18 (HH:MM:SS.mmm+HH:MM): the offset is applied and the value
        // normalised to UTC, so only the fractional seconds are delivered.
        assert_eq!(trino_ty_precision(&TrinoTy::TimeWithTimeZone), 12);
    }

    #[test]
    fn date_precision_is_10() {
        // YYYY-MM-DD = 10 chars
        assert_eq!(trino_ty_precision(&TrinoTy::Date), 10);
    }

    #[test]
    fn timestamp_precision_is_23() {
        // YYYY-MM-DD HH:MM:SS.mmm = 23 chars (default millisecond precision)
        assert_eq!(trino_ty_precision(&TrinoTy::Timestamp), 23);
    }

    #[test]
    fn timestamp_with_tz_precision_is_23() {
        // Not 29 (YYYY-MM-DD HH:MM:SS.mmm+HH:MM): the offset is applied and
        // the value normalised to UTC, so only YYYY-MM-DD HH:MM:SS.mmm
        // survives into SQL_TIMESTAMP_STRUCT.
        assert_eq!(trino_ty_precision(&TrinoTy::TimestampWithTimeZone), 23);
    }

    #[test]
    fn option_precision_delegates_to_inner() {
        assert_eq!(
            trino_ty_precision(&TrinoTy::Option(Box::new(TrinoTy::TrinoInt(TrinoInt::I64)))),
            19
        );
    }

    // --- trino_ty_scale ---

    #[test]
    fn decimal_scale_extracted() {
        assert_eq!(trino_ty_scale(&TrinoTy::Decimal(10, 3)), 3);
    }

    #[test]
    fn integer_scale_is_zero() {
        assert_eq!(trino_ty_scale(&TrinoTy::TrinoInt(TrinoInt::I64)), 0);
    }

    #[test]
    fn option_decimal_scale_delegates_to_inner() {
        assert_eq!(
            trino_ty_scale(&TrinoTy::Option(Box::new(TrinoTy::Decimal(10, 4)))),
            4
        );
    }

    // --- json_to_column_value: Option unwrapping ---

    #[test]
    fn option_i64_non_null_converts_as_i64() {
        assert_eq!(
            json_to_column_value(
                serde_json::json!(99),
                &TrinoTy::Option(Box::new(TrinoTy::TrinoInt(TrinoInt::I64)))
            ),
            ColumnValue::I64(99)
        );
    }

    #[test]
    fn option_i64_null_returns_null() {
        assert_eq!(
            json_to_column_value(
                Value::Null,
                &TrinoTy::Option(Box::new(TrinoTy::TrinoInt(TrinoInt::I64)))
            ),
            ColumnValue::Null
        );
    }

    // --- date / time / timestamp parsing ---

    #[test]
    fn date_string_parses_to_column_date() {
        assert_eq!(
            json_to_column_value(Value::String("1998-01-14".into()), &TrinoTy::Date),
            ColumnValue::Date {
                year: 1998,
                month: 1,
                day: 14
            }
        );
    }

    /// Trino renders a year before 1 CE with a leading `-`, which splits the
    /// same way as the field separators. A parser that does not strip the sign
    /// first drops the whole value to the string fallback, and
    /// `SQLGetData(SQL_C_TYPE_DATE)` then fails on a column the driver
    /// described as `SQL_TYPE_DATE`.
    ///
    /// This driver produces such a date itself, from a bound `SQL_DATE_STRUCT`
    /// with a negative year, so it has to be able to read one back.
    #[test]
    fn a_date_before_1_ce_parses_to_column_date() {
        assert_eq!(
            json_to_column_value(Value::String("-0001-01-01".into()), &TrinoTy::Date),
            ColumnValue::Date {
                year: -1,
                month: 1,
                day: 1
            }
        );
    }

    /// The same argument as `a_date_before_1_ce_parses_to_column_date`, for the
    /// type that had the local `splitn` the shared parser now replaces.
    /// `backend::params` renders a bound `SQL_TIMESTAMP_STRUCT` through the same
    /// `year4` as a `SQL_DATE_STRUCT`, so the two must read the same years.
    #[test]
    fn a_timestamp_before_1_ce_parses_to_column_timestamp() {
        assert_eq!(
            json_to_column_value(
                Value::String("-0001-01-01 12:34:56.789".into()),
                &TrinoTy::Timestamp
            ),
            ColumnValue::Timestamp {
                year: -1,
                month: 1,
                day: 1,
                hour: 12,
                minute: 34,
                second: 56,
                fraction: 789_000_000,
            }
        );
    }

    /// The two parsers agree on every year either can meet, which is the
    /// property that made the timestamp defect invisible: `DATE` round-tripped,
    /// so the shared renderer looked correct.
    #[test]
    fn dates_and_timestamps_read_the_same_years() {
        for year in ["-4713", "-0001", "0000", "0001", "1970", "9999"] {
            let date = json_to_column_value(Value::String(format!("{year}-06-15")), &TrinoTy::Date);
            let timestamp = json_to_column_value(
                Value::String(format!("{year}-06-15 00:00:00")),
                &TrinoTy::Timestamp,
            );
            let ColumnValue::Date { year: d, .. } = date else {
                panic!("{year} did not parse as a DATE: {date:?}");
            };
            let ColumnValue::Timestamp { year: t, .. } = timestamp else {
                panic!("{year} parsed as a DATE but not as a TIMESTAMP: {timestamp:?}");
            };
            assert_eq!(d, t, "the two parsers disagree about year {year}");
        }
    }

    #[test]
    fn year_zero_parses_to_column_date() {
        assert_eq!(
            json_to_column_value(Value::String("0000-01-01".into()), &TrinoTy::Date),
            ColumnValue::Date {
                year: 0,
                month: 1,
                day: 1
            }
        );
    }

    /// A year beyond `SQL_DATE_STRUCT`'s signed 16-bit field cannot be carried
    /// as a date at all, so it keeps the documented string fallback rather
    /// than being truncated into a different year.
    #[test]
    fn a_year_beyond_the_date_struct_falls_back_to_text() {
        assert!(matches!(
            json_to_column_value(Value::String("+99999-01-01".into()), &TrinoTy::Date),
            ColumnValue::String(_)
        ));
    }

    #[test]
    fn time_string_parses_to_column_time() {
        assert_eq!(
            json_to_column_value(Value::String("13:14:15".into()), &TrinoTy::Time),
            ColumnValue::Time {
                hour: 13,
                minute: 14,
                second: 15,
                fraction: 0,
            }
        );
    }

    #[test]
    fn time_with_fractional_seconds_parses_correctly() {
        // The fraction is kept, not discarded: SQL_TIME_STRUCT cannot carry
        // it, but the SQL_C_CHAR/SQL_C_WCHAR string rendering can.
        assert_eq!(
            json_to_column_value(Value::String("09:05:03.336".into()), &TrinoTy::Time),
            ColumnValue::Time {
                hour: 9,
                minute: 5,
                second: 3,
                fraction: 336_000_000,
            }
        );
    }

    #[test]
    fn time_with_timezone_parses_correctly() {
        assert_eq!(
            json_to_column_value(
                Value::String("13:14:15.000 UTC".into()),
                &TrinoTy::TimeWithTimeZone
            ),
            ColumnValue::Time {
                hour: 13,
                minute: 14,
                second: 15,
                fraction: 0,
            }
        );
    }

    #[test]
    fn time_with_timezone_normalises_to_utc() {
        // The two "with time zone" types must agree: TIMESTAMP WITH TIME
        // ZONE converts to UTC, so discarding the offset here rather than
        // applying it would make TIME WITH TIME ZONE contradict it.
        let val = parse_trino_time_with_tz("13:14:15+02:00").expect("parses");
        assert_eq!(
            val,
            ColumnValue::Time {
                hour: 11,
                minute: 14,
                second: 15,
                fraction: 0,
            }
        );
    }

    #[test]
    fn time_with_negative_offset_normalises_to_utc() {
        let val = parse_trino_time_with_tz("13:14:15-05:30").expect("parses");
        assert_eq!(
            val,
            ColumnValue::Time {
                hour: 18,
                minute: 44,
                second: 15,
                fraction: 0,
            }
        );
    }

    #[test]
    fn time_with_offset_wraps_across_midnight() {
        let val = parse_trino_time_with_tz("01:00:00+02:00").expect("parses");
        assert_eq!(
            val,
            ColumnValue::Time {
                hour: 23,
                minute: 0,
                second: 0,
                fraction: 0,
            }
        );
    }

    #[test]
    fn time_with_utc_offset_is_unchanged() {
        let val = parse_trino_time_with_tz("13:14:15.000 UTC").expect("parses");
        assert_eq!(
            val,
            ColumnValue::Time {
                hour: 13,
                minute: 14,
                second: 15,
                fraction: 0,
            }
        );
    }

    #[test]
    fn time_with_timezone_keeps_fraction_through_offset_shift() {
        // The offset shift only touches whole minutes, so a fractional-seconds
        // part must survive `shift_time` unchanged.
        let val = parse_trino_time_with_tz("13:14:15.123456+02:00").expect("parses");
        assert_eq!(
            val,
            ColumnValue::Time {
                hour: 11,
                minute: 14,
                second: 15,
                fraction: 123_456_000,
            }
        );
    }

    #[test]
    fn timestamp_string_parses_to_column_timestamp() {
        assert_eq!(
            json_to_column_value(
                Value::String("1998-01-14 13:14:15".into()),
                &TrinoTy::Timestamp
            ),
            ColumnValue::Timestamp {
                year: 1998,
                month: 1,
                day: 14,
                hour: 13,
                minute: 14,
                second: 15,
                fraction: 0
            }
        );
    }

    #[test]
    fn timestamp_with_millis_converts_fraction_to_nanoseconds() {
        assert_eq!(
            json_to_column_value(
                Value::String("1998-01-14 13:14:15.123".into()),
                &TrinoTy::Timestamp
            ),
            ColumnValue::Timestamp {
                year: 1998,
                month: 1,
                day: 14,
                hour: 13,
                minute: 14,
                second: 15,
                fraction: 123_000_000
            }
        );
    }

    /// UTC is a no-op conversion: fields should pass through unchanged.
    #[test]
    fn timestamp_with_named_timezone_converts_to_utc() {
        let val = json_to_column_value(
            Value::String("1998-01-14 13:14:15.000 UTC".into()),
            &TrinoTy::TimestampWithTimeZone,
        );
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 1998,
                month: 1,
                day: 14,
                hour: 13,
                minute: 14,
                second: 15,
                fraction: 0,
            },
        );
    }

    #[test]
    fn date_null_returns_null() {
        assert_eq!(
            json_to_column_value(Value::Null, &TrinoTy::Date),
            ColumnValue::Null
        );
    }

    #[test]
    fn decimal_maps_to_decimal_variant() {
        use serde_json::json;
        let val = json_to_column_value(json!("123.456"), &TrinoTy::Decimal(6, 3));
        assert_eq!(val, ColumnValue::Decimal("123.456".to_string()));
    }

    #[test]
    fn json_ty_maps_to_json_variant() {
        use serde_json::json;
        let val = json_to_column_value(json!(r#"{"a":1}"#), &TrinoTy::Json);
        assert_eq!(val, ColumnValue::Json(r#"{"a":1}"#.to_string()));
    }

    #[test]
    fn interval_year_month_parses_correctly() {
        use serde_json::json;
        let val = json_to_column_value(json!("3-7"), &TrinoTy::IntervalYearToMonth);
        assert_eq!(
            val,
            ColumnValue::IntervalYearMonth {
                years: 3,
                months: 7,
                precision: Interval::YearToMonth,
            }
        );
    }

    #[test]
    fn interval_day_time_parses_correctly() {
        use serde_json::json;
        let val = json_to_column_value(json!("2 03:04:05.678"), &TrinoTy::IntervalDayToSecond);
        assert_eq!(
            val,
            ColumnValue::IntervalDayTime {
                total_nanoseconds: 2 * NANOS_PER_DAY
                    + 3 * NANOS_PER_HOUR
                    + 4 * NANOS_PER_MINUTE
                    + 5 * NANOS_PER_SECOND
                    + 678_000_000,
                precision: Interval::DayToSecond,
            }
        );
    }

    #[test]
    fn negative_interval_day_time_is_fully_negative() {
        let val = parse_interval_day_time("-2 03:04:05.678").expect("parses");
        // -(2 days + 3h4m5.678s) = -183_845_678 ms in nanoseconds.
        assert_eq!(
            val,
            ColumnValue::IntervalDayTime {
                total_nanoseconds: -183_845_678_000_000,
                precision: Interval::DayToSecond,
            }
        );
    }

    #[test]
    fn negative_zero_day_interval_keeps_its_sign() {
        // "-0 03:04:05" must keep its sign: parsing the sign only off `days`
        // loses it entirely, because "-0".parse::<i64>() is 0.
        let val = parse_interval_day_time("-0 03:04:05").expect("parses");
        assert_eq!(
            val,
            ColumnValue::IntervalDayTime {
                total_nanoseconds: -11_045_000_000_000,
                precision: Interval::DayToSecond,
            }
        );
    }

    #[test]
    fn positive_interval_day_time_is_unchanged() {
        let val = parse_interval_day_time("2 03:04:05.678").expect("parses");
        assert_eq!(
            val,
            ColumnValue::IntervalDayTime {
                total_nanoseconds: 183_845_678_000_000,
                precision: Interval::DayToSecond,
            }
        );
    }

    /// A fraction finer than Trino's own millisecond rendering survives now that
    /// the variant counts nanoseconds: the parser no longer truncates at three
    /// digits.
    #[test]
    fn interval_day_time_keeps_sub_millisecond_digits() {
        let val = parse_interval_day_time("0 00:00:01.234567").expect("parses");
        assert_eq!(
            val,
            ColumnValue::IntervalDayTime {
                total_nanoseconds: NANOS_PER_SECOND + 234_567_000,
                precision: Interval::DayToSecond,
            }
        );
    }

    /// Numeric offset +05:30: 10:30 local = 05:00 UTC (subtract 5h30m).
    #[test]
    fn timestamp_with_tz_numeric_offset_converts_to_utc() {
        let val = json_to_column_value(
            Value::String("2024-03-15 10:30:00.000 +05:30".into()),
            &TrinoTy::TimestampWithTimeZone,
        );
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 2024,
                month: 3,
                day: 15,
                hour: 5,
                minute: 0,
                second: 0,
                fraction: 0,
            },
        );
    }

    #[test]
    fn timestamp_tz_named_utc_converts_to_utc_timestamp() {
        let val = json_to_column_value(
            Value::String("2020-05-05 22:00:00.000 UTC".into()),
            &TrinoTy::TimestampWithTimeZone,
        );
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 2020,
                month: 5,
                day: 5,
                hour: 22,
                minute: 0,
                second: 0,
                fraction: 0,
            },
        );
    }

    #[test]
    fn timestamp_tz_named_zone_converts_to_utc() {
        // America/New_York in March 2025 is EDT (UTC-4).
        // 20:21:22 EDT = 2025-03-11 00:21:22 UTC (date rolls forward).
        let val = json_to_column_value(
            Value::String("2025-03-10 20:21:22.123 America/New_York".into()),
            &TrinoTy::TimestampWithTimeZone,
        );
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 2025,
                month: 3,
                day: 11,
                hour: 0,
                minute: 21,
                second: 22,
                fraction: 123_000_000,
            },
        );
    }

    #[test]
    fn timestamp_tz_numeric_offset_converts_to_utc() {
        // +05:30 means wall clock is 5h30m ahead of UTC.
        // 10:30:00 +05:30 = 05:00:00 UTC (same day).
        let val = json_to_column_value(
            Value::String("2024-03-15 10:30:00.000 +05:30".into()),
            &TrinoTy::TimestampWithTimeZone,
        );
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 2024,
                month: 3,
                day: 15,
                hour: 5,
                minute: 0,
                second: 0,
                fraction: 0,
            },
        );
    }

    #[test]
    fn timestamp_tz_negative_offset_converts_to_utc() {
        // -08:00: 16:00:00 PST = 2024-12-16 00:00:00 UTC (date rolls forward).
        let val = json_to_column_value(
            Value::String("2024-12-15 16:00:00.000 -08:00".into()),
            &TrinoTy::TimestampWithTimeZone,
        );
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 2024,
                month: 12,
                day: 16,
                hour: 0,
                minute: 0,
                second: 0,
                fraction: 0,
            },
        );
    }

    #[test]
    fn timestamp_tz_dst_winter_converts_correctly() {
        // America/New_York in December is EST (UTC-5).
        // 23:00:00 EST = 2025-01-02 04:00:00 UTC.
        let val = json_to_column_value(
            Value::String("2025-01-01 23:00:00.000 America/New_York".into()),
            &TrinoTy::TimestampWithTimeZone,
        );
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 2025,
                month: 1,
                day: 2,
                hour: 4,
                minute: 0,
                second: 0,
                fraction: 0,
            },
        );
    }

    /// Trino can send POSIX abbreviations like CET; chrono-tz resolves these.
    /// CET is UTC+1 (no DST variant; CEST is the summer equivalent).
    #[test]
    fn timestamp_tz_posix_abbreviation_cet_converts_to_utc() {
        // CET (Central European Time) = UTC+1.
        // 15:00:00 CET = 14:00:00 UTC.
        let val = json_to_column_value(
            Value::String("2025-01-15 15:00:00.000 CET".into()),
            &TrinoTy::TimestampWithTimeZone,
        );
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 2025,
                month: 1,
                day: 15,
                hour: 14,
                minute: 0,
                second: 0,
                fraction: 0,
            },
        );
    }

    /// Full IANA name Europe/Berlin, same offset as CET in winter (UTC+1),
    /// but Europe/Berlin also covers CEST (UTC+2) in summer. This test uses
    /// a winter date so the expected result matches CET.
    #[test]
    fn timestamp_tz_europe_berlin_winter_converts_to_utc() {
        let val = json_to_column_value(
            Value::String("2025-01-15 15:00:00.000 Europe/Berlin".into()),
            &TrinoTy::TimestampWithTimeZone,
        );
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 2025,
                month: 1,
                day: 15,
                hour: 14,
                minute: 0,
                second: 0,
                fraction: 0,
            },
        );
    }

    /// Europe/Berlin in summer is CEST (UTC+2): verify DST shifts correctly.
    #[test]
    fn timestamp_tz_europe_berlin_summer_converts_to_utc() {
        // 2025-07-15 is in CEST (UTC+2).
        // 15:00:00 CEST = 13:00:00 UTC.
        let val = json_to_column_value(
            Value::String("2025-07-15 15:00:00.000 Europe/Berlin".into()),
            &TrinoTy::TimestampWithTimeZone,
        );
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 2025,
                month: 7,
                day: 15,
                hour: 13,
                minute: 0,
                second: 0,
                fraction: 0,
            },
        );
    }

    /// Hour-only numeric offset without minutes (+05 instead of +05:00).
    /// parse_numeric_offset defaults the minutes component to 0.
    #[test]
    fn timestamp_tz_hour_only_numeric_offset() {
        // +05 = +05:00. 10:00:00 +05:00 = 05:00:00 UTC.
        let val = json_to_column_value(
            Value::String("2024-06-01 10:00:00.000 +05".into()),
            &TrinoTy::TimestampWithTimeZone,
        );
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 2024,
                month: 6,
                day: 1,
                hour: 5,
                minute: 0,
                second: 0,
                fraction: 0,
            },
        );
    }

    /// Zero offset (+00:00) is equivalent to UTC.
    #[test]
    fn timestamp_tz_zero_numeric_offset() {
        let val = json_to_column_value(
            Value::String("2024-06-01 10:00:00.000 +00:00".into()),
            &TrinoTy::TimestampWithTimeZone,
        );
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 2024,
                month: 6,
                day: 1,
                hour: 10,
                minute: 0,
                second: 0,
                fraction: 0,
            },
        );
    }

    /// Plain TIMESTAMP (no TZ) is a separate code path, via
    /// `TrinoTy::Timestamp`, and no UTC conversion applies to it.
    #[test]
    fn timestamp_no_tz_is_unaffected_by_tz_changes() {
        let val = json_to_column_value(
            Value::String("2025-06-15 09:30:45.678".into()),
            &TrinoTy::Timestamp,
        );
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 2025,
                month: 6,
                day: 15,
                hour: 9,
                minute: 30,
                second: 45,
                fraction: 678_000_000,
            },
        );
    }

    /// Fraction (sub-second nanoseconds) must survive UTC conversion unchanged.
    #[test]
    fn timestamp_tz_preserves_fraction_through_conversion() {
        // 10:30:00.999 +05:30 = 05:00:00.999 UTC; fraction stays 999ms.
        let val = json_to_column_value(
            Value::String("2024-03-15 10:30:00.999 +05:30".into()),
            &TrinoTy::TimestampWithTimeZone,
        );
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 2024,
                month: 3,
                day: 15,
                hour: 5,
                minute: 0,
                second: 0,
                fraction: 999_000_000,
            },
        );
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        // The type-name parsers must never panic on any input, however
        // malformed: a panic would cross the FFI boundary.
        #[test]
        fn type_name_parsers_never_panic(s in ".*") {
            let _ = type_name_precision(&s);
            let _ = type_name_scale(&s);
            let _ = trino_type_name_to_sql_type(&s);
        }

        // A well-formed `varchar(n)` reports its declared length as the precision.
        #[test]
        fn varchar_precision_round_trips(n in 1i32..1_000_000) {
            prop_assert_eq!(type_name_precision(&format!("varchar({n})")), Some(n));
        }

        // `decimal(p,s)` reports p as the precision and s as the scale.
        #[test]
        fn decimal_precision_and_scale_round_trip(p in 1i32..=38, s in 0i32..=38) {
            let decl = format!("decimal({p},{s})");
            prop_assert_eq!(type_name_precision(&decl), Some(p));
            prop_assert_eq!(type_name_scale(&decl), Some(s));
        }
    }
}
