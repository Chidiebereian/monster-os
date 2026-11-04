//! Sports research questions answered from stored records (monster-gpt-v2.md
//! sections 4-6): standings, champions, a player's recent games, a team's results.
//!
//! Deterministic like the router: `detect` reads the question, `mentions` finds the
//! names in it, every name goes through `entities.resolve` (an ambiguous one gets a
//! question back, never a guess), and the reply is written in code from the tool
//! results, so every number in it is one projection-service returned.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::tools::{Tool, ToolCall, Tools};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind { Standings, Champion, Stats, Results }

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Ask {
    pub kind: Option<Kind>,
    pub sport: Option<String>,
    /// season start year (2025 = 2025-26 in the NBA and NHL)
    pub season: Option<i32>,
    #[serde(default)]
    pub this_season: bool,
    pub last: Option<u32>,
    /// stat words as asked ("points", "rebounds"); mapped per sport later
    #[serde(default)]
    pub stats: Vec<String>,
    #[serde(default)]
    pub mentions: Vec<String>,
}

fn has(l: &str, keys: &[&str]) -> bool { keys.iter().any(|k| l.contains(k)) }

/// Stat words, longest first so "passing yards" wins over "yards".
const STAT_WORDS: &[(&str, &str)] = &[
    ("passing yards", "passing yards"), ("rushing yards", "rushing yards"), ("receiving yards", "receiving yards"),
    ("passing touchdowns", "passing touchdowns"), ("home runs", "home runs"), ("stolen bases", "stolen bases"), ("total bases", "total bases"),
    ("three pointers", "threes"), ("three-pointers", "threes"), ("3-pointers", "threes"), ("threes", "threes"),
    ("points", "points"), ("rebounds", "rebounds"), ("boards", "rebounds"), ("assists", "assists"), ("steals", "steals"), ("blocks", "blocks"),
    ("turnovers", "turnovers"), ("minutes", "minutes"), ("goals", "goals"), ("shots", "shots"), ("hits", "hits"),
    ("receptions", "receptions"), ("catches", "receptions"), ("touchdowns", "touchdowns"), ("tds", "touchdowns"), ("carries", "carries"),
    ("targets", "targets"), ("interceptions", "interceptions"), ("homers", "home runs"), ("strikeouts", "strikeouts"),
    ("rbis", "rbi"), ("rbi", "rbi"), ("walks", "walks"), ("runs", "runs"),
];

/// A stat word in one sport's log keys (None: that sport does not log it).
pub fn stat_key(sport: &str, word: &str) -> Option<&'static str> {
    Some(match (sport, word) {
        ("nba", "points") => "pts", ("nba", "rebounds") => "reb", ("nba", "assists") => "ast", ("nba", "threes") => "threes",
        ("nba", "steals") => "stl", ("nba", "blocks") => "blk", ("nba", "turnovers") => "tov", ("nba", "minutes") => "minutes",
        ("nhl", "points") => "points", ("nhl", "goals") => "goals", ("nhl", "assists") => "assists", ("nhl", "shots") => "shots",
        ("nhl", "hits") => "hits", ("nhl", "minutes") => "toi",
        ("nfl", "passing yards") => "passing_yards", ("nfl", "rushing yards") => "rushing_yards", ("nfl", "receiving yards") => "receiving_yards",
        ("nfl", "passing touchdowns") => "passing_tds", ("nfl", "touchdowns") => "tds", ("nfl", "receptions") => "receptions",
        ("nfl", "carries") => "carries", ("nfl", "targets") => "targets", ("nfl", "interceptions") => "interceptions",
        ("mlb", "hits") => "h", ("mlb", "home runs") => "hr", ("mlb", "strikeouts") => "k", ("mlb", "rbi") => "rbi", ("mlb", "runs") => "r",
        ("mlb", "walks") => "bb", ("mlb", "stolen bases") => "sb", ("mlb", "total bases") => "tb",
        _ => return None,
    })
}

/// How a log key reads in an answer.
fn stat_label(key: &str) -> &str {
    match key {
        "pts" => "points", "reb" => "rebounds", "ast" => "assists", "stl" => "steals", "blk" => "blocks", "tov" => "turnovers",
        "toi" => "minutes on ice", "passing_yards" => "passing yards", "rushing_yards" => "rushing yards", "receiving_yards" => "receiving yards",
        "passing_tds" => "passing TDs", "tds" => "touchdowns", "h" => "hits", "hr" => "home runs", "k" => "strikeouts", "rbi" => "RBI",
        "r" => "runs", "bb" => "walks", "sb" => "stolen bases", "tb" => "total bases", k => k,
    }
}

fn number_word(w: &str) -> Option<u32> {
    ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen",
     "sixteen", "seventeen", "eighteen", "nineteen", "twenty"].iter().position(|x| *x == w).map(|n| n as u32).or_else(|| w.parse().ok())
}

fn words(l: &str) -> Vec<String> {
    l.split(|c: char| !(c.is_alphanumeric() || c == '-')).filter(|w| !w.is_empty()).map(|w| w.to_string()).collect()
}

