#!/usr/bin/env python3
"""
Run the Trino integration suites on a Windows VM over WinRM.

Builds the ODBC driver DLL, discovers the VM, deploys the suites, registers
the driver, and runs everything `suites/registry.py` marks as running on
Windows, through the Windows Driver Manager. Trino must be running on the host
(via integration-tests/setup.sh) before running this script.

Which suites those are is not decided here. `suites/registry.py` is the one
list, shared with `scripts/run-tests.sh`, and a suite that does not run here
carries its reason in that file. What *is* decided here is how a suite is
invoked on the VM, and the four connect configurations, which are not the same
four the Linux runner uses: these cross a registry-registered DSN with a
DSN-less string, and connect by address for the unverified cases so that no SNI
is sent.

Usage:
    uv run --with pywinrm python3 integration-tests/windows/windows_test.py
    uv run --with pywinrm python3 integration-tests/windows/windows_test.py --skip-build
    uv run --with pywinrm python3 integration-tests/windows/windows_test.py --host 192.168.197.138
    uv run --with pywinrm python3 integration-tests/windows/windows_test.py --suite tls

Requires a running Trino on the host, the Windows VM, and `pip install pywinrm`.
A suite needing a compose profile is skipped unless the host stack was set up
with it.
"""

import argparse
import http.server
import os
import re
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent            # integration-tests/windows
TEST_DIR = SCRIPT_DIR.parent                            # integration-tests
PROJECT_DIR = TEST_DIR.parent
OPENSSL_CNF = SCRIPT_DIR / "openssl_legacy.cnf"

sys.path.insert(0, str(TEST_DIR / "suites"))

from harness import Stack  # noqa: E402
from registry import SUITES, suite_argv  # noqa: E402

# The VM mirrors the repository layout under one directory, rather than holding
# a flat pile of files. The suites address their neighbours by relative path:
# `test_folding_contract.py` reads `../../connector/StackableTrinoODBC.pq`, and
# `harness.Stack` defaults to `../generated/stack.env`. Mirroring makes all of
# that resolve on the VM exactly as it does on the host, so no suite needs a
# Windows branch to find its own inputs.
REMOTE_DIR = r"C:\odbc_test_trino"
REMOTE_SUITES = rf"{REMOTE_DIR}\integration-tests\suites"
REMOTE_GENERATED = rf"{REMOTE_DIR}\integration-tests\generated"
REMOTE_CERTS = rf"{REMOTE_GENERATED}\certs"
REMOTE_CA = rf"{REMOTE_CERTS}\ca.crt"
# The DLL stays at the root rather than under a mirrored `packaging/windows/`,
# because `configure-dsn.ps1` has to sit beside it: that is where the driver's
# ConfigDSN looks for the script. Their adjacency is a requirement, not a
# layout choice.
REMOTE_DLL = rf"{REMOTE_DIR}\stackable_odbc_trino.dll"
REMOTE_LOG = rf"{REMOTE_DIR}\stackable_odbc_trino.log"

# Written on the host, served to the VM, and kept for inspection: a failing
# suite that read the wrong host or certificate is diagnosed from this file.
HOST_VM_STACK_ENV = TEST_DIR / "generated" / "windows-stack.env"

DRIVER_NAME = "stackable_odbc_trino"
DSN_NAME = "test_trino"

# Absolute path to Python on the VM.
REMOTE_PYTHON = r'"C:\Program Files\Python312\python.exe"'

# The host-only network gateway. The VM reaches the host (and Docker) through
# this IP. Override with --gateway or ODBC_TEST_HOST_GATEWAY for a non-default
# subnet.
DEFAULT_HOST_GATEWAY = "192.168.197.1"
HTTP_PORT = 8081  # avoid conflict with the file server's own use

# The VM reaches the host by IP, and TLS sends no SNI for an IP literal, so
# Jetty serves Trino's internal self-signed certificate instead of the
# CA-signed one and verification cannot succeed. Mapping a name the
# coordinator's certificate carries (DNS:trino, see scripts/gen-certs.sh) to
# the gateway in the VM's hosts file makes SNI work, which is what keeps the
# verified-TLS configurations meaningful here.
TRINO_VM_HOSTNAME = "trino"


