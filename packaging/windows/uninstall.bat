@echo off
rem Uninstall the Stackable Trino ODBC driver on Windows.
rem Must be run from an Administrator Command Prompt (cmd.exe).
setlocal

set "INSTALL_DIR=%ProgramFiles%\Stackable\ODBC"
set "DRIVER_DLL=stackable_odbc_trino.dll"
set "DIALOG_SCRIPT=configure-dsn.ps1"
set "DRIVER_NAME=stackable_odbc_trino"

rem Remove the driver registration from the registry.
reg delete "HKLM\SOFTWARE\ODBC\ODBCINST.INI\%DRIVER_NAME%" /f >nul 2>&1
reg delete "HKLM\SOFTWARE\ODBC\ODBCINST.INI\ODBC Drivers" /v "%DRIVER_NAME%" /f >nul 2>&1

rem Both files install.bat placed, not just the driver: it copies the dialog
rem script beside the DLL and refuses to run without it, so leaving the script
rem behind leaves half an installation.
if exist "%INSTALL_DIR%\%DRIVER_DLL%" del /F /Q "%INSTALL_DIR%\%DRIVER_DLL%"
if exist "%INSTALL_DIR%\%DIALOG_SCRIPT%" del /F /Q "%INSTALL_DIR%\%DIALOG_SCRIPT%"

rem Remove the directories this installer created, innermost first, and only
rem when empty: rmdir without /S fails on a non-empty directory, which is the
rem wanted behaviour if an administrator put something else in there.
rem %ProgramFiles%\Stackable is removed too, but only if this was the last
rem Stackable product on the machine.
if exist "%INSTALL_DIR%" rmdir "%INSTALL_DIR%" >nul 2>&1
if exist "%ProgramFiles%\Stackable" rmdir "%ProgramFiles%\Stackable" >nul 2>&1

echo Stackable Trino ODBC driver uninstalled.
echo.
echo Note: StackableTrinoODBC.mez in Power BI's Custom Connectors folder must be removed manually.
echo If you created any DSNs, remove them with:
echo   reg delete "HKCU\SOFTWARE\ODBC\ODBC.INI\YourDsnName" /f
echo   reg delete "HKCU\SOFTWARE\ODBC\ODBC.INI\ODBC Data Sources" /v "YourDsnName" /f
endlocal
