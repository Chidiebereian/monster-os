//! Market-anchored blending for AI Picks.
//!
//! An engine's raw probability is not used as is. It is moved toward an anchor
//! that is hard to beat, on the log-odds scale:
//!
//!   blend(p_model, p_anchor, w) = σ( logit(p_anchor) + w · (logit(p_model) − logit(p_anchor)) )
//!
//! w = 0 is the anchor, w = 1 the raw model. Props first move toward the
//! player's own prior frequency over the line (w_naive, fitted in full-season
//! backtests); every pick then moves toward the de-vigged market consensus
//! (w_market). w_market is fitted on real closing lines: from the backtests for
//! game lines (NBA / NFL closing spreads and totals) and, as they accumulate,
//! from every line this service has priced and seen settled
//! (`pick_line_evals`), which covers props and NHL game lines too.
//!
//! Weights are fitted by Brier score and judged out of sample (leave one season
//! out in backtests, five folds by game on recorded lines), so a blend only
//! "beats the market" when a weight fitted on other data does. An engine with
//! no signal beyond the anchor fits w near 0: its edges vanish and it publishes
//! nothing, which is the honest outcome.

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::picks::{result_of, Evidence};

pub const METHOD: &str = "blend-v1";
/// Recorded lines needed before their fitted w_market replaces the backtest / default one.
pub const LINES_MIN_FIT: usize = 300;
const EPS: f64 = 0.005;

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}

pub fn logit(p: f64) -> f64 { let p = p.clamp(EPS, 1.0 - EPS); (p / (1.0 - p)).ln() }
pub fn sigmoid(x: f64) -> f64 { 1.0 / (1.0 + (-x).exp()) }
pub fn blend(p_model: f64, p_anchor: f64, w: f64) -> f64 { sigmoid(logit(p_anchor) + w * (logit(p_model) - logit(p_anchor))) }

/// The weight in [0, 1] minimising the Brier score of blend(p_model, p_anchor, w) over (p_model, p_anchor, y).
pub fn fit_w(rows: &[(f64, f64, f64)]) -> f64 {
    if rows.is_empty() { return 0.0; }
    let loss = |w: f64| rows.iter().map(|(m, a, y)| (blend(*m, *a, w) - y).powi(2)).sum::<f64>();
    let mut best = (0.0, loss(0.0));
    for k in 1..=50 { let w = k as f64 / 50.0; let l = loss(w); if l < best.1 { best = (w, l); } }
    best.0
}

/// Paired out-of-sample differences Brier(blend) − Brier(anchor), each group scored
/// with a weight fitted on the other groups. Returns (group -> diffs, weight per group).
pub fn out_of_sample<G: Ord + Clone>(rows: &[(G, f64, f64, f64)]) -> BTreeMap<G, (Vec<f64>, f64)> {
    let mut groups: BTreeMap<G, Vec<(f64, f64, f64)>> = BTreeMap::new();
    for (g, m, a, y) in rows { groups.entry(g.clone()).or_default().push((*m, *a, *y)); }
    let mut out = BTreeMap::new();
    if groups.len() < 2 { return out; }
    for (g, v) in &groups {
        let train: Vec<(f64, f64, f64)> = groups.iter().filter(|(k, _)| *k != g).flat_map(|(_, x)| x.iter().copied()).collect();
        let w = fit_w(&train);
        let d = v.iter().map(|(m, a, y)| (blend(*m, *a, w) - y).powi(2) - (a - y).powi(2)).collect();
        out.insert(g.clone(), (d, w));
    }
    out
}

pub fn mean_se(d: &[f64]) -> (f64, f64) {
    let n = d.len() as f64;
    if n < 2.0 { return (d.first().copied().unwrap_or(0.0), 0.0); }
    let m = d.iter().sum::<f64>() / n;
    (m, (d.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n - 1.0) / n).sqrt())
}

/// Laplace-smoothed share of past values above the line (the engines' naive baseline).
pub fn freq_over(past: &[f64], line: f64) -> f64 {
    (past.iter().filter(|x| **x > line).count() as f64 + 1.0) / (past.len() as f64 + 2.0)
}

