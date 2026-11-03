//! AI Picks: model-backed plays on player props and game lines.
//!
//! A pick is a two-way market line (lines.rs) priced by the champion engine's
//! frozen forecast for that game: the simulators' per-player and per-game
//! distributions in the prediction ledger, or the team model's score
//! distribution. Nothing is re-simulated here, so every pick traces back to an
//! immutable ledger entry.
//!
//!   edge  = P(model, side) - P(market, side, de-vigged at the same book)
//!   EV    = P(win) x (decimal - 1) - P(loss)          (pushes return the stake)
//!   P(model) = the engine's probability blended toward the player's own prior
//!           frequency (props) and the de-vigged market with weights fitted on
//!           backtests and on real closing lines (blend.rs)
//!   grade = from the edge, then CAPPED by the engine's track record on that
//!           market (pick_evidence): proven (beat real lines) uncapped,
//!           baseline (beat a naive model in backtests) B+, unproven C+,
//!           losing C. Unproven and losing picks are hidden by default.
//!
//! The published price, probability, edge and grade are written once; the
//! record is scored on them. Settled picks feed back into pick_evidence, so
//! grades unlock (or lock) from live results without code changes.
//!
//!   stake = fractional Kelly on P(model): x1 proven, x0.5 baseline, 0 otherwise,
//!           and 0 when Pinnacle's own price makes the bet negative (sharp.rs)
//!
//!   POST /picks/run?sport=&force=      lines -> build -> stale lines -> settle
//!   GET  /picks?sport=&kind=&side=&grades=&market=&game=&book=&min_edge=&min_prob=&sort=&q=&include_hidden=&status=
//!   GET  /picks/{id}
//!   GET  /picks/record?sport=&days=
//!   GET  /picks/meta?sport=

use std::collections::{BTreeMap, HashMap, HashSet};

use actix_web::{web, HttpResponse, Responder};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::blend;
use crate::lines;
use crate::segments;
use crate::State;

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}
fn internal(m: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(picks): {m}");
    HttpResponse::InternalServerError().json(json!({ "error": m.to_string() }))
}
fn r1(x: f64) -> f64 { (x * 10.0).round() / 10.0 }
fn r3(x: f64) -> f64 { (x * 1000.0).round() / 1000.0 }
fn pct(x: f64) -> f64 { r1(x * 100.0) }

pub const SPORTS: [&str; 4] = ["nba", "nhl", "nfl", "mlb"];
/// NFL prop markets with a game-log column (player history and hit rates).
pub(crate) const NFL_PROPS: [&str; 15] = ["pass_yds", "rush_yds", "rec_yds", "receptions", "pass_td", "pass_att", "completions", "interceptions",
    "carries", "targets", "rush_td", "rec_td", "rush_rec_yds", "pass_rush_yds", "tds"];
pub const GRADES: [&str; 8] = ["A+", "A", "A-", "B+", "B", "B-", "C+", "C"];

// Public visibility comes from governance::public_filter (publication state x
// tier); picks are still built and settled for every market; internal callers
// pass include_held=true.

// ---------------------------------------------------------------------------
// Distributions and pricing (pure)
// ---------------------------------------------------------------------------

/// An integer distribution: P(X = offset + i) = pmf[i].
#[derive(Clone, Debug)]
pub(crate) struct Dist { pub offset: i64, pub pmf: Vec<f64> }

impl Dist {
    pub(crate) fn from_json(v: &Value) -> Option<Dist> {
        let pmf: Vec<f64> = v["pmf"].as_array()?.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect();
        let s: f64 = pmf.iter().sum();
        if pmf.is_empty() || s <= 0.0 { return None; }
        let offset = v["pmf_offset"].as_i64().or_else(|| v["offset"].as_i64()).unwrap_or(0);
        Some(Dist { offset, pmf: pmf.iter().map(|p| p / s).collect() })
    }
    /// (P(X > line), P(X < line), P(X == line))
    pub(crate) fn split(&self, line: f64) -> (f64, f64, f64) {
        let (mut o, mut u, mut p) = (0.0, 0.0, 0.0);
        for (i, &m) in self.pmf.iter().enumerate() {
            let x = (self.offset + i as i64) as f64;
            if (x - line).abs() < 1e-9 { p += m } else if x > line { o += m } else { u += m }
        }
        (o, u, p)
    }
    pub(crate) fn mean(&self) -> f64 { self.pmf.iter().enumerate().map(|(i, m)| (self.offset + i as i64) as f64 * m).sum() }
    pub(crate) fn quantile(&self, q: f64) -> i64 {
        let mut c = 0.0;
        for (i, m) in self.pmf.iter().enumerate() { c += m; if c >= q { return self.offset + i as i64; } }
        self.offset + self.pmf.len() as i64 - 1
    }
}

/// The engine's view of one side at one line. `win`/`loss`/`push` are unconditional.
#[derive(Clone, Debug)]
pub(crate) struct Priced {
    pub win: f64,
    pub loss: f64,
    pub push: f64,
    pub projection: Option<f64>,
    pub q: Option<(i64, i64, i64)>,
    pub p_active: Option<f64>,
}
impl Priced {
    /// P(win | no push) — comparable with a de-vigged two-way price.
    pub(crate) fn p(&self) -> f64 { self.win / (self.win + self.loss).max(1e-12) }
    fn from_dist(d: &Dist, side_over: bool, line: f64) -> Priced {
        let (o, u, p) = d.split(line);
        let (win, loss) = if side_over { (o, u) } else { (u, o) };
        Priced { win, loss, push: p, projection: Some(d.mean()), q: Some((d.quantile(0.1), d.quantile(0.5), d.quantile(0.9))), p_active: None }
    }
}

/// Find a player's entry (skaters, then goalies) in a simulator payload.
pub(crate) fn player_entry<'a>(payload: &'a Value, subject: &str) -> Option<(&'a Value, String)> {
    for side in ["home", "away"] {
        let team = payload[side].as_str().unwrap_or("").to_string();
        for p in payload["players"][side].as_array().into_iter().flatten() {
            let id = match &p["id"] { Value::String(s) => s.clone(), Value::Number(n) => n.to_string(), _ => continue };
            if id == subject { return Some((p, team)); }
        }
    }
    for g in payload["goalies"].as_array().into_iter().flatten() {
        let id = match &g["id"] { Value::String(s) => s.clone(), Value::Number(n) => n.to_string(), _ => continue };
        if id == subject { return Some((g, g["team"].as_str().unwrap_or("").to_string())); }
    }
    None
}

/// Price one selection from a ledger payload. `team` = team-model payload.
pub(crate) fn price(payload: &Value, team: bool, market: &str, subject: &str, side: &str, line: Option<f64>) -> Option<Priced> {
    match market {
        "moneyline" => {
            let ph = if team {
                payload["forecasts"].as_array()?.iter().find(|f| f["market"] == "home_win")?["p"].as_f64()?
            } else {
                let (h, a) = (payload["win"]["home"].as_f64()?, payload["win"]["away"].as_f64().unwrap_or(0.0));
                if a > 0.0 && h + a < 0.999 { h / (h + a) } else { h } // ties (NFL) refund a moneyline
            };
            let win = if side == "home" { ph } else { 1.0 - ph };
            let margin = if team { payload["margin_home"].as_f64() } else { payload["margin_home"]["mean"].as_f64() };
            Some(Priced { win, loss: 1.0 - win, push: 0.0, projection: margin, q: None, p_active: None })
        }
        "spread" => {
            let l = line?;
            let margin = if team { Dist::from_json(&payload["dist"]["margin"]) } else { Dist::from_json(&payload["margin_home"]) };
            if let Some(d) = margin {
                // home covers when margin + l > 0, i.e. margin > -l; away (line = -home line) when margin < l_away
                let (o, u, p) = d.split(if side == "home" { -l } else { l });
                let (win, loss) = if side == "home" { (o, u) } else { (u, o) };
                return Some(Priced { win, loss, push: p, projection: Some(d.mean()), q: Some((d.quantile(0.1), d.quantile(0.5), d.quantile(0.9))), p_active: None });
            }
            // NHL simulator: puck lines only (+/-1.5)
            let pl = &payload["puckline"];
            if (l.abs() - 1.5).abs() > 1e-9 { return None; }
            let (hm, am) = (pl["home_minus_1_5"].as_f64()?, pl["away_minus_1_5"].as_f64()?);
            let win = match (side, l < 0.0) { ("home", true) => hm, ("home", false) => 1.0 - am, (_, true) => am, (_, false) => 1.0 - hm };
            Some(Priced { win, loss: 1.0 - win, push: 0.0, projection: None, q: None, p_active: None })
        }
        "total" => {
            let d = if team { Dist::from_json(&payload["dist"]["total"]) } else { Dist::from_json(&payload["total"]) }?;
            Some(Priced::from_dist(&d, side == "over", line?))
        }
        // team totals and the first-segment markets: every engine serves the same
        // keys (`score.home|away`, `h1.*`), see segments::Joint
        "team_total_home" | "team_total_away" => {
            let d = Dist::from_json(&payload["score"][if market == "team_total_home" { "home" } else { "away" }])?;
            Some(Priced::from_dist(&d, side == "over", line?))
        }
        "moneyline_1h" => {
            let w = &payload["h1"]["win"];
            let (h, a, t) = (w["home"].as_f64()?, w["away"].as_f64()?, w["tie"].as_f64().unwrap_or(0.0));
            let (win, loss) = if side == "home" { (h, a) } else { (a, h) };
            // a tied first half refunds a two-way moneyline
            Some(Priced { win, loss, push: t, projection: payload["h1"]["margin_home"]["mean"].as_f64(), q: None, p_active: None })
        }
        "spread_1h" => {
            let l = line?;
            let d = Dist::from_json(&payload["h1"]["margin_home"])?;
            let (o, u, p) = d.split(if side == "home" { -l } else { l });
            let (win, loss) = if side == "home" { (o, u) } else { (u, o) };
            Some(Priced { win, loss, push: p, projection: Some(d.mean()), q: Some((d.quantile(0.1), d.quantile(0.5), d.quantile(0.9))), p_active: None })
        }
        "total_1h" | "team_total_home_1h" | "team_total_away_1h" => {
            let k = match market { "total_1h" => "total", "team_total_home_1h" => "home", _ => "away" };
            let d = Dist::from_json(&payload["h1"][k])?;
            Some(Priced::from_dist(&d, side == "over", line?))
        }
        stat => {
            let (p, _) = player_entry(payload, subject)?;
            let d = Dist::from_json(if stat == "saves" && p["saves"].is_object() { &p["saves"] } else { &p["stats"][stat] })?;
            let mut pr = Priced::from_dist(&d, side == "over", line?);
            pr.p_active = p["p_active"].as_f64();
            Some(pr)
        }
    }
}

pub(crate) fn decimal(american: i32) -> f64 { if american > 0 { 1.0 + american as f64 / 100.0 } else { 1.0 + 100.0 / (-american as f64) } }
pub(crate) fn implied(american: i32) -> f64 { 1.0 / decimal(american) }
/// De-vigged probability of side a from a two-way pair (power method).
pub(crate) fn novig(a: i32, b: i32) -> f64 { devig(implied(a), implied(b)) }

/// Fair probability of side x from a two-way pair of implied probabilities, power
/// method: p = x^k with k such that x^k + y^k = 1. Unlike dividing by the sum, it
/// takes more of the margin from the longshot, which books overprice (research R10:
/// sides the proportional method put at 15% won 10.9%; power 11.9%).
pub(crate) fn devig(x: f64, y: f64) -> f64 {
    if !(x > 0.0 && y > 0.0 && x < 1.0 && y < 1.0) { return x / (x + y); }
    let (mut lo, mut hi) = (0.01_f64, 50.0_f64); // x^k + y^k falls as k grows
    for _ in 0..60 {
        let k = (lo + hi) / 2.0;
        if x.powf(k) + y.powf(k) > 1.0 { lo = k; } else { hi = k; }
    }
    x.powf((lo + hi) / 2.0)
}

/// A quote whose no-vig probability is this far from the median of 3+ books at the same line is dropped.
pub(crate) const OUTLIER_GAP: f64 = 0.15;
pub(crate) fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = s.len();
    if n == 0 { 0.0 } else if n % 2 == 1 { s[n / 2] } else { (s[n / 2 - 1] + s[n / 2]) / 2.0 }
}

/// The worst American price at which a bet with these win / loss chances (pushes
/// are the rest) still earns `min_ev` per unit; 0 gives the break-even (fair)
/// price. Rounded toward the safe side.
pub(crate) fn price_for_ev(win: f64, loss: f64, min_ev: f64) -> Option<i32> {
    if win <= 0.0 { return None; }
    let d = 1.0 + (min_ev + loss) / win;
    if !d.is_finite() || d > 101.0 { return None; }
    Some(if d >= 2.0 { ((d - 1.0) * 100.0 - 1e-9).ceil() as i32 } else { -((100.0 / (d - 1.0)) + 1e-9).floor() as i32 })
}

/// "Playable to": the worst price still worth `min_ev`, never better than the
/// published price (a pick that cleared the edge rule but not `min_ev` is
/// playable only at its own price).
pub(crate) fn playable_floor(win: f64, loss: f64, min_ev: f64, published: i32) -> Option<i32> {
    price_for_ev(win, loss, min_ev).map(|p| if at_least(published, p) { p } else { published })
}

/// Is `now` at least as good a price as `floor`?
pub(crate) fn at_least(now: i32, floor: i32) -> bool { decimal(now) >= decimal(floor) - 1e-9 }

