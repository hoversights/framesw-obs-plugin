# Packages the FrameSW Companion Plugin as a real Windows OBS plugin.
#
# LOAD-TESTED on the real Win 11 machine 2026-07-16 against OBS 32.1.2 -
# see WINDOWS_HANDOFF.md's "Windows verification results" section. The
# built layout mirrors DistroAV's actual install exactly
# (%ProgramData%\obs-studio\plugins\<name>\bin\64bit\<name>.dll).
#
# Keep this file pure ASCII: it was originally written on the Mac as
# UTF-8 without BOM, and Windows PowerShell 5.1 reads BOM-less files as
# ANSI - an em-dash decoded to a smart-quote character that silently
# terminated a string and broke parsing of everything after it.
#
# Usage: powershell -ExecutionPolicy Bypass -File package-windows.ps1

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

Write-Host "Building..."
# Release, plain host triple (this only ever builds on the Windows machine
# itself, whose host IS x86_64-pc-windows-msvc - an explicit --target just
# moves the artifact into a different target subdirectory for no benefit).
# Relaxed for cargo as for git below: cargo writes its progress
# ("Compiling ...") to stderr, and with "Stop" PowerShell 5.1 wraps the
# first such line in a NativeCommandError and ends the script there
# (Windows, 2026-10-23: packaging stopped before any file was made).
# Success is read from the exit code instead.
$PrevEap = $ErrorActionPreference
$ErrorActionPreference = "Continue"
cargo build --release
$CargoExit = $LASTEXITCODE
$ErrorActionPreference = $PrevEap
if ($CargoExit -ne 0) { exit 1 }

$PluginDir = "target\framesw-companion"
if (Test-Path $PluginDir) { Remove-Item -Recurse -Force $PluginDir }
New-Item -ItemType Directory -Path "$PluginDir\bin\64bit" -Force | Out-Null

# Two layouts, one folder (OBS 33.0, 2026-10: "Legacy Plugin Locations").
# OBS 33 and later load <name>\<name>.dll first and then skip the same
# plugin in a legacy layout; OBS 32 and earlier only know
# <name>\bin\64bit\<name>.dll. OBS 34 will not load the legacy one at all.
# The old copy goes once FrameSW asks for OBS 33 or later.
Copy-Item "target\release\framesw_obs_plugin.dll" `
    "$PluginDir\framesw-companion.dll"
Copy-Item "target\release\framesw_obs_plugin.dll" `
    "$PluginDir\bin\64bit\framesw-companion.dll"

# Plain-text version marker FrameSW's app reads to detect a stale
# installed copy (platform::installed_plugin_version). Cargo.toml's
# version alone relies on someone remembering to bump it every time the
# plugin's behavior changes - in practice that didn't happen for 6 real
# feature/fix commits in a row (all shipped as "0.1.0"), so this check
# never once fired. Appending this repo's own commit hash (plus a
# -dirty marker for uncommitted local changes, same convention as
# package-macos.sh) makes every real code change produce a different
# stamp automatically, with no bump discipline required at all.
$VersionLine = Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
$Version = $VersionLine.Matches.Groups[1].Value
# $ErrorActionPreference is deliberately relaxed across these git calls.
# With it at "Stop", PowerShell 5.1 wraps ANY line a native command writes
# to stderr in a NativeCommandError and terminates the script - and git
# writes "warning: in the working copy of '...', LF will be replaced by
# CRLF" to stderr routinely on this machine. `2>$null` does not help: the
# wrap happens before the redirect. The effect was that packaging failed
# exactly when the working tree was dirty, which is the only situation the
# -dirty stamp below exists for. Found 2026-09-21 while packaging the
# Show FrameSW fix.
$PrevEap = $ErrorActionPreference
$ErrorActionPreference = "Continue"
$GitSha = (git rev-parse --short HEAD 2>$null)
if (-not $GitSha) { $GitSha = "unknown" }
$GitDirty = ""
git diff --quiet 2>$null
if ($LASTEXITCODE -ne 0) { $GitDirty = "-dirty" }
else {
    git diff --cached --quiet 2>$null
    if ($LASTEXITCODE -ne 0) { $GitDirty = "-dirty" }
}
$ErrorActionPreference = $PrevEap
$Stamp = "$Version+$GitSha$GitDirty"
Set-Content -Path "$PluginDir\VERSION" -Value $Stamp -NoNewline

Write-Host ""
Write-Host "Built: $PluginDir"
Write-Host ""
Write-Host "To install for testing, copy the 'framesw-companion' folder into"
Write-Host "%ProgramData%\obs-studio\plugins\ (FrameSW does this itself on launch)."
Write-Host "It holds the DLL in both layouts: framesw-companion.dll for OBS 33+,"
Write-Host "bin\64bit\framesw-companion.dll for OBS 32."
Write-Host ""
Write-Host "Then fully quit and relaunch OBS Studio, and check its log"
Write-Host "(Help > Log Files > View Current Log, or"
Write-Host "%APPDATA%\obs-studio\logs\) for lines starting with '[framesw]'."
