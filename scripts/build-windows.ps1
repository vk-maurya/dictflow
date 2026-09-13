# Full local verification + Windows installer build.
# Run from the repo root:  powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1
$ErrorActionPreference = "Stop"

function Step($name) { Write-Host "`n=== $name ===" -ForegroundColor Cyan }

Step "Toolchain"
$env:Path += ";$env:USERPROFILE\.cargo\bin"
cargo --version; node --version; npm --version

Step "Frontend install + build"
npm ci
npm run build

Step "Rust lints + tests"
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml

Step "Installer + portable exe"
npx tauri build

$portableDir = "src-tauri\target\release\bundle\portable"
New-Item -ItemType Directory -Force -Path $portableDir | Out-Null
Copy-Item "src-tauri\target\release\dictflow.exe" "$portableDir\DictFlow.exe" -Force

Write-Host "`nArtifacts:" -ForegroundColor Green
Get-ChildItem src-tauri\target\release\bundle\nsis\*.exe, src-tauri\target\release\bundle\msi\*.msi, src-tauri\target\release\bundle\portable\*.exe -ErrorAction SilentlyContinue |
  Select-Object FullName, @{n = "MB"; e = { [math]::Round($_.Length / 1MB, 1) } }
