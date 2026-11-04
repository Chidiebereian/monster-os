//! Model diagnostics answers from the validation scorecard (projection-service
//! /validation/scorecard). "Why isn't Monster publishing NFL spreads?" and
//! "which models are calibrated?" are answered with the engine's own numbers,
//! for every sport, engine, game market and player prop, without a language
//! model in between.

use serde_json::Value;

/// Words people use for each market key the engines price.
const ALIASES: &[(&str, &[&str])] = &[
    ("moneyline", &["moneyline", "money line", "who wins", "to win"]),
    ("spread", &["spread", "puck line", "run line", "handicap", "cover"]),
    ("total", &["total", "over/under", "o/u"]),
    ("team_total_home", &["team total"]), ("team_total_away", &["team total"]),
    ("pts", &["points"]), ("points", &["points"]), ("reb", &["rebound"]), ("ast", &["assist"]), ("assists", &["assist"]),
    ("fg3m", &["three", "3-point", "3pt", "threes"]), ("stl", &["steal"]), ("blk", &["block"]), ("blocks", &["block"]), ("tov", &["turnover"]),
    ("pra", &["pra", "points rebounds assists", "points + rebounds + assists"]), ("pr", &["points + rebounds", "points and rebounds"]),
    ("pa", &["points + assists", "points and assists"]), ("ra", &["rebounds + assists", "rebounds and assists"]),
    ("sog", &["shots on goal", "shots", "sog"]), ("goals", &["goal"]), ("saves", &["save"]),
    ("pass_yds", &["passing yards", "pass yards"]), ("rush_yds", &["rushing yards", "rush yards"]), ("rec_yds", &["receiving yards", "rec yards"]),
    ("receptions", &["reception", "catches"]), ("double_double", &["double double", "double-double"]), ("triple_double", &["triple double", "triple-double"]),
];

fn label(market: &str) -> String {
    match market {
        "pts" => "points".into(), "reb" => "rebounds".into(), "ast" => "assists".into(), "fg3m" => "threes made".into(), "stl" => "steals".into(),
        "blk" => "blocks".into(), "tov" => "turnovers".into(), "pra" => "points + rebounds + assists".into(), "pr" => "points + rebounds".into(),
        "pa" => "points + assists".into(), "ra" => "rebounds + assists".into(), "sog" => "shots on goal".into(), "pass_yds" => "passing yards".into(),
        "rush_yds" => "rushing yards".into(), "rec_yds" => "receiving yards".into(), m => m.replace('_', " "),
    }
}

fn engine_label(engine: &str) -> &'static str {
    if engine.ends_with("_team") { "team-strength model" } else {
        match engine { "nba" => "possession simulator", "nhl" => "event simulator", "nfl" => "play-by-play simulator", "mlb" => "plate-appearance simulator", _ => "engine" }
    }
}

fn status_label(s: &str) -> &'static str {
    match s { "verified" => "verified edge", "model_only" => "beats its baseline, not the market", "losing" => "losing to the market",
        "insufficient_data" => "insufficient data", _ => "no verified signal" }
}

/// Market keys the question names.
pub fn markets_named(text: &str) -> Vec<&'static str> {
    let l = text.to_lowercase();
    ALIASES.iter().filter(|(_, words)| words.iter().any(|w| l.contains(w))).map(|(k, _)| *k).collect()
}

fn kind_named(text: &str) -> Option<&'static str> {
    let l = text.to_lowercase();
    if l.contains("prop") || l.contains("player") { Some("prop") } else if l.contains("game line") || l.contains("game market") || l.contains("sides") { Some("game") } else { None }
}

fn asks_ml(text: &str) -> bool {
    let l = format!(" {} ", text.to_lowercase());
    ["machine learning", "xgboost", "lightgbm", "elastic net", "experiment", "residual", "overfit", "shadow", "promot", "ml model", "ml research"].iter().any(|k| l.contains(k))
}

fn pct4(v: &Value) -> String { v.as_f64().map(|x| format!("{x:+.4}")).unwrap_or_else(|| "?".into()) }

