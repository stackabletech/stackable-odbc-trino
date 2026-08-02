#!/usr/bin/env python3
"""Drive the Windows ODBC Data Source Administrator's buttons, with screenshots.

This is the only check on `TrinoBackend::configure_dsn`. Everything else in
`integration-tests/` reaches the driver through a connection: `odbcconf` and
`configure-dsn.ps1` both call `SQLConfigDataSource` with a **null** hwndParent,
which is the headless path, so nothing else exercises the dialog at all.

    ./integration-tests/setup.sh                    # Trino must be up
    ./integration-tests/windows/windows_test.py     # deploys the DLL and script
    uv run --with pywinrm python3 \
        integration-tests/windows/dsn_dialog_test.py

Screenshots land in `integration-tests/generated/windows-dialog/`, numbered in
the order they were taken, and are what a reviewer looks at when a step reports
a mismatch. `--keep-open` leaves the Administrator up to poke at by hand.

They cover the **Add…** path only, through to the data source appearing in the
Administrator's list and reopening under **Configure…**. Cancel and Remove are
checked but not photographed: what they produce is a *transient* dialog and an
empty list, neither of which a picture settles, and both are asserted against
the registry instead.

Three things about the mechanics are load-bearing, and each one silently
produced a wrong answer before it was understood:

- **WinRM lands in session 0**, which has no visible desktop. A GUI started
  from it is invisible and an in-guest screenshot is blank. Every UI step
  therefore runs through a scheduled task with an interactive logon type, which
  lands in the console session the VM auto-logs into, and screenshots come from
  `virsh screenshot` on the host, which captures that session's framebuffer.
- **Neither dialog ships a UI Automation provider**, so every control arrives
  as a generic `Pane` with no `InvokePattern` and, unreliably, no
  `ValuePattern`. Buttons take a posted `BM_CLICK`, text fields take
  `WM_SETTEXT`, and the tab strip -- which is not a control with a handle at
  all -- takes a mouse click at its coordinates.
- **Control text is read from UI Automation's `Name`, never `GetWindowTextW`**,
  which does not retrieve an edit control's text across a process boundary and
  answers empty. That empty answer reads exactly like the write having failed.
- **`BM_CLICK` is posted, never sent.** A button that opens a modal dialog does
  not return from its click until that dialog closes, so `SendMessage` hung the
  driving script for as long as the dialog was up, and the stuck instance then
  made every later scheduled run refuse to start.
"""
import argparse
import base64
import json
import subprocess
import sys
import time
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
TEST_DIR = SCRIPT_DIR.parent
SHOT_DIR = TEST_DIR / "generated" / "windows-dialog"

REMOTE_DIR = r"C:\odbc_test_trino"
DRIVER_NAME = "stackable_odbc_trino"
DSN_NAME = "trino_dialog_test"
TRINO_VM_HOSTNAME = "trino"

# WinRM runs each command through cmd.exe, which caps its command line at 8191
# characters, and pywinrm re-encodes the script as UTF-16 base64 on the way --
# about 2.7x. A chunk this size stays under that with room to spare. A larger
# one failed the upload *silently*, leaving the previous script in place to run
# again, which looks exactly like the new one having no effect.
UPLOAD_CHUNK = 1200

