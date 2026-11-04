//! Claim-level verification and citations (docs/architecture/monster-gpt-v2.md, 7).
//!
//! Every tool result becomes facts: the numbers and names it holds, what kind of
//! value each is (observed, computed from observed rows, a model's estimate, a
//! hypothetical run, a reference document), where it came from and how old it is.
//! The final answer is split into sentences; each number and name in a sentence is
//! matched to the facts that hold it, and the sentence gets the weakest status of its
//! claims plus a citation for every fact it rests on. Citations point only at sources
//! of this turn: nothing is written by the model, so a citation cannot be invented.

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use crate::tools::Source;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FactKind { Observed, Computed, ModelEstimate, Hypothetical, Reference }

#[derive(Clone, Debug)]
pub struct Fact {
    pub id: String,
    pub kind: FactKind,
    /// the turn's source tag ("S2") and its record
    pub source: String,
    pub source_id: String,
    pub label: String,
    pub url: String,
    pub as_of: Option<String>,
    pub fetched_at: DateTime<Utc>,
    pub stale: bool,
    values: HashSet<String>,
    text: String,
    /// subject|metric -> value, for spotting two sources that disagree
    keyed: HashMap<String, String>,
}

/// Keyed values of the record shapes knowledge.rs returns: a team's record in a
/// standings table, a player's average per stat.
fn keyed_of(v: &Value) -> HashMap<String, String> {
    let mut k = HashMap::new();
    for r in v["standings"].as_array().into_iter().flatten() {
        if let Some(n) = r["name"].as_str().or(r["team"].as_str()) { k.insert(format!("{}|record", n.to_lowercase()), format!("{}-{}", r["w"], r["l"])); }
    }
    if let (Some(n), Some(sum)) = (v["name"].as_str(), v["summary"].as_object()) {
        for (stat, x) in sum { if let Some(m) = x["mean"].as_f64() { k.insert(format!("{}|{stat}|{}", n.to_lowercase(), v["window"].as_str().unwrap_or("")), norm_num(m)); } }
    }
    k
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status { Verified, CrossVerified, PartiallyVerified, Conflicting, Unverified, Stale, ModelEstimated, Hypothetical }

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self { Status::Verified => "verified", Status::CrossVerified => "cross_verified", Status::PartiallyVerified => "partially_verified",
            Status::Conflicting => "conflicting", Status::Unverified => "unverified", Status::Stale => "stale", Status::ModelEstimated => "model_estimated",
            Status::Hypothetical => "hypothetical" }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Claim {
    pub sentence_ix: usize,
    pub text: String,
    pub status: Status,
    pub citations: Vec<Value>,
    pub issues: Vec<String>,
}

/// JSON keys whose numbers are a model's output, not an observed record.
const MODEL_KEYS: &[&str] = &["model", "p_model", "prob", "proj", "fair", "edge", "ev", "win_home", "win_away", "expected", "lean", "sim", "kelly",
    "stake", "playable", "margin_home", "blend", "mean_points", "posterior"];
/// Keys whose numbers are computed from observed rows (averages, tables, records).
const COMPUTED_KEYS: &[&str] = &["summary", "mean", "median", "sd", "avg", "pct", "gb", "pts", "diff", "record", "standings", "units", "roi", "win_rate",
    "brier", "log_loss", "calibration", "clv", "series", "total"];

fn kind_for(tool: &str, path: &[String]) -> FactKind {
    let has = |keys: &[&str]| path.iter().any(|p| { let p = p.to_lowercase(); keys.iter().any(|k| p == *k || p.starts_with(&format!("{k}_")) || p.ends_with(&format!("_{k}"))) });
    if tool == "docs.search" || tool == "research.search" { return FactKind::Reference; }
    // a simulation is the model's estimate; a scenario run is hypothetical
    if tool == "sim.run" { return if path.iter().any(|p| p == "hypothetical" || p == "overrides") { FactKind::Hypothetical } else { FactKind::ModelEstimate }; }
    if path.iter().any(|p| p == "overrides" || p == "hypothetical") { return FactKind::Hypothetical; }
    // an AI pick's price, line and book are what the sportsbook posted; the rest is the model's
    if tool == "picks.today" && path.iter().any(|p| matches!(p.as_str(), "price" | "line" | "odds" | "book" | "american")) && !has(&["fair", "playable", "p_model", "edge", "ev"]) {
        return FactKind::Observed;
    }
    if has(MODEL_KEYS) || tool == "picks.today" { return FactKind::ModelEstimate; }
    if has(COMPUTED_KEYS) || matches!(tool, "picks.record" | "model.validation" | "kb.standings") { return FactKind::Computed; }
    FactKind::Observed
}

/// A value as compared: rounded to 2 decimals, sign, $ and thousands separators dropped.
fn norm_num(x: f64) -> String { format!("{}", (x.abs() * 100.0).round() / 100.0) }

fn walk(v: &Value, path: &mut Vec<String>, tool: &str, out: &mut HashMap<FactKind, (HashSet<String>, String)>) {
    match v {
        Value::Object(m) => for (k, x) in m { path.push(k.clone()); walk(x, path, tool, out); path.pop(); },
        Value::Array(a) => for x in a { walk(x, path, tool, out); },
        Value::Number(n) => if let Some(x) = n.as_f64() {
            let e = out.entry(kind_for(tool, path)).or_default();
            e.0.insert(norm_num(x));
            // a probability may be written as a percentage, an average to one decimal
            if x.abs() <= 1.0 { e.0.insert(norm_num(x * 100.0)); e.0.insert(norm_num((x * 1000.0).round() / 10.0)); }
            e.0.insert(norm_num((x * 10.0).round() / 10.0));
        },
        Value::String(s) => {
            let e = out.entry(kind_for(tool, path)).or_default();
            e.1.push_str(&s.to_lowercase());
            e.1.push('\n');
            // numbers inside strings ("138-117", "4-3", "2026-11-02") are values too
            for n in crate::validate::numbers(s) { if let Some(c) = crate::validate::canon_value(&n) { e.0.insert(c); } }
            for part in s.split(|c: char| c == '-' || c == ':' || c == ' ') { if let Ok(x) = part.parse::<f64>() { e.0.insert(norm_num(x)); } }
        }
        _ => {}
    }
}

/// How long a tool's data counts as current, against the record's own date.
fn freshness_days(tool: &str) -> Option<i64> {
    match tool { "kb.standings" => Some(3), "kb.results" | "kb.schedule" => Some(14), _ => None }
}

fn as_of_of(v: &Value) -> Option<String> {
    for k in ["as_of", "last_date", "generated_at", "updated_at"] {
        if let Some(s) = v[k].as_str() { return Some(s.to_string()); }
    }
    v["games"].as_array().and_then(|g| g.first()).and_then(|g| g["date"].as_str()).map(String::from)
}

/// Facts from one tool result.
pub fn facts_from(tool: &str, v: &Value, src: &Source, start: usize, today: NaiveDate) -> Vec<Fact> {
    let mut parts: HashMap<FactKind, (HashSet<String>, String)> = HashMap::new();
    walk(v, &mut Vec::new(), tool, &mut parts);
    let as_of = as_of_of(v);
    let stale = match (freshness_days(tool), as_of.as_deref().and_then(|d| NaiveDate::parse_from_str(&d[..d.len().min(10)], "%Y-%m-%d").ok())) {
        (Some(days), Some(d)) => (today - d).num_days() > days,
        _ => false,
    };
    let source_id = v["source"]["source_id"].as_str().unwrap_or("monster_os").to_string();
    let mut kinds: Vec<FactKind> = parts.keys().copied().collect();
    kinds.sort();
    let keyed = keyed_of(v);
    kinds.into_iter().enumerate().map(|(i, k)| {
        let (values, text) = parts.remove(&k).unwrap_or_default();
        Fact { id: format!("F{}", start + i + 1), kind: k, source: src.id.clone(), source_id: source_id.clone(), label: src.label.clone(), url: src.url.clone(),
            as_of: as_of.clone(), fetched_at: src.fetched_at, stale, values, text, keyed: if i == 0 { keyed.clone() } else { HashMap::new() } }
    }).collect()
}

/// A fact for text the turn built in code from tool results (a bet card, a model-vs-market block).
pub fn fact_from_text(kind: FactKind, text: &str, src: &Source, id: usize) -> Fact {
    let mut values = HashSet::new();
    for n in crate::validate::numbers(text) { if let Some(c) = crate::validate::canon_value(&n) { values.insert(c); } }
    Fact { id: format!("F{id}"), kind, source: src.id.clone(), source_id: "monster_os".into(), label: src.label.clone(), url: src.url.clone(),
        as_of: None, fetched_at: src.fetched_at, stale: false, values, text: text.to_lowercase(), keyed: HashMap::new() }
}

/// Sentences as shown (a line break or ". ", "? ", "! " ends one; decimals and initials stay whole).
pub fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let chars: Vec<char> = line.chars().collect();
        let mut cur = String::new();
        for (i, c) in chars.iter().enumerate() {
            cur.push(*c);
            let end = matches!(c, '.' | '?' | '!') && chars.get(i + 1).map(|n| n.is_whitespace()).unwrap_or(true)
                // "C. McDavid", "vs. Boston": a single capital or "vs" before the dot is not an end
                && !(cur.trim_end_matches('.').rsplit(' ').next().map(|w| (w.len() == 1 && w.chars().all(|x| x.is_uppercase())) || w == "vs").unwrap_or(false));
            if end { if !cur.trim().is_empty() { out.push(cur.trim().to_string()); } cur.clear(); }
        }
        if !cur.trim().is_empty() { out.push(cur.trim().to_string()); }
    }
    out
}

