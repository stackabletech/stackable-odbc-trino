#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use stackable_odbc_trino::fuzz_api::{
    trino_type_name_to_sql_type, type_name_precision, type_name_scale,
};

// Fuzzes the Trino type-signature parsers.
//
// These read type names as *text*, not as a parsed `TrinoTy`: they run on the
// `data_type` column of an `information_schema` query and on the rows
// `DESCRIBE INPUT` returns, both of which are strings the coordinator chose.
// The parsing is index arithmetic over `find('(')` and `rfind(')')`, which is
// the shape that goes wrong when the parentheses are not where a well-formed
// signature would put them.
//
// The generator mixes free-form strings with signature-shaped ones. Pure random
// bytes rarely contain a parenthesis pair at all, so without the shaped arms
// almost every input would exit at the first `find`.

/// A type name: free text, or something with a signature's shape.
#[derive(Arbitrary, Debug)]
enum FuzzName {
    /// Free-form, to reach the arms that take no parameter at all.
    Raw(String),
    /// `<base>(<args>)<suffix>`, assembled so the parentheses are present but
    /// their contents and ordering are the fuzzer's to choose.
    Shaped {
        base: String,
        args: String,
        suffix: String,
    },
    /// A real base name with fuzzed parameters, which is what a coordinator
    /// sends and therefore where a regression would actually be observed.
    Known {
        base: u8,
        args: String,
        suffix: String,
    },
    /// The parentheses in the wrong order. The parsers locate the argument list
    /// with `find('(')` and `rfind(')')` independently, so nothing guarantees
    /// the opening one comes first; the other arms all render a well-formed
    /// pair and can never express this.
    Reversed { base: String, args: String },
}

/// The base names this driver resolves to a specific SQL type. Anything else
/// falls through to `trino_type_name_to_sql_type`'s default arm.
const KNOWN_BASES: &[&str] = &[
    "varchar",
    "char",
    "decimal",
    "timestamp",
    "time",
    "date",
    "interval year to month",
    "interval day to second",
    "array",
    "map",
    "row",
    "varbinary",
    "json",
    "uuid",
    "ipaddress",
    "boolean",
    "integer",
    "bigint",
    "smallint",
    "tinyint",
    "real",
    "double",
];

fn render(name: &FuzzName) -> String {
    match name {
        FuzzName::Raw(s) => s.clone(),
        FuzzName::Shaped { base, args, suffix } => format!("{base}({args}){suffix}"),
        FuzzName::Known { base, args, suffix } => {
            let base = KNOWN_BASES[*base as usize % KNOWN_BASES.len()];
            format!("{base}({args}){suffix}")
        }
        FuzzName::Reversed { base, args } => format!("{base}){args}("),
    }
}

fuzz_target!(|name: FuzzName| {
    let name = render(&name);
    let _ = type_name_precision(&name);
    let _ = type_name_scale(&name);
    let _ = trino_type_name_to_sql_type(&name);
});