def main():
    args = parse_args()
    setup_openssl()

    # Verify Trino is running on the host before doing anything on the VM.
    check_trino_reachable()

    if not args.skip_build:
        build_dll(args.target)

    dll_path = resolve_dll_path(args.target)
    host_stack = Stack.load()
    trino_host = args.trino_host or args.gateway
    vm_stack = write_vm_stack_env(host_stack)

    host = args.host or discover_vm_ip(args.vm_network)

    import winrm

    print(f"=== Connecting to {host} via WinRM ===")
    session = winrm.Session(
        f"http://{host}:5985/wsman",
        auth=(args.user, args.password),
        transport="ntlm",
    )

    # Verify connectivity
    r = session.run_ps("hostname")
    hostname = r.std_out.decode().strip()
    if r.status_code != 0:
        print(f"ERROR: WinRM connection failed: {r.std_err.decode()}", file=sys.stderr)
        sys.exit(1)
    print(f"Connected to {hostname}")

    # Wait for VM setup to complete (Python, pyodbc installed).
    wait_for_setup(session)

    deploy(session, args.gateway, dll_path)

    # Before `register_driver`, because the uninstaller it runs deregisters the
    # driver: doing it afterwards would tear down the registration every suite
    # below depends on.
    check_installers(session)

    print("=== Registering ODBC driver ===")
    register_driver(session)

    map_trino_hostname(session, trino_host)

    run_suites(session, args, vm_stack, trino_host)


def run_suites(session, args, vm_stack, trino_host):
    """Run every suite the registry marks as running on Windows.

    A failure records rather than aborts. Aborting on the first one hid every
    later suite and configuration entirely, so a single flaky case cost the
    whole run's information.
    """
    # The DSN-less verified string every non-matrix suite runs against, the
    # counterpart of the Linux runner's CONN_HTTPS. Built from the VM's own
    # stack.env, so it cannot disagree with what the suites reading that file
    # will connect with.
    conn_verified = vm_stack.conn_str()
    driver = vm_stack.get("DRIVER_PATH")
    profiles = vm_stack.profiles

    failed, skipped = [], []

    for suite in SUITES:
        if args.suite and args.suite not in suite.name:
            continue
        if not suite.runs_on_windows:
            print(f"SKIP  {suite.name}: {suite.windows_skip_reason}")
            skipped.append(suite.name)
            continue
        if suite.profile and suite.profile not in profiles:
            print(f"SKIP  {suite.name}: profile '{suite.profile}' is not active "
                  f"(setup.sh --profile {suite.profile})")
            skipped.append(suite.name)
            continue

        if suite.matrix:
            failed += run_matrix(session, suite, vm_stack, driver, trino_host)
        else:
            print(f"=== Running {suite.name} ===")
            argv = suite_argv(suite, driver, conn_verified)
            if run_remote(session, suite.script, argv) != 0:
                failed.append(suite.name)

    print("")
    print("=== Windows summary ===")
    if skipped:
        for name in skipped:
            print(f"  SKIP  {name}")
    if failed:
        for name in failed:
            print(f"  FAIL  {name}")
        print(f"{len(failed)} suite run(s) failed")
        sys.exit(1)
    print("all selected suites passed")


