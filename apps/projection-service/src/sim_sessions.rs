//! Broadcast simulation sessions — the contract between the simulation engine
//! and the broadcast UI.
//!
//! A session is ONE seeded simulated game. The engine produces the complete,
//! ordered, authoritative event log up front (a game takes milliseconds), and
//! each event carries the full game state after it (score, clock, possession,
//! down & distance, ball spot, box score). The client only paces playback:
//! play / pause / step / scrub / replay never change an outcome, and replaying
//! the same seed with the same inputs reproduces the same game.
//!
//!   POST /sim/sessions                    {sport, game_id, seed?, away_qb?, home_qb?}
//!   GET  /sim/sessions?sport=&game_id=    recent sessions
//!   GET  /sim/sessions/{id}               metadata (+ final only with ?reveal=true)
//!   GET  /sim/sessions/{id}/events?after=&limit=
//!   GET  /sim/sessions/{id}/state?seq=    snapshot after event `seq`

use actix_web::{web, HttpResponse, Responder};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::{nfl_game, nfl_sim, State};

fn err(code: actix_web::http::StatusCode, m: impl Into<String>) -> HttpResponse {
    HttpResponse::build(code).json(json!({ "error": m.into() }))
}

#[derive(Deserialize)]
pub struct CreateBody {
    sport: String,
    game_id: String,
    seed: Option<u64>,
    away_qb: Option<String>,
    home_qb: Option<String>,
    /// alternate reality: player ids ruled out / forced to play
    #[serde(default)]
    out: Vec<String>,
    #[serde(default, rename = "in")]
    force_in: Vec<String>,
}

pub async fn create(state: web::Data<State>, body: web::Json<CreateBody>) -> impl Responder {
    let b = body.into_inner();
    let seed = b.seed.unwrap_or_else(|| nfl_sim::derive_seed(0x5e55, &[&b.game_id, &chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).to_string()]))
        & 0x7fff_ffff_ffff_ffff; // fits BIGINT
    let (meta, events, fin, cutoff, version) = match b.sport.as_str() {
        "nfl" => match nfl_game::new_session(&state.db, &b.game_id, seed,
            nfl_game::opts_scenario(b.away_qb.clone(), b.home_qb.clone(), &b.out, &b.force_in)).await {
            Ok((m, e, f, d)) => (m, e, f, d, nfl_game::MODEL_VERSION),
            Err(e) => return err(actix_web::http::StatusCode::UNPROCESSABLE_ENTITY, e),
        },
        other => return err(actix_web::http::StatusCode::BAD_REQUEST, format!("broadcast sessions are not available for '{other}' yet (nfl only)")),
    };
    let id = format!("sim_{:016x}", nfl_sim::derive_seed(seed, &[&b.sport, &b.game_id, &chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).to_string()]));
    let n_events = events.len();
    if let Err(e) = state.db.execute(
        "INSERT INTO sim_sessions (session_id, sport, game_id, model_version, seed, data_cutoff, meta, events, final) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        &[&id, &b.sport, &b.game_id, &version, &(seed as i64), &cutoff, &Json(&meta), &Json(Value::Array(events)), &Json(&fin)],
    ).await {
        return err(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR, format!("store session: {e}"));
    }
    HttpResponse::Created().json(json!({
        "session_id": id, "sport": b.sport, "game_id": b.game_id, "seed": seed, "model_version": version,
        "data_cutoff": cutoff, "events": n_events, "meta": meta,
    }))
}

#[derive(Deserialize)]
pub struct ListQuery {
    sport: Option<String>,
    game_id: Option<String>,
    #[serde(default = "d_limit")] limit: i64,
}
fn d_limit() -> i64 { 20 }

