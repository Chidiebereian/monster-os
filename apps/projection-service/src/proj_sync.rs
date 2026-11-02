//! Keeps the Projections page current (autopilot `sync:{sport}`; POST /sync/projections?sport=).
//!
//! The Projections page, Hit Rates and the prop matcher read the shared tables
//! sports_players / player_game_logs / player_projections / sports_games. Those
//! were filled once by bulk ingests (stats.nba.com, MoneyPuck, nflverse) and then
//! went stale: players were listed on teams they had left and the games stopped
//! at the last import. This module rebuilds them from the warehouses the
//! simulators already keep current, then refits every player projection:
//!
//! * NBA: ESPN box scores, keyed by ESPN athlete ids (the ids the simulator,
//!   picks, lines and headshots use). The old stats.nba.com rows move to sport
//!   `nba_legacy`; careers before ESPN coverage (2019-20) are kept by re-keying
//!   players whose name matches exactly one player on each side.
//! * NHL: the NHL API box scores (same NHL ids as before).
//! * NFL: the current-season nflverse weekly file (no 1999- re-download).
//! * MLB: refit only (its game logs are the simulator's own warehouse).
//!
//! A player's team is the team's live roster (ESPN / NHL API / nflverse weekly
//! rosters). Players on no roster have no team (free agents, retired). When a
//! roster fetch fails the latest box score's team stands.

use std::collections::{HashMap, HashSet};

use actix_web::{web, HttpResponse, Responder};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{fit_continuous, fit_count, Family, State, StatDef};

pub const SPORTS: &[&str] = &["nba", "nhl", "nfl", "mlb"];

fn dberr(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() { Some(d) => format!("{}: {}", d.code().code(), d.message()), None => e.to_string() }
}

/// Rebuild one sport's Projections-page data. Returns a summary.
pub async fn run(state: &State, sport: &str) -> Result<Value, String> {
    let started = std::time::Instant::now();
    let mut out = match sport {
        "nba" => nba(state).await?,
        "nhl" => nhl(state).await?,
        "nfl" => nfl(state).await?,
        "mlb" => json!({ "fit": local_post(state, "/fit/mlb").await }),
        _ => return Err(format!("sport must be one of {SPORTS:?}")),
    };
    out["sport"] = json!(sport);
    out["secs"] = json!(started.elapsed().as_secs());
    Ok(out)
}

/// POST to this service (the existing handlers that fit team models and ingest).
async fn local_post(state: &State, path: &str) -> Value {
    let port = std::env::var("PORT").unwrap_or_else(|_| "8121".into());
    let url = format!("http://127.0.0.1:{port}{path}");
    match state.http.post(&url).timeout(std::time::Duration::from_secs(1800)).send().await {
        Ok(r) => { let ok = r.status().is_success(); let body: Value = r.json().await.unwrap_or(Value::Null);
            json!({ "ok": ok, "summary": if ok { Value::Null } else { body } }) }
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }
}

/// Logs newer than this need re-syncing: two weeks before the newest synced game
/// (late stat corrections), or everything on the first run.
async fn since(db: &tokio_postgres::Client, sport: &str, synced_filter: &str) -> Result<chrono::NaiveDate, String> {
    let q = format!("SELECT max(game_date) FROM player_game_logs WHERE sport=$1 AND {synced_filter}");
    let d: Option<chrono::NaiveDate> = db.query_one(&q, &[&sport]).await.map_err(|e| dberr(&e))?.get(0);
    Ok(d.map(|d| d - chrono::Duration::days(14)).unwrap_or(chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()))
}

