//! Shared types and traits for every MonsterOS app and service.

use actix_web::{HttpResponse, Responder};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a service listens: loopback only by default, 127.0.0.1 plus ::1 (so "localhost"
/// resolving to IPv6 still connects). Every caller (the gateway, the dashboard proxy, the
/// autopilot) is on this machine; the services have no authentication, so they are not
/// served to the network (ops backlog B2: they were reachable from a public Wi-Fi).
/// `BIND_ADDR` overrides it (e.g. `0.0.0.0` to serve the LAN deliberately).
pub fn bind_addrs(port: u16) -> Vec<std::net::SocketAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    let host = std::env::var("BIND_ADDR").ok().filter(|v| !v.trim().is_empty());
    let ip = match host.as_deref().map(|h| h.trim().parse::<IpAddr>()) {
        Some(Ok(ip)) => ip,
        Some(Err(_)) => {
            eprintln!("BIND_ADDR {:?} is not an IP address; listening on loopback", host.unwrap_or_default());
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        }
        None => IpAddr::V4(Ipv4Addr::LOCALHOST),
    };
    let mut out = vec![SocketAddr::new(ip, port)];
    if ip == IpAddr::V4(Ipv4Addr::LOCALHOST) && std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_ok() {
        out.push(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port));
    }
    println!("listening on {}", out.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", "));
    out
}

/// Every sport implements this pipeline:
/// ingest -> engineer_features -> fit_model -> simulate -> output_probabilities
pub trait SportModel {
    fn ingest(&self);
    fn engineer_features(&self);
    fn fit_model(&self);
    fn simulate(&self);
    fn output_probabilities(&self);
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SimulationOutput {
    pub mean: f64,
    pub median: f64,
    pub p10: f64,
    pub p90: f64,
}

/// Posterior summary for one model parameter (`posterior_features` row).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PosteriorSummary {
    pub parameter: String,
    pub mean: f64,
    pub sd: f64,
    pub q05: Option<f64>,
    pub q95: Option<f64>,
    pub rhat: Option<f64>,
    pub ess: Option<f64>,
}

/// Top-of-book for the YES side of a binary prediction market, in dollars
/// per $1 contract. `None` means no resting order on that side.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MarketQuote {
    pub venue: String,
    pub market_id: String,
    pub title: String,
    pub yes_bid: Option<f64>,
    pub yes_ask: Option<f64>,
    pub last: Option<f64>,
    pub as_of: DateTime<Utc>,
    /// Venue's grouping id (e.g. Kalshi event ticker), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// What YES resolves on, e.g. "Chicago WS" for a game-winner market.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Scheduled time of the underlying event, when the venue reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_time: Option<DateTime<Utc>>,
    /// Venue taker fee, when known. `None` means unknown, not free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taker_fee: Option<FeeSchedule>,
}

/// Taker fee per $1 contract: `rate × (p × (1 − p))^exponent`. Kalshi uses
/// rate 0.07, Polymarket sports 0.05, both with exponent 1.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq)]
pub struct FeeSchedule {
    pub rate: f64,
    pub exponent: f64,
}

impl FeeSchedule {
    pub const FREE: FeeSchedule = FeeSchedule { rate: 0.0, exponent: 1.0 };

    pub fn per_contract(&self, price: f64) -> f64 {
        self.rate * (price * (1.0 - price)).powf(self.exponent)
    }
}

/// One outcome of a multi-outcome event on a prediction-market venue (e.g. "Baltimore"
/// in Kalshi's AFC Champion 2026-27 event), as prediction-market-service `/futures`
/// returns it. Prices are dollars per $1 contract; `None` = no resting order.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VenueMarket {
    pub venue: String,
    /// Kalshi market ticker, or Polymarket market slug.
    pub market_id: String,
    pub event_id: String,
    pub event_title: String,
    /// What YES resolves on, as the venue labels it ("Baltimore", "Buffalo Bills").
    pub outcome: String,
    /// The venue's own short code for the outcome, when it has one (Kalshi ticker suffix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Season as the year it ends (Kalshi "-27", Polymarket "2027"), when the venue says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<i32>,
    pub yes_bid: Option<f64>,
    pub yes_ask: Option<f64>,
    /// When the outcome is expected to be known (Kalshi expected expiration, Polymarket end date).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolves_at: Option<DateTime<Utc>>,
    /// The venue's resolution rules for this market, verbatim.
    pub rules: String,
    /// The venue's page for this market.
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f64>,
    /// Taker fee, when known. `None` means unknown, not free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taker_fee: Option<FeeSchedule>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PricingOutput {
    pub probability: f64,
    pub fair_price: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArbitrageEdge {
    pub edge: f64,
    pub roi: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PortfolioSnapshot {
    pub pnl: f64,
    pub exposure: f64,
    pub volatility: f64,
    pub drawdown: f64,
    pub sharpe: f64,
    pub kelly: f64,
}

/// Serialized in snake_case to match the `models.type` CHECK constraint.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelType {
    Poisson,
    NegativeBinomial,
    Elo,
    BradleyTerry,
    Bootstrap,
    MonteCarlo,
    Bayesian,
    Prp,
    Toor,
    Gssd,
    Zsd,
}

