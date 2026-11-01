//! Plain English -> intent + bet filters. Deterministic on purpose: the
//! filters decide which real picks reach the card, so they must not depend on
//! a small model reading the question right. Follow-ups ("what about hockey?",
//! "only props") inherit the previous turn's filters.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Filters {
    /// nba | nfl | nhl | mlb (None = every sport)
    pub sport: Option<String>,
    /// prop | game (None = both)
    pub kind: Option<String>,
    /// today | tomorrow (None = anything still open)
    pub day: Option<String>,
    pub count: Option<usize>,
    pub bankroll: Option<f64>,
    /// safe | normal | aggressive
    pub risk: Option<String>,
    /// plus (plus-money only) | favorites (minus odds only)
    pub odds: Option<String>,
    #[serde(default)]
    pub parlay: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// "what should I bet", "best props tonight", follow-up filters
    Bets,
    /// a named matchup: "break down Avalanche vs Flames"
    Game,
    /// "how are your picks doing"
    Record,
    /// anything else: concepts, explanations
    General,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Route {
    pub intent: Intent,
    pub filters: Filters,
    /// filters carried over from the previous turn
    pub inherited: bool,
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '$' || c == '+' || c == '.' || c == '-'))
        .filter(|w| !w.is_empty())
        .map(|w| w.trim_end_matches(['.', '-']).to_string())
        .collect()
}

fn has(l: &str, keys: &[&str]) -> bool { keys.iter().any(|k| l.contains(k)) }

fn has_word(ws: &[String], keys: &[&str]) -> bool { ws.iter().any(|w| keys.contains(&w.as_str())) }

pub fn sport_of(ws: &[String], l: &str) -> Option<Option<String>> {
    if has(l, &["all sports", "any sport", "every sport", "all the sports"]) { return Some(None); }
    let pairs: &[(&[&str], &str)] = &[
        (&["nba", "basketball", "hoops"], "nba"),
        (&["nfl", "football"], "nfl"),
        (&["nhl", "hockey"], "nhl"),
        (&["mlb", "baseball"], "mlb"),
    ];
    pairs.iter().find(|(keys, _)| has_word(ws, keys)).map(|(_, s)| Some(s.to_string()))
}

fn number_word(w: &str) -> Option<usize> {
    let n = ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten"];
    n.iter().position(|x| *x == w).or_else(|| w.parse::<usize>().ok())
}

fn count_of(ws: &[String]) -> Option<usize> {
    for (i, w) in ws.iter().enumerate() {
        // "top 3", "best 5"
        if (w == "top" || w == "best") && i + 1 < ws.len() {
            if let Some(n) = number_word(&ws[i + 1]) { return Some(n); }
        }
        // "3 bets", "five picks", "3 safe NHL props" (noun within the next 3 words)
        let nouns = ["bets", "picks", "plays", "props", "legs", "locks", "bet", "pick", "play", "leg", "lock", "prop", "parlays"];
        if let Some(n) = number_word(w).filter(|n| (1..=20).contains(n)) {
            if ws.iter().skip(i + 1).take(3).any(|x| nouns.contains(&x.as_str())) { return Some(n); }
        }
    }
    None
}

fn money(w: &str) -> Option<f64> {
    let t = w.trim_start_matches('$').replace(',', "");
    let (num, mult) = if let Some(x) = t.strip_suffix('k') { (x.to_string(), 1000.0) } else { (t, 1.0) };
    num.parse::<f64>().ok().map(|v| v * mult).filter(|v| *v > 0.0)
}

fn bankroll_of(ws: &[String]) -> Option<f64> {
    for (i, w) in ws.iter().enumerate() {
        if w.starts_with('$') { if let Some(v) = money(w) { return Some(v); } }
        let next = ws.get(i + 1).map(|s| s.as_str());
        if matches!(next, Some("dollars" | "bucks" | "usd" | "bankroll" | "budget")) {
            if let Some(v) = money(w) { return Some(v); }
        }
        if matches!(w.as_str(), "bankroll" | "budget") {
            // "bankroll of 500", "budget is $300", "bankroll 1000"
            for k in 1..=3 { if let Some(v) = ws.get(i + k).and_then(|x| money(x)) { return Some(v); } }
        }
    }
    None
}

/// Short messages that only adjust the last request.
fn is_follow_up(l: &str, ws: &[String]) -> bool {
    let starts = ["what about", "how about", "and ", "only ", "now ", "make it", "instead", "same but", "just ", "also ", "ok ", "what if"];
    ws.len() <= 10 && (starts.iter().any(|s| l.starts_with(s)) || ws.len() <= 4)
}