/// "last 20 games", "past ten", "last game" (= 1)
fn last_n(ws: &[String]) -> Option<u32> {
    for (i, w) in ws.iter().enumerate() {
        if matches!(w.as_str(), "last" | "past" | "previous" | "recent") {
            if let Some(n) = ws.get(i + 1).and_then(|x| number_word(x)).filter(|n| (1..=200).contains(n)) { return Some(n); }
            if matches!(ws.get(i + 1).map(|s| s.as_str()), Some("game" | "night")) { return Some(1); }
        }
    }
    None
}

/// The season a year in the question means. "2025-26" is a start year; a single year
/// is the season that ended (or was played) in it: the 2025 World Series is MLB 2025,
/// the 2025 NBA title is the 2024-25 season, the 2025 Super Bowl ends the 2024 season,
/// and "the 2025 season" is the season that started in 2025.
pub fn season_of(l: &str, sport: Option<&str>) -> Option<i32> {
    let ws = words(l);
    for (i, w) in ws.iter().enumerate() {
        if let Some((a, b)) = w.split_once('-') {
            if let (Ok(y), true) = (a.parse::<i32>(), b.len() == 2 && b.chars().all(|c| c.is_ascii_digit())) {
                if (1990..=2100).contains(&y) { return Some(y); }
            }
        }
        let Ok(y) = w.parse::<i32>() else { continue };
        if !(1990..=2100).contains(&y) { continue; }
        let says_season = ws.get(i + 1).map(|x| x == "season").unwrap_or(false);
        return Some(match (sport, says_season) { (Some("mlb"), _) | (_, true) => y, _ => y - 1 });
    }
    None
}

fn sport_from(l: &str) -> Option<String> {
    let ws = words(l);
    let w = |keys: &[&str]| ws.iter().any(|x| keys.contains(&x.as_str()));
    if l.contains("super bowl") || w(&["nfl", "football"]) { return Some("nfl".into()); }
    if l.contains("world series") || w(&["mlb", "baseball"]) { return Some("mlb".into()); }
    if l.contains("stanley cup") || w(&["nhl", "hockey"]) { return Some("nhl".into()); }
    if l.contains("nba finals") || w(&["nba", "basketball"]) { return Some("nba".into()); }
    None
}

const BET_WORDS: &[&str] = &["bet", "bets", "betting", "prop", "props", "odds", "line", "lines", "pick", "picks", "parlay", "parlays", "lock", "locks",
    "wager", "edge", "price", "moneyline", "spread", "spreads", "o/u"];

/// A betting question: a betting word, or over/under followed by a number ("over 25.5").
fn is_bets(l: &str) -> bool {
    let ws = words(l);
    ws.iter().any(|w| BET_WORDS.contains(&w.as_str())) || l.contains("over/under")
        || ws.windows(2).any(|p| matches!(p[0].as_str(), "over" | "under") && p[1].chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false))
}

/// Sport, season, window, stats and names of a question (no kind yet).
fn parse(text: &str) -> Ask {
    let l = text.to_lowercase();
    let ws = words(&l);
    let sport = sport_from(&l);
    let stats: Vec<String> = {
        let mut rest = l.clone();
        let mut v = Vec::new();
        for (k, g) in STAT_WORDS {
            if rest.contains(k) && !v.iter().any(|x: &String| x == g) { v.push(g.to_string()); rest = rest.replace(k, " "); }
        }
        v
    };
    Ask { sport: sport.clone(), season: season_of(&l, sport.as_deref()), last: last_n(&ws), this_season: l.contains("this season"), stats, mentions: mentions(text), ..Default::default() }
}

/// What kind of research question this is, if any.
pub fn detect(text: &str) -> Option<Ask> {
    let l = text.to_lowercase();
    let mut ask = parse(text);
    let bets = is_bets(&l);
    if has(&l, &["champion", "championship", "won the title", "won the cup", "title", "super bowl", "world series", "stanley cup", "nba finals", "the finals"])
        && has(&l, &["who won", "winner", "won ", "champion", "win the", "who took"]) && !bets {
        ask.kind = Some(Kind::Champion);
    } else if has(&l, &["standings", "best record", "worst record", "league table", "first place", "top of the league", "leading the league", "atop the"]) && !bets {
        ask.kind = Some(Kind::Standings);
    } else if !bets && !ask.mentions.is_empty() && (!ask.stats.is_empty() || has(&l, &["averag", "per game", "stats", "statistics", "how many"]))
        && (ask.last.is_some() || ask.this_season || ask.season.is_some() || has(&l, &["averag", "per game", "compare", "compared", " vs ", "versus", "career", "so far"])) {
        ask.kind = Some(Kind::Stats);
    } else if !bets && !ask.mentions.is_empty() && (has(&l, &["results", "record against", "head to head", "head-to-head", "how did the", "did the ", "who won the", "last night", "recent games"])
        || (ask.last.is_some() && has(&l, &["games", "game"]))) {
        ask.kind = Some(Kind::Results);
    }
    ask.kind.map(|_| ask)
}

