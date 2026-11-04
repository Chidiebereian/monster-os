//! WebSocket protocol v1 (docs/architecture/monster-gpt.md section 4).
//! Every frame is JSON with `"v": 1` and a `type`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const VERSION: u32 = 1;
/// Longest user message accepted (characters).
pub const MAX_TEXT: usize = 8000;

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMsg {
    Hello { #[serde(default)] v: Option<u32>, token: String },
    Message {
        #[serde(default)] v: Option<u32>,
        request_id: String,
        #[serde(default)] conversation_id: Option<String>,
        text: String,
        /// optional caller evidence, e.g. a game brief from a matchup page
        #[serde(default)] context: Option<Value>,
        /// model id from the `ready.models` list
        #[serde(default)] model: Option<String>,
    },
    Cancel { #[serde(default)] v: Option<u32>, request_id: String },
    Ping { #[serde(default)] v: Option<u32> },
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Ready { session: String, owner: String, models: Value, limits: Value },
    Ack { request_id: String, conversation_id: Option<String>, duplicate: bool },
    /// stage: route | tool | retrieval | model ; state: started | queued | completed | failed
    Status { request_id: String, stage: String, name: String, state: String, #[serde(skip_serializing_if = "Option::is_none")] detail: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] ms: Option<u64> },
    Card { request_id: String, card: Value },
    /// v2: the players and teams a research question resolved to (canonical ids)
    Entities { request_id: String, entities: Value },
    Sources { request_id: String, sources: Value },
    Delta { request_id: String, seq: u64, text: String },
    /// discard the text streamed so far (a checked rewrite follows)
    Reset { request_id: String, reason: String },
    Done { request_id: String, message_id: Option<String>, model: Option<String>, cancelled: bool, usage: Value },
    Error { request_id: Option<String>, code: String, message: String },
    Pong,
}

impl ServerMsg {
    pub fn to_json(&self) -> String {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Value::Object(m) = &mut v { m.insert("v".into(), Value::from(VERSION)); }
        v.to_string()
    }
    pub fn error(request_id: Option<&str>, code: &str, message: impl Into<String>) -> Self {
        ServerMsg::Error { request_id: request_id.map(|s| s.to_string()), code: code.into(), message: message.into() }
    }
}

/// Parse a client frame; unknown types, unknown fields and other versions are errors.
pub fn parse_client(text: &str) -> Result<ClientMsg, String> {
    let m: ClientMsg = serde_json::from_str(text).map_err(|e| format!("bad frame: {e}"))?;
    let v = match &m { ClientMsg::Hello { v, .. } | ClientMsg::Message { v, .. } | ClientMsg::Cancel { v, .. } | ClientMsg::Ping { v } => *v };
    if let Some(v) = v { if v != VERSION { return Err(format!("unsupported protocol version {v}; this server speaks v{VERSION}")); } }
    if let ClientMsg::Message { request_id, text, .. } = &m {
        if request_id.is_empty() || request_id.len() > 64 { return Err("request_id must be 1-64 characters".into()); }
        if text.trim().is_empty() { return Err("text must not be empty".into()); }
        if text.chars().count() > MAX_TEXT { return Err(format!("text longer than {MAX_TEXT} characters")); }
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_client_frames() {
        assert_eq!(parse_client(r#"{"type":"ping"}"#), Ok(ClientMsg::Ping { v: None }));
        let m = parse_client(r#"{"v":1,"type":"message","request_id":"r1","text":"best bets?"}"#).unwrap();
        assert!(matches!(m, ClientMsg::Message { ref request_id, .. } if request_id == "r1"));
        assert!(parse_client(r#"{"type":"hello","token":"t"}"#).is_ok());
    }

    #[test]
    fn rejects_bad_frames() {
        assert!(parse_client(r#"{"type":"launch_missiles"}"#).is_err());
        assert!(parse_client(r#"{"type":"ping","extra":1}"#).is_err());
        assert!(parse_client(r#"{"v":2,"type":"ping"}"#).unwrap_err().contains("version"));
        assert!(parse_client(r#"{"type":"message","request_id":"","text":"x"}"#).is_err());
        assert!(parse_client(r#"{"type":"message","request_id":"r","text":"   "}"#).is_err());
        let long = format!(r#"{{"type":"message","request_id":"r","text":"{}"}}"#, "a".repeat(MAX_TEXT + 1));
        assert!(parse_client(&long).unwrap_err().contains("longer"));
        assert!(parse_client("not json").is_err());
    }

    #[test]
    fn server_frames_carry_version_and_type() {
        let s = ServerMsg::Delta { request_id: "r".into(), seq: 3, text: "hi".into() }.to_json();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "delta");
        assert_eq!(v["seq"], 3);
        let e: Value = serde_json::from_str(&ServerMsg::error(Some("r"), "busy", "one at a time").to_json()).unwrap();
        assert_eq!((e["type"].as_str(), e["code"].as_str()), (Some("error"), Some("busy")));
    }
}