// ---------------------------------------------------------------------------
// Weights in force
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Weights { pub w_naive: Option<f64>, pub w_market: f64, pub market_source: &'static str, pub market_n: i32 }

/// The weights for one engine on one market, from its current-version evidence rows.
pub fn weights(rows: &[Evidence], default_w: f64) -> Weights {
    let w = |s: &str, c: &str| rows.iter().find(|r| r.source == s && r.comparison == c && r.detail["method"] == METHOD)
        .and_then(|r| r.detail["w"].as_f64().map(|w| (w, r.n)));
    let w_naive = w("backtest", "baseline").map(|x| x.0);
    let (w_market, market_source, market_n) = match (w("lines", "market"), w("backtest", "market")) {
        (Some((x, n)), _) if n as usize >= LINES_MIN_FIT => (x, "lines", n),
        (_, Some((x, n))) => (x, "backtest", n),
        (Some((x, n)), None) if n > 0 => {
            // too few recorded lines to stand alone: shrink their fit toward the default
            let k = n as f64 / LINES_MIN_FIT as f64;
            (k * x + (1.0 - k) * default_w, "lines_partial", n)
        }
        _ => (default_w, "default", 0),
    };
    Weights { w_naive, w_market, market_source, market_n }
}

/// Final P(side) from the engine's raw P(side), the naive P(side) (props) and the market's.
pub fn apply(w: &Weights, p_model: f64, p_naive: Option<f64>, p_market: f64) -> f64 {
    let p1 = match (w.w_naive, p_naive) { (Some(wn), Some(pn)) => blend(p_model, pn, wn), _ => p_model };
    blend(p1, p_market, w.w_market)
}

pub fn source_text(w: &Weights) -> String {
    match w.market_source {
        "lines" => format!("fitted on {} settled real lines", w.market_n),
        "backtest" => format!("fitted on {} games of closing lines in backtests", w.market_n),
        "lines_partial" => format!("provisional: {} settled real lines so far", w.market_n),
        _ => "default (not fitted yet)".into(),
    }
}

// ---------------------------------------------------------------------------
// Recorded lines -> evidence
// ---------------------------------------------------------------------------

/// One priced selection, oriented to over / home.
pub struct LineEval<'a> {
    pub engine: &'a str, pub engine_version: &'a str, pub sport: &'a str, pub game_id: &'a str, pub market: &'a str, pub subject: &'a str,
    pub line: f64, pub starts_at: Option<chrono::DateTime<chrono::Utc>>, pub ledger_id: i64,
    pub p_model: f64, pub p_naive: Option<f64>, pub p_market: f64, pub books: i32,
}

pub async fn record(db: &tokio_postgres::Client, e: &LineEval<'_>) -> Result<u64, String> {
    db.execute("INSERT INTO pick_line_evals (engine, engine_version, sport, game_id, market, subject, line, starts_at, ledger_id, p_model, p_naive, p_market, books, observed_at) \
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,now()) ON CONFLICT (engine, game_id, market, subject, line) DO UPDATE SET \
        engine_version=EXCLUDED.engine_version, ledger_id=EXCLUDED.ledger_id, p_model=EXCLUDED.p_model, p_naive=EXCLUDED.p_naive, p_market=EXCLUDED.p_market, \
        books=EXCLUDED.books, starts_at=EXCLUDED.starts_at, observed_at=now() WHERE pick_line_evals.settled_at IS NULL AND (pick_line_evals.starts_at IS NULL OR pick_line_evals.starts_at > now())",
        &[&e.engine, &e.engine_version, &e.sport, &e.game_id, &e.market, &e.subject, &e.line, &e.starts_at, &e.ledger_id, &e.p_model, &e.p_naive, &e.p_market, &e.books])
        .await.map_err(|x| dberr(&x))
}

