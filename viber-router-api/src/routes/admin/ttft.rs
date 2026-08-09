use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::routes::AppState;

type ApiError = (StatusCode, Json<serde_json::Value>);

fn err(status: StatusCode, msg: &str) -> ApiError {
    (status, Json(serde_json::json!({"error": msg})))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

#[derive(Debug, Deserialize)]
pub struct TtftStatsParams {
    pub group_id: Option<Uuid>,
    pub group_key_id: Option<Uuid>,
    pub period: Option<String>,
    /// Absolute time range (ISO 8601). When both are provided, `period` is ignored.
    pub start: Option<chrono::DateTime<chrono::Utc>>,
    pub end: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize)]
struct TtftStatsResponse {
    servers: Vec<ServerTtftStats>,
}

#[derive(Debug, Serialize)]
struct ServerTtftStats {
    server_id: Uuid,
    server_name: String,
    /// "group_server", "bonus", or "user_endpoint". The latter two share a nil
    /// `server_id`, so the frontend must key rows on the pair, not on the id alone.
    source: String,
    /// Streaming-only time-to-first-token stats.
    avg_ttft_ms: Option<f64>,
    p50_ttft_ms: Option<f64>,
    p95_ttft_ms: Option<f64>,
    /// Non-streaming end-to-end stats, kept separate so the two never mix.
    avg_total_ms: Option<f64>,
    p50_total_ms: Option<f64>,
    p95_total_ms: Option<f64>,
    timeout_count: i64,
    total_count: i64,
    stream_count: i64,
    non_stream_count: i64,
    non_stream_timeout_count: i64,
    data_points: Vec<TtftDataPointOut>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
struct TtftDataPoint {
    server_id: Uuid,
    source: String,
    created_at: chrono::DateTime<chrono::Utc>,
    ttft_ms: Option<i32>,
    total_ms: Option<i32>,
    timed_out: bool,
    is_streaming: bool,
}

#[derive(Debug, Serialize)]
struct TtftDataPointOut {
    created_at: chrono::DateTime<chrono::Utc>,
    ttft_ms: Option<i32>,
    total_ms: Option<i32>,
    timed_out: bool,
    is_streaming: bool,
}

#[derive(Debug, sqlx::FromRow)]
struct AggRow {
    server_id: Uuid,
    server_name: String,
    source: String,
    avg_ttft_ms: Option<f64>,
    p50_ttft_ms: Option<f64>,
    p95_ttft_ms: Option<f64>,
    avg_total_ms: Option<f64>,
    p50_total_ms: Option<f64>,
    p95_total_ms: Option<f64>,
    timeout_count: i64,
    total_count: i64,
    stream_count: i64,
    non_stream_count: i64,
    non_stream_timeout_count: i64,
}

/// Validated interval values — only these strings can appear in SQL.
fn resolve_interval(period: &str) -> &'static str {
    match period {
        "1h" => "1 hour",
        "6h" => "6 hours",
        "24h" => "24 hours",
        _ => "1 hour",
    }
}

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(get_ttft_stats))
}

