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

odbcconf.exe /A {INSTALLDRIVER "stackable_odbc_trino|Driver=%INSTALL_DIR%\%DRIVER_DLL%|Setup=%INSTALL_DIR%\%DRIVER_DLL%|"}
if errorlevel 1 (
    echo ERROR: Driver registration failed.
    exit /b 1
)

copy /Y "%~dp0configure-dsn.ps1" "%INSTALL_DIR%\" >nul
if errorlevel 1 (
    echo ERROR: Failed to copy configure-dsn.ps1.
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