def run_matrix(session, suite, vm_stack, driver, trino_host):
    """Run one suite over the four Windows connect configurations.

    DSN and DSN-less crossed with verified and unverified TLS. The verified
    ones connect by TRINO_VM_HOSTNAME so that SNI is sent; the unverified ones
    use the address directly, which is what an operator who has not set up a
    name would do, and which no verification could accept.

    The DSN configurations re-register `DSN_NAME` in between, so the order here
    is load bearing and this stays a sequence rather than a table.
    """
    failed = []

    def run_config(label, conn_str):
        name = f"{suite.name} ({label})"
        print(f"=== Running {name} ===")
        if run_remote(session, suite.script, suite_argv(suite, driver, conn_str)) != 0:
            failed.append(name)

    run_config("DSN-less, verified TLS", vm_stack.conn_str())
    run_config("DSN-less, TlsVerify=false", vm_stack.conn_str(
        Host=trino_host, TlsVerify="false", Certificate=None,
    ))

    print("=== Registering DSN (verified TLS) ===")
    register_dsn(session, TRINO_VM_HOSTNAME, protocol="https", port=8443,
                 extra=f"Certificate={REMOTE_CA}")
    run_config("DSN, verified TLS", f"DSN={DSN_NAME}")

    print("=== Registering DSN (TlsVerify=false) ===")
    register_dsn(session, trino_host, protocol="https", port=8443,
                 extra="TlsVerify=false")
    run_config("DSN, TlsVerify=false", f"DSN={DSN_NAME}")

    return failed


def write_vm_stack_env(host_stack) -> Stack:
    """Write the VM's `stack.env` and return it parsed.

    The suites that read `stack.env` rather than taking a connection string
    (tls, spooling, transactions) were unrunnable on Windows for want of this
    file alone: the host's names the driver's `.so`, a `localhost` the VM is
    not, and certificate paths under the user's home directory.

    Everything that is a property of the *stack* is copied from the host's
    file, so a credential or a port stays stated in one place. Only what the VM
    sees differently is rewritten.
    """
    values = {
        # Two keys where the host needs one. The Windows Driver Manager loads a
        # driver by its registered name, while the ctypes suites want the DLL's
        # path; on Linux one string serves both. See `Stack.driver_ref`.
        "DRIVER_NAME": DRIVER_NAME,
        "DRIVER_PATH": REMOTE_DLL,
        # The name mapped into the VM's hosts file, not the host's `localhost`.
        # It is also a name the coordinator's certificate carries, so TLS sends
        # an SNI Jetty can match.
        "TRINO_HOST": TRINO_VM_HOSTNAME,
        "TRINO_HTTPS_PORT": host_stack.get("TRINO_HTTPS_PORT"),
        "TRINO_USER": host_stack.get("TRINO_USER"),
        "TRINO_PASSWORD": host_stack.get("TRINO_PASSWORD"),
        "TRINO_CATALOG": host_stack.get("TRINO_CATALOG"),
        "CA_CERT": rf"{REMOTE_CERTS}\ca.crt",
        "CLIENT_PEM": rf"{REMOTE_CERTS}\client.pem",
        # The profiles are the host stack's, because that is the coordinator the
        # VM connects to. A suite gated on one is gated on the same one here.
        "PROFILES": ",".join(host_stack.profiles),
    }
    HOST_VM_STACK_ENV.parent.mkdir(parents=True, exist_ok=True)
    with open(HOST_VM_STACK_ENV, "w", encoding="utf-8") as f:
        f.write("# generated by windows/windows_test.py, do not edit\n")
        f.write("# the VM's view of the stack; the host's is generated/stack.env\n")
        for key, value in values.items():
            f.write(f"{key}={value}\n")
    return Stack.load(str(HOST_VM_STACK_ENV))


