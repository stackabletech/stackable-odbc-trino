# Integration tests

Everything needed to run the Trino ODBC driver against a real coordinator.

```bash
./integration-tests/setup.sh          # start the stack, generate config, build the driver
./integration-tests/run-tests.sh      # run the suites, then tear the stack down
./integration-tests/scripts/teardown.sh
```

`run-tests.sh` calls `setup.sh` itself if the stack has not been set up.

## Layout

| Directory | Holds |
|---|---|
| `scripts/` | All the bash. `lib.sh` is sourced by the rest and owns the paths, the profile parsing and the readiness helpers. |
| `stack/` | All the docker material: `compose.yaml`, the Trino config fragments, the Postgres init SQL. |
| `suites/` | All the Python. `harness.py` is shared; every `test_*.py` is a suite. |
| `perf/` | Profiling tooling, deliberately not named `profiling/` so it never collides with compose profiles. |
| `windows/` | The Windows VM harness and its libvirt definitions. See [WINDOWS.md](windows/WINDOWS.md). |
| `generated/` | Every produced artefact: certificates, secrets, the assembled Trino config, the ODBC ini files, `stack.env`. Gitignored, and safe to delete. |

## Profiles

The unprofiled set is the core stack. Everything heavier is opt-in.

| Profile | Services added | Buys |
|---|---|---|
| *(none)* | `postgres`, `trino` | The `tpcds`, `postgresql` and `hive` catalogs, HTTPS, password and client-certificate auth, transactions, a non-empty `SQLTablePrivileges` |
| `oauth` | `keycloak` | The OAuth 2.0 flow, through `suites/test_oauth.py` |
| `spooling` | `minio`, `minio-init` | The spooling protocol, server side |

```bash
./integration-tests/setup.sh --profile oauth
./integration-tests/setup.sh --profile oauth,spooling
PROFILES=all ./integration-tests/setup.sh
```

Changing profiles recreates the coordinator, because its configuration is
assembled per profile rather than mounted from the checkout. A suite whose
profile is not active is skipped and says which profile would enable it.

The `hive` catalog is in the base stack rather than behind a profile because it
costs no container: a file metastore on a named volume needs neither a metastore
service nor object storage. It is the only connector Trino ships that accepts
writes outside autocommit, which is what makes a transaction rollback
observable, and `hive.security=sql-standard` is what fills
`information_schema.table_privileges`.

`keycloak` is configured, and its realm is imported from
`stack/keycloak/realm-trino.json` by `scripts/gen-keycloak-config.sh`. `minio` and
`minio-init` are configured, and the coordinator spools to the bucket the init
container creates.

**No suite drives spooling.** The coordinator produces spooled segments, and the
driver cannot yet read them: `src/backend/execute.rs` pages with `client.get` and
`client.get_next`, which cannot decode a spooled segment. The profile exists so
that work has something to be written against.

## Flags

| Flag | Effect |
|---|---|
| `--profile <list>` | `setup.sh`: comma or space separated, or `all` |
| `--suite <substring>` | `run-tests.sh`: run only matching suites |
| `--skip-build` | Skip the cargo build |
| `--skip-delete` | Leave the stack running afterwards |
| `--windows` | Also run the Windows VM suite |

## The stack is HTTPS only

The coordinator serves 8443 and nothing else. `Protocol=http` is still a
supported connection-string value but has no integration coverage here; its
parsing is unit-tested.

## Certificates

`scripts/gen-certs.sh` builds one CA into `generated/certs/` and signs the
coordinator, client and Keycloak leaves from it.

Jetty selects the certificate on SNI and serves Trino's internal self-signed
certificate for anything it cannot match, so a name the coordinator's
certificate does not carry gets a *different certificate* rather than a
hostname mismatch, and an IP address gets one every time because TLS sends no
SNI for an IP literal. `suites/test_tls.py` documents what that rules out, and
`windows/windows_test.py` works around it with a hosts entry in the VM.

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

`isql` connects through `SQLConnect`, which carries no *DriverCompletion*, so the
driver is allowed to prompt and a real browser opens on Keycloak's login page. It
will warn about the test CA, which is expected: `scripts/gen-certs.sh` generates
that CA and no browser trusts it. The credentials are the `KEYCLOAK_USER` and
`KEYCLOAK_PASSWORD` values in `generated/stack.env`.

pyodbc cannot do this. It passes `SQL_DRIVER_NOPROMPT` unconditionally, so the
driver refuses the connection; `suites/test_oauth.py` uses `ctypes` for that
reason.
