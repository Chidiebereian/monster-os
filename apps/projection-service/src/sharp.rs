//! Stale lines and stakes.
//!
//! A sharp book (Pinnacle) moves first and with the least vig, so its de-vigged
//! price is the best estimate of the true probability that needs no model. A
//! retail book still offering a price that beats it is a stale line:
//!
//!   p_fair = Pinnacle de-vigged at the same line, seen within 30 minutes of the
//!            book's price; else the median de-vigged price of at least four
//!            other books from the same snapshot (consensus)
//!   EV     = p_fair x decimal - 1
//!
//! Every book x selection x line whose EV clears `stale_min_ev` is published
//! once to market_edges, followed until the start and settled like a pick, with
//! the closing fair price for CLV. EVs above `stale_max_ev` are dropped: a gap
//! that large is a mismatched line or a suspended market, not a stale price.
//!
//! Stakes (both feeds) are fractional Kelly on the probability we trust:
//!   f* = (p x decimal - 1) / (decimal - 1),  stake = min(fraction x f*, cap)
//!
//!   GET /picks/edges?sport=&status=open|settled|all&limit=

use std::collections::{BTreeMap, HashMap};

use actix_web::{web, HttpResponse, Responder};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::lines;
use crate::picks::{book_title, decimal, novig, pick_label, result_of, stat_label};
use crate::State;

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}
fn r3(x: f64) -> f64 { (x * 1000.0).round() / 1000.0 }
fn r4(x: f64) -> f64 { (x * 10000.0).round() / 10000.0 }

/// The sharp book whose price is taken as fair when it quotes the line.
pub(crate) const SHARP: &str = "pinnacle";
/// Reference prices must be this close in time to the price they judge.
const WINDOW_MIN: i64 = 30;

/// One book's two-way quote at one line: (book, price of this side, price of the other side, seen at).
pub(crate) type Ref<'a> = (&'a str, i32, i32, DateTime<Utc>);

