//! NFL team model (the "for teams" spec: Andrew Mack's log-linear Poisson).
//!
//! Each team has an offensive and a defensive rating; a game's expected points
//! are log-linear in them plus a home-field term:
//!   mu = exp(intercept + off[team] - def[opp] + home_adv*is_home)
//! Points are drawn Negative-Binomial (Poisson over-dispersed, dispersion fit
//! from residuals), so a matchup is simulated 100k times to give win / spread /
//! total distributions. Ratings are fit by IRLS-style coordinate-Newton updates
//! on the recency-weighted Poisson log-likelihood with a ridge prior (partial
//! pooling toward the league mean) — the posterior mode of the hierarchical
//! model. Full MCMC posterior sampling and the per-scoring-type (rush/pass TD,
//! FG, XP, safety) decomposition from the R/Stan reference are the documented
//! next step; this v1 models total team points from real final scores (nflverse
//! games, 1999-present).

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Responder};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::State;

const GAMES_URL: &str = "https://github.com/nflverse/nfldata/raw/master/data/games.csv";
pub const TEAM_MODEL: &str = "nfl-team-points-v1";

fn internal(msg: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(teams): {msg}");
    HttpResponse::InternalServerError().json(json!({ "error": msg.to_string() }))
}
fn bad_request(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({ "error": msg.into() }))
}

// --------------------------------------------------------------------------
// RNG (SplitMix64) with Gamma / Poisson / Negative-Binomial draws.
// --------------------------------------------------------------------------
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    }
    fn normal(&mut self) -> f64 {
        let (u1, u2) = (self.uniform(), self.uniform());
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
    fn gamma(&mut self, shape: f64, scale: f64) -> f64 {
        if shape < 1.0 {
            let g = self.gamma(shape + 1.0, 1.0);
            return scale * g * self.uniform().powf(1.0 / shape);
        }
        let d = shape - 1.0 / 3.0;
        let c = 1.0 / (9.0 * d).sqrt();
        loop {
            let x = self.normal();
            let v = (1.0 + c * x).powi(3);
            if v <= 0.0 {
                continue;
            }
            let u = self.uniform();
            if u.ln() < 0.5 * x * x + d - d * v + d * v.ln() {
                return scale * d * v;
            }
        }
    }
    fn poisson(&mut self, lambda: f64) -> f64 {
        if lambda <= 0.0 {
            return 0.0;
        }
        if lambda > 30.0 {
            return (lambda + lambda.sqrt() * self.normal()).round().max(0.0);
        }
        let l = (-lambda).exp();
        let (mut k, mut p) = (0.0f64, 1.0f64);
        loop {
            k += 1.0;
            p *= self.uniform();
            if p <= l {
                return k - 1.0;
            }
        }
    }
    fn negbin(&mut self, mean: f64, r: f64) -> f64 {
        if mean <= 0.0 {
            return 0.0;
        }
        let lambda = self.gamma(r, mean / r);
        self.poisson(lambda)
    }
}

// --------------------------------------------------------------------------
// Ingestion + fit
// --------------------------------------------------------------------------

struct Game {
    game_id: String,
    season: i32,
    week: String,
    game_type: String,
    date: Option<NaiveDate>,
    home: String,
    away: String,
    home_score: Option<i32>,
    away_score: Option<i32>,
    spread_line: Option<f64>,
    total_line: Option<f64>,
}

#[derive(Deserialize)]
pub struct IngestQuery {
    #[serde(default = "d_halflife")]
    halflife_seasons: f64,
    #[serde(default = "d_epochs")]
    epochs: usize,
    #[serde(default = "d_ridge")]
    ridge: f64,
}
fn d_halflife() -> f64 { 1.5 }
fn d_epochs() -> usize { 80 }
fn d_ridge() -> f64 { 0.5 }

