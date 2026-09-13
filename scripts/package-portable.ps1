# Copy the release binary as a portable artifact.
# Run from the repo root after `npx tauri build`.
$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$ver = (Get-Content (Join-Path $root "src-tauri\tauri.conf.json") -Raw | ConvertFrom-Json).version
$src = Join-Path $root "src-tauri\target\release\dictflow.exe"
if (-not (Test-Path $src)) {
    throw "missing $src - run npx tauri build first"
}

$dir = Join-Path $root "src-tauri\target\release\bundle\portable"
New-Item -ItemType Directory -Force -Path $dir | Out-Null

Copy-Item $src (Join-Path $dir "DictFlow.exe") -Force
$named = "DictFlow-$ver-windows-x64-portable.exe"
$dest = Join-Path $dir $named
Copy-Item $src $dest -Force

Write-Output (Resolve-Path $dest).Path
