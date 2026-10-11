//! market-intelligence
//! Aggregates prediction-market (Kalshi/Polymarket) order book and sentiment signals.

use actix_web::{web, App, HttpServer};
use common::healthz;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8114);
    println!("market-intelligence listening on 0.0.0.0:{}", port);
    HttpServer::new(|| App::new().route("/healthz", web::get().to(healthz)))
        .bind(("0.0.0.0", port))?
        .run()
        .await
}
