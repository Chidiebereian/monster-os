//! WebSocket gateway for MonsterGPT (protocol v1, see protocol.rs), plus the
//! session and conversation REST endpoints.
//!
//! Auth: a bearer token from `POST /session`, which only answers loopback
//! clients (the dashboard reaches it through the local gateway). Browser
//! connections must come from an allowed Origin. One generation per
//! connection, a global limit on concurrent generations, 20 messages/min per
//! connection, a bounded outbound queue, heartbeats, and cancel-on-disconnect.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use actix_web::{web, HttpRequest, HttpResponse};
use actix_ws::Message;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use crate::orchestrator::{self, Gpt, Request};
use crate::protocol::{self, ClientMsg, ServerMsg, MAX_TEXT};

const HEARTBEAT: Duration = Duration::from_secs(20);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const RATE_PER_MIN: usize = 20;
const OUTBOUND_QUEUE: usize = 64;

/// Sliding one-minute window; true when the message is allowed.
pub fn rate_ok(window: &mut VecDeque<Instant>, now: Instant, limit: usize) -> bool {
    while window.front().map(|t| now.duration_since(*t) > Duration::from_secs(60)).unwrap_or(false) { window.pop_front(); }
    if window.len() >= limit { return false; }
    window.push_back(now);
    true
}

fn bearer(req: &HttpRequest) -> Option<String> {
    req.headers().get("authorization")?.to_str().ok()?.strip_prefix("Bearer ").map(|s| s.trim().to_string())
}

/// Local caller: the socket peer is loopback and, when a local proxy (the
/// gateway) forwarded the request, so is every X-Forwarded-For address.
fn is_loopback(req: &HttpRequest) -> bool {
    let peer = req.peer_addr().map(|a| a.ip().is_loopback()).unwrap_or(false);
    let forwarded_ok = req.headers().get_all("x-forwarded-for").all(|v| v.to_str().map(|s| s.split(',')
        .all(|ip| ip.trim().parse::<std::net::IpAddr>().map(|i| i.is_loopback()).unwrap_or(false))).unwrap_or(false));
    peer && forwarded_ok
}

#[derive(Deserialize, Default)]
pub struct SessionReq { #[serde(default)] user: Option<String> }

/// POST /session -> { token, owner }. Loopback only. `user` (letters, digits,
/// - and _) gives a separate owner, e.g. for tests; default owner is "local".
pub async fn session(req: HttpRequest, g: web::Data<Gpt>, body: Option<web::Json<SessionReq>>) -> HttpResponse {
    if !is_loopback(&req) { return HttpResponse::Forbidden().json(json!({ "error": "sessions are issued to local clients only" })); }
    if let Some(o) = req.headers().get("origin").and_then(|o| o.to_str().ok()) {
        if !g.origin_ok(o) { return HttpResponse::Forbidden().json(json!({ "error": "origin not allowed" })); }
    }
    let user = body.and_then(|b| b.into_inner().user).unwrap_or_default();
    if !user.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') || user.len() > 40 {
        return HttpResponse::BadRequest().json(json!({ "error": "user may contain letters, digits, - and _ only" }));
    }
    let owner = if user.is_empty() { "local".to_string() } else { format!("local:{user}") };
    let token = g.issue(&owner);
    HttpResponse::Ok().json(json!({ "token": token, "owner": owner, "expires_in": 12 * 3600, "protocol": protocol::VERSION }))
}

fn owner_or_401(req: &HttpRequest, g: &Gpt) -> Result<String, HttpResponse> {
    bearer(req).and_then(|t| g.owner_for(&t)).ok_or_else(|| HttpResponse::Unauthorized().json(json!({ "error": "missing or expired session token" })))
}

pub async fn conversations(req: HttpRequest, g: web::Data<Gpt>) -> HttpResponse {
    let owner = match owner_or_401(&req, &g) { Ok(o) => o, Err(r) => return r };
    match &g.store {
        Some(s) => match s.list(&owner).await { Ok(v) => HttpResponse::Ok().json(json!({ "conversations": v })), Err(e) => HttpResponse::InternalServerError().json(json!({ "error": e })) },
        None => HttpResponse::Ok().json(json!({ "conversations": [], "note": "conversation storage is off (no database)" })),
    }
}

