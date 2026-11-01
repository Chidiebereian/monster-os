//! Answer checks beyond numbers: the explanation must not recommend bets that
//! aren't on the card, flip a bet's side, promise outcomes, misstate how much
//! of the bankroll is at risk, or skip the risk. Serious problems trigger one
//! rewrite with the problems spelled out; whatever survives is shown to the user.

use serde::Serialize;

use crate::card::Card;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Issue {
    pub code: &'static str,
    pub detail: String,
    /// serious issues trigger a rewrite
    pub serious: bool,
}

const GUARANTEES: &[&str] = &["guaranteed", "guarantee you", "can't lose", "cannot lose", "can not lose", "sure thing", "risk-free", "risk free", "free money",
    "100% chance", "100% sure", "will definitely", "definitely win", "lock of the", "easy money", "no-brainer", "cant lose", "certain to win"];
const RISK_WORDS: &[&str] = &["risk", "variance", "lose", "afford", "no guarantee", "not guaranteed", "uncertain", "estimate", "could go wrong", "losing"];
const WHOLE_BANKROLL: &[&str] = &["entire bankroll", "whole bankroll", "all of your bankroll", "all your bankroll", "full bankroll", "all-in", "all in on", "your entire $", "all your money"];

fn sentences(text: &str) -> Vec<String> {
    text.split(['.', '!', '?', '\n']).map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect()
}

/// Capitalised multi-word names in the answer ("Cale Makar", "Kent Johnson").
pub fn names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    // a label ("**Notes:** Remember to ...") ends a run like a full stop does
    let text = text.replace([':', ';'], ". ");
    let words: Vec<&str> = text.split(|c: char| c.is_whitespace() || matches!(c, ',' | '(' | ')' | ':' | ';' | '*' | '"' | '[' | ']')).filter(|w| !w.is_empty()).collect();
    let cap = |w: &str| {
        let w = w.trim_end_matches(['.', '\'', 's']).trim_end_matches('\'');
        let mut c = w.chars();
        matches!(c.next(), Some(f) if f.is_uppercase()) && w.len() > 1 && w.chars().skip(1).all(|x| x.is_lowercase() || x == '-' || x == '\'')
    };
    let mut i = 0;
    while i < words.len() {
        if cap(words[i]) {
            let mut j = i + 1;
            while j < words.len() && cap(words[j]) && !words[j - 1].ends_with('.') { j += 1; }
            // sentence starters ("Take Cale Makar") aren't part of the name
            let mut s = i;
            while s < j && STARTERS.contains(&words[s].to_lowercase().as_str()) { s += 1; }
            // possessives, then ordinary capitalised words at either edge
            // ("Risk The", "Cale Makar's", "Cale Makar Under") aren't the name
            let run: Vec<String> = words[s..j].iter()
                .map(|w| w.trim_end_matches(['.', ',']).trim_end_matches("'s").trim_end_matches("\u{2019}s").to_string()).collect();
            let generic = |w: &String| { let w = w.to_lowercase(); GENERIC.contains(&w.as_str()) || STARTERS.contains(&w.as_str()) };
            let (mut a, mut b) = (0, run.len());
            while a < b && generic(&run[a]) { a += 1; }
            while b > a && generic(&run[b - 1]) { b -= 1; }
            if b - a >= 2 { out.push(run[a..b].join(" ")); }
            i = j;
        } else { i += 1; }
    }
    out
}

const STARTERS: &[&str] = &["take", "the", "this", "these", "that", "also", "and", "but", "if", "for", "with", "both", "here", "then", "back", "fade", "play",
    "consider", "bet", "hammer", "pair", "add", "skip", "avoid", "lean", "go", "my", "our", "your", "a", "an", "on", "at", "in", "of", "to"];

/// Ordinary words a model capitalises in headings and sentence starts; trimmed
/// from the edges of a capitalised run so "Sizing For" or "Risk Aversion Adjust"
/// isn't mistaken for a name.
const GENERIC: &[&str] = &["risk", "risks", "sizing", "safe", "safer", "bet", "bets", "management", "flexibility", "example", "team", "teams", "over", "under",
    "by", "you", "if", "is", "it", "its", "or", "so", "can", "adjust", "aversion", "proportional", "projection", "projections", "evidence", "evidences",
    "points", "rebounds", "assists", "goals", "shots", "saves", "notes", "note", "remember", "overall", "why", "main", "total", "stake", "stakes", "parlay",
    "leg", "legs", "edge", "model", "market", "odds", "price", "unit", "units", "bankroll", "kelly", "fractional", "grade", "grades", "card", "pick", "picks",
    "today", "tonight", "game", "games", "home", "away", "moneyline", "spread", "line", "prop", "props", "player", "players", "profit", "expected", "value",
    "source", "sources", "summary", "key", "factor", "factors", "confidence", "high", "low", "strong", "monitor", "injury", "injuries", "lineup", "keep",
    "use", "make", "each", "all", "some", "aggressive", "conservative", "plus", "money", "when", "what", "how", "we", "i", "first", "second", "third"];

