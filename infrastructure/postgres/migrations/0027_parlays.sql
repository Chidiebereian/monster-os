-- Parlay Builder: generated and saved tickets, their grading, and the parlay
-- model's version history (dependence weight). See docs/architecture/parlay-builder.md.

CREATE TABLE IF NOT EXISTS parlay_models (
    id           bigserial PRIMARY KEY,
    version      text NOT NULL UNIQUE,
    dep_weight   double precision NOT NULL CHECK (dep_weight BETWEEN 0 AND 1),
    status       text NOT NULL CHECK (status IN ('champion', 'candidate', 'rejected', 'retired')),
    parent       text,
    evidence     jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at   timestamptz NOT NULL DEFAULT now(),
    promoted_at  timestamptz,
    retired_at   timestamptz
);
CREATE UNIQUE INDEX IF NOT EXISTS parlay_models_one_champion ON parlay_models ((status)) WHERE status = 'champion';
INSERT INTO parlay_models (version, dep_weight, status, evidence, promoted_at)
VALUES ('parlay-joint-v1+dep1.00', 1.0, 'champion', '{"note": "initial model: full simulated dependence between same-game legs"}', now())
ON CONFLICT (version) DO NOTHING;

CREATE TABLE IF NOT EXISTS parlay_tickets (
    id               bigserial PRIMARY KEY,
    ticket_key       text NOT NULL,
    sport            text NOT NULL,
    source           text NOT NULL CHECK (source IN ('auto', 'custom')),
    category         text,
    objective        text,
    n_legs           int NOT NULL,
    same_game        boolean NOT NULL,
    slate_date       date NOT NULL,
    starts_at        timestamptz,
    book             text,
    -- prediction-time numbers (immutable)
    price_decimal    double precision NOT NULL,
    price_verified   boolean NOT NULL,
    p_joint          double precision NOT NULL,
    p_dep            double precision NOT NULL,
    p_indep          double precision NOT NULL,
    p_raw            double precision NOT NULL,
    p_market         double precision NOT NULL,
    p_market_indep   double precision,
    ev               double precision NOT NULL,
    ev_cons          double precision NOT NULL,
    mc_se            double precision NOT NULL,
    lift             double precision NOT NULL,
    dep_weight       double precision NOT NULL,
    model_version    text NOT NULL,
    engines          jsonb NOT NULL,
    legs             jsonb NOT NULL,
    snapshot         jsonb NOT NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    -- the latest price before the start (closing comparison), updated while open
    latest_price_decimal double precision,
    latest_p_joint   double precision,
    latest_at        timestamptz,
    -- saved / personal list (custom tickets)
    saved            boolean NOT NULL DEFAULT false,
    hidden           boolean NOT NULL DEFAULT false,
    note             text,
    -- grading
    status           text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'settled', 'void')),
    result           text CHECK (result IN ('win', 'loss', 'push', 'void')),
    payout           double precision,
    profit           double precision,
    legs_result      jsonb,
    settled_at       timestamptz
);
-- one auto ticket per distinct combination (legs + book), so it is graded once
CREATE UNIQUE INDEX IF NOT EXISTS parlay_tickets_auto_key ON parlay_tickets (ticket_key) WHERE source = 'auto';
CREATE INDEX IF NOT EXISTS parlay_tickets_open ON parlay_tickets (sport, status, starts_at);
CREATE INDEX IF NOT EXISTS parlay_tickets_date ON parlay_tickets (sport, slate_date);

-- prediction-time fields cannot change after the ticket is recorded
CREATE OR REPLACE FUNCTION parlay_tickets_immutable() RETURNS trigger AS $$
BEGIN
    IF NEW.snapshot IS DISTINCT FROM OLD.snapshot OR NEW.legs IS DISTINCT FROM OLD.legs OR NEW.p_joint IS DISTINCT FROM OLD.p_joint
       OR NEW.price_decimal IS DISTINCT FROM OLD.price_decimal OR NEW.ev IS DISTINCT FROM OLD.ev OR NEW.created_at IS DISTINCT FROM OLD.created_at
       OR NEW.model_version IS DISTINCT FROM OLD.model_version THEN
        RAISE EXCEPTION 'parlay_tickets: prediction-time fields are immutable (ticket %)', OLD.id;
    END IF;
    RETURN NEW;
END $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS parlay_tickets_immutable ON parlay_tickets;
CREATE TRIGGER parlay_tickets_immutable BEFORE UPDATE ON parlay_tickets FOR EACH ROW EXECUTE FUNCTION parlay_tickets_immutable();

CREATE TABLE IF NOT EXISTS parlay_runs (
    id           bigserial PRIMARY KEY,
    sport        text NOT NULL,
    slate_date   date NOT NULL,
    started_at   timestamptz NOT NULL DEFAULT now(),
    finished_at  timestamptz,
    ok           boolean,
    detail       jsonb NOT NULL DEFAULT '{}'::jsonb
);
