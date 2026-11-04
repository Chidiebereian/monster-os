# Production governance

What can change a production model or what AI Picks shows, and who can do it.
Code: `projection-service/src/governance.rs`; schema and authority: migrations 0038
and 0039; tools: `scripts/ops/{admin_token,governance,rotate_research_password}.py`;
tests: `scripts/ops/test_governance.py`.

## Authentication

MonsterOS has no user accounts. Administrative calls must come from this machine
and carry `Authorization: Bearer <admin token>`:

- projection-service, like every service, listens on 127.0.0.1 and ::1 only
  (`common::bind_addrs`; `BIND_ADDR` overrides it, for tests). A call is local when the socket peer is loopback and every address in
  `X-Forwarded-For`, `X-Real-IP` and `Forwarded` is loopback too; headers can only
  make a call less local. The gateway replaces any client `X-Forwarded-For` with
  its own socket peer, so a remote browser behind it is refused.
- `python scripts/ops/admin_token.py --name <principal> [--role approver|auditor]
  [--days N]` creates or rotates a principal: a random token goes to
  `.secrets/admin_<principal>.token` (git-ignored, ACL: the current user and SYSTEM
  only) and only its SHA-256 goes to `gov_private.admins`, through the owner
  connection. `--revoke` revokes, `--list` lists. Tokens expire (default 180 days).
- The database checks the token (`gov_authenticate`), not the service: a rotation
  or revocation applies to the next call, and the principal comes from the matching
  row. Names or approvers sent by the client are ignored. Tokens never appear in
  responses, logs or errors (Postgres `log_parameter_max_length_on_error = 0`).
- Roles: `approver` (approve, reject, cancel, publication decisions) and `auditor`
  (authenticates, e.g. `GET /governance/whoami`; decides nothing).
- Refusals: no approver configured 503, caller not local 403, no token 401,
  unknown / expired / revoked token 403, auditor deciding 403.
- Principals: `owner` (approver, the operator) and `selftest` (auditor in
  production, used by the read-only live tests).

## Database roles

| Role | Used by | Can | Cannot |
|---|---|---|---|
| `monster` (owner, superuser) | migrations, `admin_token.py`, `rotate_research_password.py` (`OPS_DATABASE_URL`) | everything (break-glass) | nothing is enforced against a superuser; see Residual risk |
| `monster_app` | every service (`APP_DATABASE_URL`; `start-stack.ps1` refuses to start without it) | read/write application tables; add candidates, shadows, rejections, retirements; update evidence and notes of any model; propose (`gov_propose`), expire (`gov_expire`), suspend a published market (`gov_safety_suspend`), first champion of an empty line and the engine-upgrade relabel | make, change, retire or delete a champion; write or update `promotion_requests` / `promotion_request_events` / `publication_holds`; write `promote` / `rollback` / `register` / `engine_upgrade` audit rows; update or delete `learning_audit` or research skips; read `gov_private`; approve / reject / cancel / publish without an approver token |
| `monster_research` | `scripts/ml` (`ML_DATABASE_URL`) | read the public schema; insert into `ml_examples`, `ml_experiments`, `feature_snapshots` | everything else, including every `gov_*` function and `gov_private` |

| Operation | Path | Credential needed |
|---|---|---|
| Champion change (promote, rollback) | `gov_approve` (SECURITY DEFINER) | approver token; the service role cannot change a champion directly (trigger `gov_champion_guard`) |
| Proposal | `gov_propose` | service role (proposing changes nothing; the binding is computed in the database) |
| Approval, rejection, cancellation | `gov_approve`, `gov_close` | approver token |
| Execution + audit event | inside `gov_approve`, one transaction | approver token |
| Request events | trigger on `promotion_requests` | only reachable through the functions |
| Publication decision | `gov_publish` | approver token |
| Safety suspension | `gov_safety_suspend` | service role; only suspends a market published now, refuses an eligible tier |
| Restore a suspended market | `gov_publish` | approver token |
| First champion / engine-upgrade relabel | `gov_register_initial`, `gov_engine_upgrade` | service role; only engine defaults on an empty line, only the champion's own configuration on new code |
| Research tables, experiments | direct inserts | research role |

New tables get the service role's ordinary read/write by default privileges; a
migration adding a governed table must revoke explicitly, as 0039 does.

