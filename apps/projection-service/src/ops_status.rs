//! Operations health (backlog B5): `/healthz` only says the process is up, so
//! these check what it depends on.
//!
//!   GET /readyz      200 when Postgres answers and the autopilot ticked in the
//!                    last 30 min, else 503 with the failing checks
//!   GET /ops/status  one read-only summary: every service's /healthz, Postgres,
//!                    ClickHouse and Ollama, autopilot jobs, data freshness,
//!                    provider budgets, host disk and memory; `overall` is
//!                    ok | degraded | down with the reasons

use std::time::Duration;

use actix_web::{web, HttpResponse, Responder};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::State;

/// name -> port; must match scripts/windows/start-stack.ps1 and the api-gateway SERVICES table.
const SERVICES: [(&str, u16); 29] = [
    ("api-gateway", 8090), ("simulation-engine", 8081), ("pricing-engine", 8082), ("arbitrage-engine", 8083), ("model-registry", 8084),
    ("monster-gpt", 8085), ("market-intelligence", 8114), ("copy-trading", 8115), ("whale-tracker", 8116), ("backtesting-engine", 8117),
    ("feature-store", 8118), ("sport-intelligence", 8119), ("nba-service", 8100), ("nfl-service", 8101), ("mlb-service", 8102),
    ("nhl-service", 8103), ("soccer-service", 8104), ("prediction-market-service", 8105), ("bayesian-service", 8106), ("elo-service", 8107),
    ("bradley-terry-service", 8108), ("monte-carlo-service", 8109), ("bootstrap-service", 8110), ("kelly-service", 8111),
    ("portfolio-service", 8112), ("data-quality-service", 8113), ("odds-service", 8120), ("projection-service", 8121), ("dashboard", 5190),
];
/// A down core service makes the stack "down"; any other makes it "degraded".
const CORE: [&str; 5] = ["api-gateway", "projection-service", "odds-service", "monster-gpt", "dashboard"];
/// The autopilot ticks every few minutes; longer than this means it is stuck.
const TICK_MAX_MIN: i64 = 30;
/// Open picks priced on lines older than this are stale.
const LINES_MAX_MIN: i64 = 180;

fn age_min(t: Option<DateTime<Utc>>) -> Option<i64> { t.map(|t| (Utc::now() - t).num_minutes()) }

async fn db_ok(db: &tokio_postgres::Client) -> Result<(), String> {
    match actix_web::rt::time::timeout(Duration::from_secs(3), db.query_one("SELECT 1", &[])).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("no answer in 3 s".into()),
    }
}

async fn last_tick(db: &tokio_postgres::Client) -> Option<DateTime<Utc>> {
    db.query_opt("SELECT max(last_run) FROM learning_jobs", &[]).await.ok().flatten().and_then(|r| r.get(0))
}

/// GET /readyz
pub async fn readyz(state: web::Data<State>) -> impl Responder {
    let mut failing = Vec::new();
    let db = db_ok(&state.db).await;
    if let Err(e) = &db { failing.push(format!("postgres: {e}")); }
    let tick = if db.is_ok() { age_min(last_tick(&state.db).await) } else { None };
    if db.is_ok() && tick.map(|m| m > TICK_MAX_MIN).unwrap_or(true) { failing.push(format!("autopilot: last tick {} min ago", tick.map(|m| m.to_string()).unwrap_or("never".into()))); }
    let body = json!({ "status": if failing.is_empty() { "ready" } else { "not_ready" }, "postgres": db.is_ok(), "autopilot_tick_min": tick, "failing": failing });
    if failing.is_empty() { HttpResponse::Ok().json(body) } else { HttpResponse::ServiceUnavailable().json(body) }
}

async fn probe(http: &reqwest::Client, url: String) -> (bool, Option<u16>, u128) {
    let t = std::time::Instant::now();
    match http.get(&url).timeout(Duration::from_secs(2)).send().await {
        Ok(r) => (r.status().is_success(), Some(r.status().as_u16()), t.elapsed().as_millis()),
        Err(_) => (false, None, t.elapsed().as_millis()),
    }
}

