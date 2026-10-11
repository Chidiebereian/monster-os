//! data-quality-service
//! The gate between raw ingestion and the feature store. Every dataset
//! passes the seven checkpoints (missing values, duplicates, data leakage,
//! wrong labels, outliers, sampling bias, distribution shift) before it can
//! be written to feature-store or used by model training.
//! Bad data -> bad model: fix the data before changing the model.

mod checks;

use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use checks::{run_all, Dataset, QualityConfig};
use common::healthz;
use serde::Deserialize;

#[derive(Deserialize)]
struct CheckRequest {
    train: Dataset,
    production: Option<Dataset>,
    config: Option<ConfigOverride>,
}

#[derive(Deserialize)]
struct ConfigOverride {
    max_missing_rate: Option<f64>,
    outlier_z: Option<f64>,
    min_group_share: Option<f64>,
    max_psi: Option<f64>,
}

async fn check(req: web::Json<CheckRequest>) -> impl Responder {
    let mut cfg = QualityConfig::default();
    if let Some(o) = &req.config {
        if let Some(v) = o.max_missing_rate { cfg.max_missing_rate = v; }
        if let Some(v) = o.outlier_z { cfg.outlier_z = v; }
        if let Some(v) = o.min_group_share { cfg.min_group_share = v; }
        if let Some(v) = o.max_psi { cfg.max_psi = v; }
    }
    let report = run_all(&req.train, req.production.as_ref(), &cfg);
    if report.passed { HttpResponse::Ok().json(report) }
    else { HttpResponse::UnprocessableEntity().json(report) }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8113);
    println!("data-quality-service listening on 0.0.0.0:{}", port);
    HttpServer::new(|| {
        App::new()
            .app_data(web::JsonConfig::default().limit(64 * 1024 * 1024))
            .route("/healthz", web::get().to(healthz))
            .route("/check", web::post().to(check))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
