//! Validation engine: one scorecard for every sport, engine and market.
//!
//! Beating the closing line is one test, not the only one. Each (engine,
//! market) is measured on every source the service records, with the same code
//! for NBA / NHL / NFL / MLB, player props and game markets:
//!
//!   forecasts  frozen ledger forecasts vs their outcomes and the naive baseline
//!              (prediction_forecasts): calibration and skill of the engine itself
//!   lines      every line AI Picks priced and saw settled (pick_line_evals):
//!              calibration at real lines and Brier vs the de-vigged market
//!   picks      published picks (ai_picks): ROI, CLV, drawdown, risk-adjusted return
//!   backtest   walk-forward evidence (pick_evidence) via picks::tier
//!
//! Every comparison carries a bootstrap interval clustered by game, so a market
//! with 40 settled picks cannot look "proven". Two grades come out of it, kept
//! apart on purpose:
//!
//!   model quality   is the engine reliable on this market? (track-record tier
//!                   sets the band, calibration error the step within it)
//!   opportunity     is today's price worth it? (EV at the offered price)
//!
//! and a qualification status per pick: verified_edge, watchlist,
//! high_uncertainty, no_value, insufficient_data or suspended.
//!
//!   GET /validation/scorecard?sport=&engine=&market=&kind=&refresh=

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use actix_web::{web, HttpResponse, Responder};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::State;

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}
fn r4(x: f64) -> f64 { (x * 10000.0).round() / 10000.0 }
fn r3(x: f64) -> f64 { (x * 1000.0).round() / 1000.0 }

/// Fewer settled observations than this and a market is "insufficient data".
pub const MIN_N: usize = 200;
const BINS: usize = 10;
const BOOT: usize = 400;

// ---------------------------------------------------------------------------
// Pure metrics
// ---------------------------------------------------------------------------

/// splitmix64: a fixed seed keeps intervals stable between calls.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15); let mut z = self.0; z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9); z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB); z ^ (z >> 31) }
    fn below(&mut self, n: usize) -> usize { (self.next() % n as u64) as usize }
}

fn brier(p: f64, y: f64) -> f64 { (p - y).powi(2) }
fn logloss(p: f64, y: f64) -> f64 { let p = p.clamp(1e-4, 1.0 - 1e-4); -(y * p.ln() + (1.0 - y) * (1.0 - p).ln()) }

/// Probability quality of (p, y) pairs: Brier, log loss, expected calibration
/// error and a reliability table (10 equal-width bins).
#[derive(Debug, Clone, PartialEq)]
pub struct Calibration { pub n: usize, pub brier: f64, pub logloss: f64, pub ece: f64, pub mean_p: f64, pub hit_rate: f64, pub bins: Vec<(f64, f64, usize)> }

pub fn calibration(rows: &[(f64, f64)]) -> Option<Calibration> {
    if rows.is_empty() { return None; }
    let n = rows.len();
    let mut acc = vec![(0.0, 0.0, 0usize); BINS];
    for (p, y) in rows {
        let b = ((p * BINS as f64) as usize).min(BINS - 1);
        acc[b].0 += p; acc[b].1 += y; acc[b].2 += 1;
    }
    let ece = acc.iter().filter(|b| b.2 > 0).map(|(sp, sy, k)| (*k as f64 / n as f64) * (sp / *k as f64 - sy / *k as f64).abs()).sum();
    Some(Calibration {
        n, brier: rows.iter().map(|(p, y)| brier(*p, *y)).sum::<f64>() / n as f64,
        logloss: rows.iter().map(|(p, y)| logloss(*p, *y)).sum::<f64>() / n as f64, ece,
        mean_p: rows.iter().map(|r| r.0).sum::<f64>() / n as f64, hit_rate: rows.iter().map(|r| r.1).sum::<f64>() / n as f64,
        bins: acc.into_iter().filter(|b| b.2 > 0).map(|(sp, sy, k)| (sp / k as f64, sy / k as f64, k)).collect(),
    })
}

