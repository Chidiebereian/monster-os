//! Prediction-market arbitrage: the same futures outcome (e.g. "Buffalo wins the 2027 AFC
//! Championship") on Kalshi and Polymarket, where YES on one venue plus NO on the other
//! costs less than the $1 the pair pays out, after each venue's taker fee.
//!
//! GET  /pm-arb            the latest scan (every PM_ARB_INTERVAL_SECS, default 180)
//! POST /pm-arb/refresh    scan now (at most every 30 s)
//!
//! Matching is structural, never by title similarity alone:
//!  1. each venue event maps to a family (league, competition, scope): Kalshi by series
//!     ticker (a fixed table), Polymarket by an exact title pattern
//!     ("Pro Football: 2027 AFC Champion"); anything else is not matched;
//!  2. same season (Kalshi "-27" = Polymarket "2027"; a Polymarket title without a year
//!     uses its end date's year);
//!  3. the outcome resolves to the same team entity (projection-service
//!     /entities/resolve; Kalshi's ticker code settles an ambiguous city);
//!  4. the two resolution dates are within MAX_DATE_GAP_DAYS.
//! The rules of both venues are returned verbatim: a match is research until the user has
//! read both. Nothing here places orders.
//!
//! Prices: YES costs the YES ask; NO costs 1 - YES bid (on both venues a NO buy fills
//! against a YES bid). Depth walks both order books level by level while the marginal
//! pair (prices + fees) still costs under $1.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use actix_web::{web, HttpResponse};
use chrono::{DateTime, Datelike, Utc};
use common::{FeeSchedule, VenueMarket};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Kalshi series -> (league, competition, scope).
pub const KALSHI_FAMILIES: [(&str, &str, &str, &str); 19] = [
    ("KXSB", "nfl", "champion", ""),
    ("KXNFLAFCCHAMP", "nfl", "conference", "afc"),
    ("KXNFLNFCCHAMP", "nfl", "conference", "nfc"),
    ("KXNFLAFCEAST", "nfl", "division", "afc east"),
    ("KXNFLAFCNORTH", "nfl", "division", "afc north"),
    ("KXNFLAFCSOUTH", "nfl", "division", "afc south"),
    ("KXNFLAFCWEST", "nfl", "division", "afc west"),
    ("KXNFLNFCEAST", "nfl", "division", "nfc east"),
    ("KXNFLNFCNORTH", "nfl", "division", "nfc north"),
    ("KXNFLNFCSOUTH", "nfl", "division", "nfc south"),
    ("KXNFLNFCWEST", "nfl", "division", "nfc west"),
    ("KXNBA", "nba", "champion", ""),
    ("KXNBAEAST", "nba", "conference", "east"),
    ("KXNBAWEST", "nba", "conference", "west"),
    ("KXNHL", "nhl", "champion", ""),
    ("KXNHLEAST", "nhl", "conference", "east"),
    ("KXNHLWEST", "nhl", "conference", "west"),
    ("KXMLB", "mlb", "champion", ""),
    ("KXMLBWS", "mlb", "champion", ""),
];
const POLYMARKET_TAGS: [&str; 4] = ["nfl", "nba", "nhl", "mlb"];
/// Two venues may date the same outcome differently (Kalshi: expected expiration;
/// Polymarket: a deadline weeks later), but not by a season.
pub const MAX_DATE_GAP_DAYS: i64 = 120;
const MIN_REFRESH: Duration = Duration::from_secs(30);
/// Pairs closest to an arb, shown when there is none.
const CLOSEST_N: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct Family {
    pub league: String,
    pub kind: String,
    pub scope: String,
}

impl Family {
    fn new(league: &str, kind: &str, scope: &str) -> Family {
        Family { league: league.into(), kind: kind.into(), scope: scope.into() }
    }
    pub fn label(&self, season: i32) -> String {
        let l = self.league.to_uppercase();
        match self.kind.as_str() {
            "champion" => format!("{l} {season} champion"),
            "conference" if self.league == "nfl" => format!("{season} {} champion", self.scope.to_uppercase()),
            "conference" => format!("{l} {season} {} conference champion", title_case(&self.scope)),
            _ => format!("{l} {season} {} division winner", self.scope.to_uppercase()),
        }
    }
}