/// Grade from the edge alone (before the track-record cap).
pub(crate) fn edge_grade(edge: f64) -> Option<&'static str> {
    const T: [(f64, &str); 8] = [(0.10, "A+"), (0.075, "A"), (0.06, "A-"), (0.05, "B+"), (0.04, "B"), (0.03, "B-"), (0.025, "C+"), (0.015, "C")];
    T.iter().find(|(t, _)| edge >= *t).map(|(_, g)| *g)
}
pub(crate) fn tier_cap(tier: &str) -> &'static str {
    match tier { "proven" => "A+", "baseline" => "B+", "unproven" => "C+", _ => "C" }
}
pub(crate) fn cap(grade: &'static str, tier: &str) -> &'static str {
    let c = tier_cap(tier);
    let (gi, ci) = (GRADES.iter().position(|g| *g == grade).unwrap_or(7), GRADES.iter().position(|g| *g == c).unwrap_or(7));
    GRADES[gi.max(ci)]
}

// ---------------------------------------------------------------------------
// Track record
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub(crate) struct Evidence { pub source: String, pub comparison: String, pub n: i32, pub seasons: i32, pub better: i32, pub worse: i32, pub metric: Option<f64>, pub se: Option<f64>, pub detail: Value }

/// proven | baseline | unproven | losing, with the evidence row that decided it.
pub(crate) fn tier(rows: &[Evidence]) -> (&'static str, Option<Evidence>) {
    let find = |s: &str, c: &str| rows.iter().find(|r| r.source == s && r.comparison == c).cloned();
    if let Some(l) = find("live", "market") {
        if let (Some(m), Some(se)) = (l.metric, l.se) {
            if l.n >= 150 && se > 0.0 {
                if m < -2.0 * se { return ("proven", Some(l)); }
                if m > 2.0 * se { return ("losing", Some(l)); }
            }
        }
    }
    // every line this service priced and saw settled, blended out of sample
    if let Some(l) = find("lines", "market") {
        if let (Some(m), Some(se)) = (l.metric, l.se) {
            if l.n >= 150 && se > 0.0 {
                if m < -2.0 * se { return ("proven", Some(l)); }
                if m > 2.0 * se { return ("losing", Some(l)); }
            }
        }
    }
    if let Some(b) = find("backtest", "market") {
        if b.better >= 2 && b.worse == 0 { return ("proven", Some(b)); }
        if b.worse >= 2 && b.worse > b.better { return ("losing", Some(b)); }
    }
    if let Some(b) = find("backtest", "baseline") {
        if b.worse >= 2 && b.worse > b.better { return ("losing", Some(b)); }
        if b.better >= 2 && b.worse == 0 { return ("baseline", Some(b)); }
    }
    ("unproven", None)
}

/// The tier of an engine on a market. A prop market with no evidence of its own
/// inherits a "losing" tier from the same engine's measured props (PRA cannot be
/// better than the points, rebounds and assists it adds up).
pub(crate) fn prop_tier(ev: &HashMap<(String, String), Vec<Evidence>>, engine: &str, market: &str) -> (&'static str, Option<Evidence>) {
    let own = ev.get(&(engine.to_string(), market.to_string())).map(|v| v.as_slice()).unwrap_or(&[]);
    let t = tier(own);
    if !own.is_empty() || segments::is_game_market(market) { return t; }
    let mut related: Vec<(&String, (&'static str, Option<Evidence>))> = ev.iter()
        .filter(|((e, m), _)| e == engine && !segments::is_game_market(m) && m != "_none")
        .map(|((_, m), rows)| (m, tier(rows))).collect();
    related.sort_by(|a, b| a.0.cmp(b.0));
    if related.iter().any(|(_, (t, _))| *t == "proven" || *t == "baseline") { return t; }
    if let Some((m, (_, Some(mut e)))) = related.into_iter().find(|(_, (t, _))| *t == "losing") {
        e.detail["inherited_from"] = json!(m);
        return ("losing", Some(e));
    }
    t
}

/// Evidence rows per (engine, market) that apply to the engine's CURRENT code version.
pub(crate) async fn evidence(db: &tokio_postgres::Client, engines: &HashMap<String, (String, String)>) -> Result<HashMap<(String, String), Vec<Evidence>>, String> {
    let rows = db.query("SELECT engine, market, source, comparison, n, seasons, better, worse, metric, se, detail FROM pick_evidence", &[]).await.map_err(|e| dberr(&e))?;
    let mut out: HashMap<(String, String), Vec<Evidence>> = HashMap::new();
    for r in &rows {
        let engine: String = r.get(0);
        let detail: Json<Value> = r.get(10);
        let Some((_, code)) = engines.get(&engine) else { continue };
        if detail.0["engine_version"].as_str().map(|v| v != code).unwrap_or(false) { continue; }
        out.entry((engine, r.get(1))).or_default().push(Evidence {
            source: r.get(2), comparison: r.get(3), n: r.get(4), seasons: r.get(5), better: r.get(6), worse: r.get(7), metric: r.get(8), se: r.get(9), detail: detail.0,
        });
    }
    Ok(out)
}

/// engine key -> (registry version, engine code version) of each champion.
pub(crate) async fn champions(db: &tokio_postgres::Client) -> Result<HashMap<String, (String, String)>, String> {
    Ok(db.query("SELECT sport, version, engine FROM model_registry WHERE status='champion'", &[]).await.map_err(|e| dberr(&e))?
        .iter().map(|r| (r.get(0), (r.get(1), r.get(2)))).collect())
}


// ---------------------------------------------------------------------------
// Prop pricer for the Games hub: the same blend and sanity check AI Picks uses
// ---------------------------------------------------------------------------

/// Turns the engine's raw prop probability into the number AI Picks would
/// publish: blended toward the player's own history (naive baseline) and the
/// market by the weights fitted on evidence, with the max-edge sanity check.
pub(crate) struct PropPricer {
    ev: HashMap<(String, String), Vec<Evidence>>,
    default_w: f64,
    pub max_edge: f64,
    hist: HashMap<String, Vec<(HashMap<&'static str, f64>, String)>>,
}

impl PropPricer {
    pub async fn load(db: &tokio_postgres::Client, sport: &str, ids: &[String]) -> Result<Self, String> {
        let champs = champions(db).await?;
        Ok(Self {
            ev: evidence(db, &champs).await?,
            default_w: lines::setting(db, "blend_default_w", 0.2).await,
            max_edge: lines::setting(db, "max_edge", 0.25).await,
            hist: player_history(db, sport, ids).await?,
        })
    }
    fn values(&self, subject: &str, stat: &str) -> Vec<f64> {
        self.hist.get(subject).map(|h| h.iter().filter_map(|(m, _)| m.get(stat).copied()).collect()).unwrap_or_default()
    }
    /// The player's own frequency over the line (needs 5+ games).
    pub fn naive_over(&self, subject: &str, stat: &str, line: f64) -> Option<f64> {
        let v = self.values(subject, stat);
        if v.len() < 5 { return None; }
        Some(blend::freq_over(&v, line))
    }
    /// Mean of the last 10 games (and how many there were).
    pub fn recent(&self, subject: &str, stat: &str) -> Option<(f64, usize)> {
        let v: Vec<f64> = self.values(subject, stat).into_iter().take(10).collect();
        if v.is_empty() { return None; }
        Some((v.iter().sum::<f64>() / v.len() as f64, v.len()))
    }
    /// Final P(side): model, blended toward naive (props) and the market.
    pub fn blend(&self, engine: &str, market: &str, p_raw: f64, p_naive: Option<f64>, p_market: f64) -> f64 {
        let w = blend::weights(self.ev.get(&(engine.to_string(), market.to_string())).map(|v| v.as_slice()).unwrap_or(&[]), self.default_w);
        blend::apply(&w, p_raw, p_naive, p_market)
    }
}

// ---------------------------------------------------------------------------
// Labels
// ---------------------------------------------------------------------------

pub(crate) fn stat_label(market: &str) -> &'static str {
    match market {
        "pts" | "points" => "Points", "reb" => "Rebounds", "ast" | "assists" => "Assists", "fg3m" => "3-Pointers Made", "stl" => "Steals",
        "blk" => "Blocks", "tov" => "Turnovers", "pra" => "Pts + Rebs + Asts", "sog" => "Shots on Goal", "goals" => "Goals",
        "saves" => "Saves", "blocks" => "Blocked Shots", "pass_yds" => "Passing Yards", "rush_yds" => "Rushing Yards",
        "rec_yds" => "Receiving Yards", "receptions" => "Receptions", "moneyline" => "Moneyline", "spread" => "Spread", "total" => "Total",
        "pr" => "Pts + Rebs", "pa" => "Pts + Asts", "ra" => "Rebs + Asts", "stocks" => "Steals + Blocks", "fgm" => "Field Goals Made",
        "fga" => "Field Goal Attempts", "fg3a" => "3-Point Attempts", "ftm" => "Free Throws Made", "fta" => "Free Throw Attempts",
        "oreb" => "Offensive Rebounds", "dreb" => "Defensive Rebounds", "double_double" => "Double-Double", "triple_double" => "Triple-Double",
        "pass_td" => "Passing TDs", "pass_att" => "Pass Attempts", "completions" => "Completions", "interceptions" => "Interceptions",
        "carries" => "Rush Attempts", "targets" => "Targets", "rush_td" => "Rushing TDs", "rec_td" => "Receiving TDs",
        "rush_rec_yds" => "Rush + Rec Yards", "pass_rush_yds" => "Pass + Rush Yards", "tds" => "Touchdowns", "kick_pts" => "Kicking Points",
        _ => "",
    }
}

fn signed(x: f64) -> String { if x > 0.0 { format!("+{}", fmt_num(x)) } else { fmt_num(x) } }
fn fmt2(x: f64) -> String { format!("{:.2}", x) }
fn signed_pct(x: f64) -> String { let p = pct(x); if p > 0.0 { format!("+{p}%") } else { format!("{p}%") } }
fn fmt_num(x: f64) -> String { if (x - x.round()).abs() < 1e-9 { format!("{}", x.round() as i64) } else { format!("{:.1}", x) } }
fn odds_str(p: i32) -> String { if p > 0 { format!("+{p}") } else { p.to_string() } }

pub(crate) fn pick_label(sport: &str, market: &str, side: &str, line: Option<f64>, home: &str, away: &str) -> String {
    let team = if side == "home" { home } else { away };
    let ou = if side == "over" { "Over" } else { "Under" };
    let seg = if market.ends_with("_1h") { format!("{} ", segments::tag(sport)) } else { String::new() };
    match market {
        "moneyline" | "moneyline_1h" => format!("{team} {seg}ML"),
        "spread" | "spread_1h" => format!("{team} {seg}{}", line.map(signed).unwrap_or_default()),
        "total" | "total_1h" => format!("{seg}{ou} {}", line.map(fmt_num).unwrap_or_default()),
        "team_total_home" | "team_total_home_1h" => format!("{home} {seg}{ou} {}", line.map(fmt_num).unwrap_or_default()),
        "team_total_away" | "team_total_away_1h" => format!("{away} {seg}{ou} {}", line.map(fmt_num).unwrap_or_default()),
        // yes/no props (double-double, triple-double, anytime TD) read as yes / no
        m if crate::lines::yes_no_market(m) && line == Some(0.5) => {
            let what = if m == "tds" { "Anytime TD".to_string() } else { stat_label(m).to_string() };
            format!("{what} {}", if side == "over" { "Yes" } else { "No" })
        }
        "tds" if line == Some(1.5) => format!("{} 2+ TDs", if side == "over" { "Yes" } else { "No" }),
        m => format!("{ou} {} {}", line.map(fmt_num).unwrap_or_default(), stat_label(m)),
    }
}

/// The partner of a one-sided (yes-only) quote: the price whose implied
/// probability completes it to 1, so the "no-vig" probability is the yes
/// price's raw implied probability (vig included, i.e. conservative).
pub(crate) fn complement_price(a: i32) -> i32 {
    let p = (1.0 - implied(a)).clamp(0.01, 0.99);
    if p >= 0.5 { (-100.0 * p / (1.0 - p)).round() as i32 } else { (100.0 * (1.0 - p) / p).round() as i32 }
}

fn engine_label(engine: &str) -> &'static str {
    match engine {
        "nba" => "possession simulator", "nhl" => "event simulator", "nfl" => "play-by-play simulator", "mlb" => "plate-appearance simulator",
        e if e.ends_with("_team") => "team-strength model", _ => "model",
    }
}

// ---------------------------------------------------------------------------
// History (for facts)
// ---------------------------------------------------------------------------

/// Recent values of each stat, newest first, for players (props).
pub(crate) async fn player_history(db: &tokio_postgres::Client, sport: &str, ids: &[String]) -> Result<HashMap<String, Vec<(HashMap<&'static str, f64>, String)>>, String> {
    let mut out: HashMap<String, Vec<(HashMap<&'static str, f64>, String)>> = HashMap::new();
    if ids.is_empty() { return Ok(out); }
    match sport {
        "nba" => {
            let rows = db.query("SELECT player_id, pts, COALESCE(oreb,0)+COALESCE(dreb,0), ast, fg3m, stl, blk, tov, min, CASE WHEN b.team = g.home THEN g.away ELSE g.home END, \
                fgm, fga, fg3a, ftm, fta, oreb, dreb FROM ( \
                SELECT b.*, row_number() OVER (PARTITION BY b.player_id ORDER BY g2.game_date DESC) rn FROM nba_box_espn b JOIN nba_games_espn g2 USING (espn_id) \
                WHERE b.player_id = ANY($1) AND g2.completed AND g2.season_type IN (2,3) AND NOT b.dnp AND COALESCE(b.min,0) > 0 AND g2.game_date < (now() - interval '5 hours')::date) b \
                JOIN nba_games_espn g USING (espn_id) WHERE rn <= 82 ORDER BY player_id, g.game_date DESC", &[&ids]).await.map_err(|e| dberr(&e))?;
            for r in &rows {
                let g = |i: usize| r.get::<_, Option<i32>>(i).unwrap_or(0) as f64;
                let (pts, reb, ast) = (g(1), r.get::<_, i32>(2) as f64, g(3));
                let tens = [pts, reb, ast, g(5), g(6)].iter().filter(|x| **x >= 10.0).count();
                let m: HashMap<&'static str, f64> = [("pts", pts), ("reb", reb), ("ast", ast), ("fg3m", g(4)), ("stl", g(5)), ("blk", g(6)), ("tov", g(7)),
                    ("pra", pts + reb + ast), ("min", r.get::<_, Option<f64>>(8).unwrap_or(0.0)),
                    ("pr", pts + reb), ("pa", pts + ast), ("ra", reb + ast), ("stocks", g(5) + g(6)), ("fgm", g(10)), ("fga", g(11)), ("fg3a", g(12)),
                    ("ftm", g(13)), ("fta", g(14)), ("oreb", g(15)), ("dreb", g(16)),
                    ("double_double", (tens >= 2) as i32 as f64), ("triple_double", (tens >= 3) as i32 as f64)].into();
                out.entry(r.get(0)).or_default().push((m, r.get(9)));
            }
        }
        "nhl" => {
            let ids_n: Vec<i64> = ids.iter().filter_map(|x| x.parse().ok()).collect();
            let rows = db.query("SELECT player_id, sog, goals, assists, blocks, saves, CASE WHEN b.team = g.home THEN g.away ELSE g.home END FROM ( \
                SELECT b.*, row_number() OVER (PARTITION BY b.player_id ORDER BY g2.game_date DESC) rn FROM nhl_box_api b JOIN nhl_games_api g2 USING (game_id) \
                WHERE b.player_id = ANY($1) AND g2.completed AND COALESCE(b.toi_sec,0) > 0 AND g2.game_date < (now() - interval '5 hours')::date) b \
                JOIN nhl_games_api g USING (game_id) WHERE rn <= 82 ORDER BY player_id, g.game_date DESC", &[&ids_n]).await.map_err(|e| dberr(&e))?;
            for r in &rows {
                let g = |i: usize| r.get::<_, Option<i32>>(i).unwrap_or(0) as f64;
                let m: HashMap<&'static str, f64> = [("sog", g(1)), ("goals", g(2)), ("assists", g(3)), ("points", g(2) + g(3)), ("blocks", g(4)), ("saves", g(5))].into();
                out.entry(r.get::<_, i64>(0).to_string()).or_default().push((m, r.get(6)));
            }
        }
        "nfl" => {
            let cols: Vec<(&'static str, &'static str)> = NFL_PROPS.iter()
                .filter_map(|k| crate::player_backtest::stat_def("nfl", k).map(|d| (*k, d.column))).collect();
            let rows = db.query("SELECT player_id, stats, COALESCE(opponent,'') FROM ( \
                SELECT *, row_number() OVER (PARTITION BY player_id ORDER BY season DESC, game_date DESC NULLS LAST, week DESC) rn FROM player_game_logs \
                WHERE sport='nfl' AND player_id = ANY($1) AND (game_date IS NULL OR game_date < (now() - interval '5 hours')::date)) x WHERE rn <= 34 \
                ORDER BY player_id, rn", &[&ids]).await.map_err(|e| dberr(&e))?;
            for r in &rows {
                let st: Json<Value> = r.get(1);
                let m: HashMap<&'static str, f64> = cols.iter().map(|(k, c)| (*k, st.0[*c].as_f64().unwrap_or(0.0))).collect();
                out.entry(r.get(0)).or_default().push((m, r.get(2)));
            }
        }
        _ => {}
    }
    Ok(out)
}

/// Last ten finals per team: (points for, points against), newest first.
async fn team_form(db: &tokio_postgres::Client, sport: &str, teams: &[String]) -> Result<HashMap<String, Vec<(f64, f64)>>, String> {
    let (table, home, away, hs, as_) = match sport {
        "nba" => ("nba_games_espn", "home", "away", "home_score", "away_score"),
        "nhl" => ("nhl_games_api", "home", "away", "home_score", "away_score"),
        "nfl" => ("sports_games", "home_team", "away_team", "home_score", "away_score"),
        _ => return Ok(HashMap::new()),
    };
    let extra = if sport == "nfl" { "AND sport='nfl'" } else { "" };
    let q = format!("SELECT team, pf, pa FROM ( \
        SELECT t.team, t.pf, t.pa, row_number() OVER (PARTITION BY t.team ORDER BY t.d DESC) rn FROM ( \
          SELECT {home} team, {hs}::float8 pf, {as_}::float8 pa, game_date d FROM {table} WHERE {hs} IS NOT NULL AND {home} = ANY($1) {extra} \
          UNION ALL SELECT {away}, {as_}::float8, {hs}::float8, game_date FROM {table} WHERE {hs} IS NOT NULL AND {away} = ANY($1) {extra}) t) x \
        WHERE rn <= 10 ORDER BY team, rn");
    let rows = db.query(&q, &[&teams]).await.map_err(|e| dberr(&e))?;
    let mut out: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
    for r in &rows { out.entry(r.get(0)).or_default().push((r.get(1), r.get(2))); }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Writeups
// ---------------------------------------------------------------------------

fn evidence_sentence(sport: &str, tier: &str, ev: &Option<Evidence>, market: &str) -> String {
    let what = if segments::is_game_market(market) { format!("{} lines", segments::market_label(sport, market).to_lowercase()) } else { format!("{} props", stat_label(market).to_lowercase()) };
    match (tier, ev) {
        ("proven", Some(e)) if e.source == "live" => format!("Track record: on {} settled {what}, this engine's probabilities have beaten the de-vigged market.", e.n),
        ("proven", Some(e)) => format!("Track record: in walk-forward backtests this engine beat real {what} in {} of {} seasons and was never significantly worse.", e.better, e.seasons),
        ("baseline", Some(e)) => format!("Track record: in walk-forward backtests the engine beat a naive baseline on {what} in {} of {} seasons, but it has not been measured against real lines yet, so the grade is capped at B+.", e.better, e.seasons),
        ("losing", Some(e)) if e.source == "live" => format!("Caution: on {} settled {what}, the market's probabilities have been more accurate than this engine's; treat this as a lean, not a play.", e.n),
        ("losing", Some(e)) if e.detail["inherited_from"].is_string() => format!("Caution: this engine has no record on {what} yet, and in walk-forward backtests it did worse than a naive baseline on {} props in {} of {} seasons, so this is a lean, not a play.",
            stat_label(e.detail["inherited_from"].as_str().unwrap_or("")).to_lowercase(), e.worse, e.seasons),
        ("losing", Some(e)) => format!("Caution: in walk-forward backtests this engine did worse than {} on {what} in {} of {} seasons, so this is a lean, not a play.",
            if e.comparison == "market" { "the closing line" } else { "a naive baseline" }, e.worse, e.seasons),
        _ => format!("There is no backtest or live record for this engine on {what} yet, so the grade is capped at C+ until picks settle."),
    }
}

/// The deterministic writeup: every number comes from `facts`.
pub(crate) fn template(f: &Value) -> String {
    let s = |k: &str| f[k].as_str().unwrap_or("").to_string();
    let n = |k: &str| f[k].as_f64();
    let mut out = Vec::new();
    let side_word = if s("side") == "over" { "over" } else if s("side") == "under" { "under" } else { "" };
    if s("kind") == "prop" {
        let who = s("subject_name");
        let last = who.split_whitespace().last().unwrap_or(&who).to_string();
        let venue = if f["home"].as_bool() == Some(true) { format!("at home against {}", s("opponent")) } else { format!("on the road at {}", s("opponent")) };
        let range = match (n("q10"), n("q90")) { (Some(a), Some(b)) => format!(", 80% range {}–{}", fmt_num(a), fmt_num(b)), _ => String::new() };
        out.push(format!("{who} ({}) plays {venue}. The {} projects {} {} (median {}{range}) against a line of {}, making the {side_word} {}% likely; the de-vigged price at {} implies {}%, an edge of {} points.",
            s("team"), s("engine_label"), fmt_num(n("projection").unwrap_or(0.0)), s("stat_label").to_lowercase(), fmt_num(n("q50").unwrap_or(0.0)),
            fmt_num(n("line").unwrap_or(0.0)), pct(n("p_model").unwrap_or(0.0)), s("book_title"), pct(n("p_market").unwrap_or(0.0)), pct(n("edge").unwrap_or(0.0))));
        if let (Some(l10), Some(h), Some(g)) = (n("l10_avg"), n("l10_hits"), n("l10_games")) {
            let mut t = format!("Over the last {} games {last} has averaged {} and gone {side_word} {} in {} of them", fmt_num(g), fmt_num(l10), fmt_num(n("line").unwrap_or(0.0)), fmt_num(h));
            if let (Some(l20), Some(g20)) = (n("l20_avg"), n("l20_games")) { if g20 > g { t.push_str(&format!(" (last {}: {} per game)", fmt_num(g20), fmt_num(l20))); } }
            t.push('.');
            out.push(t);
        }
        if let (Some(pm), Some(l5m)) = (n("proj_minutes"), n("l5_minutes")) {
            out.push(format!("The simulation plays {last} {} minutes, against {} over the last five games.", fmt_num(pm), fmt_num(l5m)));
        }
    } else {
        let (home, away) = (s("home_team"), s("away_team"));
        let market = s("market");
        // first-segment markets: "in the 1st half" / "in the 1st period" / "in the first 5 innings"
        let seg = if market.ends_with("_1h") { format!(" in the {}", s("segment_label")) } else { String::new() };
        let tm = if market.starts_with("team_total_home") { Some(home.clone()) } else if market.starts_with("team_total_away") { Some(away.clone()) } else { None };
        match segments::base_market(&market) {
            "moneyline" => {
                let ties = match n("p_push") { Some(t) if market.ends_with("_1h") && t > 0.0 => format!(" (it is tied after the {} in {}% of simulations, which refunds the bet)", s("segment_label"), pct(t)), _ => String::new() };
                out.push(format!("{away} at {home}: the {} gives {} a {}% chance to lead{seg}{ties}; the de-vigged price at {} implies {}%, an edge of {} points.",
                    s("engine_label"), s("pick_team"), pct(n("p_model").unwrap_or(0.0)), s("book_title"), pct(n("p_market").unwrap_or(0.0)), pct(n("edge").unwrap_or(0.0))))
            }
            "spread" => out.push(format!("{away} at {home}: the {} projects a home margin of {}{seg}, so {} {} covers {}% of the time (pushes excluded) against {}% implied by {}, an edge of {} points.",
                s("engine_label"), signed(n("projection").unwrap_or(0.0)), s("pick_team"), signed(n("line").unwrap_or(0.0)), pct(n("p_model").unwrap_or(0.0)),
                pct(n("p_market").unwrap_or(0.0)), s("book_title"), pct(n("edge").unwrap_or(0.0)))),
            _ => {
                let what = match &tm { Some(t) => format!("{t} to score {}", fmt_num(n("projection").unwrap_or(0.0))), None => format!("{} total", fmt_num(n("projection").unwrap_or(0.0))) };
                out.push(format!("{away} at {home}: the {} projects {what}{seg} (median {}), so the {side_word} {} hits {}% of the time against {}% implied by {}, an edge of {} points.",
                    s("engine_label"), fmt_num(n("q50").unwrap_or(0.0)), fmt_num(n("line").unwrap_or(0.0)),
                    pct(n("p_model").unwrap_or(0.0)), pct(n("p_market").unwrap_or(0.0)), s("book_title"), pct(n("edge").unwrap_or(0.0))))
            }
        }
        let form = |t: &str, k: &str| -> Option<String> {
            let v = &f[k];
            Some(format!("{t} is {}-{} over its last {} ({} scored, {} allowed per game)", v["w"].as_f64().map(fmt_num)?, v["l"].as_f64().map(fmt_num)?,
                v["games"].as_f64().map(fmt_num)?, v["pf"].as_f64().map(fmt_num)?, v["pa"].as_f64().map(fmt_num)?))
        };
        let parts: Vec<String> = [form(&home, "home_form"), form(&away, "away_form")].into_iter().flatten().collect();
        if !parts.is_empty() { out.push(format!("{}.", parts.join("; "))); }
        for (side, team) in [("home", &home), ("away", &away)] {
            let mut why = Vec::new();
            if n(&format!("{side}_rest_days")) == Some(1.0) { why.push("is on the second night of a back-to-back".to_string()); }
            if let Some(tz) = n(&format!("{side}_tz_shift")) { if tz.abs() >= 1.0 {
                why.push(format!("crossed {} time zone{} {}", fmt_num(tz.abs()), if tz.abs() > 1.0 { "s" } else { "" }, if tz > 0.0 { "east" } else { "west" })); } }
            if n(&format!("{side}_rest_days")).map(|r| r >= 3.0).unwrap_or(false) { why.push(format!("has had {} days off", fmt_num(n(&format!("{side}_rest_days")).unwrap_or(0.0)))); }
            let sm = n(&format!("{side}_schedule_margin")).unwrap_or(0.0);
            if !why.is_empty() && sm.abs() >= 0.1 {
                out.push(format!("{team} {}, worth {} points of margin in the simulation.", why.join(" and "), signed(sm)));
            }
            if let Some(g) = n(&format!("{side}_lineup_gap")) {
                out.push(format!("{team}'s expected lineup rates {} points {} its last ten games on player impact.", fmt_num(g.abs()), if g > 0.0 { "better than" } else { "worse than" }));
            }
        }
    }
    if let (Some(raw), Some(pm)) = (n("p_raw"), n("p_model")) {
        if (pct(raw) - pct(pm)).abs() >= 0.1 {
            out.push(format!("Those probabilities are blended: on its own the {} says {}%, and weighted against the market (trust weight {}, {}) that becomes {}%.",
                s("engine_label"), pct(raw), fmt2(n("w_market").unwrap_or(0.0)), s("blend_text"), pct(pm)));
        }
    }
    if let (Some(ps), Some(sev)) = (n("p_sharp"), n("sharp_ev")) {
        out.push(format!("Sharp check: {} prices this side at {}%, so this price is worth {} per unit without the model.", s("sharp_text"), pct(ps), signed_pct(sev)));
    }
    if let (Some(ol), Some(l)) = (n("open_line"), n("line")) {
        if (ol - l).abs() > 1e-9 { out.push(format!("The line has moved from {} at open to {}.", fmt_num(ol), fmt_num(l))); }
    }
    out.push(s("evidence_text"));
    let against = against_case(f);
    if !against.is_empty() { out.push(format!("What could make it wrong: {against}")); }
    out.join(" ")
}

/// The case against the pick, from the facts only: what the bettor is relying on
/// and what would break it.
pub(crate) fn against_case(f: &Value) -> String {
    let s = |k: &str| f[k].as_str().unwrap_or("").to_string();
    let n = |k: &str| f[k].as_f64();
    let side = s("side");
    let mut out: Vec<String> = Vec::new();
    if let (Some(pm), Some(pk)) = (n("p_model"), n("p_market")) {
        out.push(format!("the simulations still land on the other side {}% of the time, and the de-vigged market says {}%, so the edge rests on the model seeing something the books have not.",
            pct(1.0 - pm), pct(pk)));
    }
    if s("kind") == "prop" {
        let who = s("subject_name");
        let last = who.split_whitespace().last().unwrap_or(&who).to_string();
        if let (Some(h), Some(g), Some(l)) = (n("l10_hits"), n("l10_games"), n("line")) {
            if g >= 5.0 && h * 2.0 < g { out.push(format!("{last} has finished {side} {} in only {} of the last {} games.", fmt_num(l), fmt_num(h), fmt_num(g))); }
        }
        if let (Some(pmin), Some(l5)) = (n("proj_minutes"), n("l5_minutes")) {
            if side == "over" && pmin - l5 >= 3.0 { out.push(format!("The projection plays {last} {} minutes, more than the recent {}; a normal workload shrinks the over.", fmt_num(pmin), fmt_num(l5))); }
            if side == "under" && l5 - pmin >= 3.0 { out.push(format!("The projection plays {last} {} minutes, fewer than the recent {}; a full workload breaks the under.", fmt_num(pmin), fmt_num(l5))); }
        }
        if let Some(a) = n("p_active") { if a < 0.97 { out.push(format!("{last} is not certain to play ({}% of simulations).", pct(a))); } }
    }
    if s("lineup_mode") == "projected" { out.push("Lineups are projected, so a late absence changes the numbers.".into()); }
    if let (Some(ps), Some(sev)) = (n("p_sharp"), n("sharp_ev")) {
        if sev < 0.0 { out.push(format!("{} has this side at {}%, which makes the price a losing bet without the model.", s("sharp_text"), pct(ps))); }
    }
    if let (Some(o), Some(l)) = (n("open_line"), n("line")) {
        let against = match side.as_str() { "over" => l > o, "under" => l < o, _ => l < o };
        if (l - o).abs() > 1e-9 && against { out.push(format!("The line has already moved against this side ({} to {}).", fmt_num(o), fmt_num(l))); }
    }
    if let (Some(raw), Some(pm)) = (n("p_raw"), n("p_model")) {
        if raw - pm >= 0.05 { out.push(format!("On its own the engine is more confident ({}%), but its record only justifies {}%.", pct(raw), pct(pm))); }
    }
    if let Some(p) = n("p_push") { if p > 0.05 { out.push(format!("{}% of simulations push.", pct(p))); } }
    if let (Some(pp), Some(m)) = (f["playable_price"].as_i64(), n("playable_min_ev")) {
        out.push(format!("At worse odds than {} the bet is worth less than {}% per unit.", odds_str(pp as i32), pct(m)));
    }
    let mut t = out.join(" ");
    if let Some(c) = t.get(0..1) { t = c.to_uppercase() + &t[1..]; }
    t
}

/// Numbers in a text (for checking an LLM rewrite against the facts).
pub(crate) fn numbers(text: &str) -> Vec<f64> {
    let mut out = Vec::new();
    let b: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let st = i;
            while i < b.len() && (b[i].is_ascii_digit() || (b[i] == '.' && i + 1 < b.len() && b[i + 1].is_ascii_digit())) { i += 1; }
            if let Ok(x) = b[st..i].iter().collect::<String>().parse::<f64>() { out.push(x); }
        } else { i += 1; }
    }
    out
}

/// The writeup model: (base url, model, api key) from PICKS_LLM_* or LLM_*.
pub(crate) fn llm_endpoint() -> Option<(String, String, Option<String>)> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let base = env("PICKS_LLM_BASE_URL").or_else(|| env("LLM_BASE_URL"))?;
    let model = env("PICKS_LLM_MODEL").or_else(|| env("LLM_MODEL")).unwrap_or_else(|| "qwen2.5:7b".into());
    Some((base.trim_end_matches('/').to_string(), model, env("PICKS_LLM_API_KEY").or_else(|| env("LLM_API_KEY"))))
}

