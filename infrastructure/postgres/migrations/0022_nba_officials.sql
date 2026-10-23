-- NBA referee crews per game (ESPN summary gameInfo.officials), for the crew
-- foul-rate study and, if it earns it, a crew input to the simulator.
CREATE TABLE IF NOT EXISTS nba_officials_espn (
    espn_id   text    NOT NULL,
    ord       integer NOT NULL,          -- ESPN's order (crew chief first)
    name      text    NOT NULL,
    PRIMARY KEY (espn_id, ord)
);
CREATE INDEX IF NOT EXISTS nba_officials_name ON nba_officials_espn (name);