pub async fn list(state: web::Data<State>, q: web::Query<ListQuery>) -> impl Responder {
    let rows = state.db.query(
        "SELECT session_id, sport, game_id, created_at::text, model_version, seed, jsonb_array_length(events), final -> 'score' \
         FROM sim_sessions WHERE ($1::text IS NULL OR sport = $1) AND ($2::text IS NULL OR game_id = $2) \
         ORDER BY created_at DESC LIMIT $3", &[&q.sport, &q.game_id, &q.limit.clamp(1, 200)]).await;
    match rows {
        Ok(rows) => HttpResponse::Ok().json(json!({ "sessions": rows.iter().map(|r| json!({
            "session_id": r.get::<_, String>(0), "sport": r.get::<_, String>(1), "game_id": r.get::<_, String>(2),
            "created_at": r.get::<_, String>(3), "model_version": r.get::<_, String>(4), "seed": r.get::<_, i64>(5),
            "events": r.get::<_, i32>(6), "final_score": r.get::<_, Option<Json<Value>>>(7).map(|j| j.0),
        })).collect::<Vec<_>>() })),
        Err(e) => err(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct GetQuery {
    #[serde(default)] reveal: bool,
}

pub async fn get(state: web::Data<State>, path: web::Path<String>, q: web::Query<GetQuery>) -> impl Responder {
    let id = path.into_inner();
    match state.db.query_opt(
        "SELECT session_id, sport, game_id, created_at::text, model_version, seed, data_cutoff, meta, jsonb_array_length(events), final \
         FROM sim_sessions WHERE session_id = $1", &[&id]).await {
        Ok(Some(r)) => HttpResponse::Ok().json(json!({
            "session_id": r.get::<_, String>(0), "sport": r.get::<_, String>(1), "game_id": r.get::<_, String>(2),
            "created_at": r.get::<_, String>(3), "model_version": r.get::<_, String>(4), "seed": r.get::<_, i64>(5),
            "data_cutoff": r.get::<_, Option<chrono::NaiveDate>>(6), "meta": r.get::<_, Json<Value>>(7).0,
            "events": r.get::<_, i32>(8),
            "final": if q.reveal { r.get::<_, Json<Value>>(9).0 } else { Value::Null },
        })),
        Ok(None) => err(actix_web::http::StatusCode::NOT_FOUND, format!("no session {id}")),
        Err(e) => err(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct EventsQuery {
    #[serde(default)] after: i32,
    #[serde(default = "d_ev_limit")] limit: i32,
}
fn d_ev_limit() -> i32 { 1000 }

pub async fn events(state: web::Data<State>, path: web::Path<String>, q: web::Query<EventsQuery>) -> impl Responder {
    let id = path.into_inner();
    let after = q.after.max(0);
    let limit = q.limit.clamp(1, 1000);
    match state.db.query_opt(
        "SELECT jsonb_array_length(events), COALESCE((SELECT jsonb_agg(e ORDER BY (e->>'seq')::int) FROM jsonb_array_elements(events) e \
           WHERE (e->>'seq')::int > $2 AND (e->>'seq')::int <= $2 + $3), '[]'::jsonb) FROM sim_sessions WHERE session_id = $1",
        &[&id, &after, &limit]).await {
        Ok(Some(r)) => {
            let total: i32 = r.get(0);
            let ev: Json<Value> = r.get(1);
            let n = ev.0.as_array().map(|a| a.len()).unwrap_or(0) as i32;
            HttpResponse::Ok().json(json!({ "session_id": id, "total": total, "after": after, "next": after + n, "done": after + n >= total, "events": ev.0 }))
        }
        Ok(None) => err(actix_web::http::StatusCode::NOT_FOUND, format!("no session {id}")),
        Err(e) => err(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct StateQuery {
    seq: i32,
}

pub async fn snapshot(state: web::Data<State>, path: web::Path<String>, q: web::Query<StateQuery>) -> impl Responder {
    let id = path.into_inner();
    match state.db.query_opt(
        "SELECT e FROM sim_sessions, jsonb_array_elements(events) e WHERE session_id = $1 AND (e->>'seq')::int = $2", &[&id, &q.seq]).await {
        Ok(Some(r)) => {
            let e: Json<Value> = r.get(0);
            HttpResponse::Ok().json(json!({ "session_id": id, "seq": q.seq, "state": e.0["state"], "box_score": e.0["box_score"] }))
        }
        Ok(None) => err(actix_web::http::StatusCode::NOT_FOUND, format!("no event {} in session {id}", q.seq)),
        Err(e) => err(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
