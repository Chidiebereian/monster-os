# Smart Signals (Polymarket)

The Smart Signals page (`/signals`) shows trades from Polymarket wallets that
have proven themselves in the category of the trade, and only while each trade
can still be copied (tailed) cleanly. It is built by whale-tracker's
`signals.rs` from Polymarket's public APIs. No keys or ClickHouse are needed.
(The old sport-intelligence version is no longer used by the page.)

> The strongest signal is a good wallet, in its proven category, at a still-clean price.

## Cycle

It runs every `PM_SIGNALS_INTERVAL_SECS` (default 600; set 0 to turn it off), or
on demand with `POST /pm/signals/run`. A cycle takes 3–4 minutes.

1. **Sharp set** (refreshed every 6 h into `pm_standings`). For each of the 10
   categories, take the top 50 all-time P&L wallets plus the top 50 this month.
   Month leaders are kept only if they are also up all-time in that category.
   That gives about 870 rows and 780 distinct wallets.
2. **Poll trades.** Each cycle polls the 400 sharps polled longest ago (latest 100
   trades each), one request at a time at about 6 per second. Polymarket's
   Cloudflare blocks an IP for a while (error 1015) after bursts, so a hard 429
   ends polling for that cycle. Each wallet's **normal bet** is the median dollars
   it put into one market. It is not the median fill, because one order often
   fills in many small pieces.
3. **Positions.** Buys from the last 48 h are grouped per (wallet, outcome token)
   into a VWAP entry, dollars and shares. Up to 600 are checked, live signals
   first.
4. **Context:**
   - **Category:** from the event's Polymarket tags, most specific first:
     esports before sports, Fed before Trump.
   - **Standing:** the wallet's all-time standing in that category and overall.
     This is looked up and cached for 24 h when the wallet is not on that board.
   - **Order book:** the live best ask and the dollars offered within 2¢ of it.
5. **Score**, then upsert into `pm_signals`. Positions that are no longer
   tail-able move to history with a reason. Resolved markets get `won`, `lost` or
   `void`.

## Score (0–100)

| Part | Points | Full marks when |
|---|---|---|
| Category proof | 30 | Up all-time in this category; rank near the top (log scale to #5,000); return on volume ≥ 8% (P&L per share traded: Polymarket reports category volume in shares). **0 if not up all-time in this category.** |
| Overall record | 10 | Up all-time overall; high overall rank |
| Slippage | 20 | Best ask within 0.5¢ of the trader's entry. Paying more costs fully. A drop below entry costs half as much: the market moved against them. |
| Relative size | 15 | 10× the wallet's normal bet (1× = 30%) |
| Position value | 5 | $10K+ (log scale from $100) |
| Liquidity | 10 | $5K+ within 2¢ of the ask ($200 or less = 0) |
| Recency | 5 | Bought just now (0 at 48 h) |
| Entry price | 5 | Between 8¢ and 92¢ (outside: 30%) |

**Wallet score** = 75% category proof + 25% overall record.

**Flags:**
- `unproven_in_category`: not up all-time in this category.
- `arber_or_maker`: more than 5M shares of volume at under 0.5% return on it (P&L per share).
- `bot_pace`: 100 fills within 30 minutes at a small per-market size.

## Modes

- **Precision:**
  - proven in the category, and no arber or bot flag;
  - best ask between 5¢ below and 3¢ above the entry;
  - at least $500 within 2¢ of the ask;
  - score ≥ 70.
- **Flow:** score ≥ 50 and best ask between 10¢ below and 8¢ above the entry.

## Lifecycle: why a signal leaves the feed

- **Market closed or resolved:** there is no order book.
- **Trader exited:** they sold at least 80% of the shares.
- **No room left in price:** the best ask is at least 98¢ or at most 2¢.
- **Price ran away from entry:** more than 10¢ above it.
- **Price collapsed below entry:** more than 15¢ below it.
- **Below flow threshold.**
- **Stale:** no buys in 48 h.

## Consensus

Consensus lists outcomes where at least N tracked sharps hold flow-qualified
signals. It also counts sharps on the **other** side of the same market. The
score is 60% average wallet score plus 40% net breadth (sharps on this side
minus the other; 5 more for full marks). A market split 10 vs 8 scores in the
30s even though many sharps are involved.

## Track record

`GET /pm/signals/record` reports, for each mode, how many resolved signals won
and the ROI of tailing $1 at the best ask when each was first shown.

## Routes

```
GET  /pm/signals?mode=precision|flow&category=&min_score=&limit=&wallet=
GET  /pm/signals/consensus?category=&min_wallets=
GET  /pm/signals/history?category=&limit=
GET  /pm/signals/record
GET  /pm/signals/chart?asset=<token id>&range=1d|1w|1m   (CLOB prices-history)
GET  /pm/signals/status
POST /pm/signals/run[?wait=true]
```

## Not yet

**Best entry across Polymarket and Kalshi** is not covered. sport-intelligence
has the Polymarket→Kalshi game matcher, but that service and
prediction-market-service are not running. Folding the Kalshi price into the
card is the next step once they are.