# The PowerShell every UI step is prefixed with. Kept in one string so a step
# body reads as the actions it performs.
UIA_PRELUDE = r"""
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$AE = [System.Windows.Automation.AutomationElement]
$TS = [System.Windows.Automation.TreeScope]
$TRUE_COND = [System.Windows.Automation.Condition]::TrueCondition

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class Win32 {
  [DllImport("user32.dll")]
  public static extern bool PostMessage(IntPtr h, uint msg, IntPtr wp, IntPtr lp);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")]
  public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)]
  public static extern IntPtr SendMessage(IntPtr h, uint msg, IntPtr wp, string lp);
  [DllImport("user32.dll", EntryPoint="GetWindowLongPtrW")]
  public static extern IntPtr GetWindowLongPtr(IntPtr h, int idx);
}
"@

function Find-Win([string]$Like, [int]$Tries = 40) {
  foreach ($i in 1..$Tries) {
    foreach ($w in $AE::RootElement.FindAll($TS::Children, $TRUE_COND)) {
      if ($w.Current.Name -like $Like) { return $w }
    }
    Start-Sleep -Milliseconds 500
  }
  throw "no window matching '$Like'"
}

function Find-Ctl($Win, [string]$Like, [int]$Tries = 40) {
  foreach ($i in 1..$Tries) {
    foreach ($c in $Win.FindAll($TS::Descendants, $TRUE_COND)) {
      if ($c.Current.Name -like $Like) { return $c }
    }
    Start-Sleep -Milliseconds 500
  }
  throw "no control matching '$Like' in '$($Win.Current.Name)'"
}

# The Create New Data Source dialog reports an *empty* Name to UI Automation,
# so it can only be found by a control it owns.
function Find-CtlAnywhere([string]$Name, [int]$Tries = 40) {
  foreach ($i in 1..$Tries) {
    foreach ($w in $AE::RootElement.FindAll($TS::Children, $TRUE_COND)) {
      foreach ($c in $w.FindAll($TS::Descendants, $TRUE_COND)) {
        if ($c.Current.Name -eq $Name) { return $c }
      }
    }
    Start-Sleep -Milliseconds 500
  }
  throw "no control named '$Name' on any window"
}

function Invoke-Ctl($Ctl) {
  # Posted, never sent: a button that opens a modal dialog does not return
  # from its click until that dialog closes.
  [void][Win32]::PostMessage([IntPtr]$Ctl.Current.NativeWindowHandle,
                             0x00F5, [IntPtr]::Zero, [IntPtr]::Zero)  # BM_CLICK
}

function Click-Point([int]$X, [int]$Y) {
  [void][Win32]::SetCursorPos($X, $Y)
  Start-Sleep -Milliseconds 200
  [Win32]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)   # LEFTDOWN
  [Win32]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)   # LEFTUP
  Start-Sleep -Milliseconds 400
}

function Click-Ctl($Ctl) {
  $r = $Ctl.Current.BoundingRectangle
  Click-Point ([int]($r.X + $r.Width / 2)) ([int]($r.Y + $r.Height / 2))
}

# A list view's rows are not exposed to UI Automation either -- neither the
# driver list nor the data source list has a child per row -- so a row is
# selected by clicking where it sits. The caller supplies the index, computed
# from the registry, because the control cannot be asked what it contains.
function Click-ListRow($Win, [int]$Index) {
  $list = $null
  foreach ($c in $Win.FindAll($TS::Descendants, $TRUE_COND)) {
    if ($c.Current.Name -eq "List1") { $list = $c }
  }
  if (-not $list) { throw "no list on '$($Win.Current.Name)'" }
  $r = $list.Current.BoundingRectangle
  # Below the column header, then one row height per row.
  Click-Point ([int]($r.X + 60)) ([int]($r.Y + 22 + $Index * 17 + 8))
  Start-Sleep -Milliseconds 400
}

# The tab strip is not a control with a handle, so it is clicked by position
# along the top edge of the tab control.
function Click-Tab($Win, [int]$Index) {
  $tabs = $null
  foreach ($c in $Win.FindAll($TS::Descendants, $TRUE_COND)) {
    if ($c.Current.AutomationId -eq "1") { $tabs = $c }
  }
  if (-not $tabs) { throw "no tab control" }
  $r = $tabs.Current.BoundingRectangle
  # Tab widths vary with their captions, so walk the measured offsets the
  # dialog's own six tabs sit at rather than assuming a uniform width.
  $offsets = @(34, 108, 166, 213, 258, 310)
  Click-Point ([int]($r.X + $offsets[$Index])) ([int]($r.Y + 11))
}

# A control's text is read from UI Automation's Name, never with
# GetWindowTextW: that does not retrieve an edit control's text across a
# process boundary and answers empty instead. It answered empty for a field
# that had just been filled in, which read as the write having failed and sent
# this script chasing three input mechanisms that were all working.
function Get-CtlText($Ctl) { $Ctl.Current.Name }

function Get-FieldText($Win, [string]$Label) {
  # Re-found rather than cached: an element handed out before a write can
  # answer from a stale property cache.
  (Get-FieldCtl $Win $Label).Current.Name
}

function Test-CtlReadOnly($Ctl) {
  # ES_READONLY. There is no ValuePattern to ask: the dialog exposes no UI
  # Automation provider, so every control is a raw Pane.
  $style = [int64][Win32]::GetWindowLongPtr([IntPtr]$Ctl.Current.NativeWindowHandle, -16)
  ($style -band 0x0800) -ne 0
}

# Each field is an edit box that follows its label in creation order, and the
# labels are the only named thing on a tab. Looking a control up by its label
# rather than by a fixed index matters: the descendant list gains and loses
# leading entries between dialog instances, so an index that addressed the
# name box on one run addressed its *label* on the next -- and a label has no
# ValuePattern, which is where that showed up.
function Get-FieldCtl($Win, [string]$Label) {
  $all = @($Win.FindAll($TS::Descendants, $TRUE_COND))
  for ($i = 0; $i -lt $all.Count; $i++) {
    if ($all[$i].Current.Name -ne $Label) { continue }
    # A secret's row is label, Save box, edit; a file's row is label, Browse
    # button, edit. Taking the element straight after the label therefore
    # returned a CheckBox for Password, which has no ValuePattern.
    for ($j = $i + 1; $j -lt $all.Count; $j++) {
      $n = $all[$j].Current.Name
      if ($n -eq "Save" -or $n -eq "Browse...") { continue }
      return $all[$j]
    }
  }
  throw "no field labelled '$Label'"
}

$WM_SETTEXT = 0x000C

function Set-Field($Win, [string]$Label, [string]$Value) {
  <#
      WM_SETTEXT, which replaces the whole contents and needs no focus, so a
      pre-filled field does not have to be cleared first.

      ValuePattern is not usable: the raw-window provider offers it on these
      edits only some of the time, and the same dialog answered "Unsupported
      Pattern" for every one of its controls on a later run.
  #>
  $ctl = Get-FieldCtl $Win $Label
  [void][Win32]::SendMessage([IntPtr]$ctl.Current.NativeWindowHandle,
                             $WM_SETTEXT, [IntPtr]::Zero, $Value)
  Start-Sleep -Milliseconds 150

  # Read it back. A field that did not take the value is this function's whole
  # failure mode, and it is otherwise invisible until an assertion far
  # downstream reports something unrelated.
  $got = Get-FieldText $Win $Label
  if ($got -ne $Value) { throw "field '$Label' holds '$got' after setting '$Value'" }
}
"""