/// Is the writeup model up? (one quick request, so a stopped server costs nothing per pick)
pub(crate) async fn llm_reachable(state: &State) -> bool {
    let Some((base, _, key)) = llm_endpoint() else { return false };
    let mut req = state.http.get(format!("{base}/models")).timeout(std::time::Duration::from_secs(3));
    if let Some(k) = key { req = req.bearer_auth(k); }
    req.send().await.map(|r| r.status().is_success()).unwrap_or(false)
}

/// Optional NLP rewrite through an OpenAI-compatible endpoint (PICKS_LLM_BASE_URL
/// or LLM_BASE_URL). Rejected unless every number it uses is in the facts.
async fn llm_writeup(state: &State, facts: &Value, template: &str) -> Option<(String, String)> {
    let (base, model, key) = llm_endpoint()?;
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let _ = env;
    let system = "You are MonsterGPT, a sports betting analyst. Rewrite the draft into one tight paragraph of 3 to 5 sentences explaining why the pick surfaced. \
        Use ONLY the numbers in the facts and draft; do not invent statistics, injuries, trends or news. Keep the risk or track-record caution if the draft has one. \
        Then end with a second part that starts exactly \"What could make it wrong:\" and keeps the draft's case against the pick. \
        No hype, no guarantees, no emojis. Plain text only.";
    let user = format!("FACTS (JSON):\n{}\n\nDRAFT:\n{template}", serde_json::to_string(facts).ok()?);
    let body = json!({ "model": model, "temperature": 0.3, "max_tokens": 320, "messages": [{ "role": "system", "content": system }, { "role": "user", "content": user }] });
    let mut req = state.http.post(format!("{base}/chat/completions")).json(&body).timeout(std::time::Duration::from_secs(90));
    if let Some(k) = key { req = req.bearer_auth(k); }
    let v: Value = req.send().await.ok()?.json().await.ok()?;
    let text = v["choices"][0]["message"]["content"].as_str()?.trim().to_string();
    if text.len() < 120 || text.len() > 2000 || !text.contains("What could make it wrong:") { return None; }
    let low = text.to_lowercase();
    if ["guarantee", "lock of", "can't lose", "cannot lose", "sure thing"].iter().any(|w| low.contains(w)) { return None; }
    let mut allowed = numbers(template);
    fn walk(v: &Value, out: &mut Vec<f64>) {
        match v { Value::Number(n) => if let Some(x) = n.as_f64() { out.push(x); out.push(x * 100.0); }, Value::String(s) => out.extend(numbers(s)),
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)), Value::Object(o) => o.values().for_each(|x| walk(x, out)), _ => {} }
    }
    walk(facts, &mut allowed);
    let ok = numbers(&text).iter().all(|x| allowed.iter().any(|a| (a - x).abs() <= 0.051 || (a.round() - x).abs() < 1e-9 && *x >= 10.0));
    if !ok { return None; }
    Some((text, format!("llm:{model}")))
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

/// One book's two-way quote for a selection.
#[derive(Clone, Debug)]
struct Quote { book: String, a_line: Option<f64>, a_price: i32, b_price: i32, a_link: Option<String>, observed: DateTime<Utc>, open_line: Option<f64>, provider_subject: Option<String>, subject_name: Option<String> }

struct Ledger { id: i64, version: String, lineup: String, payload: Value }

pub(crate) fn other(side: &str) -> &'static str { match side { "over" => "under", "under" => "over", "home" => "away", _ => "home" } }

async fn latest_ledger(db: &tokio_postgres::Client, engine: &str, version: &str, ids: &[String]) -> Result<HashMap<String, Ledger>, String> {
    let rows = db.query("SELECT DISTINCT ON (game_id) game_id, id, model_version, lineup_mode, payload FROM prediction_ledger \
        WHERE sport=$1 AND model_version=$2 AND game_id = ANY($3) ORDER BY game_id, (lineup_mode='confirmed') DESC, id DESC", &[&engine, &version, &ids]).await.map_err(|e| dberr(&e))?;
    Ok(rows.iter().map(|r| { let p: Json<Value> = r.get(4); (r.get(0), Ledger { id: r.get(1), version: r.get(2), lineup: r.get(3), payload: p.0 }) }).collect())
}