/// Mean of a per-observation statistic with a 95% bootstrap interval that
/// resamples whole clusters (games), since lines of one game move together.
pub fn clustered_mean(values: &[(String, f64)], seed: u64) -> Option<(f64, f64, f64)> {
    if values.is_empty() { return None; }
    let mut groups: BTreeMap<&str, (f64, usize)> = BTreeMap::new();
    for (g, v) in values { let e = groups.entry(g.as_str()).or_default(); e.0 += v; e.1 += 1; }
    let g: Vec<(f64, usize)> = groups.into_values().collect();
    let mean = values.iter().map(|v| v.1).sum::<f64>() / values.len() as f64;
    if g.len() < 2 { return Some((mean, mean, mean)); }
    let mut rng = Rng(seed);
    let mut means: Vec<f64> = (0..BOOT).map(|_| {
        let (mut s, mut k) = (0.0, 0usize);
        for _ in 0..g.len() { let x = g[rng.below(g.len())]; s += x.0; k += x.1; }
        s / k.max(1) as f64
    }).collect();
    means.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some((mean, means[(BOOT as f64 * 0.025) as usize], means[(BOOT as f64 * 0.975) as usize - 1]))
}

/// Realised betting results of 1-unit picks in settlement order.
#[derive(Debug, Clone, PartialEq)]
pub struct Returns { pub n: usize, pub units: f64, pub roi: f64, pub roi_lo: f64, pub roi_hi: f64, pub hit_rate: f64, pub max_drawdown: f64,
    pub profit_factor: Option<f64>, pub sharpe: f64, pub sortino: f64, pub clv_mean: Option<f64>, pub clv_positive: Option<f64> }

/// rows: (game, profit in units for a 1-unit stake, clv) in settlement order.
pub fn returns(rows: &[(String, f64, Option<f64>)], seed: u64) -> Option<Returns> {
    if rows.is_empty() { return None; }
    let n = rows.len();
    let units: f64 = rows.iter().map(|r| r.1).sum();
    let (mut peak, mut cum, mut dd) = (0.0f64, 0.0, 0.0f64);
    for r in rows { cum += r.1; peak = peak.max(cum); dd = dd.max(peak - cum); }
    let mean = units / n as f64;
    let sd = (rows.iter().map(|r| (r.1 - mean).powi(2)).sum::<f64>() / n.max(2).saturating_sub(1) as f64).sqrt();
    let down = (rows.iter().map(|r| r.1.min(0.0).powi(2)).sum::<f64>() / n as f64).sqrt();
    let gains: f64 = rows.iter().filter(|r| r.1 > 0.0).map(|r| r.1).sum();
    let losses: f64 = -rows.iter().filter(|r| r.1 < 0.0).map(|r| r.1).sum::<f64>();
    let ci = clustered_mean(&rows.iter().map(|r| (r.0.clone(), r.1)).collect::<Vec<_>>(), seed).unwrap_or((mean, mean, mean));
    let clv: Vec<f64> = rows.iter().filter_map(|r| r.2).collect();
    Some(Returns { n, units, roi: mean, roi_lo: ci.1, roi_hi: ci.2,
        hit_rate: rows.iter().filter(|r| r.1 > 0.0).count() as f64 / n as f64, max_drawdown: dd,
        profit_factor: if losses > 0.0 { Some(gains / losses) } else { None },
        sharpe: if sd > 0.0 { mean / sd } else { 0.0 }, sortino: if down > 0.0 { mean / down } else { 0.0 },
        clv_mean: if clv.is_empty() { None } else { Some(clv.iter().sum::<f64>() / clv.len() as f64) },
        clv_positive: if clv.is_empty() { None } else { Some(clv.iter().filter(|c| **c > 0.0).count() as f64 / clv.len() as f64) } })
}

/// z-score of a feed's hit rate against the probability its own prices imply.
pub fn feed_z(n: usize, implied: f64, hit: f64) -> f64 {
    let se = (implied * (1.0 - implied) / n.max(1) as f64).sqrt();
    if se > 0.0 { (hit - implied) / se } else { 0.0 }
}

/// A feed whose bets win far more often than its prices allow is mislabelled, not
/// beatable: a real edge moves the hit rate a few points, not 8+ points at z > 4.
pub fn feed_suspect(n: usize, implied: f64, hit: f64) -> bool {
    n >= 40 && hit - implied >= 0.08 && feed_z(n, implied, hit) > 4.0
}