/// Fair probability of a side from the other books' quotes at the same line:
/// (p, source, books). `exclude` is the book being judged.
pub(crate) fn fair(refs: &[Ref], exclude: &str, at: DateTime<Utc>, min_books: usize) -> Option<(f64, &'static str, usize)> {
    let near: Vec<&Ref> = refs.iter().filter(|r| r.0 != exclude && (r.3 - at).num_minutes().abs() <= WINDOW_MIN).collect();
    if let Some(p) = near.iter().find(|r| r.0 == SHARP) { return Some((novig(p.1, p.2), "pinnacle", 1)); }
    if near.len() < min_books { return None; }
    let mut v: Vec<f64> = near.iter().map(|r| novig(r.1, r.2)).collect();
    v.sort_by(|a, b| a.total_cmp(b));
    let m = if v.len() % 2 == 1 { v[v.len() / 2] } else { (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0 };
    Some((m, "consensus", near.len()))
}

/// Fractional Kelly stake (fraction of bankroll) for a win probability `p` (pushes excluded) at an American price.
pub(crate) fn kelly(p: f64, american: i32, fraction: f64, cap: f64) -> f64 {
    let d = decimal(american);
    let f = (p * d - 1.0) / (d - 1.0);
    (f.max(0.0) * fraction).min(cap)
}

/// Stake settings: (Kelly fraction, cap per play).
pub(crate) async fn stake_settings(db: &tokio_postgres::Client) -> (f64, f64) {
    (lines::setting(db, "kelly_fraction", 0.25).await, lines::setting(db, "kelly_cap", 0.02).await)
}

/// How much of a full fractional-Kelly stake a model pick earns from its track
/// record: only engines that have beaten something get money behind them.
pub(crate) fn tier_stake_mult(tier: &str) -> f64 {
    match tier { "proven" => 1.0, "baseline" => 0.5, _ => 0.0 }
}

pub(crate) fn source_text(source: &str, books: usize) -> String {
    if source == "pinnacle" { "Pinnacle".into() } else { format!("the consensus of {books} other books") }
}

// ---------------------------------------------------------------------------
// Scan
// ---------------------------------------------------------------------------

struct Quote { book: String, line: Option<f64>, a: i32, b: i32, link: Option<String>, observed: DateTime<Utc> }

/// Find and publish stale lines for a sport; update the latest price of open ones.
pub(crate) async fn scan(state: &State, sport: &str) -> Result<Value, String> {
    let db = &state.db;
    let min_ev = lines::setting(db, "stale_min_ev", 0.02).await;
    let max_ev = lines::setting(db, "stale_max_ev", 0.12).await;
    let min_books = lines::setting(db, "stale_consensus_books", 4.0).await as usize;
    let (kf, kcap) = stake_settings(db).await;
    let rows = db.query("SELECT DISTINCT ON (book, game_id, market, subject, side) book, game_id, kind, market, subject, subject_name, side, line, price, link, observed_at, starts_at \
        FROM market_lines WHERE sport=$1 AND game_id IS NOT NULL AND starts_at > now() + interval '5 minutes' AND observed_at > now() - interval '3 hours' \
        ORDER BY book, game_id, market, subject, side, observed_at DESC", &[&sport]).await.map_err(|e| dberr(&e))?;
    type Key = (String, String, String); // game, market, subject
    let mut sides: HashMap<(Key, String, String), (Option<f64>, i32, Option<String>, DateTime<Utc>)> = HashMap::new();
    let mut info: HashMap<Key, (String, Option<String>, Option<DateTime<Utc>>)> = HashMap::new(); // kind, subject name, start
    for r in &rows {
        let key: Key = (r.get(1), r.get(3), r.get(4));
        let e = info.entry(key.clone()).or_insert((r.get(2), None, r.get(11)));
        if e.1.is_none() { e.1 = r.get(5); }
        sides.insert((key, r.get(0), r.get(6)), (r.get(7), r.get(8), r.get(9), r.get(10)));
    }
    let other = |s: &str| match s { "over" => "under", "under" => "over", "home" => "away", _ => "home" };
    let mut quotes: HashMap<(Key, String), Vec<Quote>> = HashMap::new();
    for ((key, book, side), (line, price, link, obs)) in &sides {
        let Some((oline, oprice, _, oobs)) = sides.get(&(key.clone(), book.clone(), other(side).to_string())) else { continue };
        let same = match crate::segments::base_market(&key.1) { "moneyline" => true, "spread" => matches!((line, oline), (Some(a), Some(b)) if (a + b).abs() < 1e-9), _ => line == oline };
        // both sides from the same fetch
        if !same || (*obs - *oobs).num_minutes().abs() > 5 { continue; }
        quotes.entry((key.clone(), side.clone())).or_default().push(Quote { book: book.clone(), line: *line, a: *price, b: *oprice, link: link.clone(), observed: *obs });
    }
    let games: Vec<String> = info.keys().map(|k| k.0.clone()).collect::<std::collections::HashSet<_>>().into_iter().collect();
    let teams: HashMap<String, (String, String)> = db.query("SELECT DISTINCT ON (game_id) game_id, COALESCE(payload->'game'->>'home', payload->>'home'), COALESCE(payload->'game'->>'away', payload->>'away') FROM prediction_ledger \
        WHERE game_id = ANY($1) AND sport = ANY($2) ORDER BY game_id, id DESC", &[&games, &vec![sport.to_string(), format!("{sport}_team")]]).await.map_err(|e| dberr(&e))?
        .iter().map(|r| (r.get(0), (r.get::<_, Option<String>>(1).unwrap_or_default(), r.get::<_, Option<String>>(2).unwrap_or_default()))).collect();

    let (mut published, mut judged, mut too_big) = (0u64, 0usize, 0usize);
    let mut latest: Vec<Value> = Vec::new();
    for ((key, side), qs) in &quotes {
        let mut by_line: BTreeMap<String, Vec<&Quote>> = BTreeMap::new();
        for q in qs { by_line.entry(format!("{:?}", q.line)).or_default().push(q); }
        for same in by_line.values() {
            let refs: Vec<Ref> = same.iter().map(|q| (q.book.as_str(), q.a, q.b, q.observed)).collect();
            for q in same {
                if q.book == SHARP { continue; }
                let Some((p, src, n)) = fair(&refs, &q.book, q.observed, min_books) else { continue };
                judged += 1;
                let ev = p * decimal(q.a) - 1.0;
                latest.push(json!({ "game_id": key.0, "market": key.1, "subject": key.2, "side": side, "book": q.book, "line": q.line, "price": q.a, "ev": ev }));
                if ev < min_ev || !(0.1..=0.9).contains(&p) { continue; }
                if ev > max_ev { too_big += 1; continue; }
                let (kind, name, start) = info.get(key).cloned().unwrap_or(("game".into(), None, None));
                let (home, away) = teams.get(&key.0).cloned().unwrap_or_default();
                let subject_name = if key.2 == "game" { format!("{away} @ {home}") } else { name.unwrap_or_else(|| key.2.clone()) };
                let stake = kelly(p, q.a, kf, kcap) * if src == "pinnacle" { 1.0 } else { 0.5 };
                published += db.execute("INSERT INTO market_edges (sport, game_id, game_label, home, away, starts_at, kind, market, subject, subject_name, side, line, book, price, link, \
                      fair_source, fair_books, p_fair, p_book, ev, stake, observed_at, latest_price, latest_ev) \
                    VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$14,$20) \
                    ON CONFLICT (sport, game_id, market, subject, side, book, line) DO NOTHING",
                    &[&sport, &key.0, &format!("{away} @ {home}"), &home, &away, &start, &kind, &key.1, &key.2, &subject_name, side, &q.line, &q.book, &q.a, &q.link,
                      &src, &(n as i32), &p, &novig(q.a, q.b), &ev, &stake, &q.observed]).await.map_err(|e| dberr(&e))?;
            }
        }
    }
    // open edges follow their book's price (and the fair price) until the start
    let updated = db.execute("UPDATE market_edges m SET latest_price = x.price, latest_ev = x.ev, updated_at = now() \
        FROM jsonb_to_recordset($2) AS x(game_id text, market text, subject text, side text, book text, line float8, price int, ev float8) \
        WHERE m.sport=$1 AND m.status='open' AND m.game_id=x.game_id AND m.market=x.market AND m.subject=x.subject AND m.side=x.side AND m.book=x.book \
          AND m.line IS NOT DISTINCT FROM x.line", &[&sport, &Json(Value::Array(latest))]).await.map_err(|e| dberr(&e))?;
    Ok(json!({ "judged": judged, "published": published, "updated": updated, "suppressed_too_large": too_big }))
}

// ---------------------------------------------------------------------------
// Settle
// ---------------------------------------------------------------------------

/// The closing fair probability of a side: the last quotes before the start.
pub(crate) async fn closing_fair(db: &tokio_postgres::Client, sport: &str, game: &str, market: &str, subject: &str, side: &str, line: Option<f64>,
    exclude: &str, start: Option<DateTime<Utc>>, min_books: usize) -> Result<Option<(f64, &'static str, usize)>, String> {
    let o = match side { "over" => "under", "under" => "over", "home" => "away", _ => "home" };
    let rows = db.query("SELECT DISTINCT ON (book, side) book, side, line, price, observed_at FROM market_lines WHERE sport=$1 AND game_id=$2 AND market=$3 AND subject=$4 \
        AND side = ANY($5) AND observed_at <= COALESCE($6, now()) ORDER BY book, side, observed_at DESC",
        &[&sport, &game, &market, &subject, &vec![side.to_string(), o.to_string()], &start]).await.map_err(|e| dberr(&e))?;
    let mut by_book: HashMap<String, (Option<(Option<f64>, i32, DateTime<Utc>)>, Option<(Option<f64>, i32)>)> = HashMap::new();
    for r in &rows {
        let e = by_book.entry(r.get(0)).or_default();
        if r.get::<_, String>(1) == side { e.0 = Some((r.get(2), r.get(3), r.get(4))); } else { e.1 = Some((r.get(2), r.get(3))); }
    }
    let mirror = |l: Option<f64>| if market == "spread" { l.map(|x| -x) } else { l };
    let refs: Vec<(String, i32, i32, DateTime<Utc>)> = by_book.into_iter().filter_map(|(b, (a, o))| {
        let ((al, ap, at), (ol, op)) = (a?, o?);
        (al == line && mirror(ol) == line).then_some((b, ap, op, at))
    }).collect();
    let Some(last) = refs.iter().map(|r| r.3).max() else { return Ok(None) };
    let r: Vec<Ref> = refs.iter().map(|(b, a, o, t)| (b.as_str(), *a, *o, *t)).collect();
    Ok(fair(&r, exclude, last, min_books))
}

pub(crate) async fn settle(state: &State, sport: &str) -> Result<Value, String> {
    let db = &state.db;
    let min_books = lines::setting(db, "stale_consensus_books", 4.0).await as usize;
    db.execute("UPDATE market_edges SET status='started' WHERE sport=$1 AND status='open' AND starts_at <= now()", &[&sport]).await.map_err(|e| dberr(&e))?;
    let open = db.query("SELECT id, game_id, market, subject, side, line, price, book, starts_at FROM market_edges WHERE sport=$1 AND status='started'", &[&sport]).await.map_err(|e| dberr(&e))?;
    let mut by_game: BTreeMap<String, Vec<&tokio_postgres::Row>> = BTreeMap::new();
    for r in &open { by_game.entry(r.get(1)).or_default().push(r); }
    let (mut settled, mut pending) = (0, 0);
    for (game, rows) in by_game {
        let Some(go) = crate::learning::game_outcomes(db, sport, &game).await? else { pending += 1; continue };
        let props = if rows.iter().any(|r| r.get::<_, String>(3) != "game") { crate::picks::prop_actuals(db, sport, &game).await? } else { None };
        for r in rows {
            let (id, market, subject, side, line, price, book, start): (i64, String, String, String, Option<f64>, i32, String, Option<DateTime<Utc>>) =
                (r.get(0), r.get(2), r.get(3), r.get(4), r.get(5), r.get(6), r.get(7), r.get(8));
            let actual = if subject == "game" { go.game.get(crate::segments::outcome_key(&market)).copied() } else {
                match &props { Some(p) => p.get(&subject).and_then(|m| m.get(market.as_str()).copied()), None => continue }
            };
            let res = actual.map(|a| result_of(&market, &side, line, a)).unwrap_or("void");
            let profit = match res { "win" => decimal(price) - 1.0, "loss" => -1.0, _ => 0.0 };
            let close = closing_fair(db, sport, &game, &market, &subject, &side, line, &book, start, min_books).await?;
            let (cpf, cev) = match close { Some((p, _, _)) => (Some(p), Some(p * decimal(price) - 1.0)), None => (None, None) };
            db.execute("UPDATE market_edges SET status=$2, actual=$3, result=$4, profit=$5, close_p_fair=$6, close_ev=$7, settled_at=now() WHERE id=$1",
                &[&id, &if res == "void" { "void" } else { "settled" }, &actual, &res, &profit, &cpf, &cev]).await.map_err(|e| dberr(&e))?;
            settled += 1;
        }
    }
    Ok(json!({ "settled": settled, "games_pending": pending }))
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct EdgesQ { sport: Option<String>, status: Option<String>, limit: Option<i64>, days: Option<i32> }

pub async fn list(state: web::Data<State>, q: web::Query<EdgesQ>) -> impl Responder {
    let db = &state.db;
    let sport = q.sport.clone().filter(|s| s != "all");
    let status = match q.status.as_deref().unwrap_or("open") {
        "open" => "status='open' AND starts_at > now()", "settled" => "status IN ('settled','void')", _ => "TRUE",
    };
    let sql = format!("SELECT id, sport, game_id, game_label, home, away, starts_at, kind, market, subject, subject_name, side, line, book, price, link, fair_source, fair_books, \
        p_fair, p_book, ev, stake, observed_at, published_at, latest_price, latest_ev, updated_at, status, close_p_fair, close_ev, actual, result, profit, settled_at \
        FROM market_edges WHERE {status} AND ($1::text IS NULL OR sport=$1) ORDER BY {} LIMIT $2",
        if status.starts_with("status='open'") { "ev DESC" } else { "COALESCE(settled_at, published_at) DESC" });
    let rows = match db.query(&sql, &[&sport, &q.limit.unwrap_or(300).clamp(1, 1000)]).await { Ok(r) => r, Err(e) => return HttpResponse::InternalServerError().json(json!({ "error": dberr(&e) })) };
    let edges: Vec<Value> = rows.iter().map(|r| {
        let (market, side, line, home, away): (String, String, Option<f64>, Option<String>, Option<String>) = (r.get(8), r.get(11), r.get(12), r.get(4), r.get(5));
        let book: String = r.get(13);
        let (src, n): (String, i32) = (r.get(16), r.get(17));
        let sp: String = r.get(1);
        let mlabel = if crate::segments::is_game_market(&market) { crate::segments::market_label(&sp, &market) } else { stat_label(&market).to_string() };
        json!({ "id": r.get::<_, i64>(0), "sport": sp, "game_id": r.get::<_, String>(2), "game_label": r.get::<_, String>(3), "home": home, "away": away,
            "starts_at": r.get::<_, Option<DateTime<Utc>>>(6), "kind": r.get::<_, String>(7), "market": market, "market_label": mlabel,
            "subject": r.get::<_, String>(9), "subject_name": r.get::<_, String>(10), "side": side,
            "label": pick_label(&sp, &market, &side, line, home.as_deref().unwrap_or(""), away.as_deref().unwrap_or("")), "line": line,
            "book": book, "book_title": book_title(&book), "price": r.get::<_, i32>(14), "link": r.get::<_, Option<String>>(15),
            "fair_source": src, "fair_books": n, "fair_text": source_text(&src, n as usize),
            "p_fair": r3(r.get(18)), "p_book": r3(r.get(19)), "ev": r3(r.get(20)), "stake": r4(r.get(21)),
            "observed_at": r.get::<_, DateTime<Utc>>(22), "published_at": r.get::<_, DateTime<Utc>>(23),
            "latest": { "price": r.get::<_, Option<i32>>(24), "ev": r.get::<_, Option<f64>>(25).map(r3), "updated_at": r.get::<_, DateTime<Utc>>(26) },
            "status": r.get::<_, String>(27), "close_p_fair": r.get::<_, Option<f64>>(28).map(r3), "close_ev": r.get::<_, Option<f64>>(29).map(r3),
            "actual": r.get::<_, Option<f64>>(30), "result": r.get::<_, Option<String>>(31), "profit": r.get::<_, Option<f64>>(32).map(r3),
            "settled_at": r.get::<_, Option<DateTime<Utc>>>(33) })
    }).collect();
    let days = q.days.unwrap_or(90).clamp(1, 3650);
    let rec = db.query("SELECT fair_source, count(*) FILTER (WHERE result IN ('win','loss','push')), count(*) FILTER (WHERE result='win'), count(*) FILTER (WHERE result='loss'), \
        COALESCE(sum(profit) FILTER (WHERE result IN ('win','loss','push')), 0), sum(stake * profit) FILTER (WHERE result IN ('win','loss','push')), \
        sum(stake) FILTER (WHERE result IN ('win','loss','push')), avg(close_ev), count(close_ev), avg(ev) FILTER (WHERE result IS NOT NULL) \
        FROM market_edges WHERE status IN ('settled','void') AND settled_at > now() - make_interval(days => $2) AND ($1::text IS NULL OR sport=$1) \
        GROUP BY ROLLUP (fair_source) ORDER BY 1 NULLS FIRST", &[&sport, &days]).await;
    let record: Vec<Value> = rec.map(|rows| rows.iter().map(|r| {
        let (n, w, l, units): (i64, i64, i64, f64) = (r.get(1), r.get(2), r.get(3), r.get(4));
        let (ks, kst): (Option<f64>, Option<f64>) = (r.get(5), r.get(6));
        json!({ "fair_source": r.get::<_, Option<String>>(0), "graded": n, "wins": w, "losses": l, "pushes": n - w - l, "units": r3(units),
            "roi": if n > 0 { json!(r3(units / n as f64)) } else { Value::Null },
            "kelly_roi": match (ks, kst) { (Some(a), Some(b)) if b > 0.0 => json!(r3(a / b)), _ => Value::Null },
            "close_ev_mean": r.get::<_, Option<f64>>(7).map(r3), "close_ev_n": r.get::<_, i64>(8), "ev_mean": r.get::<_, Option<f64>>(9).map(r3) })
    }).collect()).unwrap_or_default();
    HttpResponse::Ok().json(json!({ "count": edges.len(), "edges": edges, "record": record, "days": days,
        "settings": { "min_ev": lines::setting(db, "stale_min_ev", 0.02).await, "max_ev": lines::setting(db, "stale_max_ev", 0.12).await,
            "consensus_books": lines::setting(db, "stale_consensus_books", 4.0).await, "kelly_fraction": lines::setting(db, "kelly_fraction", 0.25).await,
            "kelly_cap": lines::setting(db, "kelly_cap", 0.02).await } }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinnacle_wins_over_consensus_and_must_be_current() {
        let t = Utc::now();
        let old = t - chrono::Duration::hours(2);
        let refs: Vec<Ref> = vec![("pinnacle", -120, 110, t), ("draftkings", -110, -110, t), ("fanduel", -105, -115, t), ("betmgm", -110, -110, t), ("bovada", -110, -110, t)];
        let (p, src, n) = fair(&refs, "draftkings", t, 4).unwrap();
        assert_eq!((src, n), ("pinnacle", 1));
        assert!((p - novig(-120, 110)).abs() < 1e-12);
        // a stale Pinnacle quote is ignored; three other books are not enough for a consensus
        let refs2: Vec<Ref> = vec![("pinnacle", -120, 110, old), ("draftkings", -110, -110, t), ("fanduel", -105, -115, t), ("betmgm", -110, -110, t), ("bovada", -110, -110, t)];
        assert!(fair(&refs2, "draftkings", t, 4).is_none());
        let (p, src, n) = fair(&refs2, "draftkings", t, 3).unwrap();
        assert_eq!((src, n), ("consensus", 3));
        assert!((p - 0.5).abs() < 1e-12, "median of 0.512, 0.5, 0.5");
    }

    #[test]
    fn kelly_is_fractional_capped_and_never_negative() {
        // 55% at +100: full Kelly 10%, quarter 2.5%, capped at 2%
        assert!((kelly(0.55, 100, 0.25, 0.02) - 0.02).abs() < 1e-12);
        assert!((kelly(0.55, 100, 0.25, 0.05) - 0.025).abs() < 1e-12);
        assert_eq!(kelly(0.50, -110, 0.25, 0.02), 0.0);
        // 40% at +200: f* = (0.4*3 - 1)/2 = 0.1
        assert!((kelly(0.40, 200, 0.5, 1.0) - 0.05).abs() < 1e-12);
        assert_eq!(tier_stake_mult("unproven"), 0.0);
    }
}
