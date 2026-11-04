//! Conversation storage (Postgres, migration 0030). Every read filters by
//! owner, so a conversation id from a client is never trusted on its own.

use serde_json::{json, Value};
use tokio_postgres::types::Json;
use tokio_postgres::{Client, NoTls};

use crate::llm::ChatMsg;
use crate::router::Filters;

pub struct Store { db: Client }

pub fn new_id(prefix: &str) -> String {
    use rand::Rng;
    let n: u128 = rand::thread_rng().gen();
    format!("{prefix}_{:x}", n)[..prefix.len() + 1 + 20].to_string()
}

/// A stored assistant turn, replayed when a request id is resent.
pub struct StoredAnswer { pub message_id: String, pub content: String, pub card: Value, pub sources: Value, pub model: Option<String>, pub usage: Value, pub cancelled: bool }

fn e(x: tokio_postgres::Error) -> String { x.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| x.to_string()) }

impl Store {
    pub async fn connect(url: &str) -> Result<Self, String> {
        let (db, conn) = tokio_postgres::connect(url, NoTls).await.map_err(e)?;
        actix_web::rt::spawn(async move { if let Err(err) = conn.await { eprintln!("monster-gpt: postgres connection ended: {err}"); } });
        db.query("SELECT 1 FROM gpt_conversations LIMIT 1", &[]).await
            .map_err(|x| format!("{} (apply migration 0030_monster_gpt.sql)", e(x)))?;
        Ok(Self { db })
    }

    /// Existing conversation (owner-checked) or a new one titled from the first message.
    pub async fn conversation(&self, owner: &str, id: Option<&str>, first_text: &str) -> Result<String, String> {
        if let Some(id) = id {
            let row = self.db.query_opt("SELECT id FROM gpt_conversations WHERE id=$1 AND owner=$2", &[&id, &owner]).await.map_err(e)?;
            return row.map(|r| r.get(0)).ok_or_else(|| "conversation not found".to_string());
        }
        let id = new_id("c");
        let title: String = first_text.trim().chars().take(80).collect();
        self.db.execute("INSERT INTO gpt_conversations (id, owner, title) VALUES ($1, $2, $3)", &[&id, &owner, &title]).await.map_err(e)?;
        Ok(id)
    }

    pub async fn list(&self, owner: &str) -> Result<Value, String> {
        let rows = self.db.query("SELECT c.id, c.title, c.updated_at, (SELECT count(*) FROM gpt_messages m WHERE m.conversation_id=c.id) \
            FROM gpt_conversations c WHERE owner=$1 ORDER BY updated_at DESC LIMIT 100", &[&owner]).await.map_err(e)?;
        Ok(json!(rows.iter().map(|r| json!({ "id": r.get::<_, String>(0), "title": r.get::<_, String>(1),
            "updated_at": r.get::<_, chrono::DateTime<chrono::Utc>>(2), "messages": r.get::<_, i64>(3) })).collect::<Vec<_>>()))
    }

    pub async fn messages(&self, owner: &str, conv: &str) -> Result<Value, String> {
        let ok = self.db.query_opt("SELECT 1 FROM gpt_conversations WHERE id=$1 AND owner=$2", &[&conv, &owner]).await.map_err(e)?;
        if ok.is_none() { return Err("conversation not found".into()); }
        let rows = self.db.query("SELECT id, request_id, role, content, card, sources, model, usage, cancelled, created_at, feedback FROM gpt_messages \
            WHERE conversation_id=$1 ORDER BY created_at, role DESC", &[&conv]).await.map_err(e)?;
        Ok(json!(rows.iter().map(|r| json!({ "id": r.get::<_, String>(0), "request_id": r.get::<_, Option<String>>(1), "role": r.get::<_, String>(2),
            "content": r.get::<_, String>(3), "card": r.get::<_, Option<Json<Value>>>(4).map(|j| j.0), "sources": r.get::<_, Option<Json<Value>>>(5).map(|j| j.0),
            "model": r.get::<_, Option<String>>(6), "usage": r.get::<_, Option<Json<Value>>>(7).map(|j| j.0), "cancelled": r.get::<_, bool>(8),
            "created_at": r.get::<_, chrono::DateTime<chrono::Utc>>(9), "feedback": r.get::<_, Option<i16>>(10) })).collect::<Vec<_>>()))
    }

    /// Rate one of the owner's assistant messages: 1, -1, or 0 to clear (migration 0032).
    pub async fn set_feedback(&self, owner: &str, message_id: &str, rating: i16) -> Result<bool, String> {
        let v: Option<i16> = if rating == 0 { None } else { Some(rating.signum()) };
        Ok(self.db.execute("UPDATE gpt_messages m SET feedback=$3, feedback_at=now() FROM gpt_conversations c             WHERE m.id=$1 AND m.role='assistant' AND c.id=m.conversation_id AND c.owner=$2", &[&message_id, &owner, &v]).await.map_err(e)? > 0)
    }

    pub async fn delete(&self, owner: &str, conv: &str) -> Result<bool, String> {
        Ok(self.db.execute("DELETE FROM gpt_conversations WHERE id=$1 AND owner=$2", &[&conv, &owner]).await.map_err(e)? > 0)
    }

