-- NFL availability: weekly roster status and the official injury report
-- (nflverse weekly_rosters + injuries releases), used to build each game's
-- confirmed skill-position lineup.

CREATE TABLE IF NOT EXISTS nfl_roster_weekly (
    season    INTEGER NOT NULL,
    week      INTEGER NOT NULL,
    team      TEXT    NOT NULL,
    gsis_id   TEXT    NOT NULL,
    full_name TEXT    NOT NULL,
    position  TEXT,
    depth_chart_position TEXT,
    status    TEXT,               -- ACT, RES, DEV, INA, ...
    status_detail TEXT,
    PRIMARY KEY (season, week, team, gsis_id)
);

CREATE TABLE IF NOT EXISTS nfl_injuries (
    season        INTEGER NOT NULL,
    week          INTEGER NOT NULL,
    team          TEXT    NOT NULL,
    gsis_id       TEXT    NOT NULL,
    full_name     TEXT    NOT NULL,
    position      TEXT,
    report_status TEXT,           -- Out, Doubtful, Questionable, or empty
    practice_status TEXT,
    injury        TEXT,
    PRIMARY KEY (season, week, team, gsis_id)
);
