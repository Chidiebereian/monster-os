# Starts the local MonsterOS stack: Docker data stores, every built Rust service
# (hidden, logs in %LOCALAPPDATA%\MonsterOS\logs), the api-gateway on :8090 and
# the dashboard dev server on :5190. Services already listening are skipped.
# .env is loaded into this process only (never printed), then the live-stack
# overrides are applied (Postgres on 5434, ClickHouse monster/monster).
#
#   powershell -ExecutionPolicy Bypass -File scripts\windows\start-stack.ps1
# Stop everything: Get-Process *-service,*-engine,api-gateway,monster-gpt,whale-tracker,feature-store,model-registry,market-intelligence,sport-intelligence,copy-trading | Stop-Process

$ErrorActionPreference = "Continue"
$repo = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$logDir = Join-Path $env:LOCALAPPDATA "MonsterOS\logs"
New-Item -ItemType Directory -Force $logDir | Out-Null

function Listening($port) {
    [bool](Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue)
}

docker start monster-os-postgres-1 monster-os-clickhouse-1 | Out-Null
if ($env:MONSTER_OLLAMA -ne "0") { docker start monster-os-ollama-1 | Out-Null }

$envFile = Join-Path $repo ".env"
if (Test-Path $envFile) {
    foreach ($line in Get-Content $envFile) {
        if ($line -match '^\s*#' -or $line -notmatch '=') { continue }
        $k, $v = $line -split '=', 2
        [Environment]::SetEnvironmentVariable($k.Trim(), $v.Trim().TrimEnd("`r"), "Process")
    }
}
$env:DATABASE_URL = "postgres://monster:monster@localhost:5434/monsteros"
$env:CLICKHOUSE_USER = "monster"
$env:CLICKHOUSE_PASSWORD = "monster"

$tries = 0
while (-not (Listening 5434) -and $tries -lt 30) { Start-Sleep -Seconds 2; $tries++ }

# name -> port (must match the api-gateway SERVICES table)
$services = [ordered]@{
    "simulation-engine" = 8081; "pricing-engine" = 8082; "arbitrage-engine" = 8083
    "model-registry" = 8084; "monster-gpt" = 8085; "market-intelligence" = 8114
    "copy-trading" = 8115; "whale-tracker" = 8116; "backtesting-engine" = 8117
    "feature-store" = 8118; "sport-intelligence" = 8119; "nba-service" = 8100
    "nfl-service" = 8101; "mlb-service" = 8102; "nhl-service" = 8103
    "soccer-service" = 8104; "prediction-market-service" = 8105; "bayesian-service" = 8106
    "elo-service" = 8107; "bradley-terry-service" = 8108; "monte-carlo-service" = 8109
    "bootstrap-service" = 8110; "kelly-service" = 8111; "portfolio-service" = 8112
    "data-quality-service" = 8113; "odds-service" = 8120; "projection-service" = 8121
    "api-gateway" = 8090
}

foreach ($name in $services.Keys) {
    $port = $services[$name]
    $exe = Join-Path $repo "target\debug\$name.exe"
    if (Listening $port) { Write-Output ("{0,-26} :{1} already running" -f $name, $port); continue }
    if (-not (Test-Path $exe)) { Write-Output ("{0,-26} :{1} NOT BUILT (cargo build -p {0})" -f $name, $port); continue }
    $env:PORT = "$port"
    Start-Process -FilePath $exe -WorkingDirectory $repo -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $logDir "$name.out.log") `
        -RedirectStandardError (Join-Path $logDir "$name.err.log") | Out-Null
    Write-Output ("{0,-26} :{1} started" -f $name, $port)
}
Remove-Item Env:PORT

$dash = Join-Path $repo "apps\dashboard"
if (Listening 5190) {
    Write-Output "dashboard                  :5190 already running"
} else {
    Start-Process -FilePath "cmd.exe" -ArgumentList "/c", "npm run dev > `"$logDir\dashboard.log`" 2>&1" `
        -WorkingDirectory $dash -WindowStyle Hidden | Out-Null
    Write-Output "dashboard                  :5190 started"
}
Write-Output "Open http://localhost:5190  (logs: $logDir)"
