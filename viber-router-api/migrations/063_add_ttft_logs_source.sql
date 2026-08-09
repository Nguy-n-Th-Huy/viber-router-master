-- Record which waterfall produced a latency row.
--
-- The bonus and user-endpoint waterfalls have no `servers` row to point at, so they
-- write server_id = '00000000-...'. The admin TTFT aggregate INNER JOINed servers and
-- silently dropped them: the timeout was recorded but never shown. A single generic
-- placeholder would make them visible but still indistinguishable from each other,
-- so store the source explicitly instead.
--
-- 'group_server' for the normal path (every pre-existing row came from there),
-- 'bonus' for the bonus-subscription waterfall, 'user_endpoint' for user endpoints.
ALTER TABLE ttft_logs
    ADD COLUMN IF NOT EXISTS source TEXT NOT NULL DEFAULT 'group_server';