/// A follow-up that changes only the stat or the window ("what about his rebounds?",
/// "and over the last 5?") keeps the previous research question and its entities.
pub fn follow_up(text: &str, prev: &Ask) -> Option<Ask> {
    let l = text.to_lowercase();
    let ws = words(&l);
    if ws.len() > 12 || is_bets(&l) || !matches!(prev.kind, Some(Kind::Stats | Kind::Results)) { return None; }
    let starts = ["what about", "how about", "and ", "only ", "now ", "same ", "over the", "in the", "his ", "her ", "their ", "just "];
    let pronoun = ws.iter().any(|w| matches!(w.as_str(), "his" | "her" | "their" | "he" | "she" | "they" | "them" | "him"));
    let d = parse(text);
    if !(starts.iter().any(|s| l.starts_with(s)) || pronoun) || (d.stats.is_empty() && d.last.is_none() && !d.this_season) { return None; }
    let mut a = prev.clone();
    if !d.stats.is_empty() { a.stats = d.stats; }
    if d.last.is_some() { a.last = d.last; a.this_season = false; a.season = None; }
    if d.this_season { a.this_season = true; a.last = None; }
    a.mentions = d.mentions;
    Some(a)
}

const STOP: &[&str] = &["compare", "break", "down", "preview", "simulate", "run", "list", "find", "look", "get", "check", "how's", "whats", "what's", "how", "who", "what", "which", "show", "tell", "give", "analyze", "analyse", "did", "does", "do", "is", "are", "has", "have",
    "over", "in", "the", "his", "her", "their", "and", "or", "vs", "versus", "with", "against", "between", "of", "for", "to", "a", "an", "i", "me", "my",
    "nba", "nfl", "nhl", "mlb", "super", "bowl", "world", "series", "stanley", "cup", "finals", "championship", "champion", "won", "win", "last", "past",
    "games", "game", "season", "this", "that", "per", "average", "averaged", "averages", "stats", "statistics", "points", "rebounds", "assists",
    "goals", "shots", "hits", "yards", "runs", "home", "away", "record", "standings", "best", "worst", "team", "player", "players", "teams", "many",
    "much", "been", "was", "were", "be", "can", "could", "would", "should", "will", "please", "about", "over", "compared", "from", "since", "so", "far",
    "night", "tonight", "today", "yesterday", "results", "result", "head", "next", "upcoming", "career", "league", "first", "place", "when", "where",
    "why", "latest", "current", "currently", "most", "recent", "versus", "comparing", "also", "now", "just", "only", "same", "than", "more", "less",
    "passing", "rushing", "receiving", "touchdowns", "receptions", "steals", "blocks", "turnovers", "minutes", "threes", "strikeouts", "rbi"];

fn is_stop(w: &str) -> bool { STOP.contains(&w.to_lowercase().as_str()) || w.chars().all(|c| c.is_ascii_digit() || c == '-') }

/// Names in the question: runs of capitalised words ("Nikola Jokic", "Connor McDavid",
/// "Edmonton"), split at "and", "vs", commas; lowercase questions fall back to runs of
/// words that are not common question words ("jokic vs doncic").
pub fn mentions(text: &str) -> Vec<String> {
    let clean: String = text.chars().map(|c| if c.is_alphanumeric() || c == '\'' || c == '.' || c == '-' || c == '’' { c } else { ' ' }).collect();
    let toks: Vec<String> = clean.split_whitespace().map(|t| {
        let t = t.trim_matches(|c: char| c == '.' || c == '\'' || c == '’');
        t.strip_suffix("'s").or_else(|| t.strip_suffix("’s")).unwrap_or(t).to_string()
    }).filter(|t| !t.is_empty()).collect();
    let runs = |pick: &dyn Fn(&str) -> bool| -> Vec<String> {
        let mut out = Vec::new();
        let mut cur: Vec<String> = Vec::new();
        for t in &toks {
            if pick(t) && !is_stop(t) { cur.push(t.clone()); } else if !cur.is_empty() { out.push(cur.join(" ")); cur.clear(); }
        }
        if !cur.is_empty() { out.push(cur.join(" ")); }
        out
    };
    let caps = runs(&|t: &str| t.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) || (t.len() == 2 && t.ends_with('.')));
    let found = if caps.is_empty() { runs(&|t: &str| t.chars().all(|c| c.is_alphabetic() || c == '-' || c == '\'') && t.len() >= 3) } else { caps };
    let mut seen = Vec::new();
    for m in found { if m.len() >= 3 && !seen.contains(&m) { seen.push(m); } }
    seen.truncate(4);
    seen
}

/// Reply to a "which one?" question: an ordinal ("the first one", "2") or a name.
pub fn pick(reply: &str, candidates: &[Value]) -> Option<String> {
    let l = reply.to_lowercase();
    let ws = words(&l);
    let ord = [("first", 0), ("1", 0), ("second", 1), ("2", 1), ("third", 2), ("3", 2), ("fourth", 3), ("4", 3)];
    if ws.len() <= 4 {
        if let Some((_, i)) = ord.iter().find(|(w, _)| ws.iter().any(|x| x == w)) { return candidates.get(*i).and_then(|c| c["entity_id"].as_str()).map(String::from); }
    }
    let n = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    let r = n(reply);
    if r.len() < 3 { return None; }
    let hits: Vec<&Value> = candidates.iter().filter(|c| { let name = n(c["name"].as_str().unwrap_or("")); name.contains(&r) || (r.contains(&name) && !name.is_empty()) }).collect();
    (hits.len() == 1).then(|| hits[0]["entity_id"].as_str().map(String::from)).flatten()
}