pub(crate) async fn build(state: &State, sport: &str) -> Result<Value, String> {
    let db = &state.db;
    let min_edge = lines::setting(db, "min_edge", 0.015).await;
    // edges this large against real markets are almost always a data or model error
    let max_edge = lines::setting(db, "max_edge", 0.25).await;
    // "playable to": the worst price still worth this much per unit
    let playable_ev = lines::setting(db, "playable_min_ev", 0.02).await;
    backfill_playable(db, sport, playable_ev).await?;
    // market weight used until one is fitted (blend.rs)
    let default_w = lines::setting(db, "blend_default_w", 0.2).await;
    let (kelly_f, kelly_cap) = crate::sharp::stake_settings(db).await;
    let min_books = lines::setting(db, "stale_consensus_books", 4.0).await as usize;
    let champs = champions(db).await?;
    let ev = evidence(db, &champs).await?;
    // latest quote per book x selection (any provider; the freshest wins)
    let rows = db.query("SELECT DISTINCT ON (book, game_id, market, subject, side) book, game_id, kind, market, subject, subject_name, provider_subject, side, line, price, link, open_line, observed_at, starts_at \
        FROM market_lines WHERE sport=$1 AND game_id IS NOT NULL AND starts_at > now() + interval '5 minutes' AND observed_at > now() - interval '3 hours' \
        ORDER BY book, game_id, market, subject, side, observed_at DESC", &[&sport]).await.map_err(|e| dberr(&e))?;
    // pair sides at the same book
    type Key = (String, String, String); // game, market, subject
    let mut sides: HashMap<(Key, String, String), (Option<f64>, i32, Option<String>, DateTime<Utc>, Option<f64>, Option<String>, Option<String>)> = HashMap::new();
    let mut starts: HashMap<String, DateTime<Utc>> = HashMap::new();
    for r in &rows {
        let key: Key = (r.get(1), r.get(3), r.get(4));
        if let Some(s) = r.get::<_, Option<DateTime<Utc>>>(13) { starts.insert(key.0.clone(), s); }
        sides.insert((key, r.get(0), r.get(7)), (r.get(8), r.get(9), r.get(10), r.get(12), r.get(11), r.get(6), r.get(5)));
    }
    // odds feeds the validation engine found mislabelled (any sport, market, book)
    let quarantined: HashSet<(String, String)> = db.query("SELECT market, book FROM feed_quarantine WHERE sport=$1", &[&sport]).await
        .map(|rows| rows.iter().map(|r| (r.get(0), r.get(1))).collect()).unwrap_or_default();
    let mut quotes: HashMap<(Key, String), Vec<Quote>> = HashMap::new(); // (selection, side) -> quotes
    for ((key, book, side), (line, price, link, obs, open, ps, name)) in &sides {
        if quarantined.contains(&(key.1.clone(), book.clone())) { continue; }
        let partner = sides.get(&(key.clone(), book.clone(), other(side).to_string())).map(|x| (x.0, x.1));
        let (oline, oprice) = match partner {
            Some(x) => x,
            // a yes-only double-double / triple-double / anytime-TD price: pair it with its complement
            None if side == "over" && crate::lines::yes_no_market(&key.1) && *line == Some(0.5) => (*line, complement_price(*price)),
            None => continue,
        };
        let (oline, oprice) = (&oline, &oprice);
        let same = match segments::base_market(&key.1) { "moneyline" => true, "spread" => matches!((line, oline), (Some(a), Some(b)) if (a + b).abs() < 1e-9), _ => line == oline };
        if !same { continue; }
        quotes.entry((key.clone(), side.clone())).or_default().push(Quote { book: book.clone(), a_line: *line, a_price: *price, b_price: *oprice, a_link: link.clone(),
            observed: *obs, open_line: *open, provider_subject: ps.clone(), subject_name: name.clone() });
    }
    // with three or more books at one line, a quote far from their median is a
    // feed error (wrong line or market), not a soft price
    let mut outliers = 0;
    for qs in quotes.values_mut() {
        let mut by_line: HashMap<String, Vec<f64>> = HashMap::new();
        for q in qs.iter() { by_line.entry(format!("{:?}", q.a_line)).or_default().push(novig(q.a_price, q.b_price)); }
        let n0 = qs.len();
        qs.retain(|q| match by_line.get(&format!("{:?}", q.a_line)) {
            Some(v) if v.len() >= 3 => (novig(q.a_price, q.b_price) - median(v)).abs() <= OUTLIER_GAP,
            _ => true,
        });
        outliers += n0 - qs.len();
    }
    quotes.retain(|_, qs| !qs.is_empty());
    let games: Vec<String> = quotes.keys().map(|((g, _, _), _)| g.clone()).collect::<HashSet<_>>().into_iter().collect();
    // ledger entries of each champion engine for these games
    let mut ledgers: HashMap<&str, HashMap<String, Ledger>> = HashMap::new();
    let team_engine = format!("{sport}_team");
    for e in [sport, team_engine.as_str()] {
        if let Some((v, _)) = champs.get(e) { ledgers.insert(e, latest_ledger(db, e, v, &games).await?); }
    }
    let tier_of = |engine: &str, market: &str| prop_tier(&ev, engine, market);
    let weights_of = |engine: &str, market: &str| blend::weights(ev.get(&(engine.to_string(), market.to_string())).map(|v| v.as_slice()).unwrap_or(&[]), default_w);
    // every prop player's history: the naive baseline (own prior frequency over the line) and the facts
    let pids: Vec<String> = quotes.keys().filter(|((_, _, s), _)| s != "game").map(|((_, _, s), _)| s.clone()).collect::<HashSet<_>>().into_iter().collect();
    let hist = player_history(db, sport, &pids).await?;
    let naive_over = |subject: &str, stat: &str, line: Option<f64>| -> Option<f64> {
        let vals: Vec<f64> = hist.get(subject)?.iter().filter_map(|(m, _)| m.get(stat).copied()).collect();
        if vals.len() < 5 { return None; }
        Some(blend::freq_over(&vals, line?))
    };
    // ESPN prop orientation check: the model and the market should agree on which side is favoured most of the time
    let (mut agree, mut checked) = (0, 0);
    for (((g, m, subj), side), qs) in &quotes {
        if side != "over" || subj == "game" { continue; }
        let Some(l) = ledgers.get(sport).and_then(|x| x.get(g)) else { continue };
        for q in qs.iter().filter(|q| q.book == "draftkings") {
            let (Some(p), mk) = (price(&l.payload, false, m, subj, "over", q.a_line), novig(q.a_price, q.b_price)) else { continue };
            if (p.p() - 0.5).abs() > 0.15 && (mk - 0.5).abs() > 0.1 { checked += 1; if (p.p() > 0.5) == (mk > 0.5) { agree += 1; } }
        }
    }
    let orientation_ok = checked < 30 || agree * 10 >= checked * 5;

    // candidate per selection-side: the book whose quote gives the best EV
    struct Cand { key: Key, side: String, engine: String, ledger_id: i64, version: String, lineup: String, q: Quote, priced: Priced, p_raw: f64, p_naive: Option<f64>,
        w: blend::Weights, p_market: f64, edge: f64, ev: f64, books: usize, tier: &'static str, evd: Option<Evidence> }
    let mut cands: Vec<Cand> = Vec::new();
    let mut considered = 0;
    let mut by_selection: HashMap<Key, Vec<usize>> = HashMap::new();
    for ((key, side), qs) in &quotes {
        let kind_prop = key.2 != "game";
        if kind_prop && !orientation_ok && qs.iter().all(|q| q.book == "draftkings") { continue; }
        // engines that can price it, best track record first
        let mut engines: Vec<&str> = vec![sport];
        if !kind_prop && ledgers.get(team_engine.as_str()).map(|m| m.contains_key(&key.0)).unwrap_or(false) { engines.push(team_engine.as_str()); }
        let rank = |t: &str| match t { "proven" => 0, "baseline" => 1, "unproven" => 2, _ => 3 };
        engines.sort_by_key(|e| rank(tier_of(e, &key.1).0));
        for e in engines {
            let Some(l) = ledgers.get(e).and_then(|m| m.get(&key.0)) else { continue };
            let team = e.ends_with("_team");
            let w = weights_of(e, &key.1);
            // each quote: the engine's raw probability, blended toward the naive
            // baseline (props) and the consensus at that line; the best EV wins
            let mut best: Option<(Quote, Priced, f64, f64, Option<f64>, f64, usize)> = None;
            for q in qs {
                let Some(raw) = price(&l.payload, team, &key.1, &key.2, side, q.a_line) else { continue };
                let same: Vec<f64> = qs.iter().filter(|x| x.a_line == q.a_line).map(|x| novig(x.a_price, x.b_price)).collect();
                let p_market = same.iter().sum::<f64>() / same.len().max(1) as f64;
                let p_naive = if kind_prop { naive_over(&key.2, &key.1, q.a_line).map(|o| if side == "over" { o } else { 1.0 - o }) } else { None };
                let pb = blend::apply(&w, raw.p(), p_naive, p_market);
                let mut p = raw.clone();
                let decided = 1.0 - raw.push;
                p.win = pb * decided;
                p.loss = (1.0 - pb) * decided;
                let evv = p.win * (decimal(q.a_price) - 1.0) - p.loss;
                if best.as_ref().map(|b| evv > b.2).unwrap_or(true) { best = Some((q.clone(), p, evv, raw.p(), p_naive, p_market, same.len())); }
            }
            let Some((q, p, evv, p_raw, p_naive, p_market, books)) = best else { continue };
            considered += 1;
            let pm = p.p();
            let (t, evd) = tier_of(e, &key.1);
            if !(0.03..=0.97).contains(&p_raw) || !(0.12..=0.88).contains(&p_market) { break; }
            if kind_prop && p.p_active.map(|a| a < 0.5).unwrap_or(false) { break; }
            let c = Cand { key: key.clone(), side: side.clone(), engine: e.to_string(), ledger_id: l.id, version: l.version.clone(), lineup: l.lineup.clone(),
                q, priced: p, p_raw, p_naive, w, p_market, edge: pm - p_market, ev: evv, books, tier: t, evd };
            by_selection.entry(key.clone()).or_default().push(cands.len());
            cands.push(c);
            break; // the best-evidenced engine that can price it decides
        }
    }
    // open picks follow the evidence as it changes: a quarantined feed voids them,
    // a lower tier re-caps their grade (never raises it)
    let mut withdrawn = db.execute("UPDATE ai_picks p SET status='void', result='void', profit=0, settled_at=now(), \
        risk_flags = risk_flags || '[{\"code\":\"feed_quarantined\",\"text\":\"the odds feed for this market and book was quarantined: its quotes did not match real outcomes\"}]'::jsonb \
        WHERE p.sport=$1 AND p.status='open' AND EXISTS (SELECT 1 FROM feed_quarantine q WHERE q.sport=p.sport AND q.market=p.market AND q.book=p.book)", &[&sport])
        .await.map_err(|e| dberr(&e))?;
    for r in db.query("SELECT id, engine, market, tier, grade FROM ai_picks WHERE sport=$1 AND status='open'", &[&sport]).await.map_err(|e| dberr(&e))? {
        let (engine, market, old_tier, grade): (String, String, String, String) = (r.get(1), r.get(2), r.get(3), r.get(4));
        let (t, _) = tier_of(&engine, &market);
        if t == old_tier { continue; }
        let g = GRADES.iter().find(|x| **x == grade).copied().unwrap_or("C");
        // price, probabilities, line and ledger stay as published; the published grade is kept in the flag
        let flag = json!([{ "code": "regraded", "text": format!("published as {grade} ({old_tier}); re-capped after the engine's track record on this market changed to {t}"),
            "published_grade": grade, "published_tier": old_tier, "at": Utc::now() }]);
        withdrawn += db.execute("UPDATE ai_picks SET tier=$2, grade=$3, risk_flags = risk_flags || $4, updated_at=now() WHERE id=$1",
            &[&r.get::<_, i64>(0), &t, &cap(g, t), &Json(&flag)]).await.map_err(|e| dberr(&e))?;
    }
    // publication safety rule: a published market whose tier fell below the bar is suspended (never the reverse)
    let suspended = crate::governance::safety_sweep(db, sport, |e, m| tier_of(e, m).0).await?;
    // existing picks (for latest_* updates)
    // the implausibility guard looks at the RAW model: a blend can shrink a broken forecast into a plausible-looking edge
    let qualifies = |c: &Cand| c.edge >= min_edge && c.ev > 0.0 && (c.p_raw - c.p_market) <= max_edge;
    let implausible = cands.iter().filter(|c| c.p_raw - c.p_market > max_edge && c.ev > 0.0).count();
    let mut keep: Vec<&Cand> = cands.iter().filter(|c| qualifies(c)).collect();

    let recorded = record_lines(db, sport, &quotes, &ledgers, &champs, &starts, &naive_over).await?;
    // NBA: the simulated role must look like the player's real one. A player the
    // rotation gives a fraction of his recent minutes (or far more) is a lineup
    // problem upstream, not an edge.
    let mut rotation_mismatch = 0;
    if sport == "nba" {
        keep.retain(|c| {
            if c.key.2 == "game" { return true; }
            let Some(l) = ledgers.get(c.engine.as_str()).and_then(|m| m.get(&c.key.0)) else { return true };
            let sim_min = player_entry(&l.payload, &c.key.2).and_then(|(e, _)| e["stats"]["min"]["mean"].as_f64());
            let recent: Vec<f64> = hist.get(&c.key.2).map(|h| h.iter().take(5).filter_map(|(m, _)| m.get("min").copied()).collect()).unwrap_or_default();
            let ok = match (sim_min, recent.len()) {
                (Some(sm), n) if n >= 3 => { let r = recent.iter().sum::<f64>() / n as f64; sm >= 0.6 * r && sm <= 1.5 * r + 6.0 }
                _ => true,
            };
            if !ok { rotation_mismatch += 1; }
            ok
        });
    }
    let mut teams: Vec<String> = Vec::new();
    for c in &keep { if let Some(l) = ledgers.get(c.engine.as_str()).and_then(|m| m.get(&c.key.0)) {
        for s in ["home", "away"] { let t = lines::payload_team(&l.payload, s); if !t.is_empty() { teams.push(t); } } } }
    teams.sort(); teams.dedup();
    let form = team_form(db, sport, &teams).await?;
    let heads: HashMap<String, String> = {
        let ids: Vec<String> = keep.iter().filter_map(|c| c.q.provider_subject.clone()).collect();
        db.query("SELECT espn_id, headshot FROM espn_athletes WHERE sport=$1 AND espn_id = ANY($2) AND headshot IS NOT NULL", &[&sport, &ids]).await
            .map_err(|e| dberr(&e))?.iter().map(|r| (r.get(0), r.get(1))).collect()
    };
    let existing: HashSet<(String, String, String, String, String)> = db.query("SELECT game_id, market, subject, side, engine FROM ai_picks WHERE sport=$1 AND status='open'", &[&sport]).await
        .map_err(|e| dberr(&e))?.iter().map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4))).collect();

    let mut published = 0;
    let mut updated = 0;
    // no new pick this close to the start: picks made under 2 h before start beat the close 23% of the
    // time vs 53% for picks made 24 h+ ahead (research-backlog R9). Lines are still recorded for evidence.
    let min_lead = lines::setting(db, "pick_min_lead_minutes", 120.0).await as i64;
    let mut too_late = 0;
    for c in &keep {
        let l = &ledgers[c.engine.as_str()][&c.key.0];
        let (home, away) = (lines::payload_team(&l.payload, "home"), lines::payload_team(&l.payload, "away"));
        let pm = c.priced.p();
        let grade = cap(edge_grade(c.edge).unwrap_or("C"), c.tier);
        let label = pick_label(sport, &c.key.1, &c.side, c.q.a_line, &home, &away);
        let exists = existing.contains(&(c.key.0.clone(), c.key.1.clone(), c.key.2.clone(), c.side.clone(), c.engine.clone()));
        if exists {
            updated += db.execute("UPDATE ai_picks SET latest_line=$1, latest_price=$2, latest_book=$3, latest_p_market=$4, latest_edge=$5, updated_at=now() \
                WHERE sport=$6 AND game_id=$7 AND market=$8 AND subject=$9 AND side=$10 AND engine=$11 AND status='open'",
                &[&c.q.a_line, &c.q.a_price, &c.q.book, &c.p_market, &c.edge, &sport, &c.key.0, &c.key.1, &c.key.2, &c.side, &c.engine]).await.map_err(|e| dberr(&e))?;
            continue;
        }
        // facts
        let mut f = serde_json::Map::new();
        let kind = if c.key.2 == "game" { "game" } else { "prop" };
        let (subject_name, subject_team) = if kind == "prop" {
            let (entry, team) = player_entry(&l.payload, &c.key.2).map(|(e, t)| (Some(e), t)).unwrap_or((None, String::new()));
            (c.q.subject_name.clone().or_else(|| entry.and_then(|e| e["name"].as_str().map(str::to_string))).unwrap_or_else(|| c.key.2.clone()), team)
        } else {
            (format!("{away} @ {home}"), if c.side == "home" { home.clone() } else if c.side == "away" { away.clone() } else { String::new() })
        };
        let opponent = if subject_team == home { away.clone() } else { home.clone() };
        for (k, v) in [("kind", json!(kind)), ("market", json!(c.key.1)), ("side", json!(c.side)), ("line", json!(c.q.a_line)), ("price", json!(c.q.a_price)),
            ("book", json!(c.q.book)), ("book_title", json!(book_title(&c.q.book))), ("books_quoting", json!(c.books)),
            ("p_model", json!(r3(pm))), ("p_market", json!(r3(c.p_market))), ("edge", json!(r3(c.edge))), ("ev", json!(r3(c.ev))),
            ("p_raw", json!(r3(c.p_raw))), ("p_naive", json!(c.p_naive.map(r3))), ("w_market", json!(r3(c.w.w_market))), ("w_naive", json!(c.w.w_naive.map(r3))),
            ("blend_source", json!(c.w.market_source)), ("blend_text", json!(blend::source_text(&c.w))),
            ("p_push", json!(r3(c.priced.push))), ("projection", json!(c.priced.projection.map(|x| r1(x)))),
            ("engine", json!(c.engine)), ("engine_label", json!(engine_label(&c.engine))), ("model_version", json!(c.version)), ("lineup_mode", json!(c.lineup)),
            ("tier", json!(c.tier)), ("evidence_text", json!(evidence_sentence(sport, c.tier, &c.evd, &c.key.1))), ("open_line", json!(c.q.open_line)),
            ("home_team", json!(home)), ("away_team", json!(away)), ("subject_name", json!(subject_name)), ("team", json!(subject_team)),
            ("opponent", json!(opponent)), ("home", json!(subject_team == home)),
            ("stat_label", json!(if kind == "game" { segments::market_label(sport, &c.key.1) } else { stat_label(&c.key.1).to_string() })),
            ("segment_label", json!(segments::label(sport))),
            ("pick_team", json!(if c.side == "home" { home.clone() } else { away.clone() }))] { f.insert(k.into(), v); }
        if let Some((a, b, cc)) = c.priced.q { f.insert("q10".into(), json!(a)); f.insert("q50".into(), json!(b)); f.insert("q90".into(), json!(cc)); }
        if let Some(a) = c.priced.p_active { f.insert("p_active".into(), json!(r3(a))); }
        if let Some(evd) = &c.evd { f.insert("evidence".into(), json!({ "source": evd.source, "comparison": evd.comparison, "n": evd.n, "seasons": evd.seasons, "better": evd.better, "worse": evd.worse, "engine_version": evd.detail["engine_version"] })); }
        let mut flags: Vec<Value> = Vec::new();
        // the sharp check: what Pinnacle (or the other books' consensus) makes of this exact price
        let refs: Vec<crate::sharp::Ref> = quotes.get(&(c.key.clone(), c.side.clone())).map(|qs| qs.iter().filter(|q| q.a_line == c.q.a_line)
            .map(|q| (q.book.as_str(), q.a_price, q.b_price, q.observed)).collect()).unwrap_or_default();
        let mut stake = crate::sharp::kelly(pm, c.q.a_price, kelly_f, kelly_cap) * crate::sharp::tier_stake_mult(c.tier);
        if let Some((ps, src, nb)) = crate::sharp::fair(&refs, &c.q.book, c.q.observed, min_books) {
            let sev = ps * decimal(c.q.a_price) - 1.0;
            f.insert("p_sharp".into(), json!(r3(ps)));
            f.insert("sharp_ev".into(), json!(r3(sev)));
            f.insert("sharp_source".into(), json!(src));
            f.insert("sharp_text".into(), json!(crate::sharp::source_text(src, nb)));
            if sev < 0.0 {
                flags.push(json!({ "code": "against_sharp", "text": format!("{} prices this side at {}%, which makes {} a losing bet without the model",
                    crate::sharp::source_text(src, nb), pct(ps), odds_str(c.q.a_price)) }));
                // Pinnacle is the best-measured price there is: no stake against it
                if src == "pinnacle" { stake = 0.0; }
            }
        }
        f.insert("stake".into(), json!((stake * 10000.0).round() / 10000.0));
        // price sensitivity: break-even, and the worst price still worth playable_min_ev
        let fair_price = price_for_ev(c.priced.win, c.priced.loss, 0.0);
        let playable_price = playable_floor(c.priced.win, c.priced.loss, playable_ev, c.q.a_price);
        f.insert("fair_price".into(), json!(fair_price));
        f.insert("playable_price".into(), json!(playable_price));
        f.insert("playable_min_ev".into(), json!(playable_ev));
        if kind == "prop" {
            if let (Some(h), Some(line)) = (hist.get(&c.key.2), c.q.a_line) {
                let stat: &str = &c.key.1;
                let vals: Vec<f64> = h.iter().filter_map(|(m, _)| m.get(stat).copied()).collect();
                let win = |v: f64| if c.side == "over" { v > line } else { v < line };
                for (k, w) in [("l10", 10usize), ("l20", 20)] {
                    let s: Vec<f64> = vals.iter().take(w).copied().collect();
                    if s.is_empty() { continue; }
                    f.insert(format!("{k}_avg"), json!(r1(s.iter().sum::<f64>() / s.len() as f64)));
                    f.insert(format!("{k}_games"), json!(s.len()));
                    f.insert(format!("{k}_hits"), json!(s.iter().filter(|v| win(**v)).count()));
                }
                f.insert("game_log".into(), json!(h.iter().take(10).map(|(m, opp)| json!({ "opp": opp, "value": m.get(stat) })).collect::<Vec<_>>()));
                if sport == "nba" {
                    let mins: Vec<f64> = h.iter().take(5).filter_map(|(m, _)| m.get("min").copied()).collect();
                    if !mins.is_empty() { f.insert("l5_minutes".into(), json!(r1(mins.iter().sum::<f64>() / mins.len() as f64))); }
                    if let Some(pmin) = player_entry(&l.payload, &c.key.2).and_then(|(e, _)| e["stats"]["min"]["mean"].as_f64()) { f.insert("proj_minutes".into(), json!(r1(pmin))); }
                }
                if vals.len() < 8 { flags.push(json!({ "code": "small_sample", "text": format!("only {} recent games on record", vals.len()) })); }
            }
            if let Some(a) = c.priced.p_active { if a < 0.9 { flags.push(json!({ "code": "may_not_play", "text": format!("plays in {}% of simulations (props void if inactive)", pct(a)) })); } }
        } else {
            // NBA simulator inputs: schedule and lineup impact per team (forecasts frozen by the engine that has them)
            for tm in l.payload["inputs"]["teams"].as_array().into_iter().flatten() {
                let Some(abbr) = tm["abbr"].as_str() else { continue };
                let side = if abbr == home { "home" } else if abbr == away { "away" } else { continue };
                let s = &tm["schedule"];
                let used = s["own_points"].as_f64().map(|x| x.abs() > 1e-9).unwrap_or(false) || s["opponent_points"].as_f64().map(|x| x.abs() > 1e-9).unwrap_or(false);
                if used {
                    f.insert(format!("{side}_rest_days"), s["rest_days"].clone());
                    f.insert(format!("{side}_tz_shift"), s["tz_shift"].clone());
                    f.insert(format!("{side}_schedule_margin"), json!(r1(s["own_points"].as_f64().unwrap_or(0.0) - s["opponent_points"].as_f64().unwrap_or(0.0))));
                }
                if let Some(g) = tm["lineup_impact"]["gap"].as_f64() { if g.abs() >= 0.5 { f.insert(format!("{side}_lineup_gap"), json!(r1(g))); } }
            }
            for (k, t) in [("home_form", &home), ("away_form", &away)] {
                if let Some(v) = form.get(t) {
                    let w = v.iter().filter(|(a, b)| a > b).count();
                    let n = v.len() as f64;
                    f.insert(k.into(), json!({ "games": v.len(), "w": w, "l": v.len() - w, "pf": r1(v.iter().map(|x| x.0).sum::<f64>() / n), "pa": r1(v.iter().map(|x| x.1).sum::<f64>() / n) }));
                }
            }
        }
        if c.edge > 0.15 { flags.push(json!({ "code": "edge_unusually_large", "text": "the market rarely misprices by this much; check injuries and late news" })); }
        if c.w.market_source == "default" || c.w.market_source == "lines_partial" {
            flags.push(json!({ "code": "blend_unfitted", "text": format!("how far to trust the model over the market is {}", blend::source_text(&c.w)) }));
        }
        if c.lineup == "projected" { flags.push(json!({ "code": "projected_lineup", "text": "priced on projected lineups; confirmed lineups re-price closer to the start" })); }
        if c.books <= 1 { flags.push(json!({ "code": "single_book", "text": "only one book quotes this line" })); }
        if (Utc::now() - c.q.observed).num_minutes() > 90 { flags.push(json!({ "code": "stale_price", "text": format!("price last seen {} minutes ago", (Utc::now() - c.q.observed).num_minutes()) })); }
        if c.priced.push > 0.05 { flags.push(json!({ "code": "push_risk", "text": format!("{}% chance of a push at this line", pct(c.priced.push)) })); }
        if let (Some(o), Some(lnow)) = (c.q.open_line, c.q.a_line) {
            let against = match c.side.as_str() { "over" => lnow > o, "under" => lnow < o, _ => lnow < o };
            if (lnow - o).abs() > 1e-9 && against { flags.push(json!({ "code": "line_moved_against", "text": format!("line moved from {} to {} since open, against this side", fmt_num(o), fmt_num(lnow)) })); }
        }
        if c.tier == "losing" || c.tier == "unproven" { flags.push(json!({ "code": format!("{}_model", c.tier), "text": evidence_sentence(sport, c.tier, &c.evd, &c.key.1) })); }
        let facts = Value::Object(f);
        let tpl = template(&facts);
        let (writeup, wsrc) = (tpl.clone(), "template".to_string());
        let start = starts.get(&c.key.0).copied();
        if start.map(|s| (s - Utc::now()).num_minutes() < min_lead).unwrap_or(false) { too_late += 1; continue; }
        let head = c.q.provider_subject.as_ref().and_then(|p| heads.get(p).cloned());
        let n = db.execute("INSERT INTO ai_picks (sport, engine, model_version, ledger_id, game_id, game_label, home, away, starts_at, kind, market, subject, subject_name, subject_team, headshot, side, \
              line, book, price, link, p_model, p_market, edge, ev, projection, grade, tier, facts, risk_flags, writeup, writeup_source, \
              latest_line, latest_price, latest_book, latest_p_market, latest_edge, stake, fair_price, playable_price) \
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,$27,$28,$29,$30,$31,$17,$19,$18,$22,$23,$32,$33,$34) \
            ON CONFLICT (sport, game_id, market, subject, side, engine) DO NOTHING",
            &[&sport, &c.engine, &c.version, &c.ledger_id, &c.key.0, &format!("{away} @ {home}"), &home, &away, &start, &kind, &c.key.1, &c.key.2,
              &subject_name, &subject_team, &head, &c.side, &c.q.a_line, &c.q.book, &c.q.a_price, &c.q.a_link, &pm, &c.p_market, &c.edge, &c.ev,
              &c.priced.projection, &grade, &c.tier, &facts, &Value::Array(flags), &writeup, &wsrc, &stake, &fair_price, &playable_price]).await.map_err(|e| dberr(&e))?;
        published += n;
        let _ = &label;
    }
    // picks whose edge disappeared still follow the market
    for c in cands.iter().filter(|c| !qualifies(c)) {
        updated += db.execute("UPDATE ai_picks SET latest_line=$1, latest_price=$2, latest_book=$3, latest_p_market=$4, latest_edge=$5, updated_at=now() \
            WHERE sport=$6 AND game_id=$7 AND market=$8 AND subject=$9 AND side=$10 AND engine=$11 AND status='open'",
            &[&c.q.a_line, &c.q.a_price, &c.q.book, &c.p_market, &c.edge, &sport, &c.key.0, &c.key.1, &c.key.2, &c.side, &c.engine]).await.map_err(|e| dberr(&e))?;
    }
    let _ = by_selection;
    let rewritten = rewrite_writeups(state, sport).await;
    Ok(json!({ "sport": sport, "quotes": rows.len(), "selections_priced": considered, "lines_recorded": recorded, "qualifying": keep.len(), "published": published, "updated": updated,
        "suppressed": { "implausible_edge": implausible, "rotation_mismatch": rotation_mismatch, "outlier_quotes": outliers, "quarantined_feeds": quarantined.len(),
                        "too_close_to_start": too_late, "min_lead_minutes": min_lead }, "regraded_or_voided": withdrawn, "publication_suspended": suspended, "llm_rewritten": rewritten,
        "orientation_check": { "checked": checked, "agree": agree, "ok": orientation_ok } }))
}

