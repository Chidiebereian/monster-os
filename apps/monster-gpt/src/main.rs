//! monster-gpt
//! LLM research assistant. Wraps an OpenAI-compatible endpoint
//! (Ollama/vLLM/Llama/Qwen/DeepSeek) with the MonsterGPT system prompt
//! and retrieves from research/, models/, feature-store, and simulation
//! output before answering. See SYSTEM_PROMPT.md in this directory.
//!
//! POST /ask {"question": "...", "context": {...optional evidence...}}
//! Configured by LLM_BASE_URL (e.g. http://localhost:11434/v1) and LLM_MODEL,
//! from the environment or a .env file (real environment variables win).
//!
//! Optional non-sensitive fallback: set FALLBACK_LLM_BASE_URL (and optionally
//! FALLBACK_LLM_MODEL) to a secondary OpenAI-compatible endpoint, e.g. a local
//! freellmapi gateway. It is used only when the primary endpoint is unreachable
//! AND the request context carries no credentials/keys/positions, so sensitive
//! data never leaves the host. Either endpoint may carry a bearer token via
//! LLM_API_KEY / FALLBACK_LLM_API_KEY (freellmapi needs a `freellmapi-...` key).
//!
//! TODO: add a retrieval step over research/ + feature-store and fetch
//! simulation-engine / pricing-engine output automatically. Until then,
//! callers pass that evidence in `context`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use actix_web::{web, App, HttpServer, HttpResponse, Responder};
use common::healthz;
use serde::Deserialize;
use serde_json::{json, Value};

const SYSTEM_PROMPT_MD: &str = include_str!("../SYSTEM_PROMPT.md");

/// One OpenAI-compatible chat endpoint.
struct Endpoint {
    base_url: String,
    model: String,
    /// Optional bearer token (e.g. a freellmapi `freellmapi-...` key). Ollama needs none.
    api_key: Option<String>,
}

struct Llm {
    http: reqwest::Client,
    primary: Endpoint,
    /// Optional remote fallback (e.g. a local freellmapi gateway). Used only
    /// when the primary is unreachable AND the request carries no sensitive
    /// data, so credentials/positions never leave the host.
    fallback: Option<Endpoint>,
    system_prompt: String,
    rag: Rag,
}

#[derive(Deserialize)]
struct AskRequest {
    question: String,
    #[serde(default)]
    context: Option<Value>,
}

/// Substrings that, in any context key, mark the payload as sensitive and
/// bar it from the remote fallback. Deliberately broad: over-flagging only
/// costs a fallback (we stay local), while under-flagging could leak keys.
const SENSITIVE_MARKERS: &[&str] = &[
    "key", "secret", "wallet", "private", "seed", "mnemonic", "passphrase",
    "credential", "password", "token", "position", "balance", "address", "auth",
];