pub async fn ingest_teams(state: web::Data<State>, q: web::Query<IngestQuery>) -> impl Responder {
    let q = q.into_inner();
    let bytes = match state.http.get(GAMES_URL).send().await.and_then(|r| r.error_for_status()) {
        Ok(r) => match r.bytes().await { Ok(b) => b, Err(e) => return internal(format!("games body: {e}")) },
        Err(e) => return internal(format!("games fetch: {e}")),
    };
    let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_reader(bytes.as_ref());
    let headers = match rdr.headers() { Ok(h) => h.clone(), Err(e) => return internal(format!("csv headers: {e}")) };
    let col = |n: &str| headers.iter().position(|h| h == n);
    let (Some(ig), Some(is), Some(ity), Some(iw), Some(igd), Some(iaway), Some(ias), Some(ihome), Some(ihs)) = (
        col("game_id"), col("season"), col("game_type"), col("week"), col("gameday"),
        col("away_team"), col("away_score"), col("home_team"), col("home_score"),
    ) else {
        return internal("games schema changed: a required column is missing");
    };
    let isp = col("spread_line");
    let itl = col("total_line");
    let geti = |r: &csv::StringRecord, i: usize| r.get(i).and_then(|s| s.parse::<i32>().ok());
    let getf = |r: &csv::StringRecord, i: Option<usize>| i.and_then(|i| r.get(i)).and_then(|s| s.parse::<f64>().ok());

    let mut games: Vec<Game> = Vec::new();
    for rec in rdr.records() {
        let r = match rec { Ok(r) => r, Err(e) => return internal(format!("csv row: {e}")) };
        let gt = r.get(ity).unwrap_or("").to_string();
        if gt != "REG" && gt != "POST" {
            continue; // skip preseason
        }
        games.push(Game {
            game_id: r.get(ig).unwrap_or("").to_string(),
            season: r.get(is).and_then(|s| s.parse().ok()).unwrap_or(0),
            week: r.get(iw).unwrap_or("").to_string(),
            game_type: gt,
            date: r.get(igd).and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()),
            home: r.get(ihome).unwrap_or("").to_string(),
            away: r.get(iaway).unwrap_or("").to_string(),
            home_score: geti(&r, ihs),
            away_score: geti(&r, ias),
            spread_line: getf(&r, isp),
            total_line: getf(&r, itl),
        });
    }
    if games.is_empty() {
        return internal("no games parsed");
    }

    // store all games (completed + scheduled)
    if let Err(e) = store_games(&state.db, &games).await {
        return internal(e);
    }

    // fit on completed REG games
    let fit = match fit_ratings(&games, q.halflife_seasons, q.epochs, q.ridge) {
        Some(f) => f,
        None => return internal("not enough completed games to fit"),
    };
    if let Err(e) = store_ratings(&state.db, &fit).await {
        return internal(e);
    }

    // top offenses / defenses for the response
    let mut off: Vec<(&String, f64)> = fit.off.iter().map(|(t, v)| (t, *v)).collect();
    off.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut def: Vec<(&String, f64)> = fit.def.iter().map(|(t, v)| (t, *v)).collect();
    def.sort_by(|a, b| b.1.total_cmp(&a.1));
    let top = |v: &[(&String, f64)]| -> Vec<Value> {
        v.iter().take(5).map(|(t, r)| json!({ "team": t, "rating": (r*1000.0).round()/1000.0 })).collect()
    };
    HttpResponse::Ok().json(json!({
        "model_id": TEAM_MODEL, "games_ingested": games.len(), "teams": fit.off.len(),
        "fit_on_games": fit.n_games, "home_advantage": fit.home_adv, "intercept": fit.intercept,
        "mean_points": (fit.intercept.exp()*1000.0).round()/1000.0, "dispersion": fit.dispersion,
        "halflife_seasons": q.halflife_seasons, "epochs": q.epochs,
        "top_offense": top(&off), "top_defense": top(&def),
    }))
}

struct Fit {
    off: HashMap<String, f64>,
    def: HashMap<String, f64>,
    home_adv: f64,
    intercept: f64,
    dispersion: f64,
    n_games: usize,
}

