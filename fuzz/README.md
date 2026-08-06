# Fuzz targets

These targets cover this driver's own parsers: the code that turns text chosen
by a Trino coordinator, or by the application, into values the driver acts on.

None of it contains `unsafe`, so AddressSanitizer is not what earns these their
keep, and that is a real difference from the targets in
`stackable-odbc-core`, which exist for the pointer marshalling. What earns
these is coverage guidance plus a structured generator. Every target below
builds its input from a grammar rather than from raw bytes, because the inputs
that reach these parsers are shaped: a random byte string is not a balanced
`{fn CONVERT(x, SQL_INTEGER)}`, is not `13:14:15+02:00`, and is not
`Host=h;Port=8443;Roles=x:y`. A `proptest` over `".*"` explores the same space
and essentially never lands in it.

The property under test is that no input panics. Panics are caught at the FFI
boundary by core's `catch_unwind`, so the blast radius is a failed ODBC call
rather than a crashed application. That is the floor, not a reason to accept
one: a value a coordinator legitimately sent must fail safe, and where a
release build has no overflow checks the same defect returns a wrong answer
instead of an error.

- `json_value` covers `json_to_column_value` and the dozen temporal, interval
  and decimal scanners under it. This is the half of the read path core does
  not see: core fuzzes `write_column_value`, which turns the resulting
  `ColumnValue` into the caller's buffer, and nothing covered the step that
  produces it.
- `type_name` covers `type_name_precision`, `type_name_scale` and
  `trino_type_name_to_sql_type`, which read Trino type signatures as text out
  of `DESCRIBE INPUT` rows and `information_schema` queries.
- `escape` covers core's escape translator driven by this crate's dialect. Core
  fuzzes the parser against its own dialects; only this repo has the Trino
  dialect, so only this repo reaches `escape_dialect::split_args`.
- `connect_params` covers the per-key value parsing layered on core's
  `ConnectParams::parse`: durations, booleans, proxy URLs, time zones, selected
  roles and four `key:value`-inside-a-value sublanguages.

The targets reach these through `stackable_odbc_trino::fuzz_api`, behind the
default-off `fuzzing` feature. The modules are private on purpose and the
feature does not reach the shipped `cdylib`; `fuzz_api`'s doc comment lists what
is exposed and why.

## Running

[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) needs nightly, because
libFuzzer does.

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run json_value
```

If cargo-fuzz fails with "sanitizer is incompatible with statically linked
libc", it picked a musl target. Pin the gnu triple explicitly, which is what CI
does:

```bash
cargo +nightly fuzz run json_value --target x86_64-unknown-linux-gnu
```

`cargo fuzz run` runs until it finds a crash or you stop it. To bound a run,
pass a libFuzzer flag after `--`:

```bash
cargo +nightly fuzz run json_value -- -max_total_time=60   # stop after 60s
cargo +nightly fuzz run json_value -- -runs=1000000        # or a run count
```

A crash writes its input to `artifacts/<target>/`. To see it as the generator's
own types rather than as bytes:

```bash
cargo +nightly fuzz fmt json_value artifacts/json_value/crash-<hash>
```

## Reading a clean run

A target that finds nothing is only as good as the inputs its generator can
express. `type_name` ran fifteen million executions without reaching a slice
that a hand-read of the code said was reachable, and the reason was the
generator: it rendered `base(args)suffix`, so the opening parenthesis always
preceded the closing one and the shape under suspicion could not be built. When
a run comes back clean, check that the generator can actually produce the input
you had in mind before concluding the code is safe.

## Workspace

This is its own Cargo workspace, so `cargo build` in the repository root does
not touch it and no `pre-commit` hook compiles it. A change to
`type_conversion`, `escape_dialect` or the connection-string parsing can break
it while every root check still passes, so build it by hand after such a change.
