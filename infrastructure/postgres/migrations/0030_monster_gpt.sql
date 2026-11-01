-- MonsterGPT conversations, messages and tool audit (apps/monster-gpt store.rs).
-- Every row carries its owner; reads always filter by owner.

CREATE TABLE IF NOT EXISTS gpt_conversations (
    id          text PRIMARY KEY,
    owner       text NOT NULL,
    title       text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS gpt_conversations_owner_idx ON gpt_conversations (owner, updated_at DESC);

CREATE TABLE IF NOT EXISTS gpt_messages (
    id              text PRIMARY KEY,
    conversation_id text NOT NULL REFERENCES gpt_conversations(id) ON DELETE CASCADE,
    request_id      text,
    role            text NOT NULL CHECK (role IN ('user', 'assistant')),
    content         text NOT NULL,
    -- assistant turns: the structured bet card, cited sources, router state, model + usage
    card            jsonb,
    sources         jsonb,
    state           jsonb,
    model           text,
    usage           jsonb,
    cancelled       boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS gpt_messages_conv_idx ON gpt_messages (conversation_id, created_at);
-- one user and one assistant message per client request id (idempotent resend)
CREATE UNIQUE INDEX IF NOT EXISTS gpt_messages_request_uq ON gpt_messages (conversation_id, request_id, role) WHERE request_id IS NOT NULL;

-- Tool calls: what was called, with which arguments, how it went. No message text.
CREATE TABLE IF NOT EXISTS gpt_tool_audit (
    id              bigserial PRIMARY KEY,
    at              timestamptz NOT NULL DEFAULT now(),
    owner           text NOT NULL,
    conversation_id text,
    request_id      text,
    tool            text NOT NULL,
    args            jsonb,
    ok              boolean NOT NULL,
    ms              integer NOT NULL,
    error           text
);
CREATE INDEX IF NOT EXISTS gpt_tool_audit_at_idx ON gpt_tool_audit (at DESC);
