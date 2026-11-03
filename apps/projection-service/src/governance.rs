//! Production governance: admin authentication, publication control and the
//! promotion-request workflow (migrations 0038 and 0039, docs/architecture/governance.md).
//!
//! The authority lives in the database. The service connects as `monster_app`,
//! which cannot make or change a champion, decide a promotion request, write its
//! events, publish a market or read admin credentials. Those steps are SECURITY
//! DEFINER functions (`gov_*`); the deciding ones take the admin's bearer token
//! and check it against `gov_private.admins` (SHA-256 digests only) themselves,
//! so the service's own database password is not enough to approve anything.
//!
//! Authentication. MonsterOS has no user accounts. Administrative calls must come
//! from this machine (loopback socket peer, and every forwarding header loopback
//! too) and carry `Authorization: Bearer <token>`. The principal is whoever the
//! token belongs to; names sent by the client are ignored. Revoking or rotating a
//! token (scripts/ops/admin_token.py) takes effect on the next call.
//!
//! Publication. Evidence tiers change automatically; publication does not.
//! `publication_state` (latest decision per engine x market) decides what public
//! readers see: `published` markets in the default feed when the pick's tier is
//! eligible, `published` or `leans_only` with "include unproven", nothing for
//! `suspended` or unpublished (no decision: every new market). Only an admin
//! decision publishes; the safety rule only ever suspends.
//!
//! Promotion requests. The autopilot, parlay learning and the dashboard may
//! propose; only an admin can approve, reject or cancel. Approval is bound to the
//! request's candidate, action, environment and artifact hash, re-checked against
//! the live registry, and executed at most once (`gov_approve`).

use std::net::IpAddr;

use actix_web::{web, HttpRequest, HttpResponse, Responder};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::State;

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}

/// The HTTP answer for a refusal raised by a gov_* function (SQLSTATE), never echoing the token.
fn refusal(e: &tokio_postgres::Error) -> HttpResponse {
    let body = json!({ "error": dberr(e) });
    match e.code().map(|c| c.code()) {
        Some("28000") | Some("42501") => HttpResponse::Forbidden().json(body),
        Some("22023") => HttpResponse::BadRequest().json(body),
        Some("55000") => HttpResponse::Conflict().json(body),
        Some("P0002") => HttpResponse::NotFound().json(body),
        _ => HttpResponse::InternalServerError().json(body),
    }
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// An address as forwarding headers write it: `1.2.3.4`, `1.2.3.4:80`, `"[::1]:80"`, `::1`.
fn header_ip(s: &str) -> Option<IpAddr> {
    let s = s.trim().trim_matches('"');
    if let Some(rest) = s.strip_prefix('[') { return rest.split(']').next()?.parse().ok(); }
    s.parse().ok().or_else(|| s.rsplit_once(':').and_then(|(h, _)| h.parse().ok()))
}

/// Every address in every value of a forwarding header is loopback (true when the header is absent).
fn header_all_loopback(req: &HttpRequest, name: &str, forwarded: bool) -> bool {
    req.headers().get_all(name).all(|v| v.to_str().map(|s| s.split(',').all(|el| {
        if forwarded {
            // RFC 7239: for=...;proto=...;by=... — every for= must be loopback ("unknown" or obfuscated is not)
            el.split(';').filter_map(|kv| kv.trim().split_once('=')).filter(|(k, _)| k.trim().eq_ignore_ascii_case("for"))
                .all(|(_, v)| header_ip(v).map(|i| i.is_loopback()).unwrap_or(false))
        } else {
            header_ip(el).map(|i| i.is_loopback()).unwrap_or(false)
        }
    })).unwrap_or(false))
}

/// Loopback socket peer, and loopback in X-Forwarded-For (set by the gateway from its
/// own socket peer), Forwarded and X-Real-IP. Headers can only make a call less local.
pub(crate) fn is_loopback(req: &HttpRequest) -> bool {
    req.peer_addr().map(|a| a.ip().is_loopback()).unwrap_or(false)
        && header_all_loopback(req, "x-forwarded-for", false)
        && header_all_loopback(req, "x-real-ip", false)
        && header_all_loopback(req, "forwarded", true)
}

/// The admin token of a local call: (token, {principal, role, expires_at}), or the refusal to send.
/// Deciding functions authenticate the token again, with the approver role.
pub(crate) async fn principal(req: &HttpRequest, db: &tokio_postgres::Client) -> Result<(String, Value), HttpResponse> {
    match db.query_one("SELECT gov_admins_configured()", &[]).await {
        Ok(r) if r.get::<_, bool>(0) => {}
        Ok(_) => return Err(HttpResponse::ServiceUnavailable().json(json!({ "error": "admin authentication is not configured; run scripts/ops/admin_token.py" }))),
        Err(e) => return Err(refusal(&e)),
    }
    if !is_loopback(req) { return Err(HttpResponse::Forbidden().json(json!({ "error": "administrative actions are accepted from this machine only" }))); }
    let token = req.headers().get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::trim);
    let Some(token) = token.filter(|t| !t.is_empty()).map(str::to_string) else {
        return Err(HttpResponse::Unauthorized().json(json!({ "error": "administrative action: Authorization: Bearer <admin token> required" })));
    };
    match db.query_one("SELECT gov_whoami($1)", &[&token]).await {
        Ok(r) => Ok((token, r.get::<_, Json<Value>>(0).0)),
        Err(e) => Err(refusal(&e)),
    }
}

