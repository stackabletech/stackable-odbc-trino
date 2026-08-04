@echo off
rem Install the Stackable Trino ODBC driver on Windows.
rem Must be run from an Administrator Command Prompt.
setlocal

set "INSTALL_DIR=%ProgramFiles%\Stackable\ODBC"
set "DRIVER_DLL=stackable_odbc_trino.dll"

if not exist "%~dp0%DRIVER_DLL%" (
    echo ERROR: %DRIVER_DLL% not found next to install.bat.
    exit /b 1
)

rem The driver's ConfigDSN runs this script to display its setup dialog, so the
rem ODBC Data Source Administrator's "Add..." button needs it installed
rem alongside the DLL. Checked here rather than after the copy so a missing
rem file is reported before the driver is registered.
if not exist "%~dp0configure-dsn.ps1" (
    echo ERROR: configure-dsn.ps1 not found next to install.bat.
    echo The driver needs it for the ODBC Administrator's "Add..." dialog.
    exit /b 1
)

if not exist "%INSTALL_DIR%" mkdir "%INSTALL_DIR%"

copy /Y "%~dp0%DRIVER_DLL%" "%INSTALL_DIR%\" >nul
if errorlevel 1 (
    echo ERROR: Failed to copy DLL. Are you running as Administrator?
    exit /b 1
)

rem Copied before the driver is registered, not after: registering is what makes
rem the ODBC Administrator's "Add..." button reach ConfigDSN, and ConfigDSN runs
rem this script. Doing it the other way round leaves a window in which the
rem button is live and its dialog is missing.
copy /Y "%~dp0configure-dsn.ps1" "%INSTALL_DIR%\" >nul
if errorlevel 1 (
    echo ERROR: Failed to copy configure-dsn.ps1.
    exit /b 1
)

odbcconf.exe /A {INSTALLDRIVER "stackable_odbc_trino|Driver=%INSTALL_DIR%\%DRIVER_DLL%|Setup=%INSTALL_DIR%\%DRIVER_DLL%|"}

rem odbcconf reports success whether or not the action it was given succeeded,
rem so its exit code proves nothing and the registry is asked instead. Without
rem this, a failed registration prints "installed successfully" and the driver
rem is then simply absent from the ODBC Administrator with no explanation --
rem which is the first symptom README.md's troubleshooting section covers.
rem
rem integration-tests/windows/windows_test.py works around the same unreliability
rem by force-writing these values after its own odbcconf call.
reg query "HKLM\SOFTWARE\ODBC\ODBCINST.INI\stackable_odbc_trino" /v Driver >nul 2>&1
if errorlevel 1 (
    echo ERROR: Driver registration failed: odbcconf.exe did not create
    echo   HKLM\SOFTWARE\ODBC\ODBCINST.INI\stackable_odbc_trino
    echo Are you running from an Administrator Command Prompt?
    exit /b 1
)

rem The driver is registered only if it is also listed here; the ODBC
rem Administrator reads this value to populate its Drivers tab.
reg query "HKLM\SOFTWARE\ODBC\ODBCINST.INI\ODBC Drivers" /v "stackable_odbc_trino" >nul 2>&1
if errorlevel 1 (
    echo ERROR: Driver registration is incomplete: stackable_odbc_trino is missing
    echo   from HKLM\SOFTWARE\ODBC\ODBCINST.INI\ODBC Drivers
    exit /b 1
)

echo Stackable Trino ODBC driver installed to %INSTALL_DIR%.
echo Verify with: ODBC Data Source Administrator (odbcad32.exe)
echo.
echo To create a DSN, use the ODBC Data Source Administrator's "Add..." button,
echo or run the same dialog directly:
echo   powershell -ExecutionPolicy Bypass -File "%INSTALL_DIR%\configure-dsn.ps1"
echo See README.md for the odbcconf and registry alternatives.
echo For Power BI users: see README.md for StackableTrinoODBC.mez installation steps.
endlocal
