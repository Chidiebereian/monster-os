//! portfolio-service
//! PnL, exposure, volatility, drawdown, Sharpe, Kelly across all positions.

use actix_web::{web, App, HttpServer, HttpResponse, Responder};
use common::{healthz, PortfolioSnapshot};

async fn portfolio() -> impl Responder {
    let out = PortfolioSnapshot {
        pnl: 0.0, exposure: 0.0, volatility: 0.0,
        drawdown: 0.0, sharpe: 0.0, kelly: 0.0,
    };
    HttpResponse::Ok().json(out)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8112);
    println!("portfolio-service listening on 0.0.0.0:{}", port);
    HttpServer::new(|| {
        App::new()
            .route("/healthz", web::get().to(healthz))
            .route("/portfolio", web::get().to(portfolio))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