/// Model-quality grade: the track-record tier sets the band (proven A, beats
/// its baseline B, unproven C, losing D); calibration error at real lines sets
/// the step. Too little data of any kind: "I" (insufficient).
pub fn quality_grade(tier: &str, ece: Option<f64>, n_lines: usize, has_backtest: bool) -> &'static str {
    if tier == "unproven" && n_lines < MIN_N && !has_backtest { return "I"; }
    let step = match ece { Some(e) if n_lines >= MIN_N => if e <= 0.02 { 0 } else if e <= 0.04 { 1 } else { 2 }, _ => 1 };
    let band: [&'static str; 3] = match tier { "proven" => ["A+", "A", "A-"], "baseline" => ["B+", "B", "B-"], "unproven" => ["C+", "C", "C-"], _ => ["D", "D", "D"] };
    band[step]
}

/// What the scorecard says about publishing an engine on a market.
pub fn market_status(tier: &str, grade: &str) -> &'static str {
    match (tier, grade) {
        (_, "I") => "insufficient_data",
        ("proven", _) => "verified",
        ("baseline", _) => "model_only",
        ("losing", _) => "losing",
        _ => "no_signal",
    }
}

/// Opportunity grade from the EV of the offered price (per unit staked).
pub fn opportunity_grade(ev: f64) -> &'static str {
    match ev { e if e >= 0.10 => "A+", e if e >= 0.07 => "A", e if e >= 0.05 => "A-", e if e >= 0.04 => "B+", e if e >= 0.03 => "B",
        e if e >= 0.02 => "B-", e if e >= 0.01 => "C+", e if e > 0.0 => "C", _ => "F" }
}

/// Qualification of one pick: market status x the price now x whether the line is still live.
pub fn qualification(status: &str, ev_now: f64, suspended: bool, edge: f64) -> &'static str {
    if suspended { return "suspended"; }
    if ev_now <= 0.0 { return "no_value"; }
    match status {
        "verified" => "verified_edge",
        "insufficient_data" => "insufficient_data",
        // a large disagreement from an engine without a market track record is the model being unsure, not an edge
        "model_only" if edge > 0.12 => "high_uncertainty",
        "model_only" => "watchlist",
        _ => "high_uncertainty",
    }
}

/// Plain-English reasons for a market's status, for the page and MonsterGPT.
pub fn reasons(tier: &str, status: &str, lines: Option<&Value>, picks: Option<&Value>, backtest: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(b) = backtest {
        let (n, s, w) = (b["n"].as_i64().unwrap_or(0), b["seasons"].as_i64().unwrap_or(0), b["w"].as_f64());
        match b["comparison"].as_str() {
            Some("market") => out.push(format!("Walk-forward backtest vs closing lines: {n} games over {s} seasons, better in {} and worse in {}{}.",
                b["better"].as_i64().unwrap_or(0), b["worse"].as_i64().unwrap_or(0),
                w.map(|w| format!("; fitted market-blend weight {w} (0 = the market alone was as good)")).unwrap_or_default())),
            _ => out.push(format!("Walk-forward backtest vs the naive baseline: {n} forecasts over {s} seasons, better in {} and worse in {}.",
                b["better"].as_i64().unwrap_or(0), b["worse"].as_i64().unwrap_or(0))),
        }
    }
    if let Some(l) = lines {
        let n = l["n"].as_u64().unwrap_or(0);
        if n > 0 {
            let d = &l["brier_vs_market"];
            out.push(format!("Settled lines priced live: {n}. Brier {} vs market {} (difference {}, 95% interval {} to {}; below 0 favours the model). Calibration error {}.",
                l["brier_model"], l["brier_market"], d["mean"], d["lo"], d["hi"], l["ece"]));
        }
    }
    if let Some(p) = picks {
        if p["n"].as_u64().unwrap_or(0) > 0 {
            out.push(format!("Published picks settled: {}, {} units, ROI {} (95% interval {} to {}), mean CLV {}, max drawdown {} units.",
                p["n"], p["units"], p["roi"], p["roi_lo"], p["roi_hi"], p["clv_mean"], p["max_drawdown"]));
        }
    }
    out.push(match status {
        "verified" => "Status: verified. The engine has beaten the market on this market, so its edges are published.".into(),
        "model_only" => "Status: model only. It beats its naive baseline but has not beaten the market, so picks are graded no higher than B+ and shown as watchlist.".into(),
        "losing" => "Status: losing. It has been worse than the market, so its picks are hidden.".into(),
        "insufficient_data" => format!("Status: insufficient data. Fewer than {MIN_N} settled lines and no backtest yet."),
        _ => format!("Status: no verified signal ({tier}). The model is blended toward the market until it proves itself, so it rarely clears the minimum edge."),
    });
    out
}

