//! mlb-service
//! MLB ingestion + sport-specific feature engineering (feeds mlb-betting-model).

use actix_web::{web, App, HttpServer};
use common::healthz;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8102);
    println!("mlb-service on port {}", port);
    HttpServer::new(|| App::new().route("/healthz", web::get().to(healthz)))
        .bind(&common::bind_addrs(port)[..])?
        .run()
        .await
}