### Residual risk (break-glass)

The owner is a superuser: it can disable triggers, rewrite any table and issue
tokens. Its use is limited to migrations and the ops scripts; no service holds
it. The database is not tamper-proof against the owner. Locally the owner
password is the Docker default and the container publishes port 5434 on every
interface: bind it to 127.0.0.1 and rotate the owner password to close that.

## Publication

Evidence, eligibility and publication are separate:

| Layer | Changes | How |
|---|---|---|
| Evidence (tier: proven / baseline / unproven / losing) | automatically | evidence refreshes, live and line evidence (unchanged) |
| Eligibility | automatically | default feed needs tier proven or baseline |
| Publication state | only by decision | `published`, `leans_only`, `suspended`, or no decision = unpublished |

Public = state `published` and an eligible tier (default feed), or `published` /
`leans_only` with "include unproven". Unpublished and suspended markets are never
public, whatever their tier. One filter (`governance::public_filter` / `VISIBLE`)
serves `/picks`, `/picks/{id}`, `/picks/meta`, the Games hub and, through `/picks`,
MonsterGPT; the validation scorecard and `include_held=true` are internal.

- Cut-over (once, 2026-11-01 23:20 UTC, `auth_method seed`): every market public
  before kept exactly its exposure (eligible tier -> published, otherwise
  leans_only); the five holds stayed suspended. Measured on the same data:
  default feed 19 = 19 open picks, everything public 4,392 = 4,392, none lost or
  gained. The seeding code was removed afterwards; a fresh database starts with
  every market unpublished.
- New markets start unpublished: a market that reaches baseline through an
  evidence refresh is not public until an admin publishes it.
- Safety rule (every picks cycle): a published market whose tier falls below
  eligible is suspended (`gov_safety_suspend`, `decided_by safety:evidence`,
  `auth_method safety_rule`). The rule never publishes; restoring needs an admin
  decision.
- Decisions: `python scripts/ops/governance.py publish <engine> <market> --status
  published|leans_only|suspended --as <principal> --reason "..."`. Every decision
  is a new row in `publication_holds` (append-only) with reason, principal,
  method and an evidence snapshot.

## Promotion requests

The only way a production model changes. `/learning/{sport}/promote|rollback`
and `/parlays/model/promote|rollback` return 410.

```
PROPOSED -> PENDING_APPROVAL -> APPROVED -> EXECUTED
          \-> CANCELLED / EXPIRED        \-> FAILED
PENDING_APPROVAL -> REJECTED / CANCELLED / EXPIRED
```

- Proposed by the autopilot (a shadow that passes the live gate, or a former
  champion that beats the current one), by parlay learning, or from this machine
  (`POST /governance/promotion-requests`, the dashboard buttons, `governance.py
  request`). Proposing changes nothing.
- Bound at creation, by the database: kind, engine, action, target environment
  (production), candidate and champion versions, validation report reference,
  artifact hash (SHA-256 over the candidate's evidence and its configuration and
  engine, or dep weight for parlay), dataset fingerprint,
  calibration and market-comparison extracts. A request is created PROPOSED
  and undecided; database triggers refuse any change to the binding, any
  transition outside the lifecycle, and deletes, for every role.
- Approving (admin): the approver restates candidate, action, environment and
  artifact hash; a mismatch is refused. The service then re-reads the registry:
  if the artifact, the candidate's status or the champion changed, the request
  is invalidated (EXPIRED). The request row is locked, so only one approval
  can win; replays and double approvals get 409. The champion swap, EXECUTED and
  the audit row commit together; a failure leaves the champion as it was and
  the request FAILED.
- `--dry-run` validates everything and changes nothing.
- One open request per model line; the same proposal can be filed again once
  the earlier one is decided. Undecided requests expire after 72 hours.
- Every transition is copied to `promotion_request_events` (append-only) with
  its actor; executions are also in `learning_audit`.

## What is still automatic

Ingestion, freezing, scoring, evidence refreshes, the tuner (candidates and
shadows), shadow rejection, retiring an old rollback reference, the
engine-upgrade relabel on a code deploy, request expiry and the publication
safety rule. None of them can make a model champion or make a market public;
the database refuses it for the service role.

Manual by design: every approval, rejection, cancellation and publication
decision; token creation, rotation and revocation; applying migrations.
