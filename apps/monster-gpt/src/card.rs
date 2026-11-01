//! The bet card: which of today's real AI picks to play, built in code from
//! the projection-service feed. Every number on it (price, book, probability,
//! edge, stake) is copied or computed from tool data, never from the LLM.

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use crate::router::Filters;

const GRADES: &[&str] = &["A+", "A", "A-", "B+", "B", "B-", "C+", "C", "C-", "D"];

fn grade_rank(g: &str) -> usize { GRADES.iter().position(|x| *x == g).unwrap_or(GRADES.len()) }
fn tier_rank(t: &str) -> usize { match t { "proven" => 0, "baseline" => 1, "unproven" => 2, _ => 3 } }

/// Eastern-time calendar date (US DST: second Sunday of March to first Sunday of November).
pub fn et_date(t: DateTime<Utc>) -> NaiveDate {
    let y = t.year();
    let nth_sunday = |m: u32, n: u32| {
        let first = NaiveDate::from_ymd_opt(y, m, 1).unwrap();
        let off = (7 - first.weekday().num_days_from_sunday()) % 7;
        first + Duration::days((off + 7 * (n - 1)) as i64)
    };
    let dst_start = Utc.from_utc_datetime(&nth_sunday(3, 2).and_hms_opt(7, 0, 0).unwrap());
    let dst_end = Utc.from_utc_datetime(&nth_sunday(11, 1).and_hms_opt(6, 0, 0).unwrap());
    let off = if t >= dst_start && t < dst_end { 4 } else { 5 };
    (t - Duration::hours(off)).date_naive()
}