/// Phrases that are capitalised in prose but are not people or teams.
const COMMON: &[&str] = &["monster os", "monster gpt", "monstergpt", "bet card", "ai picks", "the bet", "the model", "expected profit", "total stake", "model expected",
    "draft kings", "fan duel", "parlay builder", "games hub", "stake bet", "here are", "these bets", "this bet", "bet one", "bet two", "bet three", "main risk", "why it", "overall take"];

/// Names in the answer that the evidence never mentions.
pub fn unknown_names(answer: &str, evidence: &str) -> Vec<String> {
    let ev = evidence.to_lowercase();
    let ev_words: std::collections::HashSet<&str> = ev.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let mut unknown: Vec<String> = names(answer).into_iter().filter(|n| {
        let nl = n.to_lowercase();
        !COMMON.iter().any(|c| nl.contains(c)) && !ev.contains(&nl)
            // every word appears somewhere in the evidence ("Colorado Avalanche Col")
            && !nl.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).all(|w| ev_words.contains(w))
            && !nl.split_whitespace().last().map(|last| last.len() > 3 && ev.contains(last)).unwrap_or(false)
    }).collect();
    unknown.dedup();
    unknown
}

/// Does `text` contain the number token `n` ("$20", "48%") on its own, not inside "$200"?
fn has_number(text: &str, n: &str) -> bool {
    let mut from = 0;
    while let Some(i) = text[from..].find(n) {
        let (a, b) = (from + i, from + i + n.len());
        let before = text[..a].chars().next_back();
        let digitish = |c: Option<char>| matches!(c, Some(c) if c.is_ascii_digit() || c == '.' || c == ',');
        let tail = &text[b..];
        let after_ok = !tail.starts_with(|c: char| c.is_ascii_digit())
            && !((tail.starts_with('.') || tail.starts_with(',')) && tail[1..].starts_with(|c: char| c.is_ascii_digit()));
        if !digitish(before) && after_ok { return true; }
        from = b;
    }
    false
}

/// Last-resort cleanup: drop the sentences (or whole bullets, with the bullet's
/// indented sub-points) that mention names or numbers the data doesn't support.
/// Returns the cleaned text and how many pieces were removed.
pub fn scrub(text: &str, names: &[String], numbers: &[String]) -> (String, usize) {
    let bad = |s: &str| names.iter().any(|n| s.contains(n.as_str())) || numbers.iter().any(|n| has_number(s, n));
    let indent = |l: &str| l.len() - l.trim_start().len();
    let mut out: Vec<String> = Vec::new();
    let mut removed = 0;
    let mut skip_deeper_than: Option<usize> = None;
    for line in text.lines() {
        if let Some(d) = skip_deeper_than {
            if !line.trim().is_empty() && indent(line) > d { removed += 1; continue; }
            skip_deeper_than = None;
        }
        if !bad(line) { out.push(line.to_string()); continue; }
        // a heading-like line ("- **Bet 2 (Luka Doncic ...):**") goes with its sub-points
        if line.trim().trim_end_matches('*').ends_with(':') { removed += 1; skip_deeper_than = Some(indent(line)); continue; }
        let mut kept = Vec::new();
        let mut rest = line;
        while !rest.is_empty() {
            // sentence ends at ". " (not the "." inside "1.5")
            let cut = rest.match_indices(". ").map(|(i, _)| i + 2).next().unwrap_or(rest.len());
            let (sent, tail) = rest.split_at(cut);
            if bad(sent) { removed += 1; } else { kept.push(sent); }
            rest = tail;
        }
        let joined = kept.concat();
        if joined.trim().trim_start_matches(['-', '*', ' ']).chars().any(|c| c.is_alphanumeric()) { out.push(joined.trim_end().to_string()); }
    }
    let mut s = out.join("\n");
    while s.contains("\n\n\n") { s = s.replace("\n\n\n", "\n\n"); }
    (s.trim().to_string(), removed)
}

