# Windows Testing

## Quick start: running tests

Start the VM and its networks first (skip if already running):

```bash
virsh --connect qemu:///system net-start stackable-odbc-test-hostnet
virsh --connect qemu:///system net-start stackable-odbc-test-internet
virsh --connect qemu:///system start stackable-odbc-test
```

Then run from the Linux host (`pywinrm` is installed automatically by `uv`).
The suite runs once per config, across four configs: DSN and DSN-less crossed
with verified and unverified TLS. Every one of them is HTTPS on port 8443,
because the stack serves nothing else. Each config records its result rather
than aborting the run, so one failure no longer hides the other three. Trino
must be running on the host via `./integration-tests/setup.sh`:

```bash
uv run --with pywinrm python3 integration-tests/windows/windows_test.py
```

Common options:

```bash
# Skip the cargo build (use an already-built DLL)
# Do not use this while diagnosing a failure — see the warning below.
uv run --with pywinrm python3 integration-tests/windows/windows_test.py --skip-build

# Target a specific VM IP (skip DHCP lease discovery)
uv run --with pywinrm python3 integration-tests/windows/windows_test.py --host 192.168.197.138

# Non-default libvirt subnet (also applies to TLS cert generation)
export ODBC_TEST_HOST_GATEWAY=10.0.0.1
# or: --gateway 10.0.0.1

# Full usage
uv run --with pywinrm python3 integration-tests/windows/windows_test.py --help
```

**Do not diagnose a Windows failure without rebuilding the DLL first.**
`--skip-build` reuses whatever sits in `target/x86_64-pc-windows-gnu/release/`,
which can predate the feature under test by days. A stale DLL once produced four
consecutive `SQL_ATTR_QUERY_TIMEOUT` failures that looked exactly like a Windows
Driver Manager defect.

### Using a different hypervisor (VirtualBox, Hyper-V, etc.)

The VM lifecycle section below uses QEMU/KVM via libvirt, and the test script
auto-discovers the VM IP from libvirt DHCP leases. If you are running Windows
in a different hypervisor, the test script still works — just pass the VM's IP
directly with `--host`:

```bash
uv run --with pywinrm python3 integration-tests/windows/windows_test.py --host <vm-ip>
```

The VM must have WinRM enabled on port 5985 with NTLM auth, and Python 3 +
pyodbc installed. Override credentials with `--user` and `--password` if
they differ from the defaults.

### OpenSSL legacy provider

WinRM uses NTLM authentication, which requires MD4 — disabled by default in
modern OpenSSL. The test script automatically sets `OPENSSL_CONF` to point at
`integration-tests/windows/openssl_legacy.cnf`, which enables the legacy provider.

If you see `unsupported hash type md4` errors, check that the file exists and
that you haven't overridden `OPENSSL_CONF` in your environment.

## VM lifecycle

### Prerequisites

QEMU/KVM and libvirt must be installed and working as system services —
`nix-shell` only provides Ansible and the Python bindings, not the
virtualisation stack itself. Verify with:

```bash
virsh --connect qemu:///system list --all
```

If this fails, install and configure QEMU/KVM + libvirt for your distro.
You will also need a `default` storage pool (`virsh pool-list`) and your
user must be in the `libvirt` group.

**Note:** QEMU typically runs as a dedicated user (e.g. `libvirt-qemu`)
that cannot read files under your home directory. If the playbook fails
with a permission error on the ISO or virtio drivers, grant read access
with ACLs (e.g. `setfacl -m u:libvirt-qemu:r /path/to/file.iso` and
`setfacl -m u:libvirt-qemu:x` on each parent directory).

For reference, on Ubuntu 24.04 the following was used to set up these
prerequisites (package names will differ on other distros):

```bash
sudo apt install -y qemu-system-x86 qemu-utils libvirt-daemon-system \
    libvirt-clients virtinst bridge-utils virt-viewer virt-manager acl
sudo adduser $USER libvirt
sudo adduser $USER kvm
# log out and back in, then verify:
virsh --connect qemu:///system list --all
# uv (Python tool runner, used by the test script):
pipx install uv
```

### Creating the VM

```bash
# Set once — point to your Windows Server 2022 evaluation ISO.
# Download from: https://www.microsoft.com/en-us/evalcenter/evaluate-windows-server-2022
export WINDOWS_ISO=~/Downloads/SERVER_EVAL_x64FRE_en-us.iso

cd integration-tests/windows/vm
nix-shell                # loads Ansible + libvirt Python bindings
ansible-playbook start.yaml -i inventory.ini
```