pub fn clarify_text(mention: &str, candidates: &[Value]) -> String {
    let opts: Vec<String> = candidates.iter().take(4).map(|c| {
        let team = c["team_id"].as_str().and_then(|t| t.rsplit(':').next()).map(|t| format!(" ({t})")).unwrap_or_default();
        let sport = c["sport"].as_str().map(|s| s.to_uppercase()).unwrap_or_default();
        if c["kind"] == "team" { format!("{} ({sport})", c["name"].as_str().unwrap_or("")) } else { format!("{}{team}", c["name"].as_str().unwrap_or("")) }
    }).collect();
    format!("Which {mention} do you mean: {}? Reply with the name (or \"the first one\").", join_or(&opts))
}

fn join_or(v: &[String]) -> String {
    match v.len() { 0 => String::new(), 1 => v[0].clone(), n => format!("{} or {}", v[..n - 1].join(", "), v[n - 1]) }
}

fn current_season(sport: &str) -> i32 {
    use chrono::Datelike;
    let t = chrono::Utc::now();
    let (y, m) = (t.year(), t.month());
    match sport { "nba" | "nhl" => if m >= 9 { y } else { y - 1 }, "nfl" => if m >= 8 { y } else { y - 1 }, _ => y }
}

/// A per-game average as shown: one decimal, two below 1 (0.35 goals, 23.4 points).
pub fn avg_shown(x: f64) -> f64 { if x.abs() < 1.0 { (x * 100.0).round() / 100.0 } else { (x * 10.0).round() / 10.0 } }

/// Simulation requests: the chat cannot run the simulators yet (Phase 6), so it says so
/// instead of letting the model describe a run that never happened.
pub fn is_sim_request(text: &str) -> bool {
    let l = text.to_lowercase();
    (l.contains("simulat") || l.contains("monte carlo")) && !l.contains("how does") && !l.contains("what is") && !l.contains("explain")
}

pub const SIM_REPLY: &str = "I can't run simulations from chat yet, so I haven't simulated anything here. The game simulators exist (NBA possession, NFL play-by-play, NHL event and MLB plate-appearance engines) and you can run a game on the Broadcast Sim page; running one from chat, with the seed, model version and inputs recorded, is the next MonsterGPT step. I can give you the model's current numbers for a game instead: ask me to break down that game.";

fn fmt_num(x: f64) -> String { if (x - x.round()).abs() < 1e-9 { format!("{x:.0}") } else if (x * 10.0 - (x * 10.0).round()).abs() < 1e-9 { format!("{x:.1}") } else { format!("{x:.2}") } }

/// What a research turn produced.
pub struct Outcome {
    pub reply: String,
    pub calls: Vec<ToolCall>,
    /// saved on the message: the question, the entities it used, a pending clarification
    pub state: Value,
    pub entities: Vec<Value>,
}

