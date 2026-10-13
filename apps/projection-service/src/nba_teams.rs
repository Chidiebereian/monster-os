//! NBA team model: log-linear model on team points (mirrors the NFL/NHL team
//! models), fit from the games already stored in sports_games (loaded by the
//! Python games ingester, since stats.nba.com is reqwest-blocked here). IRLS
//! coordinate-Newton fit of the recency-weighted Poisson log-likelihood + ridge;
//! points drawn Negative-Binomial (dispersion from residuals) and a matchup is
//! simulated 100k times for win / total / spread with margin & total histograms.
//! The Mack NBA reference decomposes points into FT/2PT/3PT NB models; v1 models
//! total points, which is the documented simplification.

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Responder};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::State;

pub const TEAM_MODEL: &str = "nba-team-points-v1";

fn internal(m: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(nba_teams): {m}");
    HttpResponse::InternalServerError().json(json!({ "error": m.to_string() }))
}
fn bad_request(m: impl Into<String>) -> HttpResponse { HttpResponse::BadRequest().json(json!({ "error": m.into() })) }

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn uniform(&mut self) -> f64 { ((self.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64 }
    fn normal(&mut self) -> f64 {
        let (u1, u2) = (self.uniform(), self.uniform());
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
    fn gamma(&mut self, shape: f64, scale: f64) -> f64 {
        if shape < 1.0 { let g = self.gamma(shape + 1.0, 1.0); return scale * g * self.uniform().powf(1.0 / shape); }
        let d = shape - 1.0 / 3.0; let c = 1.0 / (9.0 * d).sqrt();
        loop {
            let x = self.normal(); let v = (1.0 + c * x).powi(3);
            if v <= 0.0 { continue; }
            if self.uniform().ln() < 0.5 * x * x + d - d * v + d * v.ln() { return scale * d * v; }
        }
    }
    fn poisson(&mut self, lambda: f64) -> f64 {
        if lambda <= 0.0 { return 0.0; }
        if lambda > 30.0 { return (lambda + lambda.sqrt() * self.normal()).round().max(0.0); }
        let l = (-lambda).exp(); let (mut k, mut p) = (0.0f64, 1.0f64);
        loop { k += 1.0; p *= self.uniform(); if p <= l { return k - 1.0; } }
    }
    fn negbin(&mut self, mean: f64, r: f64) -> f64 {
        if mean <= 0.0 { return 0.0; }
        let lambda = self.gamma(r, mean / r); self.poisson(lambda)
    }
}

struct Obs { y: f64, team: usize, opp: usize, home: f64, w: f64 }

#[derive(Deserialize)]
pub struct FitQuery {
    #[serde(default = "d_halflife")] halflife_seasons: f64,
    #[serde(default = "d_epochs")] epochs: usize,
    #[serde(default = "d_ridge")] ridge: f64,
}
fn d_halflife() -> f64 { 2.0 }
fn d_epochs() -> usize { 80 }
fn d_ridge() -> f64 { 0.5 }

/// Fit from completed NBA games already in sports_games.
pub async fn fit_teams(state: web::Data<State>, q: web::Query<FitQuery>) -> impl Responder {
    let q = q.into_inner();
    let rows = match state.db.query(
        "SELECT season, home_team, away_team, home_score, away_score FROM sports_games \
         WHERE sport='nba' AND game_type='REG' AND home_score IS NOT NULL AND away_score IS NOT NULL",
        &[],
    ).await { Ok(r) => r, Err(e) => return internal(e) };
    if rows.len() < 200 { return internal("not enough completed NBA games; load the schedule first"); }

    let mut idx: HashMap<String, usize> = HashMap::new();
    let mut games: Vec<(i32, String, String, f64, f64)> = Vec::new();
    for r in &rows {
        let season: i32 = r.get::<_, Option<i32>>("season").unwrap_or(0);
        let (h, a): (String, String) = (r.get("home_team"), r.get("away_team"));
        let (hs, as_): (i32, i32) = (r.get("home_score"), r.get("away_score"));
        for t in [&h, &a] { let n = idx.len(); idx.entry(t.clone()).or_insert(n); }
        games.push((season, h, a, hs as f64, as_ as f64));
    }
    let tn = idx.len();
    let names: Vec<String> = { let mut v = vec![String::new(); tn]; for (k, &i) in &idx { v[i] = k.clone(); } v };
    let max_season = games.iter().map(|g| g.0).max().unwrap_or(0) as f64;

    let mut obs: Vec<Obs> = Vec::new();
    for (season, h, a, hs, as_) in &games {
        let w = 0.5f64.powf((max_season - *season as f64) / q.halflife_seasons);
        let (hi, ai) = (idx[h], idx[a]);
        obs.push(Obs { y: *hs, team: hi, opp: ai, home: 1.0, w });
        obs.push(Obs { y: *as_, team: ai, opp: hi, home: 0.0, w });
    }
    let m = obs.len();
    let mut as_off: Vec<Vec<usize>> = vec![Vec::new(); tn];
    let mut as_def: Vec<Vec<usize>> = vec![Vec::new(); tn];
    for (i, o) in obs.iter().enumerate() { as_off[o.team].push(i); as_def[o.opp].push(i); }

    let wtot: f64 = obs.iter().map(|o| o.w).sum();
    let wmean: f64 = obs.iter().map(|o| o.y * o.w).sum::<f64>() / wtot;
    let mut off = vec![0.0f64; tn];
    let mut def = vec![0.0f64; tn];
    let mut home_adv = 0.02f64;
    let mut intercept = wmean.max(1.0).ln();
    let eta = |off: &[f64], def: &[f64], ha: f64, ic: f64, o: &Obs| (ic + off[o.team] - def[o.opp] + ha * o.home).clamp(-1.0, 6.0);

    for _ in 0..q.epochs {
        let (mut g, mut h) = (0.0, 0.0);
        for o in &obs { let mu = eta(&off, &def, home_adv, intercept, o).exp(); g += o.w * (o.y - mu); h += o.w * mu; }
        if h > 0.0 { intercept += g / h; }
        let (mut g, mut h) = (0.0, 0.0);
        for o in &obs { if o.home > 0.0 { let mu = eta(&off, &def, home_adv, intercept, o).exp(); g += o.w * (o.y - mu); h += o.w * mu; } }
        if h > 0.0 { home_adv += g / h; }
        for ti in 0..tn {
            let (mut g, mut h) = (-q.ridge * off[ti], q.ridge);
            for &i in &as_off[ti] { let o = &obs[i]; let mu = eta(&off, &def, home_adv, intercept, o).exp(); g += o.w * (o.y - mu); h += o.w * mu; }
            if h > 0.0 { off[ti] += g / h; }
        }
        for ti in 0..tn {
            let (mut g, mut h) = (-q.ridge * def[ti], q.ridge);
            for &i in &as_def[ti] { let o = &obs[i]; let mu = eta(&off, &def, home_adv, intercept, o).exp(); g += o.w * (mu - o.y); h += o.w * mu; }
            if h > 0.0 { def[ti] += g / h; }
        }
        let moff: f64 = off.iter().sum::<f64>() / tn as f64;
        let mdef: f64 = def.iter().sum::<f64>() / tn as f64;
        for v in &mut off { *v -= moff; }
        for v in &mut def { *v -= mdef; }
        intercept += moff - mdef;
    }
    let (mut a, mut b, mut c) = (0.0, 0.0, 0.0);
    for o in &obs { let mu = eta(&off, &def, home_adv, intercept, o).exp(); a += o.w * (o.y - mu).powi(2); b += o.w * mu; c += o.w * mu * mu; }
    let dispersion = if a > b { (c / (a - b)).clamp(1.0, 1e6) } else { 1e6 };

    if let Err(e) = store(&state.db, &names, &off, &def, home_adv, intercept, dispersion).await { return internal(e); }

    let mut net: Vec<(String, f64)> = names.iter().enumerate().map(|(i, n)| (n.clone(), off[i] + def[i])).collect();
    net.sort_by(|a, b| b.1.total_cmp(&a.1));
    HttpResponse::Ok().json(json!({
        "model_id": TEAM_MODEL, "teams": tn, "fit_on_rows": m,
        "home_advantage": home_adv, "mean_points": (intercept.exp()*100.0).round()/100.0, "dispersion": dispersion,
        "top_net": net.iter().take(6).map(|(t, r)| json!({"team": t, "net": (r*1000.0).round()/1000.0})).collect::<Vec<_>>(),
    }))
}

async fn store(db: &tokio_postgres::Client, names: &[String], off: &[f64], def: &[f64], ha: f64, ic: f64, disp: f64) -> Result<(), String> {
    db.execute("INSERT INTO models (id, name, sport, version, type, status) VALUES ($1, 'NBA team points (log-linear Poisson)', 'nba', '1', 'poisson', 'active') ON CONFLICT (id) DO NOTHING", &[&TEAM_MODEL]).await.map_err(|e| format!("register: {e}"))?;
    db.execute("DELETE FROM team_components WHERE model_id=$1", &[&TEAM_MODEL]).await.map_err(|e| format!("clear: {e}"))?;
    for (i, name) in names.iter().enumerate() {
        db.execute("INSERT INTO team_components (sport, team, component, rating, model_id, as_of) VALUES ('nba',$1,'points_off',$2,$3,now())", &[name, &off[i], &TEAM_MODEL]).await.map_err(|e| format!("off {name}: {e}"))?;
        db.execute("INSERT INTO team_components (sport, team, component, rating, model_id, as_of) VALUES ('nba',$1,'points_def',$2,$3,now())", &[name, &def[i], &TEAM_MODEL]).await.map_err(|e| format!("def {name}: {e}"))?;
    }
    for (k, v) in [("home_adv", ha), ("intercept", ic), ("dispersion", disp)] {
        db.execute("INSERT INTO team_model_params (model_id, key, value) VALUES ($1,$2,$3) ON CONFLICT (model_id, key) DO UPDATE SET value=EXCLUDED.value, as_of=now()", &[&TEAM_MODEL, &k, &v]).await.map_err(|e| format!("param {k}: {e}"))?;
    }
    Ok(())
}

pub async fn list_ratings(state: web::Data<State>) -> impl Responder {
    let sql = "SELECT o.team, o.rating AS off, d.rating AS def FROM team_components o \
               JOIN team_components d ON d.sport=o.sport AND d.team=o.team AND d.component='points_def' AND d.model_id=o.model_id \
               WHERE o.sport='nba' AND o.component='points_off' AND o.model_id=$1 ORDER BY (o.rating+d.rating) DESC";
    match state.db.query(sql, &[&TEAM_MODEL]).await {
        Ok(rows) => {
            let out: Vec<Value> = rows.iter().map(|r| { let (off, def): (f64, f64) = (r.get("off"), r.get("def")); json!({ "team": r.get::<_, String>("team"), "off": off, "def": def, "net": off + def }) }).collect();
            HttpResponse::Ok().json(json!({ "model_id": TEAM_MODEL, "count": out.len(), "teams": out }))
        }
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct GamesQuery { season: Option<i32>, status: Option<String>, limit: Option<i64> }
pub async fn list_games(state: web::Data<State>, q: web::Query<GamesQuery>) -> impl Responder {
    let limit = q.limit.unwrap_or(400).clamp(1, 2000);
    // Only real NBA teams (exclude global/exhibition placeholders like LON) by
    // requiring both sides to have a fitted rating.
    let sql = "SELECT g.game_id, g.season, g.week, g.game_type, g.game_date, g.home_team, g.away_team, g.home_score, g.away_score, g.status \
               FROM sports_games g WHERE g.sport='nba' AND ($1::int IS NULL OR g.season=$1) AND ($2::text IS NULL OR g.status=$2) \
                 AND EXISTS (SELECT 1 FROM team_components tc WHERE tc.sport='nba' AND tc.model_id=$4 AND tc.component='points_off' AND tc.team=g.home_team) \
                 AND EXISTS (SELECT 1 FROM team_components tc WHERE tc.sport='nba' AND tc.model_id=$4 AND tc.component='points_off' AND tc.team=g.away_team) \
               ORDER BY g.game_date ASC NULLS LAST, g.game_id LIMIT $3";
    match state.db.query(sql, &[&q.season, &q.status, &limit, &TEAM_MODEL]).await {
        Ok(rows) => {
            let out: Vec<Value> = rows.iter().map(|r| json!({
                "game_id": r.get::<_, String>("game_id"), "season": r.get::<_, Option<i32>>("season"),
                "week": r.get::<_, Option<String>>("week"), "game_type": r.get::<_, Option<String>>("game_type"),
                "date": r.get::<_, Option<NaiveDate>>("game_date").map(|d| d.to_string()),
                "home": r.get::<_, Option<String>>("home_team"), "away": r.get::<_, Option<String>>("away_team"),
                "home_score": r.get::<_, Option<i32>>("home_score"), "away_score": r.get::<_, Option<i32>>("away_score"),
                "status": r.get::<_, Option<String>>("status"),
            })).collect();
            HttpResponse::Ok().json(json!({ "count": out.len(), "games": out }))
        }
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct MatchupQuery { home: String, away: String, n: Option<usize>, seed: Option<u64>, spread: Option<f64>, total: Option<f64> }

pub async fn matchup(state: web::Data<State>, q: web::Query<MatchupQuery>) -> impl Responder {
    let n = q.n.unwrap_or(100_000).clamp(1000, 1_000_000);
    let rating = |team: &str, comp: &str| {
        let db = &state.db; let team = team.to_string(); let comp = comp.to_string();
        async move { db.query_opt("SELECT rating FROM team_components WHERE sport='nba' AND team=$1 AND component=$2 AND model_id=$3", &[&team, &comp, &TEAM_MODEL]).await.ok().flatten().map(|r| r.get::<_, f64>("rating")) }
    };
    let (oh, dh, oa, da) = (rating(&q.home, "points_off").await, rating(&q.home, "points_def").await, rating(&q.away, "points_off").await, rating(&q.away, "points_def").await);
    let (Some(oh), Some(dh), Some(oa), Some(da)) = (oh, dh, oa, da) else { return bad_request("unknown team(s); run /fit/nba/teams and use codes from /teams/nba"); };
    let param = |k: &str| { let db = &state.db; let k = k.to_string(); async move { db.query_opt("SELECT value FROM team_model_params WHERE model_id=$1 AND key=$2", &[&TEAM_MODEL, &k]).await.ok().flatten().map(|r| r.get::<_, f64>("value")) } };
    let home_adv = param("home_adv").await.unwrap_or(0.0);
    let intercept = param("intercept").await.unwrap_or(4.7);
    let dispersion = param("dispersion").await.unwrap_or(200.0);
    let mu_home = (intercept + oh - da + home_adv).clamp(-1.0, 6.0).exp();
    let mu_away = (intercept + oa - dh).clamp(-1.0, 6.0).exp();
    let seed = q.seed.unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1));
    let (spread, total) = (q.spread, q.total);

    let out = web::block(move || {
        let mut rng = Rng(seed);
        let (mut hw, mut aw, mut cover, mut over) = (0u64, 0u64, 0u64, 0u64);
        let (mut sh, mut sa) = (0.0, 0.0);
        let mut margins = Vec::with_capacity(n); let mut totals = Vec::with_capacity(n);
        let p_home_ot = mu_home / (mu_home + mu_away);
        for _ in 0..n {
            let mut hp = rng.negbin(mu_home, dispersion);
            let mut ap = rng.negbin(mu_away, dispersion);
            if (hp - ap).abs() < 0.5 { if rng.uniform() < p_home_ot { hp += 1.0; } else { ap += 1.0; } } // no NBA ties -> OT
            sh += hp; sa += ap;
            if hp > ap { hw += 1; } else { aw += 1; }
            let margin = hp - ap; let tot = hp + ap;
            if let Some(s) = spread { if margin + s > 0.0 { cover += 1; } }
            if let Some(t) = total { if tot > t { over += 1; } }
            margins.push(margin); totals.push(tot);
        }
        margins.sort_by(|a, b| a.total_cmp(b)); totals.sort_by(|a, b| a.total_cmp(b));
        let qf = |v: &[f64], p: f64| v[((p * (v.len() - 1) as f64) as usize).min(v.len() - 1)];
        let nf = n as f64;
        json!({
            "proj_home_points": (sh/nf*100.0).round()/100.0, "proj_away_points": (sa/nf*100.0).round()/100.0,
            "home_win_prob": hw as f64/nf, "away_win_prob": aw as f64/nf,
            "proj_spread": -((sh - sa)/nf*10.0).round()/10.0,
            "proj_total": ((sh+sa)/nf*10.0).round()/10.0, "median_total": qf(&totals,0.5), "total_p10": qf(&totals,0.1), "total_p90": qf(&totals,0.9),
            "median_margin": qf(&margins,0.5), "margin_p10": qf(&margins,0.1), "margin_p90": qf(&margins,0.9),
            "cover_prob": spread.map(|_| cover as f64/nf), "over_prob": total.map(|_| over as f64/nf),
            "margin_hist": hist(&margins, 40), "total_hist": hist(&totals, 40),
        })
    }).await;
    match out {
        Ok(mut v) => { v["home"] = json!(q.home); v["away"] = json!(q.away); v["mu_home"] = json!((mu_home*100.0).round()/100.0); v["mu_away"] = json!((mu_away*100.0).round()/100.0); v["n"] = json!(n); HttpResponse::Ok().json(v) }
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

fn hist(sorted: &[f64], bins: usize) -> Value {
    let q = |p: f64| sorted[((p * (sorted.len() - 1) as f64) as usize).min(sorted.len() - 1)];
    let (lo, hi) = (q(0.01), q(0.99));
    let span = (hi - lo).max(1e-9); let w = span / bins as f64;
    let mut counts = vec![0u32; bins];
    for &x in sorted { if x < lo || x > hi { continue; } let b = (((x - lo) / w) as usize).min(bins - 1); counts[b] += 1; }
    let total: u32 = counts.iter().sum();
    let density: Vec<f64> = counts.iter().map(|&c| if total > 0 { c as f64 / total as f64 } else { 0.0 }).collect();
    json!({ "lo": lo, "hi": hi, "bin_width": w, "density": density })
}
