@echo off
setlocal
rem Usage: cri_folder_search.bat [folder_path_A] [fromtime] [totime]
rem   folder_path_A\<yyyymmdd>\WTCI2638SD31*
rem Only the date folders from [fromtime] to [totime] (inclusive) are searched;
rem date folders that do not exist are skipped.
rem Each WTCI2638SD31* folder is checked for MTR_pass.flg / MTR_fail.flg and
rem the result is written to an Excel report.

if "%~3"=="" (
    echo Usage: %~nx0 [folder_path_A] [fromtime] [totime]
    echo Example: %~nx0 D:\data 20261027 20261102
    exit /b 1
)
if not exist "%~1\" (
    echo ERROR: folder "%~1" not found.
    exit /b 1
)

set "ROOT=%~f1"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"
set "FROM=%~2"
set "TO=%~3"
set "PREFIX=WTCI2638SD31"
set "CSV=%~dp0cri_folder_search_result.csv"
set "XLSX=%~dp0cri_folder_search_result.xlsx"

echo %FROM%| findstr /r "^[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]$" >nul || (echo ERROR: fromtime must be yyyymmdd.& exit /b 1)
echo %TO%| findstr /r "^[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]$" >nul || (echo ERROR: totime must be yyyymmdd.& exit /b 1)
if "%FROM%" gtr "%TO%" (
    echo ERROR: fromtime must not be later than totime.
    exit /b 1
)

rem Step 1: build the list from the date folders in range
> "%CSV%" echo Date,Folder Name,Path,Result
for /d %%D in ("%ROOT%\*") do call :datefolder "%%~fD"

rem Step 2: convert the list to Excel
if exist "%XLSX%" del "%XLSX%"
powershell -NoProfile -ExecutionPolicy Bypass -Command "try { $xl = New-Object -ComObject Excel.Application } catch { Write-Host 'Excel not available; CSV only.'; exit 2 }; $xl.DisplayAlerts = $false; $wb = $xl.Workbooks.Open($env:CSV); $ws = $wb.Worksheets.Item(1); $ws.Name = 'MTR Result'; $ws.Columns.Item(1).NumberFormat = '@'; $ws.Rows.Item(1).Font.Bold = $true; $ws.Columns.AutoFit() | Out-Null; $wb.SaveAs($env:XLSX, 51); $wb.Close($false); $xl.Quit()"
if errorlevel 1 (
    echo Excel file was not created. List saved as: %CSV%
    exit /b 1
)

del "%CSV%"
echo Done. Report saved to: %XLSX%
exit /b 0

:datefolder
rem %~1 = full path of a level-1 folder; only yyyymmdd names within range are used
set "DNAME=%~nx1"
echo %DNAME%| findstr /r "^[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]$" >nul || exit /b
if "%DNAME%" lss "%FROM%" exit /b
if "%DNAME%" gtr "%TO%" exit /b
for /d %%B in ("%~1\%PREFIX%*") do call :check "%%~fB"
exit /b

:check
set "RESULT=unknown"
for /r "%~1" %%F in (MTR_pass.flg) do if exist "%%~fF" set "RESULT=pass"
for /r "%~1" %%F in (MTR_fail.flg) do if exist "%%~fF" set "RESULT=fail"
>> "%CSV%" echo "%DNAME%","%~nx1","%~1",%RESULT%
exit /b