fn title_case(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// Kalshi event ticker "KXNFLAFCCHAMP-27" -> its family.
pub fn kalshi_family(event_id: &str) -> Option<Family> {
    let series = event_id.rsplit_once('-').map(|x| x.0).unwrap_or(event_id);
    KALSHI_FAMILIES.iter().find(|f| f.0 == series).map(|f| Family::new(f.1, f.2, f.3))
}

/// Polymarket event title -> its family, only for these exact shapes:
/// "<League>: [YYYY ]Champion", "Pro Football: [YYYY ]AFC Champion",
/// "NBA: [YYYY ]Eastern Conference Champion", "Pro Football: AFC East Champion".
pub fn polymarket_family(title: &str) -> Option<Family> {
    let t = title.trim().to_lowercase();
    let (league, rest) = t.split_once(':')?;
    let league = match league.trim() {
        "pro football" | "nfl" => "nfl",
        "nba" | "pro basketball" => "nba",
        "nhl" | "pro hockey" => "nhl",
        "mlb" | "pro baseball" => "mlb",
        _ => return None,
    };
    let mut words: Vec<&str> = rest.split_whitespace().collect();
    if words.first().is_some_and(|w| w.len() == 4 && w.chars().all(|c| c.is_ascii_digit())) {
        words.remove(0);
    }
    match (league, words.as_slice()) {
        (_, ["champion"]) => Some(Family::new(league, "champion", "")),
        ("nfl", [c @ ("afc" | "nfc"), "champion"]) => Some(Family::new(league, "conference", c)),
        ("nfl", [c @ ("afc" | "nfc"), d @ ("east" | "north" | "south" | "west"), "champion"]) => {
            Some(Family::new(league, "division", &format!("{c} {d}")))
        }
        ("nba" | "nhl", [s @ ("eastern" | "western"), "conference", "champion"]) => {
            Some(Family::new(league, "conference", &s[..4]))
        }
        _ => None,
    }
}

/// A venue's season for the market: stated, or the year its outcome is known.
fn season_of(m: &VenueMarket) -> Option<i32> {
    m.season.or_else(|| m.resolves_at.map(|t| t.year()))
}

// ---------- prices, fees and depth ----------

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
pub struct Level {
    pub price: f64,
    pub size: f64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Book {
    #[serde(default)]
    pub bids: Vec<Level>,
    #[serde(default)]
    pub asks: Vec<Level>,
}

/// Ask ladder (cheapest first) for buying one side of a market from its YES book.
pub fn ask_ladder(book: &Book, yes: bool) -> Vec<Level> {
    let mut l: Vec<Level> = if yes {
        book.asks.clone()
    } else {
        book.bids.iter().map(|b| Level { price: 1.0 - b.price, size: b.size }).collect()
    };
    l.retain(|x| x.price > 0.0 && x.price < 1.0 && x.size > 0.0);
    l.sort_by(|a, b| a.price.total_cmp(&b.price));
    l
}

fn fee(f: Option<FeeSchedule>, price: f64) -> f64 {
    f.map(|f| f.per_contract(price)).unwrap_or(0.0)
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Step {
    pub contracts: f64,
    /// price of each leg (legs[0], legs[1]) at this step
    pub price_a: f64,
    pub price_b: f64,
    /// both prices plus both fees, per contract
    pub unit_cost: f64,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Depth {
    pub contracts: f64,
    pub cost: f64,
    pub fees: f64,
    pub profit: f64,
    pub steps: Vec<Step>,
}

/// Walk two ask ladders together while a matched contract pair still costs under $1
/// including fees. Each step is a block of contracts at one marginal unit cost.
pub fn walk(a: &[Level], fa: Option<FeeSchedule>, b: &[Level], fb: Option<FeeSchedule>) -> Depth {
    let (mut i, mut j) = (0, 0);
    let (mut ra, mut rb) = (a.first().map(|x| x.size).unwrap_or(0.0), b.first().map(|x| x.size).unwrap_or(0.0));
    let mut d = Depth::default();
    while i < a.len() && j < b.len() {
        let (pa, pb) = (a[i].price, b[j].price);
        let fees = fee(fa, pa) + fee(fb, pb);
        let unit = pa + pb + fees;
        if unit >= 1.0 {
            break;
        }
        let q = ra.min(rb);
        if q > 0.0 {
            d.contracts += q;
            d.cost += q * (pa + pb);
            d.fees += q * fees;
            match d.steps.last_mut() {
                Some(s) if s.price_a == pa && s.price_b == pb => s.contracts += q,
                _ => d.steps.push(Step { contracts: q, price_a: pa, price_b: pb, unit_cost: unit }),
            }
        }
        ra -= q;
        rb -= q;
        if ra <= 1e-9 {
            i += 1;
            ra = a.get(i).map(|x| x.size).unwrap_or(0.0);
        }
        if rb <= 1e-9 {
            j += 1;
            rb = b.get(j).map(|x| x.size).unwrap_or(0.0);
        }
    }
    d.profit = d.contracts - d.cost - d.fees;
    d
}

// ---------- matching ----------

#[derive(Clone, Debug, Serialize)]
pub struct Leg {
    pub venue: String,
    /// "yes" or "no"
    pub side: String,
    /// best ask for this side
    pub price: Option<f64>,
    pub fee_per_contract: Option<f64>,
    pub fee_known: bool,
    pub market_id: String,
    pub event_title: String,
    pub outcome: String,
    pub rules: String,
    pub url: String,
    pub volume: Option<f64>,
    pub resolves_at: Option<DateTime<Utc>>,
    /// contracts and dollars resting at the best ask
    pub top_size: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Opportunity {
    pub id: String,
    pub league: String,
    pub family: String,
    pub team: String,
    pub entity_id: String,
    pub legs: [Leg; 2],
    /// best prices + fees for one matched contract pair
    pub unit_cost: f64,
    /// profit per pair at the best prices, after fees
    pub edge: f64,
    /// edge / unit_cost
    pub roi: f64,
    pub depth: Depth,
    pub resolves_at: Option<DateTime<Utc>>,
    pub days_to_resolve: Option<i64>,
    pub first_seen: DateTime<Utc>,
    pub checks: Vec<Check>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Near {
    pub family: String,
    pub team: String,
    pub kalshi: String,
    pub polymarket: String,
    /// cheaper of the two directions, best prices + fees
    pub unit_cost: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    pub scanned_at: Option<DateTime<Utc>>,
    pub seconds: f64,
    pub markets: BTreeMap<String, usize>,
    pub pairs: usize,
    pub unmatched: BTreeMap<String, usize>,
    /// outcomes that could not be tied to one team (shown so gaps are visible)
    pub unresolved: Vec<String>,
    pub opportunities: Vec<Opportunity>,
    pub closest: Vec<Near>,
    pub errors: Vec<String>,
}

/// The cheaper direction of a pair at the best prices: (k_yes, unit cost incl. fees).
/// `None` when either side has no resting ask.
pub fn best_direction(k: &VenueMarket, p: &VenueMarket) -> Option<(bool, f64)> {
    let side = |k_yes: bool| -> Option<f64> {
        let kp = if k_yes { k.yes_ask? } else { 1.0 - k.yes_bid? };
        let pp = if k_yes { 1.0 - p.yes_bid? } else { p.yes_ask? };
        Some(kp + pp + fee(k.taker_fee, kp) + fee(p.taker_fee, pp))
    };
    [true, false].into_iter().filter_map(|y| side(y).map(|c| (y, c))).min_by(|a, b| a.1.total_cmp(&b.1))
}

// ---------- state, scan and HTTP ----------

pub struct ArbState {
    pub snapshot: Mutex<Snapshot>,
    first_seen: Mutex<HashMap<String, DateTime<Utc>>>,
    last_run: Mutex<Option<Instant>>,
    teams: Mutex<HashMap<(String, String, Option<String>), Option<(String, String)>>>,
}

impl ArbState {
    pub fn new() -> ArbState {
        ArbState { snapshot: Mutex::new(Snapshot::default()), first_seen: Mutex::new(HashMap::new()),
                   last_run: Mutex::new(None), teams: Mutex::new(HashMap::new()) }
    }
}

pub struct Urls {
    pub http: reqwest::Client,
    pub markets: String,
    pub projection: String,
}

async fn get(u: &Urls, url: &str) -> Result<Value, String> {
    let r = u.http.get(url).send().await.map_err(|e| format!("{url}: {e}"))?;
    let s = r.status();
    let v: Value = r.json().await.map_err(|e| format!("{url}: {e}"))?;
    if !s.is_success() {
        return Err(format!("{url}: {s}: {}", v["error"].as_str().unwrap_or("")));
    }
    Ok(v)
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{b:02X}"),
    }).collect()
}

/// Kalshi ticker codes that differ from ours (entity ids are tm:<league>:<code>).
pub const KALSHI_CODES: [(&str, &str, &str); 6] = [
    ("nfl", "JAC", "JAX"), ("nfl", "LAR", "LA"),
    ("nhl", "LA", "LAK"), ("nhl", "NJ", "NJD"), ("nhl", "SJ", "SJS"), ("nhl", "TB", "TBL"),
];

/// Our entity id for a Kalshi team code.
pub fn kalshi_entity(league: &str, code: &str) -> String {
    let code = code.to_uppercase();
    let ours = KALSHI_CODES.iter().find(|k| k.0 == league && k.1 == code).map(|k| k.2.to_string()).unwrap_or(code);
    format!("tm:{league}:{ours}")
}

/// Lowercase letters and digits only.
pub fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase()
}

/// (entity_id, name) for a venue outcome, or None. Strict, because a wrong team makes
/// a fake arb: a Kalshi outcome is its ticker code (never the city label: "Los Angeles C"
/// once resolved to the Lakers), and a Polymarket outcome must equal one of the team's
/// names exactly (no prefix or fuzzy match).
async fn team(u: &Urls, st: &ArbState, league: &str, label: &str, code: Option<&str>) -> Option<(String, String)> {
    let key = (league.to_string(), label.to_string(), code.map(str::to_string));
    if let Some(v) = st.teams.lock().unwrap().get(&key) {
        return v.clone();
    }
    let want = code.map(|c| kalshi_entity(league, c));
    let q = match &want { Some(id) => id.rsplit(':').next().unwrap_or("").to_string(), None => label.to_string() };
    let v = get(u, &format!("{}/entities/resolve?q={}&sport={league}", u.projection, enc(&q))).await.ok();
    let out = v.and_then(|v| {
        let cands: Vec<&Value> = std::iter::once(&v["entity"]).chain(v["candidates"].as_array().into_iter().flatten())
            .filter(|e| e["kind"] == "team").collect();
        let hit = match &want {
            Some(id) => cands.into_iter().find(|e| e["entity_id"].as_str() == Some(id.as_str())),
            None => (v["status"] == "resolved").then(|| &v["entity"])
                .filter(|e| e["kind"] == "team" && e["matched"].as_str().map(norm) == Some(norm(label))),
        }?;
        Some((hit["entity_id"].as_str()?.to_string(), hit["name"].as_str()?.to_string()))
    });
    st.teams.lock().unwrap().insert(key, out.clone());
    out
}

async fn book(u: &Urls, venue: &str, id: &str) -> Result<Book, String> {
    let v = get(u, &format!("{}/orderbook/{venue}/{}", u.markets, enc(id))).await?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}

fn leg(m: &VenueMarket, yes: bool, ladder: &[Level]) -> Leg {
    let price = ladder.first().map(|l| l.price);
    Leg {
        venue: m.venue.clone(),
        side: if yes { "yes" } else { "no" }.into(),
        price,
        fee_per_contract: m.taker_fee.zip(price).map(|(f, p)| f.per_contract(p)),
        fee_known: m.taker_fee.is_some(),
        market_id: m.market_id.clone(),
        event_title: m.event_title.clone(),
        outcome: m.outcome.clone(),
        rules: m.rules.clone(),
        url: m.url.clone(),
        volume: m.volume,
        resolves_at: m.resolves_at,
        top_size: ladder.first().map(|l| l.size),
    }
}

pub async fn scan(u: &Urls, st: &ArbState) -> Snapshot {
    let t0 = Instant::now();
    let mut snap = Snapshot::default();
    let mut all: Vec<VenueMarket> = Vec::new();
    let series = KALSHI_FAMILIES.iter().map(|f| f.0).collect::<Vec<_>>().join(",");
    let mut urls = vec![format!("{}/futures/kalshi?series={series}", u.markets)];
    // every family title says "Champion"; game markets and props are left at the venue
    urls.extend(POLYMARKET_TAGS.iter().map(|t| format!("{}/futures/polymarket?tag={t}&contains=champion", u.markets)));
    for url in urls {
        match get(u, &url).await {
            Ok(v) => {
                snap.errors.extend(v["errors"].as_array().into_iter().flatten().filter_map(|e| e.as_str().map(str::to_string)));
                all.extend(v["markets"].as_array().into_iter().flatten().filter_map(|m| serde_json::from_value::<VenueMarket>(m.clone()).ok()));
            }
            Err(e) => snap.errors.push(e),
        }
    }
    // (family, season, entity) -> one market per venue
    let mut groups: BTreeMap<(Family, i32, String), (Option<VenueMarket>, Option<VenueMarket>, String)> = BTreeMap::new();
    let mut seen_poly = std::collections::HashSet::new();
    for m in all {
        *snap.markets.entry(m.venue.clone()).or_default() += 1;
        let fam = if m.venue == "kalshi" { kalshi_family(&m.event_id) } else { polymarket_family(&m.event_title) };
        let Some(fam) = fam else { continue };
        // Polymarket lists an event under several tags
        if m.venue == "polymarket" && !seen_poly.insert(m.market_id.clone()) {
            continue;
        }
        let Some(season) = season_of(&m) else { *snap.unmatched.entry("no season".into()).or_default() += 1; continue };
        let Some((id, name)) = team(u, st, &fam.league, &m.outcome, m.code.as_deref()).await else {
            *snap.unmatched.entry("outcome not one team".into()).or_default() += 1;
            if snap.unresolved.len() < 40 {
                snap.unresolved.push(format!("{} {}: {}", m.venue, fam.league, m.outcome));
            }
            continue;
        };
        let g = groups.entry((fam, season, id)).or_insert((None, None, name));
        let slot = if m.venue == "kalshi" { &mut g.0 } else { &mut g.1 };
        if slot.is_some() {
            *snap.unmatched.entry("duplicate on one venue".into()).or_default() += 1;
        } else {
            *slot = Some(m);
        }
    }
    let now = Utc::now();
    let mut keep_ids = Vec::new();
    for ((fam, season, id), (k, p, name)) in groups {
        let (Some(k), Some(p)) = (k, p) else {
            *snap.unmatched.entry("on one venue only".into()).or_default() += 1;
            continue;
        };
        let gap = match (k.resolves_at, p.resolves_at) {
            (Some(a), Some(b)) => Some((a - b).num_days().abs()),
            _ => None,
        };
        if gap.map(|g| g > MAX_DATE_GAP_DAYS).unwrap_or(false) {
            *snap.unmatched.entry("resolution dates too far apart".into()).or_default() += 1;
            continue;
        }
        snap.pairs += 1;
        let label = fam.label(season);
        let Some((k_yes, top_cost)) = best_direction(&k, &p) else { continue };
        if top_cost >= 1.0 {
            snap.closest.push(Near { family: label, team: name, kalshi: k.market_id.clone(), polymarket: p.market_id.clone(), unit_cost: top_cost });
            continue;
        }
        // candidate: price it on the real books
        let (kb, pb) = futures_util::future::join(book(u, "kalshi", &k.market_id), book(u, "polymarket", &p.market_id)).await;
        let (kb, pb) = match (kb, pb) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => { snap.errors.push(e); continue }
        };
        let (la, lb) = (ask_ladder(&kb, k_yes), ask_ladder(&pb, !k_yes));
        let depth = walk(&la, k.taker_fee, &lb, p.taker_fee);
        let legs = [leg(&k, k_yes, &la), leg(&p, !k_yes, &lb)];
        let unit = match (legs[0].price, legs[1].price) {
            (Some(a), Some(b)) => a + b + legs[0].fee_per_contract.unwrap_or(0.0) + legs[1].fee_per_contract.unwrap_or(0.0),
            _ => continue,
        };
        if unit >= 1.0 || depth.contracts <= 0.0 {
            snap.closest.push(Near { family: label, team: name, kalshi: k.market_id.clone(), polymarket: p.market_id.clone(), unit_cost: unit });
            continue;
        }
        let oid = format!("{}|{}|{}", k.market_id, p.market_id, if k_yes { "kyes" } else { "kno" });
        let first = *st.first_seen.lock().unwrap().entry(oid.clone()).or_insert(now);
        keep_ids.push(oid.clone());
        let resolves = [k.resolves_at, p.resolves_at].into_iter().flatten().max();
        let checks = vec![
            Check { name: "same competition and season".into(), ok: true, detail: format!("{} / {}", k.event_title, p.event_title) },
            Check { name: "same team".into(), ok: true, detail: format!("{} = {} ({id})", k.outcome, p.outcome) },
            Check { name: "resolution dates close".into(), ok: gap.map(|g| g <= 31).unwrap_or(false),
                    detail: gap.map(|g| format!("{g} days apart")).unwrap_or_else(|| "a venue gives no date".into()) },
            Check { name: "both fees known".into(), ok: legs.iter().all(|l| l.fee_known),
                    detail: legs.iter().map(|l| format!("{}: {}", l.venue, if l.fee_known { "known" } else { "unknown (not counted)" })).collect::<Vec<_>>().join(", ") },
        ];
        snap.opportunities.push(Opportunity {
            id: oid, league: fam.league.clone(), family: label, team: name, entity_id: id,
            edge: 1.0 - unit, roi: (1.0 - unit) / unit, unit_cost: unit, depth,
            days_to_resolve: resolves.map(|r| (r - now).num_days()), resolves_at: resolves, first_seen: first, legs, checks,
        });
    }
    st.first_seen.lock().unwrap().retain(|k, _| keep_ids.contains(k));
    snap.opportunities.sort_by(|a, b| b.roi.total_cmp(&a.roi));
    snap.closest.sort_by(|a, b| a.unit_cost.total_cmp(&b.unit_cost));
    snap.closest.truncate(CLOSEST_N);
    snap.scanned_at = Some(now);
    snap.seconds = (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    snap
}

pub async fn run(u: &Urls, st: &ArbState) -> bool {
    {
        let mut last = st.last_run.lock().unwrap();
        if last.map(|t| t.elapsed() < MIN_REFRESH).unwrap_or(false) {
            return false;
        }
        *last = Some(Instant::now());
    }
    let snap = scan(u, st).await;
    *st.snapshot.lock().unwrap() = snap;
    true
}

pub async fn get_http(st: web::Data<ArbState>) -> HttpResponse {
    let s = st.snapshot.lock().unwrap().clone();
    if s.scanned_at.is_none() {
        return HttpResponse::Accepted().json(json!({ "status": "the first scan is running", "opportunities": [] }));
    }
    HttpResponse::Ok().json(s)
}

pub async fn refresh_http(u: web::Data<Urls>, st: web::Data<ArbState>) -> HttpResponse {
    if !run(&u, &st).await {
        return HttpResponse::TooManyRequests().json(json!({ "error": "scanned under 30 s ago" }));
    }
    get_http(st).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_from_both_venues() {
        assert_eq!(kalshi_family("KXNFLAFCCHAMP-27"), Some(Family::new("nfl", "conference", "afc")));
        assert_eq!(kalshi_family("KXNFLMVP-27"), None);
        assert_eq!(polymarket_family("Pro Football: 2027 AFC Champion "), Some(Family::new("nfl", "conference", "afc")));
        assert_eq!(polymarket_family("Pro Football: 2027 Champion"), Some(Family::new("nfl", "champion", "")));
        assert_eq!(polymarket_family("Pro Football: AFC East Champion"), Some(Family::new("nfl", "division", "afc east")));
        assert_eq!(polymarket_family("NBA: 2027 Eastern Conference Champion"), Some(Family::new("nba", "conference", "east")));
        assert_eq!(polymarket_family("NHL: 2027 Champion"), kalshi_family("KXNHL-27"));
        // look-alikes are not matched
        assert_eq!(polymarket_family("NBA: 2026-27 Rookie of the Year"), None);
        assert_eq!(polymarket_family("NHL Playoffs: Team to advance to Eastern Conference Finals"), None);
        assert_eq!(polymarket_family("Pro Football: 2027 AFC Champion runner-up"), None);
        assert_eq!(Family::new("nfl", "conference", "afc").label(2027), "2027 AFC champion");
    }

    fn vm(venue: &str, bid: Option<f64>, ask: Option<f64>, fee: Option<FeeSchedule>) -> VenueMarket {
        VenueMarket { venue: venue.into(), market_id: "m".into(), event_id: "e".into(), event_title: "t".into(), outcome: "o".into(),
                      code: None, season: Some(2027), yes_bid: bid, yes_ask: ask, resolves_at: None, rules: String::new(),
                      url: String::new(), volume: None, taker_fee: fee }
    }

    #[test]
    fn kalshi_codes_map_to_our_teams() {
        assert_eq!(kalshi_entity("nfl", "LAR"), "tm:nfl:LA");
        assert_eq!(kalshi_entity("nfl", "LAC"), "tm:nfl:LAC");
        assert_eq!(kalshi_entity("nhl", "tb"), "tm:nhl:TBL");
        assert_eq!(kalshi_entity("nba", "LAC"), "tm:nba:LAC");
        assert_eq!(norm("Los Angeles Clippers"), "losangelesclippers");
    }

    #[test]
    fn picks_the_cheaper_direction_after_fees() {
        let k = vm("kalshi", Some(0.30), Some(0.32), None);
        let p = vm("polymarket", Some(0.36), Some(0.38), None);
        // YES on Kalshi 0.32 + NO on Polymarket 1-0.36 = 0.96
        let (k_yes, cost) = best_direction(&k, &p).unwrap();
        assert!(k_yes && (cost - 0.96).abs() < 1e-9);
        let fee = Some(FeeSchedule { rate: 0.07, exponent: 1.0 });
        let (_, with_fee) = best_direction(&vm("kalshi", Some(0.30), Some(0.32), fee), &p).unwrap();
        assert!((with_fee - (0.96 + 0.07 * 0.32 * 0.68)).abs() < 1e-9);
        assert_eq!(best_direction(&vm("kalshi", None, None, None), &p), None);
    }

    #[test]
    fn no_ladder_mirrors_yes_bids() {
        let b = Book { bids: vec![Level { price: 0.36, size: 50.0 }, Level { price: 0.35, size: 10.0 }], asks: vec![Level { price: 0.38, size: 5.0 }] };
        assert_eq!(ask_ladder(&b, false), vec![Level { price: 0.64, size: 50.0 }, Level { price: 0.65, size: 10.0 }]);
        assert_eq!(ask_ladder(&b, true), vec![Level { price: 0.38, size: 5.0 }]);
    }

    #[test]
    fn depth_stops_when_the_pair_costs_a_dollar() {
        let a = vec![Level { price: 0.30, size: 100.0 }, Level { price: 0.33, size: 100.0 }, Level { price: 0.40, size: 100.0 }];
        let b = vec![Level { price: 0.64, size: 150.0 }, Level { price: 0.66, size: 500.0 }];
        let d = walk(&a, None, &b, None);
        // 100 @ 0.94, 50 @ 0.97, 50 @ 0.99, then 0.40+0.66 >= 1
        assert_eq!(d.contracts, 200.0);
        assert_eq!(d.steps.len(), 3);
        assert!((d.cost - (100.0 * 0.94 + 50.0 * 0.97 + 50.0 * 0.99)).abs() < 1e-9);
        assert!((d.profit - (200.0 - d.cost)).abs() < 1e-9);
        assert_eq!(walk(&a, None, &[], None).contracts, 0.0);
        // fees can close the gap
        let f = Some(FeeSchedule { rate: 0.07, exponent: 1.0 });
        assert!(walk(&[Level { price: 0.5, size: 10.0 }], f, &[Level { price: 0.49, size: 10.0 }], f).contracts == 0.0);
    }
}
