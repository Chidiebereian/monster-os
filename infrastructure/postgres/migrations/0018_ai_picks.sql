-- AI Picks: market lines from several providers, picks priced from the
-- champion engines' frozen forecasts, and the evidence that caps each grade.
-- See docs/architecture/ai-picks.md.

-- Append-only snapshots of two-way market lines. One row per provider x book x
-- selection per fetch; the latest row per selection is the current price and
-- the last row before the start is the closing price (for CLV).
CREATE TABLE IF NOT EXISTS market_lines (
    id            bigserial PRIMARY KEY,
    source        text        NOT NULL,              -- espn | oddsapi | sgo
    sport         text        NOT NULL,              -- nfl | nba | nhl | mlb
    game_id       text,                              -- our game id (NULL when unmatched)
    event_ref     text        NOT NULL,              -- provider event id
    starts_at     timestamptz,
    kind          text        NOT NULL CHECK (kind IN ('game', 'prop')),
    market        text        NOT NULL,              -- moneyline | spread | total | pts | reb | sog | pass_yds ...
    subject       text        NOT NULL,              -- 'game' or our player id
    subject_name  text,
    provider_subject text,                           -- e.g. ESPN athlete id
    side          text        NOT NULL CHECK (side IN ('over', 'under', 'home', 'away')),
    line          double precision,                  -- NULL for moneyline; spread is the side's own spread
    price         integer     NOT NULL,              -- American odds
    book          text        NOT NULL,
    link          text,
    open_line     double precision,
    open_price    integer,
    observed_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_market_lines_sel
    ON market_lines (sport, game_id, market, subject, side, book, observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_market_lines_time ON market_lines (observed_at);

-- ESPN athlete id -> name/team/headshot (fetched once; ESPN ids differ from ours outside the NBA)
CREATE TABLE IF NOT EXISTS espn_athletes (
    sport      text NOT NULL,
    espn_id    text NOT NULL,
    name       text NOT NULL,
    team       text,
    headshot   text,
    fetched_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (sport, espn_id)
);

-- Provider request budgets (per UTC day)
CREATE TABLE IF NOT EXISTS provider_usage (
    provider text NOT NULL,
    day      date NOT NULL,
    requests integer NOT NULL DEFAULT 0,
    units    integer NOT NULL DEFAULT 0,       -- credits / entities as the provider counts them
    PRIMARY KEY (provider, day)
);

-- One pick per selection per game. The published_* fields are written once, at
-- the first price that qualified, and are what the track record is scored on;
-- latest_* follow the market until the start.
CREATE TABLE IF NOT EXISTS ai_picks (
    id              bigserial PRIMARY KEY,
    sport           text        NOT NULL,
    engine          text        NOT NULL,      -- learning-loop engine key (nba, nba_team ...)
    model_version   text        NOT NULL,
    ledger_id       bigint      NOT NULL REFERENCES prediction_ledger (id),
    game_id         text        NOT NULL,
    game_label      text        NOT NULL,
    home            text,
    away            text,
    starts_at       timestamptz,
    kind            text        NOT NULL CHECK (kind IN ('game', 'prop')),
    market          text        NOT NULL,
    subject         text        NOT NULL,
    subject_name    text        NOT NULL,
    subject_team    text,
    headshot        text,
    side            text        NOT NULL,
    -- as published (immutable)
    line            double precision,
    book            text        NOT NULL,
    price           integer     NOT NULL,
    link            text,
    p_model         double precision NOT NULL,
    p_market        double precision NOT NULL, -- de-vigged, same book pair
    edge            double precision NOT NULL, -- p_model - p_market
    ev              double precision NOT NULL, -- expected profit per unit at the price
    projection      double precision,          -- model mean of the stat (or margin / total)
    grade           text        NOT NULL,
    tier            text        NOT NULL,      -- proven | baseline | unproven | losing
    facts           jsonb       NOT NULL,
    risk_flags      jsonb       NOT NULL DEFAULT '[]',
    writeup         text        NOT NULL,
    writeup_source  text        NOT NULL,      -- template | llm:<model>
    published_at    timestamptz NOT NULL DEFAULT now(),
    -- moving
    latest_line     double precision,
    latest_price    integer,
    latest_book     text,
    latest_p_market double precision,
    latest_edge     double precision,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    -- settlement
    status          text        NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'started', 'settled', 'void')),
    close_line      double precision,
    close_price     integer,
    close_p_market  double precision,
    clv             double precision,          -- close_p_market - p_market (same line only)
    actual          double precision,
    result          text CHECK (result IN ('win', 'loss', 'push', 'void')),
    profit          double precision,          -- units, 1u stake at the published price
    settled_at      timestamptz,
    UNIQUE (sport, game_id, market, subject, side, engine)
);
CREATE INDEX IF NOT EXISTS idx_ai_picks_open ON ai_picks (status, starts_at);
CREATE INDEX IF NOT EXISTS idx_ai_picks_market ON ai_picks (sport, kind, market);

-- What a grade may claim. 'backtest' rows come from multi-season walk-forward
-- backtests; 'live' rows are recomputed from settled picks.
CREATE TABLE IF NOT EXISTS pick_evidence (
    engine      text NOT NULL,                  -- learning-loop engine key (nba, nba_team ...)
    market      text NOT NULL,                  -- moneyline | spread | total | prop stat key
    source      text NOT NULL CHECK (source IN ('backtest', 'live')),
    comparison  text NOT NULL,                  -- 'market' (vs real lines) | 'baseline' (vs a naive model)
    n           integer NOT NULL,
    seasons     integer NOT NULL DEFAULT 0,
    better      integer NOT NULL DEFAULT 0,     -- seasons beyond 2 SE better
    worse       integer NOT NULL DEFAULT 0,     -- seasons beyond 2 SE worse
    metric      double precision,               -- Brier diff (model - comparison) or ROI
    se          double precision,
    detail      jsonb NOT NULL DEFAULT '{}',
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (engine, market, source, comparison)
);