/// Names a sentence states as fact: capitalised multi-word names plus single team or
/// player words that a fact also holds are checked; ordinary words are not claims.
fn sentence_names(s: &str) -> Vec<String> {
    crate::judge::names(s).into_iter().map(|n| n.trim_end_matches(['\'', '\u{2019}']).to_string()).collect()
}

pub fn check(answer: &str, facts: &[Fact]) -> Vec<Claim> {
    let mut out = Vec::new();
    for (ix, s) in sentences(answer).into_iter().enumerate() {
        // a source line ("Source: player_game_logs (espn_site).") cites, it is not a claim
        // a question back to the user ("Which James do you mean?") asks, it does not claim
        if s.starts_with("Source:") || s.starts_with("Sources:") || s.ends_with('?') { continue; }
        // the user's own words, quoted back ("I looked for \"Madison Square Garden\""), are not claims
        let bare: String = { let mut inq = false; s.chars().filter(|c| { if *c == '"' { inq = !inq; return false; } !inq }).collect() };
        // identifiers (run ids, model versions, game ids) mix letters and digits; they are labels, not numbers
        let bare: String = bare.split(' ').map(|w| {
            let core = w.trim_matches(|c: char| !c.is_alphanumeric());
            if core.chars().any(|c| c.is_ascii_digit()) && core.chars().any(|c| c.is_ascii_alphabetic()) && !core.ends_with('%') { " " } else { w }
        }).collect::<Vec<_>>().join(" ");
        let nums: Vec<String> = crate::validate::numbers(&bare).into_iter().filter(|n| crate::validate::checkable(n)).collect();
        let names = sentence_names(&bare);
        if nums.is_empty() && names.is_empty() { continue; }
        let mut used: BTreeSet<usize> = BTreeSet::new();
        let mut issues = Vec::new();
        let (mut supported, mut unsupported) = (0, 0);
        let mut kinds_per_claim: Vec<BTreeSet<FactKind>> = Vec::new();
        let mut sources_per_claim: Vec<BTreeSet<String>> = Vec::new();
        // records about the subjects this sentence names: a number shared with another
        // player's record ("last 20 games") is cited from the named player's record only
        let about: BTreeSet<usize> = names.iter().flat_map(|nm| { let nl = nm.to_lowercase(); facts.iter().enumerate().filter(move |(_, f)| f.text.contains(&nl)).map(|(i, _)| i) }).collect();
        for n in &nums {
            let Some(c) = crate::validate::canon_value(n) else { continue };
            let pct_as_prob = n.ends_with('%').then(|| c.parse::<f64>().ok().map(|p| norm_num(p / 100.0))).flatten();
            let hits: Vec<usize> = facts.iter().enumerate().filter(|(_, f)| f.values.contains(&c) || pct_as_prob.as_ref().map(|p| f.values.contains(p)).unwrap_or(false))
                .map(|(i, _)| i).collect();
            if hits.is_empty() { unsupported += 1; issues.push(format!("{n} is not in the records this answer used")); continue; }
            let own: Vec<usize> = hits.iter().copied().filter(|i| about.contains(i)).collect();
            let hits = if own.is_empty() { hits } else { own };
            supported += 1;
            kinds_per_claim.push(hits.iter().map(|i| facts[*i].kind).collect());
            sources_per_claim.push(hits.iter().filter(|i| matches!(facts[**i].kind, FactKind::Observed | FactKind::Computed)).map(|i| facts[*i].source_id.clone()).collect());
            used.extend(hits);
        }
        for name in &names {
            let nl = name.to_lowercase();
            let hits: Vec<usize> = facts.iter().enumerate().filter(|(_, f)| f.text.contains(&nl)).map(|(i, _)| i).collect();
            if hits.is_empty() { unsupported += 1; issues.push(format!("\"{name}\" is not in the records this answer used")); continue; }
            supported += 1;
            used.extend(hits);
        }
        // two sources with different values for the same subject and metric this sentence names
        let sl = s.to_lowercase();
        let mut conflict: Option<String> = None;
        for (i, a) in facts.iter().enumerate() {
            for b in facts.iter().skip(i + 1) {
                if a.source_id == b.source_id { continue; }
                for (key, va) in &a.keyed {
                    if let Some(vb) = b.keyed.get(key) {
                        let subject = key.split('|').next().unwrap_or("");
                        if va != vb && !subject.is_empty() && sl.contains(subject) {
                            conflict = Some(format!("{} says {va}, {} says {vb} ({})", a.source_id, b.source_id, key.replace('|', " ")));
                            used.insert(i);
                            used.insert(facts.iter().position(|x| std::ptr::eq(x, b)).unwrap_or(i));
                        }
                    }
                }
            }
        }
        if let Some(c) = &conflict { issues.push(c.clone()); }
        let status = if conflict.is_some() { Status::Conflicting }
            else if supported == 0 { Status::Unverified }
            else if unsupported > 0 { Status::PartiallyVerified }
            // a number held only by model outputs is the model's estimate, not a record
            else if kinds_per_claim.iter().any(|k| k.contains(&FactKind::Hypothetical) && !k.iter().any(|x| matches!(x, FactKind::Observed | FactKind::Computed))) { Status::Hypothetical }
            else if kinds_per_claim.iter().any(|k| !k.iter().any(|x| matches!(x, FactKind::Observed | FactKind::Computed | FactKind::Reference))) { Status::ModelEstimated }
            else if used.iter().all(|i| facts[*i].stale) { Status::Stale }
            else if !sources_per_claim.is_empty() && sources_per_claim.iter().all(|s| s.len() >= 2) { Status::CrossVerified }
            else { Status::Verified };
        // one citation per source record used (facts of one tool call share a source)
        let mut seen = HashSet::new();
        let citations: Vec<Value> = used.iter().map(|i| &facts[*i]).filter(|f| seen.insert(f.source.clone())).map(|f| json!({
            "source": f.source, "fact": f.id, "source_id": f.source_id, "label": f.label, "ref": f.url, "as_of": f.as_of, "retrieved_at": f.fetched_at,
            "kind": f.kind, "stale": f.stale })).collect();
        out.push(Claim { sentence_ix: ix, text: s, status, citations, issues });
    }
    out
}

