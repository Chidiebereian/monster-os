//! Simulations from chat (docs/architecture/monster-gpt-v2.md sections 3, 6 and 8).
//!
//! "Run 100,000 simulations of the next Lakers game", "simulate Edmonton's next game
//! 200,000 times", "what if Jokic is out?". The game comes from the team's next
//! scheduled game; players named in the request are ruled out (a scenario). Every run
//! is a `gpt_sim_runs` row recording the engine, model version, seed, simulation
//! count, the inputs' fingerprint and the overrides, so the same seed and inputs give
//! the same numbers. A scenario runs the baseline too, with the same seed, so the
//! difference comes from the change alone; its numbers are marked hypothetical.
//!
//! Limits: one simulation job at a time (the engines are CPU-bound), 100,000 to
//! 1,000,000 simulations (the engines' own bounds), 150 s per run. A run is a
//! background task: a dropped connection does not lose it, and cancel stops the
//! waiting and marks the run cancelled (the engine finishes its pass; the result is
//! not kept).

use serde_json::{json, Value};

use crate::research::mentions;
use crate::tools::{Tool, ToolCall, Tools};

pub const MIN_N: u32 = 100_000;
pub const MAX_N: u32 = 1_000_000;

#[derive(Clone, Debug, PartialEq)]
pub struct SimAsk { pub n: u32, pub asked_n: Option<u64>, pub seed: Option<u64>, pub mentions: Vec<String>, pub sport: Option<String> }

fn words(l: &str) -> Vec<String> { l.split(|c: char| !(c.is_alphanumeric() || c == ',' || c == '.')).filter(|w| !w.is_empty()).map(|w| w.trim_end_matches(['.', ',']).to_string()).collect() }

/// "100,000", "200000", "100k", "1 million", "half a million", "a million"
pub fn count_of(text: &str) -> Option<u64> {
    let l = text.to_lowercase();
    if l.contains("half a million") { return Some(500_000); }
    let ws = words(&l);
    for (i, w) in ws.iter().enumerate() {
        let next = ws.get(i + 1).map(|s| s.as_str());
        if w == "a" && next == Some("million") { return Some(1_000_000); }
        let t = w.replace(',', "");
        let (num, mult) = if let Some(x) = t.strip_suffix('k') { (x.to_string(), 1_000.0) } else if let Some(x) = t.strip_suffix('m') { (x.to_string(), 1_000_000.0) } else { (t, 1.0) };
        let Ok(v) = num.parse::<f64>() else { continue };
        let mult = if matches!(next, Some("million" | "mil")) { 1_000_000.0 } else if matches!(next, Some("thousand")) { 1_000.0 } else { mult };
        let n = v * mult;
        // a count, not a year or a seed: followed by sims/times/runs, or large
        let counted = matches!(next, Some("simulations" | "sims" | "times" | "runs" | "simulation" | "million" | "thousand" | "mil"));
        if (counted || n >= 1_000.0) && !(i > 0 && ws[i - 1] == "seed") && n >= 1.0 { return Some(n as u64); }
    }
    None
}

fn seed_of(text: &str) -> Option<u64> {
    let ws = words(&text.to_lowercase());
    ws.windows(2).find(|p| p[0] == "seed").and_then(|p| p[1].replace(',', "").parse().ok())
}

pub fn parse(text: &str) -> Option<SimAsk> {
    if !crate::research::is_sim_request(text) { return None; }
    let asked = count_of(text);
    let n = asked.map(|a| a.clamp(MIN_N as u64, MAX_N as u64) as u32).unwrap_or(MIN_N);
    let l = text.to_lowercase();
    let sport = ["nba", "nfl", "nhl", "mlb"].iter().find(|s| l.split(|c: char| !c.is_alphanumeric()).any(|w| w == **s)).map(|s| s.to_string());
    Some(SimAsk { n, asked_n: asked, seed: seed_of(text), mentions: mentions(text), sport })
}

/// A seed for a run nobody gave one: derived from the run id, below 2^53 so it survives JSON.
pub fn derive_seed(run_id: &str) -> u64 {
    u64::from_str_radix(&crate::rag::fnv64(run_id.as_bytes()), 16).unwrap_or(42) % (1u64 << 53)
}