class Vm:
    """The VM, reachable two ways: WinRM for state, the console for the UI."""

    def __init__(self, host, user, password, domain, verbose):
        import winrm

        self.session = winrm.Session(
            f"http://{host}:5985/wsman", auth=(user, password), transport="ntlm"
        )
        self.domain = domain
        self.verbose = verbose

    def ps(self, script, check=True):
        r = self.session.run_ps(script)
        out = r.std_out.decode(errors="replace").strip()
        err = r.std_err.decode(errors="replace").strip()
        if check and r.status_code != 0:
            print(f"  WinRM command failed ({r.status_code}): {out} {err}",
                  file=sys.stderr)
        return r.status_code, out, err

    def wake(self):
        """Wake the console's display.

        It blanks after a few minutes idle, and a blanked console screenshots
        as a solid black frame while every UI step still reports success — the
        run looks fine and the evidence is worthless.
        """
        subprocess.run(
            ["virsh", "--connect", "qemu:///system", "send-key", self.domain,
             "KEY_LEFTSHIFT"],
            check=True, capture_output=True,
        )
        time.sleep(2)

    def shot(self, name):
        SHOT_DIR.mkdir(parents=True, exist_ok=True)
        path = SHOT_DIR / f"{name}.png"
        # virsh, not an in-guest capture: WinRM is in session 0, which has no
        # desktop to photograph.
        subprocess.run(
            ["virsh", "--connect", "qemu:///system", "screenshot", self.domain, str(path)],
            check=True, capture_output=True,
        )
        print(f"    shot  {path.relative_to(TEST_DIR.parent)}")
        return path

    def ui(self, body, timeout=120):
        """Run PowerShell on the console session and return what it printed.

        A scheduled task has no stdout to capture, so the script writes its
        output to a file and a sentinel beside it, and this polls for the
        sentinel.
        """
        script = (
            UIA_PRELUDE
            + "\n& {\n  try {\n" + body
            + '\n  } catch { "ERROR: $($_.Exception.Message)" }\n}'
            + rf' | Out-File -Encoding UTF8 "{REMOTE_DIR}\_uia.out"' + "\n"
            + rf'Set-Content -Path "{REMOTE_DIR}\_uia.done" -Value ok' + "\n"
        )
        self.ps(rf'Remove-Item "{REMOTE_DIR}\_uia.out","{REMOTE_DIR}\_uia.done",'
                rf'"{REMOTE_DIR}\_uia.b64" -EA SilentlyContinue', check=False)

        b64 = base64.b64encode(script.encode("utf-8")).decode()
        for i in range(0, len(b64), UPLOAD_CHUNK):
            rc, out, err = self.ps(
                f'Add-Content -Path "{REMOTE_DIR}\\_uia.b64" '
                f'-Value "{b64[i:i + UPLOAD_CHUNK]}" -NoNewline')
            if rc != 0:
                sys.exit(f"uploading the UI script failed: {out} {err}")
        rc, out, err = self.ps(
            f'[IO.File]::WriteAllBytes("{REMOTE_DIR}\\_uia.ps1", '
            f'[Convert]::FromBase64String((Get-Content "{REMOTE_DIR}\\_uia.b64" -Raw).Trim()))')
        if rc != 0:
            sys.exit(f"writing the UI script failed: {out} {err}")

        # An instance still stuck from an earlier step would make schtasks
        # decline to start this one, since the default is IgnoreNew.
        self.ps(r'Get-CimInstance Win32_Process -Filter "Name=' + "'powershell.exe'" + r'" | '
                r'Where-Object { $_.CommandLine -like "*_uia.ps1*" } | '
                r'ForEach-Object { Stop-Process -Id $_.ProcessId -Force }', check=False)
        self.ps(r'schtasks /delete /tn "OdbcUia" /f 2>$null | Out-Null', check=False)
        rc, out, err = self.ps(
            r'schtasks /create /tn "OdbcUia" /tr '
            rf'"powershell -ExecutionPolicy Bypass -WindowStyle Hidden -File {REMOTE_DIR}\_uia.ps1" '
            r'/sc once /st 23:59 /it /f /rl highest')
        if rc != 0:
            sys.exit(f"creating the UI task failed: {out} {err}")
        self.ps(r'schtasks /run /tn "OdbcUia"')

        deadline = time.time() + timeout
        while time.time() < deadline:
            _, out, _ = self.ps(rf'Test-Path "{REMOTE_DIR}\_uia.done"', check=False)
            if out.strip() == "True":
                break
            time.sleep(1)
        _, result, _ = self.ps(
            rf'Get-Content "{REMOTE_DIR}\_uia.out" -Raw -EA SilentlyContinue', check=False)
        if self.verbose and result:
            for line in result.splitlines():
                print(f"      | {line}")
        if result.startswith("ERROR:"):
            raise RuntimeError(result.strip())
        return result

    def driver_row(self, name):
        """Which row `name` occupies in the Create New Data Source list.

        The list is a plain `SysListView32` with no UI Automation provider, so
        its items cannot be found by name and the row has to be clicked by
        position. odbcad32 lists the registered drivers in ordinal order, which
        puts every capitalised name ahead of a lower-case one -- `SQL Server`
        sorts before `stackable_odbc_trino`. Pressing Finish without selecting
        first configures whichever driver happens to be first, which on a VM
        with the stock SQL Server driver present opened *its* wizard and looked
        like the driver's own dialog failing to appear.
        """
        _, out, _ = self.ps(r'''
(Get-ItemProperty "HKLM:\SOFTWARE\ODBC\ODBCINST.INI\ODBC Drivers").PSObject.Properties |
  Where-Object { $_.Name -notlike "PS*" } | ForEach-Object { $_.Name }''')
        drivers = sorted(line.strip() for line in out.splitlines() if line.strip())
        if name not in drivers:
            sys.exit(f"{name} is not registered on the VM. Run windows_test.py first.")
        return drivers.index(name), drivers

    def dsn_row(self, name):
        """Which row `name` occupies in the Administrator's User DSN list."""
        _, out, _ = self.ps(r'''
(Get-ItemProperty "HKCU:\Software\ODBC\ODBC.INI\ODBC Data Sources" -EA SilentlyContinue).PSObject.Properties |
  Where-Object { $_.Name -notlike "PS*" } | ForEach-Object { $_.Name }''')
        names = sorted(line.strip() for line in out.splitlines() if line.strip())
        if name not in names:
            return None, names
        return names.index(name), names

    def dsn_values(self, name):
        """The data source's stored keywords, as the registry holds them."""
        _, out, _ = self.ps(rf'''
$k = "HKCU:\Software\ODBC\ODBC.INI\{name}"
if (-not (Test-Path $k)) {{ "{{}}" }} else {{
  $h = @{{}}
  (Get-ItemProperty $k).PSObject.Properties |
    Where-Object {{ $_.Name -notlike "PS*" }} |
    ForEach-Object {{ $h[$_.Name] = "$($_.Value)" }}
  $h | ConvertTo-Json -Compress
}}''', check=False)
        try:
            return json.loads(out or "{}")
        except json.JSONDecodeError:
            return {}

    def dialog_processes(self):
        _, out, _ = self.ps(r'''(Get-CimInstance Win32_Process -Filter "Name='powershell.exe'" |
  Where-Object { $_.CommandLine -like "*configure-dsn*" } | Measure-Object).Count''', check=False)
        return int(out or 0)