// ---------------------------------------------------------------------------
// Scorecard
// ---------------------------------------------------------------------------

fn cache() -> &'static Mutex<Option<(Instant, Value)>> {
    static C: OnceLock<Mutex<Option<(Instant, Value)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}
const TTL: Duration = Duration::from_secs(900);

fn base(engine: &str) -> &str { engine.strip_suffix("_team").unwrap_or(engine) }
fn kind_of(subject: &str) -> &'static str { if subject == "game" { "game" } else { "prop" } }
fn calib_json(c: &Calibration) -> Value {
    json!({ "n": c.n, "brier": r4(c.brier), "logloss": r4(c.logloss), "ece": r4(c.ece), "mean_p": r3(c.mean_p), "hit_rate": r3(c.hit_rate),
        "reliability": c.bins.iter().map(|(p, y, k)| json!({ "predicted": r3(*p), "observed": r3(*y), "n": k })).collect::<Vec<_>>() })
}
fn ci_json(x: Option<(f64, f64, f64)>) -> Value { x.map(|(m, lo, hi)| json!({ "mean": r4(m), "lo": r4(lo), "hi": r4(hi) })).unwrap_or(Value::Null) }
fn seed(s: &str) -> u64 { s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3)) }

/// Full scorecard (cached 15 min): `markets` (engine x pick market, what AI
/// Picks and MonsterGPT read) and `forecasts` (engine version x ledger market).
pub async fn scorecard(state: &State, refresh: bool) -> Result<Value, String> {
    if !refresh {
        if let Some((t, v)) = cache().lock().unwrap().as_ref() { if t.elapsed() < TTL { return Ok(v.clone()); } }
    }
    let v = build(state).await?;
    *cache().lock().unwrap() = Some((Instant::now(), v.clone()));
    Ok(v)
}

