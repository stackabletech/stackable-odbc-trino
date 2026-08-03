# Integration tests

Everything needed to run the Trino ODBC driver against a real coordinator.
This file is the runbook. Why the stack is shaped the way it is (the `hive`
catalog, the spooling suite, what SNI rules out) is in
[AGENTS.md](../AGENTS.md#testing).

```bash
./integration-tests/setup.sh          # start the stack, generate config, build the driver
./integration-tests/run-tests.sh      # run the suites, then tear the stack down
./integration-tests/scripts/teardown.sh
```

`run-tests.sh` calls `setup.sh` itself if the stack has not been set up.

The coordinator serves HTTPS on 8443 and nothing else, so every connection
here is TLS. `Protocol=http` is a supported connection-string value with no
coverage in this stack; its parsing is unit-tested.

## Layout

| Directory | Holds |
|---|---|
| `scripts/` | All the bash. `lib.sh` is sourced by the rest and owns the paths, the profile parsing and the readiness helpers. |
| `stack/` | All the docker material: `compose.yaml`, the Trino config fragments, the Postgres init SQL. |
| `suites/` | All the Python. `harness.py` is shared; every `test_*.py` is a suite; `registry.py` is the list of them, read by both runners. |
| `perf/` | Profiling and stress tooling. `profile_stress.sh` runs `test_stress.py`'s BI-shaped queries with the driver's profiling output on, and `parse_profile.py` renders the log it writes as a per-query table. |
| `windows/` | The Windows VM harness and its libvirt definitions. See [WINDOWS.md](windows/WINDOWS.md). |
| `generated/` | Every produced artefact: certificates, secrets, the assembled Trino config, the ODBC ini files, `stack.env`. Gitignored, and safe to delete. |

## Profiles

The unprofiled set is the core stack. Everything heavier is opt-in.

| Profile | Services added | Buys |
|---|---|---|
| *(none)* | `postgres`, `trino` | The `tpcds`, `postgresql` and `hive` catalogs, HTTPS, password and client-certificate auth, transactions, a non-empty `SQLTablePrivileges` |
| `oauth` | `keycloak` | The OAuth 2.0 flow, through `suites/test_oauth.py` |
| `spooling` | `minio`, `minio-init` | The spooling protocol end to end, through `suites/test_spooling.py` |

```bash
./integration-tests/setup.sh --profile oauth
./integration-tests/setup.sh --profile oauth,spooling
PROFILES=all ./integration-tests/setup.sh
```

Changing profiles recreates the coordinator, because its configuration is
assembled per profile rather than mounted from the checkout. A suite whose
profile is not active is skipped and says which profile would enable it.

Two suites need no profile and assert something in either stack state:
`test_spooling.py` drives the spooled protocol when `spooling` is active and
the inline fallback when it is not. `test_transactions.py` writes to the
`hive` catalog, which is
[part of the core stack](../AGENTS.md#the-hive-catalog-and-why-it-is-not-behind-a-profile).

## Flags

| Flag | Script | Effect |
|---|---|---|
| `--profile <list>` | `setup.sh` | Comma or space separated, or `all`. `--profile=<list>` and the `PROFILES` environment variable do the same |
| `--suite <substring>` | `run-tests.sh` | Run only the suites whose name contains the substring |
| `--skip-build` | `run-tests.sh` | Skip the cargo build |
| `--skip-delete` | `run-tests.sh` | Leave the stack running afterwards |
| `--windows` | `run-tests.sh` | Also run the suites on the Windows VM |

`setup.sh` rejects any argument it does not recognise. `run-tests.sh` forwards
the ones it does not recognise to `windows/windows_test.py`, so that script's
flags can be passed straight through. `--suite` is forwarded too, so a filtered
run filters both platforms rather than silently meaning "on Linux only".

## The suite registry

`suites/registry.py` is the one list of suites. `scripts/run-tests.sh` reads it
through `registry.py --bash`, and `windows/windows_test.py` imports it. Adding a
suite means adding an entry there, and nothing else.

Each entry states the suite's script, the profile it needs, how it takes its
configuration, whether it needs `pyodbc`, and whether it runs on Windows. A
suite that does not run on Windows carries the reason in its own entry, and the
runner prints it as a `SKIP`, so an unrun suite is never printable as a passing
one. `test_harness.py` checks what can be checked without a stack: that every
script and deployed file exists, that names are unique, and that a suite excluded
from Windows gives a reason.

What the registry deliberately does not hold is the command. The two runners
invoke Python differently, and the four-configuration connect matrix is not the
same on both: Linux crosses DSN names out of the generated `odbc.ini`, while
Windows crosses a registry-registered DSN and connects by address for the
unverified cases so that no SNI is sent. Those are two matrices that happen to
share four labels.

## Certificates

`scripts/gen-certs.sh` builds one CA into `generated/certs/` and signs the
coordinator, client and Keycloak leaves from it.

Jetty picks its certificate from the SNI the client sends, so a connection
that verifies has to use a name the coordinator's certificate carries. An IP
address sends no SNI and gets Trino's internal self-signed certificate
instead. [AGENTS.md](../AGENTS.md#certificates) has the measured table,
`suites/test_tls.py` records what it rules out, and `windows/windows_test.py`
works around it with a hosts entry in the VM.

## Interactive use

```bash
export ODBCSYSINI=$(pwd)/integration-tests/generated
export ODBCINI=$(pwd)/integration-tests/generated/odbc.ini
isql -3 trino_https -v
```

DSNs: `trino_https`, `trino_https_verify_false`, `trino_postgresql`,
`trino_oauth`.

```bash
docker compose -f integration-tests/stack/compose.yaml logs -f trino
```

### An interactive OAuth 2.0 login

Needs the `oauth` profile and the `trino_oauth` DSN:

```bash
./integration-tests/setup.sh --profile oauth
isql -3 trino_oauth -v
```

`isql` connects through `SQLConnect`, which carries no *DriverCompletion*, so
the driver is allowed to prompt and a real browser opens on Keycloak's login
page. It will warn about the test CA, which `scripts/gen-certs.sh` generates
and no browser trusts. The credentials are the `KEYCLOAK_USER` and
`KEYCLOAK_PASSWORD` values in `generated/stack.env`.

pyodbc cannot do this. It passes `SQL_DRIVER_NOPROMPT` unconditionally, so the
driver refuses the connection; `suites/test_oauth.py` uses `ctypes` for that
reason.
