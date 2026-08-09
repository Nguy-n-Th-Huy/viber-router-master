-- Non-streaming timeouts for the bonus and user-endpoint waterfalls.
--
-- Both tables have a natural home for this, so neither needs a synthetic one:
-- key_subscriptions already carries the bonus_* column family describing the bonus
-- upstream, and user_endpoints is itself per-endpoint config.
ALTER TABLE key_subscriptions ADD COLUMN IF NOT EXISTS bonus_non_stream_timeout_ms INTEGER;
ALTER TABLE user_endpoints ADD COLUMN IF NOT EXISTS non_stream_timeout_ms INTEGER;

-- Global fallback used whenever a per-entity column is NULL, so "not configured"
-- no longer means "unbounded". Without this, every path that nobody has explicitly
-- configured keeps the old behaviour of holding a stalled request for the client's
-- full 8h budget.
--
-- 600000ms (10 min) matches the ceiling Anthropic itself applies to non-streaming
-- requests, so a legitimate completion should never reach it. Set the column to NULL
-- to restore unbounded behaviour deliberately.
ALTER TABLE settings
    ADD COLUMN IF NOT EXISTS default_non_stream_timeout_ms INTEGER DEFAULT 600000;

UPDATE settings SET default_non_stream_timeout_ms = 600000
    WHERE id = 1 AND default_non_stream_timeout_ms IS NULL;