/// Walk-forward ML experiments (scripts/ml) that match the sport and named markets.
pub fn experiments_text(sc: &Value, sport: Option<&str>, named: &[&str], limit: usize) -> String {
    let ex: Vec<&Value> = sc["experiments"].as_array().into_iter().flatten()
        .filter(|e| sport.map(|s| e["sport"] == s).unwrap_or(true))
        .filter(|e| named.is_empty() || e["markets"].as_array().into_iter().flatten().any(|m| named.contains(&m.as_str().unwrap_or(""))))
        .collect();
    if ex.is_empty() { return String::new(); }
    let mut s = String::from("ML research (walk-forward; each learned model starts from the market's probability and may only move away from it when its features justify it):\n");
    for e in ex.iter().take(limit) {
        let best = e["best_model"].as_str().unwrap_or("");
        let m = e["models"].as_array().into_iter().flatten().find(|m| m["model"] == best);
        let d = m.map(|m| &m["brier_vs_market"]);
        let why = e["gate"]["why"].as_str().unwrap_or("");
        // the Brier number is already in the line: keep only the reason
        let why = match why.find("fails: ") {
            Some(i) => format!("Fails: {}.", &why[i + 7..]),
            None if why.starts_with("Brier vs market") && why.contains("includes 0") => "Its 99% interval includes 0: no evidence it beats the market.".into(),
            None => why.to_string(),
        };
        let best_part = if best.is_empty() { String::new() } else {
            format!(" Best learned model {}{}.", best.replace("residual_", ""),
                d.map(|d| format!(", Brier vs market {} (95% {} to {})", pct4(&d["mean"]), pct4(&d["lo95"]), pct4(&d["hi95"]))).unwrap_or_default())
        };
        s.push_str(&format!("- {}: {} ({} test rows).{} {}\n", e["name"].as_str().unwrap_or(""), e["verdict"].as_str().unwrap_or("").replace('_', " "),
            e["n_test"], best_part, why));
    }
    let shadow = ex.iter().filter(|e| e["verdict"] == "shadow_candidate").count();
    s.push_str(&format!("{shadow} of {} experiments reached shadow candidate. Nothing is promoted automatically: a candidate would run beside AI Picks without changing what it publishes.\n\n", ex.len()));
    s
}

/// One-line definitions of the metrics an answer uses (master plan 15.E: no accuracy
/// figure without its metric). Only the metrics named in `answer` are listed.
pub const DEFINITIONS: &[(&str, &str)] = &[
    ("brier", "Brier score: the average squared gap between the forecast probability and what happened (1 or 0). Lower is better; always saying 50% scores 0.25."),
    ("log loss", "Log loss: the average negative log of the probability given to what happened. Lower is better; it punishes confident misses hardest."),
    ("clv", "Closing-line value (CLV): how much better the price taken was than the price at kick-off. Beating the close on average is the clearest sign of an edge."),
    ("roi", "ROI: profit divided by the amount staked, over settled picks."),
    ("calibrat", "Calibration: whether outcomes given a probability happen that often (picks rated 60% should win about 60% of the time)."),
    ("mae", "MAE: the average absolute gap between a projection and the actual result."),
    ("rmse", "RMSE: the square root of the average squared gap; it weighs big misses more than MAE."),
    ("drawdown", "Drawdown: the largest fall in cumulative profit from a previous high."),
    ("interval", "95% interval: a bootstrap range for the difference; if it excludes 0 the difference is unlikely to be noise."),
    ("walk-forward", "Walk-forward backtest: each game is forecast using only data from before it, the way the model would have run at the time."),
];

pub fn definitions(answer: &str) -> String {
    let l = answer.to_lowercase();
    let lines: Vec<&str> = DEFINITIONS.iter().filter(|(k, _)| l.contains(k)).map(|(_, d)| *d).collect();
    if lines.is_empty() { String::new() } else { format!("\n\nWhat the measures mean:\n- {}", lines.join("\n- ")) }
}

pub fn reply(sc: &Value, sport: Option<&str>, text: &str) -> String {
    if asks_ml(text) {
        let named = markets_named(text);
        let mut s = experiments_text(sc, sport, &named, 10);
        if s.is_empty() { s = experiments_text(sc, sport, &[], 10); }
        if s.is_empty() { return "No ML experiments have been run for this yet. They run from scripts/ml/experiment.py and are stored in ml_experiments.".into(); }
        if let Some(d) = sc["dataset"].as_array() {
            let parts: Vec<String> = d.iter().filter(|r| sport.map(|x| r["sport"] == x).unwrap_or(true))
                .map(|r| format!("{} {} {} examples over {} games", r["sport"].as_str().unwrap_or("").to_uppercase(), r["source"].as_str().unwrap_or(""), r["examples"], r["games"])).collect();
            if !parts.is_empty() { s.push_str(&format!("Training data (immutable, timestamped): {}.", parts.join("; "))); }
        }
        return s.trim_end().to_string();
    }
    reply_scorecard(sc, sport, text)
}

