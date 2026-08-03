# Contributing

Thanks for considering a contribution. Bug reports, connection strings that
fail, and reports of a tool that will not talk to the driver are all useful.

- **Questions and ideas:** [GitHub Discussions](https://github.com/orgs/stackabletech/discussions)
  or [Discord](https://discord.gg/7kZ3BNnCAF).
- **Bugs:** open an issue. Please say which platform, which Driver Manager
  (unixODBC or the Windows one), which application, and which Trino version.
  A driver log helps most of all: set `ODBC_LOG_FILE` and `ODBC_LOG_LEVEL=debug`
  and attach the result, with any passwords removed.
- **Security problems:** do not open an issue. See [SECURITY.md](SECURITY.md).

## Building

You need the unixODBC development libraries, because the ODBC bindings link
against them. You do not need a running Trino or any ODBC configuration to
build and run the unit tests.

```bash
sudo apt-get install unixodbc-dev   # Debian/Ubuntu
```

```bash
git clone https://github.com/stackabletech/stackable-odbc-trino
cd stackable-odbc-trino
cargo build --release
```

That produces `target/release/libstackable_odbc_trino.so`.

Everything generic about being an ODBC driver lives in
[`stackable-odbc-core`](https://github.com/stackabletech/stackable-odbc-core):
handle management, UTF-16 marshalling, diagnostics, panic safety and the
exported C entry points. This repository holds only the Trino-specific half.
Cargo fetches core for you, so there is nothing to clone by hand.

### Working on core at the same time

To build against a local checkout of core rather than the fetched one, add a
`[patch]` to your own `.cargo/config.toml`, which is not checked in:

```toml
[patch."https://github.com/stackabletech/stackable-odbc-core.git"]
stackable-odbc-core = { path = "../stackable-odbc-core" }
```

`.cargo/` is gitignored, so the override cannot be committed. **`Cargo.lock`
can**: cargo rewrites core's entry to the local path while the patch is active,
so check `git status` before committing. Remove the override, or push your core
changes, before you rely on a build.

The toolchain version is pinned in `rust-toolchain.toml`, so rustup will fetch
the right one on first build.

### Windows

Cross-compile with MinGW (`gcc-mingw-w64-x86-64`):

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

That produces `target/x86_64-pc-windows-gnu/release/stackable_odbc_trino.dll`.

Anything destined for a release archive is built with
[`cargo auditable`](https://github.com/rust-secure-code/cargo-auditable), which
embeds the dependency list the SBOM is generated from. See
[`packaging/README.md`](packaging/README.md).

## Testing

```bash
cargo test                                   # unit and FFI tests; no server needed
cargo clippy --all-targets -- -D warnings
```

`cargo test` must produce zero warnings. Tests that need a live Trino are
marked `#[ignore]`, so a bare `cargo test` stays self-contained.

The integration suite runs against a real Trino in Docker. It does not run in
CI, so run it locally before a release:

```bash
./integration-tests/setup.sh       # start the stack, build the driver, write ODBC config
./integration-tests/run-tests.sh   # run the suites, then tear the stack down
```

See [`integration-tests/README.md`](integration-tests/README.md) for the flags,
the optional compose profiles, and how to get an interactive session against the
running stack. The Windows suites run the same tests through the Windows Driver
Manager in a VM; see
[`integration-tests/windows/WINDOWS.md`](integration-tests/windows/WINDOWS.md).

## Before you commit

```bash
pre-commit run --all-files
```

That is the gate, and it is the single source of truth for what must pass. It
runs rustfmt, clippy, `cargo test`, shellcheck and markdownlint.

Two more things a change usually needs:

- **A changelog entry**, under `## [Unreleased]` in
  [`CHANGELOG.md`](CHANGELOG.md), if an ODBC application can observe the
  difference. A changed SQLSTATE, a changed `SQLGetInfo` value, a new
  connection-string key or a different type mapping all count.
- **A new connection-string key means two edits**: the parser in
  `src/backend/types/connect_params.rs`, and the table in
  [`README.md`](README.md). The Windows dialog is generated from the parser, and
  a test in `src/lib.rs` fails if the two disagree.

## Where things live

[`AGENTS.md`](AGENTS.md) is the working reference: module layout, the split
against `stackable-odbc-core`, the error-mapping rules, and the measured Trino
and Driver Manager behaviour behind the design decisions. Read the section that
covers whatever you are about to change. It is written for AI coding agents and
human contributors alike.

Two rules are worth stating here, because they are the ones most easily broken
by a reasonable-looking change:

- **Read the ODBC spec page for any function whose behaviour you change.** What
  the driver returns from `SQLGetInfo`, from the catalog functions and from the
  type-conversion paths is directly observable by applications, and each has a
  spec-defined shape and value range.
- **Route every client error through `map_trino_error`.** It is the single place
  that decides the SQLSTATE and carries Trino's own error code through to
  `SQLGetDiagRec`. Building an error at the call site quietly degrades it.

## License

By contributing you agree that your contribution is licensed under
[Apache-2.0](LICENSE).