pub async fn conversation_messages(req: HttpRequest, g: web::Data<Gpt>, path: web::Path<String>) -> HttpResponse {
    let owner = match owner_or_401(&req, &g) { Ok(o) => o, Err(r) => return r };
    let Some(s) = &g.store else { return HttpResponse::NotFound().json(json!({ "error": "conversation storage is off" })) };
    match s.messages(&owner, &path.into_inner()).await {
        Ok(v) => HttpResponse::Ok().json(json!({ "messages": v })),
        Err(e) if e.contains("not found") => HttpResponse::NotFound().json(json!({ "error": e })),
        Err(e) => HttpResponse::InternalServerError().json(json!({ "error": e })),
    }
}

pub async fn delete_conversation(req: HttpRequest, g: web::Data<Gpt>, path: web::Path<String>) -> HttpResponse {
    let owner = match owner_or_401(&req, &g) { Ok(o) => o, Err(r) => return r };
    let Some(s) = &g.store else { return HttpResponse::NotFound().finish() };
    match s.delete(&owner, &path.into_inner()).await {
        Ok(true) => HttpResponse::NoContent().finish(),
        Ok(false) => HttpResponse::NotFound().json(json!({ "error": "conversation not found" })),
        Err(e) => HttpResponse::InternalServerError().json(json!({ "error": e })),
    }
}

#[derive(serde::Deserialize)]
pub struct FeedbackBody { rating: i16 }

/// Thumbs up (1) / down (-1) / clear (0) on one of the caller's answers.
pub async fn message_feedback(req: HttpRequest, g: web::Data<Gpt>, path: web::Path<String>, body: web::Json<FeedbackBody>) -> HttpResponse {
    let owner = match owner_or_401(&req, &g) { Ok(o) => o, Err(r) => return r };
    if !(-1..=1).contains(&body.rating) { return HttpResponse::BadRequest().json(json!({ "error": "rating must be -1, 0 or 1" })); }
    let Some(s) = &g.store else { return HttpResponse::NotFound().json(json!({ "error": "conversation storage is off" })) };
    match s.set_feedback(&owner, &path.into_inner(), body.rating).await {
        Ok(true) => HttpResponse::NoContent().finish(),
        Ok(false) => HttpResponse::NotFound().json(json!({ "error": "message not found" })),
        Err(e) => HttpResponse::InternalServerError().json(json!({ "error": e })),
    }
}

pub async fn metrics(g: web::Data<Gpt>) -> HttpResponse { HttpResponse::Ok().json(g.metrics_json()) }

pub async fn memory_list(req: HttpRequest, g: web::Data<Gpt>) -> HttpResponse {
    let owner = match owner_or_401(&req, &g) { Ok(o) => o, Err(r) => return r };
    let Some(m) = &g.memory else { return HttpResponse::Ok().json(json!({ "items": [], "note": "memory is off" })) };
    match m.list(&owner).await { Ok(v) => HttpResponse::Ok().json(json!({ "items": v })), Err(e) => HttpResponse::InternalServerError().json(json!({ "error": e })) }
}

pub async fn memory_delete(req: HttpRequest, g: web::Data<Gpt>, path: web::Path<i64>) -> HttpResponse {
    let owner = match owner_or_401(&req, &g) { Ok(o) => o, Err(r) => return r };
    let Some(m) = &g.memory else { return HttpResponse::NotFound().finish() };
    match m.delete_item(&owner, path.into_inner()).await {
        Ok(true) => HttpResponse::NoContent().finish(),
        Ok(false) => HttpResponse::NotFound().json(json!({ "error": "no such memory item" })),
        Err(e) => HttpResponse::InternalServerError().json(json!({ "error": e })),
    }
}