fn reply_scorecard(sc: &Value, sport: Option<&str>, text: &str) -> String {
    let all: Vec<&Value> = sc["markets"].as_array().into_iter().flatten().filter(|m| sport.map(|s| m["sport"] == s).unwrap_or(true)).collect();
    let scope = sport.map(|s| s.to_uppercase()).unwrap_or_else(|| "all sports".into());
    if all.is_empty() { return format!("The validation scorecard has no markets for {scope} yet."); }
    let named = markets_named(text);
    let kind = kind_named(text);
    let rows: Vec<&Value> = all.iter().copied()
        .filter(|m| named.is_empty() || named.contains(&m["market"].as_str().unwrap_or("")))
        .filter(|m| kind.map(|k| m["kind"] == k).unwrap_or(true)).collect();
    let mut s = String::new();

    // one or a few named markets: the full evidence for each engine that prices them
    if !named.is_empty() && !rows.is_empty() && rows.len() <= 6 {
        for m in &rows {
            let (sp, engine, market) = (m["sport"].as_str().unwrap_or("").to_uppercase(), m["engine"].as_str().unwrap_or(""), m["market"].as_str().unwrap_or(""));
            s.push_str(&format!("{sp} {} ({}): {}, model quality {}.\n", label(market), engine_label(engine), status_label(m["status"].as_str().unwrap_or("")), m["model_quality"].as_str().unwrap_or("?")));
            for r in m["reasons"].as_array().into_iter().flatten().filter_map(|r| r.as_str()) { s.push_str(&format!("- {r}\n")); }
            s.push('\n');
        }
        s.push_str(&quarantine_note(sc, sport));
        s.push_str(&experiments_text(sc, sport, &named, 4));
        s.push_str("Model quality says whether the engine is reliable on a market; each pick also gets an opportunity grade from the EV of its current price. A pick is a verified edge only when both hold.");
        return s.trim_end().to_string();
    }

    // overview
    let pool: Vec<&Value> = if rows.is_empty() { all.clone() } else { rows };
    let count = |st: &str| pool.iter().filter(|m| m["status"] == st).count();
    s.push_str(&format!("Validation scorecard for {scope}{}: {} engine-market pairs. Verified edge {}, beats its baseline only {}, no verified signal {}, losing {}, insufficient data {}.\n",
        kind.map(|k| if k == "prop" { " (player props)" } else { " (game lines)" }).unwrap_or(""), pool.len(),
        count("verified"), count("model_only"), count("no_signal"), count("losing"), count("insufficient_data")));
    let list = |st: &str| -> String {
        let v: Vec<String> = pool.iter().filter(|m| m["status"] == st).map(|m| format!("{} {}{}", m["sport"].as_str().unwrap_or("").to_uppercase(), label(m["market"].as_str().unwrap_or("")),
            if m["engine"].as_str().unwrap_or("").ends_with("_team") { " (team model)" } else { "" })).collect();
        v.join(", ")
    };
    for (st, head) in [("verified", "Verified edge (beat the market out of sample)"), ("model_only", "Beats its naive baseline but not the market (watchlist)"), ("losing", "Losing to the market (picks hidden)")] {
        let l = list(st);
        if !l.is_empty() { s.push_str(&format!("\n{head}: {l}.\n")); }
    }
    // calibration at real lines, where there is enough data
    let mut cal: Vec<(&Value, f64, u64)> = pool.iter().filter_map(|m| Some((*m, m["lines"]["ece"].as_f64()?, m["lines"]["n"].as_u64()?))).filter(|x| x.2 >= 200).collect();
    if !cal.is_empty() {
        cal.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let fmt = |x: &(&Value, f64, u64)| format!("{} {} {:.1}% (n={})", x.0["sport"].as_str().unwrap_or("").to_uppercase(), label(x.0["market"].as_str().unwrap_or("")), x.1 * 100.0, x.2);
        if cal.len() > 3 {
            s.push_str(&format!("\nCalibration error of the raw model at real lines (lower is better). Best: {}. Worst: {}.\n",
                cal.iter().take(3).map(fmt).collect::<Vec<_>>().join("; "), cal.iter().rev().take(3).map(fmt).collect::<Vec<_>>().join("; ")));
        } else {
            s.push_str(&format!("\nCalibration error of the raw model at real lines (lower is better): {}.\n", cal.iter().map(fmt).collect::<Vec<_>>().join("; ")));
        }
    }
    // realised results of published picks
    let mut roi: Vec<(&Value, f64, f64, f64, u64)> = pool.iter().filter_map(|m| Some((*m, m["picks"]["roi"].as_f64()?, m["picks"]["roi_lo"].as_f64()?, m["picks"]["roi_hi"].as_f64()?, m["picks"]["n"].as_u64()?)))
        .filter(|x| x.4 >= 100).collect();
    if !roi.is_empty() {
        roi.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        s.push_str(&format!("\nPublished picks with 100+ settled (ROI per unit, 95% interval): {}.\n", roi.iter().take(5).map(|x| format!("{} {} {:+.1}% ({:+.1}% to {:+.1}%, n={})",
            x.0["sport"].as_str().unwrap_or("").to_uppercase(), label(x.0["market"].as_str().unwrap_or("")), x.1 * 100.0, x.2 * 100.0, x.3 * 100.0, x.4)).collect::<Vec<_>>().join("; ")));
    }
    s.push('\n');
    s.push_str(&quarantine_note(sc, sport));
    s.push_str("Ask about one market (for example \"why isn't Monster publishing NFL spreads?\") for its full evidence.");
    s.trim_end().to_string()
}

