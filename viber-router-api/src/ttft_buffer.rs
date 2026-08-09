use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct TtftLogEntry {
    pub group_id: Uuid,
    pub server_id: Uuid,
    pub request_model: Option<String>,
    /// Time to first SSE chunk. Only meaningful for streaming responses; stays
    /// `None` on the non-streaming path, where there is no "first token" event.
    pub ttft_ms: Option<i32>,
    /// End-to-end upstream time for a non-streaming response. Kept apart from
    /// `ttft_ms` so total generation time never lands in a TTFT percentile.
    pub total_ms: Option<i32>,
    pub timed_out: bool,
    /// Whether the upstream response was an SSE stream, so the dashboard can
    /// report the two kinds of latency separately.
    pub is_streaming: bool,
    /// Which waterfall produced this row. `server_id` is nil for "bonus" and
    /// "user_endpoint" — this is what actually tells those two apart.
    pub source: String,
    pub request_path: String,
    pub created_at: DateTime<Utc>,
    pub group_key_id: Option<Uuid>,
}

const BATCH_SIZE: usize = 100;
const FLUSH_INTERVAL_SECS: u64 = 5;

pub async fn flush_task(mut rx: mpsc::Receiver<TtftLogEntry>, pool: PgPool) {
    let mut buffer: Vec<TtftLogEntry> = Vec::with_capacity(BATCH_SIZE);
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(FLUSH_INTERVAL_SECS));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if !buffer.is_empty() {
                    flush_batch(&pool, &buffer).await;
                    buffer.clear();
                }
            }
            entry = rx.recv() => {
                match entry {
                    Some(e) => {
                        buffer.push(e);
                        if buffer.len() >= BATCH_SIZE {
                            flush_batch(&pool, &buffer).await;
                            buffer.clear();
                        }
                    }
                    None => {
                        if !buffer.is_empty() {
                            flush_batch(&pool, &buffer).await;
                        }
                        break;
                    }
                }
            }
        }
    }
}

async fn flush_batch(pool: &PgPool, entries: &[TtftLogEntry]) {
    let len = entries.len();
    let mut ids = Vec::with_capacity(len);
    let mut created_ats = Vec::with_capacity(len);
    let mut group_ids = Vec::with_capacity(len);
    let mut server_ids = Vec::with_capacity(len);
    let mut request_models: Vec<Option<String>> = Vec::with_capacity(len);
    let mut ttft_mss: Vec<Option<i32>> = Vec::with_capacity(len);
    let mut total_mss: Vec<Option<i32>> = Vec::with_capacity(len);
    let mut timed_outs = Vec::with_capacity(len);
    let mut is_streamings = Vec::with_capacity(len);
    let mut sources: Vec<String> = Vec::with_capacity(len);
    let mut request_paths = Vec::with_capacity(len);
    let mut group_key_ids: Vec<Option<Uuid>> = Vec::with_capacity(len);

    for e in entries {
        ids.push(Uuid::new_v4());
        created_ats.push(e.created_at);
        group_ids.push(e.group_id);
        server_ids.push(e.server_id);
        request_models.push(e.request_model.clone());
        ttft_mss.push(e.ttft_ms);
        total_mss.push(e.total_ms);
        timed_outs.push(e.timed_out);
        is_streamings.push(e.is_streaming);
        sources.push(e.source.clone());
        request_paths.push(e.request_path.clone());
        group_key_ids.push(e.group_key_id);
    }

    let result = sqlx::query(
        "INSERT INTO ttft_logs \
         (id, created_at, group_id, server_id, request_model, ttft_ms, total_ms, timed_out, \
          is_streaming, source, request_path, group_key_id) \
         SELECT * FROM UNNEST(\
           $1::uuid[], $2::timestamptz[], $3::uuid[], $4::uuid[], \
           $5::text[], $6::integer[], $7::integer[], $8::boolean[], $9::boolean[], \
           $10::text[], $11::text[], $12::uuid[])",
    )
    .bind(&ids)
    .bind(&created_ats)
    .bind(&group_ids)
    .bind(&server_ids)
    .bind(&request_models)
    .bind(&ttft_mss)
    .bind(&total_mss)
    .bind(&timed_outs)
    .bind(&is_streamings)
    .bind(&sources)
    .bind(&request_paths)
    .bind(&group_key_ids)
    .execute(pool)
    .await;

    match result {
        Ok(_) => tracing::debug!("Flushed {len} TTFT log entries"),
        Err(e) => tracing::warn!("Failed to flush {len} TTFT log entries: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry() -> TtftLogEntry {
        TtftLogEntry {
            group_id: Uuid::new_v4(),
            server_id: Uuid::new_v4(),
            request_model: Some("claude-opus-4-6".to_string()),
            ttft_ms: Some(450),
            total_ms: None,
            timed_out: false,
            is_streaming: true,
            source: "group_server".to_string(),
            request_path: "/v1/messages".to_string(),
            created_at: Utc::now(),
            group_key_id: None,
        }
    }

    #[tokio::test]
    async fn test_buffer_overflow_drops_entry() {
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(make_entry()).unwrap();
        assert!(tx.try_send(make_entry()).is_err());
    }

    #[tokio::test]
    async fn test_buffer_send_succeeds_when_not_full() {
        let (tx, _rx) = mpsc::channel(10_000);
        assert!(tx.try_send(make_entry()).is_ok());
        assert!(tx.try_send(make_entry()).is_ok());
    }

    #[tokio::test]
    async fn test_channel_close_signals_receiver() {
        let (tx, mut rx) = mpsc::channel::<TtftLogEntry>(10);
        tx.try_send(make_entry()).unwrap();
        drop(tx);
        assert!(rx.recv().await.is_some());
        assert!(rx.recv().await.is_none());
    }
}