impl ModelType {
    pub const ALL: [ModelType; 11] = [
        ModelType::Poisson,
        ModelType::NegativeBinomial,
        ModelType::Elo,
        ModelType::BradleyTerry,
        ModelType::Bootstrap,
        ModelType::MonteCarlo,
        ModelType::Bayesian,
        ModelType::Prp,
        ModelType::Toor,
        ModelType::Gssd,
        ModelType::Zsd,
    ];

    /// The value stored in Postgres, e.g. `negative_binomial`.
    pub fn as_str(self) -> &'static str {
        match self {
            ModelType::Poisson => "poisson",
            ModelType::NegativeBinomial => "negative_binomial",
            ModelType::Elo => "elo",
            ModelType::BradleyTerry => "bradley_terry",
            ModelType::Bootstrap => "bootstrap",
            ModelType::MonteCarlo => "monte_carlo",
            ModelType::Bayesian => "bayesian",
            ModelType::Prp => "prp",
            ModelType::Toor => "toor",
            ModelType::Gssd => "gssd",
            ModelType::Zsd => "zsd",
        }
    }

    pub fn parse(s: &str) -> Option<ModelType> {
        ModelType::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::ModelType;

    #[test]
    fn model_type_db_names_match_serde() {
        for t in ModelType::ALL {
            assert_eq!(serde_json::to_value(t).unwrap(), t.as_str());
            assert_eq!(ModelType::parse(t.as_str()), Some(t));
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModelRecord {
    pub id: String,
    pub name: String,
    pub sport: String,
    pub version: String,
    pub model_type: ModelType,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

/// Decimal (total-return) odds from American odds; `None` for -99..=99.
pub fn american_to_decimal(odds: i64) -> Option<f64> {
    match odds {
        o if o >= 100 => Some(1.0 + o as f64 / 100.0),
        o if o <= -100 => Some(1.0 + 100.0 / (-o) as f64),
        _ => None,
    }
}

/// American odds from decimal (total-return) odds; `None` for decimal <= 1.
/// Decimal >= 2.0 maps to positive American, below 2.0 to negative.
pub fn decimal_to_american(decimal: f64) -> Option<i64> {
    if decimal <= 1.0 {
        return None;
    }
    if decimal >= 2.0 {
        Some(((decimal - 1.0) * 100.0).round() as i64)
    } else {
        Some((-100.0 / (decimal - 1.0)).round() as i64)
    }
}

/// Implied win probability of American odds (the raw, vig-inclusive price);
/// `None` for -99..=99.
pub fn implied_probability(odds: i64) -> Option<f64> {
    american_to_decimal(odds).map(|d| 1.0 / d)
}

/// Standard health check every service exposes at GET /healthz.
pub async fn healthz() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({ "status": "ok" }))
}

#[cfg(test)]
mod bind_tests {
    use super::bind_addrs;

    #[test]
    fn loopback_unless_overridden() {
        // one test: BIND_ADDR is process-wide
        std::env::remove_var("BIND_ADDR");
        let a = bind_addrs(18999);
        assert!(a.iter().all(|x| x.ip().is_loopback()), "{a:?}");
        assert_eq!(a[0].to_string(), "127.0.0.1:18999");
        std::env::set_var("BIND_ADDR", "0.0.0.0");
        assert_eq!(bind_addrs(18999).iter().map(|x| x.to_string()).collect::<Vec<_>>(), vec!["0.0.0.0:18999"]);
        std::env::set_var("BIND_ADDR", "not-an-ip");
        assert!(bind_addrs(18999)[0].ip().is_loopback());
        std::env::remove_var("BIND_ADDR");
    }
}
