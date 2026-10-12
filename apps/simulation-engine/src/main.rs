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
use serde_json::{json, Value};

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
    /// Distribution to draw from: normal (default), gamma/continuous,
    /// negbin/negative_binomial/count, or poisson. Lets a count stat draw
    /// non-negative integers and a yardage stat draw a true Gamma instead of a
    /// Normal approximation.
    family: Option<String>,
    /// Negative-Binomial size (Gamma shape) when family is a count; ignored otherwise.
    dispersion: Option<f64>,
    /// If set, return exact Over/Under/Push probabilities at this line from the draws.
    line: Option<f64>,
    /// If set (>0), return a density histogram with this many bins.
    hist: Option<usize>,
    #[serde(default = "default_n")]
    n: usize,
    seed: Option<u64>,
}

#[derive(Clone, Copy, PartialEq)]
enum Fam {
    Normal,
    Gamma,
    NegBin,
    Poisson,
}
fn parse_family(s: Option<&str>) -> Fam {
    match s.map(|x| x.to_ascii_lowercase()).as_deref() {
        Some("gamma") | Some("continuous") => Fam::Gamma,
        Some("negbin") | Some("negative_binomial") | Some("count") => Fam::NegBin,
        Some("poisson") => Fam::Poisson,
        _ => Fam::Normal,
    }
}
fn fam_name(f: Fam) -> &'static str {
    match f {
        Fam::Normal => "normal",
        Fam::Gamma => "gamma",
        Fam::NegBin => "negative_binomial",
        Fam::Poisson => "poisson",
    }
}

#[derive(Serialize)]
struct SimResponse {
    #[serde(flatten)]
    summary: SimulationOutput,
    seed: u64,
    n: usize,
    family: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    p_over: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p_under: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p_push: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hist: Option<Value>,
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

    /// Gamma(shape, scale) via Marsaglia-Tsang (with boosting for shape < 1).
    fn gamma(&mut self, shape: f64, scale: f64) -> f64 {
        if shape < 1.0 {
            let g = self.gamma(shape + 1.0, 1.0);
            return scale * g * self.uniform().powf(1.0 / shape);
        }
        let d = shape - 1.0 / 3.0;
        let c = 1.0 / (9.0 * d).sqrt();
        loop {
            let x = self.normal();
            let v = (1.0 + c * x).powi(3);
            if v <= 0.0 {
                continue;
            }
            let u = self.uniform();
            if u.ln() < 0.5 * x * x + d - d * v + d * v.ln() {
                return scale * d * v;
            }
        }
    }

    /// Poisson(lambda): Knuth for small lambda, a rounded normal approximation
    /// for large lambda (where Knuth underflows).
    fn poisson(&mut self, lambda: f64) -> f64 {
        if lambda <= 0.0 {
            return 0.0;
        }
        if lambda > 30.0 {
            return (lambda + lambda.sqrt() * self.normal()).round().max(0.0);
        }
        let l = (-lambda).exp();
        let (mut k, mut p) = (0.0f64, 1.0f64);
        loop {
            k += 1.0;
            p *= self.uniform();
            if p <= l {
                return k - 1.0;
            }
        }
    }
}

/// Draw one sample from the chosen family. mean/sd define the predictive;
/// dispersion is the Negative-Binomial size (Gamma shape) for counts.
fn sample(rng: &mut Rng, fam: Fam, mean: f64, sd: f64, dispersion: Option<f64>) -> f64 {
    match fam {
        Fam::Normal => mean + sd * rng.normal(),
        Fam::Poisson => rng.poisson(mean),
        Fam::Gamma => {
            if mean <= 0.0 || sd <= 0.0 {
                return mean.max(0.0);
            }
            let shape = (mean / sd).powi(2);
            let scale = sd * sd / mean;
            rng.gamma(shape, scale)
        }
        Fam::NegBin => {
            if mean <= 0.0 {
                return 0.0;
            }
            // r from stored dispersion, else from mean/variance; falls back to
            // near-Poisson when the data isn't overdispersed.
            let r = dispersion.filter(|r| *r > 0.0).unwrap_or_else(|| {
                let v = sd * sd;
                if v > mean { mean * mean / (v - mean) } else { 1e6 }
            });
            let lambda = rng.gamma(r, mean / r); // Gamma-Poisson mixture = NB
            rng.poisson(lambda)
        }
    }
}

/// Linear-interpolated quantile of an already-sorted slice.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    let pos = q * (sorted.len() - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

/// Draw `n` samples and return them sorted ascending.
fn draw_sorted(fam: Fam, mean: f64, sd: f64, dispersion: Option<f64>, n: usize, seed: u64) -> Vec<f64> {
    let mut rng = Rng(seed);
    let mut draws: Vec<f64> = (0..n).map(|_| sample(&mut rng, fam, mean, sd, dispersion)).collect();
    draws.sort_by(|a, b| a.total_cmp(b));
    draws
}

