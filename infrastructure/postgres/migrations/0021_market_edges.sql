-- AI Picks: stakes and stale lines.
--
-- ai_picks.stake: the suggested stake as a fraction of bankroll, fractional
-- Kelly on the published (blended) probability, scaled by the engine's track
-- record. Written once at publish, like the rest of the published fields.
--
-- market_edges: model-free plays. A book whose price beats the sharp fair
-- price (Pinnacle de-vigged at the same line, else the consensus of the other
-- books in the same snapshot) by at least the minimum EV. Published once per
-- book x selection x line; latest_* follow the book until the start; settled
-- like picks, with the closing fair price for CLV.

ALTER TABLE ai_picks ADD COLUMN IF NOT EXISTS stake double precision;

CREATE TABLE IF NOT EXISTS market_edges (
    id              bigserial PRIMARY KEY,
    sport           text        NOT NULL,
    game_id         text        NOT NULL,
    game_label      text        NOT NULL,
    home            text,
    away            text,
    starts_at       timestamptz,
    kind            text        NOT NULL CHECK (kind IN ('game', 'prop')),
    market          text        NOT NULL,
    subject         text        NOT NULL,
    subject_name    text        NOT NULL,
    side            text        NOT NULL,
    line            double precision,               -- NULL for moneyline
    book            text        NOT NULL,
    price           integer     NOT NULL,
    link            text,
    fair_source     text        NOT NULL,           -- pinnacle | consensus
    fair_books      integer     NOT NULL,           -- books behind the fair price
    p_fair          double precision NOT NULL,      -- de-vigged sharp probability of this side
    p_book          double precision NOT NULL,      -- de-vigged at the book itself
    ev              double precision NOT NULL,      -- p_fair x decimal - 1
    stake           double precision NOT NULL,      -- fraction of bankroll
    observed_at     timestamptz NOT NULL,           -- when the book's price was seen
    published_at    timestamptz NOT NULL DEFAULT now(),
    latest_price    integer,
    latest_ev       double precision,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    status          text        NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'started', 'settled', 'void')),
    close_p_fair    double precision,
    close_ev        double precision,               -- the published price against the closing fair price
    actual          double precision,
    result          text,
    profit          double precision,
    settled_at      timestamptz,
    UNIQUE NULLS NOT DISTINCT (sport, game_id, market, subject, side, book, line)
);
CREATE INDEX IF NOT EXISTS market_edges_open ON market_edges (sport, status, starts_at);

-- pick_evidence source 'lines' (recorded real lines, blend.rs; written since
-- 0020) was missing from the check constraint.
ALTER TABLE pick_evidence DROP CONSTRAINT IF EXISTS pick_evidence_source_check;
ALTER TABLE pick_evidence ADD CONSTRAINT pick_evidence_source_check CHECK (source IN ('backtest', 'live', 'lines'));
