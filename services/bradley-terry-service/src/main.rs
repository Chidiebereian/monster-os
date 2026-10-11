//! bradley-terry-service
//! Bradley-Terry pairwise comparison ratings.

use actix_web::{web, App, HttpServer};
use common::healthz;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8108);
    println!("bradley-terry-service listening on 0.0.0.0:{}", port);
    HttpServer::new(|| App::new().route("/healthz", web::get().to(healthz)))
        .bind(("0.0.0.0", port))?
        .run()
        .await
}
