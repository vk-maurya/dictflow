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

Step "Installer"
npx tauri build

Write-Host "`nArtifacts:" -ForegroundColor Green
Get-ChildItem src-tauri\target\release\bundle\nsis\*.exe, src-tauri\target\release\bundle\msi\*.msi -ErrorAction SilentlyContinue |
  Select-Object FullName, @{n = "MB"; e = { [math]::Round($_.Length / 1MB, 1) } }
