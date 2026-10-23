-- Smart Signals on Polymarket data (whale-tracker signals.rs).

-- A wallet's standing in one Polymarket leaderboard category (all-time).
-- source: 'board' (in the category's top lists) or 'probe' (looked up because
-- the wallet traded a market in that category).
CREATE TABLE IF NOT EXISTS pm_standings (
    wallet       text NOT NULL,
    category     text NOT NULL,
    rank         integer,
    pnl          double precision,
    vol          double precision,
    month_pnl    double precision,
    source       text NOT NULL,
    refreshed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (wallet, category)
);

-- Tracked sharp wallets and their trading habits.
CREATE TABLE IF NOT EXISTS pm_sharp_wallets (
    wallet        text PRIMARY KEY,
    name          text,
    profile_image text,
    best_category text,
    median_trade  double precision,
    trades_seen   integer,
    span_minutes  double precision,
    bot           boolean NOT NULL DEFAULT false,
    polled_at     timestamptz
);

CREATE TABLE IF NOT EXISTS pm_sharp_trades (
    tx_hash      text NOT NULL,
    asset        text NOT NULL,
    wallet       text NOT NULL,
    side         text NOT NULL,
    price        double precision NOT NULL,
    size         double precision NOT NULL,
    ts           timestamptz NOT NULL,
    condition_id text,
    outcome      text,
    title        text,
    slug         text,
    event_slug   text,
    icon         text,
    PRIMARY KEY (tx_hash, asset, wallet, side, price, size)
);
CREATE INDEX IF NOT EXISTS pm_sharp_trades_ts ON pm_sharp_trades (ts);

CREATE TABLE IF NOT EXISTS pm_event_cats (
    event_slug text PRIMARY KEY,
    category   text NOT NULL,
    tags       jsonb,
    fetched_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS pm_signals (
    id             bigserial PRIMARY KEY,
    wallet         text NOT NULL,
    asset          text NOT NULL,
    condition_id   text,
    category       text,
    title          text,
    slug           text,
    event_slug     text,
    icon           text,
    outcome        text,
    entry_price    double precision,
    entry_usdc     double precision,
    entry_shares   double precision,
    first_trade_at timestamptz,
    last_trade_at  timestamptz,
    current_price  double precision,
    price_at_signal double precision,
    slippage       double precision,
    depth_2c       double precision,
    rel_size       double precision,
    wallet_score   integer,
    score          integer,
    best_score     integer,
    precision_ok   boolean,
    flow_ok        boolean,
    parts          jsonb,
    flags          jsonb,
    status         text NOT NULL DEFAULT 'active',
    close_reason   text,
    first_seen     timestamptz NOT NULL DEFAULT now(),
    last_seen      timestamptz NOT NULL DEFAULT now(),
    closed_at      timestamptz,
    result         text,
    final_price    double precision,
    UNIQUE (wallet, asset)
);
CREATE INDEX IF NOT EXISTS pm_signals_status ON pm_signals (status, score DESC);
