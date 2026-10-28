# Player prop markets (NBA, NFL)

The full NBA and NFL prop menus are covered end to end: simulator, lines, AI
Picks, settlement, learning-loop evidence, hit rates, cheat sheets and the
Projections page.

| Sport | Markets (simulator / market_lines key) |
|---|---|
| NBA | `pts` `reb` `ast` `fg3m` `stl` `blk` `tov` `pra` `pr` `pa` `ra` `stocks` (stl+blk) `fgm` `fga` `fg3a` `ftm` `fta` `oreb` `dreb` `double_double` `triple_double` |
| NFL | `pass_yds` `pass_att` `completions` `pass_td` `interceptions` `pass_rush_yds` `carries` `rush_yds` `rush_td` `targets` `receptions` `rec_yds` `rec_td` `rush_rec_yds` `tds` (anytime TD = over 0.5, 2+ TDs = over 1.5) `kick_pts` `fgm` |

## How each layer handles them

- **Simulators**
  - `nba_sim::PROPS` covers every NBA market. The first nine keep their old
    order, so existing evidence stays valid. Double-doubles count two of
    points, rebounds, assists, steals and blocks at 10 or more.
  - `nfl_sim::COMBOS` adds rush+rec yards, pass+rush yards, touchdowns and
    kicking points after the box-score stats.
  - Payloads carry each market a role is offered:
    - QB: passing, rushing, pass+rush yards, TDs.
    - Skill players: receiving, rushing, rush+rec yards, TDs.
    - Kicker: FG, XP, kicking points.
- **Lines**
  - ESPN/DraftKings two-way prop types are mapped in `espn_prop_market`. This
    adds P+R, P+A, R+A and steals+blocks, plus the NFL "Total …" types.
  - SportsGameOdds stat IDs are in `sgo_stat_ids`. Combos are '+' joins, and
    both orders are tried for the NFL ones.
  - Yes/no markets (`-game-yn-yes/no`: double-double, triple-double, touchdowns)
    are stored as over/under 0.5. Yes-only quotes are kept.
- **AI Picks**
  - A yes-only quote is paired with its complement price (`complement_price`).
    Its market probability is then the raw implied probability, with vig
    included, which is conservative.
  - Labels read "Double-Double Yes" and "Anytime TD Yes".
  - Settlement uses `nba_box_espn` and `nfl_game::full_stats`. `full_stats`
    is the full play-by-play box score and records zeros, so a 0-INT game
    settles.
- **Evidence**
  - Each market has a learning-loop forecast at the player's own average line,
    or 0.5 for yes/no. These are scored against the player's own prior
    frequency (baseline evidence; `evidence::pick_market`).
- **Hit rates, cheat sheets and Projections**
  - `NBA_STATS` and `NFL_STATS` gain the new keys.
  - Game logs carry the new columns. NBA comes from box scores. NFL derives
    `rush_rec_yds`, `pass_rush_yds`, `tds` and `touches` at nflverse ingest.
  - Logs written earlier are backfilled on each sync.

## Walk-forward check (`/learning/{sport}/scorecard`, 2026-10-27)

Brier vs the player's own prior frequency (negative = model better):

- **NBA (119 games):**
  - Better: P+R −0.009 ± 0.004, R+A −0.008 ± 0.004, P+A −0.006 ± 0.004.
  - Every other market is within ±0.005, except double-double, which is
    slightly worse (+0.009 ± 0.005).
- **NFL (120 games):**
  - Better: anytime TD −0.012 ± 0.003, targets −0.020 ± 0.005, receptions
    −0.013 ± 0.005, receiving yards −0.012 ± 0.005, 2+ TDs −0.008 ± 0.002,
    pass TDs −0.020 ± 0.012.
  - Worse: QB pass attempts (+0.024) and pass yards (+0.018).

No NFL kicker forecasts showed up in the 120-game sample.
