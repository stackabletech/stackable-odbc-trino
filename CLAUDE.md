# Project Rules

Read and follow @AGENTS.md. It holds the architecture, the patterns and the
procedures, and it is where the reasoning behind every rule below lives.

## Non-Negotiable Rules

- **ODBC spec compliance is mandatory.** Read the spec page for every function
  whose behaviour you change. The generic FFI entry points live in
  `stackable-odbc-core`, but what this driver returns from `get_info`,
  `get_info_raw`, the catalog functions and the type-conversion paths is
  directly observable by applications, and each has a spec-defined shape and
  value range. Never claim a SQLSTATE or an info value is wrong without
  checking the actual spec table first. Pay attention to **(DM)** annotations:
  those SQLSTATEs are returned by the Driver Manager, not the driver.
- **Route every client error through `map_trino_error`.** It is the single
  place that decides the SQLSTATE. See
  [Backend error mapping](AGENTS.md#backend-error-mapping).
- **Use `odbc-sys` types**, re-exported from `stackable_odbc_core::types`.
  Never redefine what it provides, and never add an `odbc-sys` dependency to
  this crate's `Cargo.toml`. See [Named constants](AGENTS.md#named-constants).
- **Convert raw integers to typed enums at the boundary** with core's
  `xxx_from_raw()` functions, never `transmute`.
- **Never work around a defect or a gap in `stackable-odbc-core` or
  `trino-rust-client`.** Fix the cause where it lives and adapt this driver to
  the corrected API.
- **Run `pre-commit run --all-files` before every commit.** It is the single
  source of truth for what must pass.

## Scope

- Do not modify files outside the scope of the current task.
- Do not add features, refactoring, or "improvements" beyond what was asked.
- If unsure whether something is in scope, ask.

## Data Retrieval

Never read entire files by default. Survey, locate, then extract.

1. Survey first. Check the file size with `stat -c%s file` before reading it.
   Anything over 50 KB must be sliced, not read whole; several modules in
   `src/` are.
2. Navigate definitions with ctags. Run `ctags -R .` once to build the index,
   then `grep "^SymbolName" tags` for the exact file and line of any function,
   struct or trait. No file reading needed.
3. Locate with Grep. Find patterns, keywords or usages before reading. Use `-C`
   for context lines.
4. Extract with Read, using `offset` and `limit` once you know the line range.
5. Read structured data with a tool that understands it: `jq` for JSON, `yq`
   for YAML. Never read raw markup whole.
6. Survey the filesystem with `tree -L 2 -I '.git|target|node_modules'`, not a
   recursive `ls`.
7. Verify edits with `git diff -u` rather than re-reading the file.
