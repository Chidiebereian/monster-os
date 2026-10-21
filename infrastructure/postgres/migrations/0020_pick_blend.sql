-- AI Picks: market-anchored blending.
--
-- Every selection the picks builder prices (not only published picks) is
-- recorded here with the engine's raw probability, the naive baseline (props:
-- the player's own prior frequency over the line) and the de-vigged market
-- consensus, kept current until the start (so the last value is the close).
-- After the game it is scored. These rows fit how far each engine's
-- probability should move away from the market (pick_evidence source 'lines')
-- and decide whether the blended model beats real closing lines.
--
-- Orientation: P(over) for totals and props, P(home) for moneyline and spread
-- (line = the home line; moneyline line = 0). Pushes are excluded.

CREATE TABLE IF NOT EXISTS pick_line_evals (
    engine         text NOT NULL,
    engine_version text NOT NULL,
    sport          text NOT NULL,
    game_id        text NOT NULL,
    market         text NOT NULL,
    subject        text NOT NULL,
    line           double precision NOT NULL,
    starts_at      timestamptz,
    ledger_id      bigint,
    p_model        double precision NOT NULL,
    p_naive        double precision,
    p_market       double precision NOT NULL,
    books          integer NOT NULL,
    observed_at    timestamptz NOT NULL DEFAULT now(),
    actual         double precision,
    y              double precision,
    settled_at     timestamptz,
    PRIMARY KEY (engine, game_id, market, subject, line)
);

CREATE INDEX IF NOT EXISTS pick_line_evals_open ON pick_line_evals (sport, starts_at) WHERE settled_at IS NULL;
CREATE INDEX IF NOT EXISTS pick_line_evals_fit ON pick_line_evals (engine, engine_version, market) WHERE y IS NOT NULL;
