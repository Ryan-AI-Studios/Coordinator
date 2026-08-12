# Start or resume Impeccable live + the Status Surface demo page.
# Safe after reboot: idempotent-ish (reuses live server if already up).
#
# Usage (from anywhere):
#   pwsh C:\dev\coordinator\coordinator\scripts\start-impeccable-live.ps1
#   pwsh ...\start-impeccable-live.ps1 -NoBrowser
#   pwsh ...\start-impeccable-live.ps1 -Port 5500

param(
    [int]$Port = 5500,
    [switch]$NoBrowser
)

$ErrorActionPreference = "Stop"
$ProductRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Set-Location $ProductRoot

function Test-PortListen([int]$p) {
    return [bool](Get-NetTCPConnection -LocalPort $p -State Listen -ErrorAction SilentlyContinue | Select-Object -First 1)
}

Write-Host "Product root: $ProductRoot"

# 1) Live helper (inject + token + background server)
Write-Host "Booting Impeccable live helper..."
$boot = node .agents/skills/impeccable/scripts/live.mjs 2>&1 | Out-String
if ($boot -notmatch '"ok":\s*true') {
    Write-Host $boot
    throw "live.mjs failed — check .impeccable/live/config.json and skill install"
}
Write-Host "Live helper: http://127.0.0.1:8400"

# 2) Static page server
if (-not (Test-PortListen $Port)) {
    Write-Host "Starting static server on 127.0.0.1:$Port ..."
    Start-Process -WindowStyle Minimized -FilePath "python" -ArgumentList @(
        "-m", "http.server", "$Port", "--bind", "127.0.0.1"
    ) -WorkingDirectory $ProductRoot | Out-Null
    Start-Sleep -Seconds 1
} else {
    Write-Host "Static server already listening on port $Port"
}

$url = "http://127.0.0.1:$Port/mock/status-surface.html"
try {
    $code = (Invoke-WebRequest -Uri $url -UseBasicParsing -TimeoutSec 5).StatusCode
    Write-Host "Page probe: $code  $url"
} catch {
    Write-Warning "Page not reachable yet: $($_.Exception.Message)"
}

if (-not $NoBrowser) {
    Start-Process $url
}

Write-Host ""
Write-Host "Resume checklist:"
Write-Host "  1. Open $url"
Write-Host "  2. In this (or another) agent session with product cwd, run:"
Write-Host "       node .agents/skills/impeccable/scripts/live-poll.mjs"
Write-Host "     so Go/Steer events are handled."
Write-Host "  3. To stop live helper only:"
Write-Host "       node .agents/skills/impeccable/scripts/live-server.mjs stop"
Write-Host ""
Write-Host "Done."
