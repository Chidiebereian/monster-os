# Prediction-market arbitrage

The Arbitrage page's **Prediction markets** mode finds the same outcome (a futures market
or a game winner) on Kalshi and Polymarket where buying YES on one venue and NO on the other costs less than the $1
the pair pays, after both taker fees. It is research only: nothing places orders.

## Pipeline

| Step | Where | What |
| --- | --- | --- |
| Discovery | prediction-market-service `/futures/kalshi?series=`, `/futures/polymarket?tag=&contains=`, `/games/kalshi?series=`, `/games/polymarket?league=` | every open outcome of the futures events, with best bid and ask, rules text verbatim, resolution date, volume, fee schedule and the venue page |
| Families | arbitrage-engine `pm_arb.rs` | Kalshi series ticker from a fixed table (`KALSHI_FAMILIES`); Polymarket title from exact patterns only ("Pro Football: 2027 AFC Champion"). Anything else is not matched |
| Games | | Kalshi game series (KXNBAGAME, KXNFLGAME, KXNHLGAME, KXMLBGAME) and Polymarket moneyline markets; a game is its league, US date (Kalshi's event ticker, Polymarket's start time) and both teams; each venue's event must resolve to exactly two teams; games that have started are skipped. Polymarket's second team trades on the first team's book, mirrored (`complement`) |
| Season | | Kalshi "-27" = Polymarket "2027"; a Polymarket title without a year uses its end date's year |
| Team | projection-service `/entities/resolve` | Kalshi: the ticker code (`KALSHI_CODES` maps JAC, LAR and NHL LA/NJ/SJ/TB to ours), never the city label. Polymarket: the label must equal one of the team's names, or be the name's first words in full ("Utah" = Utah Mammoth), for exactly one team in the league; fuzzy and partial matches are rejected. Only successful lookups are cached |
| Dates | | the two resolution dates within 120 days; over 31 days is flagged |
| Screen | | best prices: YES = YES ask, NO = 1 − YES bid, plus fees; under $1 goes on. Shown only with at least 0.1¢ edge per pair and one full pair of depth |
| Depth | `/orderbook/{venue}/{id}` | both books walked level by level while a pair still costs under $1 with fees: contracts, cost, fees, profit, and the steps the calculator uses |

The scan runs every `PM_ARB_INTERVAL_SECS` (default 180, 0 = off) and on `POST /pm-arb/refresh`
(at most every 30 s); it takes about 18 s. `GET /pm-arb` returns the latest result:
opportunities, the closest non-arb pairs, counts of what did not match and why, and any
venue errors. "Age" is how long a pair has been an arb since the service started.

## Fees

Kalshi: 0.07 × P × (1 − P) per contract, rounded up to the cent per order by Kalshi (not
modelled: small orders pay slightly more). Polymarket: the market's `feeSchedule` when
`feesEnabled`; a market that does not state it is "fee unknown", shown and not counted.

## Why matching is strict

The first version resolved Kalshi's "Los Angeles C" through the name resolver, which
matched the Lakers ("LA Clippers" was ESPN's only name for the Clippers), and paired
Kalshi's Clippers market with Polymarket's Lakers market as a $759 "arb". Kalshi outcomes
now match by ticker code, Polymarket outcomes by exact name, and the resolver knows
"Los Angeles Clippers", "Utah Hockey Club" and "A's" (entities.rs `LOCATIONS`, `OTHER_NAMES`).

## Game rows

Every game row carries an "abandoned or postponed game" check: Polymarket's game rules
have no-result provisions, and Kalshi resolves on the game "originally scheduled" for the
date; the venues can settle an abandoned game differently. NFL rows also flag ties.

## Not covered yet

- Player awards and other futures (only champion, conference and division).
- Arbs that last less than a scan (3 minutes): game prices move fast.
- Predict.fun, Novig, ProphetX: API access and terms not reviewed.
- Kalshi's terms are still `review_pending` in `data_sources`, as is Polymarket's.
