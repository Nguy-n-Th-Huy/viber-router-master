-- Non-streaming support for /v1/messages: latency metrics + per-server timeout.
--
-- ttft_ms keeps its original meaning (time to first SSE chunk) and stays NULL for
-- non-streaming requests. total_ms holds end-to-end upstream time and is only
-- written for non-streaming responses, so the two never mix in a percentile.
-- is_streaming lets the dashboard split p50/p95 by response kind; every row that
-- existed before this migration came from the SSE path, hence the true default.
ALTER TABLE ttft_logs ADD COLUMN IF NOT EXISTS total_ms INTEGER;
ALTER TABLE ttft_logs ADD COLUMN IF NOT EXISTS is_streaming BOOLEAN NOT NULL DEFAULT true;

-- Partial indexes so each dashboard query only scans rows of its own kind.
CREATE INDEX IF NOT EXISTS idx_ttft_logs_streaming_group_created
    ON ttft_logs (group_id, created_at) WHERE is_streaming;
CREATE INDEX IF NOT EXISTS idx_ttft_logs_non_streaming_group_created
    ON ttft_logs (group_id, created_at) WHERE NOT is_streaming;

-- Per-server cap on a non-streaming upstream call, mirroring groups.ttft_timeout_ms
-- for the streaming path. NULL disables it. When it fires the proxy moves to the
-- next server in the group rather than returning an error to the client.
ALTER TABLE group_servers ADD COLUMN IF NOT EXISTS non_stream_timeout_ms INTEGER;