/// Status counts for the done frame and the golden report.
pub fn summary(claims: &[Claim]) -> Value {
    let mut m: std::collections::BTreeMap<&str, usize> = Default::default();
    for c in claims { *m.entry(c.status.as_str()).or_default() += 1; }
    json!(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(id: &str, label: &str) -> Source { Source { id: id.into(), tool: "t".into(), label: label.into(), url: format!("http://p/{id}"), fetched_at: Utc::now() } }
    fn today() -> NaiveDate { NaiveDate::from_ymd_opt(2026, 11, 3).unwrap() }

    #[test]
    fn a_record_backed_sentence_is_verified_and_cites_its_source() {
        let v = json!({ "name": "Nikola Jokic", "window": "last 20 games", "summary": { "pts": { "mean": 23.4, "median": 23.0, "min": 8.0, "max": 40.0 } }, "last_date": "2026-11-02",
            "source": { "source_id": "espn_site", "table": "player_game_logs" } });
        let facts = facts_from("kb.player_stats", &v, &src("S1", "Player game logs"), 0, today());
        let c = check("Nikola Jokic averaged 23.4 points over his last 20 games.", &facts);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].status, Status::Verified);
        assert_eq!(c[0].citations.len(), 1);
        assert_eq!(c[0].citations[0]["source"], "S1");
        assert_eq!(c[0].citations[0]["source_id"], "espn_site");
    }

    #[test]
    fn an_invented_number_or_name_is_never_verified() {
        let v = json!({ "name": "Nikola Jokic", "summary": { "pts": { "mean": 23.4 } }, "source": { "source_id": "espn_site" } });
        let facts = facts_from("kb.player_stats", &v, &src("S1", "logs"), 0, today());
        assert_eq!(check("Nikola Jokic averaged 31.7 points.", &facts)[0].status, Status::PartiallyVerified);
        assert_eq!(check("Michael Jordan averaged 31.7 points.", &facts)[0].status, Status::Unverified);
        let c = check("Michael Jordan averaged 31.7 points.", &facts);
        assert!(c[0].citations.is_empty(), "nothing supports it, so nothing is cited");
    }

    #[test]
    fn model_numbers_are_model_estimates_not_facts() {
        let v = json!({ "game": "DEN @ LAL", "model": { "win_home": 0.58, "total": 228.5 }, "best_lines": { "home_ml": -140 } });
        let facts = facts_from("hub.brief", &v, &src("S2", "brief"), 0, today());
        assert_eq!(check("The model gives the home side a 58% chance.", &facts)[0].status, Status::ModelEstimated);
        assert_eq!(check("The best home moneyline is -140.", &facts)[0].status, Status::Verified);
    }

    #[test]
    fn old_standings_are_stale_and_two_sources_cross_verify() {
        let v = json!({ "as_of": "2026-10-20", "standings": [{ "name": "Houston Rockets", "w": 7, "l": 1, "pct": 0.875 }], "source": { "source_id": "espn_site" } });
        let facts = facts_from("kb.standings", &v, &src("S1", "standings"), 0, today());
        assert_eq!(check("Houston Rockets have the best record at 7-1.", &facts)[0].status, Status::Stale);
        let fresh = json!({ "as_of": "2026-11-02", "standings": [{ "name": "Houston Rockets", "pct": 0.875 }], "source": { "source_id": "espn_site" } });
        let other = json!({ "teams": [{ "name": "Houston Rockets", "winPercent": 0.875 }], "source": { "source_id": "nba_stats" } });
        let mut f = facts_from("kb.standings", &fresh, &src("S1", "computed"), 0, today());
        f.extend(facts_from("kb.standings", &other, &src("S2", "league"), f.len(), today()));
        assert_eq!(check("Houston Rockets lead at 87.5%.", &f)[0].status, Status::CrossVerified);
    }

    #[test]
    fn questions_and_quoted_user_words_are_not_claims() {
        let facts = facts_from("kb.player_stats", &json!({ "name": "LeBron James" }), &src("S1", "logs"), 0, today());
        assert!(check("Which James do you mean: LeBron James (PHI) or Jalen James?", &facts).is_empty());
        assert!(check("I looked for \"Madison Square Garden\" and found no match.", &facts).is_empty());
    }

    #[test]
    fn a_shared_number_is_cited_from_the_named_players_record() {
        let a = json!({ "name": "Nikola Jokic", "window": "last 20 games", "summary": { "pts": { "mean": 23.4 } }, "source": { "source_id": "espn_site" } });
        let b = json!({ "name": "Luka Doncic", "window": "last 20 games", "summary": { "pts": { "mean": 37.7 } }, "source": { "source_id": "espn_site" } });
        let mut f = facts_from("kb.player_stats", &a, &src("S1", "Jokic logs"), 0, today());
        f.extend(facts_from("kb.player_stats", &b, &src("S2", "Doncic logs"), f.len(), today()));
        let c = check("Nikola Jokic, last 20 games: 23.4 points per game.", &f);
        assert_eq!(c[0].citations.len(), 1, "{:?}", c[0].citations);
        assert_eq!(c[0].citations[0]["source"], "S1");
    }

    #[test]
    fn identifiers_are_labels_not_numbers() {
        let facts = facts_from("sim.run", &json!({ "seed": 42 }), &src("S1", "sim"), 0, today());
        assert!(check("Run sim_c4f30c15731ebaecaf80: the same seed reproduces it.", &facts).is_empty());
    }

    #[test]
    fn two_sources_that_disagree_make_the_sentence_conflicting() {
        let a = json!({ "as_of": "2026-11-02", "standings": [{ "name": "Houston Rockets", "w": 7, "l": 1 }], "source": { "source_id": "espn_site" } });
        let b = json!({ "as_of": "2026-11-02", "standings": [{ "name": "Houston Rockets", "w": 6, "l": 2 }], "source": { "source_id": "nba_stats" } });
        let mut f = facts_from("kb.standings", &a, &src("S1", "computed"), 0, today());
        f.extend(facts_from("kb.standings", &b, &src("S2", "league"), f.len(), today()));
        let c = check("Houston Rockets are 7-1.", &f);
        assert_eq!(c[0].status, Status::Conflicting);
        assert!(c[0].issues[0].contains("espn_site says 7-1, nba_stats says 6-2"), "{:?}", c[0].issues);
        assert_eq!(c[0].citations.len(), 2, "both records are shown");
    }

    #[test]
    fn scenario_runs_are_hypothetical_and_source_lines_are_not_claims() {
        let v = json!({ "overrides": { "home_win": 0.44 } });
        let facts = facts_from("sim.run", &v, &src("S3", "sim"), 0, today());
        assert_eq!(check("Without him the home side wins 44% of simulations.", &facts)[0].status, Status::Hypothetical);
        assert!(check("Source: sports_games (espn_site).", &facts).is_empty());
        let t = facts_from("kb.schedule", &json!({ "name": "Los Angeles Lakers", "upcoming": [{ "opponent_name": "Sacramento Kings" }] }), &src("S4", "schedule"), 0, today());
        assert_eq!(check("The Los Angeles Lakers' next game is at the Sacramento Kings.", &t)[0].status, Status::Verified, "a possessive is the same name");
    }

    #[test]
    fn sentences_split_without_breaking_decimals_or_initials() {
        assert_eq!(sentences("C. McDavid scored 2.6 points per game. MacKinnon 1.5.\nSource: x."),
            vec!["C. McDavid scored 2.6 points per game.", "MacKinnon 1.5.", "Source: x."]);
    }
}
