#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use stackable_odbc_trino::fuzz_api::json_value;
use trino_rust_client::{TrinoFloat, TrinoInt, TrinoTy};

// Fuzzes the read path's first half: a coordinator's JSON value under its
// declared Trino type, becoming a ColumnValue.
//
// stackable-odbc-core already fuzzes the second half (`write_column_value`,
// ColumnValue -> the caller's buffer). Nothing covered the step before it,
// which is where this crate's temporal, interval and decimal parsers live:
// roughly a dozen hand-written scanners over text a Trino coordinator chose.
// Every one of them runs on the server's side of the trust boundary.
//
// The property is that no input panics. A panic here is caught at the FFI
// boundary by core's `catch_unwind`, so the blast radius is a failed ODBC call
// rather than a crashed application, but a query that cannot fail-safe on a
// value the server legitimately sent is still a defect.

/// The scalar Trino types, mirroring `TrinoTy`'s non-recursive variants.
#[derive(Arbitrary, Debug)]
enum FuzzScalar {
    // Listed first: every temporal parser reads `val.as_str()`, so these are
    // the arms where fuzzed text actually reaches a scanner.
    Date,
    Time,
    TimeWithTimeZone,
    Timestamp,
    TimestampWithTimeZone,
    IntervalYearToMonth,
    IntervalDayToSecond,
    Uuid,
    VarBinary,
    // `u16` rather than `usize`: Trino caps CHAR at 65536, and a fuzzed
    // `usize::MAX` would only ever prove that a 16-exabyte pad allocation
    // fails, which is not a defect this target is looking for.
    Char(u16),
    Decimal(u8, u8),
    Boolean,
    Int(FuzzInt),
    Float(FuzzFloat),
    Varchar,
    IpAddress,
    Json,
    Unknown,
}

#[derive(Arbitrary, Debug)]
enum FuzzInt {
    I8,
    I16,
    I32,
    I64,
}

#[derive(Arbitrary, Debug)]
enum FuzzFloat {
    F32,
    F64,
}

/// A declared type: a scalar, or one level of container around scalars.
///
/// Nesting is bounded at one level deliberately. `TrinoTy` is recursive, and a
/// derived `Arbitrary` on a recursive type spends most of its input budget
/// building depth instead of reaching the scanners. The container arms exist
/// to cover the recursion into element conversion, which one level already
/// does.
#[derive(Arbitrary, Debug)]
enum FuzzTy {
    Scalar(FuzzScalar),
    Nullable(FuzzScalar),
    Array(FuzzScalar),
    Map(FuzzScalar, FuzzScalar),
    Row(Vec<(String, FuzzScalar)>),
    Tuple(Vec<FuzzScalar>),
}

/// A JSON value. Not `serde_json::Value` directly, which is recursive and is
/// not `Arbitrary`; the leaves are flat for the same reason `FuzzTy` is.
#[derive(Arbitrary, Debug)]
enum FuzzValue {
    // First, because every scanner in this crate reads strings.
    Str(String),
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Arr(Vec<FuzzLeaf>),
    Obj(Vec<(String, FuzzLeaf)>),
}

#[derive(Arbitrary, Debug)]
enum FuzzLeaf {
    Str(String),
    Null,
    Bool(bool),
    I64(i64),
}

fn scalar_ty(s: &FuzzScalar) -> TrinoTy {
    match s {
        FuzzScalar::Date => TrinoTy::Date,
        FuzzScalar::Time => TrinoTy::Time,
        FuzzScalar::TimeWithTimeZone => TrinoTy::TimeWithTimeZone,
        FuzzScalar::Timestamp => TrinoTy::Timestamp,
        FuzzScalar::TimestampWithTimeZone => TrinoTy::TimestampWithTimeZone,
        FuzzScalar::IntervalYearToMonth => TrinoTy::IntervalYearToMonth,
        FuzzScalar::IntervalDayToSecond => TrinoTy::IntervalDayToSecond,
        FuzzScalar::Uuid => TrinoTy::Uuid,
        FuzzScalar::VarBinary => TrinoTy::VarBinary,
        FuzzScalar::Char(n) => TrinoTy::Char(*n as usize),
        FuzzScalar::Decimal(p, s) => TrinoTy::Decimal(*p as usize, *s as usize),
        FuzzScalar::Boolean => TrinoTy::Boolean,
        FuzzScalar::Int(i) => TrinoTy::TrinoInt(match i {
            FuzzInt::I8 => TrinoInt::I8,
            FuzzInt::I16 => TrinoInt::I16,
            FuzzInt::I32 => TrinoInt::I32,
            FuzzInt::I64 => TrinoInt::I64,
        }),
        FuzzScalar::Float(f) => TrinoTy::TrinoFloat(match f {
            FuzzFloat::F32 => TrinoFloat::F32,
            FuzzFloat::F64 => TrinoFloat::F64,
        }),
        FuzzScalar::Varchar => TrinoTy::Varchar,
        FuzzScalar::IpAddress => TrinoTy::IpAddress,
        FuzzScalar::Json => TrinoTy::Json,
        FuzzScalar::Unknown => TrinoTy::Unknown,
    }
}

fn trino_ty(t: &FuzzTy) -> TrinoTy {
    match t {
        FuzzTy::Scalar(s) => scalar_ty(s),
        FuzzTy::Nullable(s) => TrinoTy::Option(Box::new(scalar_ty(s))),
        FuzzTy::Array(s) => TrinoTy::Array(Box::new(scalar_ty(s))),
        FuzzTy::Map(k, v) => TrinoTy::Map(Box::new(scalar_ty(k)), Box::new(scalar_ty(v))),
        FuzzTy::Row(fields) => TrinoTy::Row(
            fields
                .iter()
                .map(|(name, s)| (name.clone(), scalar_ty(s)))
                .collect(),
        ),
        FuzzTy::Tuple(items) => TrinoTy::Tuple(items.iter().map(scalar_ty).collect()),
    }
}

fn leaf_value(l: &FuzzLeaf) -> Value {
    match l {
        FuzzLeaf::Str(s) => Value::String(s.clone()),
        FuzzLeaf::Null => Value::Null,
        FuzzLeaf::Bool(b) => Value::Bool(*b),
        FuzzLeaf::I64(n) => Value::from(*n),
    }
}

fn json_value_of(v: &FuzzValue) -> Value {
    match v {
        FuzzValue::Str(s) => Value::String(s.clone()),
        FuzzValue::Null => Value::Null,
        FuzzValue::Bool(b) => Value::Bool(*b),
        FuzzValue::I64(n) => Value::from(*n),
        // `Value::from` on a non-finite f64 yields Null, which is a legitimate
        // input rather than a case to filter: the coordinator can send one.
        FuzzValue::F64(f) => Value::from(*f),
        FuzzValue::Arr(items) => Value::Array(items.iter().map(leaf_value).collect()),
        FuzzValue::Obj(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, l)| (k.clone(), leaf_value(l)))
                .collect(),
        ),
    }
}

fuzz_target!(|input: (FuzzTy, FuzzValue)| {
    let (ty, val) = input;
    let _ = json_value(json_value_of(&val), &trino_ty(&ty));
});