def deploy(session, gateway: str, dll_path: Path):
    """Serve the suites over HTTP and have the VM download them.

    The file map is keyed by repository-relative path and the VM recreates that
    layout, which is what lets a suite find the connector source or the
    certificates by the same relative path it uses on the host.
    """
    print("=== Deploying files via HTTP ===")
    files = {
        "stackable_odbc_trino.dll": dll_path,
        # Deployed even though the automated configurations never open a
        # dialog: without it the ODBC Administrator's Add... button fails on
        # the VM, and that is the one thing the harness could not otherwise be
        # used to check.
        "configure-dsn.ps1": PROJECT_DIR / "packaging" / "windows" / "configure-dsn.ps1",
        # The shipped installers, so `check_installers` can run the real thing
        # rather than a paraphrase of it. They are what a user runs, and until
        # this harness ran them nothing did: `register_driver` below registers
        # the driver its own way.
        "install.bat": PROJECT_DIR / "packaging" / "windows" / "install.bat",
        "uninstall.bat": PROJECT_DIR / "packaging" / "windows" / "uninstall.bat",
        # The test CA, so the VM can verify the coordinator rather than only
        # skip it. Needed by every configuration, not by any one suite.
        "integration-tests/generated/certs/ca.crt":
            TEST_DIR / "generated" / "certs" / "ca.crt",
        # The VM's own view of the stack, which is what the suites reading
        # stack.env rather than taking a connection string work from.
        "integration-tests/generated/stack.env": HOST_VM_STACK_ENV,
    }
    # The whole suites directory, not the scripts the registry selects. The
    # suites import shared modules from beside themselves (`harness`, and
    # `odbc_abi` for the two ctypes suites), and deploying a computed list meant
    # a new shared module was a deployment failure on the VM and nowhere else.
    # They are a few hundred kilobytes in total, so nothing is bought by
    # deploying fewer of them.
    for script in sorted((TEST_DIR / "suites").glob("*.py")):
        files[f"integration-tests/suites/{script.name}"] = script
    for suite in SUITES:
        if suite.runs_on_windows:
            for rel in suite.deploy:
                files[rel] = PROJECT_DIR / rel

    missing = [str(p) for p in files.values() if not p.exists()]
    if missing:
        print("ERROR: missing files; run ./integration-tests/setup.sh\n  "
              + "\n  ".join(missing), file=sys.stderr)
        sys.exit(1)

    with http_file_server(files) as port:
        base_url = f"http://{gateway}:{port}"
        # A PowerShell loop rather than one Invoke-WebRequest per file: the
        # command line is sent over WinRM, and a couple of dozen of them
        # spelled out reliably exceeded what it would carry.
        names = ", ".join(f"'{name}'" for name in files)
        # .Replace rather than -replace, so that neither side is a regex. A
        # path separator is not a pattern, and -replace would make the
        # substitution depend on .NET replacement-pattern rules.
        r = session.run_ps(
            f'$ProgressPreference = "SilentlyContinue"; '
            f'foreach ($f in @({names})) {{ '
            f'  $dest = Join-Path "{REMOTE_DIR}" $f.Replace("/", "\\"); '
            f'  New-Item -ItemType Directory -Force -Path (Split-Path $dest) | Out-Null; '
            f'  Invoke-WebRequest -Uri "{base_url}/$f" -OutFile $dest; '
            f'}}'
        )
        if r.status_code != 0:
            print(f"ERROR: file download failed:\n{r.std_err.decode()}", file=sys.stderr)
            sys.exit(1)

    print(f"  {len(files)} files, DLL {dll_path.stat().st_size / 1024:.0f} KB")


def map_trino_hostname(session, gateway: str):
    """Point TRINO_VM_HOSTNAME at the host in the VM's hosts file.

    TLS sends no SNI for an IP literal, and Jetty serves Trino's internal
    self-signed certificate for anything it cannot match on SNI, so connecting
    by address can never verify against the CA-signed certificate. A name the
    certificate carries fixes that, and the VM's hosts file is the only place
    to put it.
    """
    print(f"=== Mapping {TRINO_VM_HOSTNAME} -> {gateway} in the VM hosts file ===")
    hosts = r"C:\Windows\System32\drivers\etc\hosts"
    ps = (
        f'$h = "{hosts}"; '
        f'$line = "{gateway}`t{TRINO_VM_HOSTNAME}"; '
        f'$kept = (Get-Content $h) | Where-Object {{ $_ -notmatch "\\s{TRINO_VM_HOSTNAME}\\s*$" }}; '
        f'Set-Content -Path $h -Value ($kept + $line)'
    )
    r = session.run_ps(ps)
    if r.status_code != 0:
        print(f"ERROR: could not write the VM hosts file:\n{r.std_err.decode()}",
              file=sys.stderr)
        sys.exit(1)