/// Score recorded lines of finished games. Returns how many were settled.
pub async fn settle(db: &tokio_postgres::Client, sport: &str) -> Result<u64, String> {
    let rows = db.query("SELECT DISTINCT game_id FROM pick_line_evals WHERE sport=$1 AND settled_at IS NULL AND starts_at < now() - interval '2 hours'", &[&sport])
        .await.map_err(|e| dberr(&e))?;
    let mut n = 0;
    for r in &rows {
        let game: String = r.get(0);
        let Some(go) = crate::learning::game_outcomes(db, sport, &game).await? else { continue };
        let props = crate::picks::prop_actuals(db, sport, &game).await?;
        for e in db.query("SELECT engine, market, subject, line FROM pick_line_evals WHERE sport=$1 AND game_id=$2 AND settled_at IS NULL", &[&sport, &game]).await.map_err(|e| dberr(&e))? {
            let (engine, market, subject, line): (String, String, String, f64) = (e.get(0), e.get(1), e.get(2), e.get(3));
            let actual = if subject == "game" { go.game.get(crate::segments::outcome_key(&market)).copied() }
                else { props.as_ref().and_then(|p| p.get(&subject)).and_then(|m| m.get(market.as_str()).copied()) };
            let side = if crate::segments::home_away(&market) { "home" } else { "over" };
            let y: Option<f64> = actual.and_then(|a| match result_of(&market, side, Some(line), a) { "win" => Some(1.0), "loss" => Some(0.0), _ => None });
            n += db.execute("UPDATE pick_line_evals SET actual=$5, y=$6, settled_at=now() WHERE engine=$1 AND game_id=$2 AND market=$3 AND subject=$4 AND line=$7",
                &[&engine, &game, &market, &subject, &actual, &y, &line]).await.map_err(|e| dberr(&e))?;
        }
    }
    Ok(n)
}

fn fold(game: &str) -> u8 { (game.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619)) % 5) as u8 }

