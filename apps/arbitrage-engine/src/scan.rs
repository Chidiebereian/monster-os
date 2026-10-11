//! Cross-venue scan: match Kalshi game-winner markets to the same game at
//! sportsbooks (via odds-service) and price each market against the
//! sportsbooks' no-vig consensus probability.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use common::MarketQuote;
use serde::{Deserialize, Serialize};

use crate::{best_edge, Side};

/// Kalshi's event time and the sportsbook start time must be this close.
const MAX_TIME_GAP_HOURS: i64 = 12;

#[derive(Deserialize)]
pub struct OddsEvent {
    pub id: String,
    pub commence_time: DateTime<Utc>,
    pub home_team: String,
    pub away_team: String,
    #[serde(default)]
    pub bookmakers: Vec<serde_json::Value>,
    #[serde(default)]
    pub consensus: Vec<ConsensusPrice>,
}

#[derive(Deserialize)]
pub struct ConsensusPrice {
    pub name: String,
    pub probability: f64,
}

#[derive(Serialize)]
pub struct Opportunity {
    pub market_id: String,
    pub outcome: String,
    pub sportsbook_team: String,
    pub sportsbook_event_id: String,
    pub game: String,
    pub commence_time: DateTime<Utc>,
    /// Game already started: sportsbook and Kalshi prices may be from
    /// different moments, so treat any edge with suspicion.
    pub in_play: bool,
    pub books: usize,
    pub fair_probability: f64,
    pub side: Side,
    pub price: f64,
    pub edge: f64,
    pub est_fee: f64,
    pub net_edge: f64,
    pub net_roi: f64,
    pub quote: MarketQuote,
}

#[derive(Serialize)]
pub struct Unmatched {
    pub event_id: String,
    pub outcomes: Vec<String>,
    pub reason: String,
}

fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

/// Does a venue's short team label ("Chicago WS", "NY Yankees", "San Diego",
/// "LA D") name this full team ("Chicago White Sox")? Label tokens must match
/// team tokens in order, either exactly or as initials of consecutive team
/// tokens ("ws" = White Sox, "ny" = New York); the final label token may also
/// be a prefix of the next team token ("c" for Cubs). Trailing team tokens
/// (the nickname) may go unmatched.
pub fn label_matches(label: &str, team: &str) -> bool {
    let (l, t) = (tokens(label), tokens(team));
    if l.is_empty() {
        return false;
    }
    let mut j = 0;
    for (i, tok) in l.iter().enumerate() {
        let last = i == l.len() - 1;
        if t.get(j) == Some(tok) {
            j += 1;
            continue;
        }
        let k = tok.len();
        let initials = j + k <= t.len()
            && tok.chars().zip(&t[j..j + k]).all(|(c, word)| word.starts_with(c));
        if k > 1 && initials {
            j += k;
            continue;
        }
        if last && t.get(j).is_some_and(|w| w.starts_with(tok.as_str())) {
            return true;
        }
        return false;
    }
    true
}

/// Kalshi's taker fee is about 0.07 × P × (1 − P) per contract (rounded up
/// to the cent per order, so small orders pay slightly more).
pub fn kalshi_fee(price: f64) -> f64 {
    0.07 * price * (1.0 - price)
}

/// Which sportsbook team each Kalshi outcome label refers to, if the pair
/// maps one-to-one onto this game.
fn assign<'a>(labels: [&str; 2], e: &'a OddsEvent) -> Option<[&'a str; 2]> {
    let (h, a) = (e.home_team.as_str(), e.away_team.as_str());
    let straight = label_matches(labels[0], h) && label_matches(labels[1], a);
    let swapped = label_matches(labels[0], a) && label_matches(labels[1], h);
    match (straight, swapped) {
        (true, false) => Some([h, a]),
        (false, true) => Some([a, h]),
        _ => None,
    }
}