/// What a run keeps: the numbers an answer uses and the inputs' fingerprint, not the full distributions.
pub fn compact(v: &Value) -> Value {
    json!({ "game_id": v["game_id"], "away": v["away"], "home": v["home"], "date": v["date"], "model_version": v["model_version"], "seed": v["seed"], "n": v["n"],
        "sim_ms": v["sim_ms"], "win": v["win"], "score": { "away": v["score"]["away"]["mean"], "home": v["score"]["home"]["mean"] },
        "total": { "mean": v["total"]["mean"], "q10": v["total"]["q10"], "q50": v["total"]["q50"], "q90": v["total"]["q90"] },
        "margin_home": { "mean": v["margin_home"]["mean"], "q10": v["margin_home"]["q10"], "q90": v["margin_home"]["q90"] },
        "data_cutoff": v["inputs"]["data_cutoff"], "lineup_mode": v["inputs"]["lineup_mode"], "flags": v["inputs"]["flags"] })
}

pub fn inputs_fingerprint(v: &Value) -> String { format!("fnv64:{}", crate::rag::fnv64(v["inputs"].to_string().as_bytes())) }

/// A share as a percentage, rounded half away from zero the way the verifier rounds it.
fn pct(x: f64) -> String { format!("{:.1}%", (x * 1000.0).round() / 10.0) }
fn r1(x: f64) -> String { format!("{:.1}", x) }

/// Variance of a win share between parameter draws, per run of 200 draws. The engines
/// sample 200 posterior parameter draws per run, so games within a draw are correlated
/// and the binomial error sqrt(p(1-p)/n) understates how much a run moves with the seed.
/// Measured 2026-11-03, six seeds x 100,000 games per sport: seed-to-seed sd of the home
/// win share 0.33 (NBA), 0.37 (NHL), 0.33 (NFL) points, 2.1-2.4x binomial; the
/// between-draw part (sd^2 - binomial^2) averaged 1.0e-5 (apps/monster-gpt/tests/test_sim_repro.py).
pub const BETWEEN_DRAW_VAR: f64 = 1.0e-5;

/// Monte Carlo error of a simulated share (one standard deviation across seeds, in
/// percentage points): the binomial part, which shrinks with more games, plus the
/// between-draw part, which does not.
pub fn mc_error_pts(p: f64, n: f64) -> f64 { if n > 0.0 { (p * (1.0 - p) / n + BETWEEN_DRAW_VAR).sqrt() * 100.0 } else { 0.0 } }

pub struct Names { pub away: String, pub home: String }

/// The answer for one run (and its baseline when it is a scenario). Returns the reply, the
/// numbers it derived from the run's results (model output) and the facts about the run
/// itself (count, time, draws, interval), for the verifier.
pub fn reply(run_id: &str, v: &Value, base: Option<&Value>, out_names: &[String], names: &Names, sport: &str) -> (String, Vec<String>, Vec<String>) {
    let n = v["n"].as_f64().unwrap_or(0.0);
    let (wa, wh) = (v["win"]["away"].as_f64().unwrap_or(0.0), v["win"]["home"].as_f64().unwrap_or(0.0));
    let se = mc_error_pts(wh, n);
    let mut derived = vec![format!("Monte Carlo error {:.1} points = sqrt({wh} x {} / {n} + {BETWEEN_DRAW_VAR}) x 100", se, 1.0 - wh)];
    let secs = v["sim_ms"].as_f64().unwrap_or(0.0) / 1000.0;
    let meta = vec![format!("run time {secs:.1} s = sim_ms / 1000"), "200 parameter draws per run (engine default)".to_string(),
        "80% of simulations = the 10th to 90th percentile".to_string(), format!("{} simulations", fmt_count(n as u64))];
    let engine = match sport { "nba" => "NBA possession simulator", "nfl" => "NFL play-by-play simulator", "nhl" => "NHL event simulator", _ => "MLB plate-appearance simulator" };
    let what = if out_names.is_empty() { String::new() } else { format!(" with {} ruled out (hypothetical)", out_names.join(" and ")) };
    let mut s = format!("I simulated the {} at the {} {} times{what} with the {engine} ({}, seed {}, data through {}, {} lineups, {:.1} s).",
        names.away, names.home, fmt_count(n as u64), v["model_version"].as_str().unwrap_or("?"), v["seed"], v["inputs"]["data_cutoff"].as_str().unwrap_or("?"),
        v["inputs"]["lineup_mode"].as_str().unwrap_or("?"), secs);
    let ot = v["win"]["overtime"].as_f64().map(|o| format!(" ({} of simulations went to overtime)", pct(o))).unwrap_or_default();
    s.push_str(&format!("\n\n{} win {}, {} win {}{ot}. Rerunning with another seed moves these shares by about {:.1} points (the engines sample 200 parameter draws, \
        so more simulations barely shrink that); the model's own error is larger still (ask how reliable the {} model is).",
        names.home, pct(wh), names.away, pct(wa), se, sport.to_uppercase()));
    s.push_str(&format!("\n\nAverage score: {} {}, {} {}. Total: average {}, and 80% of simulations landed between {} and {}.",
        names.away, r1(v["score"]["away"]["mean"].as_f64().unwrap_or(0.0)), names.home, r1(v["score"]["home"]["mean"].as_f64().unwrap_or(0.0)),
        r1(v["total"]["mean"].as_f64().unwrap_or(0.0)), v["total"]["q10"], v["total"]["q90"]));
    if let Some(b) = base {
        let bh = b["win"]["home"].as_f64().unwrap_or(0.0);
        // from the shares as shown, so the sentence agrees with the numbers beside it
        let shown = |x: f64| (x * 1000.0).round() / 10.0;
        let d = ((shown(wh) - shown(bh)) * 10.0).round() / 10.0;
        derived.push(format!("change {:.1} points = {} - {}", d, pct(wh), pct(bh)));
        let noise = if d.abs() < 2.0 * se { format!(" That is within twice the run-to-run error ({:.1} points), so the simulator shows no measurable effect of the change.", 2.0 * se) } else { String::new() };
        if !noise.is_empty() { derived.push(format!("twice the run-to-run error {:.1} points = 2 x {:.1}", 2.0 * se, se)); }
        s.push_str(&format!("\n\nWith everyone available (same seed): {} win {}, so the change moves the {} by {:+.1} points.{noise}", names.home, pct(bh), names.home, d));
    }
    let flags: Vec<&str> = v["inputs"]["flags"].as_array().into_iter().flatten().filter_map(|f| f.as_str()).collect();
    if !flags.is_empty() { s.push_str(&format!("\n\nThe engine flagged: {}.", flags.join("; "))); }
    s.push_str(&format!("\n\nThese are model estimates, not results. Run {run_id}: the same seed and inputs reproduce it."));
    (s, derived, meta)
}

pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() { if i > 0 && (s.len() - i) % 3 == 0 { out.push(','); } out.push(c); }
    out
}

/// Resolve the game and the ruled-out players for a request (no simulation yet).
pub struct Plan { pub sport: String, pub game_id: String, pub names: Names, pub out_ids: Vec<String>, pub out_names: Vec<String>, pub entities: Vec<Value> }

pub async fn plan(tools: &Tools, ask: &SimAsk, focus: &[String], forced: &std::collections::HashMap<String, String>, calls: &mut Vec<ToolCall>) -> Result<Plan, String> {
    let mut team: Option<String> = None;
    let mut players: Vec<(String, String)> = Vec::new();
    let mut entities = Vec::new();
    for m in &ask.mentions {
        let c = tools.call(Tool::Resolve { q: m.clone(), sport: ask.sport.clone(), kind: None, focus: focus.to_vec() }).await;
        let mut v = c.value.clone();
        calls.push(c);
        // a name picked with @: take that candidate, whatever else shares the name
        if let Some(id) = forced.get(m) {
            if let Some(pick) = v["candidates"].as_array().and_then(|cs| cs.iter().find(|x| x["entity_id"] == id.as_str()).cloned()) {
                v = json!({ "status": "resolved", "entity": pick });
            }
        }
        match v["status"].as_str() {
            Some("resolved") => {
                let id = v["entity"]["entity_id"].as_str().unwrap_or("").to_string();
                entities.push(v["entity"].clone());
                if id.starts_with("tm:") && team.is_none() { team = Some(id); }
                else if id.starts_with("pl:") { players.push((id, v["entity"]["name"].as_str().unwrap_or("").to_string())); }
            }
            Some("ambiguous") => {
                let c = v["candidates"].as_array().cloned().unwrap_or_default();
                return Err(crate::research::clarify_text(m, &c));
            }
            _ => {}
        }
    }
    // "simulate the Lakers without LeBron": the team; "what if Jokic is out?": his team;
    // otherwise the team the conversation is about
    let team = team
        .or_else(|| players.first().and_then(|(id, _)| entities.iter().find(|e| e["entity_id"] == id.as_str()).and_then(|e| e["team_id"].as_str().map(String::from))))
        .or_else(|| focus.iter().find(|f| f.starts_with("tm:")).cloned());
    let Some(team) = team else { return Err("Which game should I simulate? Name a team (for example \"simulate the next Lakers game\").".into()) };
    let c = tools.call(Tool::TeamSchedule { entity: team.clone(), next: 1 }).await;
    let v = c.value.clone();
    calls.push(c);
    let Some(g) = v["upcoming"].as_array().and_then(|a| a.first()).cloned() else {
        return Err(format!("I don't have a scheduled game stored for the {}, so there is nothing to simulate.", v["name"].as_str().unwrap_or("team")));
    };
    let sport = team.split(':').nth(1).unwrap_or("").to_string();
    let names = Names { away: g["away_name"].as_str().or(g["away_team"].as_str()).unwrap_or("away team").into(), home: g["home_name"].as_str().or(g["home_team"].as_str()).unwrap_or("home team").into() };
    Ok(Plan { sport, game_id: g["game_id"].as_str().unwrap_or("").to_string(), names,
        out_ids: players.iter().map(|(id, _)| id.rsplit(':').next().unwrap_or("").to_string()).collect(), out_names: players.into_iter().map(|(_, n)| n).collect(), entities })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_seeds_and_scenarios_are_read_from_the_request() {
        let a = parse("Run 100,000 simulations of the next Lakers game.").unwrap();
        assert_eq!((a.n, a.asked_n, a.mentions.clone()), (100_000, Some(100_000), vec!["Lakers".to_string()]));
        assert_eq!(parse("Simulate Edmonton's next game 200,000 times and tell me the win probability.").unwrap().n, 200_000);
        assert_eq!(parse("simulate the next Celtics game 1 million times").unwrap().n, 1_000_000);
        assert_eq!(parse("simulate the Celtics game 250k times, seed 7").unwrap().seed, Some(7));
        let small = parse("simulate the Lakers game 10,000 times").unwrap();
        assert_eq!((small.n, small.asked_n), (MIN_N, Some(10_000)), "the engines run at least 100,000");
        assert_eq!(parse("simulate the Lakers game 5 million times").unwrap().n, MAX_N);
        assert_eq!(parse("simulate the next Nuggets game without Nikola Jokic").unwrap().mentions, vec!["Nuggets", "Nikola Jokic"]);
        assert!(parse("How does the NHL simulator handle empty nets?").is_none());
    }

    #[test]
    fn seeds_fit_in_json_and_repeat() {
        assert_eq!(derive_seed("sim_abc"), derive_seed("sim_abc"));
        assert!(derive_seed("sim_abc") < (1u64 << 53));
        assert_ne!(derive_seed("sim_abc"), derive_seed("sim_abd"));
    }

    #[test]
    fn the_reply_states_the_run_and_its_uncertainty() {
        let v = json!({ "n": 100000, "seed": 42, "model_version": "nhl-event-sim-v4", "sim_ms": 1077, "win": { "home": 0.5475, "away": 0.4525, "overtime": 0.2141 },
            "score": { "away": { "mean": 2.804 }, "home": { "mean": 3.103 } }, "total": { "mean": 5.906, "q10": 3, "q90": 9 },
            "inputs": { "data_cutoff": "2026-11-04", "lineup_mode": "confirmed", "flags": [] } });
        let names = Names { away: "Ottawa Senators".into(), home: "Buffalo Sabres".into() };
        let (r, derived, meta) = reply("sim_1", &v, None, &[], &names, "nhl");
        assert!(meta[0].starts_with("run time 1.1 s"));
        assert!(r.starts_with("I simulated the Ottawa Senators at the Buffalo Sabres 100,000 times with the NHL event simulator (nhl-event-sim-v4, seed 42, data through 2026-11-04, confirmed lineups, 1.1 s)."), "{r}");
        assert!(r.contains("Buffalo Sabres win 54.8%, Ottawa Senators win 45.3% (21.4% of simulations went to overtime)."), "{r}");
        assert!(r.contains("moves these shares by about 0.4 points"), "{r}");
        assert!((mc_error_pts(0.5475, 1_000_000.0) - 0.32).abs() < 0.01, "ten times the games, almost the same error");
        assert!(r.contains("Run sim_1: the same seed and inputs reproduce it."));
        assert!(derived[0].starts_with("Monte Carlo error 0.4 points"), "{}", derived[0]);
        let mut s = v.clone();
        s["win"]["home"] = json!(0.5010);
        let (r2, d2, _) = reply("sim_2", &s, Some(&v), &["Rasmus Dahlin".into()], &names, "nhl");
        assert!(r2.contains("with Rasmus Dahlin ruled out (hypothetical)") && r2.contains("by -4.7 points"), "{r2}");
        assert!(!r2.contains("no measurable effect"), "4.7 points is well outside the error");
        let mut tiny = v.clone();
        tiny["win"]["home"] = json!(0.5450);
        let (r3, _, _) = reply("sim_3", &tiny, Some(&v), &["Someone".into()], &names, "nhl");
        assert!(r3.contains("by -0.3 points. That is within twice the run-to-run error (0.7 points), so the simulator shows no measurable effect of the change."), "{r3}");
        assert!(d2[1].starts_with("change -4.7 points"));
    }

    #[test]
    fn counts_print_with_separators() {
        assert_eq!(fmt_count(100000), "100,000");
        assert_eq!(fmt_count(1000000), "1,000,000");
    }
}
