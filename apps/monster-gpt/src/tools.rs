//! Typed, read-only tool contracts over existing Monster OS services. The
//! orchestrator can call only what is listed here; every call has validated
//! arguments, a timeout, a source record (what was read, from where, when)
//! and an audit row. Nothing here places bets or writes to another service.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

pub const SPORTS: &[&str] = &["nba", "nfl", "nhl", "mlb"];

#[derive(Clone, Debug, Serialize)]
pub struct Source {
    /// short id the answer cites, e.g. "S1"
    pub id: String,
    pub tool: String,
    pub label: String,
    pub url: String,
    pub fetched_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub tool: &'static str,
    pub args: Value,
    pub ok: bool,
    pub ms: u64,
    pub error: Option<String>,
    pub value: Value,
    pub source: Option<Source>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Tool {
    /// open AI picks (graded, priced, staked) from projection-service
    PicksToday { sport: Option<String> },
    /// settled-pick track record
    PicksRecord { sport: Option<String>, days: u32 },
    /// briefs for games a question names (team names, cities, codes)
    HubBrief { q: String, sport: Option<String> },
    /// a sport's schedule for today
    HubSlate { sport: String },
    /// validation scorecard: calibration, Brier vs baseline and market, ROI, CLV,
    /// drawdown and status per engine x market, plus quarantined odds feeds
    Validation { sport: Option<String> },
    /// league table from final scores (knowledge.rs)
    Standings { sport: String, season: Option<i32> },
    /// each season's champion: the last postseason game and the final series
    Champions { sport: String, season: Option<i32>, limit: u32 },
    /// one player's recent games and averages
    PlayerStats { entity: String, stats: Vec<String>, last: u32, season: Option<i32> },
    /// one team's recent results, optionally against one opponent
    TeamResults { entity: String, opponent: Option<String>, last: u32, season: Option<i32> },
    /// one team's next scheduled games
    TeamSchedule { entity: String, next: u32 },
    /// a game simulator run (projection-service /{sport}/simulate/{game}); `out` rules players out
    SimRun { sport: String, game_id: String, n: u32, seed: u64, out: Vec<String> },
    /// a name -> canonical entity, or the candidates when it is ambiguous
    Resolve { q: String, sport: Option<String>, kind: Option<String>, focus: Vec<String> },
}

fn entity_ok(id: &str, prefix: &str) -> bool {
    let p: Vec<&str> = id.split(':').collect();
    p.len() == 3 && p[0] == prefix && SPORTS.contains(&p[1]) && !p[2].is_empty() && p[2].len() <= 24
        && p[2].chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

impl Tool {
    pub fn name(&self) -> &'static str {
        match self { Tool::PicksToday { .. } => "picks.today", Tool::PicksRecord { .. } => "picks.record", Tool::HubBrief { .. } => "hub.brief", Tool::HubSlate { .. } => "hub.slate", Tool::Validation { .. } => "model.validation",
            Tool::Standings { .. } => "kb.standings", Tool::Champions { .. } => "kb.champions", Tool::PlayerStats { .. } => "kb.player_stats",
            Tool::TeamResults { .. } => "kb.results", Tool::TeamSchedule { .. } => "kb.schedule", Tool::SimRun { .. } => "sim.run", Tool::Resolve { .. } => "entities.resolve" }
    }

    /// Argument checks: known sports only, bounded free text.
    pub fn validate(&self) -> Result<(), String> {
        let sport_ok = |s: &Option<String>| s.as_deref().map(|x| SPORTS.contains(&x)).unwrap_or(true);
        match self {
            Tool::PicksToday { sport } | Tool::PicksRecord { sport, .. } | Tool::Validation { sport } if !sport_ok(sport) => Err("unknown sport".into()),
            Tool::PicksRecord { days, .. } if *days == 0 || *days > 365 => Err("days must be 1-365".into()),
            Tool::HubBrief { q, sport } => {
                if q.trim().is_empty() || q.len() > 400 { return Err("q must be 1-400 characters".into()); }
                if !sport_ok(sport) { return Err("unknown sport".into()); }
                Ok(())
            }
            Tool::HubSlate { sport } | Tool::Standings { sport, .. } | Tool::Champions { sport, .. } if !SPORTS.contains(&sport.as_str()) => Err("unknown sport".into()),
            Tool::Standings { season: Some(y), .. } | Tool::Champions { season: Some(y), .. } if !(1990..=2100).contains(y) => Err("season out of range".into()),
            Tool::Champions { limit, .. } if *limit == 0 || *limit > 30 => Err("limit must be 1-30".into()),
            Tool::PlayerStats { entity, stats, last, .. } => {
                if !entity_ok(entity, "pl") { return Err("entity must be pl:<sport>:<id>".into()); }
                if stats.len() > 8 || stats.iter().any(|x| x.is_empty() || x.len() > 30 || !x.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) { return Err("bad stat names".into()); }
                if *last == 0 || *last > 200 { return Err("last must be 1-200".into()); }
                Ok(())
            }
            Tool::TeamResults { entity, opponent, last, .. } => {
                if !entity_ok(entity, "tm") || opponent.as_deref().map(|o| !entity_ok(o, "tm")).unwrap_or(false) { return Err("entity must be tm:<sport>:<code>".into()); }
                if *last == 0 || *last > 200 { return Err("last must be 1-200".into()); }
                Ok(())
            }
            Tool::SimRun { sport, game_id, n, out, .. } => {
                if !SPORTS.contains(&sport.as_str()) { return Err("unknown sport".into()); }
                if game_id.is_empty() || game_id.len() > 32 || !game_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { return Err("bad game id".into()); }
                if !(crate::sim::MIN_N..=crate::sim::MAX_N).contains(n) { return Err("n must be 100,000-1,000,000".into()); }
                if out.len() > 6 || out.iter().any(|o| o.is_empty() || o.len() > 20 || !o.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')) { return Err("bad player ids".into()); }
                Ok(())
            }
            Tool::TeamSchedule { entity, next } => {
                if !entity_ok(entity, "tm") { return Err("entity must be tm:<sport>:<code>".into()); }
                if *next == 0 || *next > 20 { return Err("next must be 1-20".into()); }
                Ok(())
            }
            Tool::Resolve { q, sport, kind, focus } => {
                if q.trim().is_empty() || q.len() > 60 { return Err("q must be 1-60 characters".into()); }
                if !sport_ok(sport) { return Err("unknown sport".into()); }
                if kind.as_deref().map(|k| !matches!(k, "player" | "team")).unwrap_or(false) { return Err("kind must be player or team".into()); }
                if focus.len() > 10 || focus.iter().any(|f| !(entity_ok(f, "pl") || entity_ok(f, "tm"))) { return Err("bad focus ids".into()); }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn timeout(&self) -> Duration {
        match self { Tool::SimRun { .. } => Duration::from_secs(150), Tool::HubBrief { .. } => Duration::from_secs(25), Tool::HubSlate { .. } => Duration::from_secs(20), Tool::Validation { .. } => Duration::from_secs(30), _ => Duration::from_secs(12) }
    }

    pub(crate) fn url(&self, base: &str) -> String {
        match self {
            Tool::PicksToday { sport } => format!("{base}/picks?status=open&sort=edge&limit=200{}", sport.as_deref().map(|s| format!("&sport={s}")).unwrap_or_default()),
            Tool::PicksRecord { sport, days } => format!("{base}/picks/record?days={days}{}", sport.as_deref().map(|s| format!("&sport={s}")).unwrap_or_default()),
            Tool::HubBrief { q, sport } => format!("{base}/hub/brief?q={}{}", crate::urlencode(q), sport.as_deref().map(|s| format!("&sport={s}")).unwrap_or_default()),
            Tool::HubSlate { sport } => format!("{base}/hub/{sport}/schedule"),
            Tool::Validation { sport } => format!("{base}/validation/scorecard{}", sport.as_deref().map(|s| format!("?sport={s}")).unwrap_or_default()),
            Tool::Standings { sport, season } => format!("{base}/kb/standings?sport={sport}{}", season.map(|y| format!("&season={y}")).unwrap_or_default()),
            Tool::Champions { sport, season, limit } => format!("{base}/kb/champions?sport={sport}&limit={limit}{}", season.map(|y| format!("&season={y}")).unwrap_or_default()),
            Tool::PlayerStats { entity, stats, last, season } => format!("{base}/kb/player_stats?entity={}&last={last}{}{}", crate::urlencode(entity),
                if stats.is_empty() { String::new() } else { format!("&stats={}", stats.join(",")) }, season.map(|y| format!("&season={y}")).unwrap_or_default()),
            Tool::TeamResults { entity, opponent, last, season } => format!("{base}/kb/results?entity={}&last={last}{}{}", crate::urlencode(entity),
                opponent.as_deref().map(|o| format!("&opponent={}", crate::urlencode(o))).unwrap_or_default(), season.map(|y| format!("&season={y}")).unwrap_or_default()),
            Tool::TeamSchedule { entity, next } => format!("{base}/kb/results?entity={}&last={next}&upcoming=true", crate::urlencode(entity)),
            Tool::SimRun { sport, game_id, n, seed, out } => format!("{base}/{sport}/simulate/{game_id}?n={n}&seed={seed}{}",
                if out.is_empty() { String::new() } else { format!("&out={}", out.join(",")) }),
            Tool::Resolve { q, sport, kind, focus } => format!("{base}/entities/resolve?q={}{}{}{}", crate::urlencode(q), sport.as_deref().map(|s| format!("&sport={s}")).unwrap_or_default(),
                kind.as_deref().map(|k| format!("&kind={k}")).unwrap_or_default(), if focus.is_empty() { String::new() } else { format!("&focus={}", crate::urlencode(&focus.join(","))) }),
        }
    }

    pub(crate) fn label(&self) -> String {
        match self {
            Tool::PicksToday { sport } => format!("AI Picks: open {} picks", sport.as_deref().map(|s| s.to_uppercase()).unwrap_or_else(|| "all-sport".into())),
            Tool::PicksRecord { sport, days } => format!("AI Picks track record, last {days} days{}", sport.as_deref().map(|s| format!(" ({})", s.to_uppercase())).unwrap_or_default()),
            Tool::HubBrief { .. } => "Games hub: game briefs (model, best lines, injuries, matchup ranks)".into(),
            Tool::HubSlate { sport } => format!("Games hub: {} schedule", sport.to_uppercase()),
            Tool::Validation { sport } => format!("Validation engine: model scorecard{}", sport.as_deref().map(|s| format!(" ({})", s.to_uppercase())).unwrap_or_default()),
            Tool::Standings { sport, .. } => format!("{} standings computed from stored final scores", sport.to_uppercase()),
            Tool::Champions { sport, .. } => format!("{} champions from stored postseason games", sport.to_uppercase()),
            Tool::PlayerStats { entity, .. } => format!("Player game logs ({entity})"),
            Tool::TeamResults { entity, .. } => format!("Team results ({entity})"),
            Tool::TeamSchedule { entity, .. } => format!("Team schedule ({entity})"),
            Tool::SimRun { sport, game_id, n, seed, out } => format!("{} simulator: game {game_id}, {} runs, seed {seed}{}", sport.to_uppercase(), crate::sim::fmt_count(*n as u64),
                if out.is_empty() { String::new() } else { format!(", ruled out: {}", out.join(", ")) }),
            Tool::Resolve { q, .. } => format!("Entity lookup: \"{q}\""),
        }
    }
}

pub struct Tools {
    pub http: reqwest::Client,
    pub projection_url: String,
    /// url -> (when, fetched_at, body). Picks and briefs change every few
    /// minutes, so a follow-up within CACHE_TTL reuses the last fetch.
    pub cache: std::sync::Mutex<std::collections::HashMap<String, (Instant, DateTime<Utc>, Value)>>,
}

const CACHE_TTL: Duration = Duration::from_secs(30);

impl Tools {
    pub async fn call(&self, tool: Tool) -> ToolCall {
        let t0 = Instant::now();
        let name = tool.name();
        let args = match &tool {
            Tool::PicksToday { sport } => json!({ "sport": sport }),
            Tool::PicksRecord { sport, days } => json!({ "sport": sport, "days": days }),
            Tool::HubBrief { q, sport } => json!({ "q": q, "sport": sport }),
            Tool::HubSlate { sport } => json!({ "sport": sport }),
            Tool::Validation { sport } => json!({ "sport": sport }),
            Tool::Standings { sport, season } => json!({ "sport": sport, "season": season }),
            Tool::Champions { sport, season, limit } => json!({ "sport": sport, "season": season, "limit": limit }),
            Tool::PlayerStats { entity, stats, last, season } => json!({ "entity": entity, "stats": stats, "last": last, "season": season }),
            Tool::TeamResults { entity, opponent, last, season } => json!({ "entity": entity, "opponent": opponent, "last": last, "season": season }),
            Tool::TeamSchedule { entity, next } => json!({ "entity": entity, "next": next }),
            Tool::SimRun { sport, game_id, n, seed, out } => json!({ "sport": sport, "game_id": game_id, "n": n, "seed": seed, "out": out }),
            Tool::Resolve { q, sport, kind, focus } => json!({ "q": q, "sport": sport, "kind": kind, "focus": focus }),
        };
        if let Err(e) = tool.validate() {
            return ToolCall { tool: name, args, ok: false, ms: 0, error: Some(e), value: Value::Null, source: None };
        }
        let url = tool.url(&self.projection_url);
        let hit = self.cache.lock().unwrap().get(&url).filter(|(at, _, _)| at.elapsed() < CACHE_TTL).map(|(_, f, v)| (*f, v.clone()));
        if let Some((fetched_at, v)) = hit {
            return ToolCall { tool: name, args, ok: true, ms: t0.elapsed().as_millis() as u64, error: None, value: v,
                source: Some(Source { id: String::new(), tool: name.into(), label: tool.label(), url, fetched_at }) };
        }
        let res = async {
            let r = self.http.get(&url).timeout(tool.timeout()).send().await.map_err(|e| if e.is_timeout() { "timed out".to_string() } else { format!("unreachable: {e}") })?;
            let status = r.status();
            if !status.is_success() { return Err(format!("HTTP {status}")); }
            r.json::<Value>().await.map_err(|e| format!("bad JSON: {e}"))
        }.await;
        let ms = t0.elapsed().as_millis() as u64;
        match res {
            Ok(v) => {
                let now = Utc::now();
                let mut c = self.cache.lock().unwrap();
                c.retain(|_, (at, _, _)| at.elapsed() < CACHE_TTL);
                c.insert(url.clone(), (Instant::now(), now, v.clone()));
                ToolCall { tool: name, args, ok: true, ms, error: None, value: v,
                    source: Some(Source { id: String::new(), tool: name.into(), label: tool.label(), url, fetched_at: now }) }
            }
            Err(e) => ToolCall { tool: name, args, ok: false, ms, error: Some(e), value: Value::Null, source: None },
        }
    }
}

/// Track record rows -> per-sport totals (rows with only `sport` set are the
/// sport roll-up; fall back to summing the finest rows).
pub fn record_summary(v: &Value) -> Vec<Value> {
    let rows: Vec<&Value> = v["rows"].as_array().into_iter().flatten().collect();
    let rollup: Vec<&&Value> = rows.iter().filter(|r| r["kind"].is_null() && r["market"].is_null() && r["tier"].is_null() && r["grade"].is_null()).collect();
    rollup.iter().map(|r| json!({
        "sport": r["sport"], "graded": r["graded"], "wins": r["wins"], "losses": r["losses"], "pushes": r["pushes"],
        "units": r["units"], "roi": r["roi"], "win_rate": r["win_rate"], "clv_mean": r["clv_mean"],
    })).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_bad_args() {
        assert!(Tool::PicksToday { sport: Some("cricket".into()) }.validate().is_err());
        assert!(Tool::HubBrief { q: "".into(), sport: None }.validate().is_err());
        assert!(Tool::HubBrief { q: "x".repeat(401), sport: None }.validate().is_err());
        assert!(Tool::PicksRecord { sport: None, days: 0 }.validate().is_err());
        assert!(Tool::HubSlate { sport: "nhl".into() }.validate().is_ok());
        assert!(Tool::PicksToday { sport: None }.validate().is_ok());
    }

    #[test]
    fn knowledge_tools_take_only_well_formed_ids_and_bounded_windows() {
        assert!(Tool::PlayerStats { entity: "pl:nba:3112335".into(), stats: vec!["pts".into()], last: 20, season: None }.validate().is_ok());
        assert!(Tool::PlayerStats { entity: "pl:nba:1; DROP TABLE x".into(), stats: vec![], last: 20, season: None }.validate().is_err());
        assert!(Tool::PlayerStats { entity: "tm:nba:LAL".into(), stats: vec![], last: 20, season: None }.validate().is_err());
        assert!(Tool::PlayerStats { entity: "pl:nba:1".into(), stats: vec!["pts' OR 1=1".into()], last: 20, season: None }.validate().is_err());
        assert!(Tool::PlayerStats { entity: "pl:nba:1".into(), stats: vec![], last: 0, season: None }.validate().is_err());
        assert!(Tool::TeamResults { entity: "tm:nhl:EDM".into(), opponent: Some("tm:nhl:NJD".into()), last: 10, season: None }.validate().is_ok());
        assert!(Tool::Standings { sport: "xfl".into(), season: None }.validate().is_err());
        assert!(Tool::Champions { sport: "mlb".into(), season: Some(2025), limit: 1 }.validate().is_ok());
        assert!(Tool::Resolve { q: "James".into(), sport: Some("nba".into()), kind: Some("player".into()), focus: vec!["pl:nba:1966".into()] }.validate().is_ok());
        assert!(Tool::Resolve { q: "x".into(), sport: None, kind: Some("venue".into()), focus: vec![] }.validate().is_err());
        assert_eq!(Tool::Standings { sport: "nhl".into(), season: Some(2025) }.url("http://p"), "http://p/kb/standings?sport=nhl&season=2025");
        let sim = Tool::SimRun { sport: "nba".into(), game_id: "401909923".into(), n: 100_000, seed: 42, out: vec!["3112335".into()] };
        assert!(sim.validate().is_ok());
        assert_eq!(sim.url("http://p"), "http://p/nba/simulate/401909923?n=100000&seed=42&out=3112335");
        assert!(Tool::SimRun { sport: "nba".into(), game_id: "1".into(), n: 50_000, seed: 1, out: vec![] }.validate().is_err(), "below the engines' minimum");
        assert!(Tool::SimRun { sport: "nba".into(), game_id: "1/../x".into(), n: 100_000, seed: 1, out: vec![] }.validate().is_err());
    }

    #[tokio::test]
    async fn invalid_tool_never_calls_the_network() {
        let tools = Tools { http: reqwest::Client::new(), projection_url: "http://127.0.0.1:9".into(), cache: Default::default() };
        let c = tools.call(Tool::PicksToday { sport: Some("xfl".into()) }).await;
        assert!(!c.ok);
        assert_eq!(c.error.as_deref(), Some("unknown sport"));
        assert_eq!(c.ms, 0);
    }

    #[tokio::test]
    async fn unreachable_service_is_a_controlled_error() {
        let tools = Tools { http: reqwest::Client::new(), projection_url: "http://127.0.0.1:9".into(), cache: Default::default() };
        let c = tools.call(Tool::PicksToday { sport: None }).await;
        assert!(!c.ok);
        assert!(c.error.unwrap().starts_with("unreachable"));
        assert!(c.source.is_none());
    }

    #[test]
    fn record_rollup_rows_only() {
        let v = json!({ "rows": [
            { "sport": "nhl", "kind": null, "market": null, "tier": null, "grade": null, "graded": 40, "wins": 24, "losses": 16, "units": 5.1 },
            { "sport": "nhl", "kind": "prop", "market": "points", "tier": "proven", "grade": "A", "graded": 10, "wins": 7, "losses": 3, "units": 2.0 },
        ]});
        let s = record_summary(&v);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0]["graded"], 40);
    }
}
