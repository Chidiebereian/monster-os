//! Parlay Builder: recording, grading, track record and controlled learning.
//!
//! Recording
//!   Every auto-generated ticket is stored once per distinct combination (legs
//!   + book) with its prediction-time price, probabilities, model versions and
//!   a full JSON snapshot; a database trigger makes those fields immutable.
//!   While a ticket is open its latest price is refreshed (the last one before
//!   the start is the closing comparison). Saved custom tickets are stored the
//!   same way (source = custom).
//!
//! Grading
//!   Each leg settles on the same results as AI Picks (learning::game_outcomes,
//!   picks::prop_actuals, picks::result_of): win / loss / push, void when the
//!   player did not play, void when the game is not final four days after its
//!   start (postponed). A losing leg loses the ticket; pushed and void legs
//!   drop out of the payout. Stake convention: 1 unit per ticket.
//!
//! Track record (GET /parlays/record)
//!   Probability calibration uses tickets whose legs all won or lost (no push /
//!   void, so P(all legs win) is the quantity being tested): Brier and log loss
//!   of p_joint vs the independence baseline (same marginals, no dependence)
//!   and vs the market (product of de-vigged leg prices), paired differences
//!   with standard errors clustered by slate date (tickets of one slate share
//!   legs and are not independent), reliability by probability bucket with
//!   Wilson intervals, ROI and the price move to the close.
//!
//! Learning (daily, POST /parlays/learn)
//!   The parlay model's free parameter is the dependence weight a in
//!   p = a p_dep + (1 - a) p_indep. On settled same-game tickets in time order,
//!   a is chosen on the older 70% and tested on the newest 30% against the
//!   weight in force. A candidate is promoted only if it beats the champion by
//!   more than two clustered standard errors on at least 100 test tickets;
//!   otherwise it is recorded as rejected. POST /parlays/model/rollback restores
//!   the previous champion. The marginals themselves are learned by the AI
//!   Picks loop (blend weights fitted on settled lines).

use std::collections::{BTreeMap, HashMap};

use actix_web::{web, HttpResponse, Responder};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::parlay::{self, GenQ, Slate, Ticket};
use crate::picks;
use crate::State;

fn dberr(e: &tokio_postgres::Error) -> String { e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string()) }
fn internal(m: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(parlay): {m}");
    HttpResponse::InternalServerError().json(json!({ "error": m.to_string() }))
}
fn r4(x: f64) -> f64 { (x * 10000.0).round() / 10000.0 }

/// Auto boards stored each run: every category x leg count, best by the conservative EV.
pub const AUTO_CATEGORIES: [&str; 6] = ["moneyline", "spread", "total", "team_total", "props", "mixed"];
pub const AUTO_LEGS: [usize; 3] = [2, 3, 4];
const AUTO_TOP: usize = 5;
const MIN_TEST: usize = 100;

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------

async fn engines(db: &tokio_postgres::Client, sport: &str) -> Value {
    db.query_opt("SELECT version FROM model_registry WHERE sport=$1 AND status='champion'", &[&sport]).await.ok().flatten()
        .map(|r| json!({ sport: r.get::<_, String>(0) })).unwrap_or(json!({}))
}

pub async fn record(db: &tokio_postgres::Client, slate: &Slate, t: &Ticket, source: &str, category: &str, objective: &str, saved: bool, note: Option<&str>) -> Result<i64, String> {
    let snap = parlay::ticket_json(t, slate);
    let legs: Vec<Value> = snap["legs"].as_array().cloned().unwrap_or_default().into_iter().map(|l| json!({
        "id": l["id"], "game": l["game"], "game_label": l["game_label"], "market": l["market"], "subject": l["subject"], "subject_name": l["subject_name"],
        "side": l["side"], "line": l["line"], "label": l["label"], "price": l["price"], "decimal": l["decimal"], "p_win": l["p_win"], "p_blend": l["p_blend"],
        "p_market": l["p_market"], "p_sim": l["p_sim"], "push": l["push"], "void": l["void"], "starts_at": l["starts_at"] })).collect();
    let key = snap["key"].as_str().unwrap_or_default().to_string();
    let starts: Option<DateTime<Utc>> = snap["starts_at"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc));
    let eng = engines(db, &slate.sport).await;
    let sql = "INSERT INTO parlay_tickets (ticket_key, sport, source, category, objective, n_legs, same_game, slate_date, starts_at, book, price_decimal, price_verified, \
        p_joint, p_dep, p_indep, p_raw, p_market, p_market_indep, ev, ev_cons, mc_se, lift, dep_weight, model_version, engines, legs, snapshot, saved, note, \
        latest_price_decimal, latest_p_joint, latest_at) \
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,$27,$28,$29,$11,$13,now())";
    let conflict = if source == "auto" {
        " ON CONFLICT (ticket_key) WHERE source = 'auto' DO UPDATE SET latest_price_decimal = EXCLUDED.price_decimal, latest_p_joint = EXCLUDED.p_joint, latest_at = now() \
          WHERE parlay_tickets.status = 'open' AND (parlay_tickets.starts_at IS NULL OR parlay_tickets.starts_at > now())"
    } else { "" };
    let n_legs = t.legs.len() as i32;
    let row = db.query_opt(&format!("{sql}{conflict} RETURNING id"), &[&key, &slate.sport, &source, &category, &objective, &n_legs, &t.same_game, &slate.date, &starts, &t.book,
        &t.price_decimal, &t.price_verified, &t.p_joint, &t.p_dep, &t.p_indep, &t.p_raw, &t.p_market, &t.p_market_indep, &t.ev, &t.ev_cons, &t.mc_se, &t.lift(),
        &slate.dep_weight, &slate.model_version, &Json(&eng), &Json(&legs), &Json(&snap), &saved, &note]).await.map_err(|e| dberr(&e))?;
    Ok(row.map(|r| r.get(0)).unwrap_or(0))
}