/// A string value that looks like a key/address/PEM, regardless of its field name.
fn value_looks_secret(s: &str) -> bool {
    let t = s.trim();
    if t.contains("-----BEGIN") {
        return true;
    }
    let hex = t.strip_prefix("0x").unwrap_or(t);
    // 20-byte address (40 hex) or 32-byte key (64 hex) and up.
    hex.len() >= 40 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// True if any object key matches a sensitive marker or any string value
/// looks like a secret, anywhere in the (possibly nested) context.
fn context_is_sensitive(v: &Value) -> bool {
    match v {
        Value::Object(map) => map.iter().any(|(k, val)| {
            let k = k.to_lowercase();
            SENSITIVE_MARKERS.iter().any(|m| k.contains(m)) || context_is_sensitive(val)
        }),
        Value::Array(arr) => arr.iter().any(context_is_sensitive),
        Value::String(s) => value_looks_secret(s),
        _ => false,
    }
}

/// The prompt is the body of the first ``` fence in SYSTEM_PROMPT.md.
fn extract_system_prompt(md: &str) -> String {
    md.split("```").nth(1).unwrap_or(md).trim().to_string()
}

/// Fair (no-vig) American odds; same formula as pricing-engine.
fn american_odds(p: f64) -> i64 {
    if p >= 0.5 {
        (-100.0 * p / (1.0 - p)).round() as i64
    } else {
        (100.0 * (1.0 - p) / p).round() as i64
    }
}

/// Decimal payout per unit staked for American odds; matches
/// common::american_to_decimal and pricing-engine's convention.
fn american_to_decimal(odds: i64) -> f64 {
    if odds >= 0 {
        1.0 + odds as f64 / 100.0
    } else {
        1.0 + 100.0 / (-odds) as f64
    }
}

/// Facts computed here so the LLM never has to infer them; small models
/// get favorite/underdog backwards and edge signs wrong when left to read
/// raw numbers. Edge/ROI use the same formula as arbitrage-engine:
/// edge = fair_p - implied_p, roi = fair_p * decimal - 1.
fn derived_facts(ctx: &Value) -> Vec<String> {
    let mut facts = Vec::new();
    let Some(p) = ctx["fair_probability"].as_f64().filter(|p| *p > 0.0 && *p < 1.0) else {
        return facts;
    };
    let role = if p > 0.5 {
        "the FAVORITE (more likely than not to win)"
    } else if p < 0.5 {
        "the UNDERDOG (less likely than not to win)"
    } else {
        "a PICK'EM (neither favorite nor underdog)"
    };
    facts.push(format!("Fair win probability {:.1}%, so this side is {role}.", p * 100.0));
    facts.push(format!("Fair (no-vig) American odds: {:+}.", american_odds(p)));

    // Optional market price: a $1-payout contract price in (0,1), or American odds.
    let market = ctx["price"]
        .as_f64()
        .filter(|c| *c > 0.0 && *c < 1.0)
        .map(|cost| (1.0 / cost, format!("contract price {cost:.2}")))
        .or_else(|| {
            ctx["odds"].as_i64().map(|o| (american_to_decimal(o), format!("American odds {o:+}")))
        });
    if let Some((decimal, label)) = market {
        let implied = 1.0 / decimal;
        let edge = p - implied;
        let roi = p * decimal - 1.0;
        let verdict = if edge > 0.0 {
            "POSITIVE edge: fair probability exceeds the market's implied probability, so this is a value bet"
        } else if edge < 0.0 {
            "NEGATIVE edge: the market's implied probability exceeds fair probability, so there is NO value"
        } else {
            "ZERO edge: fairly priced"
        };
        facts.push(format!(
            "Market {label} implies {:.1}% to win. Edge {:+.1} pts, ROI {:+.1}% \u{2014} {verdict}.",
            implied * 100.0,
            edge * 100.0,
            roi * 100.0
        ));
    }
    facts
}

// ---- Retrieval-augmented evidence --------------------------------------

/// Sport keyword -> the code feature-store / model-registry use.
const SPORT_KEYWORDS: &[(&str, &str)] = &[
    ("mlb", "MLB"), ("baseball", "MLB"), ("nba", "NBA"), ("basketball", "NBA"),
    ("nfl", "NFL"), ("nhl", "NHL"), ("hockey", "NHL"), ("soccer", "SOCCER"),
    ("football", "NFL"),
];

/// Uppercase tokens that look like team codes but aren't (odds/market jargon, sport codes).
const NOT_A_TEAM: &[&str] = &[
    "ML", "EV", "ROI", "YES", "NO", "US", "USA", "AI", "OK", "PM", "AM", "UFC",
    "NBA", "NFL", "MLB", "NHL", "MLS", "PGA", "ATP", "WTA", "UK", "EU", "GPT",
    "LLM", "ID", "PRP", "GSSD", "ZSD", "TOOR",
];

/// Best-effort entities from a question: a sport code and candidate team codes
/// (2-4 uppercase letters, jargon filtered out).
fn extract_entities(question: &str) -> (Option<String>, Vec<String>) {
    let lower = question.to_lowercase();
    let sport = SPORT_KEYWORDS
        .iter()
        .find(|(kw, _)| lower.split(|c: char| !c.is_alphanumeric()).any(|w| w == *kw))
        .map(|(_, code)| code.to_string());
    let mut teams = Vec::new();
    for tok in question.split(|c: char| !c.is_ascii_alphanumeric()) {
        if (2..=4).contains(&tok.len())
            && tok.chars().all(|c| c.is_ascii_uppercase())
            && !NOT_A_TEAM.contains(&tok)
            && !teams.contains(&tok.to_string())
        {
            teams.push(tok.to_string());
        }
        if teams.len() >= 4 {
            break;
        }
    }
    (sport, teams)
}

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "to", "of", "in", "on", "for",
    "and", "or", "with", "at", "by", "it", "this", "that", "will", "be", "do",
    "does", "how", "what", "why", "when", "which", "who", "vs",
];

fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2 && !STOPWORDS.contains(w))
        .map(|w| w.to_string())
        .collect()
}

struct Chunk {
    source: String,
    text: String,
    len: usize,
    tf: HashMap<String, u32>,
}

/// A minimal BM25 index over small text documents — dependency-free lexical
/// retrieval for the `research/` corpus (and any docs dirs).
struct DocIndex {
    chunks: Vec<Chunk>,
    df: HashMap<String, u32>,
    avgdl: f64,
}

impl DocIndex {
    fn from_docs(docs: Vec<(String, String)>) -> Self {
        let mut chunks = Vec::new();
        let mut df: HashMap<String, u32> = HashMap::new();
        for (source, text) in docs {
            let toks = tokenize(&text);
            if toks.is_empty() {
                continue;
            }
            let mut tf: HashMap<String, u32> = HashMap::new();
            for t in &toks {
                *tf.entry(t.clone()).or_insert(0) += 1;
            }
            for t in tf.keys() {
                *df.entry(t.clone()).or_insert(0) += 1;
            }
            chunks.push(Chunk { source, text, len: toks.len(), tf });
        }
        let avgdl = if chunks.is_empty() {
            0.0
        } else {
            chunks.iter().map(|c| c.len as f64).sum::<f64>() / chunks.len() as f64
        };
        DocIndex { chunks, df, avgdl }
    }

    /// Read small text files under each dir (recursively) into an index.
    fn from_dirs(dirs: &[PathBuf]) -> Self {
        const EXTS: &[&str] = &["md", "txt", "rst", "stan", "py", "csv"];
        let mut docs = Vec::new();
        let mut stack: Vec<PathBuf> = dirs.to_vec();
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().and_then(|x| x.to_str()).is_some_and(|x| EXTS.contains(&x)) {
                    if let Ok(meta) = e.metadata() {
                        if meta.len() > 512 * 1024 {
                            continue; // skip very large files
                        }
                    }
                    if let Ok(text) = std::fs::read_to_string(&p) {
                        docs.push((p.to_string_lossy().to_string(), text));
                    }
                }
            }
        }
        DocIndex::from_docs(docs)
    }

    fn search(&self, query: &str, k: usize) -> Vec<(String, f64, String)> {
        const K1: f64 = 1.5;
        const B: f64 = 0.75;
        let q = tokenize(query);
        let n = self.chunks.len() as f64;
        let mut scored: Vec<(usize, f64)> = self
            .chunks
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let score = q
                    .iter()
                    .map(|t| {
                        let f = *c.tf.get(t).unwrap_or(&0) as f64;
                        if f == 0.0 {
                            return 0.0;
                        }
                        let df = *self.df.get(t).unwrap_or(&0) as f64;
                        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                        idf * (f * (K1 + 1.0))
                            / (f + K1 * (1.0 - B + B * c.len as f64 / self.avgdl.max(1.0)))
                    })
                    .sum::<f64>();
                (i, score)
            })
            .filter(|(_, s)| *s > 0.0)
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored
            .into_iter()
            .take(k)
            .map(|(i, s)| {
                let c = &self.chunks[i];
                let snippet: String = c.text.chars().take(300).collect();
                (c.source.clone(), (s * 1000.0).round() / 1000.0, snippet)
            })
            .collect()
    }
}

