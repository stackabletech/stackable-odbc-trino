# Project Rules

Read and follow @AGENTS.md — it contains architecture, patterns, and procedures.

## Non-Negotiable Rules

- **ODBC spec compliance is mandatory.** Read the spec page for every function
  whose behaviour you change. The generic FFI entry points live in
  `stackable-odbc-core`, but what this driver returns from `get_info`,
  `get_info_raw`, the catalog functions and the type-conversion paths is
  directly observable by applications, and each has a spec-defined shape and
  value range. Never claim a SQLSTATE or an info value is wrong without checking
  the actual spec table first. Pay attention to **(DM)** annotations — those
  SQLSTATEs are returned by the Driver Manager, not the driver.
- **Route every client error through `map_trino_error`.** Never hand-build an
  `OdbcError` or `TrinoError` from a `trino-rust-client` error at the call site;
  that function is the single place that decides the SQLSTATE.
- **Use `odbc-sys` types** — never redefine enums, structs, or constants it
  already provides. They are re-exported from `stackable_odbc_core::types`.
- **Convert raw integers to typed enums at the boundary** — use the
  `xxx_from_raw()` functions from core, never `transmute`.
- **Run `pre-commit run --all-files`** before every commit. This is the single
  source of truth for what must pass.

## Scope

- Do not modify files outside the scope of the current task.
- Do not add features, refactoring, or "improvements" beyond what was asked.
- If unsure whether something is in scope, ask.

## Data Retrieval

Never read entire files by default. Survey, locate, then extract.

1. **Survey first** — check file size before reading (`stat -c%s file`). Files >50 KB must be sliced, not read whole. `src/backend/info.rs` and `src/ffi_integration_tests.rs` are both well over that.
2. **Navigate definitions with ctags** — run `ctags -R .` once to build a tags index, then `grep "^SymbolName" tags` to find the exact file and line of any function, struct, or trait — no file reading needed.
3. **Locate with Grep** — find patterns, keywords, or usages before reading. Use `-C` for context lines.
4. **Extract with Read (offset + limit)** — once you know the line range, read only that slice.
5. **Structured data** — use `jq` for JSON, `yq` for YAML; never read raw markup whole.
6. **Filesystem survey** — use `tree -L 2 -I '.git|target|node_modules'` instead of recursive `ls`.
7. **Verify edits with diff** — after editing, `git diff -u` to confirm changes instead of re-reading.