/// GET /governance/whoami — the principal and role of the presented token (changes nothing).
pub async fn whoami(req: HttpRequest, state: web::Data<State>) -> impl Responder {
    match principal(&req, &state.db).await { Ok((_, who)) => HttpResponse::Ok().json(who), Err(r) => r }
}

// ---------------------------------------------------------------------------
// Publication
// ---------------------------------------------------------------------------

/// Anything public at all: published, or leans-only (shown with "include unproven").
pub(crate) const VISIBLE: &str = "EXISTS (SELECT 1 FROM publication_state s WHERE s.engine = ai_picks.engine AND s.market = ai_picks.market AND s.status IN ('published', 'leans_only'))";
pub(crate) const ELIGIBLE_TIERS: [&str; 2] = ["proven", "baseline"];

/// The one public filter for ai_picks: every pick in a market that is public at all
/// (published or leans-only), whatever its track-record tier. Picks are graded on price;
/// the track record is proven on the Track record and Models tabs. Suspended and
/// unpublished markets stay hidden (that is a governance decision).
pub(crate) fn public_filter() -> String { VISIBLE.to_string() }

/// Safety rule (pure): a published market whose evidence tier is no longer eligible is suspended.
pub(crate) fn safety_suspensions<'a>(published: &'a [(String, String)], tier: impl Fn(&str, &str) -> &'static str) -> Vec<(&'a str, &'a str, &'static str)> {
    published.iter().filter_map(|(e, m)| {
        let t = tier(e, m);
        (!ELIGIBLE_TIERS.contains(&t)).then_some((e.as_str(), m.as_str(), t))
    }).collect()
}

/// Suspend published markets of a sport whose tier fell out of eligibility. Never publishes:
/// `gov_safety_suspend` only records a suspension of a market that is published now.
pub(crate) async fn safety_sweep(db: &tokio_postgres::Client, sport: &str, tier: impl Fn(&str, &str) -> &'static str) -> Result<usize, String> {
    let engines = vec![sport.to_string(), format!("{sport}_team")];
    let published: Vec<(String, String)> = db.query("SELECT engine, market FROM publication_state WHERE status='published' AND engine = ANY($1)", &[&engines]).await
        .map_err(|e| dberr(&e))?.iter().map(|r| (r.get(0), r.get(1))).collect();
    let mut n = 0;
    for (e, m, t) in safety_suspensions(&published, tier) {
        if db.query_one("SELECT gov_safety_suspend($1, $2, $3)", &[&e, &m, &t]).await.map_err(|e| dberr(&e))?.get::<_, bool>(0) { n += 1; }
    }
    Ok(n)
}

