-- MonsterOS core schema (PostgreSQL): metadata, registry, features, trading.

CREATE TABLE IF NOT EXISTS models (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    sport       TEXT NOT NULL,
    version     TEXT NOT NULL,
    type        TEXT NOT NULL CHECK (type IN (
                  'poisson','negative_binomial','elo','bradley_terry','bootstrap',
                  'monte_carlo','bayesian','prp','toor','gssd','zsd')),
    status      TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft','active','retired')),
    artifact_uri TEXT,               -- MinIO path to posterior samples (Parquet)
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (name, version)
);

-- Context-aware team ratings: one row per (team, component), e.g.
-- NFL pass_offense, NHL power_play, MLB run1..run4/pitching.
CREATE TABLE IF NOT EXISTS team_components (
    sport       TEXT NOT NULL,
    team        TEXT NOT NULL,
    component   TEXT NOT NULL,
    rating      DOUBLE PRECISION NOT NULL,
    rating_sd   DOUBLE PRECISION,
    model_id    TEXT REFERENCES models(id),
    as_of       TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (sport, team, component, as_of)
);

-- Feature store
CREATE TABLE IF NOT EXISTS team_features (
    sport TEXT NOT NULL, team TEXT NOT NULL, as_of TIMESTAMPTZ NOT NULL,
    features JSONB NOT NULL, PRIMARY KEY (sport, team, as_of));
CREATE TABLE IF NOT EXISTS player_features (
    sport TEXT NOT NULL, player_id TEXT NOT NULL, as_of TIMESTAMPTZ NOT NULL,
    features JSONB NOT NULL, PRIMARY KEY (sport, player_id, as_of));
CREATE TABLE IF NOT EXISTS market_features (
    venue TEXT NOT NULL, market_id TEXT NOT NULL, as_of TIMESTAMPTZ NOT NULL,
    features JSONB NOT NULL, PRIMARY KEY (venue, market_id, as_of));
CREATE TABLE IF NOT EXISTS simulation_features (
    simulation_id TEXT NOT NULL, model_id TEXT REFERENCES models(id),
    event_id TEXT NOT NULL, mean DOUBLE PRECISION, median DOUBLE PRECISION,
    p10 DOUBLE PRECISION, p90 DOUBLE PRECISION, created_at TIMESTAMPTZ DEFAULT now(),
    PRIMARY KEY (simulation_id, event_id));
CREATE TABLE IF NOT EXISTS posterior_features (
    model_id TEXT REFERENCES models(id), parameter TEXT NOT NULL,
    mean DOUBLE PRECISION, sd DOUBLE PRECISION, q05 DOUBLE PRECISION, q95 DOUBLE PRECISION,
    rhat DOUBLE PRECISION, ess DOUBLE PRECISION, PRIMARY KEY (model_id, parameter));

-- Data quality: every dataset must pass before it reaches the feature store.
CREATE TABLE IF NOT EXISTS data_quality_runs (
    id          BIGSERIAL PRIMARY KEY,
    dataset     TEXT NOT NULL,
    sport       TEXT,
    passed      BOOLEAN NOT NULL,
    row_count   INTEGER NOT NULL,
    report      JSONB NOT NULL,       -- full 7-check report from data-quality-service
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Users, orders, copy trading
CREATE TABLE IF NOT EXISTS users (
    id BIGSERIAL PRIMARY KEY, email TEXT UNIQUE NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now());
CREATE TABLE IF NOT EXISTS subscriptions (
    user_id BIGINT REFERENCES users(id), plan TEXT NOT NULL,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(), ends_at TIMESTAMPTZ);
CREATE TABLE IF NOT EXISTS orders (
    id BIGSERIAL PRIMARY KEY, user_id BIGINT REFERENCES users(id),
    venue TEXT NOT NULL, market_id TEXT NOT NULL, side TEXT NOT NULL,
    price DOUBLE PRECISION NOT NULL, size DOUBLE PRECISION NOT NULL,
    model_id TEXT REFERENCES models(id), edge DOUBLE PRECISION,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now());
CREATE TABLE IF NOT EXISTS trader_profiles (
    wallet TEXT PRIMARY KEY, venue TEXT NOT NULL, label TEXT,
    first_seen TIMESTAMPTZ NOT NULL DEFAULT now());
CREATE TABLE IF NOT EXISTS trader_performance (
    wallet TEXT REFERENCES trader_profiles(wallet), as_of DATE NOT NULL,
    win_rate DOUBLE PRECISION, edge DOUBLE PRECISION, pnl DOUBLE PRECISION,
    risk DOUBLE PRECISION, trades INTEGER, PRIMARY KEY (wallet, as_of));
CREATE TABLE IF NOT EXISTS copied_positions (
    id BIGSERIAL PRIMARY KEY, user_id BIGINT REFERENCES users(id),
    wallet TEXT REFERENCES trader_profiles(wallet), market_id TEXT NOT NULL,
    size DOUBLE PRECISION NOT NULL, opened_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at TIMESTAMPTZ);