/// Refresh the latest price and probability of open tickets on this slate
/// (their last value before the start is the closing comparison).
async fn refresh_open(db: &tokio_postgres::Client, slate: &Slate) -> Result<u64, String> {
    let rows = db.query("SELECT id, legs, book FROM parlay_tickets WHERE sport=$1 AND slate_date=$2 AND status='open' AND starts_at > now()",
        &[&slate.sport, &slate.date]).await.map_err(|e| dberr(&e))?;
    let mut gc = HashMap::new();
    let mut n = 0;
    for r in rows {
        let legs: Json<Value> = r.get(1);
        let book: Option<String> = r.get(2);
        let ids: Vec<String> = legs.0.as_array().into_iter().flatten().filter_map(|l| l["id"].as_str().map(String::from)).collect();
        let idx: Vec<usize> = ids.iter().filter_map(|id| slate.legs.iter().position(|l| &l.id == id)).collect();
        if idx.len() != ids.len() { continue; } // a leg is no longer offered at that line
        let Ok(t) = parlay::evaluate(slate, &idx, None, book.as_deref(), &mut gc) else { continue };
        if t.book != book { continue; }
        db.execute("UPDATE parlay_tickets SET latest_price_decimal=$2, latest_p_joint=$3, latest_at=now() WHERE id=$1", &[&r.get::<_, i64>(0), &t.price_decimal, &t.p_joint])
            .await.map_err(|e| dberr(&e))?;
        n += 1;
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// Grading
// ---------------------------------------------------------------------------

pub async fn settle(state: &State, sport: &str) -> Result<Value, String> {
    let db = &state.db;
    let rows = db.query("SELECT id, legs, starts_at FROM parlay_tickets WHERE sport=$1 AND status='open' AND starts_at <= now() ORDER BY id", &[&sport]).await.map_err(|e| dberr(&e))?;
    let mut games: HashMap<String, Option<crate::learning::Outcomes>> = HashMap::new();
    let mut props: HashMap<String, Option<HashMap<String, HashMap<&'static str, f64>>>> = HashMap::new();
    let (mut settled, mut pending) = (0, 0);
    for r in rows {
        let id: i64 = r.get(0);
        let legs: Json<Value> = r.get(1);
        let mut out = Vec::new();
        let mut open = false;
        for l in legs.0.as_array().into_iter().flatten() {
            let (game, market, subject, side) = (l["game"].as_str().unwrap_or(""), l["market"].as_str().unwrap_or(""), l["subject"].as_str().unwrap_or(""), l["side"].as_str().unwrap_or(""));
            let line = l["line"].as_f64();
            let start = l["starts_at"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc));
            if !games.contains_key(game) { games.insert(game.to_string(), crate::learning::game_outcomes(db, sport, game).await?); }
            let res = match games.get(game).and_then(|x| x.as_ref()) {
                None => {
                    if start.map(|s| Utc::now() - s > Duration::days(4)).unwrap_or(false) { ("void", None, "game not final four days after its start") } else { open = true; ("open", None, "") }
                }
                Some(go) if subject == "game" => match go.game.get(crate::segments::outcome_key(market)) {
                    Some(a) => (picks::result_of(market, side, line, *a), Some(*a), ""),
                    None => ("void", None, "no result for this market"),
                },
                Some(_) => {
                    if !props.contains_key(game) { props.insert(game.to_string(), picks::prop_actuals(db, sport, game).await?); }
                    match props.get(game).and_then(|p| p.as_ref()).and_then(|p| p.get(subject)).and_then(|m| m.get(market)) {
                        Some(a) => (picks::result_of(market, side, line, *a), Some(*a), ""),
                        None => ("void", None, "player did not play"),
                    }
                }
            };
            out.push(json!({ "id": l["id"], "label": l["label"], "result": res.0, "actual": res.1, "note": res.2, "decimal": l["decimal"] }));
        }
        let lost = out.iter().any(|x| x["result"] == "loss");
        if open && !lost { pending += 1; continue; }
        let payout: f64 = if lost { 0.0 } else { out.iter().map(|x| if x["result"] == "win" { x["decimal"].as_f64().unwrap_or(1.0) } else { 1.0 }).product() };
        let result = if lost { "loss" } else if out.iter().any(|x| x["result"] == "win") { "win" } else if out.iter().all(|x| x["result"] == "void") { "void" } else { "push" };
        let status = if result == "void" { "void" } else { "settled" };
        db.execute("UPDATE parlay_tickets SET status=$2, result=$3, payout=$4, profit=$5, legs_result=$6, settled_at=now() WHERE id=$1",
            &[&id, &status, &result, &payout, &(payout - 1.0), &Json(&out)]).await.map_err(|e| dberr(&e))?;
        settled += 1;
    }
    Ok(json!({ "sport": sport, "settled": settled, "pending": pending }))
}

// ---------------------------------------------------------------------------
// Statistics (pure)
// ---------------------------------------------------------------------------

/// Mean and standard error of per-ticket values, clustered by slate (key).
pub fn clustered(vals: &[(String, f64)]) -> (f64, f64, usize) {
    let n = vals.len();
    if n == 0 { return (0.0, f64::NAN, 0); }
    let mean = vals.iter().map(|v| v.1).sum::<f64>() / n as f64;
    let mut sums: BTreeMap<&str, f64> = BTreeMap::new();
    for (k, v) in vals { *sums.entry(k.as_str()).or_default() += v - mean; }
    let g = sums.len();
    if g < 2 { return (mean, f64::NAN, g); }
    let var = sums.values().map(|s| s * s).sum::<f64>() * g as f64 / (g as f64 - 1.0) / (n as f64 * n as f64);
    (mean, var.sqrt(), g)
}
pub fn brier(p: f64, y: f64) -> f64 { (p - y).powi(2) }
pub fn logloss(p: f64, y: f64) -> f64 { let p = p.clamp(1e-6, 1.0 - 1e-6); -(y * p.ln() + (1.0 - y) * (1.0 - p).ln()) }
pub fn wilson(k: f64, n: f64) -> (f64, f64) {
    if n <= 0.0 { return (0.0, 1.0); }
    let z = 1.96;
    let p = k / n;
    let d = 1.0 + z * z / n;
    let c = (p + z * z / (2.0 * n)) / d;
    let h = z * ((p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt()) / d;
    ((c - h).max(0.0), (c + h).min(1.0))
}

#[derive(Clone)]
pub struct Row { pub day: String, pub sport: String, pub source: String, pub n_legs: i32, pub category: String, pub same_game: bool,
    pub price: f64, pub verified: bool, pub p: f64, pub p_dep: f64, pub p_indep: f64, pub p_mkt: Option<f64>, pub lift: f64, pub result: String, pub profit: f64,
    pub clean: bool, pub hit: bool, pub latest_price: Option<f64> }

fn summary(rows: &[&Row]) -> Value {
    let n = rows.len();
    let count = |r: &str| rows.iter().filter(|x| x.result == r).count();
    let roi = clustered(&rows.iter().map(|r| (r.day.clone(), r.profit)).collect::<Vec<_>>());
    let clean: Vec<&&Row> = rows.iter().filter(|r| r.clean).collect();
    let y = |r: &Row| if r.hit { 1.0 } else { 0.0 };
    let pd = |f: &dyn Fn(&Row) -> f64| clustered(&clean.iter().map(|r| (r.day.clone(), f(r))).collect::<Vec<_>>());
    let b_model = pd(&|r| brier(r.p, y(r)));
    let b_ind = pd(&|r| brier(r.p_indep, y(r)));
    let vs_ind = pd(&|r| brier(r.p, y(r)) - brier(r.p_indep, y(r)));
    let mk: Vec<&&&Row> = clean.iter().filter(|r| r.p_mkt.is_some()).collect();
    let vs_mkt = clustered(&mk.iter().map(|r| (r.day.clone(), brier(r.p, y(r)) - brier(r.p_mkt.unwrap(), y(r)))).collect::<Vec<_>>());
    let ll = pd(&|r| logloss(r.p, y(r)));
    let ll_ind = pd(&|r| logloss(r.p_indep, y(r)));
    let close: Vec<(String, f64)> = rows.iter().filter(|r| r.verified).filter_map(|r| r.latest_price.map(|c| (r.day.clone(), r.price / c - 1.0))).collect();
    let clv = clustered(&close);
    let hits = clean.iter().filter(|r| r.hit).count() as f64;
    let nan = |x: f64| if x.is_finite() { json!(r4(x)) } else { Value::Null };
    json!({
        "n": n, "wins": count("win"), "losses": count("loss"), "pushes": count("push"), "voids": count("void"),
        "roi": r4(roi.0), "roi_se": nan(roi.1), "units": r4(rows.iter().map(|r| r.profit).sum::<f64>()), "slates": roi.2,
        "calibration_n": clean.len(), "hit_rate": if clean.is_empty() { Value::Null } else { json!(r4(hits / clean.len() as f64)) },
        "mean_p": if clean.is_empty() { Value::Null } else { json!(r4(clean.iter().map(|r| r.p).sum::<f64>() / clean.len() as f64)) },
        "brier": r4(b_model.0), "brier_indep": r4(b_ind.0), "brier_vs_indep": r4(vs_ind.0), "brier_vs_indep_se": nan(vs_ind.1),
        "brier_vs_market": if mk.is_empty() { Value::Null } else { json!(r4(vs_mkt.0)) }, "brier_vs_market_se": nan(vs_mkt.1), "market_n": mk.len(),
        "logloss": r4(ll.0), "logloss_indep": r4(ll_ind.0),
        "close_n": close.len(), "price_vs_close": if close.is_empty() { Value::Null } else { json!(r4(clv.0)) }, "price_vs_close_se": nan(clv.1),
    })
}

fn odds_bucket(d: f64) -> &'static str { if d < 2.5 { "< +150" } else if d < 4.0 { "+150 to +300" } else if d < 7.0 { "+300 to +600" } else if d < 12.0 { "+600 to +1100" } else { "+1100 or longer" } }
fn lift_bucket(l: f64) -> &'static str { if l < 0.9 { "negative (< 0.9x)" } else if l <= 1.1 { "near independent (0.9-1.1x)" } else if l <= 1.5 { "positive (1.1-1.5x)" } else { "strong positive (> 1.5x)" } }

pub fn report(rows: &[Row]) -> Value {
    let all: Vec<&Row> = rows.iter().collect();
    let group = |f: &dyn Fn(&Row) -> String| -> Value {
        let mut m: BTreeMap<String, Vec<&Row>> = BTreeMap::new();
        for r in rows { m.entry(f(r)).or_default().push(r); }
        Value::Array(m.into_iter().map(|(k, v)| { let mut s = summary(&v); s["key"] = json!(k); s }).collect())
    };
    let edges = [0.0, 0.05, 0.10, 0.20, 0.35, 0.5, 1.01];
    let calib: Vec<Value> = edges.windows(2).filter_map(|w| {
        let b: Vec<&Row> = rows.iter().filter(|r| r.clean && r.p >= w[0] && r.p < w[1]).collect();
        if b.is_empty() { return None; }
        let k = b.iter().filter(|r| r.hit).count() as f64;
        let n = b.len() as f64;
        let (lo, hi) = wilson(k, n);
        Some(json!({ "lo": w[0], "hi": w[1].min(1.0), "n": b.len(), "predicted": r4(b.iter().map(|r| r.p).sum::<f64>() / n), "observed": r4(k / n), "ci": [r4(lo), r4(hi)] }))
    }).collect();
    json!({
        "overall": summary(&all),
        "by_sport": group(&|r| r.sport.clone()), "by_source": group(&|r| r.source.clone()), "by_legs": group(&|r| format!("{} legs", r.n_legs)),
        "by_category": group(&|r| r.category.clone()), "by_odds": group(&|r| odds_bucket(r.price).to_string()), "by_dependence": group(&|r| lift_bucket(r.lift).to_string()),
        "by_scope": group(&|r| if r.same_game { "same-game".into() } else { "cross-game".into() }),
        "calibration": calib,
    })
}

async fn settled_rows(db: &tokio_postgres::Client, sport: Option<&str>, days: i32, source: Option<&str>) -> Result<Vec<Row>, String> {
    let rows = db.query("SELECT slate_date::text, created_at, sport, source, n_legs, COALESCE(category,'custom'), same_game, price_decimal, price_verified, p_joint, p_dep, p_indep, \
        p_market_indep, lift, result, COALESCE(profit,0), legs_result, latest_price_decimal FROM parlay_tickets \
        WHERE status='settled' AND ($1::text IS NULL OR sport=$1) AND ($3::text IS NULL OR source=$3) AND slate_date >= current_date - $2::int ORDER BY created_at",
        &[&sport, &days, &source]).await.map_err(|e| dberr(&e))?;
    Ok(rows.iter().map(|r| {
        let lr: Option<Json<Value>> = r.get(16);
        let res: Vec<String> = lr.map(|x| x.0.as_array().cloned().unwrap_or_default().iter().map(|l| l["result"].as_str().unwrap_or("").to_string()).collect()).unwrap_or_default();
        let clean = !res.is_empty() && res.iter().all(|x| x == "win" || x == "loss");
        Row { day: r.get(0), sport: r.get(2), source: r.get(3), n_legs: r.get(4), category: r.get(5), same_game: r.get(6), price: r.get(7), verified: r.get(8),
            p: r.get(9), p_dep: r.get(10), p_indep: r.get(11), p_mkt: r.get(12), lift: r.get(13), result: r.get::<_, Option<String>>(14).unwrap_or_default(), profit: r.get(15),
            clean, hit: clean && res.iter().all(|x| x == "win"), latest_price: r.get(17) }
    }).collect())
}

// ---------------------------------------------------------------------------
// Controlled learning: the dependence weight
// ---------------------------------------------------------------------------

pub const ALPHAS: [f64; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];

/// Fit the weight on the older 70% (time order), test it against `current` on the newest 30%.
pub fn fit_alpha(rows: &[Row], current: f64) -> Value {
    let data: Vec<&Row> = rows.iter().filter(|r| r.clean && r.same_game).collect();
    let cut = (data.len() as f64 * 0.7).floor() as usize;
    let (train, test) = data.split_at(cut);
    let y = |r: &Row| if r.hit { 1.0 } else { 0.0 };
    let p = |r: &Row, a: f64| a * r.p_dep + (1.0 - a) * r.p_indep;
    let train_brier: Vec<(f64, f64)> = ALPHAS.iter().map(|a| (*a, train.iter().map(|r| brier(p(r, *a), y(r))).sum::<f64>() / train.len().max(1) as f64)).collect();
    let best = train_brier.iter().cloned().fold((current, f64::MAX), |b, x| if x.1 < b.1 - 1e-12 { x } else { b }).0;
    let diff = clustered(&test.iter().map(|r| (r.day.clone(), brier(p(r, best), y(r)) - brier(p(r, current), y(r)))).collect::<Vec<_>>());
    let pass = test.len() >= MIN_TEST && best != current && diff.1.is_finite() && diff.0 < -2.0 * diff.1;
    let reason = if test.len() < MIN_TEST { format!("{} test tickets; {MIN_TEST} needed", test.len()) }
        else if best == current { "the weight in force is still the best on the training tickets".into() }
        else if pass { "beat the weight in force by more than 2 clustered standard errors".into() }
        else { "no significant improvement on the test tickets".into() };
    json!({ "candidate": best, "current": current, "train_n": train.len(), "test_n": test.len(), "train_brier": train_brier.iter().map(|(a, b)| json!({ "a": a, "brier": r4(*b) })).collect::<Vec<_>>(),
        "test_diff": r4(diff.0), "test_se": if diff.1.is_finite() { json!(r4(diff.1)) } else { Value::Null }, "pass": pass, "reason": reason })
}

pub async fn learn(state: &State) -> Result<Value, String> {
    let db = &state.db;
    let rows = settled_rows(db, None, 3650, None).await?;
    let (version, current) = parlay::model(db).await;
    let ev = fit_alpha(&rows, current);
    let cand = ev["candidate"].as_f64().unwrap_or(current);
    if cand == current { return Ok(json!({ "outcome": "kept", "version": version, "evidence": ev })); }
    let v = format!("{}+dep{:.2}", parlay::ENGINE, cand);
    if ev["pass"].as_bool().unwrap_or(false) {
        // policy: recommend only; an admin-approved promotion request makes it the champion.
        // A weight rejected on fewer tickets that passes now is a candidate again (never the champion row).
        db.execute("INSERT INTO parlay_models (version, dep_weight, status, parent, evidence) VALUES ($1,$2,'candidate',$3,$4) \
            ON CONFLICT (version) DO UPDATE SET evidence=EXCLUDED.evidence, \
              status = CASE WHEN parlay_models.status = 'rejected' THEN 'candidate' ELSE parlay_models.status END, \
              retired_at = CASE WHEN parlay_models.status = 'rejected' THEN NULL ELSE parlay_models.retired_at END \
            WHERE parlay_models.status <> 'champion'",
            &[&v, &cand, &version, &Json(&ev)]).await.map_err(|e| dberr(&e))?;
        let req = crate::governance::propose(db, "parlay", "parlay", "promote", &v, "autopilot").await.unwrap_or_else(|e| json!({ "error": e }));
        let filed = req.get("error").is_none();
        let action = if filed { "promotion_recommended" } else { "promotion_recommendation_failed" };
        crate::learning::audit(db, "parlay", action, Some(&v), json!({ "champion": version, "evidence": ev, "request": req })).await;
        Ok(json!({ "outcome": if filed { "recommended" } else { "recommendation_failed" }, "version": v, "evidence": ev, "request": req }))
    } else {
        db.execute("INSERT INTO parlay_models (version, dep_weight, status, parent, evidence) VALUES ($1,$2,'rejected',$3,$4) \
            ON CONFLICT (version) DO UPDATE SET evidence=EXCLUDED.evidence WHERE parlay_models.status <> 'champion'", &[&v, &cand, &version, &Json(&ev)]).await.map_err(|e| dberr(&e))?;
        Ok(json!({ "outcome": "rejected", "version": v, "evidence": ev }))
    }
}

// ---------------------------------------------------------------------------
// The automated run (autopilot every 30 minutes per sport)
// ---------------------------------------------------------------------------

pub async fn run(state: &web::Data<State>, sport: &str) -> Value {
    let db = &state.db;
    let t0 = std::time::Instant::now();
    let today = parlay::et_date(Utc::now());
    let mut out = Vec::new();
    for date in [today, today + Duration::days(1)] {
        let run_id: Option<i64> = db.query_opt("INSERT INTO parlay_runs (sport, slate_date) VALUES ($1,$2) RETURNING id", &[&sport, &date]).await.ok().flatten().map(|r| r.get(0));
        let res = run_slate(state, sport, date).await;
        let (ok, detail) = match res { Ok(v) => (true, v), Err(e) => (false, json!({ "error": e })) };
        if let Some(id) = run_id { let _ = db.execute("UPDATE parlay_runs SET finished_at=now(), ok=$2, detail=$3 WHERE id=$1", &[&id, &ok, &Json(&detail)]).await; }
        out.push(json!({ "date": date, "ok": ok, "detail": detail }));
    }
    let settled = settle(state, sport).await;
    json!({ "sport": sport, "ok": out.iter().all(|x| x["ok"] == true) && settled.is_ok(), "slates": out,
        "settle": settled.unwrap_or_else(|e| json!({ "error": e })), "secs": t0.elapsed().as_secs() })
}

async fn run_slate(state: &web::Data<State>, sport: &str, date: NaiveDate) -> Result<Value, String> {
    let (_, needs) = parlay::load(state, sport, date, 180).await?;
    if needs.is_empty() { return Ok(json!({ "games": 0, "note": "no games with lines and frozen forecasts" })); }
    let ts = std::time::Instant::now();
    let (_, errors) = parlay::ensure(state, sport, &needs, None).await;
    let sim_secs = ts.elapsed().as_secs_f64();
    let (slate, _) = parlay::load(state, sport, date, 180).await?;
    let tg = std::time::Instant::now();
    let slate = std::sync::Arc::new(slate);
    let s2 = slate.clone();
    let boards = web::block(move || {
        let mut v = Vec::new();
        for cat in AUTO_CATEGORIES {
            for k in AUTO_LEGS {
                let q = GenQ { category: Some(cat.into()), legs: Some(k), objective: Some("ev_cons".into()), limit: Some(AUTO_TOP), ..GenQ::default() };
                if let Ok(g) = parlay::generate(&s2, &q) { v.push((cat, k, g.evaluated, g.tickets)); }
            }
        }
        v
    }).await.map_err(|e| e.to_string())?;
    let gen_secs = tg.elapsed().as_secs_f64();
    let (mut stored, mut evaluated) = (0, 0);
    for (cat, _, ev, tickets) in &boards {
        evaluated += ev;
        for t in tickets { record(&state.db, &slate, t, "auto", cat, "ev_cons", false, None).await?; stored += 1; }
    }
    let refreshed = refresh_open(&state.db, &slate).await?;
    Ok(json!({ "games": slate.games.len(), "simulated": slate.games.iter().filter(|g| g.status == "simulated").count(), "legs": slate.legs.len(),
        "eligible": slate.legs.iter().filter(|l| l.eligible).count(), "boards": boards.len(), "stored": stored, "evaluated": evaluated, "refreshed": refreshed,
        "sim_secs": (sim_secs * 10.0).round() / 10.0, "generate_secs": (gen_secs * 10.0).round() / 10.0,
        "sim_errors": errors.iter().map(|(g, e)| format!("{g}: {e}")).collect::<Vec<_>>() }))
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RunQ { sport: Option<String> }

pub async fn run_http(state: web::Data<State>, q: web::Query<RunQ>) -> impl Responder {
    let sports: Vec<String> = match &q.sport { Some(s) => vec![s.to_lowercase()], None => parlay::SPORTS.iter().map(|s| s.to_string()).collect() };
    let mut out = Vec::new();
    for s in sports { out.push(run(&state, &s).await); }
    HttpResponse::Ok().json(json!({ "runs": out }))
}

#[derive(Deserialize)]
pub struct SaveBody { sport: Option<String>, date: Option<NaiveDate>, legs: Vec<String>, book: Option<String>, note: Option<String> }

/// Save a custom ticket (evaluated now, then frozen).
pub async fn save_http(state: web::Data<State>, body: web::Json<SaveBody>) -> impl Responder {
    let sport = body.sport.clone().unwrap_or_else(|| "nba".into()).to_lowercase();
    let date = body.date.unwrap_or_else(|| parlay::et_date(Utc::now()));
    let (_, needs) = match parlay::load(&state, &sport, date, 180).await { Ok(x) => x, Err(e) => return internal(e) };
    parlay::ensure(&state, &sport, &needs, Some(std::time::Duration::from_secs(25))).await;
    let (slate, _) = match parlay::load(&state, &sport, date, 180).await { Ok(x) => x, Err(e) => return internal(e) };
    let idx: Vec<usize> = body.legs.iter().filter_map(|id| slate.legs.iter().position(|l| &l.id == id)).collect();
    if idx.len() != body.legs.len() { return HttpResponse::Conflict().json(json!({ "error": "a leg is no longer offered at that line; re-evaluate the ticket before saving" })); }
    let t = match parlay::evaluate(&slate, &idx, None, body.book.as_deref(), &mut HashMap::new()) { Ok(t) => t, Err(e) => return HttpResponse::BadRequest().json(json!({ "error": e })) };
    match record(&state.db, &slate, &t, "custom", "custom", "custom", true, body.note.as_deref()).await {
        Ok(id) => HttpResponse::Ok().json(json!({ "id": id, "ticket": parlay::ticket_json(&t, &slate) })),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct ListQ { sport: Option<String>, source: Option<String>, status: Option<String>, saved: Option<bool>, date: Option<NaiveDate>, category: Option<String>, legs: Option<i32>, limit: Option<i64> }

const COLS: &str = "id, ticket_key, sport, source, category, objective, n_legs, same_game, slate_date, starts_at, book, price_decimal, price_verified, p_joint, p_indep, p_market_indep, \
    ev, ev_cons, mc_se, lift, dep_weight, model_version, legs, created_at, latest_price_decimal, latest_p_joint, latest_at, saved, note, status, result, payout, profit, legs_result, settled_at";

fn row_json(r: &tokio_postgres::Row, full: bool) -> Value {
    let legs: Json<Value> = r.get(22);
    let lr: Option<Json<Value>> = r.get(33);
    let mut v = json!({
        "id": r.get::<_, i64>(0), "key": r.get::<_, String>(1), "sport": r.get::<_, String>(2), "source": r.get::<_, String>(3), "category": r.get::<_, Option<String>>(4),
        "objective": r.get::<_, Option<String>>(5), "n_legs": r.get::<_, i32>(6), "same_game": r.get::<_, bool>(7), "date": r.get::<_, NaiveDate>(8),
        "starts_at": r.get::<_, Option<DateTime<Utc>>>(9), "book": r.get::<_, Option<String>>(10), "book_title": r.get::<_, Option<String>>(10).as_deref().map(picks::book_title),
        "price_decimal": r.get::<_, f64>(11), "price_american": parlay::american(r.get(11)), "price_verified": r.get::<_, bool>(12),
        "p_joint": r.get::<_, f64>(13), "p_indep": r.get::<_, f64>(14), "p_market_indep": r.get::<_, Option<f64>>(15), "ev": r.get::<_, f64>(16), "ev_cons": r.get::<_, f64>(17),
        "mc_se": r.get::<_, f64>(18), "lift": r.get::<_, f64>(19), "dep_weight": r.get::<_, f64>(20), "model_version": r.get::<_, String>(21), "legs": legs.0,
        "created_at": r.get::<_, DateTime<Utc>>(23), "latest_price_decimal": r.get::<_, Option<f64>>(24), "latest_p_joint": r.get::<_, Option<f64>>(25), "latest_at": r.get::<_, Option<DateTime<Utc>>>(26),
        "saved": r.get::<_, bool>(27), "note": r.get::<_, Option<String>>(28), "status": r.get::<_, String>(29), "result": r.get::<_, Option<String>>(30),
        "payout": r.get::<_, Option<f64>>(31), "profit": r.get::<_, Option<f64>>(32), "legs_result": lr.map(|x| x.0), "settled_at": r.get::<_, Option<DateTime<Utc>>>(34),
    });
    if !full { v.as_object_mut().unwrap().remove("snapshot"); }
    v
}

pub async fn list_http(state: web::Data<State>, q: web::Query<ListQ>) -> impl Responder {
    let rows = state.db.query(&format!("SELECT {COLS} FROM parlay_tickets WHERE NOT hidden AND ($1::text IS NULL OR sport=$1) AND ($2::text IS NULL OR source=$2) \
        AND ($3::text IS NULL OR status=$3) AND ($4::boolean IS NULL OR saved=$4) AND ($5::date IS NULL OR slate_date=$5) AND ($6::text IS NULL OR category=$6) \
        AND ($7::int IS NULL OR n_legs=$7) ORDER BY created_at DESC LIMIT $8"),
        &[&q.sport, &q.source, &q.status, &q.saved, &q.date, &q.category, &q.legs, &q.limit.unwrap_or(200).clamp(1, 2000)]).await;
    match rows { Ok(rows) => HttpResponse::Ok().json(json!({ "tickets": rows.iter().map(|r| row_json(r, false)).collect::<Vec<_>>() })), Err(e) => internal(dberr(&e)) }
}

pub async fn get_http(state: web::Data<State>, path: web::Path<i64>) -> impl Responder {
    match state.db.query_opt(&format!("SELECT {COLS}, snapshot FROM parlay_tickets WHERE id=$1"), &[&path.into_inner()]).await {
        Ok(Some(r)) => { let mut v = row_json(&r, true); v["snapshot"] = r.get::<_, Json<Value>>(35).0; HttpResponse::Ok().json(v) }
        Ok(None) => HttpResponse::NotFound().json(json!({ "error": "no such ticket" })),
        Err(e) => internal(dberr(&e)),
    }
}

/// Remove a saved ticket from the personal list (the record and its grading stay).
pub async fn unsave_http(state: web::Data<State>, path: web::Path<i64>) -> impl Responder {
    match state.db.execute("UPDATE parlay_tickets SET saved=false, hidden=(source='custom') WHERE id=$1", &[&path.into_inner()]).await {
        Ok(n) => HttpResponse::Ok().json(json!({ "updated": n })), Err(e) => internal(dberr(&e)),
    }
}

#[derive(Deserialize)]
pub struct RecordQ { sport: Option<String>, days: Option<i32>, source: Option<String> }

pub async fn record_http(state: web::Data<State>, q: web::Query<RecordQ>) -> impl Responder {
    let rows = match settled_rows(&state.db, q.sport.as_deref(), q.days.unwrap_or(3650), q.source.as_deref()).await { Ok(r) => r, Err(e) => return internal(e) };
    let open = state.db.query_one("SELECT count(*) FILTER (WHERE status='open'), count(*) FILTER (WHERE status='void'), min(created_at) FROM parlay_tickets \
        WHERE ($1::text IS NULL OR sport=$1) AND ($2::text IS NULL OR source=$2)", &[&q.sport, &q.source]).await;
    let (open_n, void_n, since): (i64, i64, Option<DateTime<Utc>>) = match open { Ok(r) => (r.get(0), r.get(1), r.get(2)), Err(e) => return internal(dberr(&e)) };
    let mut rep = report(&rows);
    rep["open"] = json!(open_n);
    rep["void"] = json!(void_n);
    rep["since"] = json!(since);
    rep["stake"] = json!("1 unit per ticket at the price recorded when the ticket was generated or saved");
    rep["note"] = json!("Calibration uses tickets whose legs all won or lost. Standard errors are clustered by slate date: tickets from one slate share legs. A positive ROI on a small sample is not evidence of an edge.");
    HttpResponse::Ok().json(rep)
}

pub async fn validation_http(state: web::Data<State>) -> impl Responder {
    let db = &state.db;
    let models = match db.query("SELECT version, dep_weight, status, parent, evidence, created_at, promoted_at, retired_at FROM parlay_models ORDER BY created_at DESC", &[]).await {
        Ok(r) => r, Err(e) => return internal(dberr(&e)),
    };
    let rows = match settled_rows(db, None, 3650, None).await { Ok(r) => r, Err(e) => return internal(e) };
    let (version, current) = parlay::model(db).await;
    let runs = db.query("SELECT sport, slate_date, started_at, finished_at, ok, detail FROM parlay_runs ORDER BY id DESC LIMIT 20", &[]).await.unwrap_or_default();
    HttpResponse::Ok().json(json!({
        "engine": parlay::ENGINE, "champion": version, "dep_weight": current, "preview": fit_alpha(&rows, current),
        "rule": format!("a candidate dependence weight is promoted only if, on the newest 30% of settled same-game tickets (at least {MIN_TEST}), its Brier score beats the weight in force by more than two standard errors clustered by slate date"),
        "models": models.iter().map(|r| json!({ "version": r.get::<_, String>(0), "dep_weight": r.get::<_, f64>(1), "status": r.get::<_, String>(2), "parent": r.get::<_, Option<String>>(3),
            "evidence": r.get::<_, Json<Value>>(4).0, "created_at": r.get::<_, DateTime<Utc>>(5), "promoted_at": r.get::<_, Option<DateTime<Utc>>>(6), "retired_at": r.get::<_, Option<DateTime<Utc>>>(7) })).collect::<Vec<_>>(),
        "runs": runs.iter().map(|r| json!({ "sport": r.get::<_, String>(0), "date": r.get::<_, NaiveDate>(1), "started_at": r.get::<_, DateTime<Utc>>(2),
            "finished_at": r.get::<_, Option<DateTime<Utc>>>(3), "ok": r.get::<_, Option<bool>>(4), "detail": r.get::<_, Json<Value>>(5).0 })).collect::<Vec<_>>(),
    }))
}

pub async fn learn_http(state: web::Data<State>) -> impl Responder {
    match learn(&state).await { Ok(v) => HttpResponse::Ok().json(v), Err(e) => internal(e) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(day: &str, p_dep: f64, p_indep: f64, hit: bool, _i: i64) -> Row {
        Row { day: day.into(), sport: "nba".into(), source: "auto".into(), n_legs: 2, category: "props".into(), same_game: true,
            price: 3.0, verified: false, p: p_dep, p_dep, p_indep, p_mkt: Some(p_indep), lift: p_dep / p_indep, result: if hit { "win".into() } else { "loss".into() },
            profit: if hit { 2.0 } else { -1.0 }, clean: true, hit, latest_price: None }
    }

    #[test]
    fn clustered_se_counts_slates_not_tickets() {
        // two slates, every ticket on a slate identical: the effective sample is two
        let v: Vec<(String, f64)> = (0..100).map(|i| (if i < 50 { "a" } else { "b" }.to_string(), if i < 50 { 1.0 } else { 0.0 })).collect();
        let (m, se, g) = clustered(&v);
        assert_eq!((m, g), (0.5, 2));
        assert!(se > 0.4, "clustered SE reflects two slates: {se}");
        let iid: Vec<(String, f64)> = (0..100).map(|i| (format!("{i}"), if i < 50 { 1.0 } else { 0.0 })).collect();
        assert!(clustered(&iid).1 < 0.06);
    }

    #[test]
    fn scoring_rules_and_wilson() {
        assert_eq!(brier(0.25, 1.0), 0.5625);
        assert!((logloss(0.5, 1.0) - 2f64.ln()).abs() < 1e-12);
        let (lo, hi) = wilson(5.0, 10.0);
        assert!(lo < 0.5 && hi > 0.5 && lo > 0.2 && hi < 0.8);
    }

    #[test]
    fn dependence_weight_needs_significant_out_of_sample_gain() {
        // outcomes follow the dependent probability: a = 1 is right
        let mut rows = Vec::new();
        for i in 0..600 {
            let hit = (i * 7919) % 100 < 40; // 40% hit rate
            rows.push(row(&format!("d{}", i / 6), 0.40, 0.20, hit, i as i64));
        }
        let ev = fit_alpha(&rows, 0.0);
        assert_eq!(ev["candidate"], 1.0);
        assert_eq!(ev["pass"], true, "{ev}");
        // too few test tickets: never promoted
        let ev = fit_alpha(&rows[..200], 0.0);
        assert_eq!(ev["pass"], false);
        // the weight in force already best: kept
        assert_eq!(fit_alpha(&rows, 1.0)["pass"], false);
    }

    #[test]
    fn report_separates_unclean_tickets_from_calibration() {
        let mut rows = vec![row("d1", 0.3, 0.3, true, 1), row("d2", 0.3, 0.3, false, 2)];
        let mut pushed = row("d3", 0.3, 0.3, false, 3);
        pushed.clean = false;
        pushed.result = "push".into();
        pushed.profit = 0.0;
        rows.push(pushed);
        let r = report(&rows);
        assert_eq!(r["overall"]["n"], 3);
        assert_eq!(r["overall"]["calibration_n"], 2);
        assert_eq!(r["overall"]["pushes"], 1);
    }
}
