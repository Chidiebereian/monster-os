//! simulation-engine
//! Runs Monte Carlo simulations over posterior distributions and returns
//! summary statistics, e.g. {"mean":117.4,"median":117.0,"p10":104,"p90":130}
//!
//! GET /simulate?mean=117&sd=10&n=10000&seed=42
//!     draws `n` samples from a Normal(mean, sd).
//! GET /simulate?model_id=mlb-runs-v1&parameter=total_runs[&event_id=...]
//!     reads that parameter's posterior summary from feature-store
//!     (FEATURE_STORE_URL) and draws from Normal(posterior mean, posterior sd).
//!     Posteriors with R-hat above MAX_RHAT are rejected as unconverged.
//!     With `event_id`, the summary is also recorded in simulation_features.
//!
//! TODO: draw from the full posterior samples in MinIO (models.artifact_uri)
//! instead of a normal approximation of the summary.

use std::time::Duration;

use actix_web::{web, App, HttpServer, HttpResponse, Responder};
use common::{healthz, PosteriorSummary, SimulationOutput};
use serde::{Deserialize, Serialize};
use serde_json::json;

const MAX_DRAWS: usize = 1_000_000;
/// Vehtari et al. (2021) recommend R-hat < 1.01 before trusting a posterior.
const MAX_RHAT: f64 = 1.01;

#[derive(Deserialize)]
struct SimQuery {
    mean: Option<f64>,
    sd: Option<f64>,
    model_id: Option<String>,
    parameter: Option<String>,
    event_id: Option<String>,
    #[serde(default = "default_n")]
    n: usize,
    seed: Option<u64>,
}

#[derive(Serialize)]
struct SimResponse {
    #[serde(flatten)]
    summary: SimulationOutput,
    seed: u64,
    n: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    posterior: Option<PosteriorSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    simulation_id: Option<String>,
}

struct FeatureStore {
    http: reqwest::Client,
    base_url: String,
}

fn default_n() -> usize {
    10_000
}

