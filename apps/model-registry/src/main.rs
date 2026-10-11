//! model-registry
//! CRUD + versioning for every statistical model.
//! Backing table defined in infrastructure/postgres/migrations/0001_init.sql
//!
//! GET   /models?sport=MLB&status=active   list (filters optional)
//! GET   /models/{id}                      one model
//! POST  /models                           register a new model/version
//! PATCH /models/{id}/status               {"status":"draft"|"active"|"retired"}

use std::sync::Arc;

use actix_web::{web, App, HttpServer, HttpResponse, Responder};
use common::{healthz, ModelRecord, ModelType};
use serde::Deserialize;
use tokio_postgres::{error::SqlState, Client, NoTls, Row};

const STATUSES: [&str; 3] = ["draft", "active", "retired"];
const COLUMNS: &str = "id, name, sport, version, type, status, created_at";

type Db = web::Data<Arc<Client>>;

#[derive(Deserialize)]
struct ListQuery {
    sport: Option<String>,
    status: Option<String>,
}

#[derive(Deserialize)]
struct NewModel {
    id: String,
    name: String,
    sport: String,
    version: String,
    model_type: ModelType,
    #[serde(default = "default_status")]
    status: String,
}

fn default_status() -> String {
    "draft".into()
}

#[derive(Deserialize)]
struct StatusUpdate {
    status: String,
}

fn to_record(row: &Row) -> Result<ModelRecord, String> {
    let ty: String = row.get("type");
    Ok(ModelRecord {
        id: row.get("id"),
        name: row.get("name"),
        sport: row.get("sport"),
        version: row.get("version"),
        model_type: ModelType::parse(&ty).ok_or_else(|| format!("unknown model type {ty:?}"))?,
        status: row.get("status"),
        created_at: row.get("created_at"),
    })
}

fn bad_request(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(serde_json::json!({ "error": msg.into() }))
}

fn internal(msg: impl std::fmt::Display) -> HttpResponse {
    eprintln!("model-registry: {msg}");
    HttpResponse::InternalServerError().finish()
}

fn db_error(e: tokio_postgres::Error) -> HttpResponse {
    match e.code() {
        Some(c) if *c == SqlState::UNIQUE_VIOLATION => HttpResponse::Conflict()
            .json(serde_json::json!({ "error": "model id or (name, version) already exists" })),
        Some(c) if *c == SqlState::CHECK_VIOLATION => bad_request(e.to_string()),
        _ => internal(e),
    }
}

/// Renders one row with the given success status, or a 500 if it can't be decoded.
fn row_response(row: &Row, mut ok: actix_web::HttpResponseBuilder) -> HttpResponse {
    match to_record(row) {
        Ok(m) => ok.json(m),
        Err(e) => internal(e),
    }
}

fn check_status(status: &str) -> Result<(), HttpResponse> {
    if STATUSES.contains(&status) {
        Ok(())
    } else {
        Err(bad_request(format!("status must be one of {STATUSES:?}")))
    }
}

async fn list_models(db: Db, q: web::Query<ListQuery>) -> impl Responder {
    let sql = format!(
        "SELECT {COLUMNS} FROM models \
         WHERE ($1::text IS NULL OR sport = $1) AND ($2::text IS NULL OR status = $2) \
         ORDER BY created_at DESC"
    );
    match db.query(&sql, &[&q.sport, &q.status]).await {
        Ok(rows) => match rows.iter().map(to_record).collect::<Result<Vec<_>, _>>() {
            Ok(models) => HttpResponse::Ok().json(models),
            Err(e) => internal(e),
        },
        Err(e) => db_error(e),
    }
}

async fn get_model(db: Db, id: web::Path<String>) -> impl Responder {
    let sql = format!("SELECT {COLUMNS} FROM models WHERE id = $1");
    match db.query_opt(&sql, &[&id.as_str()]).await {
        Ok(Some(row)) => row_response(&row, HttpResponse::Ok()),
        Ok(None) => HttpResponse::NotFound().finish(),
        Err(e) => db_error(e),
    }
}

async fn create_model(db: Db, body: web::Json<NewModel>) -> impl Responder {
    if let Err(resp) = check_status(&body.status) {
        return resp;
    }
    let sql = format!(
        "INSERT INTO models (id, name, sport, version, type, status) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING {COLUMNS}"
    );
    let ty = body.model_type.as_str();
    match db
        .query_one(&sql, &[&body.id, &body.name, &body.sport, &body.version, &ty, &body.status])
        .await
    {
        Ok(row) => row_response(&row, HttpResponse::Created()),
        Err(e) => db_error(e),
    }
}

async fn set_status(db: Db, id: web::Path<String>, body: web::Json<StatusUpdate>) -> impl Responder {
    if let Err(resp) = check_status(&body.status) {
        return resp;
    }
    let sql = format!("UPDATE models SET status = $2 WHERE id = $1 RETURNING {COLUMNS}");
    match db.query_opt(&sql, &[&id.as_str(), &body.status]).await {
        Ok(Some(row)) => row_response(&row, HttpResponse::Ok()),
        Ok(None) => HttpResponse::NotFound().finish(),
        Err(e) => db_error(e),
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8084);
    let url = std::env::var("DATABASE_URL")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "DATABASE_URL must be set"))?;
    let (client, connection) = tokio_postgres::connect(&url, NoTls)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    actix_web::rt::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("model-registry: postgres connection closed: {e}");
        }
    });
    let db = web::Data::new(Arc::new(client));

    println!("model-registry listening on 0.0.0.0:{}", port);
    HttpServer::new(move || {
        App::new()
            .app_data(db.clone())
            .route("/healthz", web::get().to(healthz))
            .route("/models", web::get().to(list_models))
            .route("/models", web::post().to(create_model))
            .route("/models/{id}", web::get().to(get_model))
            .route("/models/{id}/status", web::patch().to(set_status))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