/// Check an answer against the card and the evidence it was given.
pub fn check(answer: &str, card: Option<&Card>, evidence: &str) -> Vec<Issue> {
    let mut issues = Vec::new();
    let l = answer.to_lowercase();

    if let Some(g) = GUARANTEES.iter().find(|g| l.contains(*g)) {
        issues.push(Issue { code: "guarantee", detail: format!("promises an outcome (\"{g}\"); every pick here is a probability, not a certainty"), serious: true });
    }

    // names the evidence never mentions (likely invented players/teams or bets not on the card)
    let unknown = unknown_names(answer, evidence);
    if !unknown.is_empty() {
        issues.push(Issue { code: "unknown_names", detail: format!("mentions {} which {} not in the data", unknown.join(", "), if unknown.len() == 1 { "is" } else { "are" }), serious: true });
    }

    if let Some(c) = card {
        // side flips: a sentence naming the player says the opposite side and not the right one
        for b in &c.bets {
            let side = b.side.to_lowercase();
            let opposite = match side.as_str() { "over" => "under", "under" => "over", _ => continue };
            let last = b.pick.split_whitespace().nth(1).unwrap_or("").to_lowercase();
            if last.len() < 3 { continue; }
            for s in sentences(answer) {
                if s.contains(&last) && s.contains(opposite) && !s.contains(&side) {
                    issues.push(Issue { code: "side_flip", detail: format!("says {opposite} for {} but the card has {side}", b.pick), serious: true });
                    break;
                }
            }
        }
        // bankroll share
        if let (Some(total), Some(bank)) = (c.total_stake_usd, c.filters.bankroll) {
            if bank > 0.0 && total > 0.0 && total / bank < 0.5 {
                if let Some(p) = WHOLE_BANKROLL.iter().find(|p| l.contains(*p)) {
                    issues.push(Issue { code: "bankroll_claim", detail: format!("says \"{p}\" but the card risks ${total:.0} of ${bank:.0} ({:.0}%)", total / bank * 100.0), serious: true });
                }
            }
        }
        // recommending bets when the card is empty
        if c.bets.is_empty() && ["i recommend", "you should bet", "take the", "bet on the", "my pick is", "best bet is"].iter().any(|p| l.contains(p)) {
            issues.push(Issue { code: "off_card", detail: "recommends a bet although the card is empty".into(), serious: true });
        }
        if !c.bets.is_empty() && !RISK_WORDS.iter().any(|w| l.contains(w)) {
            issues.push(Issue { code: "no_risk", detail: "never mentions risk".into(), serious: false });
        }
    }
    issues
}

/// Feedback for the rewrite pass.
pub fn feedback(issues: &[Issue]) -> String {
    let mut s = String::from("Your previous draft had these problems. Rewrite the whole answer and fix every one of them; keep using only EVIDENCE:\n");
    for i in issues.iter().filter(|i| i.serious) { s.push_str(&format!("- {}\n", i.detail)); }
    s
}