async fn build(state: &State) -> Result<Value, String> {
    let db = &state.db;
    let champs = crate::picks::champions(db).await?;
    let ev = crate::picks::evidence(db, &champs).await?;

    // lines: model vs market at real prices, current champion version only
    let mut lines: HashMap<(String, String), (Vec<(f64, f64)>, Vec<(String, f64)>, &'static str)> = HashMap::new();
    for r in db.query("SELECT engine, engine_version, market, subject, game_id, p_model, p_market, y FROM pick_line_evals WHERE y IS NOT NULL", &[]).await.map_err(|e| dberr(&e))? {
        let (engine, version): (String, String) = (r.get(0), r.get(1));
        if champs.get(&engine).map(|c| c.1 != version).unwrap_or(false) { continue; }
        let (p, pm, y): (f64, f64, f64) = (r.get(5), r.get(6), r.get(7));
        let e = lines.entry((engine, r.get(2))).or_insert_with(|| (vec![], vec![], kind_of(&r.get::<_, String>(3))));
        e.0.push((p, y));
        e.1.push((r.get(4), brier(p, y) - brier(pm, y)));
    }
    // feed audit: does each book's price match how often its bets actually win?
    let mut feeds = Vec::new();
    let mut quarantined: Vec<(String, String, String)> = Vec::new();
    for r in db.query("SELECT sport, market, book, side, count(*)::int, avg(CASE WHEN price > 0 THEN 100.0 / (price + 100) ELSE -price::float8 / (100 - price) END)::float8,                          avg((result = 'win')::int)::float8 FROM ai_picks WHERE status = 'settled' AND result IN ('win','loss') GROUP BY 1,2,3,4 ORDER BY 1,2,3,4", &[]).await.map_err(|e| dberr(&e))? {
        let (sport, market, book, side): (String, String, String, String) = (r.get(0), r.get(1), r.get(2), r.get(3));
        let (n, implied, hit): (i32, f64, f64) = (r.get(4), r.get(5), r.get(6));
        let z = feed_z(n as usize, implied, hit);
        let suspect = feed_suspect(n as usize, implied, hit);
        if suspect {
            let reason = format!("{side} bets at {book} won {:.0}% of the time at prices implying {:.0}% (n={n}, z={z:.1}): the quotes likely belong to another line or market", hit * 100.0, implied * 100.0);
            db.execute("INSERT INTO feed_quarantine (sport, market, book, n, implied, hit_rate, z, reason) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)                 ON CONFLICT (sport, market, book) DO UPDATE SET n=EXCLUDED.n, implied=EXCLUDED.implied, hit_rate=EXCLUDED.hit_rate, z=EXCLUDED.z, reason=EXCLUDED.reason",
                &[&sport, &market, &book, &n, &implied, &hit, &z, &reason]).await.map_err(|e| dberr(&e))?;
        }
        feeds.push(json!({ "sport": sport, "market": market, "book": book, "side": side, "n": n, "implied": r3(implied), "hit_rate": r3(hit), "z": r3(z), "suspect": suspect }));
    }
    for r in db.query("SELECT sport, market, book FROM feed_quarantine", &[]).await.map_err(|e| dberr(&e))? { quarantined.push((r.get(0), r.get(1), r.get(2))); }

    // published picks: realised results (quarantined feeds left out)
    let mut excluded: HashMap<(String, String), usize> = HashMap::new();
    let mut picks: HashMap<(String, String), (Vec<(String, f64, Option<f64>)>, &'static str)> = HashMap::new();
    for r in db.query("SELECT engine, market, kind, game_id, profit, clv, sport, book FROM ai_picks WHERE status='settled' AND result IN ('win','loss','push') ORDER BY settled_at, id", &[]).await.map_err(|e| dberr(&e))? {
        let engine: String = r.get(0);
        if quarantined.contains(&(r.get(6), r.get(1), r.get(7))) { *excluded.entry((engine, r.get(1))).or_default() += 1; continue; }
        let kind: String = r.get(2);
        let e = picks.entry((engine, r.get(1))).or_insert_with(|| (vec![], if kind == "game" { "game" } else { "prop" }));
        e.0.push((r.get(3), r.get::<_, Option<f64>>(4).unwrap_or(0.0), r.get(5)));
    }

    let holds: HashMap<(String, String), (String, DateTime<Utc>)> = db.query("SELECT engine, market, reason, decided_at FROM publication_holds_active", &[]).await
        .map(|rows| rows.iter().map(|r| ((r.get(0), r.get(1)), (r.get(2), r.get(3)))).collect()).unwrap_or_default();
    let mut keys: Vec<(String, String)> = lines.keys().chain(picks.keys()).chain(ev.keys()).filter(|k| k.1 != "_none").cloned().collect();
    keys.sort(); keys.dedup();
    let mut markets = Vec::new();
    for k in keys {
        let (engine, market) = (&k.0, &k.1);
        let (tier, decided) = crate::picks::prop_tier(&ev, engine, market);
        let rows = ev.get(&k).cloned().unwrap_or_default();
        let bt = rows.iter().find(|e| e.source == "backtest" && e.comparison == "market").or_else(|| rows.iter().find(|e| e.source == "backtest"));
        let backtest = bt.map(|b| json!({ "comparison": b.comparison, "n": b.n, "seasons": b.seasons, "better": b.better, "worse": b.worse, "w": b.detail["w"].as_f64() }));
        let lj = lines.get(&k).map(|(pv, dv, _)| {
            let c = calibration(pv);
            let mut j = c.as_ref().map(calib_json).unwrap_or(json!({ "n": 0 }));
            j["brier_model"] = j["brier"].clone();
            j["brier_market"] = json!(r4(pv.iter().zip(dv.iter()).map(|((p, y), (_, d))| brier(*p, *y) - d).sum::<f64>() / pv.len().max(1) as f64));
            j["brier_vs_market"] = ci_json(clustered_mean(dv, seed(&format!("{engine}{market}"))));
            j
        });
        let n_lines = lines.get(&k).map(|l| l.0.len()).unwrap_or(0);
        let pj = picks.get(&k).and_then(|(rows, _)| returns(rows, seed(&format!("p{engine}{market}")))).map(|r| json!({
            "n": r.n, "units": r3(r.units), "roi": r3(r.roi), "roi_lo": r3(r.roi_lo), "roi_hi": r3(r.roi_hi), "hit_rate": r3(r.hit_rate),
            "max_drawdown": r3(r.max_drawdown), "profit_factor": r.profit_factor.map(r3), "sharpe": r3(r.sharpe), "sortino": r3(r.sortino),
            "clv_mean": r.clv_mean.map(r4), "clv_positive": r.clv_positive.map(r3) }));
        let ece = lj.as_ref().and_then(|l| l["ece"].as_f64());
        let grade = quality_grade(tier, ece, n_lines, backtest.is_some());
        let status = market_status(tier, grade);
        let kind = lines.get(&k).map(|l| l.2).or(picks.get(&k).map(|p| p.1)).unwrap_or(if crate::segments::is_game_market(market) { "game" } else { "prop" });
        let mut why = reasons(tier, status, lj.as_ref(), pj.as_ref(), backtest.as_ref());
        if let Some(x) = excluded.get(&k) { why.insert(why.len() - 1, format!("{x} settled picks from quarantined odds feeds are left out of these results.")); }
        markets.push(json!({
            "sport": base(engine), "engine": engine, "version": champs.get(engine).map(|c| c.0.clone()), "market": market, "kind": kind,
            "tier": tier, "tier_source": decided.map(|d| format!("{} vs {}", d.source, d.comparison)), "model_quality": grade, "status": status,
            "backtest": backtest, "lines": lj, "picks": pj,
            "excluded_picks": excluded.get(&k).copied().unwrap_or(0),
            "publication_hold": holds.get(&k).map(|(reason, at)| json!({ "reason": reason, "since": at })),
            "reasons": why,
        }));
    }

    // frozen forecasts: engine skill vs its naive baseline, per version and ledger market
    let mut fc: BTreeMap<(String, String, String), (Vec<(f64, f64)>, Vec<(String, f64)>)> = BTreeMap::new();
    for r in db.query("SELECT l.sport, l.model_version, f.market, l.game_id, f.p_model, f.p_base, f.y::float8 FROM prediction_forecasts f \
                       JOIN prediction_ledger l ON l.id = f.ledger_id WHERE f.y IS NOT NULL AND f.void_reason IS NULL", &[]).await.map_err(|e| dberr(&e))? {
        let (p, pb, y): (f64, f64, f64) = (r.get(4), r.get(5), r.get(6));
        let e = fc.entry((r.get(0), r.get(1), r.get(2))).or_default();
        e.0.push((p, y));
        e.1.push((r.get(3), brier(p, y) - brier(pb, y)));
    }
    let champ_versions: Vec<String> = champs.values().map(|c| c.0.clone()).collect();
    let forecasts: Vec<Value> = fc.into_iter().filter(|(_, v)| v.0.len() >= 30).map(|((engine, version, market), (pv, dv))| {
        let mut j = calibration(&pv).as_ref().map(calib_json).unwrap_or(Value::Null);
        j["sport"] = json!(base(&engine)); j["engine"] = json!(engine); j["version"] = json!(version); j["market"] = json!(market);
        j["champion"] = json!(champ_versions.contains(&version));
        j["brier_vs_baseline"] = ci_json(clustered_mean(&dv, seed(&format!("f{version}{market}"))));
        j
    }).collect();

    let count = |s: &str| markets.iter().filter(|m| m["status"] == s).count();
    Ok(json!({ "generated_at": Utc::now(), "min_n": MIN_N,
        "summary": { "markets": markets.len(), "verified": count("verified"), "model_only": count("model_only"), "no_signal": count("no_signal"),
                     "losing": count("losing"), "insufficient_data": count("insufficient_data") },
        "experiments": experiments(db, None, true).await.unwrap_or_default(),
        "dataset": dataset_summary(db).await.unwrap_or(Value::Null),
        "quarantined_feeds": quarantined.iter().map(|(s, m, b)| json!({ "sport": s, "market": m, "book": b })).collect::<Vec<_>>(),
        "markets": markets, "forecasts": forecasts, "feeds": feeds }))
}

/// (engine, market) -> (model quality grade, status), for grading picks.
pub async fn statuses(state: &State) -> HashMap<(String, String), (String, String)> {
    let Ok(v) = scorecard(state, false).await else { return HashMap::new() };
    let mut out: HashMap<(String, String), (String, String)> = v["markets"].as_array().into_iter().flatten().filter_map(|m| Some(((m["engine"].as_str()?.to_string(), m["market"].as_str()?.to_string()),
        (m["model_quality"].as_str()?.to_string(), m["status"].as_str()?.to_string())))).collect();
    // publication holds as ("h:<engine>", "<market>")
    if let Ok(rows) = state.db.query("SELECT engine, market FROM publication_holds_active", &[]).await {
        for r in rows { out.insert((format!("h:{}", r.get::<_, String>(0)), r.get(1)), (String::new(), "held".into())); }
    }
    // quarantined feeds as ("q:<sport>", "<market>|<book>")
    for q in v["quarantined_feeds"].as_array().into_iter().flatten() {
        if let (Some(s), Some(m), Some(b)) = (q["sport"].as_str(), q["market"].as_str(), q["book"].as_str()) {
            out.insert((format!("q:{s}"), format!("{m}|{b}")), (String::new(), "quarantined".into()));
        }
    }
    out
}

#[derive(Deserialize)]
pub struct ScoreQ { sport: Option<String>, engine: Option<String>, market: Option<String>, kind: Option<String>, status: Option<String>, #[serde(default)] refresh: bool }

pub async fn scorecard_http(state: web::Data<State>, q: web::Query<ScoreQ>) -> impl Responder {
    let mut v = match scorecard(&state, q.refresh).await { Ok(v) => v, Err(e) => return HttpResponse::InternalServerError().json(json!({ "error": e })) };
    let keep = |m: &Value| q.sport.as_deref().map(|s| m["sport"] == s).unwrap_or(true) && q.engine.as_deref().map(|s| m["engine"] == s).unwrap_or(true)
        && q.market.as_deref().map(|s| m["market"] == s).unwrap_or(true) && q.kind.as_deref().map(|s| m["kind"] == s).unwrap_or(true)
        && q.status.as_deref().map(|s| m["status"] == s).unwrap_or(true);
    if let Some(a) = v["markets"].as_array_mut() { a.retain(keep); }
    if let Some(a) = v["forecasts"].as_array_mut() {
        a.retain(|m| q.sport.as_deref().map(|s| m["sport"] == s).unwrap_or(true) && q.engine.as_deref().map(|s| m["engine"] == s).unwrap_or(true));
    }
    HttpResponse::Ok().json(v)
}

// ---------------------------------------------------------------------------
// ML research results (scripts/ml, migration 0035)
// ---------------------------------------------------------------------------

/// Experiments from ml_experiments, newest first; `latest` keeps one per name.
pub async fn experiments(db: &tokio_postgres::Client, sport: Option<&str>, latest: bool) -> Result<Vec<Value>, String> {
    let rows = db.query("SELECT id, created_at, name, sport, markets, source, target, dataset_hash, n, n_test, folds, best_model, verdict, results, gate, code_version \
        FROM ml_experiments WHERE ($1::text IS NULL OR sport=$1) ORDER BY created_at DESC, id DESC LIMIT 500", &[&sport]).await.map_err(|e| dberr(&e))?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        let name: String = r.get(2);
        if latest && !seen.insert(name.clone()) { continue; }
        let results: tokio_postgres::types::Json<Value> = r.get(13);
        let gate: tokio_postgres::types::Json<Value> = r.get(14);
        // the compact table: one line per model
        let models: Vec<Value> = results.0["results"].as_array().into_iter().flatten().map(|m| json!({
            "model": m["model"], "brier": m["brier"], "ece": m["ece"], "brier_vs_market": m["brier_vs_market"], "folds_better": m["folds_better"],
            "bets": m["betting"]["bets"], "roi": m["betting"]["roi"], "clv_mean": m["betting"]["clv_mean"] })).collect();
        out.push(json!({ "id": r.get::<_, i64>(0), "created_at": r.get::<_, DateTime<Utc>>(1), "name": name, "sport": r.get::<_, String>(3),
            "markets": r.get::<_, Vec<String>>(4), "source": r.get::<_, String>(5), "target": r.get::<_, String>(6), "dataset_hash": r.get::<_, String>(7),
            "n": r.get::<_, i32>(8), "n_test": r.get::<_, i32>(9), "folds": r.get::<_, i32>(10), "best_model": r.get::<_, Option<String>>(11),
            "verdict": r.get::<_, String>(12), "gate": gate.0, "models": models, "info": results.0["info"], "code_version": r.get::<_, String>(15) }));
    }
    Ok(out)
}

/// Size of the immutable ML dataset per sport and source.
async fn dataset_summary(db: &tokio_postgres::Client) -> Result<Value, String> {
    let rows = db.query("SELECT sport, source, count(*)::int, count(DISTINCT game_id)::int, min(COALESCE(game_date, decision_at::date))::text, max(COALESCE(game_date, decision_at::date))::text \
        FROM ml_examples GROUP BY 1,2 ORDER BY 1,2", &[]).await.map_err(|e| dberr(&e))?;
    Ok(Value::Array(rows.iter().map(|r| json!({ "sport": r.get::<_, String>(0), "source": r.get::<_, String>(1), "examples": r.get::<_, i32>(2),
        "games": r.get::<_, i32>(3), "from": r.get::<_, Option<String>>(4), "to": r.get::<_, Option<String>>(5) })).collect()))
}

#[derive(Deserialize)]
pub struct ExpQ { sport: Option<String>, #[serde(default)] all: bool }

/// GET /validation/experiments?sport=&all= — walk-forward ML experiments (latest per name unless all=true).
pub async fn experiments_http(state: web::Data<State>, q: web::Query<ExpQ>) -> impl Responder {
    match experiments(&state.db, q.sport.as_deref(), !q.all).await {
        Ok(v) => HttpResponse::Ok().json(json!({ "experiments": v, "dataset": dataset_summary(&state.db).await.unwrap_or(Value::Null) })),
        Err(e) => HttpResponse::InternalServerError().json(json!({ "error": e })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_of_a_perfect_and_an_overconfident_model() {
        let good: Vec<(f64, f64)> = (0..1000).map(|i| (0.6, if i % 10 < 6 { 1.0 } else { 0.0 })).collect();
        let c = calibration(&good).unwrap();
        assert!(c.ece < 1e-9 && (c.brier - 0.24).abs() < 1e-9, "{c:?}");
        let over: Vec<(f64, f64)> = (0..1000).map(|i| (0.8, if i % 10 < 6 { 1.0 } else { 0.0 })).collect();
        let o = calibration(&over).unwrap();
        assert!((o.ece - 0.2).abs() < 1e-9 && o.logloss > c.logloss);
        assert_eq!(o.bins.len(), 1);
    }

    #[test]
    fn bootstrap_interval_contains_the_mean_and_shrinks_with_data() {
        let small: Vec<(String, f64)> = (0..20).map(|i| (format!("g{}", i / 2), if i % 3 == 0 { 1.0 } else { -0.4 })).collect();
        let big: Vec<(String, f64)> = (0..2000).map(|i| (format!("g{}", i / 2), if i % 3 == 0 { 1.0 } else { -0.4 })).collect();
        let (m, lo, hi) = clustered_mean(&small, 7).unwrap();
        assert!(lo <= m && m <= hi);
        let (_, lo2, hi2) = clustered_mean(&big, 7).unwrap();
        assert!(hi2 - lo2 < hi - lo);
    }

    #[test]
    fn returns_track_drawdown_and_clv() {
        let rows = vec![("a".to_string(), 1.0, Some(0.02)), ("b".into(), -1.0, Some(-0.01)), ("c".into(), -1.0, None), ("d".into(), 0.9, Some(0.03))];
        let r = returns(&rows, 1).unwrap();
        assert_eq!(r.n, 4);
        assert!((r.units + 0.1).abs() < 1e-9 && (r.max_drawdown - 2.0).abs() < 1e-9);
        assert!((r.profit_factor.unwrap() - 0.95).abs() < 1e-9);
        assert!((r.clv_positive.unwrap() - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn feed_audit_flags_impossible_hit_rates_only() {
        // SGO NHL points overs: +433 average (implied ~0.19) winning 37% over 614 bets
        assert!(feed_suspect(614, 0.19, 0.373));
        // a strong but real edge: 55% at -110 (implied 0.524)
        assert!(!feed_suspect(500, 0.524, 0.55));
        // too few bets to tell
        assert!(!feed_suspect(20, 0.2, 0.5));
    }

    #[test]
    fn grades_separate_model_quality_from_opportunity() {
        assert_eq!(quality_grade("proven", Some(0.01), 500, true), "A+");
        assert_eq!(quality_grade("baseline", Some(0.05), 500, true), "B-");
        assert_eq!(quality_grade("baseline", None, 0, true), "B");
        assert_eq!(quality_grade("unproven", None, 10, false), "I");
        assert_eq!(market_status("unproven", "I"), "insufficient_data");
        assert_eq!(market_status("proven", "A"), "verified");
        assert_eq!(opportunity_grade(0.281), "A+");
        assert_eq!(opportunity_grade(-0.01), "F");
        assert_eq!(qualification("verified", 0.05, false, 0.04), "verified_edge");
        assert_eq!(qualification("model_only", 0.05, false, 0.04), "watchlist");
        assert_eq!(qualification("model_only", 0.2, false, 0.2), "high_uncertainty");
        assert_eq!(qualification("verified", -0.01, false, 0.04), "no_value");
        assert_eq!(qualification("verified", 0.05, true, 0.04), "suspended");
    }
}
