# Sports data foundation

What Monster OS stores per sport, where it comes from, what each source's terms say, and
how players and teams get one id. Measured on 2026-11-03. Phase 3 of
`monster-gpt-v2.md`.

## 1. Coverage

| Sport | Games (`sports_games`) | Playoffs | Player game logs | Logs include |
| --- | --- | --- | --- | --- |
| NFL | 7,239 regular season (1999-2026) | 309 since 1999: WC 120, DIV 108, CON 54, SB 27 | 155,819 (1999-2026) | regular season; **no game dates** (season + week only) |
| NBA | 8,486 regular season (2019-2026) + 18,166 older rows as `nba_legacy` (2012-2027) | 595 POST + 37 play-in (2019-2025) | 291,899 (2012-2026) | **regular season only** (ESPN `season_type = 2`) |
| NHL | 18,186 regular season (2008-2027 schedule); `nhl_games_api` 8,771 regular + 647 playoff + 515 preseason (2019-20 on) | in `nhl_games_api` only (game_type 3) | 251,219 (2008-2026) | **regular season only** |
| MLB | 17,910 regular season + 341 postseason (2019-2026) | yes | 524,529 (2019-2026) | regular season **and postseason** |

Consequences for answers:
- "Last N games" for a player counts regular-season games in the NBA, NHL and NFL, and
  includes postseason games in MLB. A stats answer must say which.
- NBA and NHL player logs leave out the playoffs on purpose: they are the inputs of the
  champion projection engines, and adding playoff games would change those inputs
  without a promotion. Playoff player stats, if wanted, go in a separate table.
- NFL playoff games were never stored before 2026-11-04: nflverse labels them by round
  (WC, DIV, CON, SB), not "POST", and the import kept only REG/POST. Fixed in
  `teams.rs` (`stored_game_type`). Every NFL model input stays on `game_type = 'REG'`
  (rating fit, team-points engine history, Elo history), so no forecast changed.
- Champions are derivable for all four leagues from stored games (NFL `SB`, NBA last
  `POST` game, MLB last `POST` game, NHL `nhl_games_api` game_type 3).
- No data for WNBA, college sports, soccer, tennis, golf or UFC.

## 2. Sources and their terms

`data_sources` (migrations 0041, 0044) holds each source with its access type, cost,
terms link, review date and notes. Summaries of the published terms, read on
2026-11-03. **These are summaries, not legal advice.**

| Source | Used for | What the terms say | Fit for Monster OS |
| --- | --- | --- | --- |
| ESPN (`site.api.espn.com`) | schedules, scores, injuries, live state, teams, NBA logs | Disney Terms of Use: no robots, data mining or scraping; no commercial use | Conflicts with automated collection. Personal local use is the most these terms could cover |
| NBA.com stats | NBA stats, referees | statistics only for news or private non-commercial use, with attribution; **not for gambling** or fantasy | Conflicts with a betting-research use |
| MLB Stats API | MLB games and logs | individual, non-commercial, non-bulk use only | Bulk ingestion is outside it |
| NHL web API | NHL games, shots, rosters | no unauthorized scraping; personal non-commercial use; no copying into a database without permission | Conflicts with automated collection |
| MoneyPuck | NHL shot data | free for non-commercial use with credit; no unapproved scraping | OK for personal non-commercial use with credit |
| nflverse | NFL play-by-play, schedules, rosters | CC-BY-4.0 | OK with attribution |
| FTN via nflverse | NFL charting | CC-BY-SA-4.0 (share-alike on derived data) | OK with attribution; share-alike for anything published |
| The Odds API (paid) | prices | display, storage, research and ML allowed; no resale as a data product | OK |
| SportsGameOdds (paid) | prices, props | terms page not found (404) | not reviewed |
| Polymarket | prediction-market data | personal licence; no data-mining tools or crawlers; no redistribution | Conflicts with automated collection unless the public API is separately permitted |
| Kalshi | prediction-market data | data terms not retrieved (rate limited) | not reviewed |

Every source except Monster OS's own outputs stays `review_pending`. Approving,
restricting or retiring a source is the owner's decision (`UPDATE data_sources SET
status = ...` as the owner role); the services cannot change this table.

## 3. Entities

`POST /entities/build` (projection-service, `entities.rs`) rebuilds teams and players:

- 125 teams: 30 NBA, 32 NFL, 32 NHL, 30 MLB from ESPN's league team lists. A team is kept
  only when its canonical code (`hitrates::canon`) is used by stored games, so All-Star
  and exhibition codes (EAST, LEB, STRIPES; NHL EIS, LAU, SCB) are not teams. Relocated
  franchises keep one entity with the old name as a dated alias (Oakland Raiders,
  San Diego Chargers, St. Louis Rams, Oakland Athletics). Renamed teams carry former
  names (Washington Redskins to 2019, Washington Football Team 2020-21). The Arizona
  Coyotes are their own entity: the NHL treats Utah as a new franchise.
- 11,678 players from `sports_players` (NBA ESPN ids, NFL gsis ids, NHL and MLB league
  ids). NHL names are stored abbreviated ("C. McDavid"); 799 current players get full
  names from the NHL rosters. Retired NHL players keep only the abbreviated name and
  their last name.
- 24,634 aliases (full name, stored name, last name at weight 0.5, team names, nicknames,
  cities at 0.6, codes) and 11,802 source ids.

Not built yet: game entities (resolved at query time from team and date), coaches,
venues, `nba_legacy` players (NBA.com ids, a different id space).

`GET /entities/resolve?q=&sport=&focus=` scores every alias match (exact 3, prefix 2,
contained 1, close spelling 1 minus 0.3 per edit) times the alias weight, plus 1 for an
active entity and 3 when the entity or its team is in the conversation's focus. Close
spellings are dropped when anything matched as written. The best candidate resolves
when it leads by 2 or more; otherwise the answer is `ambiguous` with up to four options
and the caller asks the user. Measured examples: "LeBron" resolves; "James" (NBA) is
ambiguous and resolves to LeBron James with him in focus; "Boston" is ambiguous until a
sport is given; "Oakland Raiders" resolves to the Las Vegas Raiders; "Rangers" asks NHL
or MLB.

`GET /entities/search?q=&sport=&kind=&limit=` returns ranked candidates for the @ picker.

The build is run by hand for now. A daily rebuild in the autopilot is proposed and needs
approval (it is a new scheduled job).
