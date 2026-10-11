//! feature-store
//! Canonical team/player/market/simulation/posterior feature tables.
//! Backing tables defined in infrastructure/postgres/migrations/0001_init.sql
//!
//! GET  /posteriors/{model_id}                 all parameter summaries
//! GET  /posteriors/{model_id}/{parameter}     one parameter
//! PUT  /posteriors/{model_id}                 upsert [PosteriorSummary, ...]
//! POST /simulations                           record one simulation summary
//! GET  /simulations/{simulation_id}           all events for a simulation
//! PUT  /features/{kind}/{scope}/{id}          write a snapshot {"as_of"?, "features": {...}}
//! GET  /features/{kind}/{scope}/{id}?as_of=   latest snapshot at or before as_of
//!
//! `kind` is team (scope = sport), player (scope = sport) or market (scope = venue).
//! Reads are point-in-time: pass the prediction cutoff as `as_of` so training
//! never sees features written after the event (data leakage).

use std::sync::Arc;

use actix_web::{web, App, HttpServer, HttpResponse, Responder};
use chrono::{DateTime, Utc};
use common::{healthz, PosteriorSummary};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_postgres::{error::SqlState, Client, NoTls, Row};

type Db = web::Data<Arc<Client>>;

#[derive(Deserialize, Serialize)]
struct SimulationRecord {
    simulation_id: String,
    model_id: Option<String>,
    event_id: String,
    mean: f64,
    median: f64,
    p10: f64,
    p90: f64,
}

#[derive(Deserialize)]
struct FeatureWrite {
    as_of: Option<DateTime<Utc>>,
    features: Value,
}

#[derive(Deserialize)]
struct AsOfQuery {
    as_of: Option<DateTime<Utc>>,
}

/// (table, scope column, id column) for each feature kind. Static strings
/// only, so interpolating them into SQL is safe.
fn feature_table(kind: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match kind {
        "team" => Some(("team_features", "sport", "team")),
        "player" => Some(("player_features", "sport", "player_id")),
        "market" => Some(("market_features", "venue", "market_id")),
        _ => None,
    }
}

fn bad_request(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({ "error": msg.into() }))
}

fn internal(msg: impl std::fmt::Display) -> HttpResponse {
    eprintln!("feature-store: {msg}");
    HttpResponse::InternalServerError().finish()
}

fn db_error(e: tokio_postgres::Error) -> HttpResponse {
    match e.code() {
        Some(c) if *c == SqlState::FOREIGN_KEY_VIOLATION => HttpResponse::NotFound()
            .json(json!({ "error": "model_id is not registered in model-registry" })),
        Some(c) if *c == SqlState::UNIQUE_VIOLATION => HttpResponse::Conflict()
            .json(json!({ "error": "record already exists" })),
        _ => internal(e),
    }
}

fn to_posterior(row: &Row) -> Result<PosteriorSummary, tokio_postgres::Error> {
    Ok(PosteriorSummary {
        parameter: row.try_get("parameter")?,
        mean: row.try_get("mean")?,
        sd: row.try_get("sd")?,
        q05: row.try_get("q05")?,
        q95: row.try_get("q95")?,
        rhat: row.try_get("rhat")?,
        ess: row.try_get("ess")?,
    })
}

fn to_simulation(row: &Row) -> Result<SimulationRecord, tokio_postgres::Error> {
    Ok(SimulationRecord {
        simulation_id: row.try_get("simulation_id")?,
        model_id: row.try_get("model_id")?,
        event_id: row.try_get("event_id")?,
        mean: row.try_get("mean")?,
        median: row.try_get("median")?,
        p10: row.try_get("p10")?,
        p90: row.try_get("p90")?,
    })
}

fn rows_json<T: Serialize>(
    rows: &[Row],
    f: fn(&Row) -> Result<T, tokio_postgres::Error>,
) -> HttpResponse {
    match rows.iter().map(f).collect::<Result<Vec<_>, _>>() {
        Ok(items) => HttpResponse::Ok().json(items),
        Err(e) => internal(e),
    }
}

const POSTERIOR_COLUMNS: &str = "parameter, mean, sd, q05, q95, rhat, ess";

async fn list_posteriors(db: Db, model_id: web::Path<String>) -> impl Responder {
    let sql = format!(
        "SELECT {POSTERIOR_COLUMNS} FROM posterior_features WHERE model_id = $1 ORDER BY parameter"
    );
    match db.query(&sql, &[&model_id.as_str()]).await {
        Ok(rows) => rows_json(&rows, to_posterior),
        Err(e) => db_error(e),
    }
}

async fn get_posterior(db: Db, path: web::Path<(String, String)>) -> impl Responder {
    let (model_id, parameter) = path.into_inner();
    let sql = format!(
        "SELECT {POSTERIOR_COLUMNS} FROM posterior_features WHERE model_id = $1 AND parameter = $2"
    );
    match db.query_opt(&sql, &[&model_id, &parameter]).await {
        Ok(Some(row)) => match to_posterior(&row) {
            Ok(p) => HttpResponse::Ok().json(p),
            Err(e) => internal(e),
        },
        Ok(None) => HttpResponse::NotFound().finish(),
        Err(e) => db_error(e),
    }
}

