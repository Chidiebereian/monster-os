-- MonsterGPT answer ratings (thumbs up / down on the chat page). One rating
-- per assistant message; NULL = not rated.
ALTER TABLE gpt_messages ADD COLUMN IF NOT EXISTS feedback smallint CHECK (feedback IN (-1, 1));
ALTER TABLE gpt_messages ADD COLUMN IF NOT EXISTS feedback_at timestamptz;