/// Disk and memory on Windows (no extra dependency: one PowerShell call).
fn host() -> Value {
    if !cfg!(windows) { return json!({ "note": "host metrics are only collected on Windows" }); }
    let script = "$d=Get-PSDrive C; $o=Get-CimInstance Win32_OperatingSystem; \
        @{disk_free=[int64]$d.Free; disk_used=[int64]$d.Used; mem_free_kb=[int64]$o.FreePhysicalMemory; mem_total_kb=[int64]$o.TotalVisibleMemorySize} | ConvertTo-Json -Compress";
    let out = std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", script]).output();
    let Ok(out) = out else { return json!({ "error": "powershell unavailable" }) };
    let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else { return json!({ "error": "could not read host metrics" }) };
    let gb = |x: f64| (x / 1e9 * 10.0).round() / 10.0;
    let (free, used) = (v["disk_free"].as_f64().unwrap_or(0.0), v["disk_used"].as_f64().unwrap_or(0.0));
    let (mf, mt) = (v["mem_free_kb"].as_f64().unwrap_or(0.0) * 1024.0, v["mem_total_kb"].as_f64().unwrap_or(0.0) * 1024.0);
    json!({ "disk_c_free_gb": gb(free), "disk_c_used_pct": if free + used > 0.0 { (used / (free + used) * 1000.0).round() / 10.0 } else { 0.0 },
            "mem_free_gb": gb(mf), "mem_total_gb": gb(mt) })
}

/// Reasons the stack is degraded or down, from the summary (pure, so it is testable).
pub(crate) fn assess(services: &[(String, bool)], deps: &Value, failing_jobs: usize, tick_min: Option<i64>, stale_sports: &[String], host: &Value) -> (&'static str, Vec<String>) {
    let mut down = Vec::new();
    let mut degraded = Vec::new();
    for (name, ok) in services {
        if !ok { if CORE.contains(&name.as_str()) { down.push(format!("{name} is not answering")) } else { degraded.push(format!("{name} is not answering")) } }
    }
    if deps["postgres"]["ok"] != json!(true) { down.push("postgres is not answering".into()); }
    for d in ["clickhouse", "ollama"] { if deps[d]["ok"] != json!(true) { degraded.push(format!("{d} is not answering")); } }
    if failing_jobs > 0 { degraded.push(format!("{failing_jobs} autopilot job(s) failed on their last run")); }
    match tick_min { Some(m) if m <= TICK_MAX_MIN => {}, Some(m) => degraded.push(format!("autopilot last ticked {m} min ago")), None => degraded.push("autopilot has never ticked".into()) }
    for s in stale_sports { degraded.push(format!("{s}: open picks but no line seen in {LINES_MAX_MIN} min")); }
    if host["disk_c_free_gb"].as_f64().map(|g| g < 10.0).unwrap_or(false) { degraded.push(format!("C: has {} GB free", host["disk_c_free_gb"])); }
    if host["mem_free_gb"].as_f64().map(|g| g < 1.0).unwrap_or(false) { degraded.push(format!("{} GB of memory free", host["mem_free_gb"])); }
    let overall = if !down.is_empty() { "down" } else if !degraded.is_empty() { "degraded" } else { "ok" };
    down.extend(degraded);
    (overall, down)
}

