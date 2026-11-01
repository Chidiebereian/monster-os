//! Long-term memory: things the user explicitly asks MonsterGPT to remember
//! ("remember my bankroll is $500", "from now on only NHL", "remember I use
//! FanDuel"), per owner, listable and deletable. Preferences fill in filters a
//! request leaves out; notes are recalled only when they share words with the
//! question. Nothing is stored without an explicit remember/save/from-now-on.

use serde::Serialize;
use serde_json::{json, Value};
use tokio_postgres::Client;

use crate::router::{self, Filters};

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// preferences (key, value) and an optional free-text note
    Remember { prefs: Vec<(String, String)>, note: Option<String> },
    /// keys to forget; empty = everything
    Forget(Vec<String>),
    Recall,
}

const BOOKS: &[&str] = &["draftkings", "fanduel", "betmgm", "caesars", "espn bet", "fanatics", "betrivers", "pinnacle", "bet365", "hard rock", "pointsbet", "bovada"];

fn prefs_in(text: &str) -> Vec<(String, String)> {
    let r = router::route(text, None).filters;
    let l = text.to_lowercase();
    let mut out = Vec::new();
    if let Some(b) = r.bankroll { out.push(("bankroll".into(), format!("{b:.0}"))); }
    if let Some(x) = r.risk { out.push(("risk".into(), x)); }
    if let Some(x) = r.sport { out.push(("sport".into(), x)); }
    if let Some(x) = r.kind { out.push(("kind".into(), x)); }
    if let Some(x) = r.odds { out.push(("odds".into(), x)); }
    if let Some(b) = BOOKS.iter().find(|b| l.contains(*b)) { out.push(("book".into(), b.to_string())); }
    out
}

/// Recognise memory commands. Ordinary questions return None.
pub fn command(text: &str) -> Option<Command> {
    let l = text.trim().to_lowercase();
    if ["what do you remember", "what have you saved", "what do you know about me", "show my preferences", "show my memory", "list my preferences"].iter().any(|p| l.contains(p)) {
        return Some(Command::Recall);
    }
    if let Some(rest) = ["forget ", "delete my ", "clear my ", "stop remembering "].iter().find_map(|p| l.strip_prefix(p).or_else(|| l.split_once(p).map(|x| x.1))) {
        if ["everything", "all", "it all", "all my preferences", "memory", "my memory"].iter().any(|w| rest.trim().trim_end_matches(['.', '!']) == *w) { return Some(Command::Forget(vec![])); }
        let keys: Vec<String> = ["bankroll", "risk", "sport", "kind", "odds", "book", "notes", "note"].iter()
            .filter(|k| rest.contains(*k) || (**k == "book" && rest.contains("sportsbook")))
            .map(|k| if *k == "notes" { "note".to_string() } else { k.to_string() }).collect();
        if !keys.is_empty() { return Some(Command::Forget(keys)); }
    }
    let triggers = ["remember ", "save that", "save this", "from now on", "going forward", "always ", "keep in mind", "note that"];
    if triggers.iter().any(|t| l.starts_with(t) || l.contains(&format!(" {t}")) || l.contains(&format!(", {t}"))) {
        let prefs = prefs_in(&l);
        let note = if prefs.is_empty() { Some(text.trim().trim_start_matches(|c: char| !c.is_alphanumeric()).to_string()) } else { None };
        return Some(Command::Remember { prefs, note });
    }
    None
}

/// Fill filters the request left unset from saved preferences; returns which were used.
pub fn apply(f: &mut Filters, prefs: &[(String, String)]) -> Vec<String> {
    let mut used = Vec::new();
    for (k, v) in prefs {
        match k.as_str() {
            "bankroll" if f.bankroll.is_none() => if let Ok(b) = v.parse() { f.bankroll = Some(b); used.push(format!("bankroll ${v}")); },
            "risk" if f.risk.is_none() => { f.risk = Some(v.clone()); used.push(format!("{v} risk")); }
            "sport" if f.sport.is_none() => { f.sport = Some(v.clone()); used.push(v.to_uppercase()); }
            "kind" if f.kind.is_none() => { f.kind = Some(v.clone()); used.push(if v == "prop" { "props".into() } else { "game lines".into() }); }
            "odds" if f.odds.is_none() => { f.odds = Some(v.clone()); used.push(v.clone()); }
            _ => {}
        }
    }
    used
}