/// Answer a research question. `focus` are the entities of the conversation so far;
/// `forced` maps a mention to the entity the user picked after a clarification.
pub async fn answer(tools: &Tools, ask: &Ask, focus: &[String], forced: &HashMap<String, String>, original: &str) -> Outcome {
    let mut calls: Vec<ToolCall> = Vec::new();
    let mut entities: Vec<Value> = Vec::new();
    let kind = ask.kind.unwrap_or(Kind::Stats);
    let base_state = |ents: &[Value]| json!({ "research": ask, "entities": ents.iter().filter_map(|e| e["entity_id"].as_str()).collect::<Vec<_>>() });

    // 1. names -> entities (standings and champions need none)
    if matches!(kind, Kind::Stats | Kind::Results) {
        if ask.mentions.is_empty() {
            for id in focus { entities.push(json!({ "entity_id": id, "name": Value::Null, "from": "conversation" })); }
        }
        for m in &ask.mentions {
            if let Some(id) = forced.get(m) { entities.push(json!({ "entity_id": id, "mention": m, "from": "your choice" })); continue; }
            let c = tools.call(Tool::Resolve { q: m.clone(), sport: ask.sport.clone(), kind: None, focus: focus.to_vec() }).await;
            let v = c.value.clone();
            calls.push(c);
            match v["status"].as_str() {
                Some("resolved") => { let mut e = v["entity"].clone(); e["mention"] = json!(m); entities.push(e); }
                Some("ambiguous") => {
                    let cands = v["candidates"].as_array().cloned().unwrap_or_default();
                    return Outcome { reply: clarify_text(m, &cands), calls, entities,
                        state: json!({ "research": ask, "clarify": { "text": original, "mention": m, "candidates": cands, "forced": forced } }) };
                }
                _ => {}
            }
        }
        if entities.is_empty() {
            let names = if ask.mentions.is_empty() { "a player or team".to_string() } else { ask.mentions.iter().map(|m| format!("\"{m}\"")).collect::<Vec<_>>().join(", ") };
            return Outcome { reply: format!("I can't verify that from Monster OS's records. I looked for {names} among NBA, NFL, NHL and MLB players and teams and found no match, and I don't hold other records that would answer it. If you meant a player or team, try the full name."),
                calls, state: base_state(&entities), entities };
        }
    }
    let sport = ask.sport.clone().or_else(|| entities.iter().find_map(|e| e["entity_id"].as_str().and_then(|i| i.split(':').nth(1)).map(String::from)));

    // 2. the records
    let reply = match kind {
        Kind::Standings | Kind::Champion => {
            let Some(sp) = sport.clone() else {
                return Outcome { reply: format!("Which league: NBA, NFL, NHL or MLB? I keep {} for those four.", if kind == Kind::Standings { "standings" } else { "champions" }),
                    calls, state: json!({ "research": ask }), entities };
            };
            let tool = if kind == Kind::Standings { Tool::Standings { sport: sp.clone(), season: ask.season } } else { Tool::Champions { sport: sp.clone(), season: ask.season, limit: 1 } };
            let c = tools.call(tool).await;
            let r = if !c.ok { format!("I couldn't load the {} records from projection-service ({}), so I can't answer that right now.", sp.to_uppercase(), c.error.clone().unwrap_or_default()) }
                else if kind == Kind::Standings { standings_reply(&c.value, original) } else { champion_reply(&c.value, ask.season) };
            calls.push(c);
            r
        }
        Kind::Stats | Kind::Results => {
            let mut parts = Vec::new();
            let mut player_rows: Vec<(String, String, f64)> = Vec::new(); // (name, stat label, mean) for a comparison line
            for e in &entities {
                let id = e["entity_id"].as_str().unwrap_or("").to_string();
                let sp = id.split(':').nth(1).unwrap_or("").to_string();
                let season = if ask.this_season { Some(current_season(&sp)) } else { ask.season };
                if id.starts_with("pl:") {
                    let keys: Vec<String> = ask.stats.iter().filter_map(|w| stat_key(&sp, w)).map(String::from).collect();
                    let c = tools.call(Tool::PlayerStats { entity: id.clone(), stats: keys, last: ask.last.unwrap_or(10), season }).await;
                    if c.ok { let (line, rows) = player_reply(&c.value, &ask.stats); parts.push(line); player_rows.extend(rows); }
                    else { parts.push(format!("I couldn't load the games for {id} ({}).", c.error.clone().unwrap_or_default())); }
                    calls.push(c);
                } else if id.starts_with("tm:") {
                    let opponent = entities.iter().filter_map(|x| x["entity_id"].as_str()).find(|x| x.starts_with("tm:") && *x != id).map(String::from)
                        .filter(|_| has(&original.to_lowercase(), &[" vs ", " versus ", "against", "head to head", "head-to-head"]));
                    let c = tools.call(Tool::TeamResults { entity: id.clone(), opponent: opponent.clone(), last: ask.last.unwrap_or(if opponent.is_some() { 10 } else { 5 }), season }).await;
                    if c.ok { parts.push(results_reply(&c.value)); } else { parts.push(format!("I couldn't load the results for {id} ({}).", c.error.clone().unwrap_or_default())); }
                    calls.push(c);
                    if opponent.is_some() { break; }
                }
            }
            if let [a, b] = player_rows.as_slice() {
                if a.1 == b.1 && (a.2 - b.2).abs() > 1e-9 {
                    let (hi, lo) = if a.2 > b.2 { (a, b) } else { (b, a) };
                    // from the averages as shown, so the sentence agrees with the lines above it
                    parts.push(format!("{} averaged {} more {} per game than {} over those games.", hi.0, fmt_num(avg_shown(hi.2 - lo.2)), hi.1, lo.0));
                }
            }
            parts.join("\n\n")
        }
    };
    let ents: Vec<Value> = entities.clone();
    Outcome { reply, calls, state: base_state(&ents), entities }
}

/// A matchup question whose game is not on today's slate: find the team's next game and
/// say when the model's numbers will exist, instead of letting the model fill the gap.
pub async fn next_game(tools: &Tools, text: &str, sport: Option<String>, focus: &[String]) -> Option<Outcome> {
    let mut calls = Vec::new();
    let mut team: Option<String> = None;
    for m in mentions(text) {
        let c = tools.call(Tool::Resolve { q: m, sport: sport.clone(), kind: Some("team".into()), focus: focus.to_vec() }).await;
        let id = (c.value["status"] == "resolved").then(|| c.value["entity"]["entity_id"].as_str().map(String::from)).flatten();
        calls.push(c);
        if id.is_some() { team = id; break; }
    }
    let id = team?;
    let c = tools.call(Tool::TeamSchedule { entity: id.clone(), next: 1 }).await;
    let v = c.value.clone();
    calls.push(c);
    let name = v["name"].as_str().unwrap_or("That team").to_string();
    let reply = match v["upcoming"].as_array().and_then(|a| a.first()) {
        Some(g) => format!("The {name}{} next game is {} the {} on {}. Monster OS builds game briefs (model view, best lines, injuries, matchup ranks) for the day's slate, \
            so this game's numbers aren't ready yet; ask again on game day for the breakdown.\n\n{}",
            if name.ends_with('s') { "'" } else { "'s" }, if g["home"].as_bool().unwrap_or(false) { "at home against" } else { "on the road at" }, g["opponent_name"].as_str().or(g["opponent"].as_str()).unwrap_or(""),
            g["date"].as_str().unwrap_or("?"), source_line(&v)),
        None => format!("I don't have a scheduled game stored for the {name}, so I can't break one down. {}", source_line(&v)),
    };
    let entities = vec![json!({ "entity_id": id, "name": name })];
    Some(Outcome { reply, calls, state: json!({ "entities": [id] }), entities })
}

fn source_line(v: &Value) -> String {
    format!("Source: {} ({}).", v["source"]["table"].as_str().unwrap_or("stored records"), v["source"]["source_id"].as_str().unwrap_or("Monster OS"))
}

