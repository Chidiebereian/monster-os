-- Game hub (Games page + matchup pages): the NFL efficiency columns nflverse
-- publishes alongside the play-by-play we already store, FTN's free charting
-- (play action, blitz, box counts...) and the nflverse schedule (kickoff,
-- stadium, roof/surface/weather, coaches, referee, ESPN id).

CREATE TABLE IF NOT EXISTS nfl_play_metrics (
    game_id      text    NOT NULL,
    play_id      integer NOT NULL,
    season       integer NOT NULL,
    week         integer NOT NULL,
    season_type  text    NOT NULL,
    posteam      text,
    defteam      text,
    epa          real,
    success      boolean,
    wp           real,
    xpass        real,
    pass_oe      real,
    cpoe         real,
    qb_dropback  boolean NOT NULL DEFAULT false,
    is_pass      boolean NOT NULL DEFAULT false,
    is_rush      boolean NOT NULL DEFAULT false,
    shotgun      boolean NOT NULL DEFAULT false,
    no_huddle    boolean NOT NULL DEFAULT false,
    first_down   boolean NOT NULL DEFAULT false,
    run_gap      text,
    run_location text,
    yac          integer,
    PRIMARY KEY (game_id, play_id)
);
CREATE INDEX IF NOT EXISTS idx_nfl_play_metrics_season ON nfl_play_metrics (season, posteam);

CREATE TABLE IF NOT EXISTS nfl_ftn_charting (
    game_id             text    NOT NULL,
    play_id             integer NOT NULL,
    season              integer NOT NULL,
    week                integer NOT NULL,
    is_play_action      boolean,
    is_rpo              boolean,
    is_screen_pass      boolean,
    is_motion           boolean,
    is_no_huddle        boolean,
    is_qb_out_of_pocket boolean,
    is_throw_away       boolean,
    is_drop             boolean,
    is_catchable_ball   boolean,
    is_int_worthy       boolean,
    n_blitzers          integer,
    n_pass_rushers      integer,
    n_defense_box       integer,
    qb_location         text,
    PRIMARY KEY (game_id, play_id)
);
CREATE INDEX IF NOT EXISTS idx_nfl_ftn_season ON nfl_ftn_charting (season);

CREATE TABLE IF NOT EXISTS nfl_game_info (
    game_id        text PRIMARY KEY,
    season         integer NOT NULL,
    game_type      text,
    week           integer,
    gameday        date,
    gametime       text,
    away_team      text NOT NULL,
    home_team      text NOT NULL,
    away_score     integer,
    home_score     integer,
    espn_id        text,
    away_rest      integer,
    home_rest      integer,
    away_moneyline integer,
    home_moneyline integer,
    spread_line    double precision,
    total_line     double precision,
    div_game       boolean,
    roof           text,
    surface        text,
    temp           integer,
    wind           integer,
    away_qb_name   text,
    home_qb_name   text,
    away_coach     text,
    home_coach     text,
    referee        text,
    stadium        text,
    updated_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_nfl_game_info_espn ON nfl_game_info (espn_id);
CREATE INDEX IF NOT EXISTS idx_nfl_game_info_season ON nfl_game_info (season, week);
