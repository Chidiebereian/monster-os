//! Model vs market for a sport's games that have not started (hub schedule).
//!
//! AI Picks only publishes a bet when the engine's probability, blended toward
//! the market by a weight fitted on closing lines, still clears the minimum
//! edge. When nothing does (the NFL game model never beat closing lines in its
//! backtests, so its weight is 0), the user still deserves the numbers: the raw
//! model next to the market price for every game, labelled as leans, plus the
//! track record that explains why none of them is an official pick.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Lean {
    pub game: String,
    /// "4:25 PM EST" from the ESPN status line
    pub when: String,
    /// moneyline | spread | total
    pub market: &'static str,
    /// "DEN ML", "KC -1.5", "Under 41.5"
    pub pick: String,
    pub price: Option<i64>,
    pub book: Option<String>,
    /// one line of evidence: "model 50.7% vs market 49.2%"
    pub numbers: String,
    /// size of the disagreement, comparable within a market (probability points or game points)
    pub gap: f64,
}

fn implied(a: i64) -> f64 { 1.0 / crate::card::decimal(a) }
/// Power-method de-vig, as projection-service picks::devig (research R10).
fn novig(a: i64, b: i64) -> f64 {
    let (x, y) = (implied(a), implied(b));
    if !(x > 0.0 && y > 0.0 && x < 1.0 && y < 1.0) { return x / (x + y); }
    let (mut lo, mut hi) = (0.01_f64, 50.0_f64);
    for _ in 0..60 { let k = (lo + hi) / 2.0; if x.powf(k) + y.powf(k) > 1.0 { lo = k; } else { hi = k; } }
    x.powf((lo + hi) / 2.0)
}
fn odds_s(p: i64) -> String { if p > 0 { format!("+{p}") } else { p.to_string() } }
fn signed(x: f64) -> String { if x > 0.0 { format!("+{x}") } else { format!("{x}") } }
fn r1(x: f64) -> f64 { (x * 10.0).round() / 10.0 }
fn book_name(b: &str) -> String {
    match b { "draftkings" => "DraftKings".into(), "fanduel" => "FanDuel".into(), "betmgm" => "BetMGM".into(), "caesars" => "Caesars".into(),
        "espnbet" | "espn bet" => "ESPN BET".into(), "bet365" => "bet365".into(), "polymarket" => "Polymarket".into(),
        _ => { let mut c = b.chars(); c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default() } }
}

/// moneyline | spread | total, when the question names one.
pub fn market_asked(text: &str) -> Option<&'static str> {
    let l = text.to_lowercase();
    if l.contains("moneyline") || l.contains("money line") || l.contains(" ml ") || l.contains("to win outright") || l.contains("who wins") || l.contains("who will win") { return Some("moneyline"); }
    if l.contains("spread") || l.contains("puck line") || l.contains("run line") || l.contains("cover") || l.contains("handicap") { return Some("spread"); }
    if l.contains("total") || l.contains("over/under") || l.contains("o/u") || l.contains("over under") { return Some("total"); }
    None
}

/// (price, line, book) of one side: recorded lines first, ESPN's odds otherwise.
fn side(g: &Value, market: &str, side: &str) -> Option<(i64, Option<f64>, Option<String>)> {
    let l = &g["lines"][market][side];
    if let Some(p) = l["price"].as_i64() { return Some((p, l["line"].as_f64(), l["book"].as_str().map(|s| s.to_string()))); }
    let o = &g["espn_odds"];
    let book = o["provider"].as_str().map(|s| s.to_lowercase());
    match (market, side) {
        ("moneyline", "home") => o["home_ml"].as_i64().map(|p| (p, None, book)),
        ("moneyline", "away") => o["away_ml"].as_i64().map(|p| (p, None, book)),
        ("spread", "home") => o["spread_home"].as_f64().map(|x| (-110, Some(x), book)),
        ("spread", "away") => o["spread_home"].as_f64().map(|x| (-110, Some(-x), book)),
        ("total", "over" | "under") => o["total"].as_f64().map(|x| (-110, Some(x), book)),
        _ => None,
    }
}

