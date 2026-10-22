# AI Picks

Model-backed plays on player props and game lines, each with a probability, an
edge, a projection, risk flags and a written thesis. Code:
`apps/projection-service/src/lines.rs` (market lines), `picks.rs` (pricing,
grading, writeups, settlement), `blend.rs` (market-anchored blending),
`evidence.rs` (backtest evidence), `sharp.rs` (stale lines and stakes); page:
`apps/dashboard/src/pages/AiPicks.tsx` (`/picks`). Tables: migration
`0018_ai_picks.sql`; seed evidence `0019`; recorded lines `0020_pick_blend.sql`;
stakes and stale lines `0021_market_edges.sql`.

## Pipeline (every 20 minutes per sport, inside the learning autopilot)

1. **Lines** (`lines::refresh`) for games a champion engine has already frozen
   in the prediction ledger and that start within 40 hours. Only two-way markets
   are stored, so every price has a partner at the same book and line.

   | Provider | What | Cost / budget |
   |---|---|---|
   | ESPN (free) | DraftKings moneyline, spread, total, two-way player props, opening and current prices | every cycle |
   | The Odds API | moneyline / spread / total from ten named books, **Pinnacle** included (the sharp reference; not in the US region), deep links when provided. `ODDS_API_BOOKMAKERS` overrides the list | 3 credits per call (ten books cost one region); at most every 6 h per sport; `oddsapi_daily_credits` (default 9). Shares the key pool with the Odds Screen |
   | SportsGameOdds | every book's game lines and props, deep links when provided | 1 entity per event; amateur tier = 2,500 a month, 10 requests a minute; at most every 4 h per sport; `sgo_daily_entities` (default 60). A 429 pauses it for the day |

   Games are matched by team codes (`canon`) and start time; players by id
   (NBA: ESPN ids) or by first-initial + last-name within the frozen forecast's
   two rosters (ambiguous names are dropped). ESPN does not label prop sides; the
   order (over, then under) was verified on lopsided lines, and every build
   re-checks it against the model — if fewer than half of 30+ clear cases agree,
   DraftKings-only props are skipped that cycle.

2. **Price** (`picks::build`). The probability comes from the champion's frozen
   ledger payload — never a fresh simulation — so each pick points at an
   immutable prediction (`ledger_id`). Simulators carry full distributions:
   per-player stat PMFs (props), the total, and the home margin (spreads; NHL
   simulator: puck lines only). Team models carry margin and total PMFs. Props
   are conditional on playing (books void them otherwise); a player active in
   under 50% of simulations is skipped.

   **Blend.** The engine's raw probability is not used as is. It is moved
   toward anchors that are hard to beat, on the log-odds scale:
   `blend(p, anchor, w) = σ(logit(anchor) + w·(logit(p) − logit(anchor)))`.
   `w = 0` gives the anchor and `w = 1` the raw model.

   - Props first move toward the player's own prior frequency over the line,
     with `w_naive` fitted in backtests.
   - Every pick then moves toward the de-vigged market consensus, with
     `w_market`. That weight is fitted on real closing lines:
     - from the backtests (NBA and NFL closing spreads and totals);
     - and, once 300 have settled, from every line this service has priced
       (`pick_line_evals`). That covers props and NHL game lines too.
   - Until a weight is fitted, `blend_default_w` (0.2) applies and the card
     carries a risk flag. Fewer than 300 recorded lines move the weight only
     partway from the default toward their fit.

   The market weights fitted so far are near zero: the NBA team model 0.00–0.10
   and the NFL team model 0.00. The closing line already contains what those
   engines know, so their edges disappear. That is the intended outcome.

   For each selection the book with the best expected value is chosen.
   `p_market` is the de-vigged consensus of the books quoting that same line;
   `edge = P(blended) − p_market` (both excluding pushes);
   `EV = P(win)·(decimal − 1) − P(loss)`. A pick is published when
   `edge ≥ min_edge` (default 1.5%) and `EV > 0`, the model probability is in
   [3%, 97%] and the market's in [12%, 88%].

   Guards (counted in the build report as `suppressed`): a RAW edge above
   `max_edge` (default 25%) is not published (the blend could shrink a broken
   forecast into a plausible-looking edge, so the guard looks before it) — against real markets it is
   almost always a data or model error; and an NBA prop is skipped when the
   simulator plays the player under 60% (or over 150% + 6) of his average
   minutes over his last five regular-season or playoff games, which is a lineup
   problem upstream, not an edge. (Found on the first live run: in opening week
   every preseason participant counted as active, so the rotation handed a
   starting center 2 minutes and his unders looked like locks.)

