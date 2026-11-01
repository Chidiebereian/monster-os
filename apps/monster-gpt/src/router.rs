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
    /// the stake on ONE parlay ("a $25 parlay", "parlay with $50"); a parlay
    /// is a single wager, so this is never split across its legs
    #[serde(default)]
    pub parlay_stake: Option<f64>,
    /// legs per parlay ("3-leg", "four-team")
    #[serde(default)]
    pub legs: Option<usize>,
    /// how many separate parlays ("two parlays")
    #[serde(default)]
    pub parlays: Option<usize>,
    /// an amount whose meaning is unclear ("2 parlays with $100": each or in
    /// total?); the reply asks instead of guessing
    #[serde(default)]
    pub unclear_amount: Option<f64>,
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
    /// model diagnostics: "why isn't Monster publishing NFL spreads?", "which models are calibrated?"
    Model,
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
        let nouns = ["bets", "picks", "plays", "props", "legs", "locks", "bet", "pick", "play", "leg", "lock", "prop"];
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

const LEG_WORDS: &[&str] = &["leg", "legs", "legged", "team", "teams", "teamer", "picker"];

/// "3-leg", "four-team", "3 legs"
fn legs_of(ws: &[String]) -> Option<usize> {
    for (i, w) in ws.iter().enumerate() {
        if let Some((n, rest)) = w.split_once('-') {
            if LEG_WORDS.contains(&rest) { if let Some(n) = number_word(n) { return Some(n); } }
        }
        if let Some(n) = number_word(w) {
            if ws.get(i + 1).is_some_and(|x| LEG_WORDS.contains(&x.as_str())) { return Some(n); }
        }
    }
    None
}

/// "2 parlays", "two separate parlays" (but not the 3 in "3 leg parlays")
fn parlays_of(ws: &[String]) -> Option<usize> {
    for (i, w) in ws.iter().enumerate() {
        let Some(n) = number_word(w).filter(|n| (1..=10).contains(n)) else { continue };
        if ws.get(i + 1).is_some_and(|x| LEG_WORDS.contains(&x.as_str())) { continue; }
        if ws.iter().skip(i + 1).take(3).any(|x| x == "parlays") { return Some(n); }
    }
    None
}

#[derive(Debug, Default, PartialEq)]
struct ParlayMoney { bankroll: Option<f64>, stake: Option<f64>, unclear: Option<f64> }

fn round2(x: f64) -> f64 { (x * 100.0).round() / 100.0 }