class Report:
    def __init__(self):
        self.failures = []

    def check(self, ok, label, detail=""):
        print(f"  {'PASS' if ok else 'FAIL'}  {label}"
              + (f"  [{detail}]" if detail and not ok else ""))
        if not ok:
            self.failures.append(f"{label}: {detail}")
        return ok


def main():
    args = parse_args()
    SHOT_DIR.mkdir(parents=True, exist_ok=True)
    vm = Vm(args.host, args.user, args.password, args.domain, args.verbose)
    r = Report()

    print("=== Preparing ===")
    reset(vm)

    print("=== The driver lists with a version and a company ===")
    driver_identity(vm, r)

    print("=== Add... ===")
    add_data_source(vm, r, args.trino_host)

    print("=== Configure... ===")
    configure_data_source(vm, r)

    print("=== Remove ===")
    remove_data_source(vm, r)

    if not args.keep_open:
        vm.ps(r'Get-Process odbcad32 -EA SilentlyContinue | Stop-Process -Force', check=False)
        cleanup(vm)

    print("")
    print("=== Dialog summary ===")
    if r.failures:
        for f in r.failures:
            print(f"  FAIL  {f}")
        print(f"{len(r.failures)} check(s) failed; see {SHOT_DIR}")
        sys.exit(1)
    print(f"all checks passed; screenshots in {SHOT_DIR}")