3. **Grade** = edge grade (A+ ≥ 10%, A ≥ 7.5%, A- ≥ 6%, B+ ≥ 5%, B ≥ 4%,
   B- ≥ 3%, C+ ≥ 2.5%, C ≥ 1.5%) **capped by the engine's track record on that
   market** (`pick_evidence`, current engine code version only):

   | Tier | Evidence | Cap | Feed |
   |---|---|---|---|
   | proven | the blended model beats the de-vigged market by > 2 SE (paired Brier) over ≥150 settled picks, or over ≥150 recorded real lines (out of sample by 5 game folds), or in backtests against closing lines in ≥2 seasons (weight fitted on the other seasons) and never worse | none | shown |
   | baseline | backtests beat a naive model (player's own rate, league rate) in ≥2 seasons, never worse; not yet tested on real lines | B+ | shown |
   | unproven | no evidence | C+ | hidden by default |
   | losing | worse than the comparison in ≥2 seasons (or live by > 2 SE) | C | hidden by default |

   Precedence: published picks (live), then recorded lines, then backtests
   against closing lines, then backtests against the naive model. Live evidence
   takes precedence once it is significant, so grades unlock (or
   lock) from results with no code change. A prop market with no evidence of
   its own inherits "losing" from the same engine's measured props (PRA is the
   sum of points, rebounds and assists).

   Backtest evidence regenerates itself (`evidence.rs`): when a champion's
   engine code version has no backtest rows for the current blending method
   (`blend-v1`), or they are older than 30 days, the autopilot's slow loop re-runs
   full-season walk-forward backtests over the last five seasons through the
   learning loop's `evaluate` and rewrites `pick_evidence`. So an engine upgrade
   (e.g. `nhl-event-sim-v2`) starts "unproven" and re-earns its caps on its own.
   Manual trigger: `POST /picks/evidence/run?engine=nhl`. Where both the simulator and the team
   model price a game line, the one with the better tier decides.

   **Stake.** Every pick carries a suggested stake, a fraction of bankroll:
   fractional Kelly on the blended probability,
   `f* = (p·decimal − 1)/(decimal − 1)`, `stake = min(kelly_fraction·f*, kelly_cap)`
   (defaults 0.25 and 2%), times 1 for a proven engine, 0.5 for baseline and 0
   for unproven or losing. Only engines that have beaten something get money
   behind them.

   **Sharp check.** The same price is also judged against Pinnacle de-vigged
   at the same line (seen within 30 minutes), or, when Pinnacle does not quote
   it, the median of 4+ other books from the same snapshot. The card shows that
   probability and the price's EV against it. When the price is a losing bet by
   that measure, the pick carries an `against_sharp` flag, and against Pinnacle
   itself the stake is 0.

4. **Explain.** `facts` holds every number the card can cite: projection,
   median and 80% range, model vs market probability, last 10/20 games against
   the line, minutes (NBA), recent team form, line movement, playing
   probability, and the track-record sentence. The writeup is a deterministic
   template over those facts. If `PICKS_LLM_BASE_URL` (or `LLM_BASE_URL`) points
   at a reachable OpenAI-compatible endpoint (e.g. Ollama), up to `llm_per_cycle`
   (default 12) open template picks per cycle are rewritten by MonsterGPT, best
   grades first (reachability is checked once per cycle); a rewrite is
   **discarded** unless every number in it appears in the facts, it keeps to
   3–5 sentences, and it makes no guarantees.

   Risk flags: priced against the sharp line (`against_sharp`), unproven/losing engine, edge > 15%, projected (not confirmed)
   lineups, playing probability < 90%, single book, price older than 90
   minutes, push chance > 5%, line moved against the side since open, fewer than
   8 recent games.

5. **Record lines.** Each build records every selection a champion engine can
   price, at the line most books quote. The record holds the raw engine
   probability, the naive baseline and the consensus, and is updated until the
   start, so the last value is the close. After the game, `blend::settle`
   scores it, and `blend::refresh_evidence` refits `w_market` and writes the
   `lines` evidence rows. This runs on every settle, with no manual step.

