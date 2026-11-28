# Puts a tool's folder on the user PATH (HKCU\Environment\Path), or takes it off, safely.
#
# Tool-neutral: the caller names its program with -Exe (install.bat passes -Exe fabcli.exe), so
# the same file serves any CLI shipped as a folder with an .exe in it. Done in PowerShell rather
# than batch because cmd cannot do this safely: `for /f` silently reads nothing once the PATH is
# ~8 KB, delayed expansion strips `!` and `^` from entries, and `echo ... | findstr` breaks on
# `&`; `setx` truncates at 1024 characters.
#
# The PATH is the user's whole environment, so only the tool's own entries ever change:
# - An older install is a folder holding <Exe> and a BUILD_INFO.txt with a version= line (a
#   release folder). Any other folder holding <Exe> (C:\tools, a cargo target\release) is never
#   removed, only warned about. Relative and network (\\server) entries are never looked into,
#   so they are kept as they are.
# - Install: -Dir takes the place of the first older install (or keeps its own entry when that
#   comes first); the other older installs are removed. With none, -Dir is appended (before
#   trailing empty entries).
# - Uninstall: -Dir's own entries and the older installs are removed.
# - Every other entry is written back exactly as read: the value is read without expanding
#   %VARS%, split on ';' keeping empty entries, and edited by index.
# - Before writing: the preview (with which <Exe> a new terminal would run) and prompt, a backup
#   of the old value (with its registry type) in %LOCALAPPDATA%\<tool>\path-backups, a second
#   read that must match the first; after: a read back, and the old value put back on a
#   mismatch. The registry type is kept (REG_EXPAND_SZ only when there was no value).
#
# -Status only reports which <Exe> a new terminal would run, and changes nothing.
#
# Exit codes: 0 = done (added, removed, or nothing to change), 1 = failed, 2 = skipped (declined,
# or no terminal to confirm in), 3 = added in place of an older install.
#
# -Key, -SystemPath, -BackupDir and -TestHookBeforeWrite are for tests.

param(
    # The program's file name, such as fabcli.exe.
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$Dir,
    [switch]$Yes,
    [switch]$Uninstall,
    # Only report which <Exe> a new terminal runs; changes nothing.
    [switch]$Status,
    # Registry key under HKCU holding the PATH value.
    [string]$Key = 'Environment',
    # The system PATH to check which <Exe> runs first; default: the machine's.
    [string]$SystemPath,
    [string]$BackupDir,
    # PowerShell code run between the confirmation and the second read.
    [string]$TestHookBeforeWrite
)

$ErrorActionPreference = 'Stop'
$MaxLength = 32767

if ($Exe -notmatch '^[A-Za-z0-9._-]+\.exe$') {
    Write-Host "  -Exe must be a program's file name, such as fabcli.exe (got: $Exe)."
    exit 1
}
$Tool = [IO.Path]::GetFileNameWithoutExtension($Exe)

function Fail([string[]]$lines) {
    foreach ($line in $lines) { Write-Host "  $line" }
    if (-not $Uninstall) { Write-Host "  Add manually: $Dir" }
    exit 1
}

