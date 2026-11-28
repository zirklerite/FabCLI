@echo off
setlocal

:: Puts this folder (the unpacked FabCLI release) on the user PATH. The PATH editing is done by
:: add-to-path.ps1 (tool-neutral; called with -Exe fabcli.exe). cmd can't edit a long PATH safely
:: (see that script's header).
::
:: Exit codes (the script's): 0 added or already present, 1 failed, 2 skipped (declined, or no
:: terminal to ask in), 3 replaced an older FabCLI install.

set "HERE=%~dp0"
if "%HERE:~-1%"=="\" set "HERE=%HERE:~0,-1%"

set "MODE="
set "YES_FLAG="
if /i "%~1"=="--help" goto :usage
if /i "%~1"=="-h" goto :usage
if /i "%~1"=="status" set "MODE=-Status"
if /i "%~1"=="uninstall" set "MODE=-Uninstall"
if /i "%~1"=="--yes" set "YES_FLAG=-Yes"
if /i "%~1"=="-y" set "YES_FLAG=-Yes"
if /i "%~2"=="--yes" set "YES_FLAG=-Yes"
if /i "%~2"=="-y" set "YES_FLAG=-Yes"

if not exist "%HERE%\fabcli.exe" (
    echo ERROR: fabcli.exe not found next to install.bat.
    echo Unzip the whole release first ^(opening install.bat inside the zip runs it alone^),
    echo then run install.bat from the unzipped folder.
    exit /b 1
)
if not exist "%HERE%\add-to-path.ps1" (
    echo ERROR: add-to-path.ps1 not found next to install.bat. Unzip the whole release.
    echo Or add this folder to your user PATH by hand: "%HERE%"
    exit /b 1
)

:: A downloaded zip marks every file as from the internet, and a Group Policy execution policy
:: can override -ExecutionPolicy Bypass for a marked script, so unblock the release's files first
:: (inline commands aren't subject to the policy). The folder goes through %HERE% in the
:: environment, never inside PowerShell quotes, so a folder with an apostrophe works.
powershell -NoProfile -Command "Get-ChildItem -LiteralPath $env:HERE -File | Unblock-File" >nul 2>&1

powershell -NoProfile -ExecutionPolicy Bypass -File "%HERE%\add-to-path.ps1" -Exe fabcli.exe -Dir "%HERE%" %MODE% %YES_FLAG%
set "RESULT=%errorlevel%"

if defined MODE exit /b %RESULT%

echo.
if "%RESULT%"=="0" goto :next_steps
if "%RESULT%"=="3" goto :next_steps
echo PATH not changed. To use fabcli, add this folder to your user PATH by hand:
echo   %HERE%
goto :end

:next_steps
echo Next steps, in a NEW terminal ^(this one still has the old PATH^):
echo   fabcli auth login        sign in to Epic, about once a year
echo   fabcli skill install     for Claude Code: install the FabCLI skill
echo   fabcli guide             a short guide for AI agents and people
echo Later updates: fabcli update ^(no need to run install.bat again^).

:end
if not defined YES_FLAG pause
exit /b %RESULT%

:usage
echo FabCLI installer: puts this folder on your user PATH ^(no admin rights needed^).
echo.
echo Usage:
echo   install.bat              Add FabCLI to PATH ^(shows the change and asks^)
echo   install.bat --yes        Add FabCLI to PATH without asking
echo   install.bat status       Which fabcli a new terminal runs, and its version
echo   install.bat uninstall    Remove FabCLI's PATH entry ^(no files, tokens or keys are touched;
echo                            fabcli auth logout removes those^)
echo.
echo Exit codes: 0 added or already present, 1 failed, 2 skipped, 3 replaced an older install.
exit /b 0