/// Open picks published before price sensitivity existed: add the fair and
/// playable prices, and re-draft template writeups so they carry the case against.
async fn backfill_playable(db: &tokio_postgres::Client, sport: &str, min_ev: f64) -> Result<(), String> {
    let rows = db.query("SELECT id, p_model, price, facts, writeup_source FROM ai_picks WHERE sport=$1 AND status='open' AND playable_price IS NULL", &[&sport])
        .await.map_err(|e| dberr(&e))?;
    for r in &rows {
        let (id, pm, price0, facts, wsrc): (i64, f64, i32, Json<Value>, String) = (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4));
        let decided = 1.0 - facts.0["p_push"].as_f64().unwrap_or(0.0);
        let (win, loss) = (pm * decided, (1.0 - pm) * decided);
        let fair = price_for_ev(win, loss, 0.0);
        let playable = playable_floor(win, loss, min_ev, price0);
        let mut f = facts.0.clone();
        f["fair_price"] = json!(fair);
        f["playable_price"] = json!(playable);
        f["playable_min_ev"] = json!(min_ev);
        let writeup = if wsrc == "template" { Some(template(&f)) } else { None };
        db.execute("UPDATE ai_picks SET fair_price=$2, playable_price=$3, facts=$4, writeup=COALESCE($5, writeup) WHERE id=$1",
            &[&id, &fair, &playable, &f, &writeup]).await.map_err(|e| dberr(&e))?;
    }
    Ok(())
}

/// Record every selection both champion engines can price at its main line (the
/// line most books quote), oriented over / home, for blend fitting (blend.rs).
async fn record_lines(db: &tokio_postgres::Client, sport: &str, quotes: &HashMap<((String, String, String), String), Vec<Quote>>,
    ledgers: &HashMap<&str, HashMap<String, Ledger>>, champs: &HashMap<String, (String, String)>, starts_at: &HashMap<String, DateTime<Utc>>,
    naive_over: &dyn Fn(&str, &str, Option<f64>) -> Option<f64>) -> Result<u64, String> {
    let mut n = 0;
    for (((game, market, subject), side), qs) in quotes {
        let kind_prop = subject != "game";
        if side != if segments::home_away(market) { "home" } else { "over" } { continue; }
        // main line
        let mut count: Vec<(Option<f64>, usize)> = Vec::new();
        for q in qs { match count.iter_mut().find(|(l, _)| *l == q.a_line) { Some(x) => x.1 += 1, None => count.push((q.a_line, 1)) } }
        count.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)));
        let Some((line, books)) = count.first().copied() else { continue };
        let same: Vec<f64> = qs.iter().filter(|x| x.a_line == line).map(|x| novig(x.a_price, x.b_price)).collect();
        let p_market = same.iter().sum::<f64>() / same.len().max(1) as f64;
        let starts = starts_at.get(game).copied();
        for (engine, by_game) in ledgers {
            let Some(l) = by_game.get(game) else { continue };
            if kind_prop && engine.ends_with("_team") { continue; }
            let Some(p) = price(&l.payload, engine.ends_with("_team"), market, subject, side, line) else { continue };
            if kind_prop && p.p_active.map(|a| a < 0.5).unwrap_or(false) { continue; }
            let code = champs.get(*engine).map(|c| c.1.as_str()).unwrap_or("");
            let p_naive = if kind_prop { naive_over(subject, market, line) } else { None };
            n += blend::record(db, &blend::LineEval { engine, engine_version: code, sport, game_id: game, market, subject, line: line.unwrap_or(0.0),
                starts_at: starts, ledger_id: l.id, p_model: p.p(), p_naive, p_market, books: books as i32 }).await?;
        }
    }
    Ok(n)
}

/// MonsterGPT rewrites of open picks still on the template (best grades first),
/// when the writeup model is reachable. Returns how many were rewritten.
async fn rewrite_writeups(state: &State, sport: &str) -> u64 {
    if !llm_reachable(state).await { return 0; }
    let n = lines::setting(&state.db, "llm_per_cycle", 12.0).await as i64;
    let Ok(rows) = state.db.query("SELECT id, facts, writeup FROM ai_picks WHERE sport=$1 AND status='open' AND starts_at > now() AND writeup_source='template'         ORDER BY (tier IN ('proven','baseline')) DESC, array_position(ARRAY['A+','A','A-','B+','B','B-','C+','C'], grade), edge DESC LIMIT $2", &[&sport, &n]).await else { return 0 };
    let mut done = 0;
    for r in &rows {
        let (id, facts, tpl): (i64, Json<Value>, String) = (r.get(0), r.get(1), r.get(2));
        if let Some((text, src)) = llm_writeup(state, &facts.0, &tpl).await {
            if state.db.execute("UPDATE ai_picks SET writeup=$2, writeup_source=$3 WHERE id=$1", &[&id, &text, &src]).await.is_ok() { done += 1; }
        }
    }
    done
}

pub(crate) fn book_title(b: &str) -> String {
    match b {
        "draftkings" => "DraftKings", "fanduel" => "FanDuel", "betmgm" => "BetMGM", "caesars" | "williamhillus" => "Caesars", "espnbet" => "ESPN BET",
        "betrivers" => "BetRivers", "pinnacle" => "Pinnacle", "bovada" => "Bovada", "betonlineag" | "betonline" => "BetOnline", "fanatics" => "Fanatics",
        "hardrockbet" => "Hard Rock", "ballybet" => "Bally Bet", "bet365" => "bet365", "mybookieag" => "MyBookie", "lowvig" => "LowVig", "betus" => "BetUS",
        other => return other.to_string(),
    }.to_string()
}

// ---------------------------------------------------------------------------
// Settle + live evidence
// ---------------------------------------------------------------------------

/// Final player stats for settling props; None until the game is final.
pub(crate) async fn prop_actuals(db: &tokio_postgres::Client, sport: &str, game: &str) -> Result<Option<HashMap<String, HashMap<&'static str, f64>>>, String> {
    let mut out: HashMap<String, HashMap<&'static str, f64>> = HashMap::new();
    match sport {
        "nba" => {
            let done = db.query_opt("SELECT 1 FROM nba_games_espn WHERE espn_id=$1 AND completed", &[&game]).await.map_err(|e| dberr(&e))?.is_some();
            if !done { return Ok(None); }
            for r in db.query("SELECT player_id, pts, COALESCE(oreb,0)+COALESCE(dreb,0), ast, fg3m, stl, blk, tov, fgm, fga, fg3a, ftm, fta, oreb, dreb FROM nba_box_espn WHERE espn_id=$1 AND NOT dnp AND COALESCE(min,0) > 0", &[&game]).await.map_err(|e| dberr(&e))? {
                let g = |i: usize| r.get::<_, Option<i32>>(i).unwrap_or(0) as f64;
                let reb = r.get::<_, i32>(2) as f64;
                let tens = [g(1), reb, g(3), g(5), g(6)].iter().filter(|x| **x >= 10.0).count();
                out.insert(r.get(0), [("pts", g(1)), ("reb", reb), ("ast", g(3)), ("fg3m", g(4)), ("stl", g(5)), ("blk", g(6)), ("tov", g(7)), ("pra", g(1) + reb + g(3)),
                    ("pr", g(1) + reb), ("pa", g(1) + g(3)), ("ra", reb + g(3)), ("stocks", g(5) + g(6)), ("fgm", g(8)), ("fga", g(9)), ("fg3a", g(10)),
                    ("ftm", g(11)), ("fta", g(12)), ("oreb", g(13)), ("dreb", g(14)),
                    ("double_double", (tens >= 2) as i32 as f64), ("triple_double", (tens >= 3) as i32 as f64)].into());
            }
        }
        "nhl" => {
            let Ok(gid) = game.parse::<i64>() else { return Ok(None) };
            let done = db.query_opt("SELECT 1 FROM nhl_games_api WHERE game_id=$1 AND completed", &[&gid]).await.map_err(|e| dberr(&e))?.is_some();
            if !done { return Ok(None); }
            for r in db.query("SELECT player_id, sog, goals, assists, blocks, saves FROM nhl_box_api WHERE game_id=$1 AND COALESCE(toi_sec,0) > 0", &[&gid]).await.map_err(|e| dberr(&e))? {
                let g = |i: usize| r.get::<_, Option<i32>>(i).unwrap_or(0) as f64;
                out.insert(r.get::<_, i64>(0).to_string(), [("sog", g(1)), ("goals", g(2)), ("assists", g(3)), ("points", g(2) + g(3)), ("blocks", g(4)), ("saves", g(5))].into());
            }
        }
        "nfl" => {
            let Some(o) = crate::learning::game_outcomes(db, "nfl", game).await? else { return Ok(None) };
            for (id, m) in o.players {
                let mut x: HashMap<&'static str, f64> = HashMap::new();
                for k in NFL_PROPS.iter().chain(["fgm", "kick_pts"].iter()) { if let Some(v) = m.get(*k) { x.insert(*k, *v); } }
                out.insert(id, x);
            }
        }
        _ => return Ok(None),
    }
    if out.is_empty() { return Ok(None); }
    Ok(Some(out))
}

pub(crate) fn result_of(market: &str, side: &str, line: Option<f64>, actual: f64) -> &'static str {
    // actual = stat value, or the home margin for moneyline/spread, or the total
    let market = if segments::is_game_market(market) { segments::base_market(market) } else { market };
    let diff = match (market, side) {
        ("moneyline", "home") => actual,
        ("moneyline", _) => -actual,
        ("spread", "home") => actual + line.unwrap_or(0.0),
        ("spread", _) => -actual + line.unwrap_or(0.0),
        (_, "over") => actual - line.unwrap_or(0.0),
        _ => line.unwrap_or(0.0) - actual,
    };
    if diff.abs() < 1e-9 { "push" } else if diff > 0.0 { "win" } else { "loss" }
}

