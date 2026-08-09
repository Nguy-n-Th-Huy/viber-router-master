use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{post, put},
};
use uuid::Uuid;

use crate::models::{AssignServer, GroupServer, ReorderServers, UpdateAssignment};
use crate::routes::AppState;

type ApiError = (StatusCode, Json<serde_json::Value>);

fn err(status: StatusCode, msg: &str) -> ApiError {
    (status, Json(serde_json::json!({"error": msg})))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

/// Validate circuit breaker fields: all-or-nothing, each >= 1.
fn validate_cb_fields(
    max_failures: Option<Option<i32>>,
    window_seconds: Option<Option<i32>>,
    cooldown_seconds: Option<Option<i32>>,
) -> Result<(), ApiError> {
    let provided: Vec<&Option<i32>> = [&max_failures, &window_seconds, &cooldown_seconds]
        .into_iter()
        .filter_map(|f| f.as_ref())
        .collect();

    if provided.is_empty() {
        return Ok(());
    }

    if provided.len() != 3 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Circuit breaker fields must be all set or all null",
        ));
    }

    let all_null = provided.iter().all(|v| v.is_none());
    let all_some = provided.iter().all(|v| v.is_some());

    if !all_null && !all_some {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Circuit breaker fields must be all set or all null",
        ));
    }

    if all_some {
        for v in &provided {
            if let Some(val) = v
                && *val < 1
            {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "Circuit breaker values must be >= 1",
                ));
            }
        }
    }

    Ok(())
}

/// Validate rate limit fields: all-or-nothing, each >= 1.
fn validate_rate_limit_fields(
    max_requests: Option<Option<i32>>,
    rate_window_seconds: Option<Option<i32>>,
) -> Result<(), ApiError> {
    let provided: Vec<&Option<i32>> = [&max_requests, &rate_window_seconds]
        .into_iter()
        .filter_map(|f| f.as_ref())
        .collect();

    if provided.is_empty() {
        return Ok(());
    }

    if provided.len() != 2 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Rate limit fields must be all set or all null",
        ));
    }

    let all_null = provided.iter().all(|v| v.is_none());
    let all_some = provided.iter().all(|v| v.is_some());

    if !all_null && !all_some {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Rate limit fields must be all set or all null",
        ));
    }

    if all_some {
        for v in &provided {
            if let Some(val) = v
                && *val < 1
            {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "Rate limit values must be >= 1",
                ));
            }
        }
    }

    Ok(())
}

/// Validate active hours fields: all-or-nothing, times must be HH:MM, timezone must be valid IANA.
fn validate_active_hours_fields(
    start: &Option<Option<String>>,
    end: &Option<Option<String>>,
    timezone: &Option<Option<String>>,
) -> Result<(), ApiError> {
    // Count how many of the three outer Options are Some (i.e., provided by caller)
    let provided_count = [start, end, timezone]
        .iter()
        .filter(|f| f.is_some())
        .count();

    if provided_count == 0 {
        return Ok(()); // Not provided — leave unchanged
    }

    if provided_count != 3 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Active hours fields must all be provided or all omitted (active_hours_start, active_hours_end, active_hours_timezone)",
        ));
    }

    // All three are provided. Check if they are all null (clearing) or all non-null (setting).
    let inner_count = [start, end, timezone]
        .iter()
        .filter(|f| f.as_ref().and_then(|v| v.as_ref()).is_some())
        .count();

    if inner_count == 0 {
        return Ok(()); // All null — clearing active hours, no further validation needed
    }

    if inner_count != 3 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Active hours fields must all be set or all null",
        ));
    }

    // All three are non-null — validate format
    let start_val = start.as_ref().unwrap().as_ref().unwrap();
    let end_val = end.as_ref().unwrap().as_ref().unwrap();
    let tz_val = timezone.as_ref().unwrap().as_ref().unwrap();

    if !is_valid_hhmm(start_val) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "active_hours_start must be in HH:MM format (00:00-23:59)",
        ));
    }

    if !is_valid_hhmm(end_val) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "active_hours_end must be in HH:MM format (00:00-23:59)",
        ));
    }

    if tz_val.parse::<chrono_tz::Tz>().is_err() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "active_hours_timezone is not a recognized IANA timezone",
        ));
    }

    Ok(())
}