struct Rag {
    enabled: bool,
    feature_store_url: String,
    model_registry_url: String,
    docs: DocIndex,
}

impl Rag {
    /// Auto-pull evidence relevant to the question: team features from
    /// feature-store, active models from model-registry, and research snippets.
    /// Best-effort — any source that errors is simply left out.
    async fn retrieve(&self, http: &reqwest::Client, question: &str) -> Value {
        if !self.enabled {
            return Value::Null;
        }
        let (sport, teams) = extract_entities(question);
        let mut out = serde_json::Map::new();

        if let Some(sport) = &sport {
            let mut features = Vec::new();
            for team in teams.iter().take(4) {
                let url = format!("{}/features/team/{sport}/{team}", self.feature_store_url);
                if let Ok(resp) = http.get(&url).send().await {
                    if resp.status().is_success() {
                        if let Ok(v) = resp.json::<Value>().await {
                            features.push(json!({ "team": team, "sport": sport, "data": v }));
                        }
                    }
                }
            }
            if !features.is_empty() {
                out.insert("team_features".into(), json!(features));
            }
            let url = format!("{}/models?sport={sport}&status=active", self.model_registry_url);
            if let Ok(resp) = http.get(&url).send().await {
                if resp.status().is_success() {
                    if let Ok(Value::Array(models)) = resp.json::<Value>().await {
                        if !models.is_empty() {
                            out.insert("active_models".into(), json!(models));
                        }
                    }
                }
            }
        }

        let docs = self.docs.search(question, 3);
        if !docs.is_empty() {
            out.insert(
                "research".into(),
                json!(docs
                    .into_iter()
                    .map(|(source, score, snippet)| json!({ "source": source, "score": score, "snippet": snippet }))
                    .collect::<Vec<_>>()),
            );
        }

        if out.is_empty() {
            Value::Null
        } else {
            Value::Object(out)
        }
    }
}

/// Merge caller-supplied context with auto-retrieved evidence. Caller fields stay
/// at the top level (so `derived_facts` still sees fair_probability/price/odds).
fn merge_context(caller: Option<&Value>, retrieved: &Value) -> Option<Value> {
    let mut map = match caller {
        Some(Value::Object(m)) => m.clone(),
        Some(other) => {
            let mut m = serde_json::Map::new();
            m.insert("provided".into(), other.clone());
            m
        }
        None => serde_json::Map::new(),
    };
    if !retrieved.is_null() {
        map.insert("retrieved_evidence".into(), retrieved.clone());
    }
    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
    }
}

fn user_message(question: &str, context: Option<&Value>) -> String {
    match context {
        Some(ctx) => {
            let facts = derived_facts(ctx);
            let derived = if facts.is_empty() {
                String::new()
            } else {
                format!(
                    "Derived facts (computed exactly; state them as given, never contradict them):\n- {}\n\n",
                    facts.join("\n- ")
                )
            };
            format!(
                "{derived}Evidence (model outputs, posteriors, simulations, prices, retrieved research):\n{}\n\nQuestion: {}",
                serde_json::to_string_pretty(ctx).unwrap_or_default(),
                question
            )
        }
        None => format!(
            "No model or simulation evidence was supplied or retrieved; say so explicitly.\n\nQuestion: {}",
            question
        ),
    }
}

fn bad_gateway(msg: impl std::fmt::Display) -> HttpResponse {
    eprintln!("monster-gpt: {msg}");
    HttpResponse::BadGateway().json(json!({ "error": msg.to_string() }))
}

