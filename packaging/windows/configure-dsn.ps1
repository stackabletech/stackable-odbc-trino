<#
.SYNOPSIS
    Create or edit a Stackable Trino ODBC data source.

.DESCRIPTION
    Presents a dialog covering the driver's whole connection-string surface and
    writes the result as an ODBC data source.

    The write goes through the installer's SQLConfigDataSource, which calls the
    driver's own ConfigDSN entry point, rather than writing the registry
    directly. That keeps the driver in the loop and inherits whatever validation
    it performs.

    This is not the ODBC Data Source Administrator's "Add..." button. That
    button loads the driver's setup DLL and asks it to display a dialog, which
    this driver does not do; it answers the call headlessly instead. Run this
    script to get a dialog.

.PARAMETER Dsn
    Data source to edit. Omitted, the dialog starts empty.

.PARAMETER System
    Start on System scope (HKLM) rather than User (HKCU). Needs elevation.

.PARAMETER NoGui
    Write the data source from -Set without displaying a dialog. Intended for
    scripted installs and for testing the write path.

.PARAMETER Set
    Key/value pairs for -NoGui, keyed by connection-string keyword.

.EXAMPLE
    .\configure-dsn.ps1

.EXAMPLE
    .\configure-dsn.ps1 -Dsn trino_prod

.EXAMPLE
    .\configure-dsn.ps1 -NoGui -Set @{ DSN='trino'; Host='trino.example.com'
                                       Port='8443'; User='admin'; Catalog='tpcds' }