/// IRLS-style coordinate-Newton fit of the recency-weighted Poisson model.
fn fit_ratings(games: &[Game], halflife: f64, epochs: usize, ridge: f64) -> Option<Fit> {
    // completed REG games only
    let completed: Vec<&Game> = games
        .iter()
        .filter(|g| g.game_type == "REG" && g.home_score.is_some() && g.away_score.is_some())
        .collect();
    if completed.len() < 50 {
        return None;
    }
    let max_season = completed.iter().map(|g| g.season).max().unwrap_or(0) as f64;

    // team index
    let mut idx: HashMap<String, usize> = HashMap::new();
    for g in &completed {
        let n = idx.len();
        idx.entry(g.home.clone()).or_insert(n);
        let n = idx.len();
        idx.entry(g.away.clone()).or_insert(n);
    }
    let t = idx.len();
    let names: Vec<String> = {
        let mut v = vec![String::new(); t];
        for (k, &i) in &idx {
            v[i] = k.clone();
        }
        v
    };

    // observations: (y, team, opp, is_home, weight)
    let (mut y, mut team, mut opp, mut home, mut w) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for g in &completed {
        let wt = 0.5f64.powf((max_season - g.season as f64) / halflife);
        let (h, a) = (idx[&g.home], idx[&g.away]);
        y.push(g.home_score.unwrap() as f64); team.push(h); opp.push(a); home.push(1.0); w.push(wt);
        y.push(g.away_score.unwrap() as f64); team.push(a); opp.push(h); home.push(0.0); w.push(wt);
    }
    let m = y.len();

    // per-team observation indices (as offense, as defense/opponent)
    let mut as_off: Vec<Vec<usize>> = vec![Vec::new(); t];
    let mut as_def: Vec<Vec<usize>> = vec![Vec::new(); t];
    for i in 0..m {
        as_off[team[i]].push(i);
        as_def[opp[i]].push(i);
    }

    let wtot: f64 = w.iter().sum();
    let wmean_pts: f64 = y.iter().zip(&w).map(|(yi, wi)| yi * wi).sum::<f64>() / wtot;
    let mut off = vec![0.0f64; t];
    let mut def = vec![0.0f64; t];
    let mut home_adv = 0.05f64;
    let mut intercept = wmean_pts.max(1.0).ln();

    let eta = |off: &[f64], def: &[f64], ha: f64, ic: f64, i: usize| {
        (ic + off[team[i]] - def[opp[i]] + ha * home[i]).clamp(-2.0, 5.0)
    };

    for _ in 0..epochs {
        // intercept
        let (mut g, mut h) = (0.0, 0.0);
        for i in 0..m {
            let mu = eta(&off, &def, home_adv, intercept, i).exp();
            g += w[i] * (y[i] - mu);
            h += w[i] * mu;
        }
        if h > 0.0 { intercept += g / h; }
        // home advantage
        let (mut g, mut h) = (0.0, 0.0);
        for i in 0..m {
            if home[i] > 0.0 {
                let mu = eta(&off, &def, home_adv, intercept, i).exp();
                g += w[i] * (y[i] - mu);
                h += w[i] * mu;
            }
        }
        if h > 0.0 { home_adv += g / h; }
        // offense per team
        for team_i in 0..t {
            let (mut g, mut h) = (-ridge * off[team_i], ridge);
            for &i in &as_off[team_i] {
                let mu = eta(&off, &def, home_adv, intercept, i).exp();
                g += w[i] * (y[i] - mu);
                h += w[i] * mu;
            }
            if h > 0.0 { off[team_i] += g / h; }
        }
        // defense per team (opponent effect, enters with a minus sign)
        for team_i in 0..t {
            let (mut g, mut h) = (-ridge * def[team_i], ridge);
            for &i in &as_def[team_i] {
                let mu = eta(&off, &def, home_adv, intercept, i).exp();
                g += w[i] * (mu - y[i]); // d eta / d def = -1
                h += w[i] * mu;
            }
            if h > 0.0 { def[team_i] += g / h; }
        }
        // identifiability: center off and def
        let moff: f64 = off.iter().sum::<f64>() / t as f64;
        let mdef: f64 = def.iter().sum::<f64>() / t as f64;
        for v in &mut off { *v -= moff; }
        for v in &mut def { *v -= mdef; }
        intercept += moff - mdef; // keep predictions unchanged (off-def centered)
    }

    // dispersion from weighted residuals: var = mu + mu^2/r  =>  r = C/(A-B)
    let (mut a, mut b, mut c) = (0.0, 0.0, 0.0);
    for i in 0..m {
        let mu = eta(&off, &def, home_adv, intercept, i).exp();
        a += w[i] * (y[i] - mu).powi(2);
        b += w[i] * mu;
        c += w[i] * mu * mu;
    }
    let dispersion = if a > b { (c / (a - b)).clamp(1.0, 1e6) } else { 1e6 };

    Some(Fit {
        off: names.iter().cloned().zip(off).collect(),
        def: names.iter().cloned().zip(def).collect(),
        home_adv,
        intercept,
        dispersion,
        n_games: completed.len(),
    })
}