#[derive(Clone, Debug, Serialize)]
pub struct Item { pub id: i64, pub kind: String, pub key: String, pub value: String, pub updated_at: chrono::DateTime<chrono::Utc> }

pub struct Memory { pub db: Client }

fn e(x: tokio_postgres::Error) -> String { x.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| x.to_string()) }

impl Memory {
    pub async fn connect(url: &str) -> Result<Self, String> {
        let (db, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls).await.map_err(e)?;
        actix_web::rt::spawn(async move { let _ = conn.await; });
        db.query("SELECT 1 FROM gpt_memory LIMIT 1", &[]).await.map_err(|x| format!("{} (apply migration 0031_rag.sql)", e(x)))?;
        Ok(Self { db })
    }

    pub async fn list(&self, owner: &str) -> Result<Vec<Item>, String> {
        Ok(self.db.query("SELECT id, kind, key, value, updated_at FROM gpt_memory WHERE owner=$1 ORDER BY kind DESC, key, updated_at DESC", &[&owner]).await.map_err(e)?
            .iter().map(|r| Item { id: r.get(0), kind: r.get(1), key: r.get(2), value: r.get(3), updated_at: r.get(4) }).collect())
    }

    pub async fn prefs(&self, owner: &str) -> Vec<(String, String)> {
        self.list(owner).await.unwrap_or_default().into_iter().filter(|i| i.kind == "preference").map(|i| (i.key, i.value)).collect()
    }

