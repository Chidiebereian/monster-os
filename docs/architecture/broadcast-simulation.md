# Broadcast simulation — architecture and contract

Status as of 2026-10-19. NFL is implemented end to end; NBA, NHL and MLB are
not yet available as broadcast sessions (MLB has a plate-appearance simulator,
`mlb_sim.rs`, but it does not emit events yet).

## Principle

The simulation engine produces every outcome; the frontend only paces and
draws it. No play, yard, score or stat is computed in the browser.

```
nflverse play-by-play ──► nfl_plays (Postgres) ──► fit_league ──► sim_model_params 'nfl:sim:{season}'
                                     │                                  │
                                     └──► team/player inputs (plays     │
                                          strictly before game date) ───┤
                                                                        ▼
                                         nfl_sim.rs (pure, seeded engine)
                                          ├─ Mode A: single_game  → ordered event log → sim_sessions
                                          └─ Mode B: simulate     → 100k-game ensemble → /nfl/simulate
                                                                        │
                                       dashboard /broadcast ◄───────────┘
                                       (pacing, field, scoreboard, box scores)
```

Where it lives (this repo's conventions were kept rather than the target
layout of separate crates):

| Concern | File |
|---|---|
| Engine (rules, clock, play outcomes, events, ensemble) | `apps/projection-service/src/nfl_sim.rs` |
| Ingest, league fit, game inputs, ensemble/backtest endpoints | `apps/projection-service/src/nfl_game.rs` |
| Sport-agnostic session API | `apps/projection-service/src/sim_sessions.rs` |
| Tables `nfl_plays`, `sim_model_params`, `sim_sessions` | `infrastructure/postgres/migrations/0011_nfl_plays_sim_sessions.sql` |
| Broadcast page | `apps/dashboard/src/pages/BroadcastSim.tsx` |
| Field / scoreboard / box scores / controls / timeline / play-by-play | `apps/dashboard/src/components/simulation/` |
| Playback (pacing only) | `apps/dashboard/src/hooks/useSimulationPlayback.ts` |
| Contract types + client | `apps/dashboard/src/lib/sim.ts` |

## Two execution modes

* **Interactive (Mode A).** One seeded game. The engine generates the whole
  ordered event log at creation (~0.1 s for a full game), so the first play is
  available immediately and replay is exact. Same inputs + same seed = same
  game (tested).
* **Prediction ensemble (Mode B).** 100,000 games by default, in 200 parameter
  batches (each batch redraws every rate and share from its posterior), run on
  up to 4 threads; results depend only on (inputs, n, draws, seed), not on the
  thread count. 100k games take ~3 s in a debug build.

## Endpoints (projection-service, via the gateway at `/api/projection-service/...`)

| Endpoint | Purpose |
|---|---|
| `POST /ingest/nfl/pbp?seasons=2024,2025,2026` | load nflverse play-by-play |
| `POST /fit/nfl/sim` | fit league parameters for every stored season (+ injury-designation play rates) |
| `POST /ingest/nfl/availability?seasons=` | weekly roster status + official injury reports |
| `GET /nfl/sim/games?days=&back=` | recent + upcoming games |
| `POST /sim/sessions` `{sport, game_id, seed?, out?, in?, away_qb?, home_qb?}` | create a session (one seeded game, optionally an alternate reality) |
| `GET /sim/sessions?sport=&game_id=` | recent sessions |
| `GET /sim/sessions/{id}` | metadata (`?reveal=true` adds the final) |
| `GET /sim/sessions/{id}/events?after=&limit=` | ordered events |
| `GET /sim/sessions/{id}/state?seq=` | snapshot after event `seq` |
| `GET /nfl/simulate/{game_id}?n=100000&seed=&out=&in=&away_qb=&home_qb=` | ensemble prediction (n >= 100,000 enforced) |
| `GET /backtest/nfl/sim?season=` / `GET /nfl/backtests` | walk-forward validation |

## Event contract

Each event is self-contained: what happened, who was involved, and the full
authoritative state after it.

```json
{
  "seq": 37, "kind": "pass_complete", "qtr": 2, "clock_before": 512, "clock_after": 474,
  "offense": 1, "down": 2, "togo": 7, "yardline_100": 44, "yards": 12,
  "passer": 4, "receiver": 6, "rusher": null, "kicker": null, "location": "right",
  "x_from": 44.0, "x_to": 32.0, "first_down": true, "touchdown": false, "turnover": false,
  "points": 0, "scoring_team": null,
  "text": "Brock Purdy pass right to George Kittle for 12 yards, first down",
  "state": { "qtr": 2, "clock": 474, "score": [7, 10], "possession": 1, "down": 1, "togo": 10,
             "yardline_100": 32, "ball_x": 32.0, "first_down_x": 22.0 },
  "box_score": [[4, [21, 14, 160, 1, 0, 2, 3, 9, 0, 0, 0, 0, 0, 0, 0, 0, 2, 2]], ...]
}
```

Conventions:

* `yardline_100` is from the offense's perspective (yards to the goal line it attacks), before the play.
* `ball_x` / `x_from` / `x_to` are absolute field coordinates: 0 = away goal line, 100 = home goal line; the away team attacks x = 100.
* `clock_before` / `clock_after` are seconds left in the period.
* `box_score` rows are `[player index, stats]`, in the order of `meta.stat_keys`. Player indices refer to `meta.inputs.players`.
* `kind` is one of: kickoff, pass_complete, pass_incomplete, interception, sack, scramble, rush, kneel, penalty, punt, field_goal, extra_point, two_point, fumble_lost, safety, turnover_on_downs, two_minute_warning, end_period, game_end.

Integrity: sequence numbers are contiguous from 1. The client verifies
continuity when loading (`loadAllEvents`) and refuses a log with a gap. Tests
assert that scoring events sum to the final score, the QB's yards equal his
receivers' yards, and clocks, downs and spots stay legal.

## Why no WebSocket (yet)

The proposal suggested streaming events over a WebSocket. A full game is
generated in well under a second and is deterministic, so the server sends the
complete log and the client paces it. That gives exact replay, scrubbing and
deep links (`/broadcast?game=…&seq=…&seed=…`) without server-side session
timers. A WebSocket becomes worthwhile for long-running or live-updating
sessions (e.g. re-simulating from a live game state); the event contract above
would not change.

## What the field draws (and what it does not)

The line of scrimmage, the first-down line, the ball's start and end spots and
the play direction (left / middle / right, simulated from each player's real
pass/run location frequencies) all come from the event. Pass depth (air yards)
and individual player positions are not simulated, so the field shows no
player dots and incompletions show direction only.