pub(crate) async fn settle(state: &State, sport: &str) -> Result<Value, String> {
    let db = &state.db;
    db.execute("UPDATE ai_picks SET status='started' WHERE sport=$1 AND status='open' AND starts_at <= now()", &[&sport]).await.map_err(|e| dberr(&e))?;
    let open = db.query("SELECT id, game_id, market, subject, side, line, price, book, p_market, starts_at FROM ai_picks WHERE sport=$1 AND status='started'", &[&sport]).await.map_err(|e| dberr(&e))?;
    let mut by_game: BTreeMap<String, Vec<&tokio_postgres::Row>> = BTreeMap::new();
    for r in &open { by_game.entry(r.get(1)).or_default().push(r); }
    let (mut settled, mut pending) = (0, 0);
    for (game, picks) in by_game {
        let go = crate::learning::game_outcomes(db, sport, &game).await?;
        let props = if picks.iter().any(|r| r.get::<_, String>(3) != "game") { prop_actuals(db, sport, &game).await? } else { None };
        let Some(go) = go else { pending += 1; continue };
        for r in picks {
            let (id, market, subject, side, line, price, book, p_mkt, start): (i64, String, String, String, Option<f64>, i32, String, f64, Option<DateTime<Utc>>) =
                (r.get(0), r.get(2), r.get(3), r.get(4), r.get(5), r.get(6), r.get(7), r.get(8), r.get(9));
            let actual = if subject == "game" {
                go.game.get(segments::outcome_key(&market)).copied()
            } else {
                match &props { Some(p) => p.get(&subject).and_then(|m| m.get(market.as_str()).copied()), None => { continue } }
            };
            let res = actual.map(|a| result_of(&market, &side, line, a)).unwrap_or("void");
            let profit = match res { "win" => decimal(price) - 1.0, "loss" => -1.0, _ => 0.0 };
            // closing price at the same book: the last snapshot of both sides before the start
            let close = db.query("SELECT DISTINCT ON (side) side, line, price FROM market_lines WHERE sport=$1 AND game_id=$2 AND market=$3 AND subject=$4 AND book=$5 \
                AND side = ANY($6) AND observed_at <= COALESCE($7, now()) ORDER BY side, observed_at DESC",
                &[&sport, &game, &market, &subject, &book, &vec![side.clone(), other(&side).to_string()], &start]).await.map_err(|e| dberr(&e))?;
            let mine = close.iter().find(|c| c.get::<_, String>(0) == side);
            let theirs = close.iter().find(|c| c.get::<_, String>(0) != side);
            let (cl, cp) = (mine.and_then(|c| c.get::<_, Option<f64>>(1)), mine.map(|c| c.get::<_, i32>(2)));
            let cpm = match (mine, theirs) { (Some(a), Some(b)) => Some(novig(a.get(2), b.get(2))), _ => None };
            let clv = match (cpm, cl == line) { (Some(c), true) => Some(c - p_mkt), _ => None };
            db.execute("UPDATE ai_picks SET status=$2, actual=$3, result=$4, profit=$5, close_line=$6, close_price=$7, close_p_market=$8, clv=$9, settled_at=now() WHERE id=$1",
                &[&id, &if res == "void" { "void" } else { "settled" }, &actual, &res, &profit, &cl, &cp, &cpm, &clv]).await.map_err(|e| dberr(&e))?;
            settled += 1;
        }
    }
    let ev = refresh_evidence(db, sport).await?;
    let lines_settled = blend::settle(db, sport).await?;
    let lines_ev = blend::refresh_evidence(db, sport).await?;
    Ok(json!({ "sport": sport, "settled": settled, "games_pending": pending, "evidence_rows": ev, "lines_settled": lines_settled, "lines_evidence_rows": lines_ev }))
}

/// Live evidence per (engine, market) from settled picks: did the model's
/// probability or the market's predict the result better (paired Brier)?
async fn refresh_evidence(db: &tokio_postgres::Client, sport: &str) -> Result<u64, String> {
    let rows = db.query("SELECT engine, market, split_part(model_version, '+', 1), p_model, p_market, result, profit, clv FROM ai_picks \
        WHERE sport=$1 AND status='settled' AND result IN ('win','loss') AND NOT EXISTS (SELECT 1 FROM feed_quarantine q WHERE q.sport=ai_picks.sport AND q.market=ai_picks.market AND q.book=ai_picks.book)", &[&sport]).await.map_err(|e| dberr(&e))?;
    let mut g: BTreeMap<(String, String, String), Vec<(f64, f64, f64, f64, Option<f64>)>> = BTreeMap::new();
    for r in &rows {
        let y = if r.get::<_, String>(5) == "win" { 1.0 } else { 0.0 };
        g.entry((r.get(0), r.get(1), r.get(2))).or_default().push((r.get(3), r.get(4), y, r.get::<_, Option<f64>>(6).unwrap_or(0.0), r.get(7)));
    }
    let mut n_rows = 0;
    for ((engine, market, code), v) in g {
        let d: Vec<f64> = v.iter().map(|(pm, pk, y, _, _)| (pm - y).powi(2) - (pk - y).powi(2)).collect();
        let n = d.len() as f64;
        let mean = d.iter().sum::<f64>() / n;
        let se = if n > 1.0 { (d.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0) / n).sqrt() } else { 0.0 };
        let roi = v.iter().map(|x| x.3).sum::<f64>() / n;
        let clvs: Vec<f64> = v.iter().filter_map(|x| x.4).collect();
        let wins = v.iter().filter(|x| x.2 > 0.5).count();
        let detail = json!({ "engine_version": code, "roi": r3(roi), "wins": wins, "losses": v.len() - wins,
            "clv_mean": if clvs.is_empty() { Value::Null } else { json!(r3(clvs.iter().sum::<f64>() / clvs.len() as f64)) }, "clv_n": clvs.len() });
        n_rows += db.execute("INSERT INTO pick_evidence (engine, market, source, comparison, n, metric, se, detail, updated_at) VALUES ($1,$2,'live','market',$3,$4,$5,$6,now()) \
            ON CONFLICT (engine, market, source, comparison) DO UPDATE SET n=EXCLUDED.n, metric=EXCLUDED.metric, se=EXCLUDED.se, detail=EXCLUDED.detail, updated_at=now()",
            &[&engine, &market, &(v.len() as i32), &mean, &se, &detail]).await.map_err(|e| dberr(&e))?;
    }
    Ok(n_rows)
}

/// The cycle the autopilot runs: lines (when due) -> build -> settle.
pub(crate) async fn run(state: &State, sport: &str, force: bool) -> Value {
    let l = lines::refresh(state, sport, force).await;
    let b = match build(state, sport).await { Ok(v) => v, Err(e) => json!({ "error": e }) };
    let e = match crate::sharp::scan(state, sport).await { Ok(v) => v, Err(e) => json!({ "error": e }) };
    let s = match settle(state, sport).await { Ok(v) => v, Err(e) => json!({ "error": e }) };
    let es = match crate::sharp::settle(state, sport).await { Ok(v) => v, Err(e) => json!({ "error": e }) };
    let ok = l["ok"].as_bool().unwrap_or(false) && [&b, &e, &s, &es].iter().all(|v| v.get("error").is_none());
    json!({ "ok": ok, "sport": sport, "lines": l, "build": b, "stale_lines": e, "settle": s, "stale_settle": es })
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RunQ { sport: Option<String>, #[serde(default)] force: bool }

pub async fn run_http(state: web::Data<State>, q: web::Query<RunQ>) -> impl Responder {
    let sports: Vec<String> = match &q.sport { Some(s) if SPORTS.contains(&s.as_str()) => vec![s.clone()], Some(_) => return HttpResponse::BadRequest().json(json!({ "error": "sport must be nba, nhl, nfl or mlb" })), None => SPORTS.iter().map(|s| s.to_string()).collect() };
    let mut out = Vec::new();
    for s in sports { out.push(run(&state, &s, q.force).await); }
    HttpResponse::Ok().json(json!({ "runs": out }))
}

#[derive(Deserialize)]
pub struct ListQ {
    sport: Option<String>, kind: Option<String>, side: Option<String>, grades: Option<String>, market: Option<String>, game: Option<String>,
    book: Option<String>, min_edge: Option<f64>, min_prob: Option<f64>, sort: Option<String>, q: Option<String>,
    status: Option<String>, limit: Option<i64>, qualification: Option<String>,
    #[serde(default)] include_held: bool,
}

const PICK_COLS: &str = "id, sport, engine, model_version, ledger_id, game_id, game_label, home, away, starts_at, kind, market, subject, subject_name, subject_team, headshot, side, \
    line, book, price, link, p_model, p_market, edge, ev, projection, grade, tier, risk_flags, writeup, writeup_source, published_at, latest_line, latest_price, latest_book, \
    latest_p_market, latest_edge, updated_at, status, close_line, close_price, clv, actual, result, profit, settled_at, stake, fair_price, playable_price";

/// Can the pick still be played? At the published line: is the latest price at
/// least the playable price. After a line move the old price floor says nothing,
/// so null (re-check).
fn playable_now(status: &str, line: Option<f64>, price: i32, playable: Option<i32>, latest_line: Option<f64>, latest_price: Option<i32>) -> Option<bool> {
    if status != "open" { return None; }
    let floor = playable?;
    match latest_price {
        None => Some(at_least(price, floor)),
        Some(_) if latest_line != line => None,
        Some(p) => Some(at_least(p, floor)),
    }
}

fn pick_json(r: &tokio_postgres::Row, full: bool) -> Value {
    let (market, side, line, home, away): (String, String, Option<f64>, Option<String>, Option<String>) = (r.get("market"), r.get("side"), r.get("line"), r.get("home"), r.get("away"));
    let sport: String = r.get("sport");
    let label = pick_label(&sport, &market, &side, line, home.as_deref().unwrap_or(""), away.as_deref().unwrap_or(""));
    let flags: Json<Value> = r.get("risk_flags");
    let book: String = r.get("book");
    let mut v = json!({
        "id": r.get::<_, i64>("id"), "sport": r.get::<_, String>("sport"), "engine": r.get::<_, String>("engine"), "model_version": r.get::<_, String>("model_version"),
        "ledger_id": r.get::<_, i64>("ledger_id"), "game_id": r.get::<_, String>("game_id"), "game_label": r.get::<_, String>("game_label"), "home": home, "away": away,
        "starts_at": r.get::<_, Option<DateTime<Utc>>>("starts_at"), "kind": r.get::<_, String>("kind"), "market": market,
        "market_label": if segments::is_game_market(&market) { segments::market_label(&sport, &market) } else { stat_label(&market).to_string() },
        "subject": r.get::<_, String>("subject"), "subject_name": r.get::<_, String>("subject_name"), "subject_team": r.get::<_, Option<String>>("subject_team"),
        "headshot": r.get::<_, Option<String>>("headshot"), "side": side, "label": label, "line": line, "book": book, "book_title": book_title(&book),
        "price": r.get::<_, i32>("price"), "link": r.get::<_, Option<String>>("link"),
        "p_model": r3(r.get("p_model")), "p_market": r3(r.get("p_market")), "edge": r3(r.get("edge")), "ev": r3(r.get("ev")),
        "projection": r.get::<_, Option<f64>>("projection").map(r1), "grade": r.get::<_, String>("grade"), "tier": r.get::<_, String>("tier"),
        "risk_flags": flags.0, "writeup": r.get::<_, String>("writeup"), "writeup_source": r.get::<_, String>("writeup_source"),
        "published_at": r.get::<_, DateTime<Utc>>("published_at"),
        "latest": { "line": r.get::<_, Option<f64>>("latest_line"), "price": r.get::<_, Option<i32>>("latest_price"), "book": r.get::<_, Option<String>>("latest_book"),
            "p_market": r.get::<_, Option<f64>>("latest_p_market").map(r3), "edge": r.get::<_, Option<f64>>("latest_edge").map(r3), "updated_at": r.get::<_, DateTime<Utc>>("updated_at") },
        "status": r.get::<_, String>("status"), "result": r.get::<_, Option<String>>("result"), "actual": r.get::<_, Option<f64>>("actual"),
        "profit": r.get::<_, Option<f64>>("profit").map(r3), "clv": r.get::<_, Option<f64>>("clv").map(r3), "stake": r.get::<_, Option<f64>>("stake"),
        "close": { "line": r.get::<_, Option<f64>>("close_line"), "price": r.get::<_, Option<i32>>("close_price") }, "settled_at": r.get::<_, Option<DateTime<Utc>>>("settled_at"),
    });
    let (fair, playable): (Option<i32>, Option<i32>) = (r.get("fair_price"), r.get("playable_price"));
    v["fair_price"] = json!(fair);
    v["playable_price"] = json!(playable);
    v["playable_now"] = json!(playable_now(&r.get::<_, String>("status"), line, r.get("price"), playable, r.get("latest_line"), r.get("latest_price")));
    let _ = full;
    if v["latest"]["line"].is_null() { v["latest"] = Value::Null; }
    v
}

/// WHERE clauses with positional parameters ('?' -> $n; repeated '?' reuse the same one).
#[derive(Default)]
struct Filters { w: Vec<String>, p: Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>> }
impl Filters {
    fn add(&mut self, cond: &str, v: Box<dyn tokio_postgres::types::ToSql + Sync + Send>) { self.p.push(v); self.w.push(cond.replace('?', &format!("${}", self.p.len()))); }
    fn raw(&mut self, cond: &str) { self.w.push(cond.to_string()); }
}

pub async fn list(state: web::Data<State>, q: web::Query<ListQ>) -> impl Responder {
    let mut f = Filters::default();
    match q.status.as_deref().unwrap_or("open") {
        "open" => f.raw("status='open' AND starts_at > now()"),
        "settled" => f.raw("status IN ('settled','void')"),
        "all" => {}
        s => f.add("status = ?", Box::new(s.to_string())),
    }
    if let Some(s) = &q.sport { if s != "all" { f.add("sport = ?", Box::new(s.clone())); } }
    if let Some(k) = &q.kind { if k == "prop" || k == "game" { f.add("kind = ?", Box::new(k.clone())); } }
    if let Some(s) = &q.side { if s == "over" || s == "under" { f.add("side = ?", Box::new(s.clone())); } }
    if let Some(m) = &q.market { if !m.is_empty() && m != "all" { f.add("market = ?", Box::new(m.clone())); } }
    if let Some(g) = &q.game { if !g.is_empty() && g != "all" { f.add("game_id = ?", Box::new(g.clone())); } }
    if let Some(b) = &q.book { if !b.is_empty() && b != "all" { f.add("book = ?", Box::new(b.clone())); } }
    if let Some(e) = q.min_edge { f.add("edge >= ?", Box::new(e)); }
    if let Some(m) = q.min_prob { f.add("p_model >= ?", Box::new(m)); }
    if let Some(s) = &q.q { if !s.trim().is_empty() { f.add("(subject_name ILIKE ? OR game_label ILIKE ?)", Box::new(format!("%{}%", s.trim()))); } }
    // every tier is visible (graded on price); an old include_hidden parameter is ignored
    if !q.include_held { f.raw(&crate::governance::public_filter()); }
    let (w, p) = (f.w, f.p);
    let order = match q.sort.as_deref().unwrap_or("grade") {
        "edge" => "edge DESC", "ev" => "ev DESC", "prob" => "p_model DESC", "odds" => "price DESC", "start" => "starts_at ASC",
        // the grade shown is the price grade (EV at the current price), computed per pick below:
        // fetch the newest picks and sort them after
        _ => "published_at DESC",
    };
    let sql = format!("SELECT {PICK_COLS} FROM ai_picks {} ORDER BY {order} LIMIT {}", if w.is_empty() { String::new() } else { format!("WHERE {}", w.join(" AND ")) }, q.limit.unwrap_or(300).clamp(1, 1000));
    let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = p.iter().map(|b| b.as_ref() as &(dyn tokio_postgres::types::ToSql + Sync)).collect();
    match state.db.query(&sql, &params).await {
        Ok(rows) => {
            let st = crate::validation::statuses(&state).await;
            let mut picks: Vec<Value> = rows.iter().map(|r| { let mut v = pick_json(r, false); qualify(&mut v, &st); v }).collect();
            if let Some(want) = q.qualification.as_deref().filter(|s| !s.is_empty() && *s != "all") {
                let want: Vec<&str> = want.split(',').collect();
                picks.retain(|p| p["qualification"].as_str().map(|s| want.contains(&s)).unwrap_or(false));
            }
            // grades are price grades (EV at the current price); the track-record cap stays on the
            // stored `grade` for the public filter, stakes and ranking elsewhere
            let price_grade = |p: &Value| p["evaluation"]["price"]["grade"].as_str().unwrap_or("F").to_string();
            if let Some(g) = q.grades.as_deref().filter(|g| !g.trim().is_empty()) {
                let want: Vec<&str> = g.split(',').map(|x| x.trim()).collect();
                picks.retain(|p| want.contains(&price_grade(p).as_str()));
            }
            if matches!(q.sort.as_deref(), None | Some("grade") | Some("price")) {
                let ev = |p: &Value| p["ev_now"].as_f64().unwrap_or(f64::MIN);
                picks.sort_by(|a, b| ev(b).total_cmp(&ev(a)));
            }
            HttpResponse::Ok().json(json!({ "count": picks.len(), "picks": picks }))
        }
        Err(e) => internal(dberr(&e)),
    }
}

/// Two grades and a qualification status from the validation scorecard: model
/// quality (is the engine reliable on this market?) and opportunity (is the
/// current price worth it?), kept apart from the combined `grade`.
fn qualify(v: &mut Value, st: &HashMap<(String, String), (String, String)>) {
    let key = (v["engine"].as_str().unwrap_or("").to_string(), v["market"].as_str().unwrap_or("").to_string());
    let (quality, status) = st.get(&key).cloned().unwrap_or_else(|| ("I".into(), "insufficient_data".into()));
    // EV at the latest price: at the published line with the published probability; after a line
    // move with the probability at the new line (latest edge + latest market probability)
    let l = &v["latest"];
    let same_line = l["line"].as_f64().or(v["line"].as_f64()) == v["line"].as_f64();
    let ev_at = |pm: f64, p: i64| pm * (decimal(p as i32) - 1.0) - (1.0 - pm);
    let ev_now = match (l["price"].as_i64(), same_line, l["edge"].as_f64(), l["p_market"].as_f64()) {
        (Some(p), true, _, _) => ev_at(v["p_model"].as_f64().unwrap_or(0.0), p),
        (Some(p), false, Some(e), Some(m)) => ev_at(e + m, p),
        _ => v["ev"].as_f64().unwrap_or(0.0),
    };
    let feed_bad = st.contains_key(&(format!("q:{}", v["sport"].as_str().unwrap_or("")), format!("{}|{}", key.1, v["book"].as_str().unwrap_or(""))));
    let held = st.contains_key(&(format!("h:{}", key.0), key.1.clone()));
    if held { v["publication_hold"] = json!(true); }
    // news circuit breaker (injury_snap.rs): a player's status changed after the price was set
    let news = v["risk_flags"].as_array().map(|a| a.iter().any(|f| f["code"] == "news_changed")).unwrap_or(false);
    if news { v["news_changed"] = json!(true); }
    let suspended = v["status"] != "open" || v["playable_now"] == json!(false) || feed_bad || held || news;
    if feed_bad { v["feed_quarantined"] = json!(true); }
    v["model_quality"] = json!(quality);
    v["validation_status"] = json!(status);
    v["opportunity_grade"] = json!(crate::validation::opportunity_grade(ev_now));
    v["ev_now"] = json!(r3(ev_now));
    v["qualification"] = json!(crate::validation::qualification(&status, ev_now, suspended, v["edge"].as_f64().unwrap_or(0.0)));
    v["evaluation"] = evaluation(v, Utc::now());
}

/// The four steps of evaluating a pick (the "How it works" tab): a current
/// projection, its probability against the price, the case and the trust behind
/// it, and what to recheck before acting. The price grade follows the current
/// price; the official `grade` stays capped by the market's track record.
pub(crate) fn evaluation(v: &Value, now: DateTime<Utc>) -> Value {
    let f = |k: &str| v[k].as_f64();
    let latest = &v["latest"];
    let flag = |c: &str| v["risk_flags"].as_array().map(|a| a.iter().any(|x| x["code"] == c)).unwrap_or(false);
    let open = v["status"] == "open";
    let time = |x: &Value| x.as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&Utc));
    let started = !open || time(&v["starts_at"]).map(|t| t <= now).unwrap_or(false);
    let moved = latest["line"].as_f64().is_some() && latest["line"].as_f64() != f("line");
    let price_now = latest["price"].as_i64().or(v["price"].as_i64()).unwrap_or(0) as i32;
    let ev_now = f("ev_now").unwrap_or_else(|| f("ev").unwrap_or(0.0));
    let seen = time(&latest["updated_at"]).or_else(|| time(&v["published_at"]));
    let seen_min = seen.map(|t| (now - t).num_minutes());
    let tier = v["tier"].as_str().unwrap_or("unproven");

    let mut checks = vec![
        json!({ "key": "not_started", "ok": !started, "text": if started { "The game has started: this is no longer a pick".to_string() } else { "The game has not started".to_string() } }),
        json!({ "key": "value_now", "ok": ev_now > 0.0, "text": format!("{} EV per unit at the current price", if ev_now >= 0.0 { format!("+{:.1}%", ev_now * 100.0) } else { format!("{:.1}%", ev_now * 100.0) }) }),
    ];
    match (v["playable_now"].as_bool(), moved) {
        (Some(ok), _) => checks.push(json!({ "key": "playable", "ok": ok, "text": if ok { format!("Best price still at or better than the playable {}", fmt_odds(v["playable_price"].as_i64())) }
            else { format!("Price now worse than the playable {}", fmt_odds(v["playable_price"].as_i64())) } })),
        (None, true) if open => checks.push(json!({ "key": "playable", "ok": null, "text": "The line moved since publishing: re-check the new number" })),
        _ => {}
    }
    if let Some(m) = seen_min { if open { checks.push(json!({ "key": "price_fresh", "ok": m <= 90, "text": format!("Price last seen {}", if m < 1 { "just now".to_string() } else { format!("{m} min ago") }) })); } }
    let news = flag("news_changed");
    checks.push(json!({ "key": "news", "ok": !news, "text": if news { "A player's injury status changed after the price was set" } else { "No injury status change since the price was set" } }));
    if flag("projected_lineup") { checks.push(json!({ "key": "lineup", "ok": false, "text": "Priced on projected lineups: confirm the lineup" })); }
    if v["feed_quarantined"] == json!(true) { checks.push(json!({ "key": "feed", "ok": false, "text": "This book's odds feed is quarantined for this market" })); }
    let ok = checks.iter().filter(|c| c["ok"] == json!(true)).count();
    let total = checks.len();

    json!({
        "projection": { "value": f("projection"), "line": f("line"), "engine": v["engine"], "model_version": v["model_version"], "ledger_id": v["ledger_id"], "published_at": v["published_at"] },
        "price": {
            // after a line move, the model's probability at the new line (latest edge + latest market probability)
            "p_model": match (moved, latest["edge"].as_f64(), latest["p_market"].as_f64()) { (true, Some(e), Some(m)) => Some(e + m), _ => f("p_model") },
            "p_market": latest["p_market"].as_f64().or(f("p_market")), "implied": implied(price_now),
            "price": price_now, "line": latest["line"].as_f64().or(f("line")), "edge": latest["edge"].as_f64().or(f("edge")), "ev": ev_now,
            "grade": crate::validation::opportunity_grade(ev_now), "moved": moved,
            "published": { "price": v["price"], "line": v["line"], "p_market": v["p_market"], "edge": v["edge"], "ev": v["ev"], "grade": crate::validation::opportunity_grade(f("ev").unwrap_or(0.0)) },
            "fair_price": v["fair_price"], "playable_price": v["playable_price"],
        },
        "trust": { "tier": tier, "cap": tier_cap(tier), "grade": v["grade"], "model_quality": v["model_quality"], "validation_status": v["validation_status"] },
        "checks": checks, "checks_ok": ok, "checks_total": total, "all_clear": ok == total,
    })
}

