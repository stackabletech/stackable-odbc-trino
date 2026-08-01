# Stackable Trino ODBC Driver

ODBC 3.x driver for [Trino](https://trino.io/), targeting Power BI
DirectQuery and generic ODBC consumers on Linux and Windows.

## Building from source

To produce the release archives yourself, run the following from the
**repository root**:

```bash
# One-time: add the Windows cross-compilation target and the SBOM tooling
rustup target add x86_64-pc-windows-gnu
cargo install cargo-auditable
# syft: see https://github.com/anchore/syft for install options

# Build the Linux and Windows binaries
cargo auditable build --release
cargo auditable build --release --target x86_64-pc-windows-gnu

# Package into release archives (replace the version as appropriate)
VERSION=0.0.1 ./packaging/build-archives.sh
```

`cargo auditable` embeds the dependency list into each binary, and the SBOM is
generated from it. A binary built with plain `cargo build` is refused rather
than turned into an SBOM that lists a handful of components.

This produces the following in `packaging/dist/`:

- `stackable-odbc-trino-<version>-linux-x64.tar.gz` — the `.so` plus
  `install.sh`, `uninstall.sh` and the SBOM describing it
- `stackable-odbc-trino-<version>-windows-x64.zip` — the `.dll`,
  `StackableTrinoODBC.mez`, `install.bat`, `uninstall.bat`,
  `configure-dsn.ps1` and an SBOM for each of the two artefacts
- `StackableTrinoODBC-<version>.mez` (standalone, for Power BI)
- a CycloneDX (`.cdx.json`) and an SPDX (`.spdx.json`) SBOM per artefact
- `sha256sums.txt` over everything above

Verify a download with `sha256sum -c sha256sums.txt` from the directory you
downloaded into.

### The SBOM

Every archive carries the CycloneDX SBOM for what is inside it, so an offline
install has it without going back to the release page, and the SBOM records the
sha256 of the very binary shipped beside it. SPDX is published alongside the
release for tooling that asks for that format by name.

The SBOM is generated from the binary rather than from `Cargo.toml`, so it lists
what actually linked: 167 components, with development dependencies excluded by
construction. It also covers what cargo cannot see, which differs by platform.
The Linux build links unixODBC (`libodbcinst.so.2`, LGPL-2.1-or-later) at load
time. The Windows build imports only Windows' own libraries and instead carries
the mingw-w64 runtime and libgcc statically, the latter under
`GPL-3.0-or-later WITH GCC-exception-3.1`.

To install on Linux, extract and run the install script:

```bash
mkdir /tmp/trino-odbc
tar xzf stackable-odbc-trino-0.0.1-linux-x64.tar.gz -C /tmp/trino-odbc
cd /tmp/trino-odbc
sudo ./install.sh
```

On Windows, extract the `.zip` and run `install.bat` from an Administrator
Command Prompt. See the installation instructions below for details.

## Installation

> **Note:** These instructions assume you are working from an extracted
> release archive, where the driver binary sits alongside the install
> scripts. If you are working from a source checkout, build the archives
> first (see above).

### Linux (x86_64)

Requires `unixODBC` (`unixodbc` package) and root privileges for
`odbcinst` registration.

```bash
sudo ./install.sh
```

Verify with `odbcinst -q -d` — the output should include
`[stackable_odbc_trino]`.

To uninstall:

```bash
sudo ./uninstall.sh
```

If you created any DSNs, also remove them from `/etc/odbc.ini` (or
`~/.odbc.ini`).

### Windows (x86_64)

Open an **Administrator** Command Prompt (`cmd.exe`), then:

```cmd
install.bat
```

Verify with the ODBC Data Source Administrator
(`%SystemRoot%\System32\odbcad32.exe`) — the Drivers tab should list
`stackable_odbc_trino`.

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

The archive includes `StackableTrinoODBC.mez`, a Power Query custom connector
that enables Power BI DirectQuery. After running `install.bat`:

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

A DSN stores connection parameters so that users don't need the full
connection string each time. This step is optional — DSN-less connection
strings (shown below) work without it.

### Windows: the dialog

`configure-dsn.ps1` ships in the archive and covers every connection-string
option in one window:

```cmd
powershell -ExecutionPolicy Bypass -File configure-dsn.ps1
```

Secrets are written only when their **Save** box is ticked, which is off by
default. A saved secret is stored unencrypted, and a System data source puts it
in `HKLM`, where every local user can read it.

> The ODBC Data Source Administrator's own **Add** button does not work with
> this driver yet: it asks the driver's setup DLL for a dialog, gets a headless
> answer, and reports `ODBC_ERROR_INVALID_KEYWORD_VALUE`. Use the script above,
> or `odbcconf` below. Both register a DSN the Administrator then lists and
> edits normally.

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

### One gotcha in a DSN

`SessionProperties`, `ResourceEstimates`, `ExtraCredentials`, `Roles` and
`ExtraHeaders` take a `name:value;name2:value2` list. In a *connection string*
those values need `{braces}`, because `;` also separates one parameter from the
next. In a *DSN* they are stored **bare**; a braced value there fails the
connection with `08001`. `configure-dsn.ps1` handles the difference for you.

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

The full list of 34 connection options is in the
[project README](https://github.com/stackabletech/stackable-odbc-trino#connecting).

## Support

<https://github.com/stackabletech/stackable-odbc-trino>