def reset(vm):
    """Start from no data source and no dialog, so a rerun is not a no-op."""
    vm.wake()
    # Edge's first-run page opens full screen over everything and grabs the
    # foreground. It covered a screenshot completely, and the window it
    # obscured was the one the step was waiting for.
    vm.ps(r'Get-Process msedge -EA SilentlyContinue | Stop-Process -Force', check=False)
    vm.ps(r'''
$k = "HKLM:\SOFTWARE\Policies\Microsoft\Edge"
if (-not (Test-Path $k)) { New-Item -Path $k -Force | Out-Null }
New-ItemProperty -Path $k -Name "HideFirstRunExperience" -Value 1 -PropertyType DWord -Force | Out-Null
New-ItemProperty -Path $k -Name "PreventFirstRunPage"    -Value 1 -PropertyType DWord -Force | Out-Null
''', check=False)
    vm.ps(r'''Get-CimInstance Win32_Process -Filter "Name='powershell.exe'" |
  Where-Object { $_.CommandLine -like "*configure-dsn*" } |
  ForEach-Object { Stop-Process -Id $_.ProcessId -Force }''', check=False)
    vm.ps(r'Get-Process odbcad32 -EA SilentlyContinue | Stop-Process -Force', check=False)
    vm.ps(rf'''
$idx = "HKCU:\Software\ODBC\ODBC.INI\ODBC Data Sources"
Remove-ItemProperty -Path $idx -Name "{DSN_NAME}" -Force -EA SilentlyContinue
Remove-Item -Path "HKCU:\Software\ODBC\ODBC.INI\{DSN_NAME}" -Recurse -Force -EA SilentlyContinue
''', check=False)
    rc, out, _ = vm.ps(rf'Test-Path "{REMOTE_DIR}\configure-dsn.ps1"', check=False)
    if out.strip() != "True":
        sys.exit(f"{REMOTE_DIR}\\configure-dsn.ps1 is missing. Run windows_test.py first: "
                 "the driver's ConfigDSN looks for it beside the DLL.")
    time.sleep(1)