fn fmt_odds(p: Option<i64>) -> String { p.map(|p| if p > 0 { format!("+{p}") } else { p.to_string() }).unwrap_or_else(|| "price".into()) }

#[derive(Deserialize)]
pub struct GetQ { #[serde(default)] include_held: bool }

pub async fn get(state: web::Data<State>, path: web::Path<i64>, q: web::Query<GetQ>) -> impl Responder {
    let id = path.into_inner();
    let held = if q.include_held { "TRUE" } else { crate::governance::VISIBLE };
    let row = match state.db.query_opt(&format!("SELECT {PICK_COLS}, facts FROM ai_picks WHERE id=$1 AND {held}"), &[&id]).await {
        Ok(Some(r)) => r, Ok(None) => return HttpResponse::NotFound().json(json!({ "error": "no such pick" })), Err(e) => return internal(dberr(&e)),
    };
    let mut v = pick_json(&row, true);
    let st = crate::validation::statuses(&state).await;
    qualify(&mut v, &st);
    let facts: Json<Value> = row.get("facts");
    v["facts"] = facts.0;
    // every book's current quote for the same selection
    let books = state.db.query("SELECT DISTINCT ON (book, side) book, side, line, price, link, observed_at FROM market_lines \
        WHERE sport=$1 AND game_id=$2 AND market=$3 AND subject=$4 AND observed_at > now() - interval '6 hours' ORDER BY book, side, observed_at DESC",
        &[&v["sport"].as_str().unwrap_or(""), &v["game_id"].as_str().unwrap_or(""), &v["market"].as_str().unwrap_or(""), &v["subject"].as_str().unwrap_or("")]).await;
    if let Ok(b) = books {
        v["books"] = json!(b.iter().map(|r| json!({ "book": r.get::<_, String>(0), "book_title": book_title(&r.get::<_, String>(0)), "side": r.get::<_, String>(1),
            "line": r.get::<_, Option<f64>>(2), "price": r.get::<_, i32>(3), "link": r.get::<_, Option<String>>(4), "observed_at": r.get::<_, DateTime<Utc>>(5) })).collect::<Vec<_>>());
    }
    HttpResponse::Ok().json(v)
}

#[derive(Deserialize)]
pub struct RecordQ { sport: Option<String>, days: Option<i32> }

pub async fn record(state: web::Data<State>, q: web::Query<RecordQ>) -> impl Responder {
    let days = q.days.unwrap_or(90).clamp(1, 3650);
    let sport = q.sport.clone().filter(|s| s != "all");
    let rows = match state.db.query(
        "SELECT sport, kind, market, tier, grade, count(*) FILTER (WHERE result IN ('win','loss','push')), count(*) FILTER (WHERE result='win'), count(*) FILTER (WHERE result='loss'), \
           COALESCE(sum(profit) FILTER (WHERE result IN ('win','loss','push')),0), avg(clv), count(clv), \
           avg(power(p_model - (result='win')::int, 2) - power(p_market - (result='win')::int, 2)) FILTER (WHERE result IN ('win','loss')), \
           sum(stake * profit) FILTER (WHERE result IN ('win','loss','push')), sum(stake) FILTER (WHERE result IN ('win','loss','push')) \
         FROM ai_picks WHERE status IN ('settled','void') AND settled_at > now() - make_interval(days => $1) AND ($2::text IS NULL OR sport=$2) AND NOT EXISTS (SELECT 1 FROM feed_quarantine q WHERE q.sport=ai_picks.sport AND q.market=ai_picks.market AND q.book=ai_picks.book) \
         GROUP BY ROLLUP (sport, kind, market, tier, grade) ORDER BY 1,2,3,4,5", &[&days, &sport]).await {
        Ok(r) => r, Err(e) => return internal(dberr(&e)),
    };
    let out: Vec<Value> = rows.iter().map(|r| {
        let (n, w, l, profit): (i64, i64, i64, f64) = (r.get(5), r.get(6), r.get(7), r.get(8));
        json!({ "sport": r.get::<_, Option<String>>(0), "kind": r.get::<_, Option<String>>(1), "market": r.get::<_, Option<String>>(2), "tier": r.get::<_, Option<String>>(3),
            "grade": r.get::<_, Option<String>>(4), "graded": n, "wins": w, "losses": l, "pushes": n - w - l, "units": r3(profit),
            "roi": if n > 0 { json!(r3(profit / n as f64)) } else { Value::Null }, "win_rate": if w + l > 0 { json!(r3(w as f64 / (w + l) as f64)) } else { Value::Null },
            "clv_mean": r.get::<_, Option<f64>>(9).map(r3), "clv_n": r.get::<_, i64>(10), "brier_vs_market": r.get::<_, Option<f64>>(11).map(|x| (x * 10000.0).round() / 10000.0),
            "kelly_roi": match (r.get::<_, Option<f64>>(12), r.get::<_, Option<f64>>(13)) { (Some(a), Some(b)) if b > 0.0 => json!(r3(a / b)), _ => Value::Null } })
    }).collect();
    let ev = state.db.query("SELECT engine, market, source, comparison, n, seasons, better, worse, metric, se, detail, updated_at FROM pick_evidence ORDER BY engine, market, source", &[]).await;
    let evidence: Vec<Value> = ev.map(|rows| rows.iter().map(|r| json!({ "engine": r.get::<_, String>(0), "market": r.get::<_, String>(1), "source": r.get::<_, String>(2),
        "comparison": r.get::<_, String>(3), "n": r.get::<_, i32>(4), "seasons": r.get::<_, i32>(5), "better": r.get::<_, i32>(6), "worse": r.get::<_, i32>(7),
        "metric": r.get::<_, Option<f64>>(8), "se": r.get::<_, Option<f64>>(9), "detail": r.get::<_, Json<Value>>(10).0, "updated_at": r.get::<_, DateTime<Utc>>(11) })).collect()).unwrap_or_default();
    // current tier per champion engine x market
    let champs = champions(&state.db).await.unwrap_or_default();
    let evm = evidence_map_for_tiers(&state.db, &champs).await;
    HttpResponse::Ok().json(json!({ "days": days, "rows": out, "evidence": evidence, "tiers": evm }))
}

async fn evidence_map_for_tiers(db: &tokio_postgres::Client, champs: &HashMap<String, (String, String)>) -> Value {
    let Ok(ev) = evidence(db, champs).await else { return json!([]) };
    let mut out: Vec<Value> = ev.iter().map(|((e, m), rows)| { let (t, _) = tier(rows); json!({ "engine": e, "market": m, "tier": t, "cap": tier_cap(t) }) }).collect();
    out.sort_by(|a, b| a["engine"].as_str().cmp(&b["engine"].as_str()).then(a["market"].as_str().cmp(&b["market"].as_str())));
    json!(out)
}

#[derive(Deserialize)]
pub struct MetaQ { sport: Option<String> }

pub async fn meta(state: web::Data<State>, q: web::Query<MetaQ>) -> impl Responder {
    let db = &state.db;
    let sport = q.sport.clone().filter(|s| s != "all");
    // menus and counts follow what the public may see at all (published or leans-only)
    let vis = crate::governance::VISIBLE;
    let opts = db.query(&format!("SELECT DISTINCT 'market', market, market FROM ai_picks WHERE status='open' AND starts_at > now() AND ($1::text IS NULL OR sport=$1) AND {vis} \
        UNION SELECT DISTINCT 'game', game_id, game_label || ' · ' || to_char(starts_at AT TIME ZONE 'America/New_York', 'MM/DD HH12:MI AM') FROM ai_picks WHERE status='open' AND starts_at > now() AND ($1::text IS NULL OR sport=$1) AND {vis} \
        UNION SELECT DISTINCT 'book', book, book FROM ai_picks WHERE status='open' AND starts_at > now() AND ($1::text IS NULL OR sport=$1) AND {vis}"), &[&sport]).await;
    let mut markets = Vec::new(); let mut games = Vec::new(); let mut books = Vec::new();
    if let Ok(rows) = opts {
        for r in rows {
            let (k, id, label): (String, String, String) = (r.get(0), r.get(1), r.get(2));
            match k.as_str() {
                "market" => markets.push(json!({ "id": id, "label": stat_label(&id) })),
                "game" => games.push(json!({ "id": id, "label": label })),
                _ => books.push(json!({ "id": id, "label": book_title(&id) })),
            }
        }
    }
    let usage = db.query("SELECT provider, requests, units FROM provider_usage WHERE day=(now() AT TIME ZONE 'utc')::date", &[]).await.map(|rows| rows.iter().map(|r|
        json!({ "provider": r.get::<_, String>(0), "requests": r.get::<_, i32>(1), "units": r.get::<_, i32>(2) })).collect::<Vec<_>>()).unwrap_or_default();
    let fresh = db.query("SELECT source, sport, max(observed_at), count(*) FROM market_lines WHERE observed_at > now() - interval '1 day' GROUP BY 1,2 ORDER BY 1,2", &[]).await
        .map(|rows| rows.iter().map(|r| json!({ "source": r.get::<_, String>(0), "sport": r.get::<_, String>(1), "last": r.get::<_, DateTime<Utc>>(2), "rows": r.get::<_, i64>(3) })).collect::<Vec<_>>()).unwrap_or_default();
    // hidden = open picks in a suspended or unpublished market (a governance decision), not by tier
    let hidden = db.query_one(&format!("SELECT count(*) FILTER (WHERE NOT {vis}), count(*) FROM ai_picks WHERE status='open' AND starts_at > now() AND ($1::text IS NULL OR sport=$1)"), &[&sport]).await
        .map(|r| json!({ "hidden": r.get::<_, i64>(0), "total": r.get::<_, i64>(1) })).unwrap_or(Value::Null);
    let holds = db.query("SELECT engine, market, reason, decided_at FROM publication_holds_active ORDER BY engine, market", &[]).await
        .map(|rows| rows.iter().map(|r| json!({ "engine": r.get::<_, String>(0), "market": r.get::<_, String>(1), "reason": r.get::<_, String>(2),
            "since": r.get::<_, DateTime<Utc>>(3) })).collect::<Vec<_>>()).unwrap_or_default();
    let llm = json!({ "configured": llm_endpoint().is_some(), "model": llm_endpoint().map(|e| e.1), "reachable": llm_reachable(&state).await });
    HttpResponse::Ok().json(json!({ "markets": markets, "games": games, "books": books, "usage_today": usage, "lines": fresh, "counts": hidden, "llm_writeups": llm,
        "publication_holds": holds,
        "settings": { "min_edge": lines::setting(db, "min_edge", 0.015).await, "oddsapi_daily_credits": lines::setting(db, "oddsapi_daily_credits", 9.0).await,
            "sgo_daily_entities": lines::setting(db, "sgo_daily_entities", 60.0).await } }))
}

#[allow(dead_code)]
fn _unused(_: Duration) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn dist(offset: i64, pmf: &[f64]) -> Value { json!({ "pmf": pmf, "pmf_offset": offset }) }

    #[test]
    fn dist_split_counts_push_at_integer_lines() {
        let d = Dist::from_json(&dist(0, &[0.2, 0.3, 0.5])).unwrap();
        assert_eq!(d.split(1.5), (0.5, 0.5, 0.0));
        let (o, u, p) = d.split(1.0);
        assert!((o - 0.5).abs() < 1e-12 && (u - 0.2).abs() < 1e-12 && (p - 0.3).abs() < 1e-12);
        assert!((d.mean() - 1.3).abs() < 1e-12);
        assert_eq!(d.quantile(0.5), 1);
    }