pub fn scan(quotes: Vec<MarketQuote>, events: &[OddsEvent]) -> (Vec<Opportunity>, Vec<Unmatched>) {
    let mut groups: BTreeMap<String, Vec<MarketQuote>> = BTreeMap::new();
    for q in quotes {
        let key = q.event_id.clone().unwrap_or_else(|| q.market_id.clone());
        groups.entry(key).or_default().push(q);
    }

    let (mut found, mut unmatched) = (Vec::new(), Vec::new());
    for (event_id, markets) in groups {
        let outcomes: Vec<String> = markets.iter().filter_map(|m| m.outcome.clone()).collect();
        let miss = |reason: &str| Unmatched {
            event_id: event_id.clone(),
            outcomes: outcomes.clone(),
            reason: reason.into(),
        };
        let (Some(time), [l0, l1]) = (markets[0].event_time, outcomes.as_slice()) else {
            unmatched.push(miss("not a two-team market with an event time"));
            continue;
        };
        let candidates: Vec<(&OddsEvent, [&str; 2])> = events
            .iter()
            .filter(|e| (e.commence_time - time).abs() <= Duration::hours(MAX_TIME_GAP_HOURS))
            .filter_map(|e| assign([l0, l1], e).map(|teams| (e, teams)))
            .collect();
        let (event, teams) = match candidates.as_slice() {
            [one] => *one,
            [] => {
                unmatched.push(miss("no sportsbook game with these teams near this time"));
                continue;
            }
            _ => {
                unmatched.push(miss("matches more than one sportsbook game"));
                continue;
            }
        };

        for (market, team) in markets.iter().zip(teams) {
            let Some(fair) = event.consensus.iter().find(|c| c.name == team) else {
                continue;
            };
            let Some((side, price, edge)) = best_edge(fair.probability, market, None) else {
                continue;
            };
            let side_p = match side {
                Side::Yes => fair.probability,
                Side::No => 1.0 - fair.probability,
            };
            let est_fee = kalshi_fee(price);
            found.push(Opportunity {
                market_id: market.market_id.clone(),
                outcome: market.outcome.clone().unwrap_or_default(),
                sportsbook_team: team.to_string(),
                sportsbook_event_id: event.id.clone(),
                game: format!("{} @ {}", event.away_team, event.home_team),
                commence_time: event.commence_time,
                in_play: event.commence_time <= Utc::now(),
                books: event.bookmakers.len(),
                fair_probability: fair.probability,
                side,
                price,
                edge: edge.edge,
                est_fee,
                net_edge: edge.edge - est_fee,
                net_roi: side_p / (price + est_fee) - 1.0,
                quote: market.clone(),
            });
        }
    }
    found.sort_by(|a, b| b.net_edge.total_cmp(&a.net_edge));
    (found, unmatched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_kalshi_style_labels() {
        assert!(label_matches("Chicago WS", "Chicago White Sox"));
        assert!(!label_matches("Chicago WS", "Chicago Cubs"));
        assert!(label_matches("Chicago C", "Chicago Cubs"));
        assert!(label_matches("Cleveland", "Cleveland Guardians"));
        assert!(label_matches("San Diego", "San Diego Padres"));
        assert!(label_matches("SD", "San Diego Padres"));
        assert!(label_matches("NY Yankees", "New York Yankees"));
        assert!(label_matches("New York Y", "New York Yankees"));
        assert!(!label_matches("New York Y", "New York Mets"));
        assert!(label_matches("LA Dodgers", "Los Angeles Dodgers"));
        assert!(label_matches("St. Louis", "St. Louis Cardinals"));
        assert!(!label_matches("Milwaukee", "San Diego Padres"));
        assert!(!label_matches("", "San Diego Padres"));
    }

    fn quote(id: &str, event: &str, outcome: &str, bid: f64, ask: f64, time: &str) -> MarketQuote {
        MarketQuote {
            venue: "kalshi".into(),
            market_id: id.into(),
            title: format!("{outcome} wins"),
            yes_bid: Some(bid),
            yes_ask: Some(ask),
            last: None,
            as_of: Utc::now(),
            event_id: Some(event.into()),
            outcome: Some(outcome.into()),
            event_time: Some(time.parse().unwrap()),
            taker_fee: None,
        }
    }

    fn event(home: &str, away: &str, time: &str, p_home: f64) -> OddsEvent {
        OddsEvent {
            id: format!("{away}@{home}"),
            commence_time: time.parse().unwrap(),
            home_team: home.into(),
            away_team: away.into(),
            bookmakers: vec![serde_json::Value::Null; 5],
            consensus: vec![
                ConsensusPrice { name: home.into(), probability: p_home },
                ConsensusPrice { name: away.into(), probability: 1.0 - p_home },
            ],
        }
    }

    #[test]
    fn pairs_markets_with_the_right_game_and_team() {
        let quotes = vec![
            quote("K-CWS", "K", "Chicago WS", 0.40, 0.41, "2026-10-09T00:00:00Z"),
            quote("K-CLE", "K", "Cleveland", 0.58, 0.59, "2026-10-09T00:00:00Z"),
        ];
        let events = vec![
            event("Chicago White Sox", "Cleveland Guardians", "2026-10-09T00:00:00Z", 0.48),
            event("Chicago Cubs", "Cleveland Guardians", "2026-10-12T00:00:00Z", 0.5),
        ];
        let (found, unmatched) = scan(quotes, &events);
        assert!(unmatched.is_empty());
        assert_eq!(found.len(), 2);
        // White Sox YES at 0.41 vs fair 0.48 is the best edge.
        let top = &found[0];
        assert_eq!((top.market_id.as_str(), top.side), ("K-CWS", Side::Yes));
        assert_eq!(top.sportsbook_team, "Chicago White Sox");
        assert!((top.edge - 0.07).abs() < 1e-9);
        assert!((top.net_edge - (0.07 - kalshi_fee(0.41))).abs() < 1e-9);
    }

    #[test]
    fn reports_unmatched_and_ambiguous_markets() {
        let quotes = vec![
            quote("A-1", "A", "Milwaukee", 0.5, 0.51, "2026-10-08T00:00:00Z"),
            quote("A-2", "A", "San Diego", 0.48, 0.49, "2026-10-08T00:00:00Z"),
            quote("B-1", "B", "Chicago WS", 0.5, 0.51, "2026-10-20T00:00:00Z"),
            quote("B-2", "B", "Cleveland", 0.48, 0.49, "2026-10-20T00:00:00Z"),
        ];
        let events = vec![
            event("San Diego Padres", "Milwaukee Brewers", "2026-10-08T02:10:00Z", 0.4),
            event("San Diego Padres", "Milwaukee Brewers", "2026-10-08T06:10:00Z", 0.4),
            event("Chicago White Sox", "Cleveland Guardians", "2026-10-09T00:00:00Z", 0.5),
        ];
        let (found, unmatched) = scan(quotes, &events);
        assert!(found.is_empty());
        let reasons: Vec<&str> = unmatched.iter().map(|u| u.reason.as_str()).collect();
        assert_eq!(
            reasons,
            vec!["matches more than one sportsbook game", "no sportsbook game with these teams near this time"]
        );
    }
}
