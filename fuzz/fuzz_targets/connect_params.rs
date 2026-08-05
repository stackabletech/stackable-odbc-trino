#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use stackable_odbc_trino::fuzz_api::trino_connect_params;

// Fuzzes connection-string handling: core splits the string into keys, then
// this driver reads the values it recognises.
//
// core already proptests `ConnectParams::parse` for panic-freedom, so the half
// under test here is the second one: the per-key value parsers, which are this
// crate's. They parse durations, booleans, proxy URLs, time zones, selected
// roles and four separate `key:value`-inside-a-value sublanguages.
//
// The input is user-supplied through a DSN rather than remote, so a panic here
// is less severe than one on the read path. It is also the cheapest of the four
// targets to run, because the surface is a pure string-to-Result function.
//
// The error is rendered to a String on the way out, so this covers the
// `Display` formatting too: several error arms interpolate the offending value.

/// One `key=value` pair.
#[derive(Arbitrary, Debug)]
enum Pair {
    /// A key this driver knows, with a fuzzed value. This is the arm that
    /// reaches the value parsers; a random key would only ever be ignored.
    Known { key: u8, value: String },
    /// A key the driver does not know, to cover the unrecognised-key handling.
    Unknown { key: String, value: String },
    /// A value wrapped in braces, which is how the connection-string grammar
    /// carries a value containing `;` or `=`.
    Braced { key: u8, value: String },
}

/// The driver's connection-string keys, from `backend::types::connect_params`.
/// Host and Port are omitted here and always prepended, so an input does not
/// have to rediscover them to get past the required-parameter checks.
const KEYS: &[&str] = &[
    "protocol",
    "tlsverify",
    "sslverification",
    "certificate",
    "clientcertificate",
    "querytimeout",
    "logintimeout",
    "catalog",
    "schema",
    "source",
    "clienttags",
    "accesstoken",
    "token",
    "sessionproperties",
    "extracredentials",
    "resourceestimates",
    "path",
    "clientinfo",
    "tracetoken",
    "proxy",
    "proxyuser",
    "proxypassword",
    "extraheaders",
    "clientcapabilities",
    "timezone",
    "roles",
    "sessionuser",
    "locale",
    "disablecompression",
    "maxattempts",
    "encoding",
    "externalauthentication",
    "externalauthenticationtimeout",
];

#[derive(Arbitrary, Debug)]
struct Input {
    /// Prepended so most inputs get past `MissingParam` and reach the value
    /// parsing. Fuzzed rather than fixed, because the port parse is itself one
    /// of the parsers under test.
    host: String,
    port: String,
    user: String,
    pairs: Vec<Pair>,
    /// Appended verbatim, so the fuzzer can still explore the raw grammar:
    /// stray semicolons, unbalanced braces, embedded nulls.
    tail: String,
}

fn render(input: &Input) -> String {
    let Input {
        host,
        port,
        user,
        pairs,
        tail,
    } = input;
    let mut s = format!("Host={host};Port={port};UID={user};");
    for pair in pairs {
        match pair {
            Pair::Known { key, value } => {
                let key = KEYS[*key as usize % KEYS.len()];
                s.push_str(&format!("{key}={value};"));
            }
            Pair::Unknown { key, value } => s.push_str(&format!("{key}={value};")),
            Pair::Braced { key, value } => {
                let key = KEYS[*key as usize % KEYS.len()];
                s.push_str(&format!("{key}={{{value}}};"));
            }
        }
    }
    s.push_str(tail);
    s
}

fuzz_target!(|input: Input| {
    let _ = trino_connect_params(&render(&input));
});