    /// Last `n` turns as chat messages, plus the router state of the latest assistant turn.
    pub async fn history(&self, conv: &str, n: i64) -> Result<(Vec<ChatMsg>, Option<Filters>), String> {
        let rows = self.db.query("SELECT role, content, state FROM gpt_messages WHERE conversation_id=$1 AND NOT cancelled \
            ORDER BY created_at DESC, role ASC LIMIT $2", &[&conv, &n]).await.map_err(e)?;
        let mut prev: Option<Filters> = None;
        let mut msgs: Vec<ChatMsg> = Vec::new();
        for r in rows.iter().rev() {
            let role: String = r.get(0);
            if role == "assistant" {
                if let Some(s) = r.get::<_, Option<Json<Value>>>(2) { if let Ok(f) = serde_json::from_value::<Filters>(s.0["filters"].clone()) { prev = Some(f); } }
            }
            msgs.push(ChatMsg::new(&role, r.get::<_, String>(1)));
        }
        Ok((msgs, prev))
    }

    /// Per-sentence evidence for an answer (gpt_claims, write-once).
    pub async fn save_claims(&self, message_id: &str, claims: &[crate::verify::Claim]) {
        for c in claims {
            let _ = self.db.execute("INSERT INTO gpt_claims (message_id, sentence_ix, text, status, facts, issues) VALUES ($1, $2, $3, $4, $5, $6) \
                ON CONFLICT (message_id, sentence_ix) DO NOTHING",
                &[&message_id, &(c.sentence_ix as i32), &c.text, &c.status.as_str(), &Json(serde_json::json!(c.citations)), &Json(serde_json::json!(c.issues))]).await;
        }
    }

    /// State saved with the newest assistant turn (research question, entities, a pending clarification).
    pub async fn last_state(&self, conv: &str) -> Option<Value> {
        self.db.query_opt("SELECT state FROM gpt_messages WHERE conversation_id=$1 AND role='assistant' AND NOT cancelled ORDER BY created_at DESC LIMIT 1", &[&conv])
            .await.ok().flatten().and_then(|r| r.get::<_, Option<Json<Value>>>(0)).map(|j| j.0)
    }

    /// Rolling summary of the conversation's older turns.
    pub async fn summary(&self, conv: &str) -> Option<String> {
        self.db.query_opt("SELECT summary FROM gpt_conversations WHERE id=$1", &[&conv]).await.ok().flatten().and_then(|r| r.get(0))
    }
    pub async fn set_summary(&self, conv: &str, s: &str) -> Result<(), String> {
        self.db.execute("UPDATE gpt_conversations SET summary=$2 WHERE id=$1", &[&conv, &s]).await.map(|_| ()).map_err(e)
    }
    /// All non-cancelled turns except the newest `keep`, oldest first.
    pub async fn older(&self, conv: &str, keep: i64) -> Result<Vec<ChatMsg>, String> {
        let rows = self.db.query("SELECT role, content FROM gpt_messages WHERE conversation_id=$1 AND NOT cancelled ORDER BY created_at, role DESC", &[&conv]).await.map_err(e)?;
        let n = rows.len().saturating_sub(keep as usize);
        Ok(rows.iter().take(n).map(|r| ChatMsg::new(&r.get::<_, String>(0), r.get::<_, String>(1))).collect())
    }

    pub async fn answer_for(&self, conv: &str, request_id: &str) -> Result<Option<StoredAnswer>, String> {
        let r = self.db.query_opt("SELECT id, content, card, sources, model, usage, cancelled FROM gpt_messages \
            WHERE conversation_id=$1 AND request_id=$2 AND role='assistant'", &[&conv, &request_id]).await.map_err(e)?;
        Ok(r.map(|r| StoredAnswer { message_id: r.get(0), content: r.get(1), card: r.get::<_, Option<Json<Value>>>(2).map(|j| j.0).unwrap_or(Value::Null),
            sources: r.get::<_, Option<Json<Value>>>(3).map(|j| j.0).unwrap_or(json!([])), model: r.get(4),
            usage: r.get::<_, Option<Json<Value>>>(5).map(|j| j.0).unwrap_or(Value::Null), cancelled: r.get(6) }))
    }

    /// Store the user turn and the assistant turn for one request.
    #[allow(clippy::too_many_arguments)]
    pub async fn save_turn(&self, conv: &str, request_id: &str, user_text: &str, answer: &str, card: &Value, sources: &Value, state: &Value,
                           model: Option<&str>, usage: &Value, cancelled: bool) -> Result<String, String> {
        let uid = new_id("m");
        let aid = new_id("m");
        self.db.execute("INSERT INTO gpt_messages (id, conversation_id, request_id, role, content) VALUES ($1,$2,$3,'user',$4) ON CONFLICT DO NOTHING",
            &[&uid, &conv, &request_id, &user_text]).await.map_err(e)?;
        self.db.execute("INSERT INTO gpt_messages (id, conversation_id, request_id, role, content, card, sources, state, model, usage, cancelled) \
            VALUES ($1,$2,$3,'assistant',$4,$5,$6,$7,$8,$9,$10) ON CONFLICT DO NOTHING",
            &[&aid, &conv, &request_id, &answer, &Json(card), &Json(sources), &Json(state), &model, &Json(usage), &cancelled]).await.map_err(e)?;
        self.db.execute("UPDATE gpt_conversations SET updated_at=now() WHERE id=$1", &[&conv]).await.map_err(e)?;
        Ok(aid)
    }

    pub async fn audit(&self, owner: &str, conv: Option<&str>, request_id: &str, tool: &str, args: &Value, ok: bool, ms: u64, error: Option<&str>) {
        let _ = self.db.execute("INSERT INTO gpt_tool_audit (owner, conversation_id, request_id, tool, args, ok, ms, error) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
            &[&owner, &conv, &request_id, &tool, &Json(args), &ok, &(ms as i32), &error]).await;
    }
}