/// GET /ops/status
pub async fn ops_status(state: web::Data<State>) -> impl Responder {
    let db = &state.db;
    let http = &state.http;
    let checks = SERVICES.iter().map(|(n, p)| async move {
        // the Vite dev server listens on localhost (IPv6 ::1), not 127.0.0.1
        let url = if *n == "dashboard" { format!("http://localhost:{p}/") } else { format!("http://127.0.0.1:{p}/healthz") };
        let (ok, code, ms) = probe(http, url).await;
        (n.to_string(), *p, ok, code, ms)
    });
    let host_task = web::block(host);
    let (svc, (ch, ol), host) = futures::join!(
        futures::future::join_all(checks),
        futures::future::join(probe(http, "http://127.0.0.1:8123/ping".into()), probe(http, "http://127.0.0.1:11434/api/version".into())),
        host_task,
    );
    let host = host.unwrap_or_else(|e| json!({ "error": e.to_string() }));

    let pg = db_ok(db).await;
    let size = db.query_opt("SELECT pg_database_size(current_database())", &[]).await.ok().flatten().map(|r| r.get::<_, i64>(0));
    let deps = json!({
        "postgres": { "ok": pg.is_ok(), "error": pg.err(), "size_gb": size.map(|b| (b as f64 / 1e9 * 10.0).round() / 10.0) },
        "clickhouse": { "ok": ch.0, "ms": ch.2 }, "ollama": { "ok": ol.0, "ms": ol.2 },
    });

    let jobs = db.query("SELECT job, last_run, last_ok, left(coalesce(detail::text, ''), 200) FROM learning_jobs ORDER BY job", &[]).await.unwrap_or_default();
    let failing: Vec<Value> = jobs.iter().filter(|r| !r.get::<_, bool>(2))
        .map(|r| json!({ "job": r.get::<_, String>(0), "last_run": r.get::<_, Option<DateTime<Utc>>>(1), "detail": r.get::<_, String>(3) })).collect();
    let tick = age_min(jobs.iter().filter_map(|r| r.get::<_, Option<DateTime<Utc>>>(1)).max());

    // freshness per sport: newest line seen, open picks
    let fresh = db.query("SELECT s.sport, (SELECT max(observed_at) FROM market_lines m WHERE m.sport = s.sport AND m.observed_at > now() - interval '7 days'), \
            (SELECT count(*) FROM ai_picks p WHERE p.sport = s.sport AND p.status = 'open' AND p.starts_at > now()), \
            (SELECT max(published_at) FROM ai_picks p WHERE p.sport = s.sport) \
        FROM unnest(ARRAY['nba','nfl','nhl','mlb']) AS s(sport)", &[]).await.unwrap_or_default();
    let mut stale = Vec::new();
    let freshness: Vec<Value> = fresh.iter().map(|r| {
        let (sport, line, open, pubd): (String, Option<DateTime<Utc>>, i64, Option<DateTime<Utc>>) = (r.get(0), r.get(1), r.get(2), r.get(3));
        let lm = age_min(line);
        if open > 0 && lm.map(|m| m > LINES_MAX_MIN).unwrap_or(true) { stale.push(sport.clone()); }
        json!({ "sport": sport, "newest_line_min": lm, "open_picks": open, "newest_pick_min": age_min(pubd) })
    }).collect();
    let injuries = jobs.iter().find(|r| r.get::<_, String>(0) == "snapshot:injuries").map(|r| age_min(r.get(1)));

    // provider budgets: today's units against the daily setting, and the month so far
    let usage = db.query("SELECT provider, coalesce(sum(units) FILTER (WHERE day = (now() AT TIME ZONE 'utc')::date), 0)::int, sum(units)::int \
        FROM provider_usage WHERE day >= date_trunc('month', now() AT TIME ZONE 'utc')::date GROUP BY provider ORDER BY provider", &[]).await.unwrap_or_default();
    let mut budgets = Vec::new();
    for r in &usage {
        let p: String = r.get(0);
        let daily = match p.as_str() { "sgo" => Some(crate::lines::setting(db, "sgo_daily_entities", 60.0).await), "oddsapi" => Some(crate::lines::setting(db, "oddsapi_daily_credits", 9.0).await), _ => None };
        budgets.push(json!({ "provider": p, "today": r.get::<_, i32>(1), "daily_budget": daily, "month": r.get::<_, i32>(2) }));
    }

    let svc_ok: Vec<(String, bool)> = svc.iter().map(|(n, _, ok, _, _)| (n.clone(), *ok)).collect();
    let (overall, reasons) = assess(&svc_ok, &deps, failing.len(), tick, &stale, &host);
    HttpResponse::Ok().json(json!({
        "overall": overall, "reasons": reasons, "checked_at": Utc::now(),
        "services": { "up": svc.iter().filter(|s| s.2).count(), "total": svc.len(),
            "list": svc.iter().map(|(n, p, ok, code, ms)| json!({ "name": n, "port": p, "ok": ok, "status": code, "ms": ms })).collect::<Vec<_>>() },
        "dependencies": deps,
        "autopilot": { "jobs": jobs.len(), "failing": failing, "last_tick_min": tick, "injury_snapshot_min": injuries.flatten() },
        "freshness": freshness, "budgets": budgets, "host": host,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_outages_are_down_and_everything_else_is_degraded() {
        let deps = json!({ "postgres": { "ok": true }, "clickhouse": { "ok": true }, "ollama": { "ok": true } });
        let host = json!({ "disk_c_free_gb": 40.0, "mem_free_gb": 4.0 });
        let up = vec![("projection-service".to_string(), true), ("elo-service".to_string(), true)];
        assert_eq!(assess(&up, &deps, 0, Some(5), &[], &host).0, "ok");
        let elo_down = vec![("projection-service".to_string(), true), ("elo-service".to_string(), false)];
        assert_eq!(assess(&elo_down, &deps, 0, Some(5), &[], &host).0, "degraded");
        let core_down = vec![("projection-service".to_string(), false)];
        assert_eq!(assess(&core_down, &deps, 0, Some(5), &[], &host).0, "down");
        let (o, r) = assess(&up, &json!({ "postgres": { "ok": true }, "clickhouse": { "ok": false }, "ollama": { "ok": true } }), 2, Some(45), &["nba".into()], &json!({ "disk_c_free_gb": 8.0, "mem_free_gb": 0.5 }));
        assert_eq!(o, "degraded");
        assert_eq!(r.len(), 6, "{r:?}");
        assert_eq!(assess(&up, &json!({ "postgres": { "ok": false } }), 0, Some(5), &[], &host).0, "down");
    }
}