The playbook creates a QEMU/KVM VM with two networks (host-only +
NAT), boots the Windows ISO, and waits for the guest agent. The
`Autounattend.xml` installs Python 3.12 and pyodbc automatically.

First run takes ~30 minutes (Windows install + downloads). Use
`virt-viewer` or `virt-manager` to watch progress:

```bash
virt-viewer --connect qemu:///system stackable-odbc-test
```

### Shutting down

```bash
virsh --connect qemu:///system shutdown stackable-odbc-test
```

The VM definition and disk persist — next `start` is fast.

### Tearing down completely

Remove the VM, its disk, and the virtual networks:

```bash
virsh --connect qemu:///system destroy stackable-odbc-test
virsh --connect qemu:///system undefine stackable-odbc-test
virsh --connect qemu:///system vol-delete --pool default stackable-odbc-test.qcow2

virsh --connect qemu:///system net-destroy stackable-odbc-test-hostnet
virsh --connect qemu:///system net-destroy stackable-odbc-test-internet
virsh --connect qemu:///system net-undefine stackable-odbc-test-hostnet
virsh --connect qemu:///system net-undefine stackable-odbc-test-internet
```

## Reference: driver and DSN management

### Building the DLL

The mingw cross-compiler is the simplest option (no extra tooling needed):

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

Output: `target/x86_64-pc-windows-gnu/release/stackable_odbc_trino.dll`

Alternatively, MSVC cross-compilation works via `cargo-xwin` (requires
`cargo install cargo-xwin` and nix for LLVM):

```bash
nix-shell -p llvmPackages_18.clang llvmPackages_18.lld llvmPackages_18.llvm --run \
  "cargo xwin build --release --target x86_64-pc-windows-msvc"
```

Output: `target/x86_64-pc-windows-msvc/release/stackable_odbc_trino.dll`

Both produce DLLs that work with the Windows Driver Manager. Prefer mingw for
simplicity; use MSVC if you need to match the target environment exactly.

### Registering the driver

All commands below run in `cmd.exe` as Administrator. Adjust the DLL path as
needed.

```cmd
odbcconf.exe /A {INSTALLDRIVER "stackable_odbc_trino|Driver=C:\Users\Administrator\Downloads\stackable_odbc_trino.dll|Setup=C:\Users\Administrator\Downloads\stackable_odbc_trino.dll|"}
```

Both `Driver=` and `Setup=` must point to the same DLL — it exports both the
ODBC API functions and the `ConfigDSNW` setup entry point.

### Creating a DSN

Three ways, in descending order of convenience.

**The ODBC Data Source Administrator.** `odbcad32.exe` → **Add…** → select
`stackable_odbc_trino`, or **Configure…** on an existing data source. Both
display the driver's dialog: `ConfigDSN` reaches
`TrinoBackend::configure_dsn`, which runs `configure-dsn.ps1` with `-Emit` and
hands the keywords back for core to write. The script must be installed
alongside the DLL, which `install.bat` does.

**The dialog on its own**, which is the same WinForms dialog without the
Administrator. It writes through `SQLConfigDataSourceW`, so the driver's own
`ConfigDSN` stays in the loop:

```powershell
powershell -ExecutionPolicy Bypass -File configure-dsn.ps1
```

**`odbcconf`**, which is what the test harness uses. It passes a null
*hwndParent*, so no dialog is displayed and the keywords on the command line
are written as given:

```cmd
odbcconf.exe /A {CONFIGDSN "stackable_odbc_trino" "DSN=MyTrino|Host=trino.example.com|Port=8443|User=admin|Password=secret|Catalog=hive|Schema=default|"}
```

Note that a DSN stores the five `name:value;name2:value2` keys **bare**. Braces
belong to connection-string syntax, where `;` separates parameters; a braced
value in a DSN fails the connection with `08001`. `configure-dsn.ps1` handles
that for you.

### Connection string parameters