def driver_identity(vm, r):
    """The ODBC Administrator's Version and Company columns.

    Both are read from the DLL's `VERSIONINFO` resource, which `build.rs`
    embeds. A driver without one lists as `Not marked` twice.
    """
    _, out, _ = vm.ps(rf'''
$d = Get-Item "{REMOTE_DIR}\{DRIVER_NAME}.dll"
@{{ FileVersion = "$($d.VersionInfo.FileVersion)"
    CompanyName = "$($d.VersionInfo.CompanyName)" }} | ConvertTo-Json -Compress''')
    try:
        info = json.loads(out or "{}")
    except json.JSONDecodeError:
        info = {}
    r.check(bool(info.get("FileVersion")), "the DLL carries a file version",
            f"got {info.get('FileVersion')!r}")
    r.check(info.get("CompanyName") == "Stackable GmbH", "the DLL names its company",
            f"got {info.get('CompanyName')!r}")


def add_data_source(vm, r, trino_host):
    row, drivers = vm.driver_row(DRIVER_NAME)
    print(f"  {DRIVER_NAME} is row {row} of {len(drivers)}: {drivers}")

    vm.ui(r'''
Start-Process "C:\Windows\System32\odbcad32.exe"
$w = Find-Win "ODBC Data Source Administrator*"
Start-Sleep -Seconds 1
Invoke-Ctl (Find-Ctl $w "Add...")
Start-Sleep -Seconds 2
"Add... clicked"
''')

    # Select the driver's row, then screenshot: the highlight in
    # 01_create_new_data_source.png is the evidence the right one was picked.
    vm.ui(rf'''
$list = Find-CtlAnywhere "List1"
$r = $list.Current.BoundingRectangle
# Below the column header, then one row height per row. Measured against the
# dialog at its fixed size, which it has no way to change.
Click-Point ([int]($r.X + 60)) ([int]($r.Y + 22 + {row} * 17 + 8))
Start-Sleep -Milliseconds 600
"row {row} clicked"
''')
    vm.shot("01_create_new_data_source")

    # Finish is what calls ConfigDSN(hwnd, ODBC_ADD_DSN, driver, "") with an
    # empty attribute list, which is the case core's hook ordering exists for:
    # there is no DSN keyword until the dialog has produced one.
    #
    # Finding the driver's own dialog by title is also what catches a mis-aimed
    # row click: another driver's wizard would open instead, and this reports
    # that rather than timing out somewhere later.
    vm.ui(r'''
Invoke-Ctl (Find-CtlAnywhere "Finish")
Start-Sleep -Seconds 6
$d = Find-Win "Stackable Trino ODBC*" 20
"the driver's dialog opened: " + $d.Current.Name
''')
    vm.shot("02_driver_dialog")

    out = vm.ui(r'''
$d = Find-Win "Stackable Trino ODBC*"
"name_readonly=" + (Test-CtlReadOnly (Get-FieldCtl $d "Data source name"))
$scope = $false
foreach ($c in $d.FindAll($TS::Descendants, $TRUE_COND)) {
  if ($c.Current.Name -eq "System") { $scope = $true }
}
"scope_shown=" + $scope
''')
    r.check("name_readonly=False" in out,
            "Add... leaves the data source name editable", out.strip())
    r.check("scope_shown=False" in out,
            "the User/System radios are hidden, since core performs the write",
            out.strip())

    print("  filling the dialog in")
    vm.ui(rf'''
$d = Find-Win "Stackable Trino ODBC*"
Set-Field $d "Data source name" "{DSN_NAME}"
Set-Field $d "Host" "{trino_host}"
Set-Field $d "Catalog" "tpcds"
Click-Tab $d 1
Start-Sleep -Milliseconds 800
Set-Field $d "User" "admin"
Set-Field $d "Password" "admin"
# The Save box beside a secret, off by default, so the password is written.
$all = @($d.FindAll($TS::Descendants, $TRUE_COND))
for ($i = 0; $i -lt $all.Count; $i++) {{
  if ($all[$i].Current.Name -eq "Password") {{ Click-Ctl $all[$i + 1] }}
}}
Click-Tab $d 2
Start-Sleep -Milliseconds 800
Set-Field $d "CA certificate" "{REMOTE_DIR}\ca.crt"
Click-Tab $d 0
Start-Sleep -Milliseconds 600
"filled"
''')
    vm.shot("03_dialog_filled")

    print("  testing the connection")
    vm.ui(r'''
$d = Find-Win "Stackable Trino ODBC*"
Invoke-Ctl (Find-Ctl $d "Test connection")
Start-Sleep -Seconds 12
"tested"
''', timeout=180)
    vm.shot("04_test_connection")
    out = vm.ui(r'''
$v = Find-Win "Connection succeeded*" 6
foreach ($c in $v.FindAll($TS::Descendants, $TRUE_COND)) { "  " + $c.Current.Name }
Invoke-Ctl (Find-Ctl $v "OK")
Start-Sleep -Seconds 1
"dismissed"
''')
    r.check("dismissed" in out, "Test connection reports success", out.strip())
    for field in ("Host:", "Version:", "User:", "Catalog:"):
        r.check(field in out, f"the result names {field.rstrip(':')}", out.strip())

    print("  writing the data source")
    # Asserting the dialog closed is what keeps a failure here local: a
    # lingering dialog is silently re-found by every later step, so Configure...
    # reported on the Add dialog and the screenshots for both were identical.
    vm.ui(r'''
$d = Find-Win "Stackable Trino ODBC*"
Invoke-Ctl (Find-Ctl $d "OK")
Start-Sleep -Seconds 5
$still = $true
try { $null = Find-Win "Stackable Trino ODBC*" 4 } catch { $still = $false }
if ($still) { throw "the dialog is still open after OK" }
"written"
''')
    values = vm.dsn_values(DSN_NAME)
    r.check(values.get("host") == trino_host, "the data source was written",
            f"got {values!r}")
    r.check(values.get("catalog") == "tpcds", "the keywords survived the dialog",
            f"catalog={values.get('catalog')!r}")
    # Written only because the Save box was ticked above.
    r.check(values.get("password") == "admin", "a saved secret is written",
            f"password={values.get('password')!r}")
    # SQLWriteDSNToIni adds this; the driver never writes it itself.
    r.check("Driver" in values, "core wrote the section through SQLWriteDSNToIni",
            f"keys={sorted(values)}")

    vm.ui(r'''
$w = Find-Win "ODBC Data Source Administrator*"
Click-Ctl $w
Start-Sleep -Seconds 1
"raised"
''')
    vm.shot("05_administrator_lists_it")


