//! Model-provider interface with streaming. Two adapters today:
//!   - `ollama`: native /api/chat (NDJSON stream) so the context window
//!     (`num_ctx`) is set explicitly instead of Ollama's silent 2048 default;
//!   - `openai`: any OpenAI-compatible /chat/completions (SSE stream), e.g.
//!     vLLM, llama.cpp server, or a future Monster GPT scratch-model server.
//! Plus a context manager that keeps a request inside the model's window.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider { Ollama, OpenAi }

#[derive(Clone, Debug, Serialize)]
pub struct ModelCfg {
    pub id: String,
    pub provider: Provider,
    #[serde(skip)]
    pub base_url: String,
    #[serde(skip)]
    pub api_key: Option<String>,
    /// context window in tokens (sent to Ollama as num_ctx)
    pub num_ctx: usize,
    /// cap on generated tokens
    pub max_tokens: usize,
    pub temperature: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChatMsg { pub role: String, pub content: String }

impl ChatMsg {
    pub fn new(role: &str, content: impl Into<String>) -> Self { Self { role: role.into(), content: content.into() } }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct StreamStats {
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
    pub completion_tokens: u64,
    pub prompt_tokens: Option<u64>,
    pub tokens_per_s: Option<f64>,
    pub cancelled: bool,
}

/// Conservative token estimate (about 3.5 characters per token for English
/// and JSON); used to stay inside the window, never to bill.
pub fn est_tokens(s: &str) -> usize { (s.chars().count() as f64 / 3.5).ceil() as usize + 4 }

/// Fit system + history + evidence + question into `num_ctx - reserve` tokens.
/// Order of sacrifice: oldest history turns, then evidence is cut (with a
/// marker), never the system prompt or the question. Returns the messages and
/// what was dropped.
pub fn fit_context(system: &str, history: &[ChatMsg], evidence: &str, question: &str, num_ctx: usize, reserve_out: usize) -> (Vec<ChatMsg>, Value) {
    let budget = num_ctx.saturating_sub(reserve_out);
    let fixed = est_tokens(system) + est_tokens(question) + 32;
    let mut evidence = evidence.to_string();
    let mut ev_tokens = est_tokens(&evidence);
    let mut hist: Vec<ChatMsg> = history.to_vec();
    let hist_tokens = |h: &[ChatMsg]| h.iter().map(|m| est_tokens(&m.content)).sum::<usize>();
    let mut dropped_turns = 0;
    while !hist.is_empty() && fixed + ev_tokens + hist_tokens(&hist) > budget {
        hist.remove(0);
        dropped_turns += 1;
    }
    let mut evidence_cut = false;
    if fixed + ev_tokens > budget {
        let keep_tokens = budget.saturating_sub(fixed + 16);
        let keep_chars = (keep_tokens as f64 * 3.5) as usize;
        evidence = evidence.chars().take(keep_chars).collect::<String>() + "\n[evidence truncated to fit the model's context window]";
        ev_tokens = est_tokens(&evidence);
        evidence_cut = true;
    }
    let user = if evidence.trim().is_empty() { question.to_string() } else { format!("{evidence}\n\nQuestion: {question}") };
    let mut msgs = vec![ChatMsg::new("system", system)];
    msgs.extend(hist);
    msgs.push(ChatMsg::new("user", user));
    let used: usize = msgs.iter().map(|m| est_tokens(&m.content)).sum();
    (msgs, json!({ "budget": budget, "used_est": used, "evidence_tokens_est": ev_tokens, "dropped_history_turns": dropped_turns, "evidence_truncated": evidence_cut }))
}

/// One NDJSON line from Ollama's /api/chat stream -> (text delta, done, eval counts).
pub fn parse_ollama_line(line: &str) -> Option<(String, bool, Option<u64>, Option<u64>)> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if let Some(e) = v["error"].as_str() { return Some((format!("\u{0}ERR{e}"), true, None, None)); }
    Some((v["message"]["content"].as_str().unwrap_or("").to_string(), v["done"].as_bool().unwrap_or(false), v["eval_count"].as_u64(), v["prompt_eval_count"].as_u64()))
}

/// One SSE line from an OpenAI-compatible stream -> text delta / done.
pub fn parse_sse_line(line: &str) -> Option<(String, bool)> {
    let data = line.trim().strip_prefix("data:")?.trim();
    if data == "[DONE]" { return Some((String::new(), true)); }
    let v: Value = serde_json::from_str(data).ok()?;
    Some((v["choices"][0]["delta"]["content"].as_str().unwrap_or("").to_string(), false))
}

/// Stream a chat completion. `on_delta` receives each text piece and returns
/// false to stop (e.g. the client went away). `cancel` is checked between chunks;
/// dropping the response closes the connection so the model server stops too.
pub async fn stream_chat<F, Fut>(http: &reqwest::Client, m: &ModelCfg, msgs: &[ChatMsg], cancel: &AtomicBool, mut on_delta: F) -> Result<StreamStats, String>
where F: FnMut(String) -> Fut, Fut: std::future::Future<Output = bool> {
    let t0 = Instant::now();
    let (url, body) = match m.provider {
        Provider::Ollama => (format!("{}/api/chat", m.base_url), json!({ "model": m.id, "messages": msgs, "stream": true,
            "options": { "num_ctx": m.num_ctx, "temperature": m.temperature, "num_predict": m.max_tokens } })),
        Provider::OpenAi => (format!("{}/chat/completions", m.base_url), json!({ "model": m.id, "messages": msgs, "stream": true,
            "temperature": m.temperature, "max_tokens": m.max_tokens })),
    };
    let mut req = http.post(&url).json(&body);
    if let Some(k) = &m.api_key { req = req.bearer_auth(k); }
    let resp = req.send().await.map_err(|e| format!("{} unreachable: {e}", m.provider_name()))?;
    if !resp.status().is_success() {
        let st = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("{} returned {st}: {}", m.provider_name(), body.chars().take(300).collect::<String>()));
    }
    let mut stats = StreamStats::default();
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    let mut pieces = 0u64;
    'outer: while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::Relaxed) { stats.cancelled = true; break; }
        let chunk = chunk.map_err(|e| format!("stream broke: {e}"))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(i) = buf.find('\n') {
            let line: String = buf.drain(..=i).collect();
            if line.trim().is_empty() { continue; }
            let (text, done) = match m.provider {
                Provider::Ollama => match parse_ollama_line(&line) {
                    Some((t, _, _, _)) if t.starts_with("\u{0}ERR") => return Err(t.trim_start_matches("\u{0}ERR").to_string()),
                    Some((t, d, eval, prompt)) => { if d { stats.completion_tokens = eval.unwrap_or(pieces); stats.prompt_tokens = prompt; } (t, d) }
                    None => continue,
                },
                Provider::OpenAi => match parse_sse_line(&line) { Some(x) => x, None => continue },
            };
            if !text.is_empty() {
                if stats.ttft_ms.is_none() { stats.ttft_ms = Some(t0.elapsed().as_millis() as u64); }
                pieces += 1;
                if !on_delta(text).await { stats.cancelled = true; break 'outer; }
                if cancel.load(Ordering::Relaxed) { stats.cancelled = true; break 'outer; }
            }
            if done { break 'outer; }
        }
    }
    stats.total_ms = t0.elapsed().as_millis() as u64;
    if stats.completion_tokens == 0 { stats.completion_tokens = pieces; }
    if let Some(ttft) = stats.ttft_ms {
        let gen_ms = stats.total_ms.saturating_sub(ttft).max(1);
        stats.tokens_per_s = Some(((stats.completion_tokens as f64) / (gen_ms as f64 / 1000.0) * 10.0).round() / 10.0);
    }
    Ok(stats)
}

impl ModelCfg {
    fn provider_name(&self) -> &'static str { match self.provider { Provider::Ollama => "ollama", Provider::OpenAi => "openai-compatible endpoint" } }
}

/// Models from env: LLM_BASE_URL / LLM_MODEL (primary), MONSTER_GPT_MODELS
/// (extra selectable ids on the same server, comma-separated), LLM_PROVIDER
/// (ollama|openai; default ollama when the URL is Ollama's port), LLM_NUM_CTX,
/// LLM_MAX_TOKENS.
pub fn models_from_env(env: &dyn Fn(&str) -> Option<String>) -> Vec<ModelCfg> {
    let base = env("LLM_BASE_URL").unwrap_or_else(|| "http://localhost:11434/v1".into()).trim_end_matches('/').to_string();
    let provider = match env("LLM_PROVIDER").as_deref() {
        Some("openai") => Provider::OpenAi,
        Some("ollama") => Provider::Ollama,
        _ if base.contains(":11434") => Provider::Ollama,
        _ => Provider::OpenAi,
    };
    let base_url = if provider == Provider::Ollama { base.trim_end_matches("/v1").to_string() } else { base };
    let num_ctx = env("LLM_NUM_CTX").and_then(|v| v.parse().ok()).unwrap_or(4096);
    let max_tokens = env("LLM_MAX_TOKENS").and_then(|v| v.parse().ok()).unwrap_or(450);
    let primary = env("LLM_MODEL").unwrap_or_else(|| "qwen2.5:7b".into());
    let mut ids = vec![primary];
    for extra in env("MONSTER_GPT_MODELS").unwrap_or_default().split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if !ids.iter().any(|x| x == extra) { ids.push(extra.to_string()); }
    }
    ids.into_iter().map(|id| ModelCfg { id, provider, base_url: base_url.clone(), api_key: env("LLM_API_KEY"), num_ctx, max_tokens, temperature: 0.3 }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_drops_oldest_history_first_and_never_exceeds() {
        let hist: Vec<ChatMsg> = (0..10).map(|i| ChatMsg::new(if i % 2 == 0 { "user" } else { "assistant" }, "x".repeat(700))).collect();
        let (msgs, info) = fit_context("system rules", &hist, &"e".repeat(2000), "question?", 1024, 300);
        let used: usize = msgs.iter().map(|m| est_tokens(&m.content)).sum();
        assert!(used <= 1024 - 300, "used {used}");
        assert!(info["dropped_history_turns"].as_u64().unwrap() > 0);
        assert_eq!(msgs[0].role, "system");
        assert!(msgs.last().unwrap().content.ends_with("Question: question?"));
        // the surviving history is the most recent turns
        if msgs.len() > 2 { assert_eq!(msgs[msgs.len() - 2].role, "assistant"); }
    }

    #[test]
    fn evidence_is_truncated_with_a_marker_when_alone_too_big() {
        let (msgs, info) = fit_context("sys", &[], &"e".repeat(20_000), "q", 2048, 400);
        assert_eq!(info["evidence_truncated"], true);
        assert!(msgs[1].content.contains("[evidence truncated"));
        let used: usize = msgs.iter().map(|m| est_tokens(&m.content)).sum();
        assert!(used <= 2048 - 400, "used {used}");
    }

    #[test]
    fn parses_stream_lines() {
        let (t, d, _, _) = parse_ollama_line(r#"{"message":{"role":"assistant","content":"Hel"},"done":false}"#).unwrap();
        assert_eq!((t.as_str(), d), ("Hel", false));
        let (_, d, eval, prompt) = parse_ollama_line(r#"{"message":{"content":""},"done":true,"eval_count":42,"prompt_eval_count":900}"#).unwrap();
        assert!(d);
        assert_eq!((eval, prompt), (Some(42), Some(900)));
        assert_eq!(parse_sse_line(r#"data: {"choices":[{"delta":{"content":"lo"}}]}"#), Some(("lo".into(), false)));
        assert_eq!(parse_sse_line("data: [DONE]"), Some((String::new(), true)));
        assert_eq!(parse_sse_line(": keep-alive"), None);
    }

    #[test]
    fn env_config_picks_provider_and_strips_v1() {
        let env = |k: &str| match k { "LLM_BASE_URL" => Some("http://localhost:11434/v1".into()), "MONSTER_GPT_MODELS" => Some("qwen2.5:0.5b, qwen2.5:7b".into()), _ => None };
        let ms = models_from_env(&env);
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0].provider, Provider::Ollama);
        assert_eq!(ms[0].base_url, "http://localhost:11434");
        assert_eq!(ms[0].id, "qwen2.5:7b");
        let env2 = |k: &str| match k { "LLM_BASE_URL" => Some("http://localhost:8000/v1".into()), _ => None };
        assert_eq!(models_from_env(&env2)[0].provider, Provider::OpenAi);
    }
}