/// Returns true if the string matches HH:MM with HH 00-23 and MM 00-59.
fn is_valid_hhmm(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return false;
    }
    let hh = &s[0..2];
    let mm = &s[3..5];
    let Ok(h) = hh.parse::<u8>() else {
        return false;
    };
    let Ok(m) = mm.parse::<u8>() else {
        return false;
    };
    h <= 23 && m <= 59
}

/// Validate retry fields: all-or-nothing; when set: retry_count >= 1, retry_delay_seconds > 0,
/// status codes in 400-599 and non-empty.
fn validate_retry_fields(
    retry_status_codes: &Option<Option<Vec<i32>>>,
    retry_count: &Option<Option<i32>>,
    retry_delay_seconds: &Option<Option<f64>>,
) -> Result<(), ApiError> {
    // Count how many of the three outer Options are Some (i.e., provided by caller)
    let provided_count = [
        retry_status_codes.is_some(),
        retry_count.is_some(),
        retry_delay_seconds.is_some(),
    ]
    .iter()
    .filter(|&&b| b)
    .count();

    if provided_count == 0 {
        return Ok(()); // Not provided — leave unchanged
    }

    if provided_count != 3 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Retry fields must all be provided or all omitted (retry_status_codes, retry_count, retry_delay_seconds)",
        ));
    }

    // All three provided. Check if they are all null (clearing) or all non-null (setting).
    let inner_count = [
        retry_status_codes
            .as_ref()
            .and_then(|v| v.as_ref())
            .is_some(),
        retry_count.as_ref().and_then(|v| v.as_ref()).is_some(),
        retry_delay_seconds
            .as_ref()
            .and_then(|v| v.as_ref())
            .is_some(),
    ]
    .iter()
    .filter(|&&b| b)
    .count();

    if inner_count == 0 {
        return Ok(()); // All null — clearing retry config, no further validation needed
    }

    if inner_count != 3 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Retry fields must all be set or all null",
        ));
    }

    // All three are non-null — validate values
    let codes = retry_status_codes.as_ref().unwrap().as_ref().unwrap();
    let count = retry_count.as_ref().unwrap().as_ref().unwrap();
    let delay = retry_delay_seconds.as_ref().unwrap().as_ref().unwrap();

    if codes.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "retry_status_codes must be non-empty",
        ));
    }

    for &code in codes {
        if !(400..=599).contains(&code) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "retry_status_codes values must be in range 400-599",
            ));
        }
    }

    if *count < 1 {
        return Err(err(StatusCode::BAD_REQUEST, "retry_count must be >= 1"));
    }

    if *delay <= 0.0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "retry_delay_seconds must be > 0",
        ));
    }

    Ok(())
}

/// Validate the non-streaming timeout: standalone, and >= 1 when set.
/// `None` leaves it unchanged, `Some(None)` clears it.
fn validate_non_stream_timeout(non_stream_timeout_ms: &Option<Option<i32>>) -> Result<(), ApiError> {
    match non_stream_timeout_ms {
        Some(Some(ms)) if *ms < 1 => Err(err(
            StatusCode::BAD_REQUEST,
            "non_stream_timeout_ms must be >= 1",
        )),
        _ => Ok(()),
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", post(assign_server))
        .route("/reorder", put(reorder_priorities))
        .route("/{server_id}", put(update_assignment).delete(remove_server))
}

async fn assign_server(
    State(state): State<AppState>,
    Path(group_id): Path<Uuid>,
    Json(input): Json<AssignServer>,
) -> Result<(StatusCode, Json<GroupServer>), ApiError> {
    let mappings = input.model_mappings.unwrap_or(serde_json::json!({}));

    let gs = sqlx::query_as::<_, GroupServer>(
        "INSERT INTO group_servers (group_id, server_id, priority, model_mappings, is_enabled, cb_max_failures, cb_window_seconds, cb_cooldown_seconds, max_input_tokens, min_input_tokens, supported_models) \
         VALUES ($1, $2, $3, $4, true, NULL, NULL, NULL, $5, $6, $7) RETURNING *",
    )
    .bind(group_id)
    .bind(input.server_id)
    .bind(input.priority)
    .bind(&mappings)
    .bind(input.max_input_tokens)
    .bind(input.min_input_tokens)
    .bind(&input.supported_models)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        if e.to_string().contains("duplicate key") || e.to_string().contains("unique") {
            err(StatusCode::CONFLICT, "Server already assigned to this group")
        } else {
            internal(e)
        }
    })?;

    invalidate_group_cache(&state, group_id).await;
    Ok((StatusCode::CREATED, Json(gs)))
}

