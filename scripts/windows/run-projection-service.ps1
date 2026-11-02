# Runs projection-service (with the learning autopilot) under a small supervisor:
# waits for Postgres, loads .env into this process only (never printed), applies
# the local database override, and restarts the service if it exits.
#
# Registered as the "MonsterOS projection-service" scheduled task (at logon) by
# install-projection-service-task.ps1, so it runs outside any terminal or editor.
# To stop it for a rebuild: Stop-ScheduledTask "MonsterOS projection-service"
# (stops this supervisor), then stop the projection-service process.

$ErrorActionPreference = "Continue"
$repo = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$exe = Join-Path $repo "target\debug\projection-service.exe"
$logDir = Join-Path $env:LOCALAPPDATA "MonsterOS\logs"
New-Item -ItemType Directory -Force $logDir | Out-Null
$log = Join-Path $logDir "projection-service.log"

function Note($msg) { Add-Content -Path $log -Value ("[{0:u}] supervisor: {1}" -f (Get-Date).ToUniversalTime(), $msg) }

# .env -> process environment (values are never written anywhere)
$envFile = Join-Path $repo ".env"
if (Test-Path $envFile) {
    foreach ($line in Get-Content $envFile) {
        if ($line -match '^\s*#' -or $line -notmatch '=') { continue }
        $k, $v = $line -split '=', 2
        [Environment]::SetEnvironmentVariable($k.Trim(), $v.Trim().TrimEnd("`r"), "Process")
    }
}
# services run as monster_app (migration 0039), never as the database owner; the URL lives in .env
if (-not $env:APP_DATABASE_URL) { throw "APP_DATABASE_URL is not set in .env: run python scripts/ops/rotate_research_password.py --role monster_app" }
$env:DATABASE_URL = $env:APP_DATABASE_URL
$env:PORT = "8121"

while ($true) {
    # Postgres (Docker) may start after logon
    $tries = 0
    while (-not (Test-NetConnection -ComputerName 127.0.0.1 -Port 5434 -InformationLevel Quiet -WarningAction SilentlyContinue)) {
        if ($tries % 20 -eq 0) { Note "waiting for Postgres on 127.0.0.1:5434" }
        $tries++
        Start-Sleep -Seconds 15
    }
    if (-not (Test-Path $exe)) { Note "missing $exe; build it with cargo build -p projection-service"; Start-Sleep -Seconds 60; continue }
    if ((Test-Path $log) -and (Get-Item $log).Length -gt 50MB) { Move-Item $log "$log.1" -Force }
    Note "starting $exe"
    $p = Start-Process -FilePath $exe -WorkingDirectory $repo -NoNewWindow -PassThru `
        -RedirectStandardOutput "$log.out" -RedirectStandardError "$log.err"
    $p.WaitForExit()
    foreach ($f in "$log.out", "$log.err") { if (Test-Path $f) { Get-Content $f | Add-Content $log; Remove-Item $f -Force } }
    Note ("projection-service exited with code {0}; restarting in 30 s" -f $p.ExitCode)
    Start-Sleep -Seconds 30
}
