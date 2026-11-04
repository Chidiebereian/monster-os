-- Per-sentence evidence for MonsterGPT answers (docs/architecture/monster-gpt-v2.md, 7).
-- Which sentence rested on which facts, with what status. A claim is never edited
-- (UPDATE refused); it is deleted only together with its conversation, because a user
-- may delete their history.
BEGIN;

CREATE TABLE IF NOT EXISTS gpt_claims (
    id          bigserial PRIMARY KEY,
    message_id  text NOT NULL REFERENCES gpt_messages(id) ON DELETE CASCADE,
    sentence_ix int NOT NULL,
    text        text NOT NULL,
    status      text NOT NULL CHECK (status IN ('verified', 'cross_verified', 'partially_verified', 'conflicting',
                                                'unverified', 'stale', 'model_estimated', 'hypothetical')),
    facts       jsonb NOT NULL DEFAULT '[]',   -- [{fact_id, value, source_id, ref, as_of}]
    issues      jsonb NOT NULL DEFAULT '[]',
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (message_id, sentence_ix)
);

CREATE OR REPLACE FUNCTION gpt_claims_no_update() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'gpt_claims rows are not edited (id %)', OLD.id;
END $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS gpt_claims_immutable ON gpt_claims;
CREATE TRIGGER gpt_claims_immutable BEFORE UPDATE ON gpt_claims FOR EACH ROW EXECUTE FUNCTION gpt_claims_no_update();

COMMIT;