async fn update_assignment(
    State(state): State<AppState>,
    Path((group_id, server_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<UpdateAssignment>,
) -> Result<Json<GroupServer>, ApiError> {
    // Validate circuit breaker fields
    validate_cb_fields(
        input.cb_max_failures,
        input.cb_window_seconds,
        input.cb_cooldown_seconds,
    )?;

    // Validate rate limit fields
    validate_rate_limit_fields(input.max_requests, input.rate_window_seconds)?;

    // Validate per-key rate limit fields (same all-or-nothing rule)
    {
        let provided: Vec<&Option<i32>> = [&input.per_key_max_requests, &input.per_key_rate_window_seconds]
            .into_iter()
            .filter_map(|f| f.as_ref())
            .collect();
        if !provided.is_empty() {
            if provided.len() != 2 {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "Per-key rate limit fields must be all set or all null",
                ));
            }
            let all_null = provided.iter().all(|v| v.is_none());
            let all_some = provided.iter().all(|v| v.is_some());
            if !all_null && !all_some {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "Per-key rate limit fields must be all set or all null",
                ));
            }
            if all_some {
                for v in &provided {
                    if let Some(val) = v
                        && *val < 1
                    {
                        return Err(err(
                            StatusCode::BAD_REQUEST,
                            "Per-key rate limit values must be >= 1",
                        ));
                    }
                }
            }
        }
    }

    // Validate non-negative rate fields
    for (field, val) in [
        ("rate_input", &input.rate_input),
        ("rate_output", &input.rate_output),
        ("rate_cache_write", &input.rate_cache_write),
        ("rate_cache_read", &input.rate_cache_read),
    ] {
        if let Some(Some(v)) = val
            && *v < 0.0
        {
            return Err(err(
                StatusCode::BAD_REQUEST,
                &format!("{field} must be non-negative"),
            ));
        }
    }

    // Validate active hours fields
    validate_active_hours_fields(
        &input.active_hours_start,
        &input.active_hours_end,
        &input.active_hours_timezone,
    )?;

    // Validate retry fields
    validate_retry_fields(
        &input.retry_status_codes,
        &input.retry_count,
        &input.retry_delay_seconds,
    )?;

    // Validate non-streaming timeout
    validate_non_stream_timeout(&input.non_stream_timeout_ms)?;

    // Determine whether to update CB fields
    let (update_cb_max, cb_max_val) = match input.cb_max_failures {
        Some(v) => (true, v),
        None => (false, None),
    };
    let (update_cb_window, cb_window_val) = match input.cb_window_seconds {
        Some(v) => (true, v),
        None => (false, None),
    };
    let (update_cb_cooldown, cb_cooldown_val) = match input.cb_cooldown_seconds {
        Some(v) => (true, v),
        None => (false, None),
    };

    // Determine whether to update rate fields
    let (update_rate_input, rate_input_val) = match input.rate_input {
        Some(v) => (true, v),
        None => (false, None),
    };
    let (update_rate_output, rate_output_val) = match input.rate_output {
        Some(v) => (true, v),
        None => (false, None),
    };
    let (update_rate_cw, rate_cw_val) = match input.rate_cache_write {
        Some(v) => (true, v),
        None => (false, None),
    };
    let (update_rate_cr, rate_cr_val) = match input.rate_cache_read {
        Some(v) => (true, v),
        None => (false, None),
    };

    // Determine whether to update rate limit fields
    let (update_max_requests, max_requests_val) = match input.max_requests {
        Some(v) => (true, v),
        None => (false, None),
    };
    let (update_rate_window, rate_window_val) = match input.rate_window_seconds {
        Some(v) => (true, v),
        None => (false, None),
    };

    let (update_pk_max, pk_max_val) = match input.per_key_max_requests {
        Some(v) => (true, v),
        None => (false, None),
    };
    let (update_pk_window, pk_window_val) = match input.per_key_rate_window_seconds {
        Some(v) => (true, v),
        None => (false, None),
    };

    // Determine whether to update max_input_tokens
    let (update_max_input_tokens, max_input_tokens_val) = match input.max_input_tokens {
        Some(v) => (true, v),
        None => (false, None),
    };

    // Determine whether to update min_input_tokens
    let (update_min_input_tokens, min_input_tokens_val) = match input.min_input_tokens {
        Some(v) => (true, v),
        None => (false, None),
    };

    // Determine whether to update supported_models
    let (update_supported_models, supported_models_val) = match input.supported_models {
        Some(v) => (true, v),
        None => (false, vec![]),
    };

    // Determine whether to update active hours fields
    let (update_active_hours_start, active_hours_start_val) = match input.active_hours_start {
        Some(v) => (true, v),
        None => (false, None),
    };
    let (update_active_hours_end, active_hours_end_val) = match input.active_hours_end {
        Some(v) => (true, v),
        None => (false, None),
    };
    let (update_active_hours_timezone, active_hours_timezone_val) =
        match input.active_hours_timezone {
            Some(v) => (true, v),
            None => (false, None),
        };

    // Determine whether to update retry fields
    let (update_retry_status_codes, retry_status_codes_val) = match input.retry_status_codes {
        Some(v) => (true, v),
        None => (false, None),
    };
    let (update_retry_count, retry_count_val) = match input.retry_count {
        Some(v) => (true, v),
        None => (false, None),
    };
    let (update_retry_delay, retry_delay_val) = match input.retry_delay_seconds {
        Some(v) => (true, v),
        None => (false, None),
    };

    let (update_non_stream_timeout, non_stream_timeout_val) = match input.non_stream_timeout_ms {
        Some(v) => (true, v),
        None => (false, None),
    };

    let gs = sqlx::query_as::<_, GroupServer>(
        "UPDATE group_servers SET \
         priority = COALESCE($1, priority), \
         model_mappings = COALESCE($2, model_mappings), \
         is_enabled = COALESCE($3, is_enabled), \
         cb_max_failures = CASE WHEN $6 THEN $7 ELSE cb_max_failures END, \
         cb_window_seconds = CASE WHEN $8 THEN $9 ELSE cb_window_seconds END, \
         cb_cooldown_seconds = CASE WHEN $10 THEN $11 ELSE cb_cooldown_seconds END, \
         rate_input = CASE WHEN $12 THEN $13 ELSE rate_input END, \
         rate_output = CASE WHEN $14 THEN $15 ELSE rate_output END, \
         rate_cache_write = CASE WHEN $16 THEN $17 ELSE rate_cache_write END, \
         rate_cache_read = CASE WHEN $18 THEN $19 ELSE rate_cache_read END, \
         max_requests = CASE WHEN $20 THEN $21 ELSE max_requests END, \
         rate_window_seconds = CASE WHEN $22 THEN $23 ELSE rate_window_seconds END, \
         normalize_cache_read = COALESCE($24, normalize_cache_read), \
         max_input_tokens = CASE WHEN $25 THEN $26 ELSE max_input_tokens END, \
         min_input_tokens = CASE WHEN $27 THEN $28 ELSE min_input_tokens END, \
         supported_models = CASE WHEN $29 THEN $30 ELSE supported_models END, \
         active_hours_start = CASE WHEN $31 THEN $32 ELSE active_hours_start END, \
         active_hours_end = CASE WHEN $33 THEN $34 ELSE active_hours_end END, \
         active_hours_timezone = CASE WHEN $35 THEN $36 ELSE active_hours_timezone END, \
         retry_status_codes = CASE WHEN $37 THEN $38 ELSE retry_status_codes END, \
         retry_count = CASE WHEN $39 THEN $40 ELSE retry_count END, \
         retry_delay_seconds = CASE WHEN $41 THEN $42 ELSE retry_delay_seconds END, \
         per_key_max_requests = CASE WHEN $43 THEN $44 ELSE per_key_max_requests END, \
         per_key_rate_window_seconds = CASE WHEN $45 THEN $46 ELSE per_key_rate_window_seconds END, \
         non_stream_timeout_ms = CASE WHEN $47 THEN $48 ELSE non_stream_timeout_ms END \
         WHERE group_id = $4 AND server_id = $5 RETURNING *",
    )
    .bind(input.priority)
    .bind(&input.model_mappings)
    .bind(input.is_enabled)
    .bind(group_id)
    .bind(server_id)
    .bind(update_cb_max)
    .bind(cb_max_val)
    .bind(update_cb_window)
    .bind(cb_window_val)
    .bind(update_cb_cooldown)
    .bind(cb_cooldown_val)
    .bind(update_rate_input)
    .bind(rate_input_val)
    .bind(update_rate_output)
    .bind(rate_output_val)
    .bind(update_rate_cw)
    .bind(rate_cw_val)
    .bind(update_rate_cr)
    .bind(rate_cr_val)
    .bind(update_max_requests)
    .bind(max_requests_val)
    .bind(update_rate_window)
    .bind(rate_window_val)
    .bind(input.normalize_cache_read)
    .bind(update_max_input_tokens)
    .bind(max_input_tokens_val)
    .bind(update_min_input_tokens)
    .bind(min_input_tokens_val)
    .bind(update_supported_models)
    .bind(&supported_models_val)
    .bind(update_active_hours_start)
    .bind(active_hours_start_val)
    .bind(update_active_hours_end)
    .bind(active_hours_end_val)
    .bind(update_active_hours_timezone)
    .bind(active_hours_timezone_val)
    .bind(update_retry_status_codes)
    .bind(retry_status_codes_val)
    .bind(update_retry_count)
    .bind(retry_count_val)
    .bind(update_retry_delay)
    .bind(retry_delay_val)
    .bind(update_pk_max)
    .bind(pk_max_val)
    .bind(update_pk_window)
    .bind(pk_window_val)
    .bind(update_non_stream_timeout)
    .bind(non_stream_timeout_val)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "Assignment not found"))?;

    invalidate_group_cache(&state, group_id).await;
    Ok(Json(gs))
}