async fn store_games(db: &tokio_postgres::Client, games: &[Game]) -> Result<(), String> {
    // batched UNNEST upsert
    let (mut gid, mut season, mut week, mut gtype, mut date, mut home, mut away, mut hs, mut asc, mut sl, mut tl) =
        (vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
    for g in games {
        gid.push(g.game_id.as_str());
        season.push(g.season);
        week.push(g.week.as_str());
        gtype.push(g.game_type.as_str());
        date.push(g.date);
        home.push(g.home.as_str());
        away.push(g.away.as_str());
        hs.push(g.home_score);
        asc.push(g.away_score);
        sl.push(g.spread_line);
        tl.push(g.total_line);
    }
    let status: Vec<&str> = games.iter().map(|g| if g.home_score.is_some() { "final" } else { "scheduled" }).collect();
    db.execute(
        "INSERT INTO sports_games (sport, game_id, season, week, game_type, game_date, home_team, away_team, home_score, away_score, spread_line, total_line, status) \
         SELECT 'nfl', * FROM UNNEST($1::text[],$2::int[],$3::text[],$4::text[],$5::date[],$6::text[],$7::text[],$8::int[],$9::int[],$10::float8[],$11::float8[],$12::text[]) \
         ON CONFLICT (sport, game_id) DO UPDATE SET season=EXCLUDED.season, week=EXCLUDED.week, game_type=EXCLUDED.game_type, \
           game_date=EXCLUDED.game_date, home_team=EXCLUDED.home_team, away_team=EXCLUDED.away_team, home_score=EXCLUDED.home_score, \
           away_score=EXCLUDED.away_score, spread_line=EXCLUDED.spread_line, total_line=EXCLUDED.total_line, status=EXCLUDED.status, updated_at=now()",
        &[&gid, &season, &week, &gtype, &date, &home, &away, &hs, &asc, &sl, &tl, &status],
    ).await.map(|_| ()).map_err(|e| format!("store games: {e}"))
}

async fn store_ratings(db: &tokio_postgres::Client, fit: &Fit) -> Result<(), String> {
    db.execute(
        "INSERT INTO models (id, name, sport, version, type, status) \
         VALUES ($1, 'NFL team points (log-linear Poisson, recency-weighted)', 'nfl', '1', 'poisson', 'active') \
         ON CONFLICT (id) DO NOTHING",
        &[&TEAM_MODEL],
    ).await.map_err(|e| format!("register team model: {e}"))?;
    db.execute("DELETE FROM team_components WHERE model_id=$1", &[&TEAM_MODEL])
        .await.map_err(|e| format!("clear ratings: {e}"))?;
    for (team, &r) in &fit.off {
        db.execute(
            "INSERT INTO team_components (sport, team, component, rating, model_id, as_of) VALUES ('nfl',$1,'points_off',$2,$3,now())",
            &[team, &r, &TEAM_MODEL],
        ).await.map_err(|e| format!("insert off {team}: {e}"))?;
    }
    for (team, &r) in &fit.def {
        db.execute(
            "INSERT INTO team_components (sport, team, component, rating, model_id, as_of) VALUES ('nfl',$1,'points_def',$2,$3,now())",
            &[team, &r, &TEAM_MODEL],
        ).await.map_err(|e| format!("insert def {team}: {e}"))?;
    }
    for (k, v) in [("home_adv", fit.home_adv), ("intercept", fit.intercept), ("dispersion", fit.dispersion)] {
        db.execute(
            "INSERT INTO team_model_params (model_id, key, value) VALUES ($1,$2,$3) \
             ON CONFLICT (model_id, key) DO UPDATE SET value=EXCLUDED.value, as_of=now()",
            &[&TEAM_MODEL, &k, &v],
        ).await.map_err(|e| format!("param {k}: {e}"))?;
    }
    Ok(())
}

// --------------------------------------------------------------------------
// Read + simulate
// --------------------------------------------------------------------------

pub async fn list_ratings(state: web::Data<State>) -> impl Responder {
    let sql = "SELECT o.team, o.rating AS off, d.rating AS def FROM team_components o \
               JOIN team_components d ON d.sport=o.sport AND d.team=o.team AND d.component='points_def' AND d.model_id=o.model_id \
               WHERE o.sport='nfl' AND o.component='points_off' AND o.model_id=$1 ORDER BY (o.rating+d.rating) DESC";
    match state.db.query(sql, &[&TEAM_MODEL]).await {
        Ok(rows) => {
            let out: Vec<Value> = rows.iter().map(|r| {
                let (off, def): (f64, f64) = (r.get("off"), r.get("def"));
                json!({ "team": r.get::<_, String>("team"), "off": off, "def": def, "net": off + def })
            }).collect();
            HttpResponse::Ok().json(json!({ "model_id": TEAM_MODEL, "count": out.len(), "teams": out }))
        }
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct GamesQuery {
    season: Option<i32>,
    week: Option<String>,
    status: Option<String>,
    limit: Option<i64>,
}
pub async fn list_games(state: web::Data<State>, q: web::Query<GamesQuery>) -> impl Responder {
    let limit = q.limit.unwrap_or(400).clamp(1, 2000);
    let sql = "SELECT game_id, season, week, game_type, game_date, home_team, away_team, home_score, away_score, spread_line, total_line, status \
               FROM sports_games WHERE sport='nfl' AND ($1::int IS NULL OR season=$1) AND ($2::text IS NULL OR week=$2) \
               AND ($3::text IS NULL OR status=$3) ORDER BY game_date DESC NULLS LAST, game_id LIMIT $4";
    match state.db.query(sql, &[&q.season, &q.week, &q.status, &limit]).await {
        Ok(rows) => {
            let out: Vec<Value> = rows.iter().map(|r| json!({
                "game_id": r.get::<_, String>("game_id"), "season": r.get::<_, Option<i32>>("season"),
                "week": r.get::<_, Option<String>>("week"), "game_type": r.get::<_, Option<String>>("game_type"),
                "date": r.get::<_, Option<NaiveDate>>("game_date").map(|d| d.to_string()),
                "home": r.get::<_, Option<String>>("home_team"), "away": r.get::<_, Option<String>>("away_team"),
                "home_score": r.get::<_, Option<i32>>("home_score"), "away_score": r.get::<_, Option<i32>>("away_score"),
                "spread_line": r.get::<_, Option<f64>>("spread_line"), "total_line": r.get::<_, Option<f64>>("total_line"),
                "status": r.get::<_, Option<String>>("status"),
            })).collect();
            HttpResponse::Ok().json(json!({ "count": out.len(), "games": out }))
        }
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct MatchupQuery {
    home: String,
    away: String,
    n: Option<usize>,
    seed: Option<u64>,
    spread: Option<f64>, // home spread: home covers if (home-away) + spread > 0
    total: Option<f64>,
}

pub async fn matchup(state: web::Data<State>, q: web::Query<MatchupQuery>) -> impl Responder {
    let n = q.n.unwrap_or(100_000).clamp(1000, 1_000_000);
    // load ratings + params
    let get_rating = |team: &str, comp: &str| {
        let db = &state.db;
        let team = team.to_string();
        let comp = comp.to_string();
        async move {
            db.query_opt(
                "SELECT rating FROM team_components WHERE sport='nfl' AND team=$1 AND component=$2 AND model_id=$3",
                &[&team, &comp, &TEAM_MODEL],
            ).await.ok().flatten().map(|r| r.get::<_, f64>("rating"))
        }
    };
    let (off_h, def_h, off_a, def_a) = (
        get_rating(&q.home, "points_off").await, get_rating(&q.home, "points_def").await,
        get_rating(&q.away, "points_off").await, get_rating(&q.away, "points_def").await,
    );
    let (Some(off_h), Some(def_h), Some(off_a), Some(def_a)) = (off_h, def_h, off_a, def_a) else {
        return bad_request("unknown team(s); run /ingest/nfl/teams and use team codes from /teams/nfl");
    };
    let param = |k: &str| {
        let db = &state.db;
        let k = k.to_string();
        async move {
            db.query_opt("SELECT value FROM team_model_params WHERE model_id=$1 AND key=$2", &[&TEAM_MODEL, &k])
                .await.ok().flatten().map(|r| r.get::<_, f64>("value"))
        }
    };
    let home_adv = param("home_adv").await.unwrap_or(0.0);
    let intercept = param("intercept").await.unwrap_or(3.0);
    let dispersion = param("dispersion").await.unwrap_or(20.0);

    let mu_home = (intercept + off_h - def_a + home_adv).clamp(-2.0, 5.0).exp();
    let mu_away = (intercept + off_a - def_h).clamp(-2.0, 5.0).exp();
    let seed = q.seed.unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1)
    });
    let (spread, total) = (q.spread, q.total);

    let out = web::block(move || {
        let mut rng = Rng(seed);
        let (mut hw, mut aw, mut tie, mut cover, mut over) = (0u64, 0u64, 0u64, 0u64, 0u64);
        let mut margins = Vec::with_capacity(n);
        let mut totals = Vec::with_capacity(n);
        let (mut sh, mut sa) = (0.0f64, 0.0f64);
        for _ in 0..n {
            let hp = rng.negbin(mu_home, dispersion);
            let ap = rng.negbin(mu_away, dispersion);
            sh += hp; sa += ap;
            if hp > ap { hw += 1; } else if ap > hp { aw += 1; } else { tie += 1; }
            let margin = hp - ap;
            let tot = hp + ap;
            if let Some(s) = spread { if margin + s > 0.0 { cover += 1; } }
            if let Some(t) = total { if tot > t { over += 1; } }
            margins.push(margin);
            totals.push(tot);
        }
        margins.sort_by(|a, b| a.total_cmp(b));
        totals.sort_by(|a, b| a.total_cmp(b));
        let q = |v: &[f64], p: f64| v[((p * (v.len() - 1) as f64) as usize).min(v.len() - 1)];
        let nf = n as f64;
        json!({
            "proj_home_points": (sh/nf*100.0).round()/100.0,
            "proj_away_points": (sa/nf*100.0).round()/100.0,
            "home_win_prob": hw as f64/nf, "away_win_prob": aw as f64/nf, "tie_prob": tie as f64/nf,
            "proj_spread": -((sh - sa)/nf*10.0).round()/10.0, // home line: negative = home favored
            "median_margin": q(&margins,0.5), "margin_p10": q(&margins,0.1), "margin_p90": q(&margins,0.9),
            "proj_total": ((sh+sa)/nf*10.0).round()/10.0,
            "median_total": q(&totals,0.5), "total_p10": q(&totals,0.1), "total_p90": q(&totals,0.9),
            "cover_prob": spread.map(|_| cover as f64/nf),
            "over_prob": total.map(|_| over as f64/nf),
            "margin_hist": hist(&margins, 40),
            "total_hist": hist(&totals, 40),
        })
    }).await;

    match out {
        Ok(mut v) => {
            v["home"] = json!(q.home); v["away"] = json!(q.away);
            v["mu_home"] = json!((mu_home*100.0).round()/100.0);
            v["mu_away"] = json!((mu_away*100.0).round()/100.0);
            v["n"] = json!(n);
            HttpResponse::Ok().json(v)
        }
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

// --------------------------------------------------------------------------
// Walk-forward backtest (out-of-sample): for each test season, fit the team
// model on prior seasons ONLY, predict that season's games, and score the
// predictions against actual results and the stored closing lines. No leakage.
// --------------------------------------------------------------------------

fn erf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.3275911 * x.abs());
    let y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * (-x * x).exp();
    if x >= 0.0 { y } else { -y }
}
fn normal_cdf(z: f64) -> f64 { 0.5 * (1.0 + erf(z / std::f64::consts::SQRT_2)) }

#[derive(Deserialize)]
pub struct BacktestQuery {
    #[serde(default = "d_bt_from")] from: i32,
    #[serde(default = "d_bt_to")] to: i32,
    #[serde(default = "d_halflife")] halflife_seasons: f64,
    #[serde(default = "d_epochs")] epochs: usize,
    #[serde(default = "d_ridge")] ridge: f64,
}
fn d_bt_from() -> i32 { 2019 }
fn d_bt_to() -> i32 { 2024 }

pub async fn backtest(state: web::Data<State>, q: web::Query<BacktestQuery>) -> impl Responder {
    let q = q.into_inner();
    let rows = match state.db.query(
        "SELECT season, week, game_type, home_team, away_team, home_score, away_score, spread_line, total_line \
         FROM sports_games WHERE sport='nfl' AND game_type='REG' AND home_score IS NOT NULL AND away_score IS NOT NULL",
        &[],
    ).await { Ok(r) => r, Err(e) => return internal(e) };
    let all: Vec<Game> = rows.iter().map(|r| Game {
        game_id: String::new(), season: r.get::<_, Option<i32>>("season").unwrap_or(0), week: String::new(),
        game_type: "REG".into(), date: None,
        home: r.get("home_team"), away: r.get("away_team"),
        home_score: r.get("home_score"), away_score: r.get("away_score"),
        spread_line: r.get("spread_line"), total_line: r.get("total_line"),
    }).collect();
    if all.len() < 500 { return internal("not enough stored NFL games; run /ingest/nfl/teams first"); }

    // aggregate accumulators
    let (mut n, mut su_correct, mut brier) = (0u32, 0u32, 0.0f64);
    let (mut ats_w, mut ats_l) = (0u32, 0u32);
    let (mut ev_w, mut ev_l) = (0u32, 0u32);
    let (mut tot_w, mut tot_l) = (0u32, 0u32);
    let mut cal = vec![(0.0f64, 0u32, 0u32); 10]; // (sum_pred, n, home_wins) per decile
    let mut per_season: Vec<Value> = Vec::new();
    const IMPLIED_110: f64 = 0.5238; // -110 breakeven

    for s in q.from..=q.to {
        let train: Vec<Game> = all.iter().filter(|g| g.season < s).map(clone_game).collect();
        let test: Vec<&Game> = all.iter().filter(|g| g.season == s).collect();
        if train.len() < 300 || test.is_empty() { continue; }
        let Some(fit) = fit_ratings(&train, q.halflife_seasons, q.epochs, q.ridge) else { continue };
        let (mut sn, mut ssu, mut sats_w, mut sats_l) = (0u32, 0u32, 0u32, 0u32);
        for g in test {
            let (Some(oh), Some(dh), Some(oa), Some(da)) =
                (fit.off.get(&g.home), fit.def.get(&g.home), fit.off.get(&g.away), fit.def.get(&g.away)) else { continue };
            let mu_h = (fit.intercept + oh - da + fit.home_adv).clamp(-2.0, 5.0).exp();
            let mu_a = (fit.intercept + oa - dh).clamp(-2.0, 5.0).exp();
            let var = |mu: f64| mu + mu * mu / fit.dispersion;
            let msd = (var(mu_h) + var(mu_a)).sqrt().max(1e-6);
            let mmean = mu_h - mu_a;
            let p_home = normal_cdf(mmean / msd);
            let (hs, a_s) = (g.home_score.unwrap() as f64, g.away_score.unwrap() as f64);
            let actual_margin = hs - a_s;
            let home_won = if actual_margin > 0.0 { 1.0 } else { 0.0 };
            // straight up + Brier + calibration
            n += 1; sn += 1;
            if (p_home >= 0.5) as i32 as f64 == home_won { su_correct += 1; ssu += 1; }
            brier += (p_home - home_won).powi(2);
            let b = ((p_home * 10.0) as usize).min(9);
            cal[b].0 += p_home; cal[b].1 += 1; cal[b].2 += home_won as u32;
            // ATS vs the closing line
            if let Some(line) = g.spread_line {
                // nflverse spread_line: home line (negative = home favored). Home covers if margin + (-line)>0 i.e. margin > line.
                let cover_home = actual_margin > line; // pushes (==) excluded below
                if (actual_margin - line).abs() > 1e-9 {
                    let p_cover_home = normal_cdf((mmean - line) / msd);
                    let pick_home = p_cover_home >= 0.5;
                    let correct = pick_home == cover_home;
                    if correct { ats_w += 1; sats_w += 1; } else { ats_l += 1; sats_l += 1; }
                    // +EV subset (edge over -110 breakeven)
                    let edge_side_p = if pick_home { p_cover_home } else { 1.0 - p_cover_home };
                    if edge_side_p > IMPLIED_110 {
                        if correct { ev_w += 1; } else { ev_l += 1; }
                    }
                }
            }
            // totals vs the closing line
            if let Some(tl) = g.total_line {
                let actual_total = hs + a_s;
                if (actual_total - tl).abs() > 1e-9 {
                    let p_over = normal_cdf((mu_h + mu_a - tl) / msd);
                    let pick_over = p_over >= 0.5;
                    let went_over = actual_total > tl;
                    if pick_over == went_over { tot_w += 1; } else { tot_l += 1; }
                }
            }
        }
        let ats_tot = sats_w + sats_l;
        per_season.push(json!({ "season": s, "games": sn, "train_games": train.len(),
            "su_acc": round3(ssu as f64 / sn.max(1) as f64),
            "ats_record": format!("{sats_w}-{sats_l}"),
            "ats_pct": if ats_tot > 0 { round3(sats_w as f64 / ats_tot as f64) } else { 0.0 } }));
    }
    if n == 0 { return internal("no testable games in range"); }
    let calib: Vec<Value> = cal.iter().enumerate().filter(|(_, c)| c.1 > 0).map(|(i, c)| json!({
        "bucket": format!("{}0-{}0%", i, i + 1), "n": c.1,
        "predicted": round3(c.0 / c.1 as f64), "actual": round3(c.2 as f64 / c.1 as f64),
    })).collect();
    let ats_tot = ats_w + ats_l;
    let ev_tot = ev_w + ev_l;
    let tot_tot = tot_w + tot_l;
    HttpResponse::Ok().json(json!({
        "model": "nfl-team-points", "method": "walk-forward (train on prior seasons only)",
        "test_seasons": format!("{}-{}", q.from, q.to), "games": n,
        "straight_up_accuracy": round3(su_correct as f64 / n as f64),
        "brier_score": round3(brier / n as f64),
        "ats_vs_closing_line": { "record": format!("{ats_w}-{ats_l}"), "pct": if ats_tot>0 {round3(ats_w as f64/ats_tot as f64)} else {0.0} },
        "plus_ev_subset": { "record": format!("{ev_w}-{ev_l}"), "pct": if ev_tot>0 {round3(ev_w as f64/ev_tot as f64)} else {0.0}, "note": "model edge > -110 breakeven; >52.4% = beats the vig" },
        "totals_vs_closing_line": { "record": format!("{tot_w}-{tot_l}"), "pct": if tot_tot>0 {round3(tot_w as f64/tot_tot as f64)} else {0.0} },
        "calibration": calib,
        "by_season": per_season,
    }))
}

fn clone_game(g: &Game) -> Game {
    Game { game_id: g.game_id.clone(), season: g.season, week: g.week.clone(), game_type: g.game_type.clone(),
        date: g.date, home: g.home.clone(), away: g.away.clone(), home_score: g.home_score, away_score: g.away_score,
        spread_line: g.spread_line, total_line: g.total_line }
}
fn round3(x: f64) -> f64 { (x * 1000.0).round() / 1000.0 }

fn hist(sorted: &[f64], bins: usize) -> Value {
    let q = |p: f64| sorted[((p * (sorted.len() - 1) as f64) as usize).min(sorted.len() - 1)];
    let (lo, hi) = (q(0.01), q(0.99));
    let span = (hi - lo).max(1e-9);
    let w = span / bins as f64;
    let mut counts = vec![0u32; bins];
    for &x in sorted {
        if x < lo || x > hi { continue; }
        let b = (((x - lo) / w) as usize).min(bins - 1);
        counts[b] += 1;
    }
    let total: u32 = counts.iter().sum();
    let density: Vec<f64> = counts.iter().map(|&c| if total > 0 { c as f64 / total as f64 } else { 0.0 }).collect();
    json!({ "lo": lo, "hi": hi, "bin_width": w, "density": density })
}