/// Dollar amounts in a parlay request. A plain amount is the stake on the
/// parlay ("a parlay with $50"); it is a bankroll only when the user says so
/// ("I have a $50 bankroll", "my budget is $100"). With several parlays, an
/// amount without "each" or "total" is unclear and gets a question.
fn parlay_money(ws: &[String], parlays: usize) -> ParlayMoney {
    let mut m = ParlayMoney::default();
    let back = |i: usize, n: usize, keys: &[&str]| ws[i.saturating_sub(n)..i].iter().any(|x| keys.contains(&x.as_str()));
    let fwd = |i: usize, n: usize, keys: &[&str]| ws.iter().skip(i + 1).take(n).any(|x| keys.contains(&x.as_str()));
    for (i, w) in ws.iter().enumerate() {
        let prev = |k: usize| if i >= k { ws[i - k].as_str() } else { "" };
        let next = ws.get(i + 1).map(|s| s.as_str());
        let v = if w.starts_with('$') || matches!(next, Some("dollars" | "dollar" | "bucks" | "usd")) { money(w) }
            // "bankroll 500", "budget is 300", "bankroll of 1k"
            else if matches!(prev(1), "bankroll" | "budget") || (matches!(prev(1), "of" | "is") && matches!(prev(2), "bankroll" | "budget")) { money(w) }
            else { None };
        let Some(v) = v else { continue };
        let says_bankroll = matches!(next, Some("bankroll" | "budget" | "available" | "bank"))
            || back(i, 3, &["bankroll", "budget", "have", "got", "i've", "ive", "within"]);
        let each = fwd(i, 2, &["each", "apiece", "piece", "per"]);
        let total = back(i, 3, &["split", "across", "total", "between"]) || fwd(i, 3, &["total", "altogether", "overall", "split", "across", "between"]);
        let budget = back(i, 3, &["budget"]) || next == Some("budget");
        if each { m.stake.get_or_insert(v); }
        else if total && parlays > 1 { m.stake.get_or_insert(round2(v / parlays as f64)); }
        else if says_bankroll && !(budget && parlays > 1) { m.bankroll.get_or_insert(v); }
        else if parlays > 1 { m.unclear.get_or_insert(v); }
        else { m.stake.get_or_insert(v); }
    }
    m
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
    if f.parlay {
        f.legs = legs_of(&ws);
        f.parlays = parlays_of(&ws);
        if f.legs.is_some() || f.parlays.is_some() { touched = true; }
        let pm = parlay_money(&ws, f.parlays.unwrap_or(1));
        f.bankroll = pm.bankroll;
        f.parlay_stake = pm.stake;
        f.unclear_amount = pm.unclear;
    }

    let record = has(&l, &["track record", "record", "how have your", "how are your", "how did your", "win rate", "roi", "performance", "hit rate", "results so far"]);
    let game = has(&l, &[" vs ", " vs. ", " versus ", " @ ", " against ", "preview", "break down", "breakdown", "matchup", "match up"]);
    let bets = has(&l, &["bet", "pick", "play", "lock", "parlay", "wager", "what should i", "best", "value", "edge", "props", "money on", "put on"]);

    // the answer to "is the $100 for each parlay or in total?"
    let answers_unclear = prev.is_some_and(|p| p.unclear_amount.is_some())
        && has_word(&ws, &["each", "apiece", "per", "total", "split", "altogether", "overall", "bankroll"]);

    // follow-up: inherit the last request and apply the changes on top
    if let Some(p) = prev {
        if !record && !game && is_follow_up(&l, &ws) && (touched || answers_unclear || has(&l, &["more", "another", "again", "others", "different"])) {
            let mut m = p.clone();
            if sport_of(&ws, &l).is_some() { m.sport = f.sport.clone(); }
            if f.kind.is_some() || has(&l, &["any bet", "any kind", "everything", "all bets", "props and game"]) { m.kind = f.kind.clone(); }
            if f.day.is_some() { m.day = f.day.clone(); }
            if f.count.is_some() { m.count = f.count; }
            if f.bankroll.is_some() { m.bankroll = f.bankroll; }
            if f.risk.is_some() { m.risk = f.risk.clone(); }
            if f.odds.is_some() { m.odds = f.odds.clone(); }
            if has(&l, &["parlay", "same game", "sgp", "no parlay", "straight bet", "singles"]) { m.parlay = f.parlay; }
            if m.parlay {
                if let Some(n) = legs_of(&ws) { m.legs = Some(n); }
                if let Some(n) = parlays_of(&ws) { m.parlays = Some(n); }
                let n = m.parlays.unwrap_or(1);
                // amounts follow the parlay rules even when this message doesn't say "parlay"
                let pm = parlay_money(&ws, n);
                if pm == ParlayMoney::default() {
                    // "make it a parlay" keeps the stated bankroll; answer to the each/total question
                    m.bankroll = p.bankroll.or(m.bankroll);
                    if let Some(v) = p.unclear_amount {
                        if has_word(&ws, &["each", "apiece", "per"]) { m.parlay_stake = Some(v); m.unclear_amount = None; }
                        else if has_word(&ws, &["total", "split", "altogether", "overall"]) { m.parlay_stake = Some(round2(v / n as f64)); m.unclear_amount = None; }
                        else if has_word(&ws, &["bankroll"]) { m.bankroll = Some(v); m.unclear_amount = None; }
                    }
                } else {
                    if pm.bankroll.is_some() { m.bankroll = pm.bankroll; } else { m.bankroll = p.bankroll; }
                    if pm.stake.is_some() || pm.unclear.is_some() { m.parlay_stake = pm.stake; m.unclear_amount = pm.unclear; }
                    else if pm.bankroll.is_some() && p.unclear_amount.is_some() { m.unclear_amount = None; }
                }
            } else {
                (m.parlay_stake, m.legs, m.parlays, m.unclear_amount) = (None, None, None, None);
            }
            return Route { intent: Intent::Bets, filters: m, inherited: true };
        }
    }

    // "how does the parlay builder price legs?" is about the product, not a request for bets
    let concept_start = ["explain", "how does", "how do ", "how is ", "how are ", "what is", "what's", "whats ", "why ", "define", "tell me about", "what does", "what do "]
        .iter().any(|p| l.starts_with(p));
    let asks_for_bets = has(&l, &["best bet", "bets today", "bets tonight", "should i bet", "give me", "picks today", "props today", "picks tonight", "props tonight",
        "what should i", "any bets", "any props", "any picks", "worth betting", "worth a bet", "bet on today", "bet on tonight"])
        // "what is the best NFL moneyline today?", "which spread should I take tonight"
        || (has(&l, &["best ", "top ", "which ", "good ", "favorite "]) && has(&l, &["moneyline", "money line", "spread", "total", "over/under", "puck line", "run line", "prop", "pick", "play", "lean", "value", "edge"]));
    let concept = concept_start && !asks_for_bets;
    // questions about the models themselves, before "performance" makes them a track-record question
    let model_q = has(&l, &["calibrat", "brier", "log loss", "model quality", "which model", "which engine", "model perform", "models perform", "engine perform",
        "overconfident", "underconfident", "validat", "scorecard", "why isn't", "why aren't", "why no ", "why don't you", "why do you not", "not publishing",
        "publishing", "beat the market", "beat closing", "beating the market", "closing line value", "clv", "quarantin", "trust the model", "how accurate",
        "how good is the model", "how good are the models", "verified edge", "proven"]);
    // "how good are the NHL player props models?", "can I trust the NBA engine"
    let model_q = model_q || (has(&l, &["model", "engine", "simulator"])
        && has(&l, &["how good", "how reliable", "reliable", "trust", "accurate", "accuracy", "working", "doing", "track record", "edge", "beat"]));
    let intent = if model_q { Intent::Model } else if record { Intent::Record } else if concept { Intent::General } else if game { Intent::Game } else if bets || (touched && f.risk.is_some()) { Intent::Bets } else { Intent::General };
    Route { intent, filters: f, inherited: false }
}

