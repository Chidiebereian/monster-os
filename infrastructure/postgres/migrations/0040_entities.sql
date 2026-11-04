-- Canonical sports entities (docs/architecture/monster-gpt-v2.md, section 3 and 4).
-- One stable id per player, team, game, league, venue or coach, the source ids that
-- point at it, and the names people use for it (with validity dates for renames).
-- Rebuilt by a deterministic job from the source tables; ids derive from the primary
-- source's id, never from row order, so a rebuild keeps them.
BEGIN;

CREATE TABLE IF NOT EXISTS entities (
    entity_id   text PRIMARY KEY,               -- 'pl:nba:<id>', 'tm:nhl:TOR', 'gm:nfl:<game_id>'
    kind        text NOT NULL CHECK (kind IN ('player', 'team', 'game', 'league', 'venue', 'coach')),
    sport       text NOT NULL,
    name        text NOT NULL,
    team_id     text,                           -- a player's current team (entity_id)
    active_from date,
    active_to   date,
    attrs       jsonb NOT NULL DEFAULT '{}',    -- position, city, conference, ...
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS entities_kind_sport_idx ON entities (kind, sport);

CREATE TABLE IF NOT EXISTS entity_ids (
    entity_id text NOT NULL REFERENCES entities(entity_id) ON DELETE CASCADE,
    source    text NOT NULL,                    -- data_sources.source_id
    source_id text NOT NULL,
    PRIMARY KEY (source, source_id)
);
CREATE INDEX IF NOT EXISTS entity_ids_entity_idx ON entity_ids (entity_id);

CREATE TABLE IF NOT EXISTS entity_aliases (
    entity_id  text NOT NULL REFERENCES entities(entity_id) ON DELETE CASCADE,
    alias      text NOT NULL,
    norm       text NOT NULL,                   -- lowercase alphanumerics
    valid_from date,
    valid_to   date,
    weight     real NOT NULL DEFAULT 1,
    UNIQUE (entity_id, norm)
);
CREATE INDEX IF NOT EXISTS entity_aliases_norm_idx ON entity_aliases (norm);

COMMIT;
