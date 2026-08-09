use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::routes::AppState;

#[derive(Debug, Deserialize)]
pub struct TtftParams {
    key: Option<String>,
    period: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PublicTtftResponse {
    models: Vec<ModelTtftStats>,
}

#[derive(Debug, Serialize)]
pub struct ModelTtftStats {
    model: Option<String>,
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
    data_points: Vec<TtftDataPoint>,
}

#[derive(Debug, Serialize)]
pub struct TtftDataPoint {
    created_at: chrono::DateTime<chrono::Utc>,
    ttft_ms: Option<i32>,
    total_ms: Option<i32>,
    timed_out: bool,
    is_streaming: bool,
}

#[derive(Debug, sqlx::FromRow)]
struct AggRow {
    model: Option<String>,
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
}

#[derive(Debug, sqlx::FromRow)]
struct PointRow {
    request_model: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    ttft_ms: Option<i32>,
    total_ms: Option<i32>,
    timed_out: bool,
    is_streaming: bool,
}

fn err(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({"error": msg})))
}

fn resolve_interval(period: &str) -> &'static str {
    match period {
        "1h" => "1 hour",
        "6h" => "6 hours",
        "24h" => "24 hours",
        _ => "24 hours",
    }
}

pub async fn public_ttft(
    State(state): State<AppState>,
    Query(params): Query<TtftParams>,
) -> impl IntoResponse {
    let Some(key) = params.key.filter(|k| !k.is_empty()) else {
        return err(StatusCode::BAD_REQUEST, "key parameter is required").into_response();
    };

    // Lookup sub-key
    let key_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM group_keys WHERE api_key = $1 AND is_active = true")
            .bind(&key)
            .fetch_optional(&state.db)
            .await
            .unwrap_or(None);

    let Some(key_id) = key_id else {
        return err(StatusCode::FORBIDDEN, "Invalid or inactive key").into_response();
    };

    let interval = resolve_interval(params.period.as_deref().unwrap_or("24h"));

    // Aggregate by model (no server info)
    let agg_rows = sqlx::query_as::<_, AggRow>(&format!(
        "SELECT t.request_model as model, \
         AVG(t.ttft_ms) FILTER (WHERE t.is_streaming)::float8 as avg_ttft_ms, \
         PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY t.ttft_ms) FILTER (WHERE t.is_streaming)::float8 as p50_ttft_ms, \
         PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY t.ttft_ms) FILTER (WHERE t.is_streaming)::float8 as p95_ttft_ms, \
         AVG(t.total_ms) FILTER (WHERE NOT t.is_streaming)::float8 as avg_total_ms, \
         PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY t.total_ms) FILTER (WHERE NOT t.is_streaming)::float8 as p50_total_ms, \
         PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY t.total_ms) FILTER (WHERE NOT t.is_streaming)::float8 as p95_total_ms, \
         COUNT(*) FILTER (WHERE t.timed_out) as timeout_count, \
         COUNT(*) as total_count, \
         COUNT(*) FILTER (WHERE t.is_streaming) as stream_count, \
         COUNT(*) FILTER (WHERE NOT t.is_streaming) as non_stream_count \
         FROM ttft_logs t \
         WHERE t.group_key_id = $1 AND t.created_at > now() - interval '{interval}' \
         GROUP BY t.request_model \
         ORDER BY t.request_model"
    ))
    .bind(key_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    // Data points for scatter chart
    let all_points = sqlx::query_as::<_, PointRow>(&format!(
        "SELECT t.request_model, t.created_at, t.ttft_ms, t.total_ms, t.timed_out, t.is_streaming \
         FROM ttft_logs t \
         WHERE t.group_key_id = $1 AND t.created_at > now() - interval '{interval}' \
         ORDER BY t.created_at"
    ))
    .bind(key_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    // Group points by model
    let mut points_by_model: std::collections::HashMap<Option<String>, Vec<TtftDataPoint>> =
        std::collections::HashMap::new();
    for p in all_points {
        points_by_model
            .entry(p.request_model)
            .or_default()
            .push(TtftDataPoint {
                created_at: p.created_at,
                ttft_ms: p.ttft_ms,
                total_ms: p.total_ms,
                timed_out: p.timed_out,
                is_streaming: p.is_streaming,
            });
    }

    let models = agg_rows
        .into_iter()
        .map(|row| {
            let data_points = points_by_model.remove(&row.model).unwrap_or_default();
            ModelTtftStats {
                model: row.model,
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
                data_points,
            }
        })
        .collect();

    Json(PublicTtftResponse { models }).into_response()
}
