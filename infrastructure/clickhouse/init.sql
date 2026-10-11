-- MonsterOS time-series store (ClickHouse): ticks, odds, market history, sims.
CREATE DATABASE IF NOT EXISTS monster;

CREATE TABLE IF NOT EXISTS monster.odds_ticks (
    ts        DateTime64(3),
    book      LowCardinality(String),
    sport     LowCardinality(String),
    event_id  String,
    market    LowCardinality(String),   -- moneyline / spread / total / prop
    selection String,
    price     Float64,                  -- American odds
    line      Nullable(Float64)
) ENGINE = MergeTree ORDER BY (sport, event_id, market, ts);

CREATE TABLE IF NOT EXISTS monster.market_ticks (
    ts        DateTime64(3),
    venue     LowCardinality(String),   -- kalshi / polymarket
    market_id String,
    outcome   String,
    bid       Float64,
    ask       Float64,
    last      Float64,
    volume    Float64
) ENGINE = MergeTree ORDER BY (venue, market_id, ts);

CREATE TABLE IF NOT EXISTS monster.wallet_trades (
    ts            DateTime64(3),
    venue         LowCardinality(String),
    wallet        String,
    market_id     String,
    side          LowCardinality(String),
    price         Float64,
    size          Float64,
    outcome       String DEFAULT '',   -- which outcome the trade was on (e.g. team/YES)
    outcome_index Int8   DEFAULT -1     -- its index in the market's outcomes
) ENGINE = MergeTree ORDER BY (wallet, ts);

CREATE TABLE IF NOT EXISTS monster.simulation_results (
    created_at    DateTime64(3),
    simulation_id String,
    model_id      String,
    event_id      String,
    draw          UInt32,
    value         Float64
) ENGINE = MergeTree ORDER BY (simulation_id, event_id, draw);
