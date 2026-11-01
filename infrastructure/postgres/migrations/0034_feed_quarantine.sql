-- Odds feeds whose settled results are impossible for their own prices (a
-- provider's quote filed under the wrong line or market). Written by the
-- validation engine (projection-service validation.rs) for any sport, market
-- and book; AI Picks skips quarantined quotes, and evidence, the track record
-- and the scorecard leave their picks out.
CREATE TABLE IF NOT EXISTS feed_quarantine (
    sport       text        NOT NULL,
    market      text        NOT NULL,
    book        text        NOT NULL,
    n           integer     NOT NULL,
    implied     double precision NOT NULL,
    hit_rate    double precision NOT NULL,
    z           double precision NOT NULL,
    reason      text        NOT NULL,
    detected_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (sport, market, book)
);