/// Fit w_market per (engine, market) on settled recorded lines of the engine's
/// current code, out of sample by game folds, and write pick_evidence rows
/// (source 'lines', comparison 'market').
pub async fn refresh_evidence(db: &tokio_postgres::Client, sport: &str) -> Result<u64, String> {
    let champs: HashMap<String, String> = db.query("SELECT sport, engine FROM model_registry WHERE status='champion' AND (sport=$1 OR sport=$1 || '_team')", &[&sport])
        .await.map_err(|e| dberr(&e))?.iter().map(|r| (r.get(0), r.get(1))).collect();
    let mut written = 0;
    for (engine, code) in &champs {
        // w_naive per market from this engine's backtests (props)
        let wn: HashMap<String, f64> = db.query("SELECT market, detail FROM pick_evidence WHERE engine=$1 AND source='backtest' AND comparison='baseline' \
            AND detail->>'engine_version'=$2 AND detail->>'method'=$3", &[engine, code, &METHOD]).await.map_err(|e| dberr(&e))?
            .iter().filter_map(|r| { let d: Json<Value> = r.get(1); d.0["w"].as_f64().map(|w| (r.get(0), w)) }).collect();
        let rows = db.query("SELECT market, game_id, p_model, p_naive, p_market, y FROM pick_line_evals WHERE engine=$1 AND engine_version=$2 AND y IS NOT NULL",
            &[engine, code]).await.map_err(|e| dberr(&e))?;
        let mut by: BTreeMap<String, Vec<(u8, f64, f64, f64)>> = BTreeMap::new();
        let mut games: BTreeMap<String, std::collections::HashSet<String>> = BTreeMap::new();
        for r in &rows {
            let (m, g, pm, pn, pk, y): (String, String, f64, Option<f64>, f64, f64) = (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5));
            let p1 = match (wn.get(&m), pn) { (Some(w), Some(pn)) => blend(pm, pn, *w), _ => pm };
            by.entry(m.clone()).or_default().push((fold(&g), p1, pk, y));
            games.entry(m).or_default().insert(g);
        }
        for (market, v) in by {
            if v.len() < 50 { continue; }
            let oos = out_of_sample(&v);
            let d: Vec<f64> = oos.values().flat_map(|(d, _)| d.iter().copied()).collect();
            let (mean, se) = mean_se(&d);
            let raw: Vec<f64> = v.iter().map(|(_, m, a, y)| (m - y).powi(2) - (a - y).powi(2)).collect();
            let (raw_mean, raw_se) = mean_se(&raw);
            let w_all = fit_w(&v.iter().map(|(_, m, a, y)| (*m, *a, *y)).collect::<Vec<_>>());
            let (better, worse) = ((mean < -2.0 * se && se > 0.0) as i32, (mean > 2.0 * se && se > 0.0) as i32);
            let detail = json!({ "engine_version": code, "method": METHOD, "w": w_all, "w_folds": oos.values().map(|x| x.1).collect::<Vec<_>>(),
                "games": games.get(&market).map(|g| g.len()).unwrap_or(0), "raw_model_metric": raw_mean, "raw_model_se": raw_se,
                "w_naive": wn.get(&market), "note": "recorded lines: model blended toward the de-vigged closing consensus, out of sample by 5 game folds" });
            written += db.execute("INSERT INTO pick_evidence (engine, market, source, comparison, n, seasons, better, worse, metric, se, detail, updated_at) \
                VALUES ($1,$2,'lines','market',$3,0,$4,$5,$6,$7,$8,now()) ON CONFLICT (engine, market, source, comparison) DO UPDATE SET n=EXCLUDED.n, \
                better=EXCLUDED.better, worse=EXCLUDED.worse, metric=EXCLUDED.metric, se=EXCLUDED.se, detail=EXCLUDED.detail, updated_at=now()",
                &[engine, &market, &(v.len() as i32), &better, &worse, &mean, &se, &detail]).await.map_err(|e| dberr(&e))?;
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_spans_anchor_to_model() {
        assert!((blend(0.7, 0.5, 0.0) - 0.5).abs() < 1e-12);
        assert!((blend(0.7, 0.5, 1.0) - 0.7).abs() < 1e-9);
        let h = blend(0.7, 0.5, 0.5);
        assert!(h > 0.5 && h < 0.7);
        // orientation-free: the under side is the complement
        assert!((blend(0.3, 0.5, 0.5) - (1.0 - h)).abs() < 1e-12);
    }

    #[test]
    fn fit_finds_no_signal_and_real_signal() {
        // model is pure noise around a well-calibrated anchor -> w ~ 0
        let mut noise = Vec::new();
        let mut s = 7u64;
        let mut u = || { s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (s >> 11) as f64 / (1u64 << 53) as f64 };
        for _ in 0..4000 {
            let a = 0.3 + 0.4 * u();
            let y = if u() < a { 1.0 } else { 0.0 };
            noise.push((0.1 + 0.8 * u(), a, y));
        }
        assert!(fit_w(&noise) < 0.1);
        // model knows the truth, anchor is a coin -> w ~ 1
        let mut signal = Vec::new();
        for _ in 0..4000 { let t = 0.2 + 0.6 * u(); let y = if u() < t { 1.0 } else { 0.0 }; signal.push((t, 0.5, y)); }
        assert!(fit_w(&signal) > 0.8);
    }

    #[test]
    fn weights_prefer_enough_lines_then_backtest_then_default() {
        let row = |s: &str, c: &str, n: i32, w: f64| Evidence { source: s.into(), comparison: c.into(), n, detail: json!({ "method": METHOD, "w": w }), ..Default::default() };
        assert_eq!(weights(&[], 0.5).market_source, "default");
        let bt = row("backtest", "market", 3000, 0.2);
        assert_eq!(weights(&[bt.clone()], 0.5).w_market, 0.2);
        assert_eq!(weights(&[bt.clone(), row("lines", "market", 400, 0.4)], 0.5).w_market, 0.4);
        assert_eq!(weights(&[bt, row("lines", "market", 100, 0.4)], 0.5).w_market, 0.2);
        let p = weights(&[row("lines", "market", 150, 0.1)], 0.5);
        assert_eq!(p.market_source, "lines_partial");
        assert!((p.w_market - 0.3).abs() < 1e-9);
        assert_eq!(weights(&[row("backtest", "baseline", 9000, 0.6)], 0.5).w_naive, Some(0.6));
    }
}
