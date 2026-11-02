@echo off
setlocal
rem Usage: dataanalysis.bat [folder_path_A]
rem Scans folder_path_A up to 2 levels deep for folders named WTCI2638SD31*,
rem checks each for MTR_pass.flg / MTR_fail.flg and writes an Excel report.

if "%~1"=="" (
    echo Usage: %~nx0 [folder_path_A]
    exit /b 1
)
if not exist "%~1\" (
    echo ERROR: folder "%~1" not found.
    exit /b 1
)

set "ROOT=%~f1"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"
set "PREFIX=WTCI2638SD31"
set "CSV=%~dp0dataanalysis_result.csv"
set "XLSX=%~dp0dataanalysis_result.xlsx"

rem Step 1: build the list (level 1 and level 2 under ROOT)
> "%CSV%" echo Folder Name,Path,Result
for /d %%A in ("%ROOT%\%PREFIX%*") do call :check "%%~fA"
for /d %%A in ("%ROOT%\*") do (
    for /d %%B in ("%%~fA\%PREFIX%*") do call :check "%%~fB"
)

rem Step 2: convert the list to Excel
if exist "%XLSX%" del "%XLSX%"
powershell -NoProfile -ExecutionPolicy Bypass -Command "try { $xl = New-Object -ComObject Excel.Application } catch { Write-Host 'Excel not available; CSV only.'; exit 2 }; $xl.DisplayAlerts = $false; $wb = $xl.Workbooks.Open($env:CSV); $ws = $wb.Worksheets.Item(1); $ws.Name = 'MTR Result'; $ws.Rows.Item(1).Font.Bold = $true; $ws.Columns.AutoFit() | Out-Null; $wb.SaveAs($env:XLSX, 51); $wb.Close($false); $xl.Quit()"
if errorlevel 1 (
    echo Excel file was not created. List saved as: %CSV%
    exit /b 1
)

del "%CSV%"
echo Done. Report saved to: %XLSX%
exit /b 0

:check
set "RESULT=unknown"
if exist "%~1\MTR_pass.flg" set "RESULT=pass"
if exist "%~1\MTR_fail.flg" set "RESULT=fail"
>> "%CSV%" echo "%~nx1","%~1",%RESULT%
exit /b