#[derive(Deserialize)]
pub struct PublicationBody { engine: String, market: String, status: String, reason: String }

/// POST /governance/publication {engine, market, status: published|leans_only|suspended, reason} — approver only.
pub async fn set_publication(req: HttpRequest, state: web::Data<State>, body: web::Json<PublicationBody>) -> impl Responder {
    let (token, _) = match principal(&req, &state.db).await { Ok(p) => p, Err(r) => return r };
    let b = body.into_inner();
    match state.db.query_one("SELECT gov_publish($1, $2, $3, $4, $5)", &[&token, &b.engine, &b.market, &b.status, &b.reason]).await {
        Ok(r) => HttpResponse::Ok().json(r.get::<_, Json<Value>>(0).0),
        Err(e) => refusal(&e),
    }
}

/// GET /governance/publication — every decided market and its state.
pub async fn get_publication(state: web::Data<State>) -> impl Responder {
    match state.db.query("SELECT engine, market, status, reason, decided_by, auth_method, decided_at FROM publication_state ORDER BY engine, market", &[]).await {
        Ok(rows) => HttpResponse::Ok().json(json!({ "markets": rows.iter().map(|r| json!({ "engine": r.get::<_, String>(0), "market": r.get::<_, String>(1),
            "status": r.get::<_, String>(2), "reason": r.get::<_, String>(3), "decided_by": r.get::<_, String>(4), "auth_method": r.get::<_, String>(5),
            "decided_at": r.get::<_, chrono::DateTime<chrono::Utc>>(6) })).collect::<Vec<_>>(), "unlisted": "unpublished (not public)" })),
        Err(e) => HttpResponse::InternalServerError().json(json!({ "error": dberr(&e) })),
    }
}

// ---------------------------------------------------------------------------
// Promotion requests
// ---------------------------------------------------------------------------

/// The version a rollback would restore: the most recently retired former champion on the same code.
async fn previous_champion(db: &tokio_postgres::Client, kind: &str, engine: &str) -> Result<Option<String>, String> {
    let r = if kind == "parlay" {
        db.query_opt("SELECT version FROM parlay_models WHERE status='retired' AND $1 = 'parlay' ORDER BY retired_at DESC NULLS LAST LIMIT 1", &[&engine]).await
    } else {
        db.query_opt("SELECT version FROM model_registry WHERE sport=$1 AND status IN ('retired','shadow') AND promoted_at IS NOT NULL AND engine=$2 ORDER BY retired_at DESC NULLS LAST LIMIT 1",
            &[&engine, &crate::learning::engine_of(engine)]).await
    };
    Ok(r.map_err(|e| dberr(&e))?.map(|r| r.get(0)))
}

/// File a request (autopilot, parlay learning, dashboard, CLI). The database binds it to the
/// live champion and the candidate's artifact; returns the request, or the open one.
pub(crate) async fn propose(db: &tokio_postgres::Client, kind: &str, engine: &str, action: &str, candidate: &str, proposed_by: &str) -> Result<Value, String> {
    let r = db.query_one("SELECT gov_propose($1, $2, $3, $4, $5)", &[&kind, &engine, &action, &candidate, &proposed_by]).await.map_err(|e| dberr(&e))?;
    Ok(r.get::<_, Json<Value>>(0).0)
}

/// Expire requests nobody decided within their window (autopilot tick).
pub(crate) async fn expire(db: &tokio_postgres::Client) -> Result<u64, String> {
    Ok(db.query_one("SELECT gov_expire()", &[]).await.map_err(|e| dberr(&e))?.get::<_, i32>(0) as u64)
}

#[derive(Deserialize)]
pub struct ProposeBody { kind: String, engine: String, action: String, candidate_version: Option<String> }

