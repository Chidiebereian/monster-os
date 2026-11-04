# Starts the local MonsterOS stack: Docker data stores, every built Rust service
# (hidden, logs in %LOCALAPPDATA%\MonsterOS\logs), the api-gateway on :8090 and
# the dashboard dev server on :5190. Services already listening are skipped.
# .env is loaded into this process only (never printed), then the live-stack
# overrides are applied (Postgres on 5434, ClickHouse monster/monster).
#
#   powershell -ExecutionPolicy Bypass -File scripts\windows\start-stack.ps1
# Logs are not overwritten: before a service starts, its previous logs move to logs\archive\
# (<name>.<out|err>.<time>.log); the archive keeps the last 5 per log, none older than 14 days,
# at most 500 MB in all. A live log cannot be rotated while its service runs (Windows holds the
# file); /ops/status flags one that grows past 100 MB.
# Stop everything: Get-Process *-service,*-engine,api-gateway,monster-gpt,whale-tracker,feature-store,model-registry,market-intelligence,sport-intelligence,copy-trading | Stop-Process

$ErrorActionPreference = "Continue"
$repo = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$logDir = Join-Path $env:LOCALAPPDATA "MonsterOS\logs"
New-Item -ItemType Directory -Force $logDir | Out-Null

$archive = Join-Path $logDir "archive"
New-Item -ItemType Directory -Force $archive | Out-Null
$stamp = Get-Date -Format "yyyyMMdd_HHmmss"
$archivePattern = '^(.+)\.\d{8}_\d{6}\.log$'

# moves a previous, non-empty log into the archive (skipped if a process still holds it)
function Archive-Log($path) {
    if (-not (Test-Path $path) -or (Get-Item $path).Length -eq 0) { return }
    $base = [IO.Path]::GetFileNameWithoutExtension($path)
    try { Move-Item $path (Join-Path $archive "$base.$stamp.log") -ErrorAction Stop } catch {}
}

# retention: only files this script archived (<log>.<yyyyMMdd_HHmmss>.log)
function Prune-Archive {
    $files = Get-ChildItem $archive -File | Where-Object { $_.Name -match $archivePattern } | Sort-Object LastWriteTime -Descending
    $cutoff = (Get-Date).AddDays(-14)
    $drop = @()
    foreach ($g in $files | Group-Object { ($_.Name -replace $archivePattern, '$1') }) {
        $drop += $g.Group | Select-Object -Skip 5
    }
    $drop += $files | Where-Object { $_.LastWriteTime -lt $cutoff }
    $total = 0
    foreach ($f in $files) { $total += $f.Length; if ($total -gt 500MB) { $drop += $f } }
    $drop | Sort-Object FullName -Unique | Remove-Item -Force -ErrorAction SilentlyContinue
}

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
# services run as monster_app (migration 0039), never as the database owner; the URL lives in the git-ignored .env
if (-not $env:APP_DATABASE_URL) { throw "APP_DATABASE_URL is not set in .env: run python scripts/ops/rotate_research_password.py --role monster_app" }
$env:DATABASE_URL = $env:APP_DATABASE_URL
# MonsterGPT: extra selectable chat model (fast, weaker) next to LLM_MODEL from .env
if (-not $env:MONSTER_GPT_MODELS) { $env:MONSTER_GPT_MODELS = "qwen2.5:0.5b" }
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
    Archive-Log (Join-Path $logDir "$name.out.log")
    Archive-Log (Join-Path $logDir "$name.err.log")
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
    Archive-Log (Join-Path $logDir "dashboard.log")
    Start-Process -FilePath "cmd.exe" -ArgumentList "/c", "npm run dev > `"$logDir\dashboard.log`" 2>&1" `
        -WorkingDirectory $dash -WindowStyle Hidden | Out-Null
    Write-Output "dashboard                  :5190 started"
}
Prune-Archive
Write-Output "Open http://localhost:5190  (logs: $logDir)"
