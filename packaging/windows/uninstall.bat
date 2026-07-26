@echo off
rem Uninstall the Stackable Trino ODBC driver on Windows.
rem Must be run from an Administrator Command Prompt (cmd.exe).
setlocal

set "INSTALL_DIR=%ProgramFiles%\Stackable\ODBC"
set "DRIVER_DLL=stackable_odbc_trino.dll"
set "DRIVER_NAME=stackable_odbc_trino"

rem Remove the driver registration from the registry.
reg delete "HKLM\SOFTWARE\ODBC\ODBCINST.INI\%DRIVER_NAME%" /f >nul 2>&1
reg delete "HKLM\SOFTWARE\ODBC\ODBCINST.INI\ODBC Drivers" /v "%DRIVER_NAME%" /f >nul 2>&1

if exist "%INSTALL_DIR%\%DRIVER_DLL%" del /F /Q "%INSTALL_DIR%\%DRIVER_DLL%"

echo Stackable Trino ODBC driver uninstalled.
echo.
echo Note: StackableTrinoODBC.mez in Power BI's Custom Connectors folder must be removed manually.
echo If you created any DSNs, remove them with:
echo   reg delete "HKCU\SOFTWARE\ODBC\ODBC.INI\YourDsnName" /f
echo   reg delete "HKCU\SOFTWARE\ODBC\ODBC.INI\ODBC Data Sources" /v "YourDsnName" /f
endlocal