/// Leans for the games still to start, biggest disagreement first within each market.
pub fn leans(slate: &Value, market: Option<&str>) -> Vec<Lean> {
    let mut out = Vec::new();
    for g in slate["games"].as_array().into_iter().flatten() {
        if g["status"]["state"].as_str() != Some("pre") { continue; }
        let m = &g["model"];
        let (Some(home), Some(away)) = (g["home"]["abbr"].as_str(), g["away"]["abbr"].as_str()) else { continue };
        let game = format!("{away} @ {home}");
        let when = g["status"]["detail"].as_str().map(|d| d.split(" - ").last().unwrap_or(d).to_string()).unwrap_or_default();
        let want = |k: &str| market.map(|x| x == k).unwrap_or(true);
        if want("moneyline") {
            if let (Some(wh), Some((ph, _, bh)), Some((pa, _, ba))) = (m["win_home"].as_f64(), side(g, "moneyline", "home"), side(g, "moneyline", "away")) {
                let mk = novig(ph, pa);
                let (team, pm, pk, price, book) = if wh >= mk { (home, wh, mk, ph, bh) } else { (away, 1.0 - wh, 1.0 - mk, pa, ba) };
                out.push(Lean { game: game.clone(), when: when.clone(), market: "moneyline", pick: format!("{team} ML"), price: Some(price), book,
                    numbers: format!("model {}% to win vs market {}%", r1(pm * 100.0), r1(pk * 100.0)), gap: r1((pm - pk) * 100.0) });
            }
        }
        if want("spread") {
            if let (Some(sh), Some((ph, Some(lh), bh))) = (m["spread_home"].as_f64(), side(g, "spread", "home")) {
                // model expects home to win by -sh; home +lh covers when margin + lh > 0
                let edge = r1(-sh + lh);
                let (team, line, price, book) = if edge >= 0.0 { (home, lh, ph, bh) } else {
                    let a = side(g, "spread", "away");
                    (away, -lh, a.as_ref().map(|x| x.0).unwrap_or(-110), a.and_then(|x| x.2))
                };
                let margin = r1(-sh);
                let view = if margin > 0.0 { format!("{home} by {margin}") } else if margin < 0.0 { format!("{away} by {}", -margin) } else { "a pick'em".into() };
                out.push(Lean { game: game.clone(), when: when.clone(), market: "spread", pick: format!("{team} {}", signed(line)), price: Some(price), book,
                    numbers: format!("model has {view}, line {home} {}", signed(lh)), gap: edge.abs() });
            }
        }
        if want("total") {
            if let (Some(t), Some((po, Some(line), bo))) = (m["total"].as_f64(), side(g, "total", "over")) {
                let d = r1(t - line);
                let (pick, price, book) = if d >= 0.0 { ("Over", po, bo) } else {
                    let u = side(g, "total", "under");
                    ("Under", u.as_ref().map(|x| x.0).unwrap_or(-110), u.and_then(|x| x.2))
                };
                out.push(Lean { game: game.clone(), when: when.clone(), market: "total", pick: format!("{pick} {line}"), price: Some(price), book,
                    numbers: format!("model {} points vs line {line}", r1(t)), gap: d.abs() });
            }
        }
    }
    let order = |m: &str| match m { "moneyline" => 0, "spread" => 1, _ => 2 };
    out.sort_by(|a, b| order(a.market).cmp(&order(b.market)).then(b.gap.partial_cmp(&a.gap).unwrap_or(std::cmp::Ordering::Equal)));
    out
}

/// Why a sport's game lines have no official picks, from /picks/record tiers and evidence.
pub fn why_no_pick(record: Option<&Value>, sport: &str, market: Option<&str>) -> String {
    let markets: Vec<&str> = market.map(|m| vec![m]).unwrap_or_else(|| vec!["moneyline", "spread", "total"]);
    let up = sport.to_uppercase();
    let Some(r) = record else { return format!("No {up} game line clears the minimum edge after the model is blended toward the market.") };
    let ev = r["evidence"].as_array().cloned().unwrap_or_default();
    let tier = |m: &str| r["tiers"].as_array().into_iter().flatten()
        .find(|t| t["engine"].as_str() == Some(sport) && t["market"].as_str() == Some(m)).and_then(|t| t["tier"].as_str()).unwrap_or("unproven");
    let unproven: Vec<&str> = markets.iter().copied().filter(|m| !matches!(tier(m), "proven" | "baseline")).collect();
    if unproven.len() == markets.len() {
        // the market-comparison backtest that set the blend weight
        let bt = ev.iter().find(|e| e["engine"].as_str() == Some(sport) && e["source"] == "backtest" && e["comparison"] == "market"
            && markets.contains(&e["market"].as_str().unwrap_or("")));
        let what = if markets.len() == 1 { format!("{up} {}", markets[0]) } else { format!("{up} game lines") };
        return match bt {
            Some(e) => format!("Our {what} model is unproven: in walk-forward backtests over {} seasons ({} games) it did not beat closing lines (fitted market weight {}), so its edges are blended away and nothing clears the minimum edge.",
                e["seasons"].as_i64().unwrap_or(0), e["n"].as_i64().unwrap_or(0), e["detail"]["w"].as_f64().unwrap_or(0.0)),
            None => format!("Our {what} model has no track record against real lines yet, so its edges are not published as picks."),
        };
    }
    format!("The {up} model and the market roughly agree today: no game line clears the minimum edge after blending toward the market.")
}