/// POST /governance/promotion-requests — anyone on this machine may propose; nothing changes until an admin approves.
pub async fn create_request(req: HttpRequest, state: web::Data<State>, body: web::Json<ProposeBody>) -> impl Responder {
    if !is_loopback(&req) { return HttpResponse::Forbidden().json(json!({ "error": "requests are accepted from this machine only" })); }
    let b = body.into_inner();
    if !["engine", "parlay"].contains(&b.kind.as_str()) || !["promote", "rollback"].contains(&b.action.as_str()) {
        return HttpResponse::BadRequest().json(json!({ "error": "kind must be engine|parlay, action promote|rollback" }));
    }
    let db = &state.db;
    let candidate = match (b.action.as_str(), b.candidate_version) {
        ("promote", Some(v)) => v,
        ("promote", None) => return HttpResponse::BadRequest().json(json!({ "error": "candidate_version is required for a promotion" })),
        (_, _) => match previous_champion(db, &b.kind, &b.engine).await {
            Ok(Some(v)) => v, Ok(None) => return HttpResponse::Conflict().json(json!({ "error": "no previous champion to roll back to" })),
            Err(e) => return HttpResponse::InternalServerError().json(json!({ "error": e })) },
    };
    match db.query_one("SELECT gov_propose($1, $2, $3, $4, $5)", &[&b.kind, &b.engine, &b.action, &candidate, &"unauthenticated:local-request"]).await {
        Ok(r) => HttpResponse::Ok().json(r.get::<_, Json<Value>>(0).0),
        Err(e) => refusal(&e),
    }
}

/// GET /governance/promotion-requests — newest first, with their events.
pub async fn list_requests(state: web::Data<State>) -> impl Responder {
    let db = &state.db;
    let rows = match db.query("SELECT row_to_json(r)::jsonb FROM (SELECT * FROM promotion_requests ORDER BY request_id DESC LIMIT 100) r", &[]).await {
        Ok(r) => r, Err(e) => return HttpResponse::InternalServerError().json(json!({ "error": dberr(&e) })) };
    let mut events: std::collections::HashMap<i64, Vec<Value>> = std::collections::HashMap::new();
    if let Ok(ev) = db.query("SELECT request_id, row_to_json(e)::jsonb FROM promotion_request_events e ORDER BY id", &[]).await {
        for r in ev { events.entry(r.get(0)).or_default().push(r.get::<_, Json<Value>>(1).0); }
    }
    HttpResponse::Ok().json(json!({ "requests": rows.iter().map(|r| { let mut v = r.get::<_, Json<Value>>(0).0;
        let id = v["request_id"].as_i64().unwrap_or(0); v["events"] = json!(events.remove(&id).unwrap_or_default()); v }).collect::<Vec<_>>() }))
}

#[derive(Deserialize)]
pub struct DecideBody {
    reason: String,
    // the approver restates what they approve; any difference from the stored request refuses it
    candidate_version: Option<String>, action: Option<String>, target_environment: Option<String>, artifact_hash: Option<String>,
    #[serde(default)] dry_run: bool,
}

/// POST /governance/promotion-requests/{id}/approve — approver only; `gov_approve` checks the binding
/// and the live registry, then swaps the champion, marks the request EXECUTED and audits, in one transaction.
pub async fn approve(req: HttpRequest, state: web::Data<State>, path: web::Path<i64>, body: web::Json<DecideBody>) -> impl Responder {
    let (token, _) = match principal(&req, &state.db).await { Ok(p) => p, Err(r) => return r };
    let (id, b) = (path.into_inner(), body.into_inner());
    let r = match state.db.query_one("SELECT gov_approve($1, $2, $3, $4, $5, $6, $7, $8)",
        &[&id, &token, &b.candidate_version, &b.action, &b.target_environment, &b.artifact_hash, &b.reason, &b.dry_run]).await {
        Ok(r) => r.get::<_, Json<Value>>(0).0, Err(e) => return refusal(&e) };
    if r["ok"].as_bool() == Some(true) { return HttpResponse::Ok().json(r); }
    if r["status"] == "FAILED" { return HttpResponse::InternalServerError().json(r); }
    HttpResponse::Conflict().json(json!({ "error": "approval refused", "problems": r["problems"], "invalidated": r["invalidated"] }))
}