def configure_data_source(vm, r):
    row, names = vm.dsn_row(DSN_NAME)
    if row is None:
        sys.exit(f"{DSN_NAME} was never written; nothing to configure (have {names})")
    vm.ui(rf'''
$w = Find-Win "ODBC Data Source Administrator*"
Click-ListRow $w {row}
Invoke-Ctl (Find-Ctl $w "Configure...")
Start-Sleep -Seconds 6
"configure clicked"
''')
    vm.shot("06_configure_prefilled")

    out = vm.ui(r'''
$d = Find-Win "Stackable Trino ODBC*"
$name = Get-FieldCtl $d "Data source name"
"name_value=" + (Get-CtlText $name)
"name_readonly=" + (Test-CtlReadOnly $name)
Click-Tab $d 1
Start-Sleep -Milliseconds 800
$all = @($d.FindAll($TS::Descendants, $TRUE_COND))
for ($i = 0; $i -lt $all.Count; $i++) {
  if ($all[$i].Current.Name -eq "User")     { "user="     + (Get-CtlText (Get-FieldCtl $d "User")) }
  if ($all[$i].Current.Name -eq "Password") { "password=" + (Get-CtlText (Get-FieldCtl $d "Password")) }
}
''')
    r.check(f"name_value={DSN_NAME}" in out,
            "Configure... prefills from the stored keywords", out.strip())
    # The spec: "if a data source name was passed to it, ConfigDSN displays
    # that name but does not allow the user to change it." Core enforces it on
    # the map coming back, so an editable box would only fail the call.
    r.check("name_readonly=True" in out,
            "the data source name is read-only on Configure...", out.strip())
    r.check("user=admin" in out, "core merged the stored section in", out.strip())
    r.check("password=admin" in out, "a stored secret is prefilled", out.strip())

    print("  cancelling")
    before = vm.dsn_values(DSN_NAME)
    vm.ui(r'''
$d = Find-Win "Stackable Trino ODBC*"
Click-Tab $d 0
Start-Sleep -Milliseconds 600
Set-Field $d "Catalog" "CANCELLED_MUST_NOT_PERSIST"
Start-Sleep -Milliseconds 300
Invoke-Ctl (Find-Ctl $d "Cancel")
Start-Sleep -Seconds 5
$still = $true
try { $null = Find-Win "Stackable Trino ODBC*" 4 } catch { $still = $false }
if ($still) { throw "the dialog is still open after Cancel" }
"cancelled"
''')
    after = vm.dsn_values(DSN_NAME)
    r.check(after == before, "a cancelled dialog writes nothing",
            f"{before!r} -> {after!r}")
    # Ok(None) from the hook, which core turns into FALSE with no installer
    # error posted, so the Administrator shows nothing.
    _, boxes, _ = vm.ps(r'''(Get-Process odbcad32 -EA SilentlyContinue |
  Where-Object { $_.MainWindowTitle -like "*Error*" } | Measure-Object).Count''', check=False)
    r.check(boxes.strip() in ("0", ""), "cancelling posts no installer error", boxes)


