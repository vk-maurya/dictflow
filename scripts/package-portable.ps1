# Copy the release binary as a portable artifact.
# Run from the repo root after `npx tauri build`.
$ErrorActionPreference = "Stop"

$ver = (Get-Content src-tauri/tauri.conf.json -Raw | ConvertFrom-Json).version
$src = "src-tauri\target\release\dictflow.exe"
if (-not (Test-Path $src)) {
    throw "missing $src — run npx tauri build first"
}

$dir = "src-tauri\target\release\bundle\portable"
New-Item -ItemType Directory -Force -Path $dir | Out-Null

Copy-Item $src "$dir\DictFlow.exe" -Force
$named = "DictFlow-$ver-windows-x64-portable.exe"
Copy-Item $src "$dir\$named" -Force

Write-Output (Resolve-Path "$dir\$named").Path