/// "$50", "$12.50"
pub fn money_s(v: f64) -> String { if (v - v.round()).abs() < 0.005 { format!("{v:.0}") } else { format!("{v:.2}") } }

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
    if f.parlay {
        let legs = f.legs.map(|n| format!("{n}-leg ")).unwrap_or_default();
        parts.push(match f.parlays.filter(|n| *n > 1) { Some(n) => format!("{n} {legs}parlays"), None => format!("{legs}parlay") });
        if let Some(st) = f.parlay_stake { parts.push(format!("${} {}", money_s(st), if f.parlays.unwrap_or(1) > 1 { "per parlay" } else { "stake" })); }
    }
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
    fn a_parlay_amount_is_the_stake_not_the_bankroll() {
        for (text, stake, legs) in [("Make me a parlay with $50.", 50.0, None), ("Build a 3-leg parlay for $50", 50.0, Some(3)),
            ("Give me a $25 four-leg parlay", 25.0, Some(4)), ("Create a parlay using $100.", 100.0, None), ("make me a 3-leg parlay with $50", 50.0, Some(3))] {
            let f = route(text, None).filters;
            assert!(f.parlay, "{text}");
            assert_eq!((f.parlay_stake, f.bankroll, f.legs, f.unclear_amount), (Some(stake), None, legs, None), "{text}");
        }
    }

    #[test]
    fn a_parlay_bankroll_needs_bankroll_words() {
        for (text, bank) in [("I have a $50 bankroll, build me a parlay", 50.0), ("My betting budget is $100, give me a parlay", 100.0),
            ("I have $200 available to bet, make a parlay", 200.0), ("Build parlays within my $100 bankroll", 100.0), ("parlay please, bankroll of 1k", 1000.0)] {
            let f = route(text, None).filters;
            assert_eq!((f.bankroll, f.parlay_stake), (Some(bank), None), "{text}");
        }
        // both in one message
        let f = route("a $10 parlay, my bankroll is $200", None).filters;
        assert_eq!((f.parlay_stake, f.bankroll), (Some(10.0), Some(200.0)));
        // nothing stated: unstaked
        let f = route("make me a parlay", None).filters;
        assert_eq!((f.parlay_stake, f.bankroll), (None, None));
    }

    #[test]
    fn several_parlays_separate_each_from_total_and_ask_otherwise() {
        let f = route("give me two 3-leg parlays, $20 each", None).filters;
        assert_eq!((f.parlays, f.legs, f.parlay_stake, f.unclear_amount), (Some(2), Some(3), Some(20.0), None));
        let f = route("3 parlays with $90 total", None).filters;
        assert_eq!((f.parlays, f.parlay_stake), (Some(3), Some(30.0)));
        let f = route("split $100 across 2 parlays", None).filters;
        assert_eq!(f.parlay_stake, Some(50.0));
        let f = route("make 2 parlays with $100", None).filters;
        assert_eq!((f.parlay_stake, f.unclear_amount, f.bankroll), (None, Some(100.0), None));
        let f2 = route("make 2 parlays, my budget is $100", None).filters;
        assert_eq!(f2.unclear_amount, Some(100.0), "a budget across several parlays is unclear too");
        // the answer to the question
        assert_eq!(route("each", Some(&f)).filters.parlay_stake, Some(100.0));
        let t = route("total", Some(&f)).filters;
        assert_eq!((t.parlay_stake, t.unclear_amount), (Some(50.0), None));
        assert_eq!(route("$30 each", Some(&f)).filters.parlay_stake, Some(30.0));
        assert_eq!(describe(&route("two 3-leg parlays, $20 each", None).filters), "all sports · props + game lines · 2 3-leg parlays · $20 per parlay");
    }

    #[test]
    fn parlay_follow_ups_keep_the_rules() {
        let singles = route("give me 3 safe bets, I have $200", None).filters;
        assert_eq!(singles.bankroll, Some(200.0));
        let p = route("make it a parlay", Some(&singles)).filters;
        assert!(p.parlay);
        assert_eq!((p.bankroll, p.parlay_stake), (Some(200.0), None), "the stated bankroll carries over");
        let p2 = route("make it a $20 parlay", Some(&singles)).filters;
        assert_eq!((p2.parlay_stake, p2.bankroll), (Some(20.0), Some(200.0)));
        let p3 = route("put $15 on it", Some(&p2)).filters;
        assert_eq!(p3.parlay_stake, Some(15.0));
        let back = route("no parlay, straight bets", Some(&p2)).filters;
        assert_eq!((back.parlay, back.parlay_stake), (false, None));
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
        let r = route("what is the best NFL moneyline today?", None);
        assert_eq!((r.intent, r.filters.sport.as_deref()), (Intent::Bets, Some("nfl")));
        assert_eq!(route("how are your picks doing?", None).intent, Intent::Record);
        let r = route("Why isn't Monster publishing NFL spreads?", None);
        assert_eq!((r.intent, r.filters.sport.as_deref()), (Intent::Model, Some("nfl")));
        assert_eq!(route("which models are best calibrated?", None).intent, Intent::Model);
        assert_eq!(route("how good are the NHL player props models?", None).intent, Intent::Model);
        assert_eq!(route("give me the best NHL props", None).intent, Intent::Bets);
        assert_eq!(route("how is the NBA model performing vs the market", None).intent, Intent::Model);
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