#>
[CmdletBinding()]
param(
    [string]$Dsn,
    [switch]$System,
    [switch]$NoGui,
    [hashtable]$Set,
    [string]$DriverName = 'stackable_odbc_trino'
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Field table
# ---------------------------------------------------------------------------
# The one place a connection-string keyword is named. Layout, the read path,
# the write path and validation are all generated from this, so adding a
# keyword is one entry here rather than an edit in four places.
#
# Key      the connection-string keyword, lower case, matching the PARAM_
#          constants in src/backend/types/connect_params.rs.
# Type     Text | Int | Bool | Enum | File | Secret | Pairs
# Pairs    a name:value;name2:value2 list. Brace-wrapped in a connection
#          string, bare in a data source; see Build-ConnectionString.
# Alias    an equivalent keyword the driver also accepts. Shown once, written
#          under Key. Recorded so the parser-vs-dialog test can account for it.
#
# `dsn_keys_match_the_connection_string_parser` in src/lib.rs fails the build
# if this list and the parser ever disagree.

$script:Fields = @(
    # --- Connection --------------------------------------------------------
    @{ Key='host';       Label='Host';          Tab='Connection'; Type='Text'; Required=$true
       Help='Trino coordinator hostname.' }
    @{ Key='port';       Label='Port';          Tab='Connection'; Type='Int';  Required=$true
       Default='8443'; Help='Coordinator port.' }
    @{ Key='protocol';   Label='Protocol';      Tab='Connection'; Type='Enum'
       Values=@('https','http'); Default='https'; Help='Transport. Default https.' }
    @{ Key='catalog';    Label='Catalog';       Tab='Connection'; Type='Text'
       Help='Default catalog.' }
    @{ Key='schema';     Label='Schema';        Tab='Connection'; Type='Text'
       Help='Default schema.' }
    @{ Key='source';     Label='Source';        Tab='Connection'; Type='Text'
       Help='Query source Trino records and can route on.' }
    @{ Key='clienttags'; Label='Client tags';   Tab='Connection'; Type='Text'
       Help='Comma-separated tags, which select a resource group.' }
    @{ Key='path';       Label='SQL path';      Tab='Connection'; Type='Text'
       Help='Default path for resolving unqualified function names.' }
    @{ Key='timezone';   Label='Time zone';     Tab='Connection'; Type='Text'
       Help='IANA zone, e.g. Europe/Berlin. Unset leaves the coordinator''s.' }
    @{ Key='locale';     Label='Locale';        Tab='Connection'; Type='Text'
       Help='Locale for locale-dependent formatting.' }
    @{ Key='clientinfo'; Label='Client info';   Tab='Connection'; Type='Text'
       Help='Free-form metadata Trino records against the query.' }
    @{ Key='tracetoken'; Label='Trace token';   Tab='Connection'; Type='Text'
       Help='Correlation token Trino records against the query.' }

    # --- Authentication ----------------------------------------------------
    @{ Key='user';       Label='User';          Tab='Authentication'; Type='Text'
       Help='Username. Optional under external authentication, where the identity provider supplies it.' }
    @{ Key='password';   Label='Password';      Tab='Authentication'; Type='Secret'
       Help='Password for Basic authentication.' }
    @{ Key='accesstoken'; Label='Access token'; Tab='Authentication'; Type='Secret'
       Alias='token'; Help='JWT bearer token.' }
    # Label kept short: the field column is 160px and a longer one wraps into
    # the row beneath it. The detail lives in the tooltip.
    @{ Key='externalauthentication'; Label='External authentication'
       Tab='Authentication'; Type='Bool'
       Help='Trino''s interactive OAuth 2.0 login. Needs https, and excludes Password and Access token.' }
    @{ Key='externalauthenticationtimeout'; Label='External auth timeout (s)'
       Tab='Authentication'; Type='Int'; Default='300'
       Help='Budget for one interactive login. Not bounded by the login timeout.' }
    @{ Key='sessionuser'; Label='Session user'; Tab='Authentication'; Type='Text'
       Help='User statements run as, while User still authenticates. Needs impersonation rights.' }
    @{ Key='roles';      Label='Roles';         Tab='Authentication'; Type='Pairs'
       Help='Authorisation role per catalog: catalog:role;catalog2:ALL' }
    @{ Key='extracredentials'; Label='Extra credentials'; Tab='Authentication'; Type='Pairs'
       Secret=$true; Help='Connector-level credentials: name:value;name2:value2' }

    # --- TLS ---------------------------------------------------------------
    @{ Key='tlsverify';  Label='Verification';  Tab='TLS'; Type='Enum'
       Values=@('full','ca','none'); Default='full'; Alias='sslverification'
       Help='full verifies chain and hostname, ca verifies the chain only (requires a CA certificate), none verifies nothing.' }
    @{ Key='certificate'; Label='CA certificate'; Tab='TLS'; Type='File'
       Help='PEM CA certificate for server verification. Required by ca.' }
    @{ Key='clientcertificate'; Label='Client certificate'; Tab='TLS'; Type='File'
       Help='One PEM holding a client certificate chain followed by its PKCS#8 key, for mutual TLS.' }

    # --- Session -----------------------------------------------------------
    @{ Key='sessionproperties'; Label='Session properties'; Tab='Session'; Type='Pairs'
       Help='name:value;name2:value2' }
    @{ Key='resourceestimates'; Label='Resource estimates'; Tab='Session'; Type='Pairs'
       Help='Scheduling hints: name:value;name2:value2' }
    @{ Key='clientcapabilities'; Label='Client capabilities'; Tab='Session'; Type='Text'
       Help='Comma-separated, on top of PARAMETRIC_DATETIME and PATH.' }
    @{ Key='encoding';   Label='Spooling encoding'; Tab='Session'; Type='Enum'
       Values=@('','json','json+zstd','json+lz4')
       Help='Spooled query-data encoding. Unset returns every row inline.' }

    # --- Proxy -------------------------------------------------------------
    @{ Key='proxy';         Label='Proxy URL';      Tab='Proxy'; Type='Text'
       Help='HTTP/HTTPS proxy for every request. Credentials in the URL are rejected.' }
    @{ Key='proxyuser';     Label='Proxy user';     Tab='Proxy'; Type='Text'
       Help='Proxy Basic username. Requires a proxy password.' }
    @{ Key='proxypassword'; Label='Proxy password'; Tab='Proxy'; Type='Secret'
       Help='Proxy Basic password.' }

    # --- Advanced ----------------------------------------------------------
    @{ Key='querytimeout'; Label='Query timeout (s)'; Tab='Advanced'; Type='Int'
       Default='30'; Alias='logintimeout'
       Help='Per-request HTTP timeout. Overridden by SQL_ATTR_CONNECTION_TIMEOUT when the application sets one.' }
    @{ Key='disablecompression'; Label='Disable compression'; Tab='Advanced'; Type='Bool'
       Help='Turn off response compression.' }
    @{ Key='maxattempts'; Label='Max attempts';   Tab='Advanced'; Type='Int'
       Help='Request retry budget. Unset leaves the client''s own default.' }
    @{ Key='extraheaders'; Label='Extra headers'; Tab='Advanced'; Type='Pairs'
       Secret=$true; Help='Extra HTTP headers: name:value;name2:value2' }
)

$script:TabOrder = @('Connection','Authentication','TLS','Session','Proxy','Advanced')

function Get-Field { param([string]$Key) $script:Fields | Where-Object { $_.Key -eq $Key } }
function Test-FieldSecret {
    param($Field)
    if ($Field.Type -eq 'Secret') { return $true }
    if ($Field.Contains('Secret') -and $Field.Secret) { return $true }
    return $false
}
function Get-FieldDefault {
    param($Field)
    if ($Field.Contains('Default')) { return $Field.Default }
    return ''
}

# ---------------------------------------------------------------------------
# ODBC installer interop
# ---------------------------------------------------------------------------

Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class OdbcInstaller {
    // BOOL, so 4 bytes: the default bool marshalling is correct here.
    [DllImport("odbccp32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern bool SQLConfigDataSourceW(IntPtr hwndParent, ushort fRequest,
        string lpszDriver, string lpszAttributes);

    // RETCODE is SQLSMALLINT: 16 bits. Declaring this as bool reads the wrong
    // width and loses the error record entirely.
    [DllImport("odbccp32.dll", CharSet = CharSet.Unicode)]
    public static extern short SQLInstallerErrorW(ushort iError, out int pfErrorCode,
        StringBuilder lpszErrorMsg, ushort cbErrorMsgMax, out ushort pcbErrorMsg);

    [DllImport("odbccp32.dll", CharSet = CharSet.Unicode)]
    public static extern int SQLGetPrivateProfileStringW(string lpszSection, string lpszEntry,
        string lpszDefault, StringBuilder RetBuffer, int cbRetBuffer, string lpszFilename);

    [DllImport("odbccp32.dll")]
    public static extern bool SQLSetConfigMode(ushort wConfigMode);
}
"@ -ErrorAction SilentlyContinue

# ConfigDSN fRequest values, from odbcinst.h.
$script:ODBC_ADD_DSN        = 1
$script:ODBC_CONFIG_DSN     = 2
$script:ODBC_ADD_SYS_DSN    = 4
$script:ODBC_CONFIG_SYS_DSN = 5
# SQLSetConfigMode values.
$script:ODBC_USER_DSN   = 1
$script:ODBC_SYSTEM_DSN = 2

function Test-Elevated {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    (New-Object Security.Principal.WindowsPrincipal $id).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-InstallerErrors {
    <# Drain the installer error buffer. Empty when the last call succeeded. #>
    $out = @()
    for ($i = 1; $i -le 8; $i++) {
        $code = 0; $pcb = 0
        $sb = New-Object System.Text.StringBuilder 1024
        $rc = [OdbcInstaller]::SQLInstallerErrorW([uint16]$i, [ref]$code, $sb, [uint16]1024, [ref]$pcb)
        # SQL_SUCCESS = 0, SQL_SUCCESS_WITH_INFO = 1; anything else ends the list.
        if ($rc -ne 0 -and $rc -ne 1) { break }
        $out += "[$code] $($sb.ToString())"
    }
    $out
}

function Read-Dsn {
    <#
        Pre-fill from an existing data source. Returns a hashtable keyed by
        connection-string keyword, holding only the keywords actually present.
    #>
    param([string]$Name, [bool]$IsSystem)

    $mode = if ($IsSystem) { $script:ODBC_SYSTEM_DSN } else { $script:ODBC_USER_DSN }
    [void][OdbcInstaller]::SQLSetConfigMode([uint16]$mode)

    $values = @{}
    foreach ($f in $script:Fields) {
        $sb = New-Object System.Text.StringBuilder 4096
        $n = [OdbcInstaller]::SQLGetPrivateProfileStringW($Name, $f.Key, '', $sb, 4096, 'ODBC.INI')
        if ($n -gt 0) {
            $values[$f.Key] = $sb.ToString()
            continue
        }
        # A data source written by hand may carry the alias instead.
        if ($f.Contains('Alias')) {
            $sb2 = New-Object System.Text.StringBuilder 4096
            $n2 = [OdbcInstaller]::SQLGetPrivateProfileStringW($Name, $f.Alias, '', $sb2, 4096, 'ODBC.INI')
            if ($n2 -gt 0) { $values[$f.Key] = $sb2.ToString() }
        }
    }
    [void][OdbcInstaller]::SQLSetConfigMode(0)
    $values
}

function Get-ExistingDsnNames {
    param([bool]$IsSystem)
    $hive = if ($IsSystem) { 'HKLM:' } else { 'HKCU:' }
    $path = "$hive\SOFTWARE\ODBC\ODBC.INI\ODBC Data Sources"
    if (-not (Test-Path $path)) { return @() }
    $item = Get-Item $path
    $item.GetValueNames() | Where-Object { $item.GetValue($_) -eq $DriverName } | Sort-Object
}

function Write-Dsn {
    <#
        Write the data source through the driver's own ConfigDSN.

        Pairs values are written bare. The braces the five name:value keys need
        belong to connection-string syntax, where `;` separates parameters; a
        data source keeps each value in its own registry value, so a brace here
        would be stored as part of the value.
    #>
    param([hashtable]$Values, [string]$Name, [bool]$IsSystem, [bool]$Replace)

    $pairs = @("DSN=$Name")
    foreach ($f in $script:Fields) {
        if (-not $Values.Contains($f.Key)) { continue }
        $v = $Values[$f.Key]
        if ([string]::IsNullOrEmpty($v)) { continue }
        $pairs += "$($f.Key)=$v"
    }
    # ConfigDSN takes a doubly null-terminated list of keyword-value pairs.
    $attributes = ($pairs -join "`0") + "`0"

    $request = if ($IsSystem) {
        if ($Replace) { $script:ODBC_CONFIG_SYS_DSN } else { $script:ODBC_ADD_SYS_DSN }
    } else {
        if ($Replace) { $script:ODBC_CONFIG_DSN } else { $script:ODBC_ADD_DSN }
    }

    [void](Get-InstallerErrors)   # clear anything stale before the call
    $ok = [OdbcInstaller]::SQLConfigDataSourceW([IntPtr]::Zero, [uint16]$request,
                                                $DriverName, $attributes)
    if (-not $ok) {
        # @() around the call: PowerShell unrolls an empty array return to
        # $null, and Set-StrictMode makes .Count on it an error.
        $errs = @(Get-InstallerErrors)
        $detail = if ($errs.Count) { $errs -join "`r`n" } else { 'the installer reported no detail' }
        throw "Writing the data source failed:`r`n$detail"
    }
}

function Build-ConnectionString {
    <#
        A DSN-less connection string for the Test button, so a configuration is
        proved before it is written.

        The five name:value;name2:value2 keys are brace-wrapped here and only
        here: `;` separates connection-string parameters, so an unbraced value
        would be truncated at its first `;` and every pair but the first would
        be dropped as an unrecognised parameter.
    #>
    param([hashtable]$Values)

    $parts = @("Driver=$DriverName")
    foreach ($f in $script:Fields) {
        if (-not $Values.Contains($f.Key)) { continue }
        $v = $Values[$f.Key]
        if ([string]::IsNullOrEmpty($v)) { continue }
        if ($f.Type -eq 'Pairs') { $v = '{' + $v.Trim('{','}') + '}' }
        $parts += "$($f.Key)=$v"
    }
    ($parts -join ';') + ';'
}

function Test-DsnConnection {
    param([hashtable]$Values)

    $cs = Build-ConnectionString $Values
    $conn = New-Object System.Data.Odbc.OdbcConnection $cs
    # Bounds the attempt so an unreachable coordinator cannot hang the dialog.
    $timeout = 15
    if ($Values.Contains('querytimeout') -and $Values['querytimeout']) {
        [void][int]::TryParse($Values['querytimeout'], [ref]$timeout)
    }
    $conn.ConnectionTimeout = $timeout
    try {
        $conn.Open()
        $cmd = $conn.CreateCommand()
        $cmd.CommandText = 'SELECT version(), current_user'
        $r = $cmd.ExecuteReader()
        $msg = if ($r.Read()) { "Connected.`r`n`r`nServer: $($r[0])`r`nUser: $($r[1])" } else { 'Connected.' }
        $r.Close()
        return @{ Ok = $true; Message = $msg }
    } catch {
        return @{ Ok = $false; Message = $_.Exception.Message }
    } finally {
        if ($conn.State -ne 'Closed') { $conn.Close() }
    }
}

function Test-Values {
    <#
        Only the rules that are cheap and certain here. Everything else is left
        to the driver, which is the authority and reports through
        SQLGetDiagRec; duplicating its rules would let the two disagree.
    #>
    param([hashtable]$Values, [string]$Name)

    $problems = @()
    if ([string]::IsNullOrWhiteSpace($Name)) { $problems += 'A data source name is required.' }
    foreach ($f in $script:Fields | Where-Object { $_.Contains('Required') -and $_.Required }) {
        if (-not $Values.Contains($f.Key) -or [string]::IsNullOrWhiteSpace($Values[$f.Key])) {
            $problems += "$($f.Label) is required."
        }
    }
    foreach ($f in $script:Fields | Where-Object { $_.Type -eq 'Int' }) {
        if ($Values.Contains($f.Key) -and $Values[$f.Key]) {
            $n = 0
            if (-not [int]::TryParse($Values[$f.Key], [ref]$n)) {
                $problems += "$($f.Label) must be a whole number."
            }
        }
    }
    # ca verifies the chain without binding it to a hostname, which rustls
    # permits only against an explicitly supplied trust store.
    if ($Values.Contains('tlsverify') -and $Values['tlsverify'] -eq 'ca') {
        if (-not $Values.Contains('certificate') -or -not $Values['certificate']) {
            $problems += 'Verification "ca" requires a CA certificate.'
        }
    }
    $problems
}

# ---------------------------------------------------------------------------
# Headless path
# ---------------------------------------------------------------------------

if ($NoGui) {
    if (-not $Set) { throw '-NoGui requires -Set.' }

    $values = @{}
    $name = $Dsn
    foreach ($k in $Set.Keys) {
        $lk = "$k".ToLowerInvariant()
        if ($lk -eq 'dsn') { $name = $Set[$k]; continue }
        $f = Get-Field $lk
        if (-not $f) {
            # Try the aliases before rejecting: a value lifted from a JDBC URL
            # or an existing connection string should transfer unchanged.
            $f = $script:Fields | Where-Object { $_.Contains('Alias') -and $_.Alias -eq $lk }
            if (-not $f) { throw "Unknown connection-string keyword: $k" }
        }
        $values[$f.Key] = "$($Set[$k])"
    }

    $problems = @(Test-Values $values $name)
    if ($problems.Count) { throw ($problems -join "`r`n") }

    if ($System -and -not (Test-Elevated)) {
        throw 'A System data source needs an elevated session. Run as Administrator, or omit -System.'
    }
    $exists = @(Get-ExistingDsnNames ([bool]$System)) -contains $name
    Write-Dsn $values $name ([bool]$System) $exists
    $scope = if ($System) { 'System' } else { 'User' }
    Write-Output "$scope data source '$name' written."
    return
}

# ---------------------------------------------------------------------------
# Dialog
# ---------------------------------------------------------------------------

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
[System.Windows.Forms.Application]::EnableVisualStyles()

$form = New-Object System.Windows.Forms.Form
$form.Text = 'Stackable Trino ODBC - Data Source'
$form.Size = New-Object System.Drawing.Size(620, 560)
$form.StartPosition = 'CenterScreen'
$form.FormBorderStyle = 'FixedDialog'
$form.MaximizeBox = $false

$tip = New-Object System.Windows.Forms.ToolTip
$tip.AutoPopDelay = 20000

# --- header: name and scope ---
$lblName = New-Object System.Windows.Forms.Label
$lblName.Text = 'Data source name'
$lblName.Location = New-Object System.Drawing.Point(12, 15)
$lblName.Size = New-Object System.Drawing.Size(130, 20)
$form.Controls.Add($lblName)

$txtName = New-Object System.Windows.Forms.TextBox
$txtName.Location = New-Object System.Drawing.Point(148, 12)
$txtName.Size = New-Object System.Drawing.Size(200, 22)
$form.Controls.Add($txtName)

$rbUser = New-Object System.Windows.Forms.RadioButton
$rbUser.Text = 'User'
$rbUser.Location = New-Object System.Drawing.Point(370, 11)
$rbUser.Size = New-Object System.Drawing.Size(60, 24)
$rbUser.Checked = -not $System
$form.Controls.Add($rbUser)

$rbSystem = New-Object System.Windows.Forms.RadioButton
$rbSystem.Text = 'System'
$rbSystem.Location = New-Object System.Drawing.Point(434, 11)
$rbSystem.Size = New-Object System.Drawing.Size(80, 24)
$rbSystem.Checked = [bool]$System
$form.Controls.Add($rbSystem)

$lblElev = New-Object System.Windows.Forms.Label
$lblElev.Location = New-Object System.Drawing.Point(370, 36)
$lblElev.Size = New-Object System.Drawing.Size(220, 18)
$lblElev.ForeColor = [System.Drawing.Color]::FromArgb(160, 90, 0)
if (-not (Test-Elevated)) {
    $lblElev.Text = 'System needs an elevated session'
    $rbSystem.Enabled = $false
    if ($System) { $rbUser.Checked = $true }
}
$form.Controls.Add($lblElev)

# --- tabs, built from the field table ---
$tabs = New-Object System.Windows.Forms.TabControl
$tabs.Location = New-Object System.Drawing.Point(12, 62)
$tabs.Size = New-Object System.Drawing.Size(580, 400)
$form.Controls.Add($tabs)

$script:Controls = @{}
$script:SaveSecret = @{}

foreach ($tabName in $script:TabOrder) {
    $page = New-Object System.Windows.Forms.TabPage
    $page.Text = $tabName
    $page.AutoScroll = $true

    $y = 14
    foreach ($f in $script:Fields | Where-Object { $_.Tab -eq $tabName }) {
        $label = New-Object System.Windows.Forms.Label
        $label.Text = $f.Label
        $label.Location = New-Object System.Drawing.Point(12, ($y + 3))
        $label.Size = New-Object System.Drawing.Size(160, 20)
        $page.Controls.Add($label)

        $ctl = $null
        switch ($f.Type) {
            'Bool' {
                $ctl = New-Object System.Windows.Forms.CheckBox
                $ctl.Location = New-Object System.Drawing.Point(178, $y)
                $ctl.Size = New-Object System.Drawing.Size(24, 22)
            }
            'Enum' {
                $ctl = New-Object System.Windows.Forms.ComboBox
                $ctl.DropDownStyle = 'DropDownList'
                $ctl.Location = New-Object System.Drawing.Point(178, $y)
                $ctl.Size = New-Object System.Drawing.Size(180, 22)
                foreach ($v in $f.Values) { [void]$ctl.Items.Add($v) }
                $ctl.SelectedItem = (Get-FieldDefault $f)
            }
            'File' {
                $ctl = New-Object System.Windows.Forms.TextBox
                $ctl.Location = New-Object System.Drawing.Point(178, $y)
                $ctl.Size = New-Object System.Drawing.Size(280, 22)
                $browse = New-Object System.Windows.Forms.Button
                $browse.Text = 'Browse...'
                $browse.Location = New-Object System.Drawing.Point(464, ($y - 1))
                $browse.Size = New-Object System.Drawing.Size(80, 24)
                $target = $ctl
                $browse.Add_Click({
                    $dlg = New-Object System.Windows.Forms.OpenFileDialog
                    $dlg.Filter = 'PEM files (*.pem;*.crt)|*.pem;*.crt|All files (*.*)|*.*'
                    if ($dlg.ShowDialog() -eq 'OK') { $target.Text = $dlg.FileName }
                }.GetNewClosure())
                $page.Controls.Add($browse)
            }
            default {
                $ctl = New-Object System.Windows.Forms.TextBox
                $ctl.Location = New-Object System.Drawing.Point(178, $y)
                if (Test-FieldSecret $f) {
                    # Narrower, to leave room for the Save box beside it.
                    $ctl.Size = New-Object System.Drawing.Size(286, 22)
                    $ctl.UseSystemPasswordChar = $true
                } else {
                    $ctl.Size = New-Object System.Drawing.Size(366, 22)
                }
                $ctl.Text = (Get-FieldDefault $f)
            }
        }

        # A data source keeps its values as plain registry values, so a saved
        # secret is stored unencrypted, and a System data source puts it in
        # HKLM where every local user can read it. Off by default: the
        # application supplies the secret at connect time unless the person
        # configuring it deliberately asks for the opposite.
        if (Test-FieldSecret $f) {
            $save = New-Object System.Windows.Forms.CheckBox
            $save.Text = 'Save'
            $save.Location = New-Object System.Drawing.Point(470, ($y + 1))
            $save.Size = New-Object System.Drawing.Size(70, 22)
            $save.Checked = $false
            $tip.SetToolTip($save, 'Store this value in the data source. It is written unencrypted.')
            $page.Controls.Add($save)
            $script:SaveSecret[$f.Key] = $save
        }

        if ($f.Contains('Help')) { $tip.SetToolTip($ctl, $f.Help) }
        $page.Controls.Add($ctl)
        $script:Controls[$f.Key] = $ctl
        $y += 30
    }

    [void]$tabs.TabPages.Add($page)
}

function Get-FormValues {
    <#
        .PARAMETER IncludeUnsavedSecrets
            Include secrets whose Save box is clear. Test connection needs
            them, since the point of testing before writing is to try a value
            you are not going to store. The write path must not.
    #>
    param([switch]$IncludeUnsavedSecrets)

    $values = @{}
    foreach ($f in $script:Fields) {
        $ctl = $script:Controls[$f.Key]
        $v = switch ($f.Type) {
            'Bool' { if ($ctl.Checked) { 'true' } else { '' } }
            'Enum' { if ($null -eq $ctl.SelectedItem) { '' } else { "$($ctl.SelectedItem)" } }
            default { $ctl.Text }
        }
        if ((Test-FieldSecret $f) -and -not $IncludeUnsavedSecrets) {
            if (-not $script:SaveSecret[$f.Key].Checked) { continue }
        }
        if (-not [string]::IsNullOrEmpty($v)) { $values[$f.Key] = $v }
    }
    $values
}

function Set-FormValues {
    param([hashtable]$Values)
    foreach ($f in $script:Fields) {
        if (-not $Values.Contains($f.Key)) { continue }
        $ctl = $script:Controls[$f.Key]
        $v = $Values[$f.Key]
        switch ($f.Type) {
            'Bool' { $ctl.Checked = ($v -match '^(?i:true|1|yes)$') }
            'Enum' { if ($ctl.Items.Contains($v)) { $ctl.SelectedItem = $v } }
            default { $ctl.Text = $v }
        }
        # A secret that is already in the data source is already saved, so
        # leaving the box clear would silently drop it on the next OK.
        if (Test-FieldSecret $f) { $script:SaveSecret[$f.Key].Checked = $true }
    }
}

# --- buttons ---
$lblSecrets = New-Object System.Windows.Forms.Label
$lblSecrets.Text = 'Secrets are only stored when "Save" is ticked, and are written unencrypted.'
$lblSecrets.Location = New-Object System.Drawing.Point(12, 506)
$lblSecrets.Size = New-Object System.Drawing.Size(580, 18)
$lblSecrets.ForeColor = [System.Drawing.Color]::FromArgb(110, 110, 110)
$form.Controls.Add($lblSecrets)

$btnTest = New-Object System.Windows.Forms.Button
$btnTest.Text = 'Test connection'
$btnTest.Location = New-Object System.Drawing.Point(12, 474)
$btnTest.Size = New-Object System.Drawing.Size(130, 30)
$btnTest.Add_Click({
    $values = Get-FormValues -IncludeUnsavedSecrets
    $problems = @(Test-Values $values $txtName.Text)
    if ($problems.Count) {
        [void][System.Windows.Forms.MessageBox]::Show(($problems -join "`r`n"),
            'Incomplete', 'OK', 'Warning')
        return
    }
    $form.Cursor = [System.Windows.Forms.Cursors]::WaitCursor
    $btnTest.Enabled = $false
    try { $result = Test-DsnConnection $values }
    finally { $form.Cursor = [System.Windows.Forms.Cursors]::Default; $btnTest.Enabled = $true }
    $icon = if ($result.Ok) { 'Information' } else { 'Error' }
    $title = if ($result.Ok) { 'Connection succeeded' } else { 'Connection failed' }
    [void][System.Windows.Forms.MessageBox]::Show($result.Message, $title, 'OK', $icon)
})
$form.Controls.Add($btnTest)

$btnOk = New-Object System.Windows.Forms.Button
$btnOk.Text = 'OK'
$btnOk.Location = New-Object System.Drawing.Point(406, 474)
$btnOk.Size = New-Object System.Drawing.Size(90, 30)
$btnOk.Add_Click({
    $values = Get-FormValues
    $problems = @(Test-Values $values $txtName.Text)
    if ($problems.Count) {
        [void][System.Windows.Forms.MessageBox]::Show(($problems -join "`r`n"),
            'Incomplete', 'OK', 'Warning')
        return
    }
    $isSystem = $rbSystem.Checked
    $exists = @(Get-ExistingDsnNames $isSystem) -contains $txtName.Text
    try {
        Write-Dsn $values $txtName.Text $isSystem $exists
    } catch {
        [void][System.Windows.Forms.MessageBox]::Show($_.Exception.Message,
            'Could not write the data source', 'OK', 'Error')
        return
    }
    $form.DialogResult = [System.Windows.Forms.DialogResult]::OK
    $form.Close()
})
$form.Controls.Add($btnOk)

$btnCancel = New-Object System.Windows.Forms.Button
$btnCancel.Text = 'Cancel'
$btnCancel.Location = New-Object System.Drawing.Point(502, 474)
$btnCancel.Size = New-Object System.Drawing.Size(90, 30)
$btnCancel.Add_Click({ $form.DialogResult = [System.Windows.Forms.DialogResult]::Cancel; $form.Close() })
$form.Controls.Add($btnCancel)
$form.CancelButton = $btnCancel

# --- pre-fill when editing ---
if ($Dsn) {
    $txtName.Text = $Dsn
    $existing = Read-Dsn $Dsn ([bool]$System)
    if ($existing.Count) { Set-FormValues $existing }
}

$result = $form.ShowDialog()
if ($result -eq [System.Windows.Forms.DialogResult]::OK) {
    $scope = if ($rbSystem.Checked) { 'System' } else { 'User' }
    Write-Output "$scope data source '$($txtName.Text)' written."
}