pub fn standings_reply(v: &Value, question: &str) -> String {
    let rows: Vec<&Value> = v["standings"].as_array().into_iter().flatten().collect();
    let sport = v["sport"].as_str().unwrap_or("");
    if rows.is_empty() { return format!("I have no final scores stored for that {} season, so I can't give standings.", sport.to_uppercase()); }
    let nhl = sport == "nhl";
    let line = |i: usize, r: &Value| {
        let name = r["name"].as_str().or(r["team"].as_str()).unwrap_or("");
        if nhl { format!("{}. {name}: {} pts ({}-{}-{})", i + 1, r["pts"], r["w"], r["l"], r["otl"]) }
        else if sport == "nfl" && r["t"].as_u64().unwrap_or(0) > 0 { format!("{}. {name}: {}-{}-{}", i + 1, r["w"], r["l"], r["t"]) }
        else { format!("{}. {name}: {}-{}", i + 1, r["w"], r["l"]) }
    };
    let top = &rows[0];
    let key = |r: &Value| if nhl { r["pts"].to_string() } else { format!("{}-{}", r["w"], r["l"]) };
    let leaders: Vec<String> = rows.iter().take_while(|r| key(r) == key(top)).map(|r| r["name"].as_str().or(r["team"].as_str()).unwrap_or("").to_string()).collect();
    let lead = if nhl { format!("{} lead{} the NHL with {} points.", join_and(&leaders), if leaders.len() == 1 { "s" } else { "" }, top["pts"]) }
        else { format!("{} {} the best record at {}.", join_and(&leaders), if leaders.len() == 1 { "has" } else { "share" }, key(top)) };
    let n = if question.to_lowercase().contains("full") || question.to_lowercase().contains("all ") { rows.len() } else { 10.min(rows.len()) };
    let table: Vec<String> = rows.iter().take(n).enumerate().map(|(i, r)| line(i, r)).collect();
    format!("{lead}\n\n{} {} regular season, through {} ({} games), whole league ranked by {}; no division or conference data is stored:\n{}\n\n{}",
        sport.to_uppercase(), v["season_label"].as_str().unwrap_or(""), v["as_of"].as_str().unwrap_or("?"), v["games"], v["ranking"].as_str().unwrap_or(""),
        table.join("\n"), source_line(v))
}

fn join_and(v: &[String]) -> String {
    match v.len() { 0 => String::new(), 1 => v[0].clone(), n => format!("{} and {}", v[..n - 1].join(", "), v[n - 1]) }
}

pub fn champion_reply(v: &Value, season: Option<i32>) -> String {
    let event = v["event"].as_str().unwrap_or("title");
    let Some(c) = v["champions"].as_array().and_then(|a| a.first()) else {
        return match season {
            Some(s) => format!("I don't have a completed {} {event} for the season starting in {s} in Monster OS's records, so I can't say who won it.", v["sport"].as_str().unwrap_or("").to_uppercase()),
            None => format!("I don't have a completed {event} in Monster OS's records."),
        };
    };
    let name = |k: &str, code: &str| c[k].as_str().or(c[code].as_str()).unwrap_or("").to_string();
    let (champ, runner) = (name("champion_name", "champion"), name("runner_up_name", "runner_up"));
    let g = &c["final_game"];
    let champ_home = g["home"] == c["champion"];
    let (cs, rs) = if champ_home { (&g["home_score"], &g["away_score"]) } else { (&g["away_score"], &g["home_score"]) };
    let series = if c["series"].is_null() { String::new() } else { format!(" {}-{}", c["series"]["champion_wins"], c["series"]["runner_up_wins"]) };
    let when = if v["sport"] == "nfl" { format!("the {} {event} (ending the {} season)", g["date"].as_str().map(|d| &d[..4]).unwrap_or(""), c["season_label"].as_str().unwrap_or("")) }
        else { format!("the {} {event}", c["season_label"].as_str().unwrap_or("")) };
    format!("The {champ} won {when}, beating the {runner}{series}. The deciding game was on {}: {champ} {cs}, {runner} {rs}.\n\n{}",
        g["date"].as_str().unwrap_or("?"), source_line(v))
}

pub fn player_reply(v: &Value, asked: &[String]) -> (String, Vec<(String, String, f64)>) {
    let name = v["name"].as_str().unwrap_or("This player").to_string();
    let team = v["team_id"].as_str().and_then(|t| t.rsplit(':').next()).map(|t| format!(" ({t})")).unwrap_or_default();
    let n = v["games_found"].as_u64().unwrap_or(0);
    if n == 0 { return (format!("I have no stored games for {name}{team} in that window."), vec![]); }
    let sum = v["summary"].as_object().cloned().unwrap_or_default();
    let mut rows = Vec::new();
    let stats: Vec<String> = sum.iter().map(|(k, s)| {
        let mean = avg_shown(s["mean"].as_f64().unwrap_or(0.0));
        rows.push((name.clone(), stat_label(k).to_string(), mean));
        format!("{} {} (median {}, range {}-{})", fmt_num(mean), stat_label(k), fmt_num(s["median"].as_f64().unwrap_or(0.0)), fmt_num(s["min"].as_f64().unwrap_or(0.0)), fmt_num(s["max"].as_f64().unwrap_or(0.0)))
    }).collect();
    let span = match (v["first_date"].as_str(), v["last_date"].as_str()) {
        (Some(a), Some(b)) => format!(", {a} to {b}"),
        _ => v["games"].as_array().and_then(|g| g.first()).map(|g| format!(", latest season {} week {}", g["season"], g["week"].as_str().unwrap_or("?"))).unwrap_or_default(),
    };
    let window = v["window"].as_str().unwrap_or("");
    let short = if window.starts_with("last ") && window.split_whitespace().nth(1).and_then(|x| x.parse::<u64>().ok()).map(|w| w > n).unwrap_or(false) { format!(" (only {n} stored)") } else { String::new() };
    let missing: Vec<String> = asked.iter().filter(|w| stat_key(v["sport"].as_str().unwrap_or(""), w).is_none()).cloned().collect();
    let miss = if missing.is_empty() { String::new() } else { format!(" Monster OS doesn't log {} for {}.", missing.join(", "), v["sport"].as_str().unwrap_or("").to_uppercase()) };
    (format!("{name}{team}, {window}{short} ({}{span}): {} per game.{miss} {}", v["game_types"].as_str().unwrap_or(""), stats.join("; "), source_line(v)), rows)
}