function Normalize([string]$p) {
    $p = [Environment]::ExpandEnvironmentVariables($p.Trim().Trim('"')).Replace('/', '\')
    if ($p.Length -gt 3) { $p = $p.TrimEnd('\') }   # keep "C:\" intact
    return $p
}

function HasFile([string]$folder, [string]$name) {
    try { return Test-Path -LiteralPath (Join-Path $folder $name) -PathType Leaf } catch { return $false }
}

# What an entry is: own (the -Dir folder), old (an older install, with Version), shared (a
# folder holding <Exe> that isn't an install), or other; Path is the folder. Cached per
# folder: each one is looked into once (a disconnected mapped drive can take seconds).
$classified = @{}
function Classify([string]$entry) {
    $n = Normalize $entry
    if (-not $classified.ContainsKey($n)) { $classified[$n] = ClassifyFolder $n }
    return $classified[$n]
}
function ClassifyFolder([string]$n) {
    if ($n -ieq $target) { return @{ Kind = 'own'; Path = $n } }
    # Only local absolute folders (C:\...) are looked into.
    if ($n -notmatch '^[A-Za-z]:\\' -or -not (HasFile $n $Exe)) { return @{ Kind = 'other' } }
    if (HasFile $n 'BUILD_INFO.txt') {
        try {
            foreach ($line in [IO.File]::ReadAllLines((Join-Path $n 'BUILD_INFO.txt'))) {
                if ($line -match '^version=(.+)$') { return @{ Kind = 'old'; Path = $n; Version = "v$($Matches[1].Trim())" } }
            }
        } catch { }
    }
    return @{ Kind = 'shared'; Path = $n }
}

# Value '' when there is none (Kind None).
function ReadPath($reg) {
    $value = $reg.GetValue('PATH', $null, 'DoNotExpandEnvironmentNames')
    if ($null -eq $value) { return @{ Value = ''; Kind = 'None' } }
    return @{ Value = [string]$value; Kind = [string]$reg.GetValueKind('PATH') }
}

# The first folder holding <Exe> in the system PATH, then the user PATH, as a new process
# sees them: @{ Found = its classification; System = whether it is on the system PATH }, or $null.
function FirstInstall([string]$userPath) {
    foreach ($source in @(@{ System = $true; Value = $SystemPath }, @{ System = $false; Value = $userPath })) {
        foreach ($entry in ($source.Value -split ';')) {
            $c = Classify $entry
            if ($c.Kind -ne 'other') { return @{ Found = $c; System = $source.System } }
        }
    }
    return $null
}

# Warn when a new terminal wouldn't run -Dir's <Exe> (after an install) or would still find one
# (after an uninstall).
function CheckWhichRuns([string]$userPath) {
    $first = FirstInstall $userPath
    if (-not $first -or ($first.Found.Kind -eq 'own' -and -not $Uninstall)) { return }
    $foundExe = Join-Path $first.Found.Path $Exe
    if ($Uninstall) { Write-Host "  NOTE: a new terminal still finds $foundExe." }
    else { Write-Host "  WARNING: with this PATH a new terminal runs $foundExe, not this folder's $Exe." }
    if ($first.System) {
        Write-Host '  That folder is on the system PATH, which needs an administrator to change:'
        Write-Host "  ask one to remove it, or delete that $Exe."
    } elseif ($first.Found.Kind -eq 'old') {
        Write-Host "  That is an older $Tool install, still on your PATH: run this installer again"
        Write-Host '  and accept the change to replace it.'
    } else {
        Write-Host '  That folder holds other files too, so it stays on your PATH:'
        Write-Host "  delete or rename that $Exe, or move the folder after this one in your PATH."
    }
    Write-Host ''
}

function ShowPath([string]$label, [string]$value) {
    Write-Host "  $label"
    Write-Host "    $(if ($value -eq '') { '[empty]' } else { $value })"
    Write-Host ''
}

if (-not $PSBoundParameters.ContainsKey('SystemPath')) {
    try {
        $sysKey = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey('SYSTEM\CurrentControlSet\Control\Session Manager\Environment')
        $SystemPath = (ReadPath $sysKey).Value
    } catch { $SystemPath = '' }
}

$target = Normalize $Dir

try {
    $reg = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($Key)
    $read = ReadPath $reg
} catch {
    Fail "Failed to read user PATH: $($_.Exception.Message)"
}
if ($read.Kind -notin @('None', 'String', 'ExpandString')) {
    Fail "Your user PATH is stored as $($read.Kind), which this installer doesn't change."
}
$current = $read.Value

if ($Status) {
    $first = FirstInstall $current
    if (-not $first) {
        Write-Host "  $Tool is not on PATH. Run this installer to add this folder."
        exit 0
    }
    $foundExe = Join-Path $first.Found.Path $Exe
    $version = try { (& $foundExe --version 2>$null | Select-Object -First 1) } catch { 'unknown version' }
    Write-Host "  A new terminal runs: $foundExe ($version)"
    # Otherwise the same advice as after an install: what to do depends on what that folder is.
    if ($first.Found.Kind -eq 'own') { Write-Host '  That is this folder.' }
    else { CheckWhichRuns $current }
    exit 0
}

# Classify each entry, empty ones included, by index.
# @(...) around the whole if: an if expression unrolls a one-element array into a bare string,
# and indexing that would give its characters.
$entries = @(if ($current -ne '') { $current -split ';' })
$ownIndexes = @()
$oldIndexes = @()
for ($i = 0; $i -lt $entries.Count; $i++) {
    switch ((Classify $entries[$i]).Kind) {
        'own' { $ownIndexes += $i }
        'old' { $oldIndexes += $i }
    }
}

# The entries that are ours go. Uninstall stops there. Install puts one entry back at the first of
# them, written as -Dir's own entry was (else as -Dir); with none, after the last non-empty entry.
# With no older install to replace, only -Dir's first own entry counts as ours, so a PATH that
# already holds -Dir (even twice) isn't rewritten.
$replacing = (-not $Uninstall) -and $oldIndexes.Count -gt 0
$ours = @(if ($Uninstall -or $replacing) { $oldIndexes + $ownIndexes | Sort-Object }
           else { $ownIndexes | Select-Object -First 1 })
$kept = New-Object System.Collections.Generic.List[string]
for ($i = 0; $i -lt $entries.Count; $i++) {
    if ($ours -notcontains $i) { $kept.Add($entries[$i]) }
}
if (-not $Uninstall) {
    if ($ours.Count) { $at = $ours[0] }   # nothing before it was removed, so the index holds
    else {
        $at = $kept.Count
        while ($at -gt 0 -and $kept[$at - 1].Trim() -eq '') { $at-- }
    }
    $kept.Insert($at, $(if ($ownIndexes.Count) { $entries[$ownIndexes[0]] } else { $Dir }))
}
$combined = $kept -join ';'

if ($combined -ceq $current) {
    if ($Uninstall) { Write-Host "  $Tool is not on your user PATH; nothing to remove." }
    else { Write-Host "  $Tool is already on your user PATH." }
    CheckWhichRuns $current
    exit 0
}

Write-Host ''
if ($Uninstall) {
    Write-Host "  $Tool entries to remove from your user PATH:"
    foreach ($i in $ours) { Write-Host "    $($entries[$i])" }
    Write-Host '  (Only the PATH entries change: no files are deleted.)'
    Write-Host ''
} elseif ($replacing) {
    Write-Host "  Older $Tool installs on your PATH, to be replaced by this one:"
    foreach ($i in $oldIndexes) { Write-Host "    $($entries[$i])  ($((Classify $entries[$i]).Version))" }
    Write-Host '  (Only the PATH entries change: those folders aren''t deleted.)'
    Write-Host ''
}
ShowPath 'User PATH before:' $current
ShowPath 'User PATH after:' $combined
CheckWhichRuns $combined

if ($combined.Length -gt $MaxLength) {
    Fail "The new PATH would be $($combined.Length) characters, more than Windows allows ($MaxLength); PATH not changed."
}

if (-not $Yes) {
    $declined = $null
    if ([Console]::IsInputRedirected) {
        $declined = 'No terminal to confirm in; PATH not changed. Re-run with --yes to apply.'
    } elseif ((Read-Host '  Apply this change? [Y/n]') -match '^\s*(n|no)\s*$') {
        $declined = 'Skipped; PATH not changed.'
    }
    if ($declined) {
        Write-Host "  $declined"
        if (-not $Uninstall) { Write-Host "  Or add manually: $Dir" }
        Write-Host ''
        exit 2
    }
}

# Back up the value as read; without a backup nothing is written.
if (-not $BackupDir) { $BackupDir = Join-Path $env:LOCALAPPDATA "$Tool\path-backups" }
try {
    [void](New-Item -ItemType Directory -Force -Path $BackupDir)
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $backup = Join-Path $BackupDir "path-$stamp.txt"
    for ($n = 2; Test-Path -LiteralPath $backup; $n++) { $backup = Join-Path $BackupDir "path-$stamp-$n.txt" }
    $stream = [IO.File]::Open($backup, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
    $writer = New-Object IO.StreamWriter($stream, (New-Object Text.UTF8Encoding($false)))
    $writer.Write("$($read.Kind)`r`n$current")
    $writer.Close()
} catch {
    Fail "Couldn't save a backup of your PATH ($($_.Exception.Message)); PATH not changed."
}

if ($TestHookBeforeWrite) { & ([scriptblock]::Create($TestHookBeforeWrite)) }

# The type as it was: rewriting a REG_SZ as REG_EXPAND_SZ would expand a literal '%' in it.
$kind = if ($read.Kind -eq 'None') { 'ExpandString' } else { $read.Kind }
$again = ReadPath $reg
if ($again.Value -cne $current -or $again.Kind -ne $read.Kind) {
    Fail 'Your user PATH changed while the installer ran; PATH not changed. Run the installer again.'
}
try {
    $reg.SetValue('PATH', $combined, [Microsoft.Win32.RegistryValueKind]$kind)
    $after = ReadPath $reg
    if ($after.Value -cne $combined -or $after.Kind -ne $kind) { throw 'the value read back differs from the one written' }
} catch {
    $reason = $_.Exception.Message
    try {
        if ($read.Kind -eq 'None') { $reg.DeleteValue('PATH', $false) }
        else { $reg.SetValue('PATH', $current, [Microsoft.Win32.RegistryValueKind]$read.Kind) }
        $restored = "Failed to update PATH ($reason); the old value was put back."
    } catch {
        $restored = "Failed to update PATH ($reason), and couldn't put the old value back: $($_.Exception.Message)"
    }
    Fail @($restored, "Your old PATH is saved in $backup")
}

# Tell Explorer the environment changed, so programs started from it see the new PATH.
if ($Key -eq 'Environment') {
    try {
        Add-Type -Namespace AddToPath -Name Win32 -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint flags, uint timeout, out UIntPtr result);
'@
        $r = [UIntPtr]::Zero
        [void][AddToPath.Win32]::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$r)
    } catch { }
}

if ($Uninstall) { Write-Host "  Removed $Tool from your user PATH." }
elseif ($replacing) { Write-Host "  Replaced the older $Tool on your PATH with: $Dir" }
else { Write-Host "  Added to user PATH: $Dir" }
Write-Host "  Your old PATH is saved in $backup"
Write-Host '  Open a new terminal for the change to take effect (restart VS Code or another editor'
Write-Host '  first if you use its terminal).'
if ($replacing) { exit 3 }
exit 0
