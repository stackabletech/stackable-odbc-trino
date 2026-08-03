# Windows testing

`suites/test_integration.py`, driven through the Windows ODBC Driver Manager
over WinRM, plus a GUI check on the driver's setup dialog. The target is a
disposable Windows Server VM on a host-only libvirt network, created by the
Ansible playbook in `vm/`.

The VM's credentials are `Administrator` / `Asdf1234`, the defaults in
`windows_test.py` and `dsn_dialog_test.py`. They are not a secret: the machine
is local, throwaway, and reachable only from the host that created it. Pass
`--user` and `--password` for a VM built some other way.

## Quick start: running the tests

If the VM does not exist yet, build it first: [Prerequisites](#prerequisites),
then [Creating the VM](#creating-the-vm).

Trino must be running on the host via `./integration-tests/setup.sh`. Start
the VM and its networks (skip whatever is already running):

```bash
virsh --connect qemu:///system net-start stackable-odbc-test-hostnet
virsh --connect qemu:///system net-start stackable-odbc-test-internet
virsh --connect qemu:///system start stackable-odbc-test
```

Then run from the Linux host. `uv` installs `pywinrm` itself:

```bash
uv run --with pywinrm python3 integration-tests/windows/windows_test.py
```

The suite runs once per configuration, over four: DSN and DSN-less crossed
with verified and unverified TLS. All four are HTTPS on port 8443, because the
stack serves nothing else. Each configuration records its own result, so the
run reports on all four and ends with a summary.

**Do not diagnose a Windows failure without rebuilding the DLL first.**
`--skip-build` reuses whatever sits in `target/x86_64-pc-windows-gnu/release/`,
which can predate the feature under test by days.

### Options

`--help` lists them all. The ones that come up:

| Flag | Default | Effect |
|---|---|---|
| `--skip-build` | off | Use the DLL already in `target/`, rather than rebuilding. See the warning above |
| `--target {gnu,msvc}` | `gnu` | Which Windows target to build and deploy. `msvc` needs an MSVC-capable linker on the host; see [Building the DLL](#building-the-dll) |
| `--host <address>` | discovered from the libvirt DHCP leases | VM IP or hostname |
| `--vm-network <name>` | `stackable-odbc-test-hostnet` | The libvirt network that discovery reads leases from |
| `--user`, `--password` | `Administrator`, `Asdf1234` | WinRM credentials |
| `--gateway <ip>` | `$ODBC_TEST_HOST_GATEWAY`, else `192.168.197.1` | The host-only gateway the VM reaches the host on. `scripts/gen-certs.sh` reads the same environment variable, so the coordinator's certificate covers the address the VM connects to |
| `--trino-host <address>` | the same as `--gateway` | Where the VM reaches Trino. The name `trino` is mapped to it in the VM's hosts file |

The verified configurations connect to `trino` rather than to an address. TLS
sends no SNI for an IP literal, and Jetty then serves Trino's internal
self-signed certificate instead of the CA-signed one, which no verification
can accept. The unverified configurations use the address directly, which is
what an operator who has not set up a name would do.

### The setup dialog

`dsn_dialog_test.py` drives the ODBC Data Source Administrator's **Add…**,
**Configure…** and **Remove** buttons, which is the only thing that exercises
`TrinoBackend::configure_dsn`. Every other suite reaches the driver through a
connection, and both `odbcconf` and `configure-dsn.ps1` call
`SQLConfigDataSource` with a null *hwndParent*, so they take the headless path.

Run `windows_test.py` first. It deploys the DLL and `configure-dsn.ps1`, which
the driver looks for beside it.

```bash
uv run --with pywinrm python3 integration-tests/windows/dsn_dialog_test.py

# Leave the Administrator open afterwards, to poke at by hand
uv run --with pywinrm python3 integration-tests/windows/dsn_dialog_test.py --keep-open
```

It takes `--host`, `--user`, `--password`, `--vm-network` and `--trino-host`
with the same meanings as above, plus `--domain` for the libvirt domain name
`virsh screenshot` is called with.

Six screenshots of the **Add…** path land in
`integration-tests/generated/windows-dialog/`, taken from the VM's framebuffer
with `virsh screenshot`. WinRM lands in session 0, which has no desktop to
photograph. They are what to look at when a check reports a mismatch. The
mechanics that took measuring are documented at the top of the script.

### Using a different hypervisor (VirtualBox, Hyper-V, etc.)

The VM lifecycle section below uses QEMU/KVM via libvirt, and the test script
discovers the VM IP from libvirt DHCP leases. Windows in a different
hypervisor works too. Pass the VM's IP directly:

```bash
uv run --with pywinrm python3 integration-tests/windows/windows_test.py --host <vm-ip>
```

The VM must have WinRM enabled on port 5985 with NTLM auth, and Python 3 plus
pyodbc installed. `dsn_dialog_test.py` additionally needs libvirt, because it
screenshots through `virsh`.

### OpenSSL legacy provider

WinRM uses NTLM authentication, which requires MD4, disabled by default in
modern OpenSSL. The test script sets `OPENSSL_CONF` to point at
`integration-tests/windows/openssl_legacy.cnf`, which enables the legacy
provider.

If you see `unsupported hash type md4` errors, check that the file exists and
that you have not overridden `OPENSSL_CONF` in your environment.

## VM lifecycle

### Prerequisites

QEMU/KVM and libvirt must be installed and working as system services.
`nix-shell` provides only Ansible and the Python bindings, not the
virtualisation stack itself. Verify with:

```bash
virsh --connect qemu:///system list --all
```

If this fails, install and configure QEMU/KVM and libvirt for your distro. You
will also need a `default` storage pool (`virsh pool-list`), and your user must
be in the `libvirt` group.

**Note:** QEMU typically runs as a dedicated user (e.g. `libvirt-qemu`) that
cannot read files under your home directory. If the playbook fails with a
permission error on the ISO or the virtio drivers, grant read access with ACLs
(e.g. `setfacl -m u:libvirt-qemu:r /path/to/file.iso`, and
`setfacl -m u:libvirt-qemu:x` on each parent directory).

On Ubuntu 24.04 that is:

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

Package names differ on other distros.

### Creating the VM

`WINDOWS_ISO` points at a Windows Server evaluation ISO you download yourself.
The table below names the one the current image was built from.

```bash
export WINDOWS_ISO=~/Downloads/SERVER_EVAL_x64FRE_en-us.iso

cd integration-tests/windows/vm
nix-shell                # loads Ansible + libvirt Python bindings
ansible-playbook start.yaml -i inventory.ini
```

The playbook creates a QEMU/KVM VM with two networks (host-only and NAT),
boots the Windows ISO, and waits for the guest agent. `Autounattend.xml`
installs Python and pyodbc.

First run takes ~30 minutes, for the Windows install and the downloads. Use
`virt-viewer` or `virt-manager` to watch progress:

```bash
virt-viewer --connect qemu:///system stackable-odbc-test
```

### What the current VM image was built with

A snapshot of the image in use, not a set of requirements. Each pin and the
paths derived from it have to move together, which is why they are collected
here.

| Thing | Value | Set in |
|---|---|---|
| Guest OS | Windows Server 2022 evaluation, [from the evalcenter](https://www.microsoft.com/en-us/evalcenter/evaluate-windows-server-2022) | `$WINDOWS_ISO`, checked by `vm/start.yaml` |
| Guest Python | 3.12.9, at `C:\Program Files\Python312\python.exe` | `vm/files/windows-install-config/Autounattend.xml`, and `REMOTE_PYTHON` in `windows_test.py` |
| virtio-win drivers | 0.1.248 | `vm/start.yaml`, downloaded and checksummed |
| LLVM for the MSVC cross build | `llvmPackages_18` | the `nix-shell` line under [Building the DLL](#building-the-dll) |

### Shutting down

```bash
virsh --connect qemu:///system shutdown stackable-odbc-test
```

The VM definition and disk persist, so the next `start` is fast.

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

`windows_test.py` builds the DLL, registers the driver and creates the DSNs
itself. These commands are for working on the VM by hand.

### Building the DLL

The mingw cross-compile recipe is in
[CONTRIBUTING.md](../../CONTRIBUTING.md#windows), and is what
`windows_test.py` runs. It produces:

```text
target/x86_64-pc-windows-gnu/release/stackable_odbc_trino.dll
```

MSVC is the alternative, selected with `--target msvc`. `cargo build` then
needs an MSVC-capable linker on the host, which `cargo-xwin` supplies
(`cargo install cargo-xwin`, plus nix for LLVM):

```bash
nix-shell -p llvmPackages_18.clang llvmPackages_18.lld llvmPackages_18.llvm --run \
  "cargo xwin build --release --target x86_64-pc-windows-msvc"
```

Output: `target/x86_64-pc-windows-msvc/release/stackable_odbc_trino.dll`

Both produce DLLs that work with the Windows Driver Manager. Prefer mingw;
use MSVC to match a target environment exactly.

A DLL for testing is built with plain `cargo build`, which is what the harness
runs. A DLL destined for a release archive is built with `cargo auditable`
instead, because the SBOM is generated from the dependency list that embeds.
See [`packaging/README.md`](../../packaging/README.md).

### Registering the driver

All commands below run in `cmd.exe` as Administrator. Adjust the DLL path as
needed.

```cmd
odbcconf.exe /A {INSTALLDRIVER "stackable_odbc_trino|Driver=C:\Users\Administrator\Downloads\stackable_odbc_trino.dll|Setup=C:\Users\Administrator\Downloads\stackable_odbc_trino.dll|"}
```

`Driver=` and `Setup=` must point at the same DLL, which exports both the ODBC
API functions and the `ConfigDSNW` setup entry point.

### Creating a DSN

Three ways, in descending order of convenience.

**The ODBC Data Source Administrator.** `odbcad32.exe` → **Add…** → select
`stackable_odbc_trino`, or **Configure…** on an existing data source. Both
display the driver's dialog: `ConfigDSN` reaches
`TrinoBackend::configure_dsn`, which runs `configure-dsn.ps1` with `-Emit` and
hands the keywords back for core to write. The script must be installed
alongside the DLL, which `install.bat` does.

**The dialog on its own**, the same WinForms dialog without the Administrator.
It writes through `SQLConfigDataSourceW`, so the driver's own `ConfigDSN`
stays in the loop:

```powershell
powershell -ExecutionPolicy Bypass -File configure-dsn.ps1
```

**`odbcconf`**, which is what the test harness uses. It passes a null
*hwndParent*, so no dialog is displayed and the keywords on the command line
are written as given:

```cmd
odbcconf.exe /A {CONFIGDSN "stackable_odbc_trino" "DSN=MyTrino|Host=trino.example.com|Port=8443|User=admin|Password=secret|Catalog=hive|Schema=default|"}
```

A DSN written by hand stores the five `name:value;name2:value2` keys **bare**,
where a connection string braces them. See
[the root README](../../README.md#values-that-contain-a-semicolon).
`configure-dsn.ps1` handles it for you.

### Connection string parameters

The full table is in the [root README](../../README.md#connecting), and the
authoritative list is `src/backend/types/connect_params.rs`. `Host` and `Port`
are the only required keys, plus `User` outside `ExternalAuthentication`. The
examples in this file add `Password`, `Protocol`, `Catalog`, `Schema` and
`TlsVerify`.

### Verifying registration

Open `%SystemRoot%\System32\odbcad32.exe` (64-bit) and confirm:

- **Drivers tab**: `stackable_odbc_trino` is listed, with a version and a
  company rather than `Not marked`
- **User DSN tab**: `MyTrino` (or whatever DSN name you chose) is listed
- **Add…** with `stackable_odbc_trino` selected opens the driver's own dialog;
  so does **Configure…** on an existing data source. See
  [Creating a DSN](#creating-a-dsn)

### Unregistering

Remove a DSN (User DSN entries are stored under `HKCU`):

```cmd
reg delete "HKCU\SOFTWARE\ODBC\ODBC.INI\MyTrino" /f
reg delete "HKCU\SOFTWARE\ODBC\ODBC.INI\ODBC Data Sources" /v "MyTrino" /f
```

Remove the driver, via the registry, as `odbcconf` does not support
`REMOVEDRIVER`:

```cmd
reg delete "HKLM\SOFTWARE\ODBC\ODBCINST.INI\stackable_odbc_trino" /f
reg delete "HKLM\SOFTWARE\ODBC\ODBCINST.INI\ODBC Drivers" /v "stackable_odbc_trino" /f
```

## Reference: manual testing (optional)

Nothing below is needed to run the suites. It is a smoke-test cookbook for a
PowerShell session on the VM.

### PowerShell smoke test

PowerShell's `System.Data.Odbc` is built into .NET, so no extra tools are
needed. This example uses an inline `VALUES` list, so it needs no table and no
writable catalog. The driver must be registered first, which the test script
does, and Trino must be reachable at the given host.

The compose stack serves HTTPS on 8443 and nothing else. Its certificate comes
from a CA no machine trusts by default, so `TlsVerify=false` is what makes a
hand-typed connection work against it. Point at a coordinator with a real
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

**DSN-based connection.** `windows_test.py` registers a DSN named
`test_trino`. To create one yourself, see [Creating a DSN](#creating-a-dsn),
then:

```powershell
$c = New-Object System.Data.Odbc.OdbcConnection("DSN=MyTrino"); $c.Open(); Write-Host "Connected: $($c.State)"; $c.Close()
```

### Running test_integration.py manually

The harness deploys `test_integration.py`, `harness.py` and the test CA to
`C:\odbc_test_trino`. To run the suite without the wrapper script, from a
PowerShell session on the VM:

```powershell
& "C:\Program Files\Python312\python.exe" C:\odbc_test_trino\test_integration.py "Driver=stackable_odbc_trino;Host=<trino-host>;Port=8443;User=admin;Password=admin;Protocol=https;TlsVerify=false;Catalog=tpcds"
```