#[derive(Clone, Debug, Serialize)]
pub struct Bet {
    pub id: i64,
    pub sport: String,
    pub kind: String,
    pub game: String,
    pub starts_at: Option<String>,
    /// "Cale Makar Under 1.5 Points" / "COL Moneyline"
    pub pick: String,
    pub market: String,
    pub line: Option<f64>,
    pub side: String,
    pub price: i64,
    pub book: String,
    pub link: Option<String>,
    pub p_model: f64,
    pub p_market: f64,
    pub edge: f64,
    pub ev: f64,
    pub grade: String,
    pub tier: String,
    /// suggested stake as a fraction of bankroll (AI Picks' fractional Kelly)
    pub stake_frac: f64,
    pub stake_usd: Option<f64>,
    pub to_win_usd: Option<f64>,
    /// worst price still worth playing
    pub playable_price: Option<i64>,
    pub why: String,
    pub risks: Vec<String>,
    pub source: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Parlay {
    pub legs: Vec<i64>,
    pub decimal_odds: f64,
    pub american: i64,
    pub p_model: f64,
    pub ev: f64,
    pub stake_usd: Option<f64>,
    pub to_win_usd: Option<f64>,
    pub note: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Card {
    pub title: String,
    pub filters: Filters,
    pub generated_at: DateTime<Utc>,
    pub bets: Vec<Bet>,
    pub total_stake_usd: Option<f64>,
    pub expected_profit_usd: Option<f64>,
    pub parlay: Option<Parlay>,
    /// why the card is empty or short, and what else is open
    pub notes: Vec<String>,
    /// open picks by sport before filtering (so the user can widen the ask)
    pub available: Value,
}

pub fn decimal(price: i64) -> f64 { if price > 0 { 1.0 + price as f64 / 100.0 } else { 1.0 + 100.0 / (-price) as f64 } }
pub fn american(dec: f64) -> i64 { if dec >= 2.0 { ((dec - 1.0) * 100.0).round() as i64 } else { (-100.0 / (dec - 1.0)).round() as i64 } }
fn odds_s(p: i64) -> String { if p > 0 { format!("+{p}") } else { p.to_string() } }
fn round2(x: f64) -> f64 { (x * 100.0).round() / 100.0 }

fn first_sentence(s: &str) -> String {
    let w = s.split("What could make it wrong:").next().unwrap_or(s).trim();
    let cut = w.find(". ").map(|i| &w[..=i]).unwrap_or(w);
    cut.chars().take(260).collect()
}

/// Build the card from `/picks` rows. `today` is the ET date "now".
pub fn build(picks: &[Value], f: &Filters, today: NaiveDate, game_filter: Option<&[String]>) -> Card {
    let risk = f.risk.as_deref().unwrap_or("normal");
    let count = f.count.unwrap_or(if f.parlay { 3 } else if risk == "safe" { 3 } else { 5 }).max(1);
    let mut available = serde_json::Map::new();
    for p in picks.iter().filter(|p| p["status"] == "open") {
        let k = p["sport"].as_str().unwrap_or("?").to_string();
        let e = available.entry(k).or_insert(json!(0));
        *e = json!(e.as_i64().unwrap_or(0) + 1);
    }
    let want_day = match f.day.as_deref() { Some("today") => Some(today), Some("tomorrow") => Some(today + Duration::days(1)), _ => None };
    let mut notes = Vec::new();
    let mut dropped_unplayable = 0;

    let mut rows: Vec<&Value> = picks.iter().filter(|p| {
        if p["status"] != "open" { return false; }
        if let Some(s) = &f.sport { if p["sport"].as_str() != Some(s) { return false; } }
        if let Some(k) = &f.kind { if p["kind"].as_str() != Some(k) { return false; } }
        if let Some(d) = want_day {
            let start = p["starts_at"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| et_date(t.with_timezone(&Utc)));
            if start != Some(d) { return false; }
        }
        if let Some(games) = game_filter { if !games.iter().any(|g| p["game_label"].as_str() == Some(g)) { return false; } }
        let price = p["price"].as_i64().unwrap_or(0);
        match f.odds.as_deref() { Some("plus") if price <= 0 => return false, Some("favorites") if price >= 0 => return false, _ => {} }
        let tier = p["tier"].as_str().unwrap_or("");
        let grade = grade_rank(p["grade"].as_str().unwrap_or(""));
        let ok = match risk {
            "safe" => matches!(tier, "proven" | "baseline") && grade <= grade_rank("B+"),
            "aggressive" => tier != "losing",
            _ => tier != "losing" && grade <= grade_rank("C+"),
        };
        ok
    }).collect();
    let before = rows.len();
    rows.retain(|p| p["playable_now"].as_bool() != Some(false));
    dropped_unplayable += before - rows.len();
    rows.sort_by(|a, b| {
        let k = |p: &Value| (tier_rank(p["tier"].as_str().unwrap_or("")), grade_rank(p["grade"].as_str().unwrap_or("")));
        k(a).cmp(&k(b)).then(b["edge"].as_f64().unwrap_or(0.0).total_cmp(&a["edge"].as_f64().unwrap_or(0.0)))
    });

    // one bet per player/team, and at most two per game (correlated otherwise)
    let mut seen_subject = std::collections::HashSet::new();
    let mut per_game: std::collections::HashMap<String, usize> = Default::default();
    let mut bets = Vec::new();
    for p in rows {
        if bets.len() >= count { break; }
        let game = p["game_label"].as_str().unwrap_or("").to_string();
        let subject = format!("{game}|{}", p["subject"].as_str().or(p["subject_name"].as_str()).unwrap_or(""));
        let max_per_game = if f.parlay { 1 } else { 2 };
        if !seen_subject.insert(subject) || *per_game.get(&game).unwrap_or(&0) >= max_per_game { continue; }
        *per_game.entry(game.clone()).or_default() += 1;
        let price = p["price"].as_i64().unwrap_or(0);
        let stake_frac = p["stake"].as_f64().filter(|x| *x > 0.0).unwrap_or(0.005);
        let stake_usd = f.bankroll.map(|b| (b * stake_frac).max(1.0).round());
        let subj = p["subject_name"].as_str().unwrap_or("");
        let label = p["label"].as_str().unwrap_or("");
        let pick = if p["kind"] == "prop" { format!("{subj} {label}") } else { label.to_string() };
        bets.push(Bet {
            id: p["id"].as_i64().unwrap_or(0), sport: p["sport"].as_str().unwrap_or("").into(), kind: p["kind"].as_str().unwrap_or("").into(),
            game, starts_at: p["starts_at"].as_str().map(|s| s.to_string()), pick, market: p["market"].as_str().unwrap_or("").into(),
            line: p["line"].as_f64(), side: p["side"].as_str().unwrap_or("").into(), price,
            book: p["book_title"].as_str().or(p["book"].as_str()).unwrap_or("").into(), link: p["link"].as_str().map(|s| s.to_string()),
            p_model: p["p_model"].as_f64().unwrap_or(0.0), p_market: p["p_market"].as_f64().unwrap_or(0.0),
            edge: p["edge"].as_f64().unwrap_or(0.0), ev: p["ev"].as_f64().unwrap_or(0.0),
            grade: p["grade"].as_str().unwrap_or("").into(), tier: p["tier"].as_str().unwrap_or("").into(), stake_frac,
            stake_usd, to_win_usd: stake_usd.map(|s| round2(s * (decimal(price) - 1.0))),
            playable_price: p["playable_price"].as_i64(), why: first_sentence(p["writeup"].as_str().unwrap_or("")),
            risks: p["risk_flags"].as_array().into_iter().flatten().filter_map(|r| r["text"].as_str().map(|s| s.to_string())).collect(),
            source: format!("pick:{}", p["id"]),
        });
    }

    if bets.is_empty() {
        let total: i64 = available.values().filter_map(|v| v.as_i64()).sum();
        notes.push(if total == 0 {
            "No AI picks are open right now. Picks publish through the day as games get within about 40 hours and books post lines.".into()
        } else {
            format!("Nothing open matches these filters. Open picks right now: {}.",
                available.iter().map(|(k, v)| format!("{} {}", k.to_uppercase(), v)).collect::<Vec<_>>().join(", "))
        });
        if risk == "safe" { notes.push("\"Safe\" only allows proven or baseline picks graded B+ or better; say \"normal risk\" to widen it.".into()); }
    } else if bets.len() < count {
        notes.push(format!("Only {} pick{} cleared the filters (asked for {count}).", bets.len(), if bets.len() == 1 { "" } else { "s" }));
    }
    if dropped_unplayable > 0 {
        notes.push(format!("{dropped_unplayable} pick{} left out because the price has moved past the playable price.", if dropped_unplayable == 1 { " was" } else { "s were" }));
    }
    if f.bankroll.is_none() && !bets.is_empty() {
        notes.push("Stakes are shown as a share of bankroll; tell me your bankroll for dollar amounts.".into());
    }

    let parlay = if f.parlay && bets.len() >= 2 {
        let legs: Vec<&Bet> = bets.iter().take(4).collect();
        let dec: f64 = legs.iter().map(|b| decimal(b.price)).product();
        let p: f64 = legs.iter().map(|b| b.p_model).product();
        let stake = f.bankroll.map(|b| (b * 0.005).max(1.0).round());
        Some(Parlay {
            legs: legs.iter().map(|b| b.id).collect(), decimal_odds: round2(dec), american: american(dec), p_model: (p * 1000.0).round() / 1000.0,
            ev: round2(p * dec - 1.0), stake_usd: stake, to_win_usd: stake.map(|s| round2(s * (dec - 1.0))),
            note: "Legs from different games, priced as independent at each leg's own price. Books pay less than this on same-game parlays; the Parlay Builder prices correlation properly.".into(),
        })
    } else {
        if f.parlay { notes.push("A parlay needs at least two legs from different games.".into()); }
        None
    };

    let total_stake = f.bankroll.map(|_| bets.iter().filter_map(|b| b.stake_usd).sum::<f64>());
    let exp = f.bankroll.map(|_| round2(bets.iter().map(|b| b.stake_usd.unwrap_or(0.0) * b.ev).sum::<f64>()));
    Card {
        title: format!("{} for {}", if f.parlay { "Parlay" } else { "Bets" }, crate::router::describe(f)),
        filters: f.clone(), generated_at: Utc::now(), bets, total_stake_usd: total_stake, expected_profit_usd: exp, parlay, notes,
        available: Value::Object(available),
    }
}

/// Plain-text card: the answer when no language model is available, and the
/// numbers the model is told to use.
pub fn render_text(c: &Card) -> String {
    let mut s = String::new();
    if c.bets.is_empty() {
        s.push_str(&c.notes.join(" "));
        return s;
    }
    s.push_str(&format!("{}:\n", c.title));
    for (i, b) in c.bets.iter().enumerate() {
        s.push_str(&format!("{}. {} ({}, {}) {} at {}. Model {:.0}% vs market {:.0}%, edge {:+.1} pts, grade {} ({}).",
            i + 1, b.pick, b.game, b.sport.to_uppercase(), odds_s(b.price), b.book, b.p_model * 100.0, b.p_market * 100.0, b.edge * 100.0, b.grade, b.tier));
        match b.stake_usd {
            Some(u) => s.push_str(&format!(" Stake ${u:.0} to win ${:.2}; model-expected profit ${:.2}.", b.to_win_usd.unwrap_or(0.0), u * b.ev)),
            None => s.push_str(&format!(" Stake {:.1}% of bankroll.", b.stake_frac * 100.0)),
        }
        if let Some(pp) = b.playable_price { s.push_str(&format!(" Playable down to {}.", odds_s(pp))); }
        s.push('\n');
    }
    if let (Some(t), Some(e)) = (c.total_stake_usd, c.expected_profit_usd) { s.push_str(&format!("Total stake ${t:.0}; model-expected profit ${e:.2}.\n")); }
    if let Some(p) = &c.parlay {
        s.push_str(&format!("Parlay of {} legs: {} (decimal {:.2}), model probability {:.1}%, EV {:+.0}%.\n", p.legs.len(), odds_s(p.american), p.decimal_odds, p.p_model * 100.0, p.ev * 100.0));
    }
    if !c.notes.is_empty() {
        s.push_str("Notes (about the card as a whole, or about picks NOT on it):\n");
        for n in &c.notes { s.push_str("- "); s.push_str(n); s.push('\n'); }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::route;

    fn pick(id: i64, sport: &str, kind: &str, game: &str, subj: &str, price: i64, edge: f64, grade: &str, tier: &str, start: &str) -> Value {
        json!({ "id": id, "sport": sport, "kind": kind, "game_label": game, "subject": subj, "subject_name": subj, "label": "Under 1.5 Points",
            "market": "points", "line": 1.5, "side": "under", "price": price, "book_title": "DraftKings", "p_model": 0.6, "p_market": 0.55,
            "edge": edge, "ev": 0.08, "grade": grade, "tier": tier, "stake": 0.02, "playable_price": -200, "playable_now": true,
            "status": "open", "starts_at": start, "writeup": "First reason. Second reason. What could make it wrong: stuff.", "risk_flags": [] })
    }

    fn slate() -> Vec<Value> {
        vec![
            pick(1, "nhl", "prop", "CGY @ COL", "Makar", -150, 0.10, "A", "proven", "2026-11-01T01:00:00Z"),
            pick(2, "nhl", "prop", "CGY @ COL", "Kadri", 120, 0.12, "B", "baseline", "2026-11-01T01:00:00Z"),
            pick(3, "nhl", "prop", "CGY @ COL", "MacKinnon", -110, 0.20, "A+", "proven", "2026-11-01T01:00:00Z"),
            pick(4, "nba", "prop", "CHA @ WAS", "Miller", -120, 0.05, "C+", "unproven", "2026-10-31T23:00:00Z"),
            pick(5, "nba", "game", "BOS @ NYK", "BOS", 140, 0.04, "B+", "baseline", "2026-11-02T00:00:00Z"),
        ]
    }

    #[test]
    fn et_dates_follow_dst() {
        // 01:00Z Nov 1 2026 is 21:00 EDT Oct 31
        assert_eq!(et_date("2026-11-01T01:00:00Z".parse().unwrap()), NaiveDate::from_ymd_opt(2026, 10, 31).unwrap());
        // after DST ends (Nov 1 06:00Z) the offset is 5 h
        assert_eq!(et_date("2026-11-02T04:30:00Z".parse().unwrap()), NaiveDate::from_ymd_opt(2026, 11, 1).unwrap());
    }

    #[test]
    fn safe_ranks_proven_first_and_caps_two_per_game() {
        let f = route("safe bets with $100", None).filters;
        let c = build(&slate(), &f, NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(), None);
        let ids: Vec<i64> = c.bets.iter().map(|b| b.id).collect();
        // proven A+ then proven A; Kadri (B, baseline) fails "safe"; 2-per-game cap; NBA C+ fails; BOS B+ baseline ok
        assert_eq!(ids, vec![3, 1, 5]);
        assert_eq!(c.bets[0].stake_usd, Some(2.0));
        assert_eq!(c.total_stake_usd, Some(6.0));
    }

    #[test]
    fn day_sport_kind_and_odds_filters() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 31).unwrap();
        let c = build(&slate(), &route("nba bets today", None).filters, today, None);
        assert_eq!(c.bets.iter().map(|b| b.id).collect::<Vec<_>>(), vec![4]);
        let c = build(&slate(), &route("plus money picks", None).filters, today, None);
        assert!(c.bets.iter().all(|b| b.price > 0));
        let c = build(&slate(), &route("game lines tomorrow", None).filters, today, None);
        assert_eq!(c.bets.iter().map(|b| b.id).collect::<Vec<_>>(), vec![5]);
    }

    #[test]
    fn empty_card_explains_what_is_open() {
        let c = build(&slate(), &route("mlb bets", None).filters, NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(), None);
        assert!(c.bets.is_empty());
        assert!(c.notes[0].contains("NHL 3"), "{:?}", c.notes);
    }

    #[test]
    fn parlay_uses_one_leg_per_game_and_multiplies() {
        let c = build(&slate(), &route("aggressive parlay with $200", None).filters, NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(), None);
        let p = c.parlay.expect("parlay");
        assert_eq!(p.legs.len(), 3);
        let games: std::collections::HashSet<&str> = c.bets.iter().map(|b| b.game.as_str()).collect();
        assert_eq!(games.len(), 3);
        let dec: f64 = c.bets.iter().map(|b| decimal(b.price)).product();
        assert!((p.decimal_odds - (dec * 100.0).round() / 100.0).abs() < 1e-9);
        assert!((p.p_model - 0.216).abs() < 1e-9);
    }

    #[test]
    fn odds_conversions_round_trip() {
        assert_eq!(american(decimal(150)), 150);
        assert_eq!(american(decimal(-200)), -200);
        assert!((decimal(-110) - 1.909090).abs() < 1e-5);
    }

    #[test]
    fn unplayable_picks_are_left_out_with_a_note() {
        let mut s = slate();
        s[2]["playable_now"] = json!(false);
        let c = build(&s, &route("best bets", None).filters, NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(), None);
        assert!(!c.bets.iter().any(|b| b.id == 3));
        assert!(c.notes.iter().any(|n| n.contains("playable price")));
    }
}
