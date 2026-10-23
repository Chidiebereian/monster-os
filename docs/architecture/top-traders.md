# Top Traders and wallet profiles

The dashboard's **Top Traders** page (`/copy`) and **wallet profile** (`/wallets/:addr`)
read whale-tracker's `/pm/*` routes. Those routes wrap Polymarket's public
APIs, so no key is needed and nothing depends on ClickHouse.

| Route | Source | Notes |
|---|---|---|
| `GET /pm/leaderboard?category=&window=day\|week\|month\|all&order=pnl\|vol&limit=&offset=&q=` | data-api `/v1/leaderboard` | 11 categories (overall, politics, sports, crypto, culture, mentions, weather, economics, tech, finance, esports). `q` = name or 0x wallet. Adds the cached wallet stats and the rank change against the last earlier day seen. |
| `GET /pm/wallet/{addr}` | profile, value, traded, leaderboards, positions | Refreshes inline when older than 6 h; `?refresh=true` forces it. |
| `GET /pm/wallet/{addr}/positions?status=active\|resolved\|closed` | `/positions`, `/closed-positions` | `resolved` = settled but not redeemed. |
| `GET /pm/wallet/{addr}/activity?type=` | `/activity` | trades, redeems, splits, merges, rewards |
| `GET /pm/wallet/{addr}/pnl?range=1d\|1w\|1m\|1y\|ytd\|all` | `/v2/user-pnl` | `realized_pnl`: equals the leaderboard's all-time P&L to the cent. |
| `GET /pm/discover/whales?min_trade=&min_value=` | global `/trades` (cash filter) | large trades from wallets holding at least `min_value` in positions |
| `GET /pm/discover/insiders?min_trade=&max_age_days=` | global `/trades` + profile join date | large trades from new wallets |
| `GET /pm/discover/movers?category=&window=` | window board vs each wallet's all-time rank | `climb` = all-time rank − window rank; only wallets up all-time |

## The win rate counts unredeemed losses

Polymarket's `/closed-positions` lists only positions that were sold or
redeemed. A lost bet pays nothing to redeem, so most losers never are. They
stay in `/positions` at 0¢ with `redeemable: true`.

A win rate taken from closed positions alone skips most losses. For one top
sports wallet it comes out at 89% (519–63). Counting the 628 settled but
unredeemed positions gives 43% (519–690). On that basis, won minus lost
($62.3M − $53.4M) lands within rebates and rewards of the wallet's realized
P&L ($9.98M).

So the record is the wallet's most recent 1,000 closed positions plus every
settled-unredeemed position. Wins are positions with P&L above 0, losses below
0, and break-evens are left out.

## Hot streaks vs track records

A one-week board readily surfaces all-time losers. One wallet was #26 in sports
for the week (+$167K) and −$2.23M all-time. The leaderboard flags such wallets
("all-time −$2.2M"). Movers drops them unless called with `proven=false`.

## Cache

- **`pm_wallet_stats`** (migration 0024) stores two sets of fields:
  - **lite:** profile, positions value, markets traded, all-time rank, P&L and volume. 4 calls; kept for 1 h.
  - **full:** the record, active count and per-category standing. About 15–40 calls; kept for 6 h.
- **Refresh:** board views queue stale wallets for a background refresh, one at a time.
- **`pm_rank_history`:** keeps one rank per wallet per board per day, which gives the rank change.

The whole cache can be dropped; it refills on the next view.

## Running it

whale-tracker needs only `DATABASE_URL` and `PORT=8116`:

```
DATABASE_URL=postgres://monster:monster@localhost:5434/monsteros PORT=8116 ./target/debug/whale-tracker.exe
```

Its older routes (`/ingest`, `/trades`, `/candles`, …) still need ClickHouse.
The `/pm/*` routes don't.