6. **Stale lines** (`sharp::scan`, after each build). Model-free plays: every
   book x selection x line whose price beats the sharp fair price (as in the
   sharp check, the book itself excluded) by at least `stale_min_ev` (2%) EV is
   published once to `market_edges`, with a stake of quarter Kelly on the fair
   probability (half that when the fair price is a consensus rather than
   Pinnacle). EVs above `stale_max_ev` (12%) are dropped as mismatched lines.
   `latest_price` / `latest_ev` follow the book until the start; after the game
   it is settled like a pick, and `close_ev` scores the published price against
   the closing fair price (the CLV of a stale-line bet). Dashboard: the "Stale
   lines" tab.

7. **Settle** (`picks::settle`, every tick). At the start a pick becomes
   `started`; when final it is settled from the box score (props) or final score
   (game lines) at the **published** price: 1 unit, win = decimal − 1,
   push/void = 0. Props of players who did not play are void. CLV = de-vigged
   closing probability at the same book and line minus the published one.
   Settled picks refresh the live evidence.

## What is immutable

`line, book, price, p_model, p_market, edge, ev, grade, tier, facts` are
written once at publication and are what the record is scored on. `latest_*`
follow the market until the start (the card shows when the line has moved).
The writeup may be regenerated; it is not part of the record.

## API

```
POST /picks/run?sport=&force=     lines -> build -> stale lines -> settle (force ignores provider spacing, not budgets)
POST /picks/evidence/run?engine=  regenerate backtest evidence for one engine (minutes)
GET  /picks?sport=&kind=prop|game&side=over|under&grades=A+,A&market=&game=&book=&min_edge=&min_prob=
           &sort=grade|edge|prob|odds&q=&include_hidden=true&status=open|settled|all
GET  /picks/{id}                  with facts and every book's current price
GET  /picks/record?sport=&days=   W-L-P, units, ROI, Kelly ROI, CLV, Brier vs market by sport/kind/market/tier/grade + evidence + tiers
GET  /picks/edges?sport=&status=open|settled|all&days=   stale lines, their record (ROI, Kelly ROI, EV at the close) and settings
GET  /picks/meta?sport=           filter options, line freshness, provider usage today, hidden count, settings
```

Settings live in `learning_settings` under key `picks`:
`{"min_edge": 0.015, "max_edge": 0.25, "oddsapi_daily_credits": 9, "sgo_daily_entities": 60, "llm_per_cycle": 12, "blend_default_w": 0.2,
"kelly_fraction": 0.25, "kelly_cap": 0.02, "stale_min_ev": 0.02, "stale_max_ev": 0.12, "stale_consensus_books": 4}`.
The dashboard's bankroll (for showing stakes in dollars) is stored in the browser only.
`SPORTSGAMEODDS_API_KEY` may list several comma-separated keys; a key that is
rate limited (429) falls through to the next. SportsGameOdds requests are limited
to the 40-hour window (`startsAfter`/`startsBefore`, `limit` = games + 5), since
every returned event costs one of the month's entities.

The dashboard page accepts `?sport=nhl&hidden=1&pick=<id>` deep links.

## Limits

- Prop markets are limited to stats the simulators produce **and** the box
  scores settle: NBA pts/reb/ast/3PM/stl/blk/tov/PRA; NHL SOG, points,
  assists, goals, saves, blocked shots; NFL passing/rushing/receiving yards and
  receptions. Milestone ladders and scorer markets are one-sided and not used.
- MLB is not in the feed (its simulator is not frozen into the ledger yet).
- Spreads need a margin distribution: NBA simulator forecasts frozen before
  2026-10-21 and team-model forecasts before the same date lack one, so those
  games price moneyline and total only.
- ESPN's API is unofficial; its DraftKings links are templated, so ESPN-only
  picks show "No link available".
- Tiers on 2026-10-22 (blend-v1 evidence): **baseline** (cap B+) for NBA
  points / rebounds / assists / threes props, NFL passing / rushing /
  receiving yards and receptions props, NHL points / SOG / saves props and
  moneyline (simulator), and the NHL team model's moneyline and total;
  **unproven** (cap C+, hidden, no stake) for every NBA and NFL game line (both
  engines), NHL totals (simulator) and NBA blocks. No market is proven yet:
  that takes 150+ settled picks or recorded lines that beat the closing line.
- Stale lines are only as fresh as the slowest feed behind them (Odds API
  every 6 h, SportsGameOdds every 4 h); books correct stale prices quickly, so
  a listed price may already be gone. Props have no Pinnacle price, so prop
  stale lines are judged against the consensus.
- A pick's tier is fixed when it is published. Picks published before an
  engine's evidence existed keep their "unproven" label; the next slate uses the
  new evidence.