/// SplitMix64: small, fast, deterministic for a given seed.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in (0, 1], never zero so ln() is safe.
    fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    }

    /// Standard normal via Box-Muller.
    fn normal(&mut self) -> f64 {
        let (u1, u2) = (self.uniform(), self.uniform());
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// Linear-interpolated quantile of an already-sorted slice.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    let pos = q * (sorted.len() - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

fn run(mean: f64, sd: f64, n: usize, seed: u64) -> SimulationOutput {
    let mut rng = Rng(seed);
    let mut draws: Vec<f64> = (0..n).map(|_| mean + sd * rng.normal()).collect();
    draws.sort_by(|a, b| a.total_cmp(b));
    SimulationOutput {
        mean: draws.iter().sum::<f64>() / n as f64,
        median: quantile(&draws, 0.5),
        p10: quantile(&draws, 0.1),
        p90: quantile(&draws, 0.9),
    }
}

fn error(status: actix_web::http::StatusCode, msg: impl std::fmt::Display) -> HttpResponse {
    HttpResponse::build(status).json(json!({ "error": msg.to_string() }))
}

fn valid_normal(mean: f64, sd: f64) -> bool {
    mean.is_finite() && sd.is_finite() && sd >= 0.0
}

/// Refuses posteriors that are unusable or whose chains didn't converge.
fn check_posterior(p: &PosteriorSummary) -> Result<(), String> {
    if !valid_normal(p.mean, p.sd) {
        return Err(format!("posterior {:?} has invalid mean/sd", p.parameter));
    }
    match p.rhat {
        Some(r) if !(r <= MAX_RHAT) => Err(format!(
            "posterior {:?} has R-hat {r} > {MAX_RHAT}; refit before simulating",
            p.parameter
        )),
        _ => Ok(()),
    }
}

impl FeatureStore {
    async fn posterior(&self, model_id: &str, parameter: &str) -> Result<PosteriorSummary, HttpResponse> {
        use actix_web::http::StatusCode;
        let url = format!("{}/posteriors/{model_id}/{parameter}", self.base_url);
        let resp = self.http.get(&url).send().await.map_err(|e| {
            eprintln!("simulation-engine: feature-store unreachable at {url}: {e}");
            error(StatusCode::BAD_GATEWAY, "feature-store unreachable")
        })?;
        match resp.status() {
            s if s.is_success() => resp
                .json()
                .await
                .map_err(|e| error(StatusCode::BAD_GATEWAY, format!("bad posterior payload: {e}"))),
            reqwest::StatusCode::NOT_FOUND => Err(error(
                StatusCode::NOT_FOUND,
                format!("no posterior for {model_id}/{parameter} in feature-store"),
            )),
            s => Err(error(StatusCode::BAD_GATEWAY, format!("feature-store returned {s}"))),
        }
    }

    async fn record(&self, record: serde_json::Value) -> Result<(), HttpResponse> {
        use actix_web::http::StatusCode;
        let resp = self
            .http
            .post(format!("{}/simulations", self.base_url))
            .json(&record)
            .send()
            .await
            .map_err(|e| error(StatusCode::BAD_GATEWAY, format!("recording simulation failed: {e}")))?;
        match resp.status() {
            s if s.is_success() => Ok(()),
            reqwest::StatusCode::CONFLICT => Err(error(
                StatusCode::CONFLICT,
                "this simulation_id/event_id was already recorded; use a different seed",
            )),
            s => Err(error(StatusCode::BAD_GATEWAY, format!("feature-store returned {s} when recording"))),
        }
    }
}

async fn simulate(fs: web::Data<FeatureStore>, q: web::Query<SimQuery>) -> impl Responder {
    use actix_web::http::StatusCode;
    let q = q.into_inner();
    if q.n == 0 || q.n > MAX_DRAWS {
        return error(StatusCode::BAD_REQUEST, format!("n must be in 1..={MAX_DRAWS}"));
    }
    let (mean, sd, posterior) = match (q.mean, q.sd, &q.model_id, &q.parameter) {
        (Some(m), Some(s), None, None) if valid_normal(m, s) => (m, s, None),
        (Some(_), Some(_), None, None) => {
            return error(StatusCode::BAD_REQUEST, "need finite mean and sd >= 0")
        }
        (None, None, Some(model_id), Some(parameter)) => {
            let p = match fs.posterior(model_id, parameter).await {
                Ok(p) => p,
                Err(resp) => return resp,
            };
            if let Err(msg) = check_posterior(&p) {
                return error(StatusCode::UNPROCESSABLE_ENTITY, msg);
            }
            (p.mean, p.sd, Some(p))
        }
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "pass either mean & sd, or model_id & parameter",
            )
        }
    };

    let seed = q.seed.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    });
    let n = q.n;
    let summary = match web::block(move || run(mean, sd, n, seed)).await {
        Ok(out) => out,
        Err(_) => return HttpResponse::InternalServerError().finish(),
    };

    let simulation_id = match &q.event_id {
        Some(event_id) => {
            let id = format!("sim-{seed:016x}");
            let record = json!({
                "simulation_id": id, "model_id": q.model_id, "event_id": event_id,
                "mean": summary.mean, "median": summary.median,
                "p10": summary.p10, "p90": summary.p90,
            });
            if let Err(resp) = fs.record(record).await {
                return resp;
            }
            Some(id)
        }
        None => None,
    };
    HttpResponse::Ok().json(SimResponse { summary, seed, n, posterior, simulation_id })
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8081);
    let fs = web::Data::new(FeatureStore {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        base_url: std::env::var("FEATURE_STORE_URL")
            .unwrap_or_else(|_| "http://localhost:8118".into())
            .trim_end_matches('/')
            .to_string(),
    });
    println!("simulation-engine listening on 0.0.0.0:{} (feature-store {})", port, fs.base_url);
    HttpServer::new(move || {
        App::new()
            .app_data(fs.clone())
            .route("/healthz", web::get().to(healthz))
            .route("/simulate", web::get().to(simulate))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_summary_matches_theory() {
        let out = run(117.0, 10.0, 200_000, 42);
        assert!((out.mean - 117.0).abs() < 0.1);
        assert!((out.median - 117.0).abs() < 0.15);
        // z(0.9) = 1.2816
        assert!((out.p10 - (117.0 - 12.816)).abs() < 0.2);
        assert!((out.p90 - (117.0 + 12.816)).abs() < 0.2);
    }

    #[test]
    fn same_seed_is_deterministic() {
        let (a, b) = (run(0.0, 1.0, 1000, 7), run(0.0, 1.0, 1000, 7));
        assert_eq!(a.mean, b.mean);
        assert_eq!(a.p90, b.p90);
    }

    #[test]
    fn quantile_interpolates() {
        assert_eq!(quantile(&[0.0, 10.0], 0.5), 5.0);
        assert_eq!(quantile(&[3.0], 0.9), 3.0);
    }

    fn posterior(sd: f64, rhat: Option<f64>) -> PosteriorSummary {
        PosteriorSummary {
            parameter: "total_runs".into(),
            mean: 8.5,
            sd,
            q05: None,
            q95: None,
            rhat,
            ess: None,
        }
    }

    #[test]
    fn rejects_unconverged_or_invalid_posteriors() {
        assert!(check_posterior(&posterior(1.0, Some(1.003))).is_ok());
        assert!(check_posterior(&posterior(1.0, None)).is_ok());
        assert!(check_posterior(&posterior(1.0, Some(1.08))).is_err());
        assert!(check_posterior(&posterior(1.0, Some(f64::NAN))).is_err());
        assert!(check_posterior(&posterior(-1.0, Some(1.0))).is_err());
    }
}