/// Set each player's team from live rosters. `rosters` is team -> player ids.
/// With `complete` (every team fetched), players on no roster lose their team.
async fn apply_rosters(db: &tokio_postgres::Client, sport: &str, rosters: &HashMap<String, Vec<String>>, complete: bool) -> Result<Value, String> {
    let mut moved = 0;
    for (team, ids) in rosters {
        moved += db.execute("UPDATE sports_players SET team=$3, updated_at=now() WHERE sport=$1 AND player_id = ANY($2) AND team IS DISTINCT FROM $3",
            &[&sport, ids, team]).await.map_err(|e| dberr(&e))?;
    }
    let mut released = 0;
    if complete {
        let all: Vec<String> = rosters.values().flatten().cloned().collect();
        released = db.execute("UPDATE sports_players SET team=NULL, updated_at=now() WHERE sport=$1 AND team IS NOT NULL AND NOT (player_id = ANY($2))",
            &[&sport, &all]).await.map_err(|e| dberr(&e))?;
    }
    Ok(json!({ "teams": rosters.len(), "complete": complete, "team_changed": moved, "no_roster": released }))
}

/// first/last season and games from the stored logs.
async fn recount(db: &tokio_postgres::Client, sport: &str) -> Result<(), String> {
    db.execute("UPDATE sports_players sp SET first_season=x.f, last_season=x.l, games=x.n::int FROM \
        (SELECT player_id, min(season) f, max(season) l, count(*) n FROM player_game_logs WHERE sport=$1 GROUP BY 1) x \
        WHERE sp.sport=$1 AND sp.player_id=x.player_id", &[&sport]).await.map_err(|e| dberr(&e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Fit (the shared empirical-Bayes recency-weighted model, as each sport's ingest did)
// ---------------------------------------------------------------------------

/// Fit `defs` for every player from the stored logs and replace the sport's
/// projections. A stat is fitted for a player only when the logs carry it (so
/// goalies get goalie stats and skaters skater stats).
pub(crate) async fn fit_logs(db: &tokio_postgres::Client, sport: &str, defs: &[&StatDef], min_games: usize, prior_k: f64, halflife: f64) -> Result<Value, String> {
    use futures::TryStreamExt;
    // each stat's value in a game, or NULL when the game does not count for it (no opportunity, or not tracked)
    let cols: Vec<String> = defs.iter().map(|d| format!("CASE WHEN COALESCE((stats->>'{o}')::float8, 0) >= 1 THEN (stats->>'{c}')::float8 END", o = d.opportunity, c = d.column)).collect();
    // league rate per stat (the prior's centre)
    let means = db.query_one(&format!("SELECT {} FROM player_game_logs WHERE sport=$1", cols.iter().map(|c| format!("avg({c})")).collect::<Vec<_>>().join(", ")), &[&sport])
        .await.map_err(|e| dberr(&e))?;
    let league: Vec<f64> = (0..defs.len()).map(|i| means.get::<_, Option<f64>>(i).unwrap_or(0.0)).collect();
    // stream the logs a player at a time, oldest first (the whole history does not fit in memory here)
    let sql = format!("SELECT player_id, {} FROM player_game_logs WHERE sport=$1         ORDER BY player_id, game_date NULLS LAST, season, CASE WHEN week ~ '^[0-9]+$' THEN lpad(week, 4, '0') ELSE week END", cols.join(", "));
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 1] = [&sport];
    let stream = db.query_raw(&sql, params).await.map_err(|e| dberr(&e))?;
    futures::pin_mut!(stream);
    #[derive(Default)]
    struct Out { ids: Vec<String>, mean: Vec<f64>, sd: Vec<f64>, disp: Vec<f64>, n: Vec<i32>, career: Vec<f64>, recent: Vec<f64>, params: Vec<String>, q05: Vec<f64>, q95: Vec<f64> }
    let mut out: Vec<Out> = defs.iter().map(|_| Out::default()).collect();
    let mut seen = 0usize;
    let mut cur: Option<String> = None;
    let mut vals: Vec<Vec<f64>> = vec![Vec::new(); defs.len()];
    let flush = |pid: &str, vals: &mut Vec<Vec<f64>>, out: &mut Vec<Out>| {
        for (i, d) in defs.iter().enumerate() {
            let v = std::mem::take(&mut vals[i]);
            if v.len() < min_games { continue; }
            let p = match d.family { Family::Count => fit_count(&v, league[i], prior_k, halflife), Family::Continuous => fit_continuous(&v, league[i], prior_k, halflife) };
            let o = &mut out[i];
            o.ids.push(pid.to_string()); o.mean.push(p.proj_mean); o.sd.push(p.proj_sd); o.disp.push(p.dispersion); o.n.push(p.sample_games);
            o.career.push(p.career_mean); o.recent.push(p.recent_mean); o.params.push(format!("{}:{pid}", d.key));
            o.q05.push((p.proj_mean - 1.645 * p.proj_sd).max(0.0)); o.q95.push(p.proj_mean + 1.645 * p.proj_sd);
        }
    };
    while let Some(r) = stream.try_next().await.map_err(|e| dberr(&e))? {
        let pid: String = r.get(0);
        if cur.as_deref() != Some(pid.as_str()) {
            if let Some(prev) = cur.take() { flush(&prev, &mut vals, &mut out); }
            cur = Some(pid);
            seen += 1;
        }
        for i in 0..defs.len() { if let Some(x) = r.get::<_, Option<f64>>(i + 1) { vals[i].push(x); } }
    }
    if let Some(prev) = cur.take() { flush(&prev, &mut vals, &mut out); }

    let mut by_stat = serde_json::Map::new();
    for (d, o) in defs.iter().zip(&out) {
        let mid = format!("{sport}-{}-v1", d.key);
        db.execute("INSERT INTO models (id, name, sport, version, type, status) VALUES ($1, $2, $3, '1', $4, 'active') ON CONFLICT (id) DO NOTHING",
            &[&mid, &format!("{} {} (EB, recency-weighted)", sport.to_uppercase(), d.label), &sport, &d.family.model_type()]).await.map_err(|e| dberr(&e))?;
        // players no longer fitted (no logs left) drop out
        db.execute("DELETE FROM player_projections WHERE sport=$1 AND stat=$2 AND game_id='' AND NOT (player_id = ANY($3))", &[&sport, &d.key, &o.ids]).await.map_err(|e| dberr(&e))?;
        db.execute("INSERT INTO player_projections (sport, player_id, stat, game_id, model_id, proj_mean, proj_sd, dispersion, sample_games, career_mean, recent_mean, family, unit)             SELECT $1, u.pid, $2, '', $3, u.m, u.s, u.d, u.n, u.c, u.r, $4, $5 FROM UNNEST($6::text[], $7::float8[], $8::float8[], $9::float8[], $10::int[], $11::float8[], $12::float8[]) AS u(pid, m, s, d, n, c, r)             ON CONFLICT (sport, player_id, stat, game_id) DO UPDATE SET model_id=EXCLUDED.model_id, proj_mean=EXCLUDED.proj_mean, proj_sd=EXCLUDED.proj_sd,               dispersion=EXCLUDED.dispersion, sample_games=EXCLUDED.sample_games, career_mean=EXCLUDED.career_mean, recent_mean=EXCLUDED.recent_mean,               family=EXCLUDED.family, unit=EXCLUDED.unit, as_of=now()",
            &[&sport, &d.key, &mid, &d.family.as_str(), &d.unit, &o.ids, &o.mean, &o.sd, &o.disp, &o.n, &o.career, &o.recent]).await.map_err(|e| dberr(&e))?;
        db.execute("INSERT INTO posterior_features (model_id, parameter, mean, sd, q05, q95, rhat, ess)             SELECT $1, u.p, u.m, u.s, u.a, u.b, 1.0, NULL FROM UNNEST($2::text[], $3::float8[], $4::float8[], $5::float8[], $6::float8[]) AS u(p, m, s, a, b)             ON CONFLICT (model_id, parameter) DO UPDATE SET mean=EXCLUDED.mean, sd=EXCLUDED.sd, q05=EXCLUDED.q05, q95=EXCLUDED.q95",
            &[&mid, &o.params, &o.mean, &o.sd, &o.q05, &o.q95]).await.map_err(|e| dberr(&e))?;
        by_stat.insert(d.key.into(), json!(o.ids.len()));
    }
    Ok(json!({ "players_seen": seen, "fitted": by_stat }))
}

// ---------------------------------------------------------------------------
// NBA
// ---------------------------------------------------------------------------

/// Letters only, lower-cased, for matching names across sources ("P.J. Tucker" = "PJ Tucker").
const NAME_KEY: &str = "lower(regexp_replace({c}, '[^A-Za-z]', '', 'g'))";

async fn nba(state: &State) -> Result<Value, String> {
    let db = &state.db;
    let key = |c: &str| NAME_KEY.replace("{c}", c);
    // 1. once: the stats.nba.com data (game ids "00...") moves to sport `nba_legacy`,
    //    and careers before ESPN coverage are copied back under ESPN ids. The two id
    //    spaces overlap numerically, so legacy rows are told apart by their game ids.
    let first_run = db.query_one("SELECT NOT EXISTS (SELECT 1 FROM player_game_logs WHERE sport='nba' AND week NOT LIKE '00%')         AND EXISTS (SELECT 1 FROM player_game_logs WHERE sport='nba')", &[]).await.map_err(|e| dberr(&e))?.get::<_, bool>(0);
    let mut legacy = json!({ "migrated": false });
    if first_run {
        let mv = |t: &str| format!("UPDATE {t} SET sport='nba_legacy' WHERE sport='nba'");
        let moved_logs = db.execute(&mv("player_game_logs"), &[]).await.map_err(|e| dberr(&e))?;
        let moved_proj = db.execute(&mv("player_projections"), &[]).await.map_err(|e| dberr(&e))?;
        let moved_players = db.execute(&mv("sports_players"), &[]).await.map_err(|e| dberr(&e))?;
        let moved_games = db.execute(&mv("sports_games"), &[]).await.map_err(|e| dberr(&e))?;
        let first_espn: Option<chrono::NaiveDate> = db.query_one("SELECT min(game_date) FROM nba_games_espn WHERE completed AND season_type=2", &[])
            .await.map_err(|e| dberr(&e))?.get(0);
        // a name that is unique on both sides
        let copied = match first_espn {
            Some(d) => db.execute(&format!("WITH ue AS (SELECT k, min(player_id) pid FROM (SELECT DISTINCT player_id, {} k FROM nba_box_espn) e GROUP BY k HAVING count(*) = 1),                 ul AS (SELECT {} k, min(player_id) pid FROM sports_players WHERE sport='nba_legacy' GROUP BY 1 HAVING count(*) = 1),                 m AS (SELECT ul.pid legacy, ue.pid espn FROM ul JOIN ue USING (k))                 INSERT INTO player_game_logs (sport, player_id, season, week, game_date, team, opponent, stats)                 SELECT 'nba', m.espn, l.season, l.week, l.game_date, l.team, l.opponent, l.stats FROM player_game_logs l JOIN m ON l.player_id = m.legacy                 WHERE l.sport='nba_legacy' AND l.game_date < $1 ON CONFLICT DO NOTHING", key("name"), key("display_name")), &[&d]).await.map_err(|e| dberr(&e))?,
            None => 0,
        };
        legacy = json!({ "migrated": true, "moved_logs": moved_logs, "moved_projections": moved_proj, "moved_players": moved_players, "moved_games": moved_games,
            "pre_espn_logs_copied": copied, "espn_from": first_espn });
    }

    // 2. game logs from ESPN box scores (regular season, played)
    let from = since(db, "nba", "week NOT LIKE '00%'").await?;
    let logs = db.execute("INSERT INTO player_game_logs (sport, player_id, season, week, game_date, team, opponent, stats) \
        SELECT 'nba', x.player_id, x.season - 1, x.espn_id, x.game_date, x.team, x.opp, jsonb_build_object( \
          'pts', x.p, 'reb', x.r, 'ast', x.a, 'threes', x.t, 'stl', x.s, 'blk', x.b, 'tov', x.tv, 'minutes', x.mn, 'min_played', x.mn, \
          'pra', x.p + x.r + x.a, 'pr', x.p + x.r, 'pa', x.p + x.a, 'ra', x.r + x.a, 'stocks', x.s + x.b, \
          'fgm', x.fgm, 'fga', x.fga, 'fg3a', x.fg3a, 'ftm', x.ftm, 'fta', x.fta, 'oreb', x.oreb, 'dreb', x.dreb, \
          'dd', ((x.p >= 10)::int + (x.r >= 10)::int + (x.a >= 10)::int + (x.s >= 10)::int + (x.b >= 10)::int >= 2)::int, \
          'td', ((x.p >= 10)::int + (x.r >= 10)::int + (x.a >= 10)::int + (x.s >= 10)::int + (x.b >= 10)::int >= 3)::int) \
        FROM (SELECT b.player_id, g.season, b.espn_id, g.game_date, b.team, CASE WHEN b.team = g.home THEN g.away ELSE g.home END opp, \
                COALESCE(b.pts,0) p, COALESCE(b.oreb,0) + COALESCE(b.dreb,0) r, COALESCE(b.ast,0) a, COALESCE(b.fg3m,0) t, COALESCE(b.stl,0) s, \
                COALESCE(b.blk,0) b, COALESCE(b.tov,0) tv, b.min mn, COALESCE(b.fgm,0) fgm, COALESCE(b.fga,0) fga, COALESCE(b.fg3a,0) fg3a, \
                COALESCE(b.ftm,0) ftm, COALESCE(b.fta,0) fta, COALESCE(b.oreb,0) oreb, COALESCE(b.dreb,0) dreb \
              FROM nba_box_espn b JOIN nba_games_espn g USING (espn_id) \
              WHERE g.completed AND g.season_type = 2 AND NOT b.dnp AND COALESCE(b.min, 0) >= 1 AND g.game_date >= $1) x \
        ON CONFLICT (sport, player_id, season, week) DO UPDATE SET game_date=EXCLUDED.game_date, team=EXCLUDED.team, opponent=EXCLUDED.opponent, stats=EXCLUDED.stats",
        &[&from]).await.map_err(|e| dberr(&e))?;

    // logs written before the full prop menu: add its columns from the box scores
    db.execute("UPDATE player_game_logs l SET stats = l.stats || jsonb_build_object( \
          'stocks', COALESCE(b.stl,0) + COALESCE(b.blk,0), 'fgm', COALESCE(b.fgm,0), 'fga', COALESCE(b.fga,0), 'fg3a', COALESCE(b.fg3a,0), \
          'ftm', COALESCE(b.ftm,0), 'fta', COALESCE(b.fta,0), 'oreb', COALESCE(b.oreb,0), 'dreb', COALESCE(b.dreb,0), \
          'dd', (t.n >= 2)::int, 'td', (t.n >= 3)::int) \
        FROM nba_box_espn b, LATERAL (SELECT (COALESCE(b.pts,0) >= 10)::int + (COALESCE(b.oreb,0) + COALESCE(b.dreb,0) >= 10)::int + (COALESCE(b.ast,0) >= 10)::int \
                                            + (COALESCE(b.stl,0) >= 10)::int + (COALESCE(b.blk,0) >= 10)::int AS n) t \
        WHERE l.sport = 'nba' AND l.week = b.espn_id AND l.player_id = b.player_id AND NOT (l.stats ? 'fgm')", &[]).await.map_err(|e| dberr(&e))?;

    // 3. players: name, position and team from the latest box score (preseason included)
    db.execute("INSERT INTO sports_players (sport, player_id, display_name, position, team, first_season, last_season, games) \
        SELECT DISTINCT ON (b.player_id) 'nba', b.player_id, b.name, NULLIF(b.pos, ''), b.team, 0, 0, 0 \
        FROM nba_box_espn b JOIN nba_games_espn g USING (espn_id) ORDER BY b.player_id, g.game_date DESC \
        ON CONFLICT (sport, player_id) DO UPDATE SET display_name=EXCLUDED.display_name, \
          position=COALESCE(EXCLUDED.position, sports_players.position), team=EXCLUDED.team, updated_at=now()", &[]).await.map_err(|e| dberr(&e))?;
    recount(db, "nba").await?;
    // live rosters
    let (rosters, complete) = nba_rosters(state).await;
    let roster = apply_rosters(db, "nba", &rosters, complete).await?;

    // 4. games (ESPN ids and team codes, the ones the rosters use)
    let games = db.execute("INSERT INTO sports_games (sport, game_id, season, week, game_date, home_team, away_team, status, home_score, away_score, total_line, game_type) \
        SELECT 'nba', espn_id, season - 1, game_date::text, game_date, home, away, CASE WHEN completed THEN 'final' WHEN game_date < current_date - 1 THEN 'canceled' ELSE 'scheduled' END, \
          home_score, away_score, over_under, CASE season_type WHEN 1 THEN 'PRE' WHEN 2 THEN 'REG' WHEN 3 THEN 'POST' WHEN 5 THEN 'PLAYIN' ELSE 'OTHER' END \
        FROM nba_games_espn WHERE game_date >= $1 \
        ON CONFLICT (sport, game_id) DO UPDATE SET game_date=EXCLUDED.game_date, home_team=EXCLUDED.home_team, away_team=EXCLUDED.away_team, status=EXCLUDED.status, \
          home_score=EXCLUDED.home_score, away_score=EXCLUDED.away_score, total_line=EXCLUDED.total_line, game_type=EXCLUDED.game_type, updated_at=now()",
        &[&from]).await.map_err(|e| dberr(&e))?;

    // 5. refit
    let defs: Vec<&StatDef> = crate::nba::NBA_STATS.iter().collect();
    let fit = fit_logs(db, "nba", &defs, 15, 10.0, 60.0).await?;
    let teams = local_post(state, "/fit/nba/teams").await;
    Ok(json!({ "legacy": legacy,
        "logs_upserted": logs, "logs_since": from, "games_upserted": games, "rosters": roster, "fit": fit, "team_model": teams }))
}

/// ESPN team rosters: team abbreviation -> ESPN athlete ids; and whether all 30 came back.
async fn nba_rosters(state: &State) -> (HashMap<String, Vec<String>>, bool) {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let Ok(v) = crate::nba_espn::get_json(&state.http, "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/teams").await else { return (out, false) };
    let teams: Vec<(String, String)> = v["sports"][0]["leagues"][0]["teams"].as_array().into_iter().flatten()
        .filter_map(|t| Some((t["team"]["id"].as_str()?.to_string(), t["team"]["abbreviation"].as_str()?.to_string()))).collect();
    for (id, abbr) in &teams {
        let url = format!("https://site.api.espn.com/apis/site/v2/sports/basketball/nba/teams/{id}/roster");
        let Ok(r) = crate::nba_espn::get_json(&state.http, &url).await else { continue };
        let ids: Vec<String> = r["athletes"].as_array().into_iter().flatten().filter_map(|a| a["id"].as_str().map(str::to_string)).collect();
        if !ids.is_empty() { out.insert(abbr.clone(), ids); }
    }
    let complete = teams.len() >= 30 && out.len() == teams.len();
    (out, complete)
}

// ---------------------------------------------------------------------------
// NHL
// ---------------------------------------------------------------------------

async fn nhl(state: &State) -> Result<Value, String> {
    let db = &state.db;
    let from = since(db, "nhl", "true").await?;
    let logs = db.execute("INSERT INTO player_game_logs (sport, player_id, season, week, game_date, team, opponent, stats) \
        SELECT 'nhl', b.player_id::text, g.season / 10000, g.game_id::text, g.game_date, b.team, CASE WHEN b.team = g.home THEN g.away ELSE g.home END, \
          CASE WHEN b.pos = 'G' THEN jsonb_build_object('saves', COALESCE(b.saves,0), 'shots_against', COALESCE(b.shots_against,0), \
                 'goals_against', COALESCE(b.goals_against,0), 'toi_seconds', b.toi_sec) \
               ELSE jsonb_build_object('shots', COALESCE(b.sog,0), 'goals', COALESCE(b.goals,0), 'assists', COALESCE(b.assists,0), \
                 'points', COALESCE(b.goals,0) + COALESCE(b.assists,0), 'hits', COALESCE(b.hits,0), 'pim', COALESCE(b.pim,0), \
                 'toi', b.toi_sec / 60.0, 'toi_seconds', b.toi_sec) END \
        FROM nhl_box_api b JOIN nhl_games_api g USING (game_id) \
        WHERE g.completed AND g.game_type = 2 AND COALESCE(b.toi_sec, 0) > 0 AND g.game_date >= $1 \
        ON CONFLICT (sport, player_id, season, week) DO UPDATE SET game_date=EXCLUDED.game_date, team=EXCLUDED.team, opponent=EXCLUDED.opponent, stats=EXCLUDED.stats",
        &[&from]).await.map_err(|e| dberr(&e))?;
    db.execute("INSERT INTO sports_players (sport, player_id, display_name, position, team, first_season, last_season, games) \
        SELECT DISTINCT ON (b.player_id) 'nhl', b.player_id::text, b.name, NULLIF(b.pos, ''), b.team, 0, 0, 0 \
        FROM nhl_box_api b JOIN nhl_games_api g USING (game_id) WHERE g.game_date >= $1 ORDER BY b.player_id, g.game_date DESC \
        ON CONFLICT (sport, player_id) DO UPDATE SET display_name=EXCLUDED.display_name, team=EXCLUDED.team, updated_at=now()",
        &[&from]).await.map_err(|e| dberr(&e))?;
    recount(db, "nhl").await?;
    let (rosters, complete) = nhl_rosters(state).await;
    let roster = apply_rosters(db, "nhl", &rosters, complete).await?;
    let games = db.execute("INSERT INTO sports_games (sport, game_id, season, week, game_date, home_team, away_team, status, home_score, away_score, game_type) \
        SELECT 'nhl', game_id::text, season / 10000, game_date::text, game_date, home, away, CASE WHEN completed THEN 'final' WHEN game_date < current_date - 1 THEN 'canceled' ELSE 'scheduled' END, \
          CASE WHEN completed THEN home_score END, CASE WHEN completed THEN away_score END, CASE game_type WHEN 1 THEN 'PRE' WHEN 2 THEN 'REG' WHEN 3 THEN 'POST' ELSE 'OTHER' END \
        FROM nhl_games_api WHERE game_date >= $1 \
        ON CONFLICT (sport, game_id) DO UPDATE SET game_date=EXCLUDED.game_date, status=EXCLUDED.status, home_score=EXCLUDED.home_score, \
          away_score=EXCLUDED.away_score, updated_at=now()", &[&from]).await.map_err(|e| dberr(&e))?;
    let defs: Vec<&StatDef> = crate::nhl::NHL_STATS.iter().chain(crate::nhl::GOALIE_STATS).collect();
    let fit = fit_logs(db, "nhl", &defs, 10, 8.0, 60.0).await?;
    let teams = local_post(state, "/ingest/nhl/teams").await;
    Ok(json!({ "logs_upserted": logs, "logs_since": from, "games_upserted": games, "rosters": roster, "fit": fit, "team_model": teams }))
}

/// NHL API current rosters: team -> player ids.
async fn nhl_rosters(state: &State) -> (HashMap<String, Vec<String>>, bool) {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let Ok(rows) = state.db.query("SELECT DISTINCT home FROM nhl_games_api WHERE game_type = 2 AND game_date > current_date - 200", &[]).await else { return (out, false) };
    let teams: HashSet<String> = rows.iter().map(|r| r.get(0)).collect();
    for t in &teams {
        let Ok(v) = crate::nba_espn::get_json(&state.http, &format!("https://api-web.nhle.com/v1/roster/{t}/current")).await else { continue };
        let ids: Vec<String> = ["forwards", "defensemen", "goalies"].iter().flat_map(|g| v[*g].as_array().cloned().unwrap_or_default())
            .filter_map(|p| p["id"].as_i64().map(|x| x.to_string())).collect();
        if !ids.is_empty() { out.insert(t.clone(), ids); }
    }
    let complete = teams.len() >= 32 && out.len() == teams.len();
    (out, complete)
}

// ---------------------------------------------------------------------------
// NFL
// ---------------------------------------------------------------------------

async fn nfl(state: &State) -> Result<Value, String> {
    let db = &state.db;
    // this season's (and last season's) weekly files only; the fit below reads every stored season
    let ingest = local_post(state, "/ingest/nfl?sources=current&fit=false").await;
    if !ingest["ok"].as_bool().unwrap_or(false) { return Err(format!("nflverse weekly ingest: {ingest}")); }
    recount(db, "nfl").await?;
    db.execute("UPDATE player_game_logs SET stats = stats || jsonb_build_object( \
          'rush_rec_yds', COALESCE((stats->>'rushing_yards')::float8, 0) + COALESCE((stats->>'receiving_yards')::float8, 0), \
          'pass_rush_yds', COALESCE((stats->>'passing_yards')::float8, 0) + COALESCE((stats->>'rushing_yards')::float8, 0), \
          'tds', COALESCE((stats->>'rushing_tds')::float8, 0) + COALESCE((stats->>'receiving_tds')::float8, 0), \
          'touches', COALESCE((stats->>'carries')::float8, 0) + COALESCE((stats->>'targets')::float8, 0)) \
        WHERE sport = 'nfl' AND NOT (stats ? 'tds')", &[]).await.map_err(|e| dberr(&e))?;
    // weekly rosters: each team's latest roster week (teams on a bye are a week behind),
    // and a player listed by two teams (traded) belongs to the more recent listing
    let rows = db.query("WITH cur AS (SELECT * FROM nfl_roster_weekly WHERE season = (SELECT max(season) FROM nfl_roster_weekly)), \
        last AS (SELECT team, max(week) w FROM cur GROUP BY team), \
        latest AS (SELECT DISTINCT ON (c.gsis_id) c.gsis_id, c.team, c.status FROM cur c JOIN last l ON l.team = c.team AND l.w = c.week \
                   ORDER BY c.gsis_id, c.week DESC) \
        SELECT team, array_agg(gsis_id) FROM latest WHERE status IN ('ACT', 'RES', 'INA', 'DEV', 'EXE') GROUP BY 1", &[]).await.map_err(|e| dberr(&e))?;
    let rosters: HashMap<String, Vec<String>> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    let complete = rosters.len() >= 32;
    let roster = apply_rosters(db, "nfl", &rosters, complete).await?;
    let defs: Vec<&StatDef> = crate::NFL_STATS.iter().collect();
    let fit = fit_logs(db, "nfl", &defs, 8, 6.0, 34.0).await?;
    let teams = local_post(state, "/ingest/nfl/teams").await;
    Ok(json!({ "ingest": ingest, "rosters": roster, "fit": fit, "team_model": teams }))
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SyncQ { sport: String, #[serde(default)] wait: bool }

/// POST /sync/projections?sport=[&wait=true] — rebuild now. Runs on the server
/// and returns at once unless `wait=true`; the result lands in the
/// `sync:{sport}` job row.
pub async fn sync_http(state: web::Data<State>, q: web::Query<SyncQ>) -> impl Responder {
    if !SPORTS.contains(&q.sport.as_str()) { return HttpResponse::BadRequest().json(json!({ "error": format!("sport must be one of {SPORTS:?}") })); }
    let (st, sport) = (state.clone(), q.sport.clone());
    if q.wait {
        return match run(&st, &sport).await { Ok(v) => HttpResponse::Ok().json(v), Err(e) => HttpResponse::InternalServerError().json(json!({ "error": e })) };
    }
    actix_web::rt::spawn(async move {
        let v = run(&st, &sport).await;
        crate::autopilot::mark(&st.db, &format!("sync:{sport}"), v.is_ok(), &v.unwrap_or_else(|e| json!({ "error": e }))).await;
    });
    HttpResponse::Accepted().json(json!({ "sport": q.sport, "started": true }))
}