async fn get_ttft_stats(
    State(state): State<AppState>,
    Query(params): Query<TtftStatsParams>,
) -> Result<Json<TtftStatsResponse>, ApiError> {
    let group_id = params
        .group_id
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "group_id is required"))?;

    let key_filter = if params.group_key_id.is_some() {
        " AND t.group_key_id = $4"
    } else {
        ""
    };
    let key_filter_rel = if params.group_key_id.is_some() {
        " AND t.group_key_id = $2"
    } else {
        ""
    };

    // Absolute range takes priority; fall back to relative period
    let (agg_rows, all_points) = if let (Some(start), Some(end)) = (params.start, params.end) {
        let agg = sqlx::query_as::<_, AggRow>(&format!(
            "SELECT t.server_id, t.source, \
             COALESCE(s.name, CASE t.source \
               WHEN 'bonus' THEN '(bonus server)' \
               WHEN 'user_endpoint' THEN '(user endpoint)' \
               ELSE '(unknown server)' END) as server_name, \
             AVG(t.ttft_ms) FILTER (WHERE t.is_streaming)::float8 as avg_ttft_ms, \
             PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY t.ttft_ms) FILTER (WHERE t.is_streaming)::float8 as p50_ttft_ms, \
             PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY t.ttft_ms) FILTER (WHERE t.is_streaming)::float8 as p95_ttft_ms, \
             AVG(t.total_ms) FILTER (WHERE NOT t.is_streaming)::float8 as avg_total_ms, \
             PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY t.total_ms) FILTER (WHERE NOT t.is_streaming)::float8 as p50_total_ms, \
             PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY t.total_ms) FILTER (WHERE NOT t.is_streaming)::float8 as p95_total_ms, \
             COUNT(*) FILTER (WHERE t.timed_out) as timeout_count, \
             COUNT(*) as total_count, \
             COUNT(*) FILTER (WHERE t.is_streaming) as stream_count, \
             COUNT(*) FILTER (WHERE NOT t.is_streaming) as non_stream_count, \
             COUNT(*) FILTER (WHERE NOT t.is_streaming AND t.timed_out) as non_stream_timeout_count \
             FROM ttft_logs t LEFT JOIN servers s ON s.id = t.server_id \
             WHERE t.group_id = $1 AND t.created_at >= $2 AND t.created_at < $3{key_filter} \
             GROUP BY t.server_id, t.source, s.name \
             ORDER BY server_name",
        ))
        .bind(group_id)
        .bind(start)
        .bind(end)
        .bind(params.group_key_id)
        .fetch_all(&state.db)
        .await
        .map_err(internal)?;

        let pts = sqlx::query_as::<_, TtftDataPoint>(&format!(
            "SELECT t.server_id, t.source, t.created_at, t.ttft_ms, t.total_ms, t.timed_out, t.is_streaming \
             FROM ttft_logs t \
             WHERE t.group_id = $1 AND t.created_at >= $2 AND t.created_at < $3{key_filter} \
             ORDER BY t.created_at",
        ))
        .bind(group_id)
        .bind(start)
        .bind(end)
        .bind(params.group_key_id)
        .fetch_all(&state.db)
        .await
        .map_err(internal)?;

        (agg, pts)
    } else {
        let interval = resolve_interval(params.period.as_deref().unwrap_or("1h"));

        let agg = sqlx::query_as::<_, AggRow>(&format!(
            "SELECT t.server_id, t.source, \
             COALESCE(s.name, CASE t.source \
               WHEN 'bonus' THEN '(bonus server)' \
               WHEN 'user_endpoint' THEN '(user endpoint)' \
               ELSE '(unknown server)' END) as server_name, \
             AVG(t.ttft_ms) FILTER (WHERE t.is_streaming)::float8 as avg_ttft_ms, \
             PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY t.ttft_ms) FILTER (WHERE t.is_streaming)::float8 as p50_ttft_ms, \
             PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY t.ttft_ms) FILTER (WHERE t.is_streaming)::float8 as p95_ttft_ms, \
             AVG(t.total_ms) FILTER (WHERE NOT t.is_streaming)::float8 as avg_total_ms, \
             PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY t.total_ms) FILTER (WHERE NOT t.is_streaming)::float8 as p50_total_ms, \
             PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY t.total_ms) FILTER (WHERE NOT t.is_streaming)::float8 as p95_total_ms, \
             COUNT(*) FILTER (WHERE t.timed_out) as timeout_count, \
             COUNT(*) as total_count, \
             COUNT(*) FILTER (WHERE t.is_streaming) as stream_count, \
             COUNT(*) FILTER (WHERE NOT t.is_streaming) as non_stream_count, \
             COUNT(*) FILTER (WHERE NOT t.is_streaming AND t.timed_out) as non_stream_timeout_count \
             FROM ttft_logs t LEFT JOIN servers s ON s.id = t.server_id \
             WHERE t.group_id = $1 AND t.created_at > now() - interval '{interval}'{key_filter_rel} \
             GROUP BY t.server_id, t.source, s.name \
             ORDER BY server_name"
        ))
        .bind(group_id)
        .bind(params.group_key_id)
        .fetch_all(&state.db)
        .await
        .map_err(internal)?;

        let pts = sqlx::query_as::<_, TtftDataPoint>(&format!(
            "SELECT t.server_id, t.source, t.created_at, t.ttft_ms, t.total_ms, t.timed_out, t.is_streaming \
             FROM ttft_logs t \
             WHERE t.group_id = $1 AND t.created_at > now() - interval '{interval}'{key_filter_rel} \
             ORDER BY t.created_at"
        ))
        .bind(group_id)
        .bind(params.group_key_id)
        .fetch_all(&state.db)
        .await
        .map_err(internal)?;

        (agg, pts)
    };

    // Group data points by (server_id, source). Keying on server_id alone would merge
    // the bonus and user-endpoint rows, since both carry a nil server_id.
    let mut points_by_server: std::collections::HashMap<(Uuid, String), Vec<TtftDataPointOut>> =
        std::collections::HashMap::new();
    for p in all_points {
        points_by_server
            .entry((p.server_id, p.source))
            .or_default()
            .push(TtftDataPointOut {
                created_at: p.created_at,
                ttft_ms: p.ttft_ms,
                total_ms: p.total_ms,
                timed_out: p.timed_out,
                is_streaming: p.is_streaming,
            });
    }

    let servers = agg_rows
        .into_iter()
        .map(|row| {
            let data_points = points_by_server
                .remove(&(row.server_id, row.source.clone()))
                .unwrap_or_default();
            ServerTtftStats {
                server_id: row.server_id,
                server_name: row.server_name,
                source: row.source,
                avg_ttft_ms: row.avg_ttft_ms,
                p50_ttft_ms: row.p50_ttft_ms,
                p95_ttft_ms: row.p95_ttft_ms,
                avg_total_ms: row.avg_total_ms,
                p50_total_ms: row.p50_total_ms,
                p95_total_ms: row.p95_total_ms,
                timeout_count: row.timeout_count,
                total_count: row.total_count,
                stream_count: row.stream_count,
                non_stream_count: row.non_stream_count,
                non_stream_timeout_count: row.non_stream_timeout_count,
                data_points,
            }
        })
        .collect();

    Ok(Json(TtftStatsResponse { servers }))
}
