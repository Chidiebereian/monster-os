# Development environment

How to run Monster OS on one Windows machine from a clean checkout, apply schema
changes, and run the checks. Everything here is free and local; the only paid
inputs are the optional odds feeds (see "External data").

## Requirements

| Tool | Used for |
| --- | --- |
| Docker Desktop | Postgres 16, ClickHouse, Ollama (and optionally Redis, MinIO, NATS, Prometheus, Grafana) |
| Rust (stable) | every service in `apps/` and `services/` |
| Node.js + npm | the dashboard (`apps/dashboard`, Vite) |
| Python 3.13 with `psycopg`, `pytest`, `websockets` | ops scripts, research checks, MonsterGPT tests |
| Optional: NVIDIA GPU | `infrastructure/docker-compose.gpu.yml` gives Ollama the GPU (qwen2.5:3b fits in 4 GB) |

## First run

1. `copy .env.example .env` and set `POSTGRES_PASSWORD`. `.env` is git-ignored; never
   commit it, and edit it only with key-targeted scripts.
2. Start the data stores. On a new volume Postgres runs every migration in
   `infrastructure/postgres/migrations` once (`docker-entrypoint-initdb.d`):
   `docker compose -f infrastructure/docker-compose.yml --env-file .env up -d postgres clickhouse ollama`
3. Record the migrations that the first start applied:
   `python scripts/ops/migrate.py --baseline <number of the newest migration>`
4. Create the service and research roles' passwords (written to `.env`, never printed):
   `python scripts/ops/rotate_research_password.py --role monster_app` and
   `python scripts/ops/rotate_research_password.py`. Add `OPS_DATABASE_URL` (owner) to `.env`
   for migrations and administration.
5. Pull the models: `docker exec monster-os-ollama-1 ollama pull qwen2.5:3b` and
   `... ollama pull nomic-embed-text`; set `LLM_BASE_URL` and `LLM_MODEL` in `.env`.
6. Build: `cargo build --workspace -j 2` (the dev profile; `target/` grows to over 20 GB),
   then `cd apps/dashboard && npm install`.
7. Start everything: `powershell -ExecutionPolicy Bypass -File scripts\windows\start-stack.ps1`.
   Open http://localhost:5190. Logs: `%LOCALAPPDATA%\MonsterOS\logs`.

## Ports

| Port | Service | Port | Service |
| --- | --- | --- | --- |
| 5190 | dashboard (Vite, localhost only) | 8090 | api-gateway (`/api/{service}/...`) |
| 8121 | projection-service (engines, picks, governance, hub, `/readyz`, `/ops/status`) | 8085 | monster-gpt (chat, WebSocket `/ws`, RAG) |
| 8081-8084 | simulation, pricing, arbitrage, model-registry | 8114-8119 | market-intelligence, copy-trading, whale-tracker, backtesting, feature-store, sport-intelligence |
| 8100-8113 | services/* (several are health-check stubs) | 8120 | odds-service |
| 5434 | Postgres (127.0.0.1 only) | 8123 / 11434 | ClickHouse HTTP / Ollama |

Health: `GET /healthz` on every service; `GET :8121/readyz` (database + autopilot) and
`GET :8121/ops/status` (every service, data stores, jobs, data freshness, API budgets,
disk and memory).

## Database roles

| Role | URL in `.env` | Used by |
| --- | --- | --- |
| monster (owner) | `OPS_DATABASE_URL` | migrations, admin tokens, break-glass only |
| monster_app | `APP_DATABASE_URL` | every running service (cannot approve promotions, publish markets or edit audit history) |
| monster_research | `ML_DATABASE_URL` | research jobs (read, insert into ML tables only) |

## Schema changes

Add `infrastructure/postgres/migrations/NNNN_name.sql` (next number, one transaction,
safe to run twice where possible), then:

```
python scripts/ops/migrate.py            # status
python scripts/ops/migrate.py --apply    # apply pending, recorded in schema_migrations
```

Never edit an applied migration; `migrate.py` reports a changed file and does not re-run it.

## Checks

```
python scripts/ci/research_checks.py            # the full local gate (about 20 min)
python scripts/ci/research_checks.py --fast     # skip cargo and the slow research batch
python scripts/ci/research_checks.py --golden   # also grade MonsterGPT on the golden questions
python apps/monster-gpt/tests/golden.py --check # golden set valid and truths computable
```

The gate runs: migrations on an empty database (twice), ML unit tests, data integrity,
governance, all Rust unit tests, the dashboard type check, the v2 schema tests, the model
curriculum tests, the golden set check and a secrets scan. Tests that need a writable
database use scratch databases (`monsteros_ci`, `monsteros_gov`, `monsteros_v2`,
`monsteros_golden`) and drop them afterwards; production is only read.

Two governance tests read production state and fail while it differs from what they
expect: open promotion requests, and publication changes made by an admin. Their
messages say which.

## External data

Sources, their access type and cost are in the `data_sources` table (migration 0041).
The Odds API and SportsGameOdds are paid plans with daily budgets; everything except
market-price features works without them. Terms of the other sources are under review
(status `review_pending`).