def parse_args():
    p = argparse.ArgumentParser(
        description="Run Trino ODBC integration tests on a Windows VM.",
    )
    p.add_argument(
        "--skip-build",
        action="store_true",
        help="skip cargo build, use existing DLL",
    )
    p.add_argument(
        "--target",
        choices=["gnu", "msvc"],
        default="gnu",
        help="cross-compilation target (default: gnu)",
    )
    p.add_argument(
        "--host",
        help="VM IP or hostname (default: auto-discover from DHCP leases)",
    )
    p.add_argument(
        "--vm-network",
        default="stackable-odbc-test-hostnet",
        help="libvirt network for IP discovery (default: stackable-odbc-test-hostnet)",
    )
    p.add_argument(
        "--user",
        default="Administrator",
        help="WinRM username (default: Administrator)",
    )
    p.add_argument(
        "--password",
        default="Asdf1234",
        help="WinRM password (default: Asdf1234)",
    )
    p.add_argument(
        "--gateway",
        default=os.environ.get("ODBC_TEST_HOST_GATEWAY", DEFAULT_HOST_GATEWAY),
        help=(
            "host-only network gateway IP the VM uses to reach the host "
            f"(default: $ODBC_TEST_HOST_GATEWAY or {DEFAULT_HOST_GATEWAY})"
        ),
    )
    p.add_argument(
        "--trino-host",
        default=None,
        help="Trino host as seen from the VM (default: same as --gateway)",
    )
    p.add_argument(
        "--suite",
        default="",
        help="only run suites whose name contains this string",
    )
    return p.parse_args()


def setup_openssl():
    """Point OPENSSL_CONF at the legacy provider config for NTLM/MD4."""
    if "OPENSSL_CONF" in os.environ:
        return
    if OPENSSL_CNF.exists():
        os.environ["OPENSSL_CONF"] = str(OPENSSL_CNF)
    else:
        print(
            f"WARNING: {OPENSSL_CNF} not found. WinRM NTLM auth may fail\n"
            "if your OpenSSL does not have the legacy provider enabled.\n"
            "See windows/WINDOWS.md for details.",
            file=sys.stderr,
        )


def check_trino_reachable():
    """Verify Trino is running on the host before deploying to the VM."""
    print("=== Checking Trino is reachable on host ===")
    try:
        result = subprocess.run(
            ["curl", "-sf", "--cacert",
             str(TEST_DIR / "generated" / "certs" / "ca.crt"),
             "-u", "admin:admin", "https://localhost:8443/v1/info"],
            capture_output=True, timeout=5,
        )
        if result.returncode == 0:
            print("Trino is running")
            return
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    print(
        "ERROR: Trino is not reachable at https://localhost:8443/v1/info\n"
        "Start it with: ./integration-tests/setup.sh",
        file=sys.stderr,
    )
    sys.exit(1)


def build_dll(target: str):
    """Cross-compile the Trino ODBC driver for Windows."""
    rust_target = (
        "x86_64-pc-windows-msvc" if target == "msvc" else "x86_64-pc-windows-gnu"
    )
    cmd = [
        "cargo", "build", "--release",
        "--target", rust_target,
        "-p", "stackable-odbc-trino",
    ]
    print(f"=== Building DLL ({rust_target}) ===")
    result = subprocess.run(cmd, cwd=PROJECT_DIR)
    if result.returncode != 0:
        print("ERROR: cargo build failed", file=sys.stderr)
        sys.exit(1)


def resolve_dll_path(target: str) -> Path:
    """Return the path to the built DLL."""
    rust_target = (
        "x86_64-pc-windows-msvc" if target == "msvc" else "x86_64-pc-windows-gnu"
    )
    dll = PROJECT_DIR / "target" / rust_target / "release" / "stackable_odbc_trino.dll"
    if not dll.exists():
        print(f"ERROR: DLL not found at {dll}", file=sys.stderr)
        print("Run without --skip-build, or check your build output.", file=sys.stderr)
        sys.exit(1)
    return dll


