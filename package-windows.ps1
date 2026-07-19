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
cargo build --release
if ($LASTEXITCODE -ne 0) { exit 1 }

$PluginDir = "target\framesw-companion"
if (Test-Path $PluginDir) { Remove-Item -Recurse -Force $PluginDir }
New-Item -ItemType Directory -Path "$PluginDir\bin\64bit" -Force | Out-Null

Copy-Item "target\release\framesw_obs_plugin.dll" `
    "$PluginDir\bin\64bit\framesw-companion.dll"

Write-Host ""
Write-Host "Built: $PluginDir"
Write-Host ""
Write-Host "To install for testing, copy the 'framesw-companion' folder into"
Write-Host "OBS's plugins directory. Two conventions exist depending on OBS"
Write-Host "version/install method - WINDOWS_HANDOFF.md has the details on"
Write-Host "which one this actual install uses:"
Write-Host "  %ProgramData%\obs-studio\plugins\framesw-companion\bin\64bit\"
Write-Host "  <OBS install dir>\obs-plugins\64bit\framesw-companion.dll  (flat, no per-plugin folder)"
Write-Host ""
Write-Host "Then fully quit and relaunch OBS Studio, and check its log"
Write-Host "(Help > Log Files > View Current Log, or"
Write-Host "%APPDATA%\obs-studio\logs\) for lines starting with '[framesw]'."