def remove_data_source(vm, r):
    """Remove reaches the hook, which must not open a dialog for it."""
    row, names = vm.dsn_row(DSN_NAME)
    if row is None:
        sys.exit(f"{DSN_NAME} is not present; nothing to remove (have {names})")
    vm.ui(rf'''
$w = Find-Win "ODBC Data Source Administrator*"
Click-ListRow $w {row}
Invoke-Ctl (Find-Ctl $w "Remove")
Start-Sleep -Seconds 3
"remove clicked"
''')
    # The Administrator asks for confirmation itself, which is the reason the
    # driver adds none. Found by the button it owns rather than by title: the
    # window is not named what its caption suggests, and a title guess failed
    # here while the dialog was plainly on screen.
    out = vm.ui(r'''
$yes = Find-CtlAnywhere "Yes" 10
"confirm_shown=true"
Invoke-Ctl $yes
Start-Sleep -Seconds 4
"confirmed"
''')
    r.check("confirm_shown=true" in out,
            "the Administrator confirms the removal itself", out.strip())
    r.check(vm.dialog_processes() == 0,
            "Remove opens no driver dialog", "a dialog process was spawned")
    r.check(vm.dsn_values(DSN_NAME) == {}, "the data source is gone",
            f"{vm.dsn_values(DSN_NAME)!r}")


def cleanup(vm):
    vm.ps(rf'Remove-Item "{REMOTE_DIR}\_uia.*" -EA SilentlyContinue', check=False)
    vm.ps(r'schtasks /delete /tn "OdbcUia" /f 2>$null | Out-Null', check=False)


def parse_args():
    p = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--host", help="VM IP; discovered from libvirt when omitted")
    p.add_argument("--user", default="Administrator")
    p.add_argument("--password", default="Asdf1234")
    p.add_argument("--domain", default="stackable-odbc-test",
                   help="libvirt domain, for virsh screenshot")
    p.add_argument("--vm-network", default="stackable-odbc-test-hostnet")
    p.add_argument("--trino-host", default=TRINO_VM_HOSTNAME,
                   help="the Host value the dialog is filled in with. The default "
                        "is the name the VM's hosts file maps to the gateway, so "
                        "TLS sends SNI and the coordinator's certificate verifies")
    p.add_argument("--keep-open", action="store_true",
                   help="leave the Administrator running afterwards")
    p.add_argument("-v", "--verbose", action="store_true",
                   help="echo what each UI step printed")
    args = p.parse_args()
    if not args.host:
        args.host = discover_vm_ip(args.vm_network)
    return args


def discover_vm_ip(network):
    out = subprocess.run(
        ["virsh", "--connect", "qemu:///system", "net-dhcp-leases", network],
        capture_output=True, text=True, check=True).stdout
    for line in out.splitlines():
        for field in line.split():
            if "/" in field and field.count(".") == 3:
                ip = field.split("/")[0]
                print(f"Found VM at {ip}")
                return ip
    sys.exit(f"no DHCP lease on {network}; is the VM running?")


if __name__ == "__main__":
    main()
