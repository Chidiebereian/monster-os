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
}

impl Tool {
    pub fn name(&self) -> &'static str {
        match self { Tool::PicksToday { .. } => "picks.today", Tool::PicksRecord { .. } => "picks.record", Tool::HubBrief { .. } => "hub.brief", Tool::HubSlate { .. } => "hub.slate" }
    }

    /// Argument checks: known sports only, bounded free text.
    pub fn validate(&self) -> Result<(), String> {
        let sport_ok = |s: &Option<String>| s.as_deref().map(|x| SPORTS.contains(&x)).unwrap_or(true);
        match self {
            Tool::PicksToday { sport } | Tool::PicksRecord { sport, .. } if !sport_ok(sport) => Err("unknown sport".into()),
            Tool::PicksRecord { days, .. } if *days == 0 || *days > 365 => Err("days must be 1-365".into()),
            Tool::HubBrief { q, sport } => {
                if q.trim().is_empty() || q.len() > 400 { return Err("q must be 1-400 characters".into()); }
                if !sport_ok(sport) { return Err("unknown sport".into()); }
                Ok(())
            }
            Tool::HubSlate { sport } if !SPORTS.contains(&sport.as_str()) => Err("unknown sport".into()),
            _ => Ok(()),
        }
    }

    fn timeout(&self) -> Duration {
        match self { Tool::HubBrief { .. } => Duration::from_secs(25), Tool::HubSlate { .. } => Duration::from_secs(20), _ => Duration::from_secs(12) }
    }

    fn url(&self, base: &str) -> String {
        match self {
            Tool::PicksToday { sport } => format!("{base}/picks?status=open&sort=edge&limit=200{}", sport.as_deref().map(|s| format!("&sport={s}")).unwrap_or_default()),
            Tool::PicksRecord { sport, days } => format!("{base}/picks/record?days={days}{}", sport.as_deref().map(|s| format!("&sport={s}")).unwrap_or_default()),
            Tool::HubBrief { q, sport } => format!("{base}/hub/brief?q={}{}", crate::urlencode(q), sport.as_deref().map(|s| format!("&sport={s}")).unwrap_or_default()),
            Tool::HubSlate { sport } => format!("{base}/hub/{sport}/schedule"),
        }
    }

    fn label(&self) -> String {
        match self {
            Tool::PicksToday { sport } => format!("AI Picks: open {} picks", sport.as_deref().map(|s| s.to_uppercase()).unwrap_or_else(|| "all-sport".into())),
            Tool::PicksRecord { sport, days } => format!("AI Picks track record, last {days} days{}", sport.as_deref().map(|s| format!(" ({})", s.to_uppercase())).unwrap_or_default()),
            Tool::HubBrief { .. } => "Games hub: game briefs (model, best lines, injuries, matchup ranks)".into(),
            Tool::HubSlate { sport } => format!("Games hub: {} schedule", sport.to_uppercase()),
        }
    }
}

pub struct Tools {
    pub http: reqwest::Client,
    pub projection_url: String,
}

impl Tools {
    pub async fn call(&self, tool: Tool) -> ToolCall {
        let t0 = Instant::now();
        let name = tool.name();
        let args = match &tool {
            Tool::PicksToday { sport } => json!({ "sport": sport }),
            Tool::PicksRecord { sport, days } => json!({ "sport": sport, "days": days }),
            Tool::HubBrief { q, sport } => json!({ "q": q, "sport": sport }),
            Tool::HubSlate { sport } => json!({ "sport": sport }),
        };
        if let Err(e) = tool.validate() {
            return ToolCall { tool: name, args, ok: false, ms: 0, error: Some(e), value: Value::Null, source: None };
        }
        let url = tool.url(&self.projection_url);
        let res = async {
            let r = self.http.get(&url).timeout(tool.timeout()).send().await.map_err(|e| if e.is_timeout() { "timed out".to_string() } else { format!("unreachable: {e}") })?;
            let status = r.status();
            if !status.is_success() { return Err(format!("HTTP {status}")); }
            r.json::<Value>().await.map_err(|e| format!("bad JSON: {e}"))
        }.await;
        let ms = t0.elapsed().as_millis() as u64;
        match res {
            Ok(v) => ToolCall { tool: name, args, ok: true, ms, error: None, value: v,
                source: Some(Source { id: String::new(), tool: name.into(), label: tool.label(), url, fetched_at: Utc::now() }) },
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

    #[tokio::test]
    async fn invalid_tool_never_calls_the_network() {
        let tools = Tools { http: reqwest::Client::new(), projection_url: "http://127.0.0.1:9".into() };
        let c = tools.call(Tool::PicksToday { sport: Some("xfl".into()) }).await;
        assert!(!c.ok);
        assert_eq!(c.error.as_deref(), Some("unknown sport"));
        assert_eq!(c.ms, 0);
    }

    #[tokio::test]
    async fn unreachable_service_is_a_controlled_error() {
        let tools = Tools { http: reqwest::Client::new(), projection_url: "http://127.0.0.1:9".into() };
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