fn quarantine_note(sc: &Value, sport: Option<&str>) -> String {
    let q: Vec<String> = sc["quarantined_feeds"].as_array().into_iter().flatten().filter(|q| sport.map(|s| q["sport"] == s).unwrap_or(true))
        .map(|q| format!("{} {} at {}", q["sport"].as_str().unwrap_or("").to_uppercase(), label(q["market"].as_str().unwrap_or("")), q["book"].as_str().unwrap_or(""))).collect();
    if q.is_empty() { String::new() } else { format!("Quarantined odds feeds (their bets won far more often than their prices allow, so the quotes are mislabelled and left out): {}.\n\n", q.join(", ")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_cover_only_the_metrics_used() {
        let d = definitions("Brier 0.2813 vs market 0.249 (difference 0.0324, 95% interval 0.0218 to 0.0423)");
        assert!(d.contains("Brier score:") && d.contains("95% interval:"));
        assert!(!d.contains("Log loss") && !d.contains("ROI"));
        assert_eq!(definitions("no metrics here"), "");
    }
    use serde_json::json;

    fn sc() -> Value {
        json!({ "quarantined_feeds": [{ "sport": "nhl", "market": "points", "book": "espnbet" }], "markets": [
            { "sport": "nfl", "engine": "nfl", "market": "spread", "kind": "game", "status": "no_signal", "model_quality": "C", "lines": { "n": 0 },
              "reasons": ["Walk-forward backtest vs closing lines: 934 games over 4 seasons, better in 0 and worse in 0; fitted market-blend weight 0 (0 = the market alone was as good).", "Status: no verified signal (unproven)."] },
            { "sport": "nfl", "engine": "nfl", "market": "pass_yds", "kind": "prop", "status": "model_only", "model_quality": "B", "lines": { "n": 0 }, "reasons": ["Status: model only."] },
            { "sport": "nhl", "engine": "nhl", "market": "points", "kind": "prop", "status": "losing", "model_quality": "D", "lines": { "n": 1904, "ece": 0.058 },
              "picks": { "n": 707, "roi": -0.114, "roi_lo": -0.184, "roi_hi": -0.046 }, "reasons": ["Status: losing."] },
        ]})
    }

    #[test]
    fn a_named_market_gets_its_evidence() {
        let r = reply(&sc(), Some("nfl"), "Why isn't Monster publishing NFL spreads?");
        assert!(r.starts_with("NFL spread (play-by-play simulator): no verified signal, model quality C."), "{r}");
        assert!(r.contains("934 games over 4 seasons") && !r.contains("pass"), "{r}");
    }

    #[test]
    fn ml_questions_report_experiments() {
        let mut v = sc();
        v["experiments"] = json!([{ "name": "nfl spread backtest outcome", "sport": "nfl", "markets": ["spread"], "verdict": "inconclusive", "n_test": 775,
            "best_model": "residual_enet", "gate": { "why": "Brier vs market +0.0015; fails: 99% interval -0.0029 to +0.0056 includes 0" },
            "models": [{ "model": "residual_enet", "brier_vs_market": { "mean": 0.0015, "lo95": -0.0016, "hi95": 0.0048 } }] }]);
        v["dataset"] = json!([{ "sport": "nfl", "source": "backtest", "examples": 10195, "games": 1035 }]);
        let r = reply(&v, Some("nfl"), "does the XGBoost residual model beat the market on NFL spreads?");
        assert!(r.contains("nfl spread backtest outcome: inconclusive (775 test rows). Best learned model enet, Brier vs market +0.0015 (95% -0.0016 to +0.0048). Fails: 99% interval -0.0029 to +0.0056 includes 0."), "{r}");
        assert!(r.contains("0 of 1 experiments reached shadow candidate") && r.contains("NFL backtest 10195 examples over 1035 games"), "{r}");
        let s = reply(&v, Some("nfl"), "Why isn't Monster publishing NFL spreads?");
        assert!(s.contains("ML research") && s.contains("775 test rows"), "{s}");
    }

    #[test]
    fn an_overview_lists_statuses_calibration_roi_and_quarantine() {
        let r = reply(&sc(), None, "which models are best calibrated?");
        assert!(r.contains("3 engine-market pairs") && r.contains("NFL passing yards") && r.contains("NHL points 5.8% (n=1904)"), "{r}");
        assert!(r.contains("NHL points -11.4% (-18.4% to -4.6%, n=707)") && r.contains("NHL points at espnbet"), "{r}");
        let p = reply(&sc(), Some("nfl"), "how are the NFL player props models?");
        assert!(p.contains("(player props): 1 engine-market pairs"), "{p}");
    }
}