/// Plain-text answer for a bets question whose bet card came back empty.
/// `game` narrows it to one matchup ("KC @ DEN").
pub fn reply(sport: &str, market: Option<&str>, leans: &[Lean], why: &str, limit: usize, game: Option<&str>) -> String {
    let up = sport.to_uppercase();
    let what = market.map(|m| format!("{up} {m}")).unwrap_or_else(|| up.clone());
    let (head, scope) = match game {
        Some(g) => (format!("No official pick on {g} right now."), "this game"),
        None => (format!("No official {what} pick right now."), "the games still to start"),
    };
    let mut s = format!("{head} {why}\n\nHere is the raw model against the market for {scope}. These are leans, not picks, so size them small or pass:\n");
    for (i, l) in leans.iter().take(limit).enumerate() {
        let price = match (l.price, &l.book) { (Some(p), Some(b)) => format!(" at {} ({})", odds_s(p), book_name(b)), (Some(p), None) => format!(" at {}", odds_s(p)), _ => String::new() };
        s.push_str(&format!("\n{}. {}{}: {} ({}{})", i + 1, l.pick, price, l.numbers, l.game, if l.when.is_empty() { String::new() } else { format!(", {}", l.when) }));
    }
    s
}

/// Compact rows for the model's EVIDENCE block.
pub fn evidence(leans: &[Lean], limit: usize) -> String {
    leans.iter().take(limit).map(|l| format!("- {} ({}): {} -> lean {}{}", l.game, l.market, l.numbers, l.pick, l.price.map(|p| format!(" {}", odds_s(p))).unwrap_or_default()))
        .collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn slate() -> Value {
        json!({ "games": [
            { "status": { "state": "pre", "detail": "11/1 - 4:25 PM EST" }, "home": { "abbr": "DEN" }, "away": { "abbr": "KC" },
              "model": { "win_home": 0.548, "spread_home": -1.0, "total": 38.0 },
              "lines": { "moneyline": { "home": { "price": -108, "book": "draftkings" }, "away": { "price": -112, "book": "draftkings" } },
                         "spread": { "home": { "price": -120, "line": 1.5, "book": "draftkings" }, "away": { "price": 100, "line": -1.5, "book": "draftkings" } },
                         "total": { "over": { "price": -110, "line": 41.5, "book": "draftkings" }, "under": { "price": -110, "line": 41.5, "book": "draftkings" } } } },
            { "status": { "state": "in", "detail": "Q2" }, "home": { "abbr": "BUF" }, "away": { "abbr": "BAL" }, "model": { "win_home": 0.6 } },
        ]})
    }

    #[test]
    fn leans_compare_model_and_market_per_market() {
        let l = leans(&slate(), None);
        assert_eq!(l.len(), 3, "started games are skipped");
        assert_eq!(l[0].pick, "DEN ML");
        assert_eq!(l[0].price, Some(-108));
        assert!(l[0].numbers.contains("54.8%"), "{}", l[0].numbers);
        assert_eq!(l[1].pick, "DEN +1.5");
        assert_eq!(l[1].gap, 2.5);
        assert_eq!(l[2].pick, "Under 41.5");
        assert_eq!(l[2].gap, 3.5);
        assert_eq!(leans(&slate(), Some("total")).len(), 1);
    }

    #[test]
    fn no_pick_reason_comes_from_the_backtest() {
        let rec = json!({ "tiers": [{ "engine": "nfl", "market": "moneyline", "tier": "unproven" }],
            "evidence": [{ "engine": "nfl", "market": "moneyline", "source": "backtest", "comparison": "market", "n": 957, "seasons": 4, "detail": { "w": 0.0 } }] });
        let why = why_no_pick(Some(&rec), "nfl", Some("moneyline"));
        assert!(why.contains("4 seasons (957 games)") && why.contains("weight 0"), "{why}");
        let r = reply("nfl", Some("moneyline"), &leans(&slate(), Some("moneyline")), &why, 5, None);
        assert!(r.contains("1. DEN ML at -108"), "{r}");
        let g = reply("nfl", None, &leans(&slate(), None), &why, 5, Some("KC @ DEN"));
        assert!(g.starts_with("No official pick on KC @ DEN") && g.contains("3. Under 41.5"), "{g}");
        assert!(r.starts_with("No official NFL moneyline pick") && r.contains("1. DEN ML at -108"), "{r}");
    }

    #[test]
    fn market_words() {
        assert_eq!(market_asked("what is the best NFL moneyline today?"), Some("moneyline"));
        assert_eq!(market_asked("best spread tonight"), Some("spread"));
        assert_eq!(market_asked("any good totals"), Some("total"));
        assert_eq!(market_asked("best bets"), None);
    }
}
