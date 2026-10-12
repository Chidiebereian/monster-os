//! api-gateway
//! Single entrypoint for every registered app/service in MonsterOS.
//!
//! GET  /                         directory of services and their upstreams
//! ANY  /api/{service}/{path..}   reverse-proxied to that service, e.g.
//!      /api/pricing-engine/price?probability=0.61 -> http://localhost:8082/price?...
//!
//! Upstreams default to localhost on each service's default port; override
//! one with `<SERVICE>_URL`, e.g. PRICING_ENGINE_URL=http://pricing:8082.

use std::collections::BTreeMap;
use std::time::Duration;

use actix_web::{http::StatusCode, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use common::healthz;

/// (name, default port) for every proxied app and service.
const SERVICES: &[(&str, u16)] = &[
    ("simulation-engine", 8081),
    ("pricing-engine", 8082),
    ("arbitrage-engine", 8083),
    ("model-registry", 8084),
    ("monster-gpt", 8085),
    ("market-intelligence", 8114),
    ("copy-trading", 8115),
    ("whale-tracker", 8116),
    ("backtesting-engine", 8117),
    ("feature-store", 8118),
    ("sport-intelligence", 8119),
    ("nba-service", 8100),
    ("nfl-service", 8101),
    ("mlb-service", 8102),
    ("nhl-service", 8103),
    ("soccer-service", 8104),
    ("prediction-market-service", 8105),
    ("bayesian-service", 8106),
    ("elo-service", 8107),
    ("bradley-terry-service", 8108),
    ("monte-carlo-service", 8109),
    ("bootstrap-service", 8110),
    ("kelly-service", 8111),
    ("portfolio-service", 8112),
    ("data-quality-service", 8113),
    ("odds-service", 8120),
    ("projection-service", 8121),
];

/// Hop-by-hop headers that must not be forwarded (RFC 9110 §7.6.1).
const HOP_BY_HOP: &[&str] = &[
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
    "te", "trailer", "transfer-encoding", "upgrade", "host", "content-length",
];

struct Gateway {
    http: reqwest::Client,
    upstreams: BTreeMap<&'static str, String>,
}

fn upstream_url(name: &str, port: u16) -> String {
    let var = format!("{}_URL", name.to_uppercase().replace('-', "_"));
    std::env::var(var)
        .unwrap_or_else(|_| format!("http://localhost:{port}"))
        .trim_end_matches('/')
        .to_string()
}

async fn directory(gw: web::Data<Gateway>) -> impl Responder {
    HttpResponse::Ok().json(&gw.upstreams)
}

async fn proxy(
    gw: web::Data<Gateway>,
    req: HttpRequest,
    path: web::Path<(String, String)>,
    body: web::Bytes,
) -> HttpResponse {
    let (service, tail) = path.into_inner();
    let Some(base) = gw.upstreams.get(service.as_str()) else {
        return HttpResponse::NotFound()
            .json(serde_json::json!({ "error": format!("unknown service {service:?}") }));
    };
    let mut url = format!("{base}/{tail}");
    if !req.query_string().is_empty() {
        url.push('?');
        url.push_str(req.query_string());
    }

    let method = match reqwest::Method::from_bytes(req.method().as_str().as_bytes()) {
        Ok(m) => m,
        Err(_) => return HttpResponse::MethodNotAllowed().finish(),
    };
    let mut upstream = gw.http.request(method, &url).body(body.to_vec());
    for (name, value) in req.headers() {
        if !HOP_BY_HOP.contains(&name.as_str()) {
            upstream = upstream.header(name.as_str(), value.as_bytes());
        }
    }

    let resp = match upstream.send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("api-gateway: {service} unreachable at {url}: {e}");
            return HttpResponse::BadGateway()
                .json(serde_json::json!({ "error": format!("{service} unreachable") }));
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out = HttpResponse::build(status);
    for (name, value) in resp.headers() {
        if !HOP_BY_HOP.contains(&name.as_str()) {
            out.append_header((name.as_str(), value.as_bytes()));
        }
    }
    match resp.bytes().await {
        Ok(bytes) => out.body(bytes),
        Err(e) => {
            eprintln!("api-gateway: reading {service} response failed: {e}");
            HttpResponse::BadGateway().finish()
        }
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let gw = web::Data::new(Gateway {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(200))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        upstreams: SERVICES.iter().map(|&(n, p)| (n, upstream_url(n, p))).collect(),
    });
    println!("api-gateway listening on 0.0.0.0:{}", port);
    HttpServer::new(move || {
        App::new()
            .app_data(gw.clone())
            .app_data(web::PayloadConfig::new(16 * 1024 * 1024))
            .route("/healthz", web::get().to(healthz))
            .route("/", web::get().to(directory))
            .route("/api/{service}/{tail:.*}", web::to(proxy))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
