# Stackable Trino ODBC Driver

ODBC 3.x driver for [Trino](https://trino.io/), targeting Power BI
DirectQuery and generic ODBC consumers on Linux and Windows.

This file ships inside both release archives. If you have just extracted one,
start at [Installation](#installation).

## What is in the archive

`stackable-odbc-trino-<version>-linux-x64.tar.gz`:

| File | Purpose |
|------|---------|
| `libstackable_odbc_trino.so` | The driver |
| `install.sh`, `uninstall.sh` | Registration with unixODBC |
| `libstackable_odbc_trino.so.cdx.json` | CycloneDX SBOM for the driver |
| `README.md`, `LICENSE` | This file, and Apache-2.0 |

`stackable-odbc-trino-<version>-windows-x64.zip`:

| File | Purpose |
|------|---------|
| `stackable_odbc_trino.dll` | The driver |
| `install.bat`, `uninstall.bat` | Registration with the Windows Driver Manager |
| `configure-dsn.ps1` | The data source dialog |
| `StackableTrinoODBC.mez` | Power Query custom connector for Power BI |
| `stackable_odbc_trino.dll.cdx.json` | CycloneDX SBOM for the driver |
| `StackableTrinoODBC.mez.cdx.json` | CycloneDX SBOM for the connector |
| `README.md`, `LICENSE` | This file, and Apache-2.0 |

The connector is also published on its own, as
`StackableTrinoODBC-<version>.mez`.

The release page carries `sha256sums.txt` over every published file. Verify a
download with `sha256sum -c sha256sums.txt`, run from the directory you
downloaded into.

## Installation

### Linux (x86_64)

Requires `unixODBC` (the `unixodbc` package) and root privileges for
`odbcinst` registration.

```bash
mkdir /tmp/trino-odbc
tar xzf stackable-odbc-trino-<version>-linux-x64.tar.gz -C /tmp/trino-odbc
cd /tmp/trino-odbc
sudo ./install.sh
```

Verify with `odbcinst -q -d`. The output should include
`[stackable_odbc_trino]`.

To uninstall:

```bash
sudo ./uninstall.sh
```

If you created any DSNs, also remove them from `/etc/odbc.ini` (or
`~/.odbc.ini`).

### Windows (x86_64)

Extract the `.zip`, open an **Administrator** Command Prompt (`cmd.exe`) in
the extracted folder, then:

```cmd
install.bat
```

This copies the driver and `configure-dsn.ps1` to
`%ProgramFiles%\Stackable\ODBC` and registers the driver. Verify with the ODBC
Data Source Administrator (`%SystemRoot%\System32\odbcad32.exe`). The Drivers
tab should list `stackable_odbc_trino`.

To uninstall:

```cmd
uninstall.bat
```

If you created any DSNs, also remove them via the registry:

```cmd
reg delete "HKCU\SOFTWARE\ODBC\ODBC.INI\YourDsnName" /f
reg delete "HKCU\SOFTWARE\ODBC\ODBC.INI\ODBC Data Sources" /v "YourDsnName" /f
```

### Power BI custom connector (Windows only)

`StackableTrinoODBC.mez` is a Power Query custom connector that gives Trino its
own entry in the **Get Data** dialog and enables DirectQuery. Install the
driver first, then:

1. Copy `StackableTrinoODBC.mez` to the Custom Connectors folder (create it if
   it does not exist):

   ```cmd
   mkdir "%USERPROFILE%\Documents\Power BI Desktop\Custom Connectors"
   copy StackableTrinoODBC.mez "%USERPROFILE%\Documents\Power BI Desktop\Custom Connectors\"
   ```

2. Open Power BI Desktop → **File** → **Options and settings** →
   **Options** → **Security** → **Data Extensions** → select
   **Allow any extension to load without validation or warning**
3. Restart Power BI Desktop
4. **Get Data** → **More** → **Database** → **Stackable Trino**

## Create a DSN (optional)

A DSN stores connection parameters under a name, so an application can pick it
from a list instead of asking for a full connection string. It is optional:
the DSN-less connection strings below work without one.

### Windows: the dialog

Open the **ODBC Data Source Administrator** (`odbcad32.exe`), press **Add…**
and choose `stackable_odbc_trino`. The driver's dialog covers every
connection-string option in one window, and **Configure…** reopens it on an
existing data source.

The same dialog runs on its own, without the Administrator:

```cmd
powershell -ExecutionPolicy Bypass -File "%ProgramFiles%\Stackable\ODBC\configure-dsn.ps1"
```

`install.bat` puts `configure-dsn.ps1` beside the driver DLL. The
Administrator's buttons need it there, so do not move it.

Secrets are written only when their **Save** box is ticked, which is off by
default. A saved secret is stored unencrypted, and a System data source puts it
in `HKLM`, where every local user can read it.

**Test connection** is unavailable for **External authentication**, because
testing connects in a way that forbids the driver from opening a login page.
The dialog says so rather than reporting a connection failure. Save the data
source and use it from your application, which opens a browser when it
connects.

### Windows: scripted

```cmd
odbcconf.exe /A {CONFIGDSN "stackable_odbc_trino" "DSN=Trino|Host=trino.example.com|Port=8443|User=admin|Password=secret|Catalog=hive|Schema=default|"}
```

> **PowerShell users:** `odbcconf.exe` commands with `{...}` use `cmd.exe`
> syntax. In PowerShell, wrap the argument in single quotes:
> `odbcconf.exe /A '{CONFIGDSN ...}'`.

The DSN appears under the **User DSN** tab in ODBC Data Source Administrator.

### Linux

Add a section to `/etc/odbc.ini` (or `~/.odbc.ini` for a per-user DSN):

```ini
[Trino]
Driver = stackable_odbc_trino
Host = trino.example.com
Port = 8443
User = admin
Password = secret
Catalog = hive
Schema = default
```

## Connection string

DSN-less, HTTPS with username and password. `Protocol` defaults to `https`, so
it can be left out:

```text
Driver=stackable_odbc_trino;Host=trino.example.com;Port=8443;User=admin;Password=secret;Catalog=hive;Schema=default
```

DSN-less, plaintext HTTP:

```text
Driver=stackable_odbc_trino;Host=trino.example.com;Port=8080;Protocol=http;User=admin;Catalog=hive;Schema=default
```

Every connection option is listed in the
[project README](https://github.com/stackabletech/stackable-odbc-trino#connecting).
Five of them take a `name:value;name2:value2` list and need `{braces}` in a
connection string but not in a DSN; see
[Values that contain a semicolon](https://github.com/stackabletech/stackable-odbc-trino#values-that-contain-a-semicolon).
The Windows dialog handles that difference for you.

## Support

- [Issues](https://github.com/stackabletech/stackable-odbc-trino/issues) for
  bugs and feature requests
- [Discussions](https://github.com/orgs/stackabletech/discussions) for questions
- [Discord](https://discord.gg/7kZ3BNnCAF) to talk to us

For a connection or query problem, attach a driver log. Set `ODBC_LOG_LEVEL` to
`debug` and `ODBC_LOG_FILE` to a writable path in the environment of the
application that loads the driver, then reproduce the problem. Logging is off
unless `ODBC_LOG_LEVEL` is set.

## The SBOM

Each archive carries the CycloneDX SBOM for what is inside it, so an offline
install has it without going back to the release page. The SBOM records the
sha256 of the binary shipped beside it. SPDX is published alongside the release,
for tooling that asks for that format by name.

It is generated from the binary rather than from `Cargo.toml`, so it lists what
linked, with development dependencies excluded by construction. It also covers
what cargo cannot see: the Linux build links unixODBC at load time, and the
Windows build carries the mingw-w64 runtime and libgcc statically. Those
components, with their licences, are declared in
[`packaging/sbom-native.json`](https://github.com/stackabletech/stackable-odbc-trino/blob/main/packaging/sbom-native.json).

## Building the release archives

Everything below is for people building the driver themselves. Set up the
compiler and the unixODBC development libraries first, following
[CONTRIBUTING.md](https://github.com/stackabletech/stackable-odbc-trino/blob/main/CONTRIBUTING.md).

From the **repository root**:

```bash
# One-time: the Windows cross-compilation target and the SBOM tooling.
# Both tools are version-pinned to what .github/workflows/release.yaml installs,
# because packaging/test-sbom.sh asserts the shape of what syft emits and how
# cargo-auditable's .dep-v0 section reads. A different version of either can
# produce a different SBOM from the same binary.
rustup target add x86_64-pc-windows-gnu
cargo install cargo-auditable@0.7.5
# syft v1.50.0: see https://github.com/anchore/syft for install options

# Build the Linux and Windows binaries.
# --locked, as release.yaml uses: it builds against the versions Cargo.lock
# pins, so the SBOM describes the dependency set that ships rather than
# whatever resolved today.
cargo auditable build --locked --release
cargo auditable build --locked --release --target x86_64-pc-windows-gnu

# Package into release archives. The version comes from Cargo.toml, which is
# also what the DLL's version resource and the connector's .pq carry, so there
# is nothing to pass and nothing to keep in step. Setting VERSION to anything
# other than that version is refused rather than producing an archive whose name
# disagrees with the driver inside it. To release a new version, bump all three
# together with release/release.sh.
./packaging/build-archives.sh
```

`cargo auditable` is required, not a preference: it embeds the dependency list
that the SBOM is generated from, and `packaging/sbom.sh` refuses a binary
without it.

The result, in `packaging/dist/`:

- `stackable-odbc-trino-<version>-linux-x64.tar.gz`
- `stackable-odbc-trino-<version>-windows-x64.zip`
- `StackableTrinoODBC-<version>.mez`, the standalone connector
- a CycloneDX (`.cdx.json`) and an SPDX (`.spdx.json`) SBOM per artefact
- `sha256sums.txt` over everything above

`./packaging/test-sbom.sh` runs every SBOM assertion against the real
artefacts, including the component count and the per-platform native
components. It needs no running Trino.