fn summarize(draws: &[f64]) -> SimulationOutput {
    SimulationOutput {
        mean: draws.iter().sum::<f64>() / draws.len() as f64,
        median: quantile(draws, 0.5),
        p10: quantile(draws, 0.1),
        p90: quantile(draws, 0.9),
    }
}

/// Exact Over/Under/Push fractions from the simulated draws. Push is the share
/// of draws exactly on the line (meaningful only for integer lines on counts).
fn line_probs(draws: &[f64], line: f64) -> (f64, f64, f64) {
    let n = draws.len() as f64;
    let (mut over, mut under, mut push) = (0.0, 0.0, 0.0);
    for &x in draws {
        if x > line {
            over += 1.0;
        } else if x < line {
            under += 1.0;
        } else {
            push += 1.0;
        }
    }
    (over / n, under / n, push / n)
}

/// Compact histogram (density per bin) over [p01, p99] for drawing the true
/// predictive shape without shipping every draw.
fn histogram(sorted: &[f64], bins: usize) -> Value {
    let lo = quantile(sorted, 0.005);
    let hi = quantile(sorted, 0.995);
    let span = (hi - lo).max(1e-9);
    let w = span / bins as f64;
    let mut counts = vec![0u32; bins];
    for &x in sorted {
        if x < lo || x > hi {
            continue;
        }
        let mut b = ((x - lo) / w) as usize;
        if b >= bins {
            b = bins - 1;
        }
        counts[b] += 1;
    }
    let total: u32 = counts.iter().sum();
    let density: Vec<f64> = counts
        .iter()
        .map(|&c| if total > 0 { c as f64 / total as f64 } else { 0.0 })
        .collect();
    json!({ "lo": lo, "hi": hi, "bin_width": w, "density": density })
}

/// Back-compat summary-only entry point (used by tests).
fn run(fam: Fam, mean: f64, sd: f64, dispersion: Option<f64>, n: usize, seed: u64) -> SimulationOutput {
    summarize(&draw_sorted(fam, mean, sd, dispersion, n, seed))
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
    let fam = parse_family(q.family.as_deref());
    let dispersion = q.dispersion;
    let (line, bins) = (q.line, q.hist);
    let draws = match web::block(move || draw_sorted(fam, mean, sd, dispersion, n, seed)).await {
        Ok(d) => d,
        Err(_) => return HttpResponse::InternalServerError().finish(),
    };
    let summary = summarize(&draws);
    let (p_over, p_under, p_push) = match line {
        Some(l) => {
            let (o, u, p) = line_probs(&draws, l);
            (Some(o), Some(u), Some(p))
        }
        None => (None, None, None),
    };
    let hist = bins.filter(|b| *b > 0).map(|b| histogram(&draws, b.min(200)));

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
    HttpResponse::Ok().json(SimResponse {
        summary, seed, n, family: fam_name(fam),
        p_over, p_under, p_push, hist, posterior, simulation_id,
    })
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
        let out = run(Fam::Normal, 117.0, 10.0, None, 200_000, 42);
        assert!((out.mean - 117.0).abs() < 0.1);
        assert!((out.median - 117.0).abs() < 0.15);
        // z(0.9) = 1.2816
        assert!((out.p10 - (117.0 - 12.816)).abs() < 0.2);
        assert!((out.p90 - (117.0 + 12.816)).abs() < 0.2);
    }

    #[test]
    fn same_seed_is_deterministic() {
        let (a, b) = (
            run(Fam::Normal, 0.0, 1.0, None, 1000, 7),
            run(Fam::Normal, 0.0, 1.0, None, 1000, 7),
        );
        assert_eq!(a.mean, b.mean);
        assert_eq!(a.p90, b.p90);
    }

    #[test]
    fn negbin_is_nonnegative_and_matches_moments() {
        // mean 1.88, dispersion r=... var = mean + mean^2/r; pick r so sd~1.39
        let r = 1.88 * 1.88 / (1.39 * 1.39 - 1.88);
        let out = run(Fam::NegBin, 1.88, 1.39, Some(r), 200_000, 11);
        assert!(out.p10 >= 0.0, "counts must be non-negative");
        assert!((out.mean - 1.88).abs() < 0.05);
    }

    #[test]
    fn gamma_is_positive_and_matches_mean() {
        let out = run(Fam::Gamma, 265.0, 80.0, None, 200_000, 5);
        assert!(out.p10 > 0.0, "gamma draws are positive");
        assert!((out.mean - 265.0).abs() < 2.0);
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
