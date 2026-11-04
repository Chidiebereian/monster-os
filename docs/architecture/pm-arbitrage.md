# Prediction-market arbitrage

The Arbitrage page's **Prediction markets** mode finds the same futures outcome on Kalshi
and Polymarket where buying YES on one venue and NO on the other costs less than the $1
the pair pays, after both taker fees. It is research only: nothing places orders.

## Pipeline

| Step | Where | What |
| --- | --- | --- |
| Discovery | prediction-market-service `/futures/kalshi?series=`, `/futures/polymarket?tag=&contains=` | every open outcome of the futures events, with best bid and ask, rules text verbatim, resolution date, volume, fee schedule and the venue page |
| Families | arbitrage-engine `pm_arb.rs` | Kalshi series ticker from a fixed table (`KALSHI_FAMILIES`); Polymarket title from exact patterns only ("Pro Football: 2027 AFC Champion"). Anything else is not matched |
| Season | | Kalshi "-27" = Polymarket "2027"; a Polymarket title without a year uses its end date's year |
| Team | projection-service `/entities/resolve` | Kalshi: the ticker code (`KALSHI_CODES` maps JAC, LAR and NHL LA/NJ/SJ/TB to ours), never the city label. Polymarket: the full name must equal one of the team's names exactly |
| Dates | | the two resolution dates within 120 days; over 31 days is flagged |
| Screen | | best prices: YES = YES ask, NO = 1 − YES bid, plus fees; under $1 goes on |
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

## Not covered yet

- Game winners and player awards (only champion, conference and division futures).
- Predict.fun, Novig, ProphetX: API access and terms not reviewed.
- Kalshi's terms are still `review_pending` in `data_sources`, as is Polymarket's.