/// One plain sentence about the stake, added when the model never mentions risk.
pub fn risk_line(card: &Card) -> String {
    match (card.total_stake_usd, card.filters.bankroll) {
        (Some(t), Some(b)) if b > 0.0 => format!("\n\nThese are model estimates, not certainties: the card puts ${t:.0} of your ${b:.0} bankroll ({:.0}%) at risk, so only stake what you can afford to lose.", t / b * 100.0),
        _ => "\n\nThese are model estimates, not certainties; any of these bets can lose, so only stake what you can afford to lose.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::route;
    use serde_json::json;

    fn card() -> Card {
        let picks = vec![json!({ "id": 1, "sport": "nhl", "kind": "prop", "game_label": "CGY @ COL", "subject": "makar", "subject_name": "Cale Makar", "label": "Under 1.5 Points",
            "market": "points", "line": 1.5, "side": "under", "price": -230, "book_title": "DraftKings", "p_model": 0.81, "p_market": 0.65, "edge": 0.16, "ev": 0.16,
            "grade": "A+", "tier": "proven", "stake": 0.02, "status": "open", "playable_now": true, "starts_at": "2026-11-01T01:00:00Z", "writeup": "Makar reason.", "risk_flags": [] })];
        crate::card::build(&picks, &route("safe bets with $200", None).filters, chrono::NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(), None)
    }

    #[test]
    fn flags_the_7b_bankroll_mistake() {
        let c = card();
        let ev = crate::card::render_text(&c);
        let issues = check("Cale Makar under 1.5 is the play. Place these bets understanding they represent your entire bankroll tonight.", Some(&c), &ev);
        assert!(issues.iter().any(|i| i.code == "bankroll_claim" && i.serious), "{issues:?}");
    }

    #[test]
    fn flags_guarantees_side_flips_and_invented_names() {
        let c = card();
        let ev = crate::card::render_text(&c);
        let a = "Cale Makar over 1.5 points is guaranteed. Also hammer Connor Mcdavid tonight.";
        let codes: Vec<&str> = check(a, Some(&c), &ev).iter().map(|i| i.code).collect();
        assert!(codes.contains(&"guarantee"));
        assert!(codes.contains(&"side_flip"));
        assert!(codes.contains(&"unknown_names"), "{codes:?}");
    }

    #[test]
    fn a_clean_answer_passes() {
        let c = card();
        let ev = crate::card::render_text(&c);
        let a = "Cale Makar under 1.5 points is the strongest edge on the card [S1]. The main risk is a big Avalanche night. It's an estimate, so stake only what you can afford to lose.";
        let issues = check(a, Some(&c), &ev);
        assert!(issues.iter().all(|i| i.code == "unknown_names" && i.detail.contains("Avalanche")) || issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn missing_risk_is_minor_and_fixable() {
        let c = card();
        let ev = crate::card::render_text(&c);
        let issues = check("Cale Makar under 1.5 points is the strongest edge on the card.", Some(&c), &ev);
        assert_eq!(issues.len(), 1);
        assert!(!issues[0].serious && issues[0].code == "no_risk");
        assert!(risk_line(&c).contains("$4 of your $200"));
    }

    #[test]
    fn name_extraction() {
        assert_eq!(names("Take Cale Makar and Kent Johnson; the Bet Card says so."), vec!["Cale Makar", "Kent Johnson"]);
        assert_eq!(names("Back the Colorado Avalanche tonight."), vec!["Colorado Avalanche"]);
    }

    #[test]
    fn section_labels_are_not_names() {
        assert!(names("**Notes:** Remember to monitor injuries. **Sizing:** Stake $20").is_empty());
        assert_eq!(names("Bet 1 (Cale Makar Under 1.5 Points): proven"), vec!["Cale Makar"]);
    }

    #[test]
    fn scrub_drops_only_the_unsupported_pieces() {
        let text = "**Overall Take:** Two solid unders.\n\n- **Bet 1 (Cale Makar Under 1.5 Points):**\n  - Why: proven model edge.\n- **Bet 2 (Luka Doncic Over 30.5):**\n  - Why: hot streak.\n  - Risk: minutes.\n\n**Sizing:** Stake $20 in total. That is 48% safer than last week. Only stake what you can afford to lose.";
        let (out, n) = scrub(text, &["Luka Doncic".into()], &["48%".into()]);
        assert!(!out.contains("Doncic") && !out.contains("hot streak") && !out.contains("48%"), "{out}");
        assert!(out.contains("Cale Makar") && out.contains("proven model edge") && out.contains("Stake $20 in total.") && out.contains("afford to lose"), "{out}");
        assert_eq!(n, 4);
        // "$20" is kept when only "$200" is unsupported
        assert_eq!(scrub("Your $20 stake.", &[], &["$200".into()]).1, 0);
    }

    #[test]
    fn generic_capitalised_words_are_not_names() {
        for t in ["Risk Aversion Adjust the stake.", "Sizing For this card, stake $20.", "Risks The main risk is injuries.", "Flexibility You can scale down.",
                  "New Safe Bet here.", "Example If the line moves, pass."] {
            assert!(names(t).is_empty(), "{t}: {:?}", names(t));
        }
        assert_eq!(names("Cale Makar's line is soft."), vec!["Cale Makar"]);
        assert_eq!(names("Luka Doncic and Stephen Curry are hot."), vec!["Luka Doncic", "Stephen Curry"]);
    }

    #[test]
    fn real_inventions_still_flagged_and_known_words_pass() {
        let c = card();
        let ev = crate::card::render_text(&c);
        let flagged = |a: &str| check(a, Some(&c), &ev).iter().any(|i| i.code == "unknown_names");
        assert!(flagged("Also consider Luka Doncic over 30.5 points. The risk is variance."));
        assert!(!flagged("Team Makar Col is how the feed lists Cale Makar's under. The risk is variance."));
    }
}