/// One chat completion against `ep`. Returns the answer text or an error string.
async fn complete(
    http: &reqwest::Client,
    ep: &Endpoint,
    system_prompt: &str,
    user_msg: &str,
) -> Result<String, String> {
    let body = json!({
        "model": ep.model,
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": user_msg },
        ],
        "temperature": 0.2,
    });
    let url = format!("{}/chat/completions", ep.base_url);
    let mut request = http.post(&url).json(&body);
    if let Some(key) = &ep.api_key {
        request = request.bearer_auth(key);
    }
    let resp = request
        .send()
        .await
        .map_err(|e| format!("request to {url} failed: {e}"))?;
    let status = resp.status();
    let payload: Value = resp
        .json()
        .await
        .map_err(|e| format!("unreadable body ({status}): {e}"))?;
    if !status.is_success() {
        return Err(format!("returned {status}: {payload}"));
    }
    payload["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("response had no message content: {payload}"))
}

async fn retrieve_handler(llm: web::Data<Llm>, q: web::Query<RetrieveQuery>) -> impl Responder {
    HttpResponse::Ok().json(llm.rag.retrieve(&llm.http, &q.q).await)
}

#[derive(Deserialize)]
struct RetrieveQuery {
    q: String,
}

async fn ask(llm: web::Data<Llm>, req: web::Json<AskRequest>) -> impl Responder {
    if req.question.trim().is_empty() {
        return HttpResponse::BadRequest().json(json!({ "error": "question must not be empty" }));
    }
    // Auto-retrieve evidence and merge it with any caller-supplied context.
    let retrieved = llm.rag.retrieve(&llm.http, &req.question).await;
    let retrieved_used = !retrieved.is_null();
    let merged = merge_context(req.context.as_ref(), &retrieved);
    let user_msg = user_message(&req.question, merged.as_ref());

    let primary_err = match complete(&llm.http, &llm.primary, &llm.system_prompt, &user_msg).await {
        Ok(answer) => {
            return HttpResponse::Ok().json(
                json!({ "answer": answer, "model": llm.primary.model, "fallback": false, "retrieved": retrieved_used }),
            );
        }
        Err(e) => format!("primary LLM ({}): {e}", llm.primary.base_url),
    };
    eprintln!("monster-gpt: {primary_err}");

    // Primary failed. Fall back only if one is configured and nothing sensitive
    // would leave the host. The merged context (caller + retrieved) is what's guarded.
    let sensitive = merged.as_ref().is_some_and(context_is_sensitive);
    match &llm.fallback {
        Some(fb) if !sensitive => {
            match complete(&llm.http, fb, &llm.system_prompt, &user_msg).await {
                Ok(answer) => HttpResponse::Ok().json(
                    json!({ "answer": answer, "model": fb.model, "fallback": true, "retrieved": retrieved_used }),
                ),
                Err(e) => bad_gateway(format!("{primary_err}; fallback LLM ({}): {e}", fb.base_url)),
            }
        }
        Some(_) => bad_gateway(format!(
            "{primary_err}; remote fallback skipped because the request context is sensitive"
        )),
        None => bad_gateway(primary_err),
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    match dotenvy::dotenv() {
        Ok(path) => println!("monster-gpt: loaded {}", path.display()),
        Err(e) if e.not_found() => println!("monster-gpt: no .env found; using environment only"),
        Err(e) => eprintln!("monster-gpt: could not parse .env: {e}"),
    }
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8085);
    // A set-but-empty env var reads as None, so a blank key never sends a bearer.
    let env_opt = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let primary = Endpoint {
        base_url: env_opt("LLM_BASE_URL")
            .unwrap_or_else(|| "http://localhost:11434/v1".into())
            .trim_end_matches('/')
            .to_string(),
        model: env_opt("LLM_MODEL").unwrap_or_else(|| "qwen2.5:14b".into()),
        api_key: env_opt("LLM_API_KEY"),
    };
    // Fallback is opt-in: set FALLBACK_LLM_BASE_URL (e.g. a local freellmapi
    // gateway). It is used only for non-sensitive requests when the primary is down.
    let fallback = env_opt("FALLBACK_LLM_BASE_URL").map(|url| Endpoint {
        base_url: url.trim_end_matches('/').to_string(),
        model: env_opt("FALLBACK_LLM_MODEL").unwrap_or_else(|| "auto".into()),
        api_key: env_opt("FALLBACK_LLM_API_KEY"),
    });
    // RAG: index the research corpus (and any extra docs dirs), and point at
    // feature-store / model-registry for structured auto-retrieval.
    let rag_enabled = std::env::var("RAG_ENABLED").map(|v| v != "false" && v != "0").unwrap_or(true);
    let research_dirs: Vec<PathBuf> = env_opt("RESEARCH_DIR")
        .unwrap_or_else(|| "research,docs".into())
        .split(',')
        .map(|s| PathBuf::from(s.trim()))
        .collect();
    let docs = if rag_enabled { DocIndex::from_dirs(&research_dirs) } else { DocIndex::from_docs(vec![]) };
    println!("monster-gpt: RAG {} ({} research chunks indexed)", if rag_enabled { "on" } else { "off" }, docs.chunks.len());
    let rag = Rag {
        enabled: rag_enabled,
        feature_store_url: env_opt("FEATURE_STORE_URL")
            .unwrap_or_else(|| "http://localhost:8118".into())
            .trim_end_matches('/')
            .to_string(),
        model_registry_url: env_opt("MODEL_REGISTRY_URL")
            .unwrap_or_else(|| "http://localhost:8084".into())
            .trim_end_matches('/')
            .to_string(),
        docs,
    };
    let llm = web::Data::new(Llm {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        primary,
        fallback,
        system_prompt: extract_system_prompt(SYSTEM_PROMPT_MD),
        rag,
    });
    match &llm.fallback {
        Some(fb) => println!(
            "monster-gpt listening on 0.0.0.0:{port} (LLM {} @ {}; non-sensitive fallback {} @ {})",
            llm.primary.model, llm.primary.base_url, fb.model, fb.base_url
        ),
        None => println!(
            "monster-gpt listening on 0.0.0.0:{port} (LLM {} @ {}; no fallback)",
            llm.primary.model, llm.primary.base_url
        ),
    }
    HttpServer::new(move || {
        App::new()
            .app_data(llm.clone())
            .route("/healthz", web::get().to(healthz))
            .route("/ask", web::post().to(ask))
            .route("/retrieve", web::get().to(retrieve_handler))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_is_fenced_body() {
        let p = extract_system_prompt(SYSTEM_PROMPT_MD);
        assert!(p.starts_with("You are MonsterGPT."));
        assert!(p.ends_with("Confidence"));
        assert!(!p.contains("```"));
    }

    #[test]
    fn user_message_flags_missing_evidence() {
        assert!(user_message("Q?", None).contains("No model or simulation evidence"));
        let ctx = json!({ "p": 0.6 });
        assert!(user_message("Q?", Some(&ctx)).contains("\"p\": 0.6"));
        assert!(!user_message("Q?", Some(&ctx)).contains("Derived facts"));
    }

    #[test]
    fn derived_facts_label_favorite_and_underdog() {
        let fav = derived_facts(&json!({ "fair_probability": 0.61 })).join(" ");
        assert!(fav.contains("FAVORITE") && fav.contains("-156"), "{fav}");
        let dog = derived_facts(&json!({ "fair_probability": 0.39 })).join(" ");
        assert!(dog.contains("UNDERDOG") && dog.contains("+156"), "{dog}");
        let even = derived_facts(&json!({ "fair_probability": 0.5 })).join(" ");
        assert!(even.contains("PICK'EM") && even.contains("-100"), "{even}");
        assert!(derived_facts(&json!({ "fair_probability": 1.2 })).is_empty());
        let ctx = json!({ "fair_probability": 0.61 });
        assert!(user_message("Q?", Some(&ctx)).starts_with("Derived facts"));
    }

    #[test]
    fn derived_facts_compute_edge_from_market() {
        // Contract price below fair cost => positive edge (matches arbitrage-engine).
        let value = derived_facts(&json!({ "fair_probability": 0.61, "price": 0.53 })).join(" ");
        assert!(value.contains("POSITIVE edge"), "{value}");
        assert!(value.contains("+8.0 pts") && value.contains("+15.1%"), "{value}");
        // Overpriced contract => no value.
        let no_value = derived_facts(&json!({ "fair_probability": 0.61, "price": 0.70 })).join(" ");
        assert!(no_value.contains("NEGATIVE edge"), "{no_value}");
        // American odds path.
        let odds = derived_facts(&json!({ "fair_probability": 0.61, "odds": -130 })).join(" ");
        assert!(odds.contains("American odds -130"), "{odds}");
        assert!(odds.contains("POSITIVE edge"), "{odds}");
    }

    #[test]
    fn normal_betting_context_is_not_sensitive() {
        // A routine evidence payload must stay eligible for the fallback.
        let ctx = json!({
            "fair_probability": 0.61, "price": 0.53, "odds": -130,
            "event_id": "NYY-BOS-2026-10-08", "market_id": "KXMLBGAME", "venue": "kalshi"
        });
        assert!(!context_is_sensitive(&ctx));
    }

    #[test]
    fn credentials_and_keys_are_flagged_sensitive() {
        assert!(context_is_sensitive(&json!({ "api_key": "x" })));
        assert!(context_is_sensitive(&json!({ "ODDS_API_KEYS": "x" })));
        assert!(context_is_sensitive(&json!({ "balance": 1000 })));
        // Nested under a benign key.
        assert!(context_is_sensitive(&json!({ "account": { "private_key": "x" } })));
        // Secret-looking value even without a telltale key name.
        assert!(context_is_sensitive(&json!({ "note": "0x4af3c2d19b8e7a6f5c4d3e2b1a0f9e8d7c6b5a4938271605f4e3d2c1b0a99887" })));
        assert!(context_is_sensitive(&json!({ "pem": "-----BEGIN PRIVATE KEY-----" })));
        // Arrays are walked too.
        assert!(context_is_sensitive(&json!([{ "seed_phrase": "a b c" }])));
    }

    #[test]
    fn extracts_sport_and_team_codes() {
        let (sport, teams) = extract_entities("Is there value on the Yankees ML? NYY vs BOS in MLB");
        assert_eq!(sport.as_deref(), Some("MLB"));
        assert!(teams.contains(&"NYY".to_string()) && teams.contains(&"BOS".to_string()));
        // "ML" (moneyline) and "MLB" (sport code) are not treated as teams.
        assert!(!teams.contains(&"ML".to_string()));
        assert!(!teams.contains(&"MLB".to_string()));
    }

    #[test]
    fn no_sport_when_none_mentioned() {
        let (sport, _) = extract_entities("who wins the election");
        assert!(sport.is_none());
    }

    #[test]
    fn bm25_ranks_relevant_doc_first() {
        let idx = DocIndex::from_docs(vec![
            ("elo.md".into(), "The Elo model rates team strength from match results.".into()),
            ("poisson.md".into(), "A Poisson model predicts goal counts in soccer.".into()),
            ("empty.md".into(), "".into()),
        ]);
        let hits = idx.search("how does the elo rating work", 2);
        assert!(!hits.is_empty());
        assert!(hits[0].0.contains("elo"), "top hit was {}", hits[0].0);
    }

    #[test]
    fn merge_context_keeps_caller_fields_top_level() {
        let caller = json!({ "fair_probability": 0.61 });
        let retrieved = json!({ "team_features": [] });
        let merged = merge_context(Some(&caller), &retrieved).unwrap();
        assert_eq!(merged["fair_probability"], json!(0.61)); // derived_facts still sees it
        assert!(merged["retrieved_evidence"].is_object());
        // Null retrieval leaves caller context untouched.
        let only_caller = merge_context(Some(&caller), &Value::Null).unwrap();
        assert!(only_caller.get("retrieved_evidence").is_none());
        assert!(merge_context(None, &Value::Null).is_none());
    }
}