#[derive(Deserialize)]
pub struct RagQ { q: String, #[serde(default)] mode: Option<crate::rag::Mode>, #[serde(default)] k: Option<usize>, #[serde(default)] sport: Option<String> }

/// GET /rag/search?q=&mode=hybrid|bm25|vector&k=&sport= (public documents, plus the caller's own with a token)
pub async fn rag_search(req: HttpRequest, g: web::Data<Gpt>, q: web::Query<RagQ>) -> HttpResponse {
    let Some(r) = &g.rag else { return HttpResponse::ServiceUnavailable().json(json!({ "error": "document search is off" })) };
    if q.q.trim().is_empty() || q.q.len() > 500 { return HttpResponse::BadRequest().json(json!({ "error": "q must be 1-500 characters" })); }
    let mut access = vec!["public".to_string()];
    if let Some(o) = bearer(&req).and_then(|t| g.owner_for(&t)) { access.push(format!("owner:{o}")); }
    let f = crate::rag::Filter { sport: q.sport.clone(), kinds: None, access };
    match r.search(&q.q, &f, q.mode.unwrap_or(crate::rag::Mode::Hybrid), q.k.unwrap_or(5).min(20)).await {
        Ok((hits, t)) => HttpResponse::Ok().json(json!({ "hits": hits, "timing": t, "chunks_indexed": r.len() })),
        Err(e) => HttpResponse::BadGateway().json(json!({ "error": e })),
    }
}

/// POST /rag/reindex[?force=true] (local callers only)
pub async fn rag_reindex(req: HttpRequest, g: web::Data<Gpt>, q: web::Query<std::collections::HashMap<String, String>>) -> HttpResponse {
    if !is_loopback(&req) { return HttpResponse::Forbidden().finish(); }
    let Some(r) = &g.rag else { return HttpResponse::ServiceUnavailable().json(json!({ "error": "document search is off" })) };
    HttpResponse::Ok().json(r.ingest(q.get("force").map(|v| v == "true").unwrap_or(false)).await)
}

/// GET /rag/eval: labeled queries in apps/monster-gpt/tests/rag_eval.json -> precision/recall/MRR per mode
pub async fn rag_eval(g: web::Data<Gpt>, q: web::Query<std::collections::HashMap<String, String>>) -> HttpResponse {
    let Some(r) = &g.rag else { return HttpResponse::ServiceUnavailable().json(json!({ "error": "document search is off" })) };
    let path = std::env::var("RAG_EVAL_FILE").unwrap_or_else(|_| "apps/monster-gpt/tests/rag_eval.json".into());
    let Ok(raw) = std::fs::read_to_string(&path) else { return HttpResponse::NotFound().json(json!({ "error": format!("{path} not found") })) };
    let cases: Vec<(String, Vec<String>)> = match serde_json::from_str::<Vec<serde_json::Value>>(&raw) {
        Ok(v) => v.iter().map(|c| (c["q"].as_str().unwrap_or("").to_string(), c["relevant"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())).collect(),
        Err(e) => return HttpResponse::BadRequest().json(json!({ "error": e.to_string() })),
    };
    let k = q.get("k").and_then(|v| v.parse().ok()).unwrap_or(5);
    HttpResponse::Ok().json(r.evaluate(&cases, k).await)
}

#[derive(Deserialize)]
pub struct ChatReq { text: String, #[serde(default)] conversation_id: Option<String>, #[serde(default)] request_id: Option<String>,
                     #[serde(default)] context: Option<serde_json::Value>, #[serde(default)] model: Option<String> }

/// POST /chat: the same turn over plain HTTP, returning every event at the end
/// (for scripts and tests; the dashboard uses the WebSocket).
pub async fn chat(req: HttpRequest, g: web::Data<Gpt>, body: web::Json<ChatReq>) -> HttpResponse {
    let owner = match owner_or_401(&req, &g) { Ok(o) => o, Err(r) => return r };
    let b = body.into_inner();
    if b.text.trim().is_empty() || b.text.chars().count() > MAX_TEXT { return HttpResponse::BadRequest().json(json!({ "error": "text must be 1-8000 characters" })); }
    let (tx, mut rx) = mpsc::channel::<ServerMsg>(4096);
    let request = Request { request_id: b.request_id.unwrap_or_else(|| crate::store::new_id("r")), conversation_id: b.conversation_id, text: b.text, context: b.context, model: b.model };
    let follow = orchestrator::run(&g, &owner, request, tx, Arc::new(AtomicBool::new(false))).await;
    background_follow_up(g.clone(), follow);
    let mut events = Vec::new();
    let mut answer = String::new();
    while let Ok(ev) = rx.try_recv() {
        match &ev {
            ServerMsg::Delta { text, .. } => answer.push_str(text),
            ServerMsg::Reset { .. } => answer.clear(), // the checker replaced the draft
            _ => {}
        }
        events.push(serde_json::from_str::<serde_json::Value>(&ev.to_json()).unwrap_or_default());
    }
    HttpResponse::Ok().json(json!({ "answer": answer, "events": events }))
}

/// GET /ws: upgrade and run the connection.
pub async fn ws(req: HttpRequest, body: web::Payload, g: web::Data<Gpt>) -> Result<HttpResponse, actix_web::Error> {
    if let Some(o) = req.headers().get("origin").and_then(|o| o.to_str().ok()) {
        if !g.origin_ok(o) {
            g.metrics.rejected_connections.fetch_add(1, Ordering::Relaxed);
            return Ok(HttpResponse::Forbidden().json(json!({ "error": "origin not allowed" })));
        }
    }
    if g.metrics.connections_active.load(Ordering::Relaxed) >= g.max_connections {
        g.metrics.rejected_connections.fetch_add(1, Ordering::Relaxed);
        return Ok(HttpResponse::ServiceUnavailable().json(json!({ "error": "too many connections" })));
    }
    let (resp, session, stream) = actix_ws::handle(&req, body)?;
    g.metrics.connections_active.fetch_add(1, Ordering::Relaxed);
    g.metrics.connections_total.fetch_add(1, Ordering::Relaxed);
    let g2 = g.clone();
    actix_web::rt::spawn(async move {
        connection(g2.clone(), session, stream).await;
        g2.metrics.connections_active.fetch_sub(1, Ordering::Relaxed);
    });
    Ok(resp)
}

struct Active { request_id: String, cancel: Arc<AtomicBool>, handle: tokio::task::JoinHandle<()> }

async fn connection(g: web::Data<Gpt>, mut session: actix_ws::Session, mut stream: actix_ws::MessageStream) {
    let (tx, mut rx) = mpsc::channel::<ServerMsg>(OUTBOUND_QUEUE);
    let mut owner: Option<String> = None;
    let mut last_seen = Instant::now();
    let mut window: VecDeque<Instant> = VecDeque::new();
    let mut active: Option<Active> = None;
    let mut beat = tokio::time::interval(HEARTBEAT);
    beat.tick().await;
    let session_id = crate::store::new_id("s");

    let reason = loop {
        tokio::select! {
            out = rx.recv() => {
                let Some(m) = out else { break "outbound closed" };
                if session.text(m.to_json()).await.is_err() { break "client gone"; }
            }
            _ = beat.tick() => {
                if last_seen.elapsed() > IDLE_TIMEOUT { break "idle timeout"; }
                if session.ping(b"hb").await.is_err() { break "client gone"; }
            }
            msg = stream.recv() => {
                let Some(Ok(msg)) = msg else { break "client closed" };
                last_seen = Instant::now();
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Ping(b) => { let _ = session.pong(&b).await; continue; }
                    Message::Pong(_) | Message::Nop => continue,
                    Message::Close(_) => break "client closed",
                    Message::Binary(_) | Message::Continuation(_) => {
                        let _ = session.text(ServerMsg::error(None, "bad_frame", "text frames only").to_json()).await;
                        continue;
                    }
                };
                let parsed = match protocol::parse_client(&text) {
                    Ok(m) => m,
                    Err(e) => { let _ = session.text(ServerMsg::error(None, "bad_frame", e).to_json()).await; continue; }
                };
                match parsed {
                    ClientMsg::Ping { .. } => { let _ = session.text(ServerMsg::Pong.to_json()).await; }
                    ClientMsg::Hello { token, .. } => match g.owner_for(&token) {
                        Some(o) => {
                            owner = Some(o.clone());
                            let models = json!(g.models.iter().map(|m| json!({ "id": m.id, "provider": m.provider, "num_ctx": m.num_ctx })).collect::<Vec<_>>());
                            let ready = ServerMsg::Ready { session: session_id.clone(), owner: o, models,
                                limits: json!({ "max_text": MAX_TEXT, "messages_per_min": RATE_PER_MIN, "concurrent_per_connection": 1, "heartbeat_s": HEARTBEAT.as_secs() }) };
                            if session.text(ready.to_json()).await.is_err() { break "client gone"; }
                            // warm the picks cache so the first question doesn't wait on the fetch
                            let g2 = g.clone();
                            actix_web::rt::spawn(async move { g2.tools.call(crate::tools::Tool::PicksToday { sport: None }).await; });
                        }
                        None => {
                            let _ = session.text(ServerMsg::error(None, "unauthorized", "invalid or expired session token; get a new one from POST /session").to_json()).await;
                            break "unauthorized";
                        }
                    },
                    _ if owner.is_none() => {
                        let _ = session.text(ServerMsg::error(None, "unauthorized", "send hello with a session token first").to_json()).await;
                        break "unauthorized";
                    }
                    ClientMsg::Cancel { request_id, .. } => {
                        match &active {
                            Some(a) if a.request_id == request_id && !a.handle.is_finished() => a.cancel.store(true, Ordering::Relaxed),
                            _ => { let _ = session.text(ServerMsg::error(Some(&request_id), "not_running", "no generation with that request_id is running").to_json()).await; }
                        }
                    }
                    ClientMsg::Message { request_id, conversation_id, text, context, model, .. } => {
                        if !rate_ok(&mut window, Instant::now(), RATE_PER_MIN) {
                            let _ = session.text(ServerMsg::error(Some(&request_id), "rate_limited", format!("more than {RATE_PER_MIN} messages in a minute")).to_json()).await;
                            continue;
                        }
                        if let Some(a) = &active {
                            if !a.handle.is_finished() {
                                let code = if a.request_id == request_id { "in_progress" } else { "busy" };
                                let _ = session.text(ServerMsg::error(Some(&request_id), code, "one answer at a time on this connection; cancel the current one or wait").to_json()).await;
                                continue;
                            }
                        }
                        let cancel = Arc::new(AtomicBool::new(false));
                        let (g2, tx2, c2, o2) = (g.clone(), tx.clone(), cancel.clone(), owner.clone().unwrap_or_default());
                        let req = Request { request_id: request_id.clone(), conversation_id, text, context, model };
                        let handle = actix_web::rt::spawn(async move {
                            let follow = orchestrator::run(&g2, &o2, req, tx2, c2).await;
                            background_follow_up(g2, follow);
                        });
                        active = Some(Active { request_id, cancel, handle });
                    }
                }
            }
        }
    };
    // disconnect: stop any running generation so it frees its model slot
    if let Some(a) = active { if !a.handle.is_finished() { a.cancel.store(true, Ordering::Relaxed); } }
    if reason != "client gone" { let _ = session.close(None).await; }
}

/// Run a turn's follow-up detached, so it never holds the connection's slot.
fn background_follow_up(g: web::Data<Gpt>, follow: Option<orchestrator::FollowUp>) {
    if let Some(f) = follow {
        actix_web::rt::spawn(async move { orchestrator::refresh_summary(&g, &f.model, &f.conversation).await; });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_window_allows_limit_then_blocks_then_recovers() {
        let mut w = VecDeque::new();
        let t0 = Instant::now();
        for _ in 0..3 { assert!(rate_ok(&mut w, t0, 3)); }
        assert!(!rate_ok(&mut w, t0 + Duration::from_secs(10), 3));
        assert!(rate_ok(&mut w, t0 + Duration::from_secs(61), 3));
    }
}
