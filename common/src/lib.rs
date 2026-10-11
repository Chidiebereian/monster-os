//! Shared types and traits for every MonsterOS app and service.

use actix_web::{HttpResponse, Responder};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