pub fn route(text: &str, prev: Option<&Filters>) -> Route {
    let l = text.trim().to_lowercase();
    let ws = words(&l);
    let mut f = Filters::default();
    let mut touched = false;

    if let Some(s) = sport_of(&ws, &l) { f.sport = s; touched = true; }
    if has(&l, &["prop", "player", "points", "rebounds", "assists", "shots on goal", "strikeouts", "yards", "receptions"]) { f.kind = Some("prop".into()); touched = true; }
    if has(&l, &["game line", "moneyline", "money line", "spread", "totals", "sides", "puck line", "run line", "over/under"]) {
        f.kind = if f.kind.is_some() { None } else { Some("game".into()) };
        touched = true;
    }
    if has(&l, &["props and game", "any bet", "any kind", "everything", "all bets"]) { f.kind = None; touched = true; }
    if has_word(&ws, &["tomorrow", "tmrw", "tmr"]) { f.day = Some("tomorrow".into()); touched = true; }
    else if has_word(&ws, &["today", "tonight", "tonights", "tonight's"]) || l.contains("tonight") { f.day = Some("today".into()); touched = true; }
    if let Some(n) = count_of(&ws) { f.count = Some(n.min(15)); touched = true; }
    if let Some(b) = bankroll_of(&ws) { f.bankroll = Some(b); touched = true; }
    if has(&l, &["safe", "conservative", "low risk", "low-risk", "nothing crazy", "sure thing", "lock", "careful", "can't lose", "cant lose"]) {
        f.risk = Some("safe".into()); touched = true;
    } else if has(&l, &["aggressive", "risky", "longshot", "long shot", "yolo", "high risk", "high-risk", "big payout", "swing"]) {
        f.risk = Some("aggressive".into()); touched = true;
    }
    if has(&l, &["plus money", "plus-money", "underdog", "dogs", "+odds", "positive odds"]) { f.odds = Some("plus".into()); touched = true; }
    else if has(&l, &["favorite", "favourite", "chalk"]) { f.odds = Some("favorites".into()); touched = true; }
    if has(&l, &["parlay", "same game", "sgp", "accumulator", "multi"]) { f.parlay = true; touched = true; }
    if has(&l, &["no parlay", "straight bet", "singles"]) { f.parlay = false; touched = true; }

    let record = has(&l, &["track record", "record", "how have your", "how are your", "how did your", "win rate", "roi", "performance", "hit rate", "results so far"]);
    let game = has(&l, &[" vs ", " vs. ", " versus ", " @ ", " against ", "preview", "break down", "breakdown", "matchup", "match up"]);
    let bets = has(&l, &["bet", "pick", "play", "lock", "parlay", "wager", "what should i", "best", "value", "edge", "props", "money on", "put on"]);

    // follow-up: inherit the last request and apply the changes on top
    if let Some(p) = prev {
        if !record && !game && is_follow_up(&l, &ws) && (touched || has(&l, &["more", "another", "again", "others", "different"])) {
            let mut m = p.clone();
            if sport_of(&ws, &l).is_some() { m.sport = f.sport.clone(); }
            if f.kind.is_some() || has(&l, &["any bet", "any kind", "everything", "all bets", "props and game"]) { m.kind = f.kind.clone(); }
            if f.day.is_some() { m.day = f.day.clone(); }
            if f.count.is_some() { m.count = f.count; }
            if f.bankroll.is_some() { m.bankroll = f.bankroll; }
            if f.risk.is_some() { m.risk = f.risk.clone(); }
            if f.odds.is_some() { m.odds = f.odds.clone(); }
            if has(&l, &["parlay", "same game", "sgp", "no parlay", "straight bet", "singles"]) { m.parlay = f.parlay; }
            return Route { intent: Intent::Bets, filters: m, inherited: true };
        }
    }

    // "how does the parlay builder price legs?" is about the product, not a request for bets
    let concept_start = ["explain", "how does", "how do ", "how is ", "how are ", "what is", "what's", "whats ", "why ", "define", "tell me about", "what does", "what do "]
        .iter().any(|p| l.starts_with(p));
    let asks_for_bets = has(&l, &["best bet", "bets today", "bets tonight", "should i bet", "give me", "picks today", "props today", "picks tonight", "props tonight",
        "what should i", "any bets", "any props", "any picks", "worth betting", "worth a bet", "bet on today", "bet on tonight"]);
    let concept = concept_start && !asks_for_bets;
    let intent = if record { Intent::Record } else if concept { Intent::General } else if game { Intent::Game } else if bets || (touched && f.risk.is_some()) { Intent::Bets } else { Intent::General };
    Route { intent, filters: f, inherited: false }
}