    /// Notes sharing at least two content words with the question (or one rare-ish word of 6+ letters).
    pub async fn relevant_notes(&self, owner: &str, question: &str) -> Vec<String> {
        let q: std::collections::HashSet<String> = question.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 3).map(|w| w.to_string()).collect();
        self.list(owner).await.unwrap_or_default().into_iter().filter(|i| i.kind == "note").filter(|i| {
            let words: Vec<String> = i.value.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 3).map(|w| w.to_string()).collect();
            let shared: Vec<&String> = words.iter().filter(|w| q.contains(*w)).collect();
            shared.len() >= 2 || shared.iter().any(|w| w.len() >= 6)
        }).map(|i| i.value).take(5).collect()
    }

    pub async fn remember(&self, owner: &str, prefs: &[(String, String)], note: Option<&str>, source: &str) -> Result<(), String> {
        for (k, v) in prefs {
            self.db.execute("INSERT INTO gpt_memory (owner, kind, key, value, source) VALUES ($1,'preference',$2,$3,$4) \
                ON CONFLICT (owner, key) WHERE kind='preference' DO UPDATE SET value=EXCLUDED.value, source=EXCLUDED.source, updated_at=now()",
                &[&owner, k, v, &source]).await.map_err(e)?;
        }
        if let Some(n) = note {
            let n: String = n.chars().take(500).collect();
            self.db.execute("INSERT INTO gpt_memory (owner, kind, key, value, source) VALUES ($1,'note','note',$2,$3)", &[&owner, &n, &source]).await.map_err(e)?;
        }
        Ok(())
    }

    pub async fn forget(&self, owner: &str, keys: &[String]) -> Result<u64, String> {
        if keys.is_empty() { return self.db.execute("DELETE FROM gpt_memory WHERE owner=$1", &[&owner]).await.map_err(e); }
        self.db.execute("DELETE FROM gpt_memory WHERE owner=$1 AND key = ANY($2)", &[&owner, &keys]).await.map_err(e)
    }

    pub async fn delete_item(&self, owner: &str, id: i64) -> Result<bool, String> {
        Ok(self.db.execute("DELETE FROM gpt_memory WHERE owner=$1 AND id=$2", &[&owner, &id]).await.map_err(e)? > 0)
    }

    /// The reply to a memory command (deterministic; no model involved).
    pub async fn handle(&self, owner: &str, cmd: &Command, source: &str) -> Result<(String, Value), String> {
        match cmd {
            Command::Recall => {
                let items = self.list(owner).await?;
                if items.is_empty() { return Ok(("I haven't saved anything for you. Say \"remember my bankroll is $500\" or \"from now on, only NHL props\" and I'll use it on future requests.".into(), json!([]))); }
                let lines: Vec<String> = items.iter().map(|i| if i.kind == "note" { format!("- note: {}", i.value) } else { format!("- {}: {}", i.key, i.value) }).collect();
                Ok((format!("Here's what I have saved:\n{}\nSay \"forget my bankroll\" (or any item) or \"forget everything\" to remove it.", lines.join("\n")), json!(items)))
            }
            Command::Forget(keys) => {
                let n = self.forget(owner, keys).await?;
                Ok((if keys.is_empty() { format!("Done. I deleted everything I had saved ({n} item{}).", if n == 1 { "" } else { "s" }) }
                    else if n == 0 { format!("I had nothing saved for {}.", keys.join(", ")) }
                    else { format!("Done. I forgot your {}.", keys.join(", ")) }, json!({ "deleted": n })))
            }
            Command::Remember { prefs, note } => {
                self.remember(owner, prefs, note.as_deref(), source).await?;
                let what: Vec<String> = prefs.iter().map(|(k, v)| match k.as_str() { "bankroll" => format!("bankroll ${v}"), "risk" => format!("{v} risk"),
                    "sport" => format!("{} by default", v.to_uppercase()), "kind" => if v == "prop" { "player props by default".into() } else { "game lines by default".into() },
                    "odds" => if v == "plus" { "plus-money only".into() } else { "favorites only".into() }, "book" => format!("you bet at {v}"), _ => format!("{k} {v}") }).collect();
                Ok((match (what.is_empty(), note) {
                    (false, _) => format!("Saved: {}. I'll use {} when a request doesn't say otherwise. Say \"forget my {}\" to undo.", what.join(", "), if what.len() == 1 { "it" } else { "these" }, prefs[0].0),
                    (true, Some(n)) => format!("Saved a note: \"{n}\". I'll bring it up when it's relevant."),
                    _ => "Nothing to save.".into(),
                }, json!({ "prefs": prefs, "note": note })))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_commands_and_ignores_questions() {
        assert_eq!(command("what are the best bets today?"), None);
        assert_eq!(command("my bankroll is 500, any bets?"), None, "no explicit remember -> not stored");
        assert_eq!(command("What do you remember about me?"), Some(Command::Recall));
        assert_eq!(command("forget everything"), Some(Command::Forget(vec![])));
        assert_eq!(command("please forget my bankroll"), Some(Command::Forget(vec!["bankroll".into()])));
        match command("Remember my bankroll is $500 and I like safe bets").unwrap() {
            Command::Remember { prefs, note } => {
                assert!(prefs.contains(&("bankroll".into(), "500".into())));
                assert!(prefs.contains(&("risk".into(), "safe".into())));
                assert!(note.is_none());
            }
            c => panic!("{c:?}"),
        }
        match command("from now on only NHL props, I bet at FanDuel").unwrap() {
            Command::Remember { prefs, .. } => {
                assert!(prefs.contains(&("sport".into(), "nhl".into())));
                assert!(prefs.contains(&("kind".into(), "prop".into())));
                assert!(prefs.contains(&("book".into(), "fanduel".into())));
            }
            c => panic!("{c:?}"),
        }
        assert!(matches!(command("remember that I hate back-to-back road teams"), Some(Command::Remember { ref note, .. }) if note.is_some()));
    }

    #[test]
    fn preferences_fill_only_missing_filters() {
        let mut f = router::route("3 nba bets tonight with $50", None).filters;
        let used = apply(&mut f, &[("bankroll".into(), "500".into()), ("risk".into(), "safe".into()), ("sport".into(), "nhl".into())]);
        assert_eq!(f.bankroll, Some(50.0), "the request's own bankroll wins");
        assert_eq!(f.sport.as_deref(), Some("nba"));
        assert_eq!(f.risk.as_deref(), Some("safe"));
        assert_eq!(used, vec!["safe risk".to_string()]);
    }
}