pub fn results_reply(v: &Value) -> String {
    let name = v["name"].as_str().or(v["entity"].as_str()).unwrap_or("").to_string();
    let games: Vec<&Value> = v["games"].as_array().into_iter().flatten().collect();
    if games.is_empty() { return format!("I have no stored results for {name} in that window."); }
    let line = |g: &Value| format!("{} {} {} {} {}{}", g["date"].as_str().unwrap_or(""), g["result"].as_str().unwrap_or(""), g["score"].as_str().unwrap_or(""),
        if g["home"].as_bool().unwrap_or(false) { "vs" } else { "at" }, g["opponent_name"].as_str().or(g["opponent"].as_str()).unwrap_or(""),
        if g["game_type"].as_str().map(|t| t != "REG").unwrap_or(false) { format!(" ({})", g["game_type"].as_str().unwrap_or("")) } else { String::new() });
    if games.len() == 1 {
        let g = games[0];
        let verb = match g["result"].as_str() { Some("W") => "beat", Some("L") => "lost to", _ => "tied" };
        return format!("The {name} {verb} the {} {} on {}{}.\n\n{}", g["opponent_name"].as_str().or(g["opponent"].as_str()).unwrap_or(""), g["score"].as_str().unwrap_or(""),
            g["date"].as_str().unwrap_or(""), if g["home"].as_bool().unwrap_or(false) { " at home" } else { " on the road" }, source_line(v));
    }
    let r = &v["record"];
    let t = if r["t"].as_u64().unwrap_or(0) > 0 { format!("-{}", r["t"]) } else { String::new() };
    format!("{name}, last {} games: {}-{}{t}, averaging {} scored and {} allowed.\n{}\n\n{}", games.len(), r["w"], r["l"],
        fmt_num(v["avg_points_for"].as_f64().unwrap_or(0.0)), fmt_num(v["avg_points_against"].as_f64().unwrap_or(0.0)),
        games.iter().map(|g| line(g)).collect::<Vec<_>>().join("\n"), source_line(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_four_research_kinds_and_leaves_bets_alone() {
        let a = detect("Who won the 2025 World Series?").unwrap();
        assert_eq!((a.kind, a.sport.as_deref(), a.season), (Some(Kind::Champion), Some("mlb"), Some(2025)));
        let a = detect("Who won the most recent NBA championship?").unwrap();
        assert_eq!((a.kind, a.sport.as_deref(), a.season), (Some(Kind::Champion), Some("nba"), None));
        assert_eq!(detect("Who won the 2025 NBA championship?").unwrap().season, Some(2024), "the 2025 title ends the 2024-25 season");
        assert_eq!(detect("Who won the 2025 Super Bowl?").unwrap().season, Some(2024));
        assert_eq!(detect("What are the latest NBA standings? Who has the best record?").unwrap().kind, Some(Kind::Standings));
        let a = detect("Compare Nikola Jokic and Luka Doncic in points over their last 20 games.").unwrap();
        assert_eq!((a.kind, a.last, a.mentions.clone(), a.stats.clone()), (Some(Kind::Stats), Some(20), vec!["Nikola Jokic".to_string(), "Luka Doncic".to_string()], vec!["points".to_string()]));
        let a = detect("How many points per game has Connor McDavid averaged over his last 10 games compared with Nathan MacKinnon?").unwrap();
        assert_eq!((a.kind, a.last, a.mentions.len()), (Some(Kind::Stats), Some(10), 2));
        let a = detect("Who won the Lakers game last night?").unwrap();
        assert_eq!((a.kind, a.last, a.mentions.clone()), (Some(Kind::Results), Some(1), vec!["Lakers".to_string()]));
        assert!(detect("best NBA points props tonight").is_none(), "a bets question stays with the bet card");
        assert!(detect("Jokic over 25.5 points over his last 10 games?").is_none(), "over a number is a bet");
        assert!(detect("Explain how no-vig fair odds are calculated").is_none());
        assert!(detect("How reliable is the NBA player points model?").is_none());
    }

    #[test]
    fn names_are_found_in_capitalised_and_lowercase_questions() {
        assert_eq!(mentions("How has LeBron James played over his last 5?"), vec!["LeBron James"]);
        assert_eq!(mentions("jokic vs doncic last 10 games points"), vec!["jokic", "doncic"]);
        assert_eq!(mentions("Break down Edmonton's next game."), vec!["Edmonton"]);
        assert_eq!(mentions("Compare Shai Gilgeous-Alexander and Jalen Brunson"), vec!["Shai Gilgeous-Alexander", "Jalen Brunson"]);
        assert!(mentions("What are the latest NBA standings?").is_empty());
    }

    #[test]
    fn stat_words_map_per_sport() {
        assert_eq!(stat_key("nba", "points"), Some("pts"));
        assert_eq!(stat_key("nhl", "points"), Some("points"));
        assert_eq!(stat_key("mlb", "hits"), Some("h"));
        assert_eq!(stat_key("nhl", "hits"), Some("hits"));
        assert_eq!(stat_key("nfl", "points"), None);
        assert_eq!(detect("Mahomes passing yards over his last 5 games").unwrap().stats, vec!["passing yards"]);
    }

    #[test]
    fn follow_ups_keep_the_players_and_change_the_stat_or_window() {
        let first = detect("Compare Nikola Jokic and Luka Doncic in points over their last 20 games").unwrap();
        let f = follow_up("what about rebounds?", &first).unwrap();
        assert_eq!((f.stats.clone(), f.last, f.mentions.len()), (vec!["rebounds".to_string()], Some(20), 0));
        let f = follow_up("and over the last 5?", &first).unwrap();
        assert_eq!((f.stats.clone(), f.last), (vec!["points".to_string()], Some(5)));
        assert!(follow_up("best NBA bets tonight", &first).is_none());
        assert!(follow_up("thanks", &first).is_none());
    }

    #[test]
    fn a_clarification_reply_picks_one_candidate_or_none() {
        let c = vec![json!({ "entity_id": "pl:nba:1", "name": "LeBron James", "kind": "player", "team_id": "tm:nba:PHI", "sport": "nba" }),
                     json!({ "entity_id": "pl:nba:2", "name": "Jalen James", "kind": "player", "team_id": null, "sport": "nba" })];
        assert_eq!(pick("LeBron", &c).as_deref(), Some("pl:nba:1"));
        assert_eq!(pick("the second one", &c).as_deref(), Some("pl:nba:2"));
        assert_eq!(pick("James", &c), None, "still ambiguous");
        assert_eq!(clarify_text("James", &c), "Which James do you mean: LeBron James (PHI) or Jalen James? Reply with the name (or \"the first one\").");
    }

    #[test]
    fn replies_use_only_the_numbers_the_tools_returned() {
        let s = json!({ "sport": "nba", "season_label": "2026-27", "as_of": "2026-11-02", "games": 107, "ranking": "win percentage",
            "standings": [{ "team": "HOU", "name": "Houston Rockets", "w": 7, "l": 1 }, { "team": "PHI", "name": "Philadelphia 76ers", "w": 7, "l": 1 }, { "team": "DET", "name": "Detroit Pistons", "w": 6, "l": 1 }],
            "source": { "table": "sports_games", "source_id": "espn_site" } });
        let r = standings_reply(&s, "who has the best record?");
        assert!(r.starts_with("Houston Rockets and Philadelphia 76ers share the best record at 7-1."), "{r}");
        let c = json!({ "sport": "mlb", "event": "World Series", "source": { "table": "sports_games", "source_id": "mlb_statsapi" }, "champions": [{ "season_label": "2025",
            "champion": "LAD", "champion_name": "Los Angeles Dodgers", "runner_up": "TOR", "runner_up_name": "Toronto Blue Jays",
            "final_game": { "date": "2025-11-01", "home": "TOR", "away": "LAD", "home_score": 4, "away_score": 5 }, "series": { "champion_wins": 4, "runner_up_wins": 3 } }] });
        assert_eq!(champion_reply(&c, Some(2025)).lines().next().unwrap(),
            "The Los Angeles Dodgers won the 2025 World Series, beating the Toronto Blue Jays 4-3. The deciding game was on 2025-11-01: Los Angeles Dodgers 5, Toronto Blue Jays 4.");
        assert!(champion_reply(&json!({ "sport": "nba", "event": "NBA Finals", "champions": [] }), Some(1950)).contains("don't have"));
        let p = json!({ "name": "Nikola Jokic", "team_id": "tm:nba:DEN", "sport": "nba", "window": "last 20 games", "games_found": 20, "game_types": "regular season only",
            "first_date": "2026-03-12", "last_date": "2026-11-02", "summary": { "pts": { "mean": 23.4, "median": 23.0, "min": 8.0, "max": 40.0 } }, "source": { "table": "player_game_logs", "source_id": "espn_site" } });
        let (line, rows) = player_reply(&p, &["points".into()]);
        assert!(line.starts_with("Nikola Jokic (DEN), last 20 games (regular season only, 2026-03-12 to 2026-11-02): 23.4 points (median 23, range 8-40) per game."), "{line}");
        assert_eq!(rows, vec![("Nikola Jokic".to_string(), "points".to_string(), 23.4)]);
        assert_eq!((avg_shown(7.65), avg_shown(0.354), avg_shown(23.0)), (7.7, 0.35, 23.0));
        assert!(is_sim_request("Run 100,000 simulations of the next Lakers game."));
        assert!(!is_sim_request("How does the NHL simulator handle empty nets?"));
    }
}
