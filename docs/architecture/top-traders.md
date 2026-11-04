# Top Traders and wallet profiles

The dashboard's **Top Traders** page (`/copy`) and **wallet profile** (`/wallets/:addr`)
read whale-tracker's `/pm/*` routes. Those routes wrap Polymarket's public
APIs, so no key is needed and nothing depends on ClickHouse.

Polymarket retired Data API v1 and `lb-api` on 2026-11-02; every v1 path answers
HTTP 410. whale-tracker reads Data API v2 since 2026-11-04 (see "Data API v2" below).

| Route | Source | Notes |
|---|---|---|
| `GET /pm/leaderboard?category=&window=day\|week\|month\|all&order=pnl\|vol&limit=&offset=&q=` | `/v2/leaderboard` | 11 categories (overall, politics, sports, crypto, culture, mentions, weather, economics, tech, finance, esports). `q` = 0x wallet, or a name matched within the board's first 1,000 rows (v2 has no name search). Adds the cached wallet stats and the rank change against the last earlier day seen. |
| `GET /pm/wallet/{addr}` | profile, `/v2/value`, `/v2/user-volume`, leaderboards, positions | Refreshes inline when older than 6 h; `?refresh=true` forces it. |
| `GET /pm/wallet/{addr}/positions?status=active\|resolved\|closed` | `/v2/positions` with `status` OPEN, REDEEMABLE + REDEEMABLE_LOST, or CLOSED | `resolved` = settled but not redeemed. |
| `GET /pm/wallet/{addr}/activity?type=` | `/v2/activity` | trades, redeems, splits, merges, rewards |
| `GET /pm/wallet/{addr}/pnl?range=1d\|1w\|1m\|1y\|ytd\|all` | `/v2/user-pnl` | `realized_pnl`: equals the leaderboard's all-time P&L to the cent. |
| `GET /pm/discover/whales?min_trade=&min_value=` | global `/v2/trades` (cash filter) | large trades from wallets holding at least `min_value` in positions |
| `GET /pm/discover/insiders?min_trade=&max_age_days=` | global `/v2/trades` + profile join date | large trades from new wallets |
| `GET /pm/discover/movers?category=&window=` | window board vs each wallet's all-time rank | `climb` = all-time rank − window rank; only wallets up all-time |

## The win rate counts unredeemed losses

Polymarket's closed positions (v2 `status=CLOSED`) are only positions that were sold
or redeemed. A lost bet pays nothing to redeem, so most losers never are. They stay
open at 0¢; v2 lists them under `status=REDEEMABLE_LOST` (unredeemed winners stay in
`OPEN` with `redeemable: true`).

A win rate taken from closed positions alone skips most losses. For one top
sports wallet it comes out at 89% (519–63). Counting the 628 settled but
unredeemed positions gives 43% (519–690). On that basis, won minus lost
($62.3M − $53.4M) lands within rebates and rewards of the wallet's realized
P&L ($9.98M).

So the record is the wallet's most recent 1,000 closed positions plus its
settled-unredeemed positions, at most 5,000 of each list. For the largest wallets
(RN1 has over 5,000 unredeemed losers) the record is therefore a sample, in v1 as in v2. Wins are positions with P&L above 0, losses below
0, and break-evens are left out.

## Hot streaks vs track records

A one-week board readily surfaces all-time losers. One wallet was #26 in sports
for the week (+$167K) and −$2.23M all-time. The leaderboard flags such wallets
("all-time −$2.2M"). Movers drops them unless called with `proven=false`.

## Data API v2

`pm.rs` reads v2 through three helpers. `v2_rows` walks the cursor, since v2 has no
offset, and resends the query with every page. `v2_data` unwraps the `data` envelope.
`legacy` adds the v1 field names (`proxyWallet`, `conditionId`, `asset`, `curPrice`,
`cashPnl`, ...) to each row, so the parsing code did not change.

Volume units:

| Field | Unit | Source |
| --- | --- | --- |
| `all_vol` (wallet stats), `trader_performance.volume` | USD | `/v2/user-volume` `volume_usdc` |
| board and movers rows `vol_shares`, category `vol_shares`, `pm_standings.vol` | shares | `/v2/leaderboard` `volume` |

v1's leaderboard `vol` was the same share count. A wallet cached on 2026-11-02 shows
sports 1.333B against v2's 1.358B shares two days later, while its USD volume is
0.71B. The dashboard used to show that share count as dollars; it now labels it `sh`.
Board rows carry `vol: null`.

v1's `/traded` (markets traded) has no v2 equivalent: `markets_traded` keeps its last
v1 value and stays empty for new wallets.

## Cache

- **`pm_wallet_stats`** (migration 0024) stores two sets of fields:
  - **lite:** profile, positions value, all-time rank, P&L and USD volume. 4 calls; kept for 1 h.
  - **full:** the record, active count and per-category standing. About 15–40 calls; kept for 6 h.
- **Refresh:** board views queue stale wallets for a background refresh, one at a time.
- **`pm_rank_history`:** keeps one rank per wallet per board per day, which gives the rank change.

The whole cache can be dropped; it refills on the next view.

## Running it

whale-tracker needs only `DATABASE_URL` (the service role, `APP_DATABASE_URL` in `.env`) and `PORT=8116`:

```
DATABASE_URL="$APP_DATABASE_URL" PORT=8116 ./target/debug/whale-tracker.exe
```

Its older routes (`/ingest`, `/trades`, `/candles`, …) still need ClickHouse.
The `/pm/*` routes don't.
