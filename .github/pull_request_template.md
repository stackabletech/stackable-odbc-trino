<!-- markdownlint-disable-file MD041 -->
<!--
MD041 wants a top-level heading first. This file is not a document: its
content is pasted into a pull request body, where GitHub supplies the title,
so an H1 here would only duplicate it. The rule stays on everywhere else.

Delete any section that does not apply. The checklist is a reminder, not a
gate. CI enforces what it can, and the rest is what a reviewer would otherwise
have to ask for.
-->

## What this changes

<!-- What an ODBC application sees differently after this, and why. -->

## Spec basis

<!--
For anything an application can observe (a SQLSTATE, a `SQLGetInfo` value, a
catalog result set, a type conversion), link the relevant page on Microsoft
Learn and quote the row or sentence this implements.

Two things worth stating explicitly, because both have caused rework here:
- whether a SQLSTATE's row carries a **(DM)** marker, which means the Driver
  Manager owes it and the driver must not return it; and
- what the info type's or attribute's description says its *purpose* is, which
  has decided the design more than once.
-->

## Trino behaviour

<!--
What the coordinator actually does, if this depends on it. Paste the response,
the `DESCRIBE OUTPUT` row or the error Trino returns, rather than describing
it. A measured answer is what the next reader needs, and Trino's behaviour is
not always what its documentation implies.
-->

## Checklist

- [ ] `pre-commit run --all-files` passes. It is the single source of truth for what must pass.
- [ ] `CHANGELOG.md` has an entry under `## [Unreleased]`, if an ODBC application can observe the difference. A changed SQLSTATE, a changed `SQLGetInfo` value, a new connection-string key or a different type mapping all count.
- [ ] Every client error added or moved goes through `map_trino_error`, which is the single place that decides the SQLSTATE and carries Trino's own error code through to `SQLGetDiagRec`.
- [ ] New tests were checked by breaking the line they cover and watching them fail. A test that cannot fail reports coverage that does not exist.

### If it applies

- [ ] A new connection-string key is two edits: the parser in `src/backend/types/connect_params.rs` and the table in `README.md`. The Windows dialog is generated from the parser, and a test in `src/lib.rs` fails if the two disagree.
- [ ] The integration suite was run against a live Trino: `./integration-tests/setup.sh` then `./integration-tests/run-tests.sh`. CI runs the Linux suites, so this is about anything you added to them.
- [ ] The Windows suites were run in the VM, for anything touching the Windows Driver Manager, the installer or the DSN dialog. They do not run in CI. See `integration-tests/windows/WINDOWS.md`.
- [ ] A defect found in `stackable-odbc-core` or in the Trino client was fixed where it lives, not worked around here, and this pull request says which version it needs.
- [ ] The Power BI connector was rebuilt and loaded in Power BI Desktop, for anything under `connector/`.

## Notes for the reviewer

<!--
Anything you decided rather than derived: a spec sentence you read two ways, a
case left unhandled on purpose, a Trino version whose behaviour you could not
test against.
-->