    #[test]
    fn spread_sides_mirror() {
        // home margin: -2 (0.2), +1 (0.3), +5 (0.5)
        let mut pmf = vec![0.0; 8]; pmf[0] = 0.2; pmf[3] = 0.3; pmf[7] = 0.5;
        let p = json!({ "margin_home": dist(-2, &pmf), "win": { "home": 0.8 } });
        let home = price(&p, false, "spread", "game", "home", Some(-3.5)).unwrap(); // home -3.5 covers only on +5
        assert!((home.win - 0.5).abs() < 1e-12);
        let away = price(&p, false, "spread", "game", "away", Some(3.5)).unwrap();
        assert!((away.win - 0.5).abs() < 1e-12 && (home.win + away.win - 1.0).abs() < 1e-12);
        let pk = price(&p, false, "spread", "game", "home", Some(-1.0)).unwrap(); // margin 1 pushes
        assert!((pk.push - 0.3).abs() < 1e-12 && (pk.p() - 0.5 / 0.7).abs() < 1e-12);
    }

    #[test]
    fn puck_lines_and_props_price_from_payload() {
        let p = json!({ "home": "DET", "away": "VAN", "puckline": { "home_minus_1_5": 0.18, "away_minus_1_5": 0.38 },
            "players": { "home": [{ "id": 42, "name": "A", "p_active": 0.95, "stats": { "sog": dist(0, &[0.2, 0.3, 0.3, 0.2]) } }], "away": [] },
            "goalies": [{ "id": 7, "name": "G", "team": "DET", "saves": dist(20, &[0.5, 0.5]) }] });
        assert!((price(&p, false, "spread", "game", "home", Some(1.5)).unwrap().win - 0.62).abs() < 1e-12);
        assert!((price(&p, false, "spread", "game", "away", Some(-1.5)).unwrap().win - 0.38).abs() < 1e-12);
        assert!(price(&p, false, "spread", "game", "home", Some(-2.5)).is_none());
        let sog = price(&p, false, "sog", "42", "over", Some(1.5)).unwrap();
        assert!((sog.win - 0.5).abs() < 1e-12 && sog.p_active == Some(0.95));
        assert!((price(&p, false, "saves", "7", "under", Some(20.5)).unwrap().win - 0.5).abs() < 1e-12);
        assert!(price(&p, false, "sog", "999", "over", Some(1.5)).is_none());
    }

    #[test]
    fn team_payload_prices_moneyline_and_totals() {
        let p = json!({ "margin_home": 4.2, "forecasts": [{ "market": "home_win", "p": 0.64 }], "dist": { "total": dist(200, &[0.25, 0.25, 0.25, 0.25]), "margin": dist(-1, &[0.3, 0.0, 0.7]) } });
        assert!((price(&p, true, "moneyline", "game", "away", None).unwrap().win - 0.36).abs() < 1e-12);
        assert!((price(&p, true, "total", "game", "over", Some(201.5)).unwrap().win - 0.5).abs() < 1e-12);
        assert!((price(&p, true, "spread", "game", "home", Some(-0.5)).unwrap().win - 0.7).abs() < 1e-12);
    }

    #[test]
    fn nfl_moneyline_excludes_ties() {
        let p = json!({ "win": { "home": 0.594, "away": 0.396, "tie": 0.01 }, "margin_home": { "mean": 2.0 } });
        let h = price(&p, false, "moneyline", "game", "home", None).unwrap();
        assert!((h.win - 0.6).abs() < 1e-9);
    }

    #[test]
    fn odds_math() {
        assert!((decimal(-110) - 1.909090909).abs() < 1e-6);
        assert!((decimal(150) - 2.5).abs() < 1e-12);
        assert!((novig(-110, -110) - 0.5).abs() < 1e-12);
        // power method: the favourite keeps more than the proportional 0.5798
        let (f, d) = (novig(-150, 130), novig(130, -150));
        assert!((f + d - 1.0).abs() < 1e-9 && f > 0.6 / (0.6 + 100.0 / 230.0) && f < 0.6);
    }

    #[test]
    fn evaluation_follows_the_current_price_and_lists_what_to_recheck() {
        let now = Utc::now();
        let mut v = json!({
            "status": "open", "starts_at": (now + chrono::Duration::hours(3)).to_rfc3339(), "published_at": (now - chrono::Duration::hours(2)).to_rfc3339(),
            "line": 5.5, "price": 120, "p_model": 0.52, "p_market": 0.45, "edge": 0.07, "ev": 0.144, "ev_now": 0.064, "projection": 6.1,
            "tier": "losing", "grade": "C", "playable_now": true, "playable_price": 105, "fair_price": -108, "risk_flags": [],
            "latest": { "line": 5.5, "price": 105, "p_market": 0.47, "edge": 0.05, "updated_at": (now - chrono::Duration::minutes(10)).to_rfc3339() },
        });
        let e = evaluation(&v, now);
        // the price grade follows the price now (+105), not the published +120; the official grade stays capped
        assert_eq!(e["price"]["grade"], "A-");
        assert_eq!(e["price"]["published"]["grade"], "A+");
        assert_eq!(e["price"]["price"], 105);
        assert_eq!(e["trust"]["cap"], "C");
        assert_eq!(e["all_clear"], true);
        v["risk_flags"] = json!([{ "code": "news_changed" }]);
        v["latest"]["updated_at"] = json!((now - chrono::Duration::minutes(200)).to_rfc3339());
        let e = evaluation(&v, now);
        assert_eq!(e["all_clear"], false);
        assert_eq!(e["checks_ok"].as_u64().unwrap() + 2, e["checks_total"].as_u64().unwrap());
        v["starts_at"] = json!((now - chrono::Duration::minutes(1)).to_rfc3339());
        assert_eq!(evaluation(&v, now)["checks"][0]["ok"], false);
    }

    #[test]
    fn grades_are_capped_by_track_record() {
        assert_eq!(edge_grade(0.12), Some("A+"));
        assert_eq!(edge_grade(0.045), Some("B"));
        assert_eq!(edge_grade(0.01), None);
        assert_eq!(cap("A+", "proven"), "A+");
        assert_eq!(cap("A+", "baseline"), "B+");
        assert_eq!(cap("B-", "baseline"), "B-");
        assert_eq!(cap("A", "unproven"), "C+");
        assert_eq!(cap("A", "losing"), "C");
    }

    #[test]
    fn tiers_prefer_live_then_market_then_baseline() {
        let e = |s: &str, c: &str, n: i32, better: i32, worse: i32, m: Option<f64>, se: Option<f64>| Evidence { source: s.into(), comparison: c.into(), n, seasons: better + worse + 1, better, worse, metric: m, se, detail: json!({}) };
        assert_eq!(tier(&[]).0, "unproven");
        assert_eq!(tier(&[e("backtest", "baseline", 9000, 6, 0, None, None)]).0, "baseline");
        assert_eq!(tier(&[e("backtest", "baseline", 9000, 6, 0, None, None), e("backtest", "market", 900, 0, 4, None, None)]).0, "losing");
        assert_eq!(tier(&[e("backtest", "market", 900, 0, 4, None, None), e("live", "market", 400, 0, 0, Some(-0.004), Some(0.001))]).0, "proven");
        assert_eq!(tier(&[e("live", "market", 100, 0, 0, Some(-0.004), Some(0.001))]).0, "unproven", "too few live picks");
        assert_eq!(tier(&[e("backtest", "baseline", 9000, 1, 0, None, None)]).0, "unproven", "one good season is not enough");
    }

    #[test]
    fn unmeasured_props_inherit_a_losing_tier() {
        let e = |worse: i32, better: i32| Evidence { source: "backtest".into(), comparison: "baseline".into(), n: 9000, seasons: 6, better, worse, metric: None, se: None, detail: json!({}) };
        let mut ev: HashMap<(String, String), Vec<Evidence>> = HashMap::new();
        ev.insert(("nba".into(), "pts".into()), vec![e(4, 0)]);
        ev.insert(("nba".into(), "moneyline".into()), vec![e(0, 0)]);
        let (t, src) = prop_tier(&ev, "nba", "pra");
        assert_eq!(t, "losing");
        assert_eq!(src.unwrap().detail["inherited_from"], "pts");
        assert_eq!(prop_tier(&ev, "nba", "total").0, "unproven", "game markets never inherit");
        ev.insert(("nba".into(), "reb".into()), vec![e(0, 5)]);
        assert_eq!(prop_tier(&ev, "nba", "pra").0, "unproven", "a proven-ish sibling blocks inheritance");
    }

    #[test]
    fn first_segment_and_team_total_markets_price_from_payload() {
        let p = json!({ "score": { "home": dist(0, &[0.0, 0.0, 0.5, 0.5]), "away": dist(0, &[0.25, 0.75]) },
            "h1": { "win": { "home": 0.5, "away": 0.3, "tie": 0.2 }, "margin_home": dist(-1, &[0.3, 0.2, 0.5]),
                    "total": dist(1, &[0.4, 0.6]), "home": dist(0, &[0.5, 0.5]), "away": dist(0, &[0.8, 0.2]) } });
        // team totals: home scores 2 or 3
        assert!((price(&p, false, "team_total_home", "game", "over", Some(2.5)).unwrap().win - 0.5).abs() < 1e-12);
        assert!((price(&p, false, "team_total_away", "game", "under", Some(0.5)).unwrap().win - 0.25).abs() < 1e-12);
        // a tied half refunds the moneyline
        let ml = price(&p, false, "moneyline_1h", "game", "away", None).unwrap();
        assert!((ml.win - 0.3).abs() < 1e-12 && (ml.push - 0.2).abs() < 1e-12 && (ml.p() - 0.375).abs() < 1e-12);
        // 1H spread: home -0.5 covers when the margin is +1
        assert!((price(&p, false, "spread_1h", "game", "home", Some(-0.5)).unwrap().win - 0.5).abs() < 1e-12);
        assert!((price(&p, false, "spread_1h", "game", "away", Some(0.5)).unwrap().win - 0.5).abs() < 1e-12);
        assert!((price(&p, false, "total_1h", "game", "over", Some(1.5)).unwrap().win - 0.6).abs() < 1e-12);
        assert!((price(&p, true, "team_total_away_1h", "game", "over", Some(0.5)).unwrap().win - 0.2).abs() < 1e-12);
        assert!(price(&json!({}), false, "total_1h", "game", "over", Some(1.5)).is_none());
    }

    #[test]
    fn power_devig_sums_to_one_and_takes_more_from_the_longshot() {
        let (fav, dog) = (novig(-300, 240), novig(240, -300));
        assert!((fav + dog - 1.0).abs() < 1e-9);
        let (x, y) = (implied(240), implied(-300));
        assert!(dog < x / (x + y), "power gives the longshot less than proportional");
        assert!((novig(-110, -110) - 0.5).abs() < 1e-9);
        assert!((devig(0.4, 0.6) - 0.4).abs() < 1e-9, "a fair pair is left alone");
    }

    #[test]
    fn yes_only_props_price_conservatively_and_label() {
        let c = complement_price(120);
        assert!((novig(120, c) - implied(120)).abs() < 0.01, "the pair's no-vig price is the yes price's raw implied");
        assert_eq!(pick_label("nba", "double_double", "over", Some(0.5), "DET", "BOS"), "Double-Double Yes");
        assert_eq!(pick_label("nfl", "tds", "over", Some(0.5), "KC", "BUF"), "Anytime TD Yes");
        assert_eq!(pick_label("nfl", "tds", "over", Some(1.5), "KC", "BUF"), "Yes 2+ TDs");
        assert_eq!(pick_label("nfl", "rush_rec_yds", "under", Some(88.5), "KC", "BUF"), "Under 88.5 Rush + Rec Yards");
        assert_eq!(result_of("double_double", "over", Some(0.5), 1.0), "win");
    }

    #[test]
    fn new_markets_settle_and_label() {
        assert_eq!(result_of("moneyline_1h", "home", None, 0.0), "push");
        assert_eq!(result_of("spread_1h", "away", Some(2.5), -2.0), "win");
        assert_eq!(result_of("team_total_home", "over", Some(110.5), 111.0), "win");
        assert_eq!(result_of("team_total_away_1h", "under", Some(2.5), 3.0), "loss");
        assert_eq!(pick_label("nhl", "moneyline_1h", "home", None, "BOS", "TOR"), "BOS 1P ML");
        assert_eq!(pick_label("mlb", "team_total_away_1h", "over", Some(2.5), "NYY", "LAD"), "LAD F5 Over 2.5");
        assert_eq!(pick_label("nba", "spread_1h", "away", Some(3.5), "BOS", "LAL"), "LAL 1H +3.5");
    }

    #[test]
    fn results_settle_each_market() {
        assert_eq!(result_of("moneyline", "home", None, 3.0), "win");
        assert_eq!(result_of("moneyline", "away", None, 3.0), "loss");
        assert_eq!(result_of("spread", "home", Some(-3.0), 3.0), "push");
        assert_eq!(result_of("spread", "away", Some(3.5), 3.0), "win");
        assert_eq!(result_of("total", "under", Some(220.5), 221.0), "loss");
        assert_eq!(result_of("pts", "over", Some(24.5), 25.0), "win");
    }

    #[test]
    fn game_writeup_names_schedule_and_lineup() {
        let f = json!({ "kind": "game", "market": "spread", "side": "home", "line": -4.5, "home_team": "DEN", "away_team": "LAL", "pick_team": "DEN",
            "engine_label": "possession simulator", "projection": 6.2, "p_model": 0.55, "p_market": 0.52, "edge": 0.03, "book_title": "FanDuel",
            "away_rest_days": 1, "away_tz_shift": 1, "away_schedule_margin": -3.1, "home_lineup_gap": -1.4, "evidence_text": "" });
        let t = template(&f);
        assert!(t.contains("LAL is on the second night of a back-to-back and crossed 1 time zone east, worth -3.1 points of margin"), "{t}");
        assert!(t.contains("DEN's expected lineup rates 1.4 points worse than its last ten games"), "{t}");
    }

    #[test]
    fn template_uses_only_fact_numbers() {
        let f = json!({ "kind": "prop", "market": "pts", "side": "under", "line": 24.5, "subject_name": "Jalen Green", "team": "PHX", "opponent": "SAC", "home": false,
            "engine_label": "possession simulator", "projection": 20.4, "q10": 13, "q50": 20, "q90": 28, "stat_label": "Points", "p_model": 0.644, "p_market": 0.535,
            "edge": 0.109, "book_title": "DraftKings", "l10_avg": 17.7, "l10_hits": 8, "l10_games": 10, "l20_avg": 19.1, "l20_games": 20, "open_line": 25.5,
            "evidence_text": "There is no backtest or live record for this engine on points props yet, so the grade is capped at C+ until picks settle." });
        let t = template(&f);
        assert!(t.contains("projects 20.4 points (median 20, 80% range 13–28)"), "{t}");
        assert!(t.contains("making the under 64.4% likely"), "{t}");
        assert!(t.contains("an edge of 10.9 points"), "{t}");
        assert!(t.contains("Green has averaged 17.7 and gone under 24.5 in 8 of them (last 20: 19.1 per game)"), "{t}");
        assert!(t.contains("moved from 25.5 at open to 24.5"), "{t}");
        assert_eq!(numbers("a 64.4% edge, 2 of 10, -3.5"), vec![64.4, 2.0, 10.0, 3.5]);
    }

    #[test]
    fn price_sensitivity() {
        // 55% to win, no pushes: break-even at -122; +2% per unit needs -117 or better
        assert_eq!(price_for_ev(0.55, 0.45, 0.0), Some(-122));
        assert_eq!(price_for_ev(0.55, 0.45, 0.02), Some(-117));
        assert_eq!(price_for_ev(0.5, 0.5, 0.0), Some(100));
        assert_eq!(price_for_ev(0.4, 0.6, 0.0), Some(150));
        assert!(at_least(-110, -118) && !at_least(-125, -118) && at_least(105, -118));
        // published at +104 with a -138 break-even (58%): playable well below the published price
        assert_eq!(playable_floor(0.58, 0.42, 0.02, 104), Some(-131));
        // a thin pick (EV under 2% at its own price) is playable only at that price
        assert_eq!(playable_floor(0.55, 0.45, 0.02, -120), Some(-120));
        // the latest price decides at the published line; a line move means re-check
        assert_eq!(playable_now("open", Some(24.5), -110, Some(-118), Some(24.5), Some(-120)), Some(false));
        assert_eq!(playable_now("open", Some(24.5), -110, Some(-118), Some(24.5), Some(-115)), Some(true));
        assert_eq!(playable_now("open", Some(24.5), -110, Some(-118), Some(23.5), Some(-110)), None);
        assert_eq!(playable_now("settled", Some(24.5), -110, Some(-118), None, None), None);
    }

    #[test]
    fn writeup_carries_the_case_against() {
        let f = json!({ "kind": "prop", "market": "pts", "side": "over", "line": 24.5, "subject_name": "Jalen Green", "team": "PHX", "opponent": "SAC", "home": false,
            "engine_label": "possession simulator", "projection": 26.1, "p_model": 0.58, "p_market": 0.52, "p_raw": 0.66, "edge": 0.06, "book_title": "DraftKings",
            "l10_hits": 3, "l10_games": 10, "proj_minutes": 36.0, "l5_minutes": 31.5, "lineup_mode": "projected", "open_line": 23.5,
            "playable_price": -118, "playable_min_ev": 0.02, "evidence_text": "" });
        let t = template(&f);
        assert!(t.contains("What could make it wrong: The simulations still land on the other side 42% of the time, and the de-vigged market says 52%"), "{t}");
        assert!(t.contains("Green has finished over 24.5 in only 3 of the last 10 games."), "{t}");
        assert!(t.contains("plays Green 36 minutes, more than the recent 31.5"), "{t}");
        assert!(t.contains("moved against this side (23.5 to 24.5)"), "{t}");
        assert!(t.contains("engine is more confident (66%), but its record only justifies 58%"), "{t}");
        assert!(t.contains("At worse odds than -118 the bet is worth less than 2% per unit."), "{t}");
    }
}
