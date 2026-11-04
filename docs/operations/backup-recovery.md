# Backup and recovery

Ops backlog B3 (backups) and B4 (recovery). Measured on 2026-11-04.

## What is backed up

| Data | Backed up | Why it matters |
| --- | --- | --- |
| Postgres `monsteros` (4.4 GB on disk, 301 MB as a compressed dump) | yes, `scripts/ops/backup.py` | system of record |
| of which cannot be rebuilt from its sources | yes, checked on every restore | forecast ledger and forecasts, model registry, learning and governance audit, publication holds, odds history (`market_lines`), decision-time features (`feature_snapshots`), AI picks and their evaluations, parlay tickets, copy-bot events, MonsterGPT conversations, claims and simulation runs, ML experiments and examples, data-source register, migration record |
| roles and grants | yes, `globals.sql`, without password hashes | passwords are set again with `scripts/ops/rotate_research_password.py` |
| ClickHouse (wallet trades, ~0.8 GB) | **no** | wallet history is re-ingestable from Polymarket's public API for tracked wallets; positions closed before ingestion would be lost |
| `.env`, admin tokens | **no, on purpose** | secrets stay out of backups; keep them in a password manager |
| code | git (GitHub) | |

## Taking a backup

```
python scripts/ops/backup.py                  # dump + manifest, keeps the last 3
python scripts/ops/backup.py --offsite gdrive:monster-os-backups
```

Files go to `%LOCALAPPDATA%\MonsterOS\backups\pg_<time>\` (or `MONSTER_BACKUP_DIR`):
`monsteros.dump` (pg_dump custom format, `-Z 6`), `globals.sql`, `manifest.json` (sha256
and size of each file, row counts of the irreplaceable tables, a fingerprint of the
forecast ledger, applied migrations, git commit). `LATEST` points at the newest.

The script refuses to start when the backup disk has less than 3x the last dump plus
2 GB free, because a full C: wedges Docker. Retention deletes only directories it made.

`/ops/status` reports the newest backup's age and turns degraded when there is none or
it is older than 36 hours.

Measured: 301 MB dump in 41 s while the stack was running.

### Schedule

`scripts/windows/register-backup-tasks.ps1` registers two Windows scheduled tasks for the
current user (re-running replaces them; missed runs start when the machine is next on):

| Task | When | Runs |
| --- | --- | --- |
| `MonsterOS backup` | daily 04:30 | `scripts/ops/backup.py` |
| `MonsterOS restore check` | Sundays 05:15 | `scripts/ops/restore_check.py` |

Both go through `scripts/windows/backup-task.cmd`, which appends the output and exit code
to `%LOCALAPPDATA%\MonsterOS\reports\backup_task.log`. They need Docker Desktop running
(the user must be logged on). A failed backup shows on `/ops/status` once the last good
one is over 36 hours old.

## Checking that it restores

```
python scripts/ops/restore_check.py          # irreplaceable tables (~0.6 GB of data)
python scripts/ops/restore_check.py --full   # everything; refused unless C: has ~1.5x the database free
python -m pytest -q scripts/ops/test_backup.py   # the check fails a tampered backup
```

It restores into a scratch database `monsteros_restore` (never the live one): verifies
the dump's sha256, restores the whole schema, restores the irreplaceable tables' data,
checks every row count against the manifest, recomputes the ledger fingerprint over the
manifest's rows, checks the write-once triggers and the migration list, drops the scratch
database and writes `%LOCALAPPDATA%\MonsterOS\reports\restore_check_<time>.json`.

Measured: PASS in 61 s (schema plus irreplaceable data). `test_backup.py` shows a changed
ledger, missing rows and a different dump each fail it.

Why the default is not a full restore: the Postgres container's disk is a file on C:
that grows and does not shrink; with under 10 GB free a 4+ GB restore could fill C:.

## Restoring production (runbook, not yet rehearsed end to end)

1. Stop the services: `Get-Process *-service,*-engine,api-gateway,monster-gpt,whale-tracker,feature-store,model-registry,market-intelligence,sport-intelligence,copy-trading | Stop-Process`.
2. Pick the backup and check it: `python scripts/ops/restore_check.py --backup <dir>`.
3. Keep the damaged database for later inspection, if there is room:
   `docker exec monster-os-postgres-1 psql -U monster -d postgres -c "ALTER DATABASE monsteros RENAME TO monsteros_broken"`.
4. Roles (only on a new Postgres volume): `docker exec -i monster-os-postgres-1 psql -U monster -d postgres < <dir>\globals.sql`.
5. `docker exec monster-os-postgres-1 psql -U monster -d postgres -c "CREATE DATABASE monsteros"`, then
   `docker exec -i monster-os-postgres-1 pg_restore -U monster -d monsteros --exit-on-error < <dir>\monsteros.dump`.
6. On a new volume, set the service and research passwords again:
   `python scripts/ops/rotate_research_password.py --role monster_app` and `python scripts/ops/rotate_research_password.py`.
7. `python scripts/ops/migrate.py` must report every migration applied and none changed.
8. Start the stack (`scripts\windows\start-stack.ps1`), then `GET :8121/readyz` and `GET :8121/ops/status`.
9. `python scripts/ci/research_checks.py --fast` (integrity, governance, schema checks).
10. Data written after the backup is lost (the backup's `created` time is the recovery point);
    the autopilot re-ingests games, lines and injuries for the recent days it covers.

Recovery point: the age of the newest backup (24 h with a daily schedule). Recovery time,
estimated from the check: about 5 minutes for the data, plus restarting the stack.

## Open

- No off-site copy: the backups sit on the same disk as the database, so they cover
  mistakes and corruption, not losing the disk. The `gdrive:` rclone remote needs a
  reconnect (`rclone config reconnect gdrive:`), and rclone's shared Google client id is
  being retired in 2026, so it should get its own client id first.
- The production restore runbook has not been rehearsed end to end (it needs downtime
  and about 5 GB free).
- ClickHouse is not backed up.