/// One-line, human description of the filters ("NHL props · today · $200 · safe").
pub fn describe(f: &Filters) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(f.sport.as_deref().map(|s| s.to_uppercase()).unwrap_or_else(|| "all sports".into()));
    parts.push(match f.kind.as_deref() { Some("prop") => "player props".into(), Some("game") => "game lines".into(), _ => "props + game lines".into() });
    if let Some(d) = &f.day { parts.push(d.clone()); }
    if let Some(n) = f.count { parts.push(format!("top {n}")); }
    if let Some(b) = f.bankroll { parts.push(format!("${b:.0} bankroll")); }
    if let Some(r) = &f.risk { parts.push(r.clone()); }
    match f.odds.as_deref() { Some("plus") => parts.push("plus money".into()), Some("favorites") => parts.push("favorites".into()), _ => {} }
    if f.parlay { parts.push("parlay".into()); }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_bets_today_is_bets_for_every_sport() {
        let r = route("What are the best bets today?", None);
        assert_eq!(r.intent, Intent::Bets);
        assert_eq!(r.filters.sport, None);
        assert_eq!(r.filters.day.as_deref(), Some("today"));
    }

    #[test]
    fn parses_sport_kind_count_bankroll_risk() {
        let r = route("give me 3 safe NHL props tonight, I have $200", None);
        assert_eq!(r.intent, Intent::Bets);
        let f = r.filters;
        assert_eq!(f.sport.as_deref(), Some("nhl"));
        assert_eq!(f.kind.as_deref(), Some("prop"));
        assert_eq!(f.count, Some(3));
        assert_eq!(f.bankroll, Some(200.0));
        assert_eq!(f.risk.as_deref(), Some("safe"));
        assert_eq!(f.day.as_deref(), Some("today"));
    }

    #[test]
    fn bankroll_forms() {
        assert_eq!(route("best bets with a bankroll of 500", None).filters.bankroll, Some(500.0));
        assert_eq!(route("I've got 150 dollars, what should I bet", None).filters.bankroll, Some(150.0));
        assert_eq!(route("what should I bet with $1.5k", None).filters.bankroll, Some(1500.0));
        assert_eq!(route("top 5 picks", None).filters.bankroll, None);
    }

    #[test]
    fn follow_up_inherits_and_overrides() {
        let first = route("give me 3 safe NHL props tonight with $200", None).filters;
        let r = route("what about basketball?", Some(&first));
        assert!(r.inherited);
        assert_eq!(r.intent, Intent::Bets);
        assert_eq!(r.filters.sport.as_deref(), Some("nba"));
        assert_eq!(r.filters.count, Some(3));
        assert_eq!(r.filters.bankroll, Some(200.0));
        assert_eq!(r.filters.risk.as_deref(), Some("safe"));
        let r2 = route("make it a parlay", Some(&r.filters));
        assert!(r2.filters.parlay);
        assert_eq!(r2.filters.sport.as_deref(), Some("nba"));
        let r3 = route("all sports", Some(&r2.filters));
        assert_eq!(r3.filters.sport, None);
    }

    #[test]
    fn new_full_question_does_not_inherit() {
        let prev = route("3 safe nhl props", None).filters;
        let r = route("Explain how no-vig fair odds are calculated from two prices", Some(&prev));
        assert!(!r.inherited);
        assert_eq!(r.intent, Intent::General);
    }

    #[test]
    fn record_and_game_intents() {
        assert_eq!(route("how are your picks doing this month?", None).intent, Intent::Record);
        assert_eq!(route("break down Avalanche vs Flames", None).intent, Intent::Game);
        assert_eq!(route("Explain no-vig fair odds", None).intent, Intent::General);
    }

    #[test]
    fn product_questions_are_not_bet_requests() {
        assert_eq!(route("How does the parlay builder price correlated legs?", None).intent, Intent::General);
        assert_eq!(route("What is fractional Kelly and why use it?", None).intent, Intent::General);
        assert_eq!(route("Explain how AI pick grades are capped", None).intent, Intent::General);
        assert_eq!(route("what are the best bets today?", None).intent, Intent::Bets);
        assert_eq!(route("what should I bet on tonight", None).intent, Intent::Bets);
        assert_eq!(route("how are your picks doing?", None).intent, Intent::Record);
    }

    #[test]
    fn odds_preferences() {
        assert_eq!(route("any plus money props?", None).filters.odds.as_deref(), Some("plus"));
        assert_eq!(route("only favorites today", None).filters.odds.as_deref(), Some("favorites"));
    }

    #[test]
    fn describe_reads_naturally() {
        let f = route("3 safe NHL props tonight with $200", None).filters;
        assert_eq!(describe(&f), "NHL · player props · today · top 3 · $200 bankroll · safe");
    }
}
