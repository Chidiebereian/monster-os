-- Copy-trading bots (whale-tracker bots.rs): named strategies that each follow
-- one Polymarket wallet. Paper only: copies are simulated at the live CLOB
-- price when the leader's trade is seen; no order is ever routed.

CREATE TABLE IF NOT EXISTS copy_bots (
    id              bigserial PRIMARY KEY,
    strategy        text NOT NULL,
    wallet          text NOT NULL,
    trader_name     text,
    trader_image    text,
    status          text NOT NULL DEFAULT 'active',   -- active | paused | expired
    config          jsonb NOT NULL,
    -- newest leader trade timestamp already processed (unix seconds)
    cursor_ts       bigint NOT NULL DEFAULT 0,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    last_checked_at timestamptz,
    last_trade_at   timestamptz,
    last_error      text
);
CREATE INDEX IF NOT EXISTS copy_bots_wallet ON copy_bots (wallet);

-- Every leader trade a bot saw, with what the bot did about it. Doubles as
-- the alert feed (seen = false until acknowledged in the dashboard).
CREATE TABLE IF NOT EXISTS copy_bot_events (
    id            bigserial PRIMARY KEY,
    bot_id        bigint NOT NULL REFERENCES copy_bots(id) ON DELETE CASCADE,
    trade_ts      timestamptz NOT NULL,
    tx            text NOT NULL,
    asset         text NOT NULL,
    condition_id  text,
    side          text NOT NULL,                     -- BUY | SELL
    title         text,
    slug          text,
    event_slug    text,
    icon          text,
    outcome       text,
    category      text,
    trader_price  double precision,
    trader_shares double precision,
    trader_usdc   double precision,
    action        text NOT NULL,                     -- copied | skipped
    copy_price    double precision,
    copy_shares   double precision,
    copy_usdc     double precision,
    reason        text,
    seen          boolean NOT NULL DEFAULT false,
    created_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (bot_id, tx, asset, side)
);
CREATE INDEX IF NOT EXISTS copy_bot_events_bot ON copy_bot_events (bot_id, trade_ts);
CREATE INDEX IF NOT EXISTS copy_bot_events_unseen ON copy_bot_events (id) WHERE NOT seen;