def discover_vm_ip(network: str) -> str:
    """Get the VM IP from libvirt DHCP leases."""
    print(f"=== Discovering VM IP from network {network} ===")
    try:
        result = subprocess.run(
            ["virsh", "--connect", "qemu:///system", "net-dhcp-leases", network],
            capture_output=True, text=True, check=True,
        )
    except FileNotFoundError:
        print("ERROR: virsh not found. Install libvirt or use --host.", file=sys.stderr)
        sys.exit(1)
    except subprocess.CalledProcessError as e:
        print(
            f"ERROR: could not query DHCP leases for network '{network}'.\n"
            "Is the VM running? See windows/WINDOWS.md for setup.\n"
            f"virsh output: {e.stderr}",
            file=sys.stderr,
        )
        sys.exit(1)

    ips = re.findall(r"ipv4\s+([\d.]+)/", result.stdout)
    if not ips:
        print(
            f"ERROR: no DHCP leases found on network '{network}'.\n"
            "Is the VM running? See windows/WINDOWS.md for setup.",
            file=sys.stderr,
        )
        sys.exit(1)

    ip = ips[-1]
    print(f"Found VM at {ip}")
    return ip


def wait_for_setup(session):
    """Wait for VM FirstLogonCommands to finish (sentinel file)."""
    r = session.run_ps(r"Test-Path C:\setup_complete.txt")
    if r.std_out.decode().strip() == "True":
        return

    print("=== Waiting for VM setup to complete ===")
    for i in range(60):
        time.sleep(10)
        r = session.run_ps(r"Test-Path C:\setup_complete.txt")
        if r.std_out.decode().strip() == "True":
            print("  Setup complete")
            return
        print(f"  still waiting... ({(i + 1) * 10}s)", end="\r")

    print(
        "\nERROR: timed out waiting for VM setup (C:\\setup_complete.txt).\n"
        "The FirstLogonCommands in Autounattend.xml may have failed.\n"
        "Check the VM with: virt-viewer --connect qemu:///system stackable-odbc-test",
        file=sys.stderr,
    )
    sys.exit(1)


def ps_quote(value: str) -> str:
    """Quote a value as a PowerShell single-quoted string.

    Connection strings carry semicolons and backslashes, both of which a
    double-quoted PowerShell string would interpret. Single quotes are literal
    throughout, and a literal single quote is written by doubling it.
    """
    escaped = value.replace("'", "''")
    return f"'{escaped}'"


def run_remote(session, script: str, argv) -> int:
    """Run one suite on the VM and return its exit code."""
    args = " ".join(ps_quote(a) for a in argv)
    # Enable driver-side debug logging and DM tracing.
    r = session.run_ps(
        f'$env:ODBC_LOG_LEVEL = "debug"; '
        f'$env:ODBC_LOG_FILE = "{REMOTE_LOG}"; '
        f'& {REMOTE_PYTHON} "{REMOTE_SUITES}\\{script}" {args}'
    )
    stdout = r.std_out.decode("utf-8", errors="replace")
    print(stdout, end="")

    if r.std_err:
        stderr = r.std_err.decode("utf-8", errors="replace")
        if "CLIXML" not in stderr:
            print(stderr, end="", file=sys.stderr)

    # Retrieve the driver trace log, then clear it for the next run.
    lr = session.run_ps(
        f'if (Test-Path "{REMOTE_LOG}") {{ Get-Content "{REMOTE_LOG}" -Tail 200 }}'
    )
    log_content = lr.std_out.decode("utf-8", errors="replace").strip()
    if log_content:
        print("\n=== stackable_odbc_trino.log (last 200 lines) ===")
        print(log_content)
    session.run_ps(f'Remove-Item -Force -ErrorAction SilentlyContinue "{REMOTE_LOG}"')

    return r.status_code


class _FileServer(http.server.SimpleHTTPRequestHandler):
    """HTTP handler that serves specific files from a lookup table."""

    file_map: dict[str, Path] = {}

    def do_GET(self):
        name = self.path.lstrip("/")
        path = self.file_map.get(name)
        if path is None or not path.exists():
            self.send_error(404, f"Not found: {name}")
            return
        data = path.read_bytes()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Content-Type", "application/octet-stream")
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, format, *args):
        pass


