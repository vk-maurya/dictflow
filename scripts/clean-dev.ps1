# Kill orphaned dev processes (stale dictflow.exe / Vite holding :1420).
# Run from the repo root:  powershell -ExecutionPolicy Bypass -File scripts\clean-dev.ps1
$ErrorActionPreference = "SilentlyContinue"

Stop-Process -Name "dictflow" -Force
$conns = netstat -ano | Select-String ":1420" | Select-String "LISTENING"
foreach ($c in $conns) {
    $pid_ = ($c -split '\s+')[-1]
    if ($pid_ -match '^\d+$') { Stop-Process -Id ([int]$pid_) -Force }
}
Start-Sleep -Seconds 1
$left = netstat -ano | Select-String ":1420" | Select-String "LISTENING"
if ($left) { Write-Host "Still in use:"; $left } else { Write-Host "Clean: dictflow stopped, port 1420 free." }