The keys used most often on Windows. The full list of 34 is in the
[root README](../../README.md#connecting), and the authoritative one is
`src/backend/types/connect_params.rs`.

| Parameter    | Required | Description |
|--------------|----------|-------------|
| Host         | Yes      | Trino coordinator hostname |
| Port         | Yes      | Coordinator port |
| User         | Yes¹     | Username (Basic Auth). ¹Optional under `ExternalAuthentication` |
| Password     | No       | Password (Basic Auth) |
| Catalog      | No       | Default catalog |
| Schema       | No       | Default schema |
| Protocol     | No       | `https` (default) or `http` |
| TlsVerify    | No       | `true`/`full` (default) verifies the chain and the hostname, `ca` the chain only, `false`/`none` nothing. Alias: `SSLVerification` |
| Certificate  | No       | Path to a PEM CA certificate file for server verification. Required by `ca` |
| AccessToken  | No       | JWT bearer token, sent as `Authorization: Bearer <token>`. Alias: `Token` |
| QueryTimeout | No       | Per-request HTTP timeout in seconds (default 30). ODBC alias: `LoginTimeout` |

### Verifying registration

Open `%SystemRoot%\System32\odbcad32.exe` (64-bit) and confirm:

- **Drivers tab**: `stackable_odbc_trino` is listed
- **User DSN tab**: `MyTrino` (or whatever DSN name you chose) is listed
- Selecting the driver under "Add" reports `ODBC_ERROR_INVALID_KEYWORD_VALUE`.
  That is the current expected behaviour, not a broken registration; see
  [Creating a DSN](#creating-a-dsn)

### Unregistering

Remove a DSN (User DSN entries are stored under `HKCU`):

```cmd
reg delete "HKCU\SOFTWARE\ODBC\ODBC.INI\MyTrino" /f
reg delete "HKCU\SOFTWARE\ODBC\ODBC.INI\ODBC Data Sources" /v "MyTrino" /f
```

Remove the driver (via registry, as `odbcconf` does not support `REMOVEDRIVER`):

```cmd
reg delete "HKLM\SOFTWARE\ODBC\ODBCINST.INI\stackable_odbc_trino" /f
reg delete "HKLM\SOFTWARE\ODBC\ODBCINST.INI\ODBC Drivers" /v "stackable_odbc_trino" /f
```

## Reference: manual testing

### PowerShell smoke test

PowerShell's `System.Data.Odbc` is built into .NET — no extra tools needed.
This example is self-contained: it uses an inline `VALUES` list, so it needs no
table and no writable catalog (Trino's DDL support depends on which connector
backs the catalog). The driver must be registered first (done automatically by
the test script), and Trino must be reachable at the given host.

The compose stack serves HTTPS on 8443 and nothing else, and its certificate
comes from a CA no machine trusts by default, so `TlsVerify=false` is what makes
a hand-typed connection work against it. Point at a coordinator with a real
certificate and it can come off.

```powershell
$conn = New-Object System.Data.Odbc.OdbcConnection("Driver=stackable_odbc_trino;Host=<trino-host>;Port=8443;User=admin;Password=admin;Protocol=https;TlsVerify=false")
$conn.Open()
Write-Host "Connected: $($conn.State)"

$cmd = $conn.CreateCommand()
$cmd.CommandText = "SELECT * FROM (VALUES (1, 'Alice', 75000.50), (2, 'Bob', 62000.00)) AS t(id, name, value)"
$reader = $cmd.ExecuteReader()
while ($reader.Read()) {
    Write-Host "$($reader[0]) | $($reader[1]) | $($reader[2])"
}
$reader.Close()

$conn.Close()
Write-Host "Done"
```

Expected output:

```text
Connected: Open
1 | Alice | 75000.50
2 | Bob | 62000.00
Done
```

**DSN-based connection:**

The automated test script registers a DSN named `test_trino`. To create one
yourself (in `cmd.exe`, not PowerShell):

```cmd
odbcconf.exe /A {CONFIGDSN "stackable_odbc_trino" "DSN=MyTrino|Host=<trino-host>|Port=8443|User=admin|Password=admin|Protocol=https|TlsVerify=false|"}
```

Then in PowerShell:

```powershell
$c = New-Object System.Data.Odbc.OdbcConnection("DSN=MyTrino"); $c.Open(); Write-Host "Connected: $($c.State)"; $c.Close()
```

### Running test_integration.py manually

If you need to run the tests without the wrapper script (e.g. from a
PowerShell session on the VM):

```powershell
& "C:\Program Files\Python312\python.exe" C:\odbc_test\test_integration.py "Driver=stackable_odbc_trino;Host=<trino-host>;Port=8443;User=admin;Password=admin;Protocol=https;TlsVerify=false;Catalog=tpcds"
```
