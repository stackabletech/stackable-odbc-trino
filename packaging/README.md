# Stackable Trino ODBC Driver

ODBC 3.x driver for [Trino](https://trino.io/), targeting Power BI
DirectQuery and generic ODBC consumers on Linux and Windows.

## Building from source

To produce the release archives yourself, run the following from the
**repository root**:

```bash
# One-time: add the Windows cross-compilation target
rustup target add x86_64-pc-windows-gnu

# Build the Linux and Windows binaries
cargo build --release
cargo build --release --target x86_64-pc-windows-gnu

# Package into release archives (replace the version as appropriate)
VERSION=0.0.1 ./packaging/build-archives.sh
```

This produces three files in `packaging/dist/`:

- `stackable-odbc-trino-<version>-linux-x64.tar.gz`
- `stackable-odbc-trino-<version>-windows-x64.zip` (includes `StackableTrinoODBC.mez`)
- `StackableTrinoODBC-<version>.mez` (standalone)

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

On Windows (`cmd.exe`):

```cmd
odbcconf.exe /A {CONFIGDSN "stackable_odbc_trino" "DSN=Trino|Host=trino.example.com|Port=8080|User=admin|Catalog=hive|Schema=default|Protocol=http|"}
```

> **PowerShell users:** `odbcconf.exe` commands with `{...}` use `cmd.exe`
> syntax. In PowerShell, wrap the argument in single quotes:
> `odbcconf.exe /A '{CONFIGDSN ...}'`.

The DSN will appear under the **User DSN** tab in ODBC Data Source
Administrator. Note: the driver has no GUI dialog, so DSNs must be
created via `odbcconf` or the registry, not the "Add" button.

On Linux, add a section to `/etc/odbc.ini` (or `~/.odbc.ini` for a
per-user DSN):

```ini
[Trino]
Driver = stackable_odbc_trino
Host = trino.example.com
Port = 8080
User = admin
Catalog = hive
Schema = default
Protocol = http
```

## Connection string

DSN-less, HTTP:

```
Driver=stackable_odbc_trino;Host=trino.example.com;Port=8080;User=admin;Protocol=http;Catalog=hive;Schema=default
```

DSN-less, HTTPS with username/password:

```
Driver=stackable_odbc_trino;Host=trino.example.com;Port=8443;User=admin;Password=secret;Protocol=https;Catalog=hive;Schema=default
```

## Support

<https://github.com/stackabletech/stackable-odbc-trino>