async fn remove_server(
    State(state): State<AppState>,
    Path((group_id, server_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let result = sqlx::query("DELETE FROM group_servers WHERE group_id = $1 AND server_id = $2")
        .bind(group_id)
        .bind(server_id)
        .execute(&state.db)
        .await
        .map_err(internal)?;

    if result.rows_affected() == 0 {
        return Err(err(StatusCode::NOT_FOUND, "Assignment not found"));
    }

    invalidate_group_cache(&state, group_id).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn reorder_priorities(
    State(state): State<AppState>,
    Path(group_id): Path<Uuid>,
    Json(input): Json<ReorderServers>,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.db.begin().await.map_err(internal)?;

    for (i, server_id) in input.server_ids.iter().enumerate() {
        sqlx::query(
            "UPDATE group_servers SET priority = $1 WHERE group_id = $2 AND server_id = $3",
        )
        .bind((i + 1) as i32)
        .bind(group_id)
        .bind(server_id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    }

    tx.commit().await.map_err(internal)?;

    invalidate_group_cache(&state, group_id).await;
    Ok(StatusCode::OK)
}

async fn invalidate_group_cache(state: &AppState, group_id: Uuid) {
    crate::cache::invalidate_group_all_keys(&state.redis, &state.db, group_id).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_non_stream_timeout_omitted_is_ok() {
        // Field absent from the PATCH body — leave the column unchanged.
        assert!(validate_non_stream_timeout(&None).is_ok());
    }

    #[test]
    fn test_non_stream_timeout_explicit_null_clears() {
        // Explicit JSON null — disable the timeout.
        assert!(validate_non_stream_timeout(&Some(None)).is_ok());
    }

    #[test]
    fn test_non_stream_timeout_positive_accepted() {
        assert!(validate_non_stream_timeout(&Some(Some(1))).is_ok());
        assert!(validate_non_stream_timeout(&Some(Some(120_000))).is_ok());
    }

    #[test]
    fn test_non_stream_timeout_rejects_zero_and_negative() {
        // A 0 would abort every non-streaming request before it started.
        let zero = validate_non_stream_timeout(&Some(Some(0)));
        assert!(zero.is_err());
        assert_eq!(zero.unwrap_err().0, StatusCode::BAD_REQUEST);
        assert!(validate_non_stream_timeout(&Some(Some(-500))).is_err());
    }
}
