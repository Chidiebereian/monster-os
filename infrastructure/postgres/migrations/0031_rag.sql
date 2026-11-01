-- MonsterGPT document search (apps/monster-gpt rag.rs): documents, chunks with
-- embeddings, and provenance. Vectors are real[] with brute-force cosine in the
-- service (fine below ~50K chunks; pgvector is the upgrade path).

CREATE TABLE IF NOT EXISTS rag_docs (
    doc_id      text PRIMARY KEY,          -- repo-relative path
    title       text NOT NULL,
    kind        text NOT NULL,             -- architecture | model_card | research | readme | prompt
    hash        text NOT NULL,             -- FNV-1a of the file; unchanged files are skipped
    access      text NOT NULL DEFAULT 'public',
    chunks      integer NOT NULL,
    indexed_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS rag_chunks (
    id          text PRIMARY KEY,          -- doc_id#index
    doc_id      text NOT NULL REFERENCES rag_docs(doc_id) ON DELETE CASCADE,
    chunk_ix    integer NOT NULL,
    heading     text NOT NULL,             -- "Title > Section > Subsection"
    text        text NOT NULL,
    sport       text,                      -- nba | nfl | nhl | mlb when the chunk is about one
    access      text NOT NULL DEFAULT 'public',
    suspicious  boolean NOT NULL DEFAULT false,  -- looks like instructions to an AI; never sent to the model
    embedding   real[],
    embed_model text
);
CREATE INDEX IF NOT EXISTS rag_chunks_doc_idx ON rag_chunks (doc_id);

-- MonsterGPT long-term memory (memory.rs): user-approved preferences and facts,
-- scoped by owner, deletable one by one.
CREATE TABLE IF NOT EXISTS gpt_memory (
    id          bigserial PRIMARY KEY,
    owner       text NOT NULL,
    kind        text NOT NULL,             -- preference | note
    key         text NOT NULL,             -- bankroll | risk | sport | kind | book | odds | note
    value       text NOT NULL,
    source      text,                      -- the message it came from
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS gpt_memory_pref_uq ON gpt_memory (owner, key) WHERE kind = 'preference';
CREATE INDEX IF NOT EXISTS gpt_memory_owner_idx ON gpt_memory (owner, updated_at DESC);

-- rolling summary of each conversation, so long chats keep their thread
ALTER TABLE gpt_conversations ADD COLUMN IF NOT EXISTS summary text;