class http_file_server:
    """Context manager that runs a temporary HTTP server in a background thread."""

    def __init__(self, file_map: dict[str, Path]):
        self.file_map = file_map
        self.server = None
        self.thread = None

    def __enter__(self) -> int:
        handler = type(
            "_Handler",
            (_FileServer,),
            {"file_map": self.file_map},
        )
        port = HTTP_PORT if _port_available(HTTP_PORT) else 0
        self.server = http.server.HTTPServer(("0.0.0.0", port), handler)
        if port == 0:
            port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        return port

    def __exit__(self, *_):
        if self.server:
            self.server.shutdown()
        if self.thread:
            self.thread.join(timeout=5)


def _port_available(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        try:
            s.bind(("0.0.0.0", port))
            return True
        except OSError:
            return False


def check_installers(session):
    """Run the shipped install.bat and uninstall.bat, and check what they left.

    Everything else in this file registers the driver its own way, so the two
    scripts a user actually runs were exercised by nothing. Both halves have
    failed silently in the past for the same reason: `odbcconf.exe` reports
    success whether or not the action succeeded, so install.bat's `errorlevel`
    check proved nothing, and uninstall.bat deleted only the DLL while
    install.bat had placed two files.

    The archive layout is reproduced rather than assumed: install.bat refuses to
    run unless the DLL and configure-dsn.ps1 sit beside it, which is exactly the
    property worth testing.
    """
    print("=== Checking the shipped installers ===")
    staging = rf"{REMOTE_DIR}\archive"
    install_dir = r"$env:ProgramFiles\Stackable\ODBC"
    key = r"HKLM:\SOFTWARE\ODBC\ODBCINST.INI\stackable_odbc_trino"
    listing = r"HKLM:\SOFTWARE\ODBC\ODBCINST.INI\ODBC Drivers"

    # Start from a clean slate: a driver left registered by an earlier run would
    # make the install check pass without installing anything.
    session.run_ps(
        f'cmd.exe /c "{staging}\\uninstall.bat" | Out-Null; '
        f'Remove-Item -Recurse -Force -ErrorAction SilentlyContinue "{staging}"'
    )

    r = session.run_ps(
        f'New-Item -ItemType Directory -Force -Path "{staging}" | Out-Null; '
        f'Copy-Item "{REMOTE_DIR}\\stackable_odbc_trino.dll","{REMOTE_DIR}\\configure-dsn.ps1",'
        f'"{REMOTE_DIR}\\install.bat","{REMOTE_DIR}\\uninstall.bat" -Destination "{staging}"; '
        f'cmd.exe /c "{staging}\\install.bat"'
    )
    if r.status_code != 0:
        print(f"ERROR: install.bat failed:\n{r.std_out.decode()}\n{r.std_err.decode()}",
              file=sys.stderr)
        sys.exit(1)

    # What the ODBC Administrator reads. `Driver` alone is not enough: the
    # Drivers tab is populated from the "ODBC Drivers" listing, and a driver
    # present in one and not the other is invisible.
    r = session.run_ps(
        f'$ok = $true; '
        f'foreach ($f in "stackable_odbc_trino.dll","configure-dsn.ps1") {{ '
        f'  if (-not (Test-Path (Join-Path "{install_dir}" $f))) '
        f'    {{ Write-Output "missing installed file: $f"; $ok = $false }} }}; '
        f'if (-not (Test-Path "{key}")) '
        f'  {{ Write-Output "missing registry key"; $ok = $false }}; '
        f'if (-not (Get-ItemProperty -Path "{listing}" -Name "stackable_odbc_trino" '
        f'  -ErrorAction SilentlyContinue)) '
        f'  {{ Write-Output "not listed under ODBC Drivers"; $ok = $false }}; '
        f'if ($ok) {{ Write-Output "INSTALL-OK" }}'
    )
    out = r.std_out.decode().strip()
    if "INSTALL-OK" not in out:
        print(f"ERROR: install.bat reported success but left an incomplete install:\n{out}",
              file=sys.stderr)
        sys.exit(1)
    print("  install.bat: both files placed, driver registered and listed")

    r = session.run_ps(f'cmd.exe /c "{staging}\\uninstall.bat"')
    if r.status_code != 0:
        print(f"ERROR: uninstall.bat failed:\n{r.std_err.decode()}", file=sys.stderr)
        sys.exit(1)

    r = session.run_ps(
        f'$left = @(); '
        f'foreach ($f in "stackable_odbc_trino.dll","configure-dsn.ps1") {{ '
        f'  if (Test-Path (Join-Path "{install_dir}" $f)) {{ $left += $f }} }}; '
        f'if (Test-Path "{install_dir}") {{ $left += "the install directory" }}; '
        f'if (Test-Path "{key}") {{ $left += "the registry key" }}; '
        f'if (Get-ItemProperty -Path "{listing}" -Name "stackable_odbc_trino" '
        f'  -ErrorAction SilentlyContinue) {{ $left += "the ODBC Drivers entry" }}; '
        f'if ($left.Count -eq 0) {{ Write-Output "UNINSTALL-OK" }} '
        f'else {{ Write-Output ("left behind: " + ($left -join ", ")) }}'
    )
    out = r.std_out.decode().strip()
    if "UNINSTALL-OK" not in out:
        print(f"ERROR: uninstall.bat did not clean up:\n{out}", file=sys.stderr)
        sys.exit(1)
    print("  uninstall.bat: both files, the directory and both registry entries removed")

    session.run_ps(f'Remove-Item -Recurse -Force -ErrorAction SilentlyContinue "{staging}"')


def register_driver(session):
    """Register the ODBC driver via registry + odbcconf.exe.

    INSTALLDRIVER alone won't update the DLL path if the driver is already
    registered (it only increments UsageCount). Force-update via the registry
    to ensure the freshly deployed DLL is always used.
    """
    cmd = (
        f'odbcconf.exe /A {{INSTALLDRIVER '
        f'"{DRIVER_NAME}|Driver={REMOTE_DLL}|Setup={REMOTE_DLL}|"}}'
    )
    r = session.run_cmd("cmd.exe", ["/c", cmd])
    if r.status_code != 0:
        print(f"ERROR: driver registration failed: {r.std_err.decode()}", file=sys.stderr)
        sys.exit(1)
    session.run_ps(
        f'Set-ItemProperty '
        f'"HKLM:\\SOFTWARE\\ODBC\\ODBCINST.INI\\{DRIVER_NAME}" '
        f'-Name "Driver" -Value "{REMOTE_DLL}"; '
        f'Set-ItemProperty '
        f'"HKLM:\\SOFTWARE\\ODBC\\ODBCINST.INI\\{DRIVER_NAME}" '
        f'-Name "Setup" -Value "{REMOTE_DLL}"'
    )
    print(f"Driver '{DRIVER_NAME}' registered")


def register_dsn(session, trino_host: str, *, protocol: str = "https",
                 port: int = 8443, extra: str = ""):
    """Register (or re-register) a DSN for a Trino connection."""
    extra_fields = f"|{extra}" if extra else ""
    cmd = (
        f'odbcconf.exe /A {{CONFIGDSN "{DRIVER_NAME}" '
        f'"DSN={DSN_NAME}|Host={trino_host}|Port={port}|User=admin|Password=admin'
        f'|Protocol={protocol}|Catalog=tpcds{extra_fields}|"}}'
    )
    r = session.run_cmd("cmd.exe", ["/c", cmd])
    if r.status_code != 0:
        print(f"ERROR: DSN registration failed: {r.std_err.decode()}", file=sys.stderr)
        sys.exit(1)
    print(f"DSN '{DSN_NAME}' registered ({protocol}:{port})")


if __name__ == "__main__":
    main()