async fn close(req: HttpRequest, state: web::Data<State>, id: i64, b: DecideBody, to: &str) -> HttpResponse {
    let (token, _) = match principal(&req, &state.db).await { Ok(p) => p, Err(r) => return r };
    match state.db.query_one("SELECT gov_close($1, $2, $3, $4)", &[&id, &token, &to, &b.reason]).await {
        Ok(r) => HttpResponse::Ok().json(r.get::<_, Json<Value>>(0).0),
        Err(e) => refusal(&e),
    }
}

/// POST /governance/promotion-requests/{id}/reject — approver only.
pub async fn reject(req: HttpRequest, state: web::Data<State>, path: web::Path<i64>, body: web::Json<DecideBody>) -> impl Responder {
    close(req, state, path.into_inner(), body.into_inner(), "REJECTED").await
}

/// POST /governance/promotion-requests/{id}/cancel — approver only.
pub async fn cancel(req: HttpRequest, state: web::Data<State>, path: web::Path<i64>, body: web::Json<DecideBody>) -> impl Responder {
    close(req, state, path.into_inner(), body.into_inner(), "CANCELLED").await
}

/// The retired direct endpoints: production changes only through promotion requests.
pub async fn gone() -> impl Responder {
    HttpResponse::Gone().json(json!({ "error": "direct promotion and rollback were retired: file POST /governance/promotion-requests and have an admin approve it (scripts/ops/governance.py)" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test::TestRequest;

    fn req(peer: &str, headers: &[(&str, &str)]) -> HttpRequest {
        let mut t = TestRequest::default().peer_addr(peer.parse().unwrap());
        for (k, v) in headers { t = t.insert_header((*k, *v)); }
        t.to_http_request()
    }

    #[test]
    fn locality_needs_a_loopback_peer_and_loopback_forwarding_headers() {
        assert!(is_loopback(&req("127.0.0.1:5000", &[])));
        assert!(is_loopback(&req("[::1]:5000", &[("x-forwarded-for", "127.0.0.1")])));
        assert!(is_loopback(&req("127.0.0.1:5000", &[("forwarded", "for=\"[::1]:80\";proto=http")])));
        // a remote socket peer is never local, whatever the headers claim
        assert!(!is_loopback(&req("192.168.1.20:5000", &[])));
        assert!(!is_loopback(&req("203.0.113.7:5000", &[("x-forwarded-for", "127.0.0.1"), ("x-real-ip", "127.0.0.1"), ("forwarded", "for=127.0.0.1")])));
        // a local proxy relaying a remote client (any header naming a remote, unknown or unparsable address)
        for h in [("x-forwarded-for", "203.0.113.7"), ("x-forwarded-for", "127.0.0.1, 203.0.113.7"), ("x-real-ip", "203.0.113.7"),
                  ("forwarded", "for=203.0.113.7"), ("forwarded", "for=unknown"), ("forwarded", "for=_hidden"), ("x-forwarded-for", "garbage")] {
            assert!(!is_loopback(&req("127.0.0.1:5000", &[h])), "{h:?}");
        }
        // IPv4-mapped loopback is not treated as loopback (fails closed)
        assert!(!is_loopback(&req("[::ffff:127.0.0.1]:5000", &[])));
    }

    #[test]
    fn public_filter_never_shows_unpublished_markets() {
        let f = public_filter();
        assert!(f.contains("status IN ('published', 'leans_only')"));
        assert!(!f.contains("tier"), "every tier is visible: picks are graded on price");
    }

    #[test]
    fn safety_rule_only_suspends() {
        let published = vec![("nba".to_string(), "pts".to_string()), ("nba".to_string(), "reb".to_string())];
        let s = safety_suspensions(&published, |_, m| if m == "pts" { "losing" } else { "baseline" });
        assert_eq!(s, vec![("nba", "pts", "losing")]);
    }
}
