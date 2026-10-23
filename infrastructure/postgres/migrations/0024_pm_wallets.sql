-- Polymarket research cache for Top Traders / wallet profiles (whale-tracker pm.rs).
-- Everything here is derived from Polymarket's public Data and Gamma APIs and is
-- safe to drop: it refills on the next leaderboard or profile view.

CREATE TABLE IF NOT EXISTS pm_wallet_stats (
    wallet           text PRIMARY KEY,
    name             text,
    pseudonym        text,
    profile_image    text,
    x_username       text,
    verified         boolean,
    joined_at        timestamptz,
    -- "lite" fields: profile, value, all-time P&L/volume/rank (4 calls)
    positions_value  double precision,
    markets_traded   integer,
    all_pnl          double precision,
    all_vol          double precision,
    all_rank         integer,
    lite_at          timestamptz,
    -- "full" fields: closed-position record, active positions, categories
    active_positions integer,
    closed_counted   integer,
    closed_capped    boolean,
    wins             integer,
    losses           integer,
    win_amount       double precision,
    loss_amount      double precision,
    biggest_win      double precision,
    biggest_loss     double precision,
    categories       jsonb,
    full_at          timestamptz
);

-- One row per wallet per leaderboard view per day, so rank changes can be shown.
CREATE TABLE IF NOT EXISTS pm_rank_history (
    category    text NOT NULL,
    time_window text NOT NULL,
    order_by    text NOT NULL,
    as_of       date NOT NULL,
    wallet      text NOT NULL,
    rank        integer NOT NULL,
    pnl         double precision,
    vol         double precision,
    PRIMARY KEY (category, time_window, order_by, as_of, wallet)
);