/// Upserts every parameter in one statement, so a batch lands atomically.
async fn put_posteriors(
    db: Db,
    model_id: web::Path<String>,
    body: web::Json<Vec<PosteriorSummary>>,
) -> impl Responder {
    if body.is_empty() {
        return bad_request("expected a non-empty array of posterior summaries");
    }
    if let Some(p) = body.iter().find(|p| !p.mean.is_finite() || !(p.sd.is_finite() && p.sd >= 0.0)) {
        return bad_request(format!("parameter {:?} needs finite mean and sd >= 0", p.parameter));
    }
    let col = |f: fn(&PosteriorSummary) -> Option<f64>| body.iter().map(f).collect::<Vec<_>>();
    let parameters: Vec<&str> = body.iter().map(|p| p.parameter.as_str()).collect();
    let (means, sds) = (col(|p| Some(p.mean)), col(|p| Some(p.sd)));
    let (q05, q95, rhat, ess) = (col(|p| p.q05), col(|p| p.q95), col(|p| p.rhat), col(|p| p.ess));
    let sql = "INSERT INTO posterior_features (model_id, parameter, mean, sd, q05, q95, rhat, ess) \
               SELECT $1, * FROM UNNEST($2::text[], $3::float8[], $4::float8[], $5::float8[], \
                                        $6::float8[], $7::float8[], $8::float8[]) \
               ON CONFLICT (model_id, parameter) DO UPDATE SET \
                 mean = EXCLUDED.mean, sd = EXCLUDED.sd, q05 = EXCLUDED.q05, q95 = EXCLUDED.q95, \
                 rhat = EXCLUDED.rhat, ess = EXCLUDED.ess";
    match db
        .execute(sql, &[&model_id.as_str(), &parameters, &means, &sds, &q05, &q95, &rhat, &ess])
        .await
    {
        Ok(n) => HttpResponse::Ok().json(json!({ "upserted": n })),
        Err(e) if e.code() == Some(&SqlState::CARDINALITY_VIOLATION) => {
            bad_request("duplicate parameter names in batch")
        }
        Err(e) => db_error(e),
    }
}

async fn record_simulation(db: Db, body: web::Json<SimulationRecord>) -> impl Responder {
    let s = body.into_inner();
    let sql = "INSERT INTO simulation_features \
               (simulation_id, model_id, event_id, mean, median, p10, p90) \
               VALUES ($1, $2, $3, $4, $5, $6, $7)";
    match db
        .execute(sql, &[&s.simulation_id, &s.model_id, &s.event_id, &s.mean, &s.median, &s.p10, &s.p90])
        .await
    {
        Ok(_) => HttpResponse::Created().json(s),
        Err(e) => db_error(e),
    }
}

async fn get_simulation(db: Db, id: web::Path<String>) -> impl Responder {
    let sql = "SELECT simulation_id, model_id, event_id, mean, median, p10, p90 \
               FROM simulation_features WHERE simulation_id = $1 ORDER BY event_id";
    match db.query(sql, &[&id.as_str()]).await {
        Ok(rows) if rows.is_empty() => HttpResponse::NotFound().finish(),
        Ok(rows) => rows_json(&rows, to_simulation),
        Err(e) => db_error(e),
    }
}

async fn put_features(
    db: Db,
    path: web::Path<(String, String, String)>,
    body: web::Json<FeatureWrite>,
) -> impl Responder {
    let (kind, scope, id) = path.into_inner();
    let Some((table, scope_col, id_col)) = feature_table(&kind) else {
        return bad_request("kind must be team, player or market");
    };
    if !body.features.is_object() {
        return bad_request("features must be a JSON object");
    }
    let as_of = body.as_of.unwrap_or_else(Utc::now);
    let sql = format!(
        "INSERT INTO {table} ({scope_col}, {id_col}, as_of, features) VALUES ($1, $2, $3, $4) \
         ON CONFLICT ({scope_col}, {id_col}, as_of) DO UPDATE SET features = EXCLUDED.features"
    );
    match db.execute(&sql, &[&scope, &id, &as_of, &body.features]).await {
        Ok(_) => HttpResponse::Ok().json(json!({ "as_of": as_of, "features": body.features })),
        Err(e) => db_error(e),
    }
}

async fn get_features(
    db: Db,
    path: web::Path<(String, String, String)>,
    q: web::Query<AsOfQuery>,
) -> impl Responder {
    let (kind, scope, id) = path.into_inner();
    let Some((table, scope_col, id_col)) = feature_table(&kind) else {
        return bad_request("kind must be team, player or market");
    };
    let as_of = q.as_of.unwrap_or_else(Utc::now);
    let sql = format!(
        "SELECT as_of, features FROM {table} \
         WHERE {scope_col} = $1 AND {id_col} = $2 AND as_of <= $3 \
         ORDER BY as_of DESC LIMIT 1"
    );
    match db.query_opt(&sql, &[&scope, &id, &as_of]).await {
        Ok(Some(row)) => {
            let (as_of, features): (DateTime<Utc>, Value) = (row.get(0), row.get(1));
            HttpResponse::Ok().json(json!({ "as_of": as_of, "features": features }))
        }
        Ok(None) => HttpResponse::NotFound().finish(),
        Err(e) => db_error(e),
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8118);
    let url = std::env::var("DATABASE_URL")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "DATABASE_URL must be set"))?;
    let (client, connection) = tokio_postgres::connect(&url, NoTls)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    actix_web::rt::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("feature-store: postgres connection closed: {e}");
        }
    });
    let db = web::Data::new(Arc::new(client));

    println!("feature-store listening on 0.0.0.0:{}", port);
    HttpServer::new(move || {
        App::new()
            .app_data(db.clone())
            .route("/healthz", web::get().to(healthz))
            .route("/posteriors/{model_id}", web::get().to(list_posteriors))
            .route("/posteriors/{model_id}", web::put().to(put_posteriors))
            .route("/posteriors/{model_id}/{parameter}", web::get().to(get_posterior))
            .route("/simulations", web::post().to(record_simulation))
            .route("/simulations/{simulation_id}", web::get().to(get_simulation))
            .route("/features/{kind}/{scope}/{id}", web::put().to(put_features))
            .route("/features/{kind}/{scope}/{id}", web::get().to(get_features))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
