#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use stackable_odbc_trino::fuzz_api::translate_escapes;

// Fuzzes ODBC escape translation with this driver's dialect.
//
// The parser itself belongs to stackable-odbc-core, which fuzzes it against its
// own dialects. What only this repo can reach is the composition: core's
// scanner calling into `escape_dialect`'s callbacks, and in particular
// `split_args`, a hand-written state machine that tracks quotes, doubled
// quotes, line comments, block comments and parenthesis depth while mapping
// character indices onto byte offsets. Index arithmetic plus a
// `saturating_sub` and a `min` clamp is exactly the code a fuzzer is for.
//
// Input is application-supplied SQL, so the trust boundary is weaker than the
// read path's, but a panic still turns a legitimate `SQLPrepareW` into a
// failure the application cannot route around.

/// One piece of SQL. Escape-shaped arms dominate on purpose: random bytes
/// essentially never produce a balanced `{fn CONVERT(x, SQL_INTEGER)}`, so a
/// `&str` target would spend its whole budget never entering the dialect.
#[derive(Arbitrary, Debug)]
enum Frag {
    /// `{fn NAME(args)}`, the arm that reaches `rewrite_scalar_fn` and
    /// `split_args`.
    Fn { name: u8, args: String },
    /// The same, with a name the fuzzer chose, to reach the fall-through path.
    FnRaw { name: String, args: String },
    /// The datetime literal escapes, which route to the render callbacks.
    Date(String),
    Time(String),
    Timestamp(String),
    /// `{escape '<char>'}`, `{oj ...}` and `{call ...}`.
    Escape(String),
    OuterJoin(String),
    Call(String),
    /// Unbalanced braces, to attack the scanner's bracket tracking.
    OpenBrace,
    CloseBrace,
    /// Quoting and comments, which `split_args` must skip over rather than
    /// treat as structure.
    SingleQuoted(String),
    DoubleQuoted(String),
    LineComment(String),
    BlockComment(String),
    /// Bare separators, so a fragment list can form argument lists and nesting
    /// without the generator having to spell them inside a string.
    Comma,
    OpenParen,
    CloseParen,
    Raw(String),
}

/// Scalar functions this dialect rewrites or remaps. Naming them explicitly is
/// what gets the fuzzer past the `remap_scalar_fn` lookup and into the arms
/// that actually reparse their arguments.
const FN_NAMES: &[&str] = &[
    "CONVERT",
    "LOCATE",
    "POSITION",
    "TRUNCATE",
    "RAND",
    "TIMESTAMPADD",
    "TIMESTAMPDIFF",
    "EXTRACT",
    "CHAR_LENGTH",
    "SUBSTRING",
    "CURDATE",
    "CURTIME",
    "NOW",
    "DAYOFWEEK",
    "IFNULL",
    "LOG10",
    "ATAN2",
    "REPEAT",
    "SPACE",
    "UCASE",
];

fn render(frags: &[Frag]) -> String {
    let mut sql = String::new();
    for frag in frags {
        match frag {
            Frag::Fn { name, args } => {
                let name = FN_NAMES[*name as usize % FN_NAMES.len()];
                sql.push_str(&format!("{{fn {name}({args})}}"));
            }
            Frag::FnRaw { name, args } => sql.push_str(&format!("{{fn {name}({args})}}")),
            Frag::Date(s) => sql.push_str(&format!("{{d '{s}'}}")),
            Frag::Time(s) => sql.push_str(&format!("{{t '{s}'}}")),
            Frag::Timestamp(s) => sql.push_str(&format!("{{ts '{s}'}}")),
            Frag::Escape(s) => sql.push_str(&format!("{{escape '{s}'}}")),
            Frag::OuterJoin(s) => sql.push_str(&format!("{{oj {s}}}")),
            Frag::Call(s) => sql.push_str(&format!("{{call {s}}}")),
            Frag::OpenBrace => sql.push('{'),
            Frag::CloseBrace => sql.push('}'),
            Frag::SingleQuoted(s) => sql.push_str(&format!("'{s}'")),
            Frag::DoubleQuoted(s) => sql.push_str(&format!("\"{s}\"")),
            Frag::LineComment(s) => sql.push_str(&format!("--{s}\n")),
            Frag::BlockComment(s) => sql.push_str(&format!("/*{s}*/")),
            Frag::Comma => sql.push(','),
            Frag::OpenParen => sql.push('('),
            Frag::CloseParen => sql.push(')'),
            Frag::Raw(s) => sql.push_str(s),
        }
    }
    sql
}

fuzz_target!(|frags: Vec<Frag>| {
    let _ = translate_escapes(&render(&frags));
});
