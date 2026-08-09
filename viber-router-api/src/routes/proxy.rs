use std::collections::HashSet;

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{OriginalUri, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{Timelike, Utc};
use futures_util::StreamExt;
use serde_json::Value;

use crate::cache;
use crate::circuit_breaker;
use crate::log_buffer::{FailoverAttempt, ProxyLogEntry};
use crate::models::{CountTokensServer, GroupConfig, GroupServerDetail, UserEndpoint};
use crate::rate_limiter;
use crate::routes::AppState;
use crate::routes::key_parser::parse_api_key;
use crate::sse_usage_parser::SseUsageParser;
use crate::telegram_notifier;
use crate::translate::{self, ClientProtocol, TranslationContext};
use crate::ttft_buffer::TtftLogEntry;
use crate::uptime_buffer::UptimeCheckEntry;
use crate::usage_buffer::{TokenUsageEntry, hash_key};

pub fn router() -> Router<AppState> {
    Router::new().fallback(proxy_handler)
}

/// Merge custom_headers into the log-headers map so proxy_logs accurately reflects
/// what was actually sent upstream (custom headers override existing log entries).
fn merge_custom_headers_into_log(
    log_headers: &mut serde_json::Map<String, Value>,
    custom_headers: &Option<serde_json::Value>,
) {
    let Some(obj) = custom_headers.as_ref().and_then(|v| v.as_object()) else {
        return;
    };
    for (name, value) in obj {
        if let Some(s) = value.as_str() {
            log_headers.insert(name.clone(), Value::String(s.to_string()));
        }
    }
}

/// Apply custom headers from server config to the request builder.
/// Custom headers override any existing header with the same name.
fn apply_custom_headers(
    req: reqwest::RequestBuilder,
    custom_headers: &Option<serde_json::Value>,
) -> reqwest::RequestBuilder {
    let Some(headers_val) = custom_headers else {
        return req;
    };
    let Some(obj) = headers_val.as_object() else {
        return req;
    };
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in obj {
        let Some(val_str) = value.as_str() else {
            continue;
        };
        let Ok(header_name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(header_value) = reqwest::header::HeaderValue::from_str(val_str) else {
            continue;
        };
        headers.insert(header_name, header_value);
    }
    // RequestBuilder::headers() calls replace_headers, which inserts each entry
    // (replacing any existing header with the same name) rather than appending.
    req.headers(headers)
}

/// Returns true if the server is within its configured active hours window right now.
/// Fail-open: returns true (treat as active) if any field is absent or the timezone is unparseable.
fn is_server_active_now(server: &GroupServerDetail) -> bool {
    let (Some(start_str), Some(end_str), Some(tz_str)) = (
        &server.active_hours_start,
        &server.active_hours_end,
        &server.active_hours_timezone,
    ) else {
        return true; // Incomplete config — fail open
    };

    let tz: chrono_tz::Tz = match tz_str.parse() {
        Ok(tz) => tz,
        Err(_) => {
            tracing::warn!(
                "Server {} has unparseable active_hours_timezone {:?} — treating as 24/7",
                server.server_name,
                tz_str
            );
            return true; // Unparseable timezone — fail open
        }
    };

    // Parse start and end as (hour, minute) tuples
    let Some(start) = parse_hhmm(start_str) else {
        return true; // Malformed — fail open
    };
    let Some(end) = parse_hhmm(end_str) else {
        return true; // Malformed — fail open
    };

    // Get current time in the server's timezone
    let now_local = Utc::now().with_timezone(&tz);
    let now = (now_local.hour(), now_local.minute());

    if start <= end {
        // Same-day window: active when start <= now <= end
        now >= start && now <= end
    } else {
        // Overnight window (start > end): active when now >= start OR now <= end
        now >= start || now <= end
    }
}

/// Parse "HH:MM" into (hour, minute). Returns None on malformed input.
fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let bytes = s.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return None;
    }
    let h = s[0..2].parse::<u32>().ok()?;
    let m = s[3..5].parse::<u32>().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

fn spawn_cb_alert(
    state: &AppState,
    config: &GroupConfig,
    server: &GroupServerDetail,
    model: Option<&str>,
) {
    let db = state.db.clone();
    let redis = state.redis.clone();
    let http_client = state.http_client.clone();
    let server_name = server.server_name.clone();
    let group_name = config.group_name.clone();
    let group_id = config.group_id;
    let server_id = server.server_id;
    let max_f = server.cb_max_failures.unwrap();
    let win = server.cb_window_seconds.unwrap();
    let cool = server.cb_cooldown_seconds.unwrap();
    tokio::spawn(telegram_notifier::send_circuit_breaker_alert(
        telegram_notifier::CircuitBreakerAlertContext {
            db,
            redis,
            http_client,
            server_name,
            group_name,
            group_id,
            server_id,
            model: model.map(|s| s.to_string()),
            error_count: max_f,
            window_seconds: win,
            cooldown_seconds: cool,
        },
    ));
}

/// Record a successful half-open probe in the background; if it closes the
/// circuit, send the re-enable alert.
fn spawn_cb_probe_success(
    state: &AppState,
    config: &GroupConfig,
    server: &GroupServerDetail,
    model: Option<&str>,
) {
    let redis = state.redis.clone();
    let db = state.db.clone();
    let http_client = state.http_client.clone();
    let group_id = config.group_id;
    let server_id = server.server_id;
    let server_name = server.server_name.clone();
    let group_name = config.group_name.clone();
    let model = model.map(|s| s.to_string());
    tokio::spawn(async move {
        let closed =
            circuit_breaker::record_probe_success(&redis, group_id, server_id, model.as_deref())
                .await;
        if closed {
            telegram_notifier::send_circuit_re_enable_alert(
                telegram_notifier::CircuitReEnableAlertContext {
                    db,
                    http_client,
                    server_name,
                    group_name,
                    model,
                },
            )
            .await;
        }
    });
}

/// Release the half-open probe permit in the background without recording
/// success or failure (outcome says nothing about server health).
fn spawn_cb_probe_release(
    state: &AppState,
    config: &GroupConfig,
    server: &GroupServerDetail,
    model: Option<&str>,
) {
    let redis = state.redis.clone();
    let group_id = config.group_id;
    let server_id = server.server_id;
    let model = model.map(|s| s.to_string());
    tokio::spawn(async move {
        circuit_breaker::release_probe(&redis, group_id, server_id, model.as_deref()).await;
    });
}

/// Build a relay-generated error response in the given client protocol's
/// envelope.
///
/// The relay interior is uniformly Anthropic-shaped (see `translate`), so this
/// is the only place that needs to know about the three envelope shapes.
fn protocol_error(
    protocol: ClientProtocol,
    status: StatusCode,
    error_type: &str,
    message: &str,
) -> Response {
    let body = translate::error_envelope(protocol, error_type, message);
    (status, axum::Json(body)).into_response()
}

/// Build a relay-generated error response for a request identified only by
/// its path — used at call sites that run before the client protocol has been
/// otherwise threaded through, such as auth failures.
fn api_error(path: &str, status: StatusCode, error_type: &str, message: &str) -> Response {
    protocol_error(ClientProtocol::from_path(path), status, error_type, message)
}

fn is_billing_endpoint(path: &str) -> bool {
    matches!(path, "/v1/messages" | "/v1/chat/completions" | "/v1/responses")
}

/// Whether the client asked for an SSE stream. Both Anthropic and OpenAI use a
/// top-level boolean `stream`, and both default to non-streaming when it is absent.
/// Anything that is not literally `true` counts as non-streaming: a malformed value
/// is rejected upstream anyway, and guessing "stream" would skip the timeout below.
fn client_wants_stream(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|json| json.get("stream").and_then(|v| v.as_bool()))
        .unwrap_or(false)
}

/// Resolve the non-streaming timeout for one upstream: the entity's own setting when
/// it has one, otherwise the global default from `settings`.
///
/// The global default exists so that "nobody configured this" does not mean "hold a
/// stalled request for the client's full 8h budget". Clearing the default in settings
/// is the deliberate opt-out back to unbounded. A non-positive value at either level
/// is treated as unset rather than as an instant timeout.
fn effective_non_stream_timeout_ms(
    per_entity_ms: Option<i32>,
    global_default_ms: Option<i32>,
) -> Option<i32> {
    per_entity_ms
        .filter(|ms| *ms > 0)
        .or_else(|| global_default_ms.filter(|ms| *ms > 0))
}

/// Time left in a non-streaming timeout budget, in milliseconds.
///
/// `None` means no timeout applies — either unconfigured, or a non-positive value
/// that would otherwise abort every request before it started. `Some(0)` is
/// meaningfully different: the budget is spent and the caller should fail over now.
fn remaining_timeout_ms(configured_ms: Option<i32>, elapsed_ms: u64) -> Option<u64> {
    let configured = configured_ms.filter(|ms| *ms > 0)? as u64;
    Some(configured.saturating_sub(elapsed_ms))
}

/// Read a non-streaming response body, optionally bounded by a timeout.
///
/// `None` means the read timed out and the caller should fail over. A read
/// *error* yields `Some(empty bytes)`, matching the pre-existing behaviour of
/// swallowing body errors rather than turning a 200 into a failover.
async fn read_body_with_timeout(resp: reqwest::Response, budget_ms: Option<u64>) -> Option<Bytes> {
    match budget_ms {
        Some(ms) => {
            tokio::time::timeout(std::time::Duration::from_millis(ms), resp.bytes())
                .await
                .ok()
                .map(|r| r.unwrap_or_default())
        }
        None => Some(resp.bytes().await.unwrap_or_default()),
    }
}

/// Token counts pulled from a non-streaming response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UsageTokens {
    input_tokens: i32,
    output_tokens: i32,
    cache_creation_tokens: Option<i32>,
    cache_read_tokens: Option<i32>,
}

/// Parse the `usage` object out of a non-streaming response body.
///
/// The body is always Anthropic-shaped here: an OpenAI-origin request was
/// translated on the way in, so upstream responses use Anthropic field names
/// whichever protocol the client spoke.
///
/// Returns `None` unless both input and output counts are present — a half-known
/// request cannot be billed, and writing a partial row would understate cost.
fn extract_usage_tokens(body: &[u8]) -> Option<UsageTokens> {
    let json = serde_json::from_slice::<Value>(body).ok()?;
    let usage = json.get("usage")?;
    let field = |name: &str| usage.get(name).and_then(|v| v.as_i64()).map(|v| v as i32);

    Some(UsageTokens {
        input_tokens: field("input_tokens")?,
        output_tokens: field("output_tokens")?,
        cache_creation_tokens: field("cache_creation_input_tokens"),
        cache_read_tokens: field("cache_read_input_tokens"),
    })
}

async fn resolve_group_config(state: &AppState, api_key: &str) -> Option<GroupConfig> {
    // Try cache first
    if let Some(config) = cache::get_group_config(&state.redis, api_key).await {
        return Some(config);
    }

    // Try master key lookup first
    let (group, group_key_id) = if let Some(group) =
        sqlx::query_as::<_, crate::models::Group>("SELECT * FROM groups WHERE api_key = $1")
            .bind(api_key)
            .fetch_optional(&state.db)
            .await
            .ok()?
    {
        (group, None)
    } else {
        // Fall back to sub-key lookup: JOIN group_keys → groups
        let row = sqlx::query_as::<_, (uuid::Uuid, bool, uuid::Uuid)>(
            "SELECT gk.group_id, gk.is_active, gk.id \
             FROM group_keys gk WHERE gk.api_key = $1",
        )
        .bind(api_key)
        .fetch_optional(&state.db)
        .await
        .ok()??;

        let (group_id, sub_key_active, sub_key_id) = row;

        // If sub-key is disabled, cache a disabled config so subsequent requests are fast
        if !sub_key_active {
            let config = GroupConfig {
                group_id,
                group_name: String::new(),
                api_key: api_key.to_string(),
                is_active: false,
                failover_status_codes: vec![],
                ttft_timeout_ms: None,
                servers: vec![],
                count_tokens_server: None,
                group_key_id: Some(sub_key_id),
                allowed_models: vec![],
                key_allowed_models: vec![],
                blocked_user_agents: vec![],
            };
            cache::set_group_config(&state.redis, api_key, &config).await;
            return Some(config);
        }

        let group = sqlx::query_as::<_, crate::models::Group>("SELECT * FROM groups WHERE id = $1")
            .bind(group_id)
            .fetch_optional(&state.db)
            .await
            .ok()??;

        (group, Some(sub_key_id))
    };

    let servers = sqlx::query_as::<_, GroupServerDetail>(
        "SELECT gs.server_id, s.short_id, s.name as server_name, s.base_url, s.api_key, s.system_prompt, s.remove_thinking, gs.priority, gs.model_mappings, gs.is_enabled, \
         gs.cb_max_failures, gs.cb_window_seconds, gs.cb_cooldown_seconds, \
         gs.rate_input, gs.rate_output, gs.rate_cache_write, gs.rate_cache_read, \
         gs.max_requests, gs.rate_window_seconds, gs.normalize_cache_read, gs.max_input_tokens, gs.min_input_tokens, gs.supported_models, \
         gs.per_key_max_requests, gs.per_key_rate_window_seconds, \
         gs.active_hours_start, gs.active_hours_end, gs.active_hours_timezone, \
         gs.retry_status_codes, gs.retry_count, gs.retry_delay_seconds, \
         s.custom_headers, gs.non_stream_timeout_ms \
         FROM group_servers gs JOIN servers s ON s.id = gs.server_id \
         WHERE gs.group_id = $1 AND gs.is_enabled = true ORDER BY gs.priority",
    )
    .bind(group.id)
    .fetch_all(&state.db)
    .await
    .ok()?;

    // Filter servers for sub-key if per-key server assignments exist
    let servers = if let Some(key_id) = group_key_id {
        let assigned: Vec<(uuid::Uuid,)> =
            sqlx::query_as("SELECT server_id FROM group_key_servers WHERE group_key_id = $1")
                .bind(key_id)
                .fetch_all(&state.db)
                .await
                .ok()?;
        if assigned.is_empty() {
            servers // No assignments — use all group servers (backward compatible)
        } else {
            let assigned_ids: HashSet<uuid::Uuid> = assigned.into_iter().map(|(id,)| id).collect();
            servers
                .into_iter()
                .filter(|s| assigned_ids.contains(&s.server_id))
                .collect()
        }
    } else {
        servers
    };

    let failover_codes: Vec<u16> = serde_json::from_value(group.failover_status_codes.clone())
        .unwrap_or_else(|_| vec![429, 500, 502, 503]);

    // Resolve count-tokens default server if configured
    let count_tokens_server = if let Some(ct_server_id) = group.count_tokens_server_id {
        sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                i32,
                String,
                String,
                Option<String>,
                Option<String>,
                bool,
                Option<serde_json::Value>,
            ),
        >(
            "SELECT id, short_id, name, base_url, api_key, system_prompt, remove_thinking, custom_headers FROM servers WHERE id = $1"
        )
        .bind(ct_server_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .map(
            |(server_id, short_id, server_name, base_url, api_key, system_prompt, remove_thinking, custom_headers)| {
                CountTokensServer {
                    server_id,
                    short_id,
                    server_name,
                    base_url,
                    api_key,
                    system_prompt,
                    remove_thinking,
                    model_mappings: group.count_tokens_model_mappings.clone(),
                    custom_headers,
                }
            },
        )
    } else {
        None
    };

    // Query group allowed models
    let allowed_models: Vec<(String,)> = sqlx::query_as(
        "SELECT m.name FROM models m \
         JOIN group_allowed_models gam ON m.id = gam.model_id \
         WHERE gam.group_id = $1 ORDER BY m.name",
    )
    .bind(group.id)
    .fetch_all(&state.db)
    .await
    .ok()?;
    let allowed_models: Vec<String> = allowed_models.into_iter().map(|(n,)| n).collect();

    // Query key allowed models if using a sub-key
    let key_allowed_models = if let Some(key_id) = group_key_id {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT m.name FROM models m \
             JOIN group_key_allowed_models gkam ON m.id = gkam.model_id \
             WHERE gkam.group_key_id = $1 ORDER BY m.name",
        )
        .bind(key_id)
        .fetch_all(&state.db)
        .await
        .ok()?;
        rows.into_iter().map(|(n,)| n).collect()
    } else {
        vec![]
    };

    // Query blocked user agents for this group
    let blocked_ua_rows: Vec<(String,)> =
        sqlx::query_as("SELECT user_agent FROM group_blocked_user_agents WHERE group_id = $1")
            .bind(group.id)
            .fetch_all(&state.db)
            .await
            .ok()?;
    let blocked_user_agents: Vec<String> = blocked_ua_rows.into_iter().map(|(ua,)| ua).collect();

    let config = GroupConfig {
        group_id: group.id,
        group_name: group.name.clone(),
        api_key: group.api_key.clone(),
        is_active: group.is_active,
        failover_status_codes: failover_codes,
        ttft_timeout_ms: group.ttft_timeout_ms,
        servers,
        count_tokens_server,
        group_key_id,
        allowed_models,
        key_allowed_models,
        blocked_user_agents,
    };

    // Cache it for next time
    cache::set_group_config(&state.redis, api_key, &config).await;
    Some(config)
}

/// Transform request body by applying model mapping.
/// This function is kept for backward compatibility and testing.
#[allow(dead_code)]
fn transform_model(body: &[u8], mappings: &serde_json::Value) -> Vec<u8> {
    let mappings_obj = match mappings.as_object() {
        Some(m) if !m.is_empty() => m,
        _ => return body.to_vec(),
    };

    let mut json: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.to_vec(),
    };

    if let Some(model) = json.get("model").and_then(|m| m.as_str())
        && let Some(mapped) = mappings_obj.get(model).and_then(|v| v.as_str())
    {
        json["model"] = Value::String(mapped.to_string());
    }

    serde_json::to_vec(&json).unwrap_or_else(|_| body.to_vec())
}

/// Check if a 400 error response body indicates an invalid thinking block signature.
fn is_thinking_signature_error(response_body: &[u8]) -> bool {
    let text = match std::str::from_utf8(response_body) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let lower = text.to_lowercase();
    lower.contains("signature") || lower.contains("thinking")
}

/// Strip all thinking content blocks from assistant messages in the request body.
/// Returns `Some(new_body)` if thinking blocks were found and stripped, `None` if no changes needed.
fn strip_thinking_blocks(body: &[u8]) -> Option<Vec<u8>> {
    let mut json: Value = serde_json::from_slice(body).ok()?;
    let messages = json.get_mut("messages")?.as_array_mut()?;

    let mut changed = false;
    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
            let before_len = content.len();
            content.retain(|block| block.get("type").and_then(|t| t.as_str()) != Some("thinking"));
            if content.len() != before_len {
                changed = true;
            }
        }
    }

    if changed {
        serde_json::to_vec(&json).ok()
    } else {
        None
    }
}

/// Extract the "model" field from the request body JSON (before any mapping).
fn extract_request_model(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("model")?.as_str().map(String::from))
}

/// Estimate the number of input tokens for a request body.
/// Strips image content blocks from messages, serializes the result, and divides the byte length by 4.
/// Returns None if the body is not valid JSON (fail open).
fn estimate_input_tokens(body: &[u8]) -> Option<usize> {
    let mut json: Value = serde_json::from_slice(body).ok()?;

    // Strip image content blocks from each message's content array
    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                content.retain(|block| block.get("type").and_then(|t| t.as_str()) != Some("image"));
            }
        }
    }

    let filtered = serde_json::to_string(&json).ok()?;
    Some(filtered.len() / 4)
}

/// Check if a system array has cache_control metadata in any block.
fn has_cache_control(system: &Value) -> bool {
    if let Some(arr) = system.as_array() {
        arr.iter().any(|block| block.get("cache_control").is_some())
    } else {
        false
    }
}

/// Extract text from all blocks in a system array and concatenate with "\n\n".
fn extract_system_text(system: &Value) -> Option<String> {
    if let Some(arr) = system.as_array() {
        let texts: Vec<String> = arr
            .iter()
            .filter_map(|block| block.get("text")?.as_str().map(String::from))
            .collect();
        if texts.is_empty() {
            None
        } else {
            Some(texts.join("\n\n"))
        }
    } else {
        None
    }
}

/// Merge client system prompt with server system prompt using hybrid strategy.
/// - If client system is array with cache_control, merge server prompt into last block's text
/// - If client system is string or array without cache_control, normalize to string and concatenate
/// - If only server has prompt, use it as-is
/// - If only client has prompt, passthrough unchanged
fn merge_system_prompts(
    client_system: Option<&Value>,
    server_system: Option<&str>,
) -> Option<Value> {
    match (client_system, server_system) {
        (None, None) => None,
        (None, Some(server)) => Some(Value::String(server.to_string())),
        (Some(client), None) => Some(client.clone()),
        (Some(client), Some(server)) => {
            // Client has system, server has system — merge
            if client.is_array() && has_cache_control(client) {
                // Preserve array format with cache_control
                let mut arr = client.as_array().unwrap().clone();
                if let Some(last) = arr.last_mut()
                    && let Some(text) = last.get("text").and_then(|t| t.as_str())
                {
                    let merged_text = format!("{}\n\n{}", text, server);
                    last["text"] = Value::String(merged_text);
                }
                Some(Value::Array(arr))
            } else {
                // Normalize to string and concatenate
                let client_text = if let Some(s) = client.as_str() {
                    s.to_string()
                } else if let Some(text) = extract_system_text(client) {
                    text
                } else {
                    return Some(client.clone());
                };
                Some(Value::String(format!("{}\n\n{}", client_text, server)))
            }
        }
    }
}

/// Transform request body: apply model mapping and system prompt merge.
fn transform_request_body(
    body: &[u8],
    model_mappings: &serde_json::Value,
    server_system_prompt: Option<&str>,
    request_path: &str,
    remove_thinking: bool,
) -> Vec<u8> {
    let mut json: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.to_vec(),
    };

    // Apply model mapping
    let mappings_obj = match model_mappings.as_object() {
        Some(m) if !m.is_empty() => m,
        _ => &serde_json::Map::new(),
    };

    if let Some(model) = json.get("model").and_then(|m| m.as_str())
        && let Some(mapped) = mappings_obj.get(model).and_then(|v| v.as_str())
    {
        json["model"] = Value::String(mapped.to_string());
    }

    // Apply system prompt merge. Bodies reaching here are always Anthropic-shaped
    // — an OpenAI-origin request was translated at the inbound seam — so the
    // Anthropic top-level `system` merge is the only form needed.
    if is_billing_endpoint(request_path) && server_system_prompt.is_some() {
        let client_system = json.get("system");
        if let Some(merged) = merge_system_prompts(client_system, server_system_prompt) {
            json["system"] = merged;
            tracing::info!(
                "System prompt injected: server_prompt={:?}, merged_system={:?}",
                server_system_prompt,
                json.get("system")
            );
        }
    }

    // Remove thinking and output_config if server has remove_thinking enabled
    if remove_thinking && let Some(obj) = json.as_object_mut() {
        let removed_thinking = obj.remove("thinking").is_some();
        let removed_output_config = obj.remove("output_config").is_some();
        if removed_thinking || removed_output_config {
            tracing::info!(
                "Removed thinking/output_config from request body (remove_thinking=true)"
            );
        }
    }

    // Strip thinking blocks with missing/empty signatures from assistant messages.
    // Upstream Anthropic rejects these with "Invalid `signature` in `thinking` block";
    // some intermediary proxies strip the signature in their responses, and clients then
    // echo the now-invalid block back on the next turn.
    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        let mut stripped = 0usize;
        for msg in messages.iter_mut() {
            if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                continue;
            }
            if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                let before = content.len();
                content.retain(|block| {
                    if block.get("type").and_then(|t| t.as_str()) != Some("thinking") {
                        return true;
                    }
                    let sig = block
                        .get("signature")
                        .and_then(|s| s.as_str())
                        .unwrap_or("");
                    !sig.is_empty()
                });
                stripped += before - content.len();
            }
        }
        if stripped > 0 {
            tracing::info!(
                stripped_blocks = stripped,
                "Stripped thinking blocks with empty signature from request"
            );
        }
    }

    // Strip empty text content blocks from messages. Upstream Anthropic rejects
    // requests containing `{"type":"text","text":""}` with
    // `messages: text content blocks must be non-empty`. Some clients (e.g. Claude
    // Code variants) emit an empty leading text block alongside tool_use blocks.
    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        let mut stripped = 0usize;
        for msg in messages.iter_mut() {
            if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                let before = content.len();
                content.retain(|block| {
                    if block.get("type").and_then(|t| t.as_str()) != Some("text") {
                        return true;
                    }
                    let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    !text.is_empty()
                });
                stripped += before - content.len();
            }
        }
        if stripped > 0 {
            tracing::info!(
                stripped_blocks = stripped,
                "Stripped empty text blocks from request messages"
            );
        }
    }

    // Strip `context_management` — not a valid Anthropic API field; some clients
    // (e.g. Claude Code) send it but upstream rejects with "Extra inputs are not permitted".
    if let Some(obj) = json.as_object_mut()
        && obj.remove("context_management").is_some()
    {
        tracing::debug!("Stripped unsupported `context_management` field from request body");
    }

    serde_json::to_vec(&json).unwrap_or_else(|_| body.to_vec())
}

/// The upstream path for a given client protocol.
///
/// Configured upstreams implement only Anthropic Messages, so a translated
/// request is always sent to `/v1/messages` regardless of the path the client
/// used. An `Anthropic` request keeps its own path — that is what preserves
/// pass-through for `/v1/messages/count_tokens` and friends.
fn upstream_url_for(protocol: ClientProtocol, base_url: &str, original_uri: &axum::http::Uri) -> String {
    let base = base_url.trim_end_matches('/');
    if protocol.needs_translation() {
        // The client's query string is meaningless to the Anthropic endpoint.
        return format!("{base}/v1/messages");
    }
    let path = original_uri.path();
    match original_uri.query() {
        Some(query) => format!("{base}{path}?{query}"),
        None => format!("{base}{path}"),
    }
}

fn user_endpoint_accepts_model(endpoint: &UserEndpoint, request_model: Option<&str>) -> bool {
    crate::models::endpoint_accepts_model(endpoint, request_model)
}


async fn load_user_endpoints(
    state: &AppState,
    group_key_id: Option<uuid::Uuid>,
) -> Vec<UserEndpoint> {
    let Some(group_key_id) = group_key_id else {
        return vec![];
    };

    if !user_endpoints_feature_enabled(state).await {
        return vec![];
    }

    if let Some(endpoints) = cache::get_user_endpoints(&state.redis, group_key_id).await {
        return endpoints;
    }

    let endpoints = sqlx::query_as::<_, UserEndpoint>(
        "SELECT * FROM user_endpoints WHERE group_key_id = $1 AND is_enabled = true ORDER BY created_at ASC",
    )
    .bind(group_key_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    cache::set_user_endpoints(&state.redis, group_key_id, &endpoints).await;
    endpoints
}

pub async fn user_endpoints_feature_enabled(state: &AppState) -> bool {
    if let Ok(Some(v)) = cache::get_user_endpoints_enabled(&state.redis).await {
        return v;
    }
    let row: Option<(bool,)> =
        sqlx::query_as("SELECT user_endpoints_enabled FROM settings WHERE id = 1")
            .fetch_optional(&state.db)
            .await
            .unwrap_or(None);
    let enabled = row.map(|(v,)| v).unwrap_or(true);
    cache::set_user_endpoints_enabled(&state.redis, enabled).await;
    enabled
}

/// Whether the proxy should store the real request body in proxy logs.
/// Defaults to false (store `{}` instead) on cache miss, DB failure, or missing row.
pub async fn log_request_body_enabled(state: &AppState) -> bool {
    if let Ok(Some(v)) = cache::get_log_request_body(&state.redis).await {
        return v;
    }
    let row: Option<(bool,)> = sqlx::query_as("SELECT log_request_body FROM settings WHERE id = 1")
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);
    let enabled = row.map(|(v,)| v).unwrap_or(false);
    cache::set_log_request_body(&state.redis, enabled).await;
    enabled
}

/// Global fallback non-streaming timeout (`settings.default_non_stream_timeout_ms`).
/// `None` means unbounded — the row does not exist yet, or an admin cleared it.
async fn default_non_stream_timeout_ms(state: &AppState) -> Option<i32> {
    if let Ok(Some(v)) = cache::get_default_non_stream_timeout_ms(&state.redis).await {
        return v;
    }
    let row: Option<(Option<i32>,)> =
        sqlx::query_as("SELECT default_non_stream_timeout_ms FROM settings WHERE id = 1")
            .fetch_optional(&state.db)
            .await
            .unwrap_or(None);
    // No settings row yet — fall back to the same 600_000ms the migration seeds,
    // rather than silently going unbounded before the row is first created.
    let value = row.map(|(v,)| v).unwrap_or(Some(600_000));
    cache::set_default_non_stream_timeout_ms(&state.redis, value).await;
    value
}

#[allow(clippy::too_many_arguments)]
async fn try_user_endpoint_waterfall(
    state: &AppState,
    endpoints: &[UserEndpoint],
    mode: &str,
    protocol: ClientProtocol,
    original_uri: &axum::http::Uri,
    method: &axum::http::Method,
    headers: &HeaderMap,
    body_bytes: &Bytes,
    request_model: &Option<String>,
    config: &GroupConfig,
    content_hash: &Option<String>,
) -> Option<Response> {
    // Fetched here rather than passed in: this function is reached from the priority
    // path and from both fallback helpers, and it already holds `state`.
    //
    // Unlike the group-server and bonus paths, the send side here needs no
    // `client_wants_stream` check: the 30s header timeout below already bounds it for
    // every request, so only the non-streaming body read is still open-ended.
    let global_default_ms = default_non_stream_timeout_ms(state).await;

    for endpoint in endpoints.iter().filter(|ep| {
        ep.priority_mode == mode && user_endpoint_accepts_model(ep, request_model.as_deref())
    }) {
        let upstream_url = upstream_url_for(protocol, &endpoint.base_url, original_uri);
        let transformed_body = transform_request_body(
            body_bytes,
            &endpoint.model_mappings,
            None,
            original_uri.path(),
            false,
        );
        let mut upstream_req = state.http_client.request(method.clone(), &upstream_url);
        for (name, value) in headers.iter() {
            if name == "x-api-key"
                || name == "authorization"
                || name == "host"
                || name == "content-length"
            {
                continue;
            }
            if let Ok(reqwest_name) =
                reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes())
                && let Ok(reqwest_value) =
                    reqwest::header::HeaderValue::from_bytes(value.as_bytes())
            {
                upstream_req = upstream_req.header(reqwest_name, reqwest_value);
            }
        }
        upstream_req = upstream_req.header("x-api-key", &endpoint.api_key);
        upstream_req = upstream_req.header("authorization", format!("Bearer {}", endpoint.api_key));
        upstream_req = apply_custom_headers(upstream_req, &endpoint.custom_headers);
        upstream_req = upstream_req.body(transformed_body);

        let start = std::time::Instant::now();
        tracing::debug!(
            endpoint_id = %endpoint.id,
            endpoint_name = %endpoint.name,
            mode,
            "User endpoint waterfall: attempting upstream"
        );
        // Resolved non-streaming timeout for this endpoint, applied to the body read in
        // build_user_endpoint_success_response. The header timeout below already bounds
        // the send side, so a stalled endpoint is now capped in both phases.
        let endpoint_timeout_ms =
            effective_non_stream_timeout_ms(endpoint.non_stream_timeout_ms, global_default_ms);

        // Per-attempt header timeout. The shared http_client has an 8h overall timeout
        // suitable for long LLM streams, but we don't want a slow/hanging upstream
        // to stall the whole fallback chain. Apply timeout only to receiving response
        // headers — for a streaming response the body then proceeds without artificial
        // cap; a non-streaming body is bounded by endpoint_timeout_ms instead.
        const HEADER_TIMEOUT_SECS: u64 = 30;
        let send_fut = upstream_req.send();
        let send_result = match tokio::time::timeout(
            std::time::Duration::from_secs(HEADER_TIMEOUT_SECS),
            send_fut,
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                tracing::warn!(
                    endpoint_id = %endpoint.id,
                    endpoint_name = %endpoint.name,
                    mode,
                    timeout_secs = HEADER_TIMEOUT_SECS,
                    latency_ms = start.elapsed().as_millis() as i32,
                    "User endpoint header timeout, trying next"
                );
                continue;
            }
        };
        match send_result {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if resp.status().is_success() {
                    tracing::debug!(
                        endpoint_id = %endpoint.id,
                        endpoint_name = %endpoint.name,
                        mode,
                        status,
                        latency_ms = start.elapsed().as_millis() as i32,
                        "User endpoint upstream succeeded"
                    );
                    if let Some(built) = build_user_endpoint_success_response(
                        state,
                        config,
                        endpoint,
                        resp,
                        original_uri.path(),
                        request_model,
                        content_hash,
                        endpoint_timeout_ms,
                    )
                    .await
                    {
                        return Some(built);
                    }
                    // Non-streaming read timed out — try the next endpoint.
                    continue;
                }
                tracing::warn!(
                    endpoint_id = %endpoint.id,
                    endpoint_name = %endpoint.name,
                    mode,
                    status,
                    latency_ms = start.elapsed().as_millis() as i32,
                    "User endpoint returned non-2xx, trying next"
                );
            }
            Err(error) => {
                tracing::warn!(
                    endpoint_id = %endpoint.id,
                    endpoint_name = %endpoint.name,
                    mode,
                    error = %error,
                    latency_ms = start.elapsed().as_millis() as i32,
                    "User endpoint connection error, trying next"
                );
            }
        }
    }
    None
}

/// Build the client response for a successful user-endpoint call.
///
/// Returns `None` when a non-streaming body read exceeds `non_stream_timeout_ms`, so the
/// caller moves on to the next endpoint in the waterfall instead of surfacing the stall.
#[allow(clippy::too_many_arguments)]
async fn build_user_endpoint_success_response(
    state: &AppState,
    config: &GroupConfig,
    endpoint: &UserEndpoint,
    resp: reqwest::Response,
    request_path: &str,
    request_model: &Option<String>,
    content_hash: &Option<String>,
    non_stream_timeout_ms: Option<i32>,
) -> Option<Response> {
    let response_start = std::time::Instant::now();
    let is_sse = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"));
    let resp_status_code = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
    let mut response_headers = HeaderMap::new();
    for (name, value) in resp.headers().iter() {
        if let Ok(axum_name) = axum::http::header::HeaderName::from_bytes(name.as_str().as_bytes())
            && let Ok(axum_value) = HeaderValue::from_bytes(value.as_bytes())
        {
            response_headers.insert(axum_name, axum_value);
        }
    }

    if is_sse {
        let stream = resp
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other));
        let parser = SseUsageParser::new();
        let body = Body::from_stream(wrap_stream_with_usage_tracking(
            stream,
            state.clone(),
            config.group_id,
            uuid::Uuid::nil(),
            request_model.clone(),
            false,
            None,
            config.group_key_id,
            None,
            Some(endpoint.id),
            None,
            1.0,
            1.0,
            1.0,
            1.0,
            false,
            parser,
            content_hash.clone(),
        ));
        let mut resp_builder = Response::builder().status(resp_status_code);
        *resp_builder.headers_mut().unwrap() = response_headers;
        return Some(
            resp_builder
                .body(body)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        );
    }

    let read_budget_ms = remaining_timeout_ms(
        non_stream_timeout_ms,
        response_start.elapsed().as_millis() as u64,
    );
    let Some(body_bytes_resp) = read_body_with_timeout(resp, read_budget_ms).await else {
        tracing::warn!(
            endpoint_id = %endpoint.id,
            endpoint_name = %endpoint.name,
            base_url = %endpoint.base_url,
            timeout_ms = ?non_stream_timeout_ms,
            latency_ms = response_start.elapsed().as_millis() as i32,
            "User endpoint non-streaming read timeout, trying next"
        );
        emit_non_stream_latency_entry(
            state,
            config.group_id,
            uuid::Uuid::nil(),
            request_model,
            None,
            true,
            LatencySource::UserEndpoint,
            request_path,
            config.group_key_id,
        );
        return None;
    };
    if is_billing_endpoint(request_path)
        && let Some(usage) = extract_usage_tokens(&body_bytes_resp)
    {
        let cost_usd = if let Some(model_name) = request_model {
            let pricing_cache = state.pricing_cache.read().await;
            pricing_cache.get(model_name).map(|pricing| {
                crate::subscription::calculate_cost(
                    pricing,
                    1.0,
                    1.0,
                    1.0,
                    1.0,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_creation_tokens,
                    usage.cache_read_tokens,
                    false,
                )
            })
        } else {
            None
        };
        let entry = TokenUsageEntry {
            group_id: config.group_id,
            server_id: uuid::Uuid::nil(),
            model: request_model.clone(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            is_dynamic_key: false,
            key_hash: None,
            group_key_id: config.group_key_id,
            cost_usd,
            subscription_id: None,
            user_endpoint_id: Some(endpoint.id),
            created_at: Utc::now(),
            content_hash: content_hash.clone(),
        };
        if state.usage_tx.try_send(entry).is_err() {
            tracing::warn!("Usage buffer full, dropping user endpoint token usage entry");
        }
    }

    let mut resp_builder = Response::builder().status(resp_status_code);
    *resp_builder.headers_mut().unwrap() = response_headers;
    Some(
        resp_builder
            .body(Body::from(body_bytes_resp))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    )
}

#[allow(clippy::too_many_arguments)]
async fn fallback_or_error(
    state: &AppState,
    endpoints: &[UserEndpoint],
    protocol: ClientProtocol,
    original_uri: &axum::http::Uri,
    method: &axum::http::Method,
    headers: &HeaderMap,
    body_bytes: &Bytes,
    request_model: &Option<String>,
    config: &GroupConfig,
    content_hash: &Option<String>,
    error_type: &str,
    message: &str,
) -> Response {
    let fallback_start = std::time::Instant::now();
    tracing::info!(
        error_type,
        path = %original_uri.path(),
        endpoint_count = endpoints.iter().filter(|ep| ep.priority_mode == "fallback").count(),
        "Entering fallback waterfall"
    );
    if let Some(resp) = try_user_endpoint_waterfall(
        state,
        endpoints,
        "fallback",
        protocol,
        original_uri,
        method,
        headers,
        body_bytes,
        request_model,
        config,
        content_hash,
    )
    .await
    {
        tracing::info!(
            elapsed_ms = fallback_start.elapsed().as_millis() as i64,
            "Fallback waterfall returned a response"
        );
        return resp;
    }

    tracing::warn!(
        elapsed_ms = fallback_start.elapsed().as_millis() as i64,
        error_type,
        "Fallback waterfall exhausted, returning error"
    );
    protocol_error(
        protocol,
        StatusCode::TOO_MANY_REQUESTS,
        error_type,
        message,
    )
}

#[allow(clippy::too_many_arguments)]
async fn fallback_or_overloaded_error(
    state: &AppState,
    endpoints: &[UserEndpoint],
    protocol: ClientProtocol,
    original_uri: &axum::http::Uri,
    method: &axum::http::Method,
    headers: &HeaderMap,
    body_bytes: &Bytes,
    request_model: &Option<String>,
    config: &GroupConfig,
    content_hash: &Option<String>,
) -> Response {
    let mut resp = fallback_or_error(
        state,
        endpoints,
        protocol,
        original_uri,
        method,
        headers,
        body_bytes,
        request_model,
        config,
        content_hash,
        "overloaded_error",
        "All upstream servers unavailable",
    )
    .await;
    if resp.status() == StatusCode::TOO_MANY_REQUESTS {
        resp.headers_mut().insert(
            header::HeaderName::from_static("retry-after"),
            HeaderValue::from_static("30"),
        );
    }
    resp
}

#[allow(clippy::too_many_arguments)]
async fn fallback_or_subscription_error(
    state: &AppState,
    endpoints: &[UserEndpoint],
    protocol: ClientProtocol,
    original_uri: &axum::http::Uri,
    method: &axum::http::Method,
    headers: &HeaderMap,
    body_bytes: &Bytes,
    request_model: &Option<String>,
    config: &GroupConfig,
    content_hash: &Option<String>,
    message: &str,
) -> Response {
    fallback_or_error(
        state,
        endpoints,
        protocol,
        original_uri,
        method,
        headers,
        body_bytes,
        request_model,
        config,
        content_hash,
        "rate_limit_error",
        message,
    )
    .await
}

async fn proxy_handler(
    state: State<AppState>,
    original_uri: OriginalUri,
    req: Request,
) -> Response {
    let start = std::time::Instant::now();
    let path = original_uri.0.path().to_string();
    let method = req.method().to_string();
    tracing::info!(%method, path = %path, "proxy: request start");

    // Outbound translation seam. `proxy_handler_inner` and every waterfall exit
    // inside it produce Anthropic-shaped responses; this is the single point
    // where they are reshaped for an OpenAI client. Placing it here rather than
    // at each exit also guarantees it wraps *outside* `UsageTrackingStream`, so
    // billing always parses the Anthropic stream it was written for.
    //
    // `translation` is an out-parameter rather than part of the return type
    // because `proxy_handler_inner` returns from ~30 places; the inbound seam
    // fills it once, and a request rejected before that seam leaves it `None`
    // (its response is already correctly enveloped by `api_error`).
    let mut translation: Option<TranslationContext> = None;
    let resp = proxy_handler_inner(state, original_uri, req, &mut translation).await;
    let resp = match translation {
        Some(ctx) => translate_client_response(&ctx, resp).await,
        None => resp,
    };

    let status = resp.status().as_u16();
    let elapsed_ms = start.elapsed().as_millis() as i64;
    if elapsed_ms > 1000 {
        tracing::warn!(%method, path = %path, status, elapsed_ms, "proxy: SLOW request");
    } else {
        tracing::info!(%method, path = %path, status, elapsed_ms, "proxy: request done");
    }
    resp
}

/// Reshape an Anthropic-shaped relay response into the client's protocol.
///
/// Anthropic clients get the response back untouched. For the OpenAI protocols:
/// an SSE body is wrapped in a lazily-translating stream (never buffered), a
/// success JSON body is translated in place, and an error body is re-enveloped.
async fn translate_client_response(ctx: &TranslationContext, resp: Response) -> Response {
    let protocol = ctx.protocol;
    if !protocol.needs_translation() {
        return resp;
    }

    let status = resp.status();
    let is_sse = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"));

    // Preserve the upstream headers, minus the ones that describe a body this
    // seam is about to replace.
    let mut headers = resp.headers().clone();
    headers.remove(header::CONTENT_LENGTH);

    if is_sse && status.is_success() {
        let body = translate_sse_body(ctx, resp.into_body());
        let mut builder = Response::builder().status(status);
        *builder.headers_mut().unwrap() = headers;
        return builder
            .body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    let body_bytes = match axum::body::to_bytes(resp.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("proxy: failed to read response body for translation: {e}");
            return protocol_error(
                protocol,
                StatusCode::BAD_GATEWAY,
                "api_error",
                "Failed to read upstream response body",
            );
        }
    };

    let translated = if status.is_success() {
        translate_success_body(ctx, &body_bytes)
    } else {
        match protocol {
            ClientProtocol::ChatCompletions => {
                Some(translate::chat::anthropic_error_to_response(&body_bytes))
            }
            ClientProtocol::Responses => {
                Some(translate::responses::anthropic_error_to_response(&body_bytes))
            }
            ClientProtocol::Anthropic => None,
        }
    };

    let Some(translated) = translated else {
        // Not translatable (for example an empty body on a 204): pass the
        // original bytes through rather than inventing a body.
        let mut builder = Response::builder().status(status);
        *builder.headers_mut().unwrap() = headers;
        return builder
            .body(Body::from(body_bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    };

    let out = serde_json::to_vec(&translated).unwrap_or_default();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let mut builder = Response::builder().status(status);
    *builder.headers_mut().unwrap() = headers;
    builder
        .body(Body::from(out))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Translate a successful non-streaming Anthropic Messages body into the
/// client's protocol.
///
/// The decision to translate is made by the caller from `ClientProtocol` and the
/// response status alone — never by inspecting the body's shape. Any success
/// response on a translated protocol came from an upstream `/v1/messages` call,
/// because the only non-message success bodies the relay produces itself (the
/// count-tokens estimate and its upstream passthrough) are reachable only on
/// `/v1/messages/count_tokens`, which classifies as `Anthropic` and never
/// reaches this seam.
///
/// `None` means the body was not JSON at all; the caller forwards it untouched
/// rather than replacing a body it could not parse.
fn translate_success_body(ctx: &TranslationContext, body: &[u8]) -> Option<Value> {
    let anthropic: Value = serde_json::from_slice(body).ok()?;
    let client_model = ctx.client_model.as_deref();
    match ctx.protocol {
        ClientProtocol::ChatCompletions => Some(translate::chat::anthropic_to_response(
            &anthropic,
            ctx.json_schema_tool.as_deref(),
            client_model,
        )),
        ClientProtocol::Responses => Some(translate::responses::anthropic_to_response(
            &anthropic,
            client_model,
        )),
        ClientProtocol::Anthropic => None,
    }
}

/// A stream that feeds every chunk of an upstream Anthropic SSE body through a
/// protocol translator and yields the translated bytes.
///
/// Mirrors `UsageTrackingStream`'s hand-rolled `Stream` impl below — the
/// project's own precedent for wrapping a byte stream without an external
/// combinator crate.
struct TranslatingSseStream<S, T> {
    inner: S,
    translator: Option<T>,
}

/// What an SSE translator needs to plug into `TranslatingSseStream`: consume a
/// chunk and return translated bytes, then flush a tail when the source ends.
trait SseTranslate {
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8>;
    fn finish(self) -> Vec<u8>;
}

impl SseTranslate for translate::chat_sse::ChatSseTranslator {
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.feed(chunk)
    }
    fn finish(self) -> Vec<u8> {
        self.finish()
    }
}

impl SseTranslate for translate::responses_sse::ResponsesSseTranslator {
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.feed(chunk)
    }
    fn finish(self) -> Vec<u8> {
        self.finish()
    }
}

impl<S, T> futures_util::Stream for TranslatingSseStream<S, T>
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
    T: SseTranslate + Unpin,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match std::pin::Pin::new(&mut this.inner).poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(chunk))) => {
                    let Some(translator) = this.translator.as_mut() else {
                        continue;
                    };
                    let out = translator.feed(&chunk);
                    if !out.is_empty() {
                        return std::task::Poll::Ready(Some(Ok(Bytes::from(out))));
                    }
                    // Empty translation (e.g. a partial event still buffered) —
                    // poll the source again rather than yielding an empty chunk.
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    this.translator = None;
                    return std::task::Poll::Ready(Some(Err(e)));
                }
                std::task::Poll::Ready(None) => {
                    let Some(translator) = this.translator.take() else {
                        return std::task::Poll::Ready(None);
                    };
                    let tail = translator.finish();
                    if tail.is_empty() {
                        return std::task::Poll::Ready(None);
                    }
                    return std::task::Poll::Ready(Some(Ok(Bytes::from(tail))));
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

/// Wrap an Anthropic SSE body in the client protocol's translating stream.
fn translate_sse_body(ctx: &TranslationContext, body: Body) -> Body {
    let stream = body
        .into_data_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other));
    match ctx.protocol {
        ClientProtocol::ChatCompletions => Body::from_stream(TranslatingSseStream {
            inner: stream,
            translator: Some(translate::chat_sse::ChatSseTranslator::new(ctx.include_usage, ctx.client_model.as_deref())),
        }),
        ClientProtocol::Responses => Body::from_stream(TranslatingSseStream {
            inner: stream,
            translator: Some(translate::responses_sse::ResponsesSseTranslator::new(ctx.client_model.as_deref())),
        }),
        ClientProtocol::Anthropic => Body::from_stream(stream),
    }
}

async fn proxy_handler_inner(
    State(state): State<AppState>,
    OriginalUri(original_uri): OriginalUri,
    req: Request,
    out_translation: &mut Option<TranslationContext>,
) -> Response {
    let t0 = std::time::Instant::now();
    let log_step = |label: &'static str, t: &std::time::Instant| {
        tracing::info!(
            label,
            elapsed_ms = t.elapsed().as_millis() as i64,
            "proxy: checkpoint"
        );
    };
    // Check blocked paths first — before any auth
    let request_path = original_uri.path();
    let blocked = match cache::get_blocked_paths(&state.redis).await {
        Ok(Some(paths)) => paths,
        Ok(None) => {
            // Cache miss — load from DB and populate cache
            match sqlx::query_as::<_, (Vec<String>,)>(
                "SELECT blocked_paths FROM settings WHERE id = 1",
            )
            .fetch_optional(&state.db)
            .await
            {
                Ok(Some((paths,))) => {
                    cache::set_blocked_paths(&state.redis, &paths).await;
                    paths
                }
                Ok(None) => {
                    cache::set_blocked_paths(&state.redis, &[]).await;
                    vec![]
                }
                Err(_) => vec![], // DB failure: fail-open
            }
        }
        Err(()) => vec![], // Redis failure: fail-open
    };
    if blocked.iter().any(|p| p == request_path) {
        return api_error(
            request_path,
            StatusCode::NOT_FOUND,
            "not_found_error",
            "Not found",
        );
    }

    // Whether to store the real request body in proxy logs; when off, store `{}`.
    let log_request_body = log_request_body_enabled(&state).await;

    // Extract API key from x-api-key header, falling back to Authorization: Bearer <key>
    let raw_key = match req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            req.headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|h| h.strip_prefix("Bearer "))
                .map(|s| s.to_string())
        }) {
        Some(key) => key,
        None => {
            return api_error(
                original_uri.path(),
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Invalid API key",
            );
        }
    };

    let parsed = parse_api_key(&raw_key);

    // Look up group config using the extracted group key
    log_step("auth_parsed", &t0);
    let config = match resolve_group_config(&state, &parsed.group_key).await {
        Some(c) => c,
        None => {
            // If dynamic keys were parsed but master key not found, the raw key might be
            // a sub-key that contains `-rsv-` literally. Re-try with the entire raw key.
            if !parsed.dynamic_keys.is_empty() {
                match resolve_group_config(&state, &raw_key).await {
                    Some(c) => c,
                    None => {
                        return api_error(
                            original_uri.path(),
                            StatusCode::UNAUTHORIZED,
                            "authentication_error",
                            "Invalid API key",
                        );
                    }
                }
            } else {
                return api_error(
                    original_uri.path(),
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    "Invalid API key",
                );
            }
        }
    };

    if !config.is_active {
        return api_error(
            original_uri.path(),
            StatusCode::FORBIDDEN,
            "permission_error",
            "API key is disabled",
        );
    }

    // Extract and normalize User-Agent header
    let user_agent = req
        .headers()
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(empty)".to_string());

    // Block check: exact match against blocked_user_agents (after is_active, before servers check)
    if config
        .blocked_user_agents
        .iter()
        .any(|ua| ua == &user_agent)
    {
        return api_error(
            original_uri.path(),
            StatusCode::FORBIDDEN,
            "permission_error",
            "Access denied",
        );
    }

    // Fire-and-forget UA recording
    {
        let redis = state.redis.clone();
        let db = state.db.clone();
        let group_id = config.group_id;
        let ua = user_agent.clone();
        tokio::spawn(async move {
            match cache::add_group_ua(&redis, group_id, &ua).await {
                Ok(true) => {
                    // New UA — insert into DB
                    if let Err(e) = sqlx::query(
                        "INSERT INTO group_user_agents (group_id, user_agent) VALUES ($1, $2) \
                         ON CONFLICT DO NOTHING",
                    )
                    .bind(group_id)
                    .bind(&ua)
                    .execute(&db)
                    .await
                    {
                        tracing::warn!("Failed to insert group_user_agents: {e}");
                    }
                }
                Ok(false) => {} // Already seen — skip DB write
                Err(e) => {
                    tracing::warn!("Failed to SADD group UA to Redis: {e}");
                }
            }
        });
    }

    // Capture request parts
    log_step("config_resolved", &t0);
    let method = req.method().clone();
    let headers = req.headers().clone();
    let body_bytes = match axum::body::to_bytes(req.into_body(), 100 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return api_error(
                original_uri.path(),
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "Request body too large. Maximum allowed size is 100 MB",
            );
        }
    };

    // Inbound translation seam. Everything below this point — routing, failover,
    // billing, instrumentation — sees only Anthropic Messages shape, whichever
    // protocol the client spoke. `Anthropic` returns the bytes untouched.
    let protocol = ClientProtocol::from_path(original_uri.path());
    let (body_bytes, translation) = match translate::request_to_anthropic(protocol, &body_bytes) {
        Ok((translated, ctx)) => (Bytes::from(translated), ctx),
        Err(e) => {
            // Left as `None`: this request never reached a translated Anthropic
            // body, so there is nothing for the outbound seam to do — the error
            // below is already enveloped for `protocol`.
            // Fail before server selection, so an untranslatable request never
            // increments a rate-limit counter or logs an upstream attempt.
            tracing::info!(
                path = %original_uri.path(),
                error = %e,
                "proxy: request rejected by protocol translation"
            );
            return protocol_error(
                protocol,
                StatusCode::BAD_REQUEST,
                &e.error_type,
                &e.message,
            );
        }
    };
    // From here on the body is Anthropic-shaped, so the outbound seam must run.
    *out_translation = Some(translation);

    let client = &state.http_client;
    log_step("body_read", &t0);
    let mut any_server_attempted = false;
    let mut any_rate_limited = false;

    // Compute content_hash from raw request body bytes (used for duplicate-request spam detection)
    let content_hash = Some(crate::usage_buffer::hash_key(&String::from_utf8_lossy(
        &body_bytes,
    )));

    // Generate a unique request_id for uptime tracking across all server attempts
    let request_id = uuid::Uuid::new_v4();

    // Extract request model before any transformation
    let request_model = extract_request_model(&body_bytes);
    let request_path = original_uri.path().to_string();
    let request_method = method.to_string();
    // Read from the original body: transform_request_body never adds or removes `stream`.
    let wants_stream = client_wants_stream(&body_bytes);
    // Global fallback timeout, read once and shared by all three waterfalls (user
    // endpoint, bonus, group servers) so no path is left unbounded by default.
    let global_non_stream_default_ms = default_non_stream_timeout_ms(&state).await;
    let loop_start = std::time::Instant::now();

    // Estimate input tokens once before the failover loop (used for max_input_tokens skip)
    let estimated_tokens: Option<usize> = estimate_input_tokens(&body_bytes);
    tracing::debug!(
        path = %request_path,
        ?request_model,
        body_bytes = body_bytes.len(),
        group_id = %config.group_id,
        "proxy: parsed request, entering server selection"
    );

    // Model allowlist validation
    if !config.allowed_models.is_empty() {
        match &request_model {
            None => {
                return api_error(
                    &request_path,
                    StatusCode::FORBIDDEN,
                    "permission_error",
                    "Your API key does not have permission to use the specified model.",
                );
            }
            Some(model) if !config.allowed_models.iter().any(|m| m == model) => {
                return api_error(
                    &request_path,
                    StatusCode::FORBIDDEN,
                    "permission_error",
                    "Your API key does not have permission to use the specified model.",
                );
            }
            _ => {}
        }

        // Key-level restriction (only when group has allowed models)
        if !config.key_allowed_models.is_empty()
            && let Some(model) = &request_model
            && !config.key_allowed_models.iter().any(|m| m == model)
        {
            return api_error(
                &request_path,
                StatusCode::FORBIDDEN,
                "permission_error",
                "Your API key does not have permission to use the specified model.",
            );
        }
    }

    let user_endpoints = load_user_endpoints(&state, config.group_key_id).await;

    if let Some(resp) = try_user_endpoint_waterfall(
        &state,
        &user_endpoints,
        "priority",
        protocol,
        &original_uri,
        &method,
        &headers,
        &body_bytes,
        &request_model,
        &config,
        &content_hash,
    )
    .await
    {
        return resp;
    }

    // Subscription budget check (only for sub-keys on billing endpoints)
    let mut selected_subscription_id: Option<uuid::Uuid> = None;
    let mut selected_rpm_limit: Option<f64> = None;
    let mut selected_tpm_limit: Option<f64> = None;
    let mut bonus_servers_to_try: Vec<crate::subscription::BonusServer> = Vec::new();
    if is_billing_endpoint(&request_path)
        && let Some(group_key_id) = config.group_key_id
    {
        match crate::subscription::check_subscriptions(
            &state,
            group_key_id,
            request_model.as_deref(),
        )
        .await
        {
            crate::subscription::SubCheckResult::Allowed {
                subscription_id,
                rpm_limit,
                tpm_limit,
            } => {
                if crate::subscription::wait_for_tpm(&state, subscription_id, tpm_limit)
                    .await
                    .is_err()
                {
                    return fallback_or_subscription_error(
                        &state,
                        &user_endpoints,
                        protocol,
                        &original_uri,
                        &method,
                        &headers,
                        &body_bytes,
                        &request_model,
                        &config,
                        &content_hash,
                        "TPM limit exceeded, please retry later",
                    )
                    .await;
                }
                selected_subscription_id = Some(subscription_id);
                selected_tpm_limit = tpm_limit;
                // Increment RPM counter (optimistic, pre-request)
                if let Some(rpm) = rpm_limit {
                    crate::subscription::increment_rpm(&state, subscription_id, rpm).await;
                }
            }
            crate::subscription::SubCheckResult::Blocked => {
                return fallback_or_subscription_error(
                    &state,
                    &user_endpoints,
                    protocol,
                    &original_uri,
                    &method,
                    &headers,
                    &body_bytes,
                    &request_model,
                    &config,
                    &content_hash,
                    "Subscription limit exceeded",
                )
                .await;
            }
            crate::subscription::SubCheckResult::BonusServers {
                servers,
                fallback_subscription,
            } => {
                bonus_servers_to_try = servers;
                // Keep fallback limits pending until all bonus servers fail.
                if let Some((sub_id, rpm_limit, tpm_limit)) = fallback_subscription {
                    selected_subscription_id = Some(sub_id);
                    selected_rpm_limit = rpm_limit;
                    selected_tpm_limit = tpm_limit;
                }
            }
        }
    }

    // Build headers map (excluding host, content-length, x-api-key) for logging
    let log_headers: serde_json::Map<String, Value> = headers
        .iter()
        .filter(|(name, _)| *name != "host" && *name != "content-length" && *name != "x-api-key")
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.to_string(), Value::String(v.to_string())))
        })
        .collect();

    // Failover chain tracking
    let mut failover_chain: Vec<FailoverAttempt> = Vec::new();
    let mut last_server_id = uuid::Uuid::nil();
    let mut last_server_name = String::new();

    // Bonus server waterfall: try each bonus server in FIFO order before group servers
    for bonus_server in &bonus_servers_to_try {
        let upstream_url = format!(
            "{}/v1/messages",
            bonus_server.base_url.trim_end_matches('/')
        );

        let mut bonus_req = client.post(&upstream_url);
        // Forward original headers except auth/host/content-length
        for (name, value) in headers.iter() {
            if name == "x-api-key"
                || name == "authorization"
                || name == "host"
                || name == "content-length"
            {
                continue;
            }
            if let Ok(reqwest_name) =
                reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes())
                && let Ok(reqwest_value) =
                    reqwest::header::HeaderValue::from_bytes(value.as_bytes())
            {
                bonus_req = bonus_req.header(reqwest_name, reqwest_value);
            }
        }
        bonus_req = bonus_req.header("x-api-key", &bonus_server.api_key);
        bonus_req = bonus_req.header("authorization", format!("Bearer {}", bonus_server.api_key));
        bonus_req = apply_custom_headers(bonus_req, &bonus_server.custom_headers);
        bonus_req = bonus_req.body(body_bytes.clone());

        let bonus_start = std::time::Instant::now();
        // Non-streaming timeout for this bonus upstream. Skipped when the client asked
        // to stream, since the response kind is unknown until headers arrive; the body
        // read below picks it up in that case.
        let bonus_timeout_ms = effective_non_stream_timeout_ms(
            bonus_server.non_stream_timeout_ms,
            global_non_stream_default_ms,
        );
        let bonus_send_budget = if wants_stream {
            None
        } else {
            remaining_timeout_ms(bonus_timeout_ms, 0)
        };
        let bonus_send_result = match bonus_send_budget {
            Some(budget_ms) => {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(budget_ms),
                    bonus_req.send(),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => {
                        tracing::warn!(
                            bonus_name = %bonus_server.name,
                            subscription_id = %bonus_server.subscription_id,
                            base_url = %bonus_server.base_url,
                            timeout_ms = budget_ms,
                            latency_ms = bonus_start.elapsed().as_millis() as i32,
                            "Bonus server non-streaming timeout, trying next"
                        );
                        emit_non_stream_latency_entry(
                            &state,
                            config.group_id,
                            uuid::Uuid::nil(),
                            &request_model,
                            None,
                            true,
                            LatencySource::Bonus,
                            &request_path,
                            config.group_key_id,
                        );
                        continue;
                    }
                }
            }
            None => bonus_req.send().await,
        };
        match bonus_send_result {
            Ok(resp) => {
                let bonus_status = resp.status().as_u16();
                if resp.status().is_success() {
                    // 2xx from bonus server — log usage and return immediately
                    let bonus_sub_id = bonus_server.subscription_id;
                    let is_sse = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|ct| ct.contains("text/event-stream"));

                    let resp_status_code =
                        StatusCode::from_u16(bonus_status).unwrap_or(StatusCode::OK);
                    let mut response_headers = HeaderMap::new();
                    for (name, value) in resp.headers().iter() {
                        if let Ok(axum_name) =
                            axum::http::header::HeaderName::from_bytes(name.as_str().as_bytes())
                            && let Ok(axum_value) = HeaderValue::from_bytes(value.as_bytes())
                        {
                            response_headers.insert(axum_name, axum_value);
                        }
                    }

                    if is_sse {
                        let stream = resp.bytes_stream();
                        let first_chunk_stream =
                            stream.map(|chunk| chunk.map_err(std::io::Error::other));
                        let parser = SseUsageParser::new();
                        let body = Body::from_stream(wrap_stream_with_usage_tracking(
                            first_chunk_stream,
                            state.clone(),
                            config.group_id,
                            uuid::Uuid::nil(),
                            request_model.clone(),
                            false,
                            None,
                            config.group_key_id,
                            Some(bonus_sub_id),
                            None,
                            None,
                            1.0,
                            1.0,
                            1.0,
                            1.0,
                            false,
                            parser,
                            content_hash.clone(),
                        ));
                        let mut resp_builder = Response::builder().status(resp_status_code);
                        *resp_builder.headers_mut().unwrap() = response_headers;
                        return resp_builder
                            .body(body)
                            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
                    } else {
                        // Bound the body read too: a bonus upstream that answers headers
                        // fast and then stalls mid-generation would otherwise hold the
                        // request for the client's full 8h budget.
                        let read_budget_ms = remaining_timeout_ms(
                            bonus_timeout_ms,
                            bonus_start.elapsed().as_millis() as u64,
                        );
                        let Some(body_bytes_resp) =
                            read_body_with_timeout(resp, read_budget_ms).await
                        else {
                            tracing::warn!(
                                bonus_name = %bonus_server.name,
                                subscription_id = %bonus_server.subscription_id,
                                base_url = %bonus_server.base_url,
                                latency_ms = bonus_start.elapsed().as_millis() as i32,
                                "Bonus server non-streaming read timeout, trying next"
                            );
                            emit_non_stream_latency_entry(
                                &state,
                                config.group_id,
                                uuid::Uuid::nil(),
                                &request_model,
                                None,
                                true,
                                LatencySource::Bonus,
                                &request_path,
                                config.group_key_id,
                            );
                            continue;
                        };
                        // Extract token usage for logging
                        if let Ok(json) = serde_json::from_slice::<Value>(&body_bytes_resp)
                            && let Some(usage) = json.get("usage")
                        {
                            let inp = usage
                                .get("input_tokens")
                                .and_then(|v| v.as_i64())
                                .map(|v| v as i32);
                            let out = usage
                                .get("output_tokens")
                                .and_then(|v| v.as_i64())
                                .map(|v| v as i32);
                            let cc = usage
                                .get("cache_creation_input_tokens")
                                .and_then(|v| v.as_i64())
                                .map(|v| v as i32);
                            let cr = usage
                                .get("cache_read_input_tokens")
                                .and_then(|v| v.as_i64())
                                .map(|v| v as i32);
                            if let (Some(inp), Some(out)) = (inp, out) {
                                let cost_usd = if let Some(ref model_name) = request_model {
                                    let pricing_cache = state.pricing_cache.read().await;
                                    pricing_cache.get(model_name).map(|pricing| {
                                        crate::subscription::calculate_cost(
                                            pricing, 1.0, 1.0, 1.0, 1.0, inp, out, cc, cr, false,
                                        )
                                    })
                                } else {
                                    None
                                };
                                let entry = crate::usage_buffer::TokenUsageEntry {
                                    group_id: config.group_id,
                                    server_id: uuid::Uuid::nil(),
                                    model: request_model.clone(),
                                    input_tokens: inp,
                                    output_tokens: out,
                                    cache_creation_tokens: cc,
                                    cache_read_tokens: cr,
                                    is_dynamic_key: false,
                                    key_hash: None,
                                    group_key_id: config.group_key_id,
                                    cost_usd,
                                    subscription_id: Some(bonus_sub_id),
                                    user_endpoint_id: None,
                                    created_at: Utc::now(),
                                    content_hash: content_hash.clone(),
                                };
                                if state.usage_tx.try_send(entry).is_err() {
                                    tracing::warn!(
                                        "Usage buffer full, dropping bonus token usage entry"
                                    );
                                }
                            }
                        }
                        let mut resp_builder = Response::builder().status(resp_status_code);
                        *resp_builder.headers_mut().unwrap() = response_headers;
                        return resp_builder
                            .body(axum::body::Body::from(body_bytes_resp))
                            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
                    }
                } else {
                    // Non-2xx from bonus server — log and continue to next bonus server
                    tracing::warn!(
                        bonus_name = %bonus_server.name,
                        allowed_models = ?bonus_server.allowed_models,
                        status = bonus_status,
                        latency_ms = bonus_start.elapsed().as_millis() as i32,
                        "Bonus server returned non-2xx, trying next"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    bonus_name = %bonus_server.name,
                    allowed_models = ?bonus_server.allowed_models,
                    error = %e,
                    "Bonus server connection error, trying next"
                );
            }
        }
    }
    // After all bonus servers are exhausted, enforce the pending fallback subscription before group servers.
    if !bonus_servers_to_try.is_empty()
        && let Some(sub_id) = selected_subscription_id
    {
        if crate::subscription::wait_for_tpm(&state, sub_id, selected_tpm_limit)
            .await
            .is_err()
        {
            return fallback_or_subscription_error(
                &state,
                &user_endpoints,
                protocol,
                &original_uri,
                &method,
                &headers,
                &body_bytes,
                &request_model,
                &config,
                &content_hash,
                "TPM limit exceeded, please retry later",
            )
            .await;
        }
        if let Some(rpm) = selected_rpm_limit {
            crate::subscription::increment_rpm(&state, sub_id, rpm).await;
        }
    }
    // Proceed to group server waterfall below

    // Count-tokens default server: try before the failover waterfall
    let is_count_tokens = request_path == "/v1/messages/count_tokens";
    let mut ct_default_attempted = false;

    // Global token counting settings override: check before per-group count_tokens flow
    if is_count_tokens {
        let ct_settings = sqlx::query_as::<_, (bool, Option<String>, Option<String>)>(
            "SELECT ct_always_estimate, ct_anthropic_base_url, ct_anthropic_api_key \
             FROM settings WHERE id = 1",
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();

        if let Some((ct_always_estimate, ct_base_url, ct_api_key)) = ct_settings {
            if ct_always_estimate {
                // Estimate locally and return immediately — skip all downstream processing
                let input_tokens = estimate_input_tokens(&body_bytes).unwrap_or(0);
                let resp_body = serde_json::json!({"input_tokens": input_tokens});
                return (StatusCode::OK, axum::Json(resp_body)).into_response();
            } else if let (Some(base_url), Some(api_key)) = (ct_base_url, ct_api_key) {
                // Forward to configured Anthropic-compatible endpoint
                let upstream_url = format!(
                    "{}/v1/messages/count_tokens",
                    base_url.trim_end_matches('/')
                );
                let upstream_result = client
                    .post(&upstream_url)
                    .header("x-api-key", &api_key)
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .body(body_bytes.clone())
                    .send()
                    .await;

                match upstream_result {
                    Ok(resp) if resp.status().is_success() => {
                        // Return the upstream response directly
                        let ct_status =
                            StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
                        let ct_bytes = resp.bytes().await.unwrap_or_default();
                        return (
                            ct_status,
                            [(
                                header::CONTENT_TYPE,
                                HeaderValue::from_static("application/json"),
                            )],
                            ct_bytes,
                        )
                            .into_response();
                    }
                    _ => {
                        // Upstream failed — fall back to local estimate
                        let input_tokens = estimate_input_tokens(&body_bytes).unwrap_or(0);
                        let resp_body = serde_json::json!({"input_tokens": input_tokens});
                        return (StatusCode::OK, axum::Json(resp_body)).into_response();
                    }
                }
            }
        }
        // No global override — fall through to per-group count_tokens flow
    }

    if is_count_tokens && let Some(ref ct_server) = config.count_tokens_server {
        // Key resolution for default server: dynamic key > server default > skip
        let ct_resolved_key = if let Some(dk) = parsed.dynamic_keys.get(&ct_server.short_id) {
            Some(dk.clone())
        } else {
            ct_server.api_key.clone()
        };

        if let Some(resolved_key) = ct_resolved_key {
            ct_default_attempted = true;
            any_server_attempted = true;

            if ct_server.system_prompt.is_some() {
                tracing::info!(
                    "Applying system prompt for count-tokens server: {} (id: {})",
                    ct_server.server_name,
                    ct_server.short_id
                );
            }

            let transformed_body = transform_request_body(
                &body_bytes,
                &ct_server.model_mappings,
                ct_server.system_prompt.as_deref(),
                original_uri.path(),
                ct_server.remove_thinking,
            );

            let path = original_uri.path();
            let upstream_url = if let Some(query) = original_uri.query() {
                format!("{}{path}?{query}", ct_server.base_url.trim_end_matches('/'))
            } else {
                format!("{}{path}", ct_server.base_url.trim_end_matches('/'))
            };

            let mut upstream_req = client.request(method.clone(), &upstream_url);

            let mut server_log_headers = log_headers.clone();
            server_log_headers.insert("x-api-key".to_string(), Value::String(resolved_key.clone()));
            server_log_headers.insert(
                "authorization".to_string(),
                Value::String(format!("Bearer {}", resolved_key)),
            );
            merge_custom_headers_into_log(&mut server_log_headers, &ct_server.custom_headers);

            for (name, value) in headers.iter() {
                if name == "x-api-key"
                    || name == "authorization"
                    || name == "host"
                    || name == "content-length"
                {
                    continue;
                }
                if let Ok(reqwest_name) =
                    reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes())
                    && let Ok(reqwest_value) =
                        reqwest::header::HeaderValue::from_bytes(value.as_bytes())
                {
                    upstream_req = upstream_req.header(reqwest_name, reqwest_value);
                }
            }
            upstream_req = upstream_req.header("x-api-key", &resolved_key);
            upstream_req = upstream_req.header("authorization", format!("Bearer {}", resolved_key));
            upstream_req = apply_custom_headers(upstream_req, &ct_server.custom_headers);

            let attempt_body: Option<serde_json::Value> = if log_request_body {
                serde_json::from_slice(&transformed_body).ok()
            } else {
                Some(serde_json::json!({}))
            };
            let attempt_headers = Value::Object(server_log_headers);
            let attempt_url = upstream_url.clone();

            upstream_req = upstream_req.body(transformed_body);

            let server_start = std::time::Instant::now();
            match upstream_req.send().await {
                Ok(resp) => {
                    let server_latency = server_start.elapsed().as_millis() as i32;
                    let status = resp.status().as_u16();

                    let is_first = failover_chain.is_empty();
                    failover_chain.push(FailoverAttempt {
                        server_id: ct_server.server_id,
                        server_name: ct_server.server_name.clone(),
                        status,
                        latency_ms: server_latency,
                        resolved_key: Some(resolved_key.clone()),
                        upstream_url: Some(attempt_url),
                        request_headers: if is_first {
                            Some(attempt_headers)
                        } else {
                            None
                        },
                        request_body: if is_first { attempt_body } else { None },
                    });
                    last_server_id = ct_server.server_id;
                    last_server_name = ct_server.server_name.clone();

                    emit_uptime_entry(
                        &state,
                        config.group_id,
                        ct_server.server_id,
                        status as i16,
                        server_latency,
                        request_id,
                        &request_model,
                    );

                    if status == 200 {
                        if failover_chain.len() > 1 {
                            emit_log_entry(
                                &state,
                                &config,
                                &parsed.group_key,
                                last_server_id,
                                &last_server_name,
                                &request_path,
                                &request_method,
                                status as i16,
                                "failover_success",
                                loop_start.elapsed().as_millis() as i32,
                                &failover_chain,
                                &request_model,
                                None,
                                None,
                                None,
                            );
                        }
                        return build_response(resp).await;
                    } else if !config.failover_status_codes.contains(&status) {
                        emit_log_entry(
                            &state,
                            &config,
                            &parsed.group_key,
                            last_server_id,
                            &last_server_name,
                            &request_path,
                            &request_method,
                            status as i16,
                            "upstream_error",
                            loop_start.elapsed().as_millis() as i32,
                            &failover_chain,
                            &request_model,
                            None,
                            None,
                            None,
                        );
                        return build_response(resp).await;
                    }
                    // Failover status code — fall through to waterfall
                }
                Err(_) => {
                    let is_first = failover_chain.is_empty();
                    failover_chain.push(FailoverAttempt {
                        server_id: ct_server.server_id,
                        server_name: ct_server.server_name.clone(),
                        status: 0,
                        latency_ms: server_start.elapsed().as_millis() as i32,
                        resolved_key: Some(resolved_key.clone()),
                        upstream_url: Some(attempt_url),
                        request_headers: if is_first {
                            Some(attempt_headers)
                        } else {
                            None
                        },
                        request_body: if is_first { attempt_body } else { None },
                    });
                    last_server_id = ct_server.server_id;
                    last_server_name = ct_server.server_name.clone();

                    emit_uptime_entry(
                        &state,
                        config.group_id,
                        ct_server.server_id,
                        0,
                        server_start.elapsed().as_millis() as i32,
                        request_id,
                        &request_model,
                    );
                }
            }
        }
    }

    // Failover waterfall with key resolution
    for (server_idx, server) in config.servers.iter().enumerate() {
        // Skip the count-tokens default server if already attempted
        if ct_default_attempted
            && let Some(ref ct) = config.count_tokens_server
            && server.server_id == ct.server_id
        {
            continue;
        }

        // Key resolution: dynamic key > server default > skip
        let resolved_key = if let Some(dk) = parsed.dynamic_keys.get(&server.short_id) {
            dk.clone()
        } else if let Some(ref default_key) = server.api_key {
            default_key.clone()
        } else {
            continue; // No key available — skip this server
        };

        let has_cb = server.cb_max_failures.is_some();

        // Rate limiter: check if server has reached its request limit
        if let Some(max_req) = server.max_requests
            && server.rate_window_seconds.is_some()
            && rate_limiter::is_rate_limited(
                &state.redis,
                config.group_id,
                server.server_id,
                max_req,
            )
            .await
        {
            any_rate_limited = true;
            continue; // Skip rate-limited server
        }

        // Per-key rate limiter: skip server if this sub-key has reached its limit
        if let (Some(max_req), Some(_), Some(key_id)) = (
            server.per_key_max_requests,
            server.per_key_rate_window_seconds,
            config.group_key_id,
        ) && rate_limiter::is_rate_limited_per_key(
            &state.redis,
            key_id,
            server.server_id,
            max_req,
        )
        .await
        {
            any_rate_limited = true;
            continue;
        }

        // Max input tokens: skip server if estimated tokens exceed configured threshold
        if let Some(limit) = server.max_input_tokens
            && let Some(est) = estimated_tokens
            && est > limit as usize
        {
            continue; // Skip server whose token threshold is exceeded
        }

        // Min input tokens: skip server if estimated tokens are below configured threshold
        if let Some(limit) = server.min_input_tokens
            && let Some(est) = estimated_tokens
            && est < limit as usize
        {
            continue; // Skip server whose minimum token threshold is not met
        }

        // Supported models: skip server if it has a non-empty filter and the request model
        // is neither in the list nor a key in model_mappings (which implies it is supported).
        if !server.supported_models.is_empty()
            && let Some(ref model) = request_model
        {
            let in_list = server.supported_models.iter().any(|m| m == model);
            let in_mappings = server
                .model_mappings
                .as_object()
                .map(|obj| obj.contains_key(model.as_str()))
                .unwrap_or(false);
            if !in_list && !in_mappings {
                continue; // Skip server that does not support this model
            }
        }

        // Active hours: skip server if current time is outside its configured active window
        if !is_server_active_now(server) {
            continue; // Skip server outside active hours
        }

        // Circuit breaker: closed → allow, open → skip, half-open → at most one
        // in-flight probe request passes; everyone else fails over.
        // Checked last among skip conditions so an acquired probe permit is
        // never leaked by a later `continue`.
        let mut cb_probe = false;
        if has_cb {
            match circuit_breaker::check_access(
                &state.redis,
                config.group_id,
                server.server_id,
                request_model.as_deref(),
            )
            .await
            {
                circuit_breaker::Access::Allow => {}
                circuit_breaker::Access::Probe => cb_probe = true,
                circuit_breaker::Access::Skip => continue,
            }
        }

        any_server_attempted = true;

        // Increment rate limit counter before sending request (optimistic)
        if let (Some(_), Some(window_sec)) = (server.max_requests, server.rate_window_seconds) {
            rate_limiter::increment_rate_limit(
                &state.redis,
                config.group_id,
                server.server_id,
                window_sec,
            )
            .await;
        }

        // Increment per-key rate limit counter (only when request came via a sub-key)
        if let (Some(_), Some(window_sec), Some(key_id)) = (
            server.per_key_max_requests,
            server.per_key_rate_window_seconds,
            config.group_key_id,
        ) {
            rate_limiter::increment_rate_limit_per_key(
                &state.redis,
                key_id,
                server.server_id,
                window_sec,
            )
            .await;
        }

        if is_billing_endpoint(&request_path) && server.system_prompt.is_some() {
            tracing::info!(
                "Applying system prompt for server: {} (id: {})",
                server.server_name,
                server.short_id
            );
        }

        let mut transformed_body = transform_request_body(
            &body_bytes,
            &server.model_mappings,
            server.system_prompt.as_deref(),
            &request_path,
            server.remove_thinking,
        );

        // On failover (any attempt after the first), strip ALL thinking blocks from
        // assistant messages. Thinking signatures are bound to the producing account/key;
        // a signature from server A is invalid at server B even if the model is the same.
        if !failover_chain.is_empty()
            && let Some(sanitized) = strip_thinking_blocks(&transformed_body)
        {
            tracing::info!(
                server_name = %server.server_name,
                "Stripped thinking blocks on failover attempt"
            );
            transformed_body = sanitized;
        }

        // Build upstream URL. A translated protocol always targets upstream
        // `/v1/messages`; an Anthropic request keeps its own path and query.
        let upstream_url = upstream_url_for(protocol, &server.base_url, &original_uri);

        // Build upstream request
        let mut upstream_req = client.request(method.clone(), &upstream_url);

        // Build per-server log headers (with resolved key)
        let mut server_log_headers = log_headers.clone();
        server_log_headers.insert("x-api-key".to_string(), Value::String(resolved_key.clone()));
        server_log_headers.insert(
            "authorization".to_string(),
            Value::String(format!("Bearer {}", resolved_key)),
        );
        merge_custom_headers_into_log(&mut server_log_headers, &server.custom_headers);

        // Forward headers, replacing x-api-key and authorization with resolved key
        for (name, value) in headers.iter() {
            if name == "x-api-key" || name == "authorization" {
                continue;
            }
            if name == "host" || name == "content-length" {
                continue;
            }
            if let Ok(reqwest_name) =
                reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes())
                && let Ok(reqwest_value) =
                    reqwest::header::HeaderValue::from_bytes(value.as_bytes())
            {
                upstream_req = upstream_req.header(reqwest_name, reqwest_value);
            }
        }
        upstream_req = upstream_req.header("x-api-key", &resolved_key);
        upstream_req = upstream_req.header("authorization", format!("Bearer {}", resolved_key));
        upstream_req = apply_custom_headers(upstream_req, &server.custom_headers);

        // Prepare curl data for this attempt
        let attempt_body: Option<serde_json::Value> = if log_request_body {
            serde_json::from_slice(&transformed_body).ok()
        } else {
            Some(serde_json::json!({}))
        };
        let attempt_headers = Value::Object(server_log_headers);
        let attempt_url = upstream_url.clone();

        upstream_req = upstream_req.body(transformed_body.clone());

        let server_start = std::time::Instant::now();
        tracing::info!(
            server = %server.server_name,
            url = %upstream_url,
            t_elapsed_ms = t0.elapsed().as_millis() as i64,
            "proxy: sending to upstream"
        );

        // Non-streaming timeout. A non-streaming upstream sends nothing until the whole
        // completion is ready, so the TTFT timeout above cannot see it stall — without
        // this a hung upstream would hold the request for the client's full 8h budget.
        // Only applied when the client did not ask to stream; when it did, the response
        // kind is unknown until headers arrive and the body read below takes over.
        let server_timeout_ms = effective_non_stream_timeout_ms(
            server.non_stream_timeout_ms,
            global_non_stream_default_ms,
        );
        let non_stream_budget = if wants_stream {
            None
        } else {
            remaining_timeout_ms(server_timeout_ms, 0)
        };

        // `None` here means "treat as a failed attempt and move on", covering both a
        // connection error and a non-streaming timeout.
        let send_outcome: Option<reqwest::Response> = match non_stream_budget {
            Some(budget_ms) => {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(budget_ms),
                    upstream_req.send(),
                )
                .await
                {
                    Ok(r) => r.ok(),
                    Err(_) => {
                        tracing::warn!(
                            server = %server.server_name,
                            timeout_ms = budget_ms,
                            "proxy: non-streaming timeout waiting for upstream, failing over"
                        );
                        emit_non_stream_latency_entry(
                            &state,
                            config.group_id,
                            server.server_id,
                            &request_model,
                            None,
                            true,
                            LatencySource::GroupServer,
                            &request_path,
                            config.group_key_id,
                        );
                        None
                    }
                }
            }
            None => upstream_req.send().await.ok(),
        };

        let upstream_resp = match send_outcome {
            Some(resp) => {
                tracing::info!(
                    server = %server.server_name,
                    status = resp.status().as_u16(),
                    upstream_ms = server_start.elapsed().as_millis() as i64,
                    t_elapsed_ms = t0.elapsed().as_millis() as i64,
                    "proxy: upstream headers received"
                );
                resp
            }
            None => {
                // Connection error or non-streaming timeout → record attempt, try next server
                let is_first = failover_chain.is_empty();
                failover_chain.push(FailoverAttempt {
                    server_id: server.server_id,
                    server_name: server.server_name.clone(),
                    status: 0,
                    latency_ms: server_start.elapsed().as_millis() as i32,
                    resolved_key: Some(resolved_key.clone()),
                    upstream_url: Some(attempt_url),
                    request_headers: if is_first {
                        Some(attempt_headers.clone())
                    } else {
                        None
                    },
                    request_body: if is_first { attempt_body } else { None },
                });
                last_server_id = server.server_id;
                last_server_name = server.server_name.clone();
                emit_uptime_entry(
                    &state,
                    config.group_id,
                    server.server_id,
                    0,
                    server_start.elapsed().as_millis() as i32,
                    request_id,
                    &request_model,
                );
                // Circuit breaker: record error on connection failure
                if has_cb {
                    let tripped = circuit_breaker::record_error(
                        &state.redis,
                        config.group_id,
                        server.server_id,
                        request_model.as_deref(),
                        server.cb_max_failures.unwrap(),
                        server.cb_window_seconds.unwrap(),
                        server.cb_cooldown_seconds.unwrap(),
                        cb_probe,
                    )
                    .await;
                    if tripped {
                        spawn_cb_alert(&state, &config, server, request_model.as_deref());
                    }
                }
                continue;
            }
        };

        let server_latency = server_start.elapsed().as_millis() as i32;
        let status = upstream_resp.status().as_u16();

        let is_first = failover_chain.is_empty();
        failover_chain.push(FailoverAttempt {
            server_id: server.server_id,
            server_name: server.server_name.clone(),
            status,
            latency_ms: server_latency,
            resolved_key: Some(resolved_key.clone()),
            upstream_url: Some(attempt_url),
            request_headers: if is_first {
                Some(attempt_headers.clone())
            } else {
                None
            },
            request_body: if is_first { attempt_body } else { None },
        });
        last_server_id = server.server_id;
        last_server_name = server.server_name.clone();

        // Per-server retry: if server has retry config and status matches, retry before failover
        let (upstream_resp, status) = {
            let mut current_resp = upstream_resp;
            let mut current_status = status;
            if let (Some(retry_codes), Some(retry_count), Some(retry_delay)) = (
                &server.retry_status_codes,
                server.retry_count,
                server.retry_delay_seconds,
            ) {
                for _ in 0..retry_count {
                    if !retry_codes.contains(&(current_status as i32)) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs_f64(retry_delay)).await;
                    let mut retry_req = client.request(method.clone(), &upstream_url);
                    for (name, value) in headers.iter() {
                        if name == "x-api-key"
                            || name == "authorization"
                            || name == "host"
                            || name == "content-length"
                        {
                            continue;
                        }
                        if let Ok(rn) =
                            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes())
                            && let Ok(rv) =
                                reqwest::header::HeaderValue::from_bytes(value.as_bytes())
                        {
                            retry_req = retry_req.header(rn, rv);
                        }
                    }
                    retry_req = retry_req.header("x-api-key", &resolved_key);
                    retry_req =
                        retry_req.header("authorization", format!("Bearer {}", resolved_key));
                    retry_req = apply_custom_headers(retry_req, &server.custom_headers);
                    retry_req = retry_req.body(transformed_body.clone());
                    match retry_req.send().await {
                        Ok(retry_resp) => {
                            let retry_status = retry_resp.status().as_u16();
                            failover_chain.push(FailoverAttempt {
                                server_id: server.server_id,
                                server_name: server.server_name.clone(),
                                status: retry_status,
                                latency_ms: server_start.elapsed().as_millis() as i32,
                                resolved_key: Some(resolved_key.clone()),
                                upstream_url: Some(upstream_url.clone()),
                                request_headers: None,
                                request_body: None,
                            });
                            current_resp = retry_resp;
                            current_status = retry_status;
                        }
                        Err(_) => break,
                    }
                }
            }
            (current_resp, current_status)
        };

        // Before failover: intercept 400 thinking signature errors on /v1/messages
        if status == 400 && is_billing_endpoint(&request_path) {
            let err_body = upstream_resp.bytes().await.unwrap_or_default();
            let err_str = String::from_utf8_lossy(&err_body);
            let is_sig_err = is_thinking_signature_error(&err_body);
            tracing::warn!(
                server_name = %server.server_name,
                status = status,
                is_thinking_signature_error = is_sig_err,
                error_body = %err_str,
                "400 error from upstream"
            );
            // Emit uptime for the 400 attempt
            emit_uptime_entry(
                &state,
                config.group_id,
                server.server_id,
                status as i16,
                server_latency,
                request_id,
                &request_model,
            );
            if is_sig_err && let Some(sanitized_body) = strip_thinking_blocks(&transformed_body) {
                tracing::info!(
                    server_name = %server.server_name,
                    original_body_len = transformed_body.len(),
                    sanitized_body_len = sanitized_body.len(),
                    "Retrying after stripping thinking blocks"
                );
                let mut retry_req = client.request(method.clone(), &upstream_url);
                for (name, value) in headers.iter() {
                    if name == "x-api-key"
                        || name == "authorization"
                        || name == "host"
                        || name == "content-length"
                    {
                        continue;
                    }
                    if let Ok(rn) =
                        reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes())
                        && let Ok(rv) = reqwest::header::HeaderValue::from_bytes(value.as_bytes())
                    {
                        retry_req = retry_req.header(rn, rv);
                    }
                }
                retry_req = retry_req.header("x-api-key", &resolved_key);
                retry_req = retry_req.header("authorization", format!("Bearer {}", resolved_key));
                retry_req = apply_custom_headers(retry_req, &server.custom_headers);
                retry_req = retry_req.body(sanitized_body);

                if let Ok(retry_resp) = retry_req.send().await {
                    let retry_status = retry_resp.status().as_u16();
                    failover_chain.push(FailoverAttempt {
                        server_id: server.server_id,
                        server_name: server.server_name.clone(),
                        status: retry_status,
                        latency_ms: server_start.elapsed().as_millis() as i32,
                        resolved_key: Some(resolved_key.clone()),
                        upstream_url: Some(upstream_url.clone()),
                        request_headers: None,
                        request_body: None,
                    });

                    if retry_status == 200 {
                        if cb_probe {
                            spawn_cb_probe_success(
                                &state,
                                &config,
                                server,
                                request_model.as_deref(),
                            );
                        }
                        emit_uptime_entry(
                            &state,
                            config.group_id,
                            server.server_id,
                            retry_status as i16,
                            server_start.elapsed().as_millis() as i32,
                            request_id,
                            &request_model,
                        );
                        // The retry really was served upstream, so it must be billed and
                        // measured like any other success rather than passed through raw.
                        return build_tracked_billing_response(
                            &state,
                            &config,
                            server,
                            &parsed,
                            retry_resp,
                            &request_path,
                            &request_model,
                            selected_subscription_id,
                            selected_tpm_limit,
                            &content_hash,
                            server_start,
                            server_timeout_ms,
                        )
                        .await;
                    } else if config.failover_status_codes.contains(&retry_status) {
                        if cb_probe {
                            spawn_cb_probe_release(
                                &state,
                                &config,
                                server,
                                request_model.as_deref(),
                            );
                        }
                        continue;
                    } else {
                        emit_log_entry(
                            &state,
                            &config,
                            &parsed.group_key,
                            last_server_id,
                            &last_server_name,
                            &request_path,
                            &request_method,
                            retry_status as i16,
                            "upstream_error",
                            loop_start.elapsed().as_millis() as i32,
                            &failover_chain,
                            &request_model,
                            None,
                            None,
                            None,
                        );
                        if cb_probe {
                            spawn_cb_probe_release(
                                &state,
                                &config,
                                server,
                                request_model.as_deref(),
                            );
                        }
                        return build_response(retry_resp).await;
                    }
                }
            }
            // Signature retry didn't help or not applicable
            // Check failover_status_codes before giving up — 400 may be configured for failover
            if config.failover_status_codes.contains(&status) {
                if has_cb {
                    let tripped = circuit_breaker::record_error(
                        &state.redis,
                        config.group_id,
                        server.server_id,
                        request_model.as_deref(),
                        server.cb_max_failures.unwrap(),
                        server.cb_window_seconds.unwrap(),
                        server.cb_cooldown_seconds.unwrap(),
                        cb_probe,
                    )
                    .await;
                    if tripped {
                        spawn_cb_alert(&state, &config, server, request_model.as_deref());
                    }
                }
                continue;
            }
            emit_log_entry(
                &state,
                &config,
                &parsed.group_key,
                last_server_id,
                &last_server_name,
                &request_path,
                &request_method,
                status as i16,
                "upstream_error",
                loop_start.elapsed().as_millis() as i32,
                &failover_chain,
                &request_model,
                None,
                None,
                None,
            );
            let db = state.db.clone();
            let redis = state.redis.clone();
            let http_client = state.http_client.clone();
            let server_id = last_server_id;
            let server_name = last_server_name.clone();
            let group_name = config.group_name.clone();
            let latency = loop_start.elapsed().as_millis() as i32;
            tokio::spawn(telegram_notifier::maybe_alert(
                telegram_notifier::AlertContext {
                    db,
                    redis,
                    http_client,
                    server_id,
                    server_name,
                    group_name,
                    status_code: status,
                    latency_ms: latency,
                },
            ));
            let mut resp = Response::builder().status(StatusCode::BAD_REQUEST);
            resp.headers_mut().unwrap().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            if cb_probe {
                spawn_cb_probe_release(&state, &config, server, request_model.as_deref());
            }
            return resp
                .body(Body::from(err_body))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }

        // Check if this is a failover status code
        if config.failover_status_codes.contains(&status) {
            emit_uptime_entry(
                &state,
                config.group_id,
                server.server_id,
                status as i16,
                server_latency,
                request_id,
                &request_model,
            );
            // Circuit breaker: record error on failover status code
            if has_cb {
                let tripped = circuit_breaker::record_error(
                    &state.redis,
                    config.group_id,
                    server.server_id,
                    request_model.as_deref(),
                    server.cb_max_failures.unwrap(),
                    server.cb_window_seconds.unwrap(),
                    server.cb_cooldown_seconds.unwrap(),
                    cb_probe,
                )
                .await;
                if tripped {
                    spawn_cb_alert(&state, &config, server, request_model.as_deref());
                }
            }
            continue;
        }

        // Non-failover error
        if status != 200 {
            emit_uptime_entry(
                &state,
                config.group_id,
                server.server_id,
                status as i16,
                server_latency,
                request_id,
                &request_model,
            );

            emit_log_entry(
                &state,
                &config,
                &parsed.group_key,
                last_server_id,
                &last_server_name,
                &request_path,
                &request_method,
                status as i16,
                "upstream_error",
                loop_start.elapsed().as_millis() as i32,
                &failover_chain,
                &request_model,
                None,
                None,
                None,
            );
            if is_billing_endpoint(&request_path) {
                let db = state.db.clone();
                let redis = state.redis.clone();
                let http_client = state.http_client.clone();
                let server_id = last_server_id;
                let server_name = last_server_name.clone();
                let group_name = config.group_name.clone();
                let latency = loop_start.elapsed().as_millis() as i32;
                tokio::spawn(telegram_notifier::maybe_alert(
                    telegram_notifier::AlertContext {
                        db,
                        redis,
                        http_client,
                        server_id,
                        server_name,
                        group_name,
                        status_code: status,
                        latency_ms: latency,
                    },
                ));
            }
            if cb_probe {
                spawn_cb_probe_release(&state, &config, server, request_model.as_deref());
            }
            return build_response(upstream_resp).await;
        }

        // Check if this is an SSE response
        let is_sse = upstream_resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/event-stream"));

        if !is_sse {
            // Non-streaming: the client (or an upstream that ignored `stream: true`)
            // gets nothing until the whole body is ready. Bound the read so a stalled
            // generation still fails over instead of holding the request for hours.
            // Applies regardless of what the client asked for — a response that came
            // back non-SSE is non-streaming from here on either way.
            let read_budget_ms = remaining_timeout_ms(
                server_timeout_ms,
                server_start.elapsed().as_millis() as u64,
            );
            let resp_status_code = StatusCode::from_u16(upstream_resp.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let mut response_headers = HeaderMap::new();
            for (name, value) in upstream_resp.headers().iter() {
                if let Ok(axum_name) =
                    axum::http::header::HeaderName::from_bytes(name.as_str().as_bytes())
                    && let Ok(axum_value) = HeaderValue::from_bytes(value.as_bytes())
                {
                    response_headers.insert(axum_name, axum_value);
                }
            }

            let Some(resp_body_bytes) = read_body_with_timeout(upstream_resp, read_budget_ms).await
            else {
                tracing::warn!(
                    server = %server.server_name,
                    "proxy: non-streaming read timeout, failing over"
                );
                if let Some(last) = failover_chain.last_mut() {
                    last.status = 0;
                }
                emit_non_stream_latency_entry(
                    &state,
                    config.group_id,
                    server.server_id,
                    &request_model,
                    None,
                    true,
                    LatencySource::GroupServer,
                    &request_path,
                    config.group_key_id,
                );
                emit_uptime_entry(
                    &state,
                    config.group_id,
                    server.server_id,
                    0,
                    server_start.elapsed().as_millis() as i32,
                    request_id,
                    &request_model,
                );
                if has_cb {
                    let tripped = circuit_breaker::record_error(
                        &state.redis,
                        config.group_id,
                        server.server_id,
                        request_model.as_deref(),
                        server.cb_max_failures.unwrap(),
                        server.cb_window_seconds.unwrap(),
                        server.cb_cooldown_seconds.unwrap(),
                        cb_probe,
                    )
                    .await;
                    if tripped {
                        spawn_cb_alert(&state, &config, server, request_model.as_deref());
                    }
                }
                continue;
            };
            let total_ms = server_start.elapsed().as_millis() as i32;

            // Status 200 — a successful probe counts toward closing the circuit
            if cb_probe {
                spawn_cb_probe_success(&state, &config, server, request_model.as_deref());
            }
            emit_uptime_entry(
                &state,
                config.group_id,
                server.server_id,
                status as i16,
                server_latency,
                request_id,
                &request_model,
            );
            // Emitted for every path, matching the streaming path's TTFT rows, so the
            // timeout ratio per path stays comparable instead of only logging failures.
            emit_non_stream_latency_entry(
                &state,
                config.group_id,
                server.server_id,
                &request_model,
                Some(total_ms),
                false,
                LatencySource::GroupServer,
                &request_path,
                config.group_key_id,
            );
            // Non-SSE: log failover chain if applicable
            if failover_chain.len() > 1 {
                emit_log_entry(
                    &state,
                    &config,
                    &parsed.group_key,
                    last_server_id,
                    &last_server_name,
                    &request_path,
                    &request_method,
                    status as i16,
                    "failover_success",
                    loop_start.elapsed().as_millis() as i32,
                    &failover_chain,
                    &request_model,
                    None,
                    None,
                    None,
                );
            }
            // Extract token usage from non-streaming billing endpoint 200 responses
            if is_billing_endpoint(&request_path) {
                record_non_stream_usage(
                    &state,
                    &config,
                    server,
                    &parsed,
                    &request_model,
                    selected_subscription_id,
                    selected_tpm_limit,
                    &content_hash,
                    &resp_body_bytes,
                )
                .await;
            }
            let mut resp = Response::builder().status(resp_status_code);
            *resp.headers_mut().unwrap() = response_headers;
            return resp
                .body(Body::from(resp_body_bytes))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }

        // SSE response — measure TTFT
        let ttft_enabled = config.ttft_timeout_ms.is_some();
        let total_servers = config.servers.len();
        let is_last_server = server_idx == total_servers - 1;
        let should_timeout = ttft_enabled && total_servers > 1 && !is_last_server;

        // Build response headers before consuming the stream
        let resp_status = StatusCode::from_u16(upstream_resp.status().as_u16())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response_headers = HeaderMap::new();
        for (name, value) in upstream_resp.headers().iter() {
            if let Ok(axum_name) =
                axum::http::header::HeaderName::from_bytes(name.as_str().as_bytes())
                && let Ok(axum_value) = HeaderValue::from_bytes(value.as_bytes())
            {
                response_headers.insert(axum_name, axum_value);
            }
        }

        let mut stream = upstream_resp.bytes_stream();

        if should_timeout {
            let timeout_ms = config.ttft_timeout_ms.unwrap() as u64;
            let elapsed_ms = server_start.elapsed().as_millis() as u64;

            if elapsed_ms >= timeout_ms {
                // Already exceeded TTFT threshold waiting for headers — failover immediately
                drop(stream);
                if let Some(last) = failover_chain.last_mut() {
                    last.status = 0;
                }
                emit_ttft_entry(
                    &state,
                    config.group_id,
                    server.server_id,
                    &request_model,
                    None,
                    true,
                    &request_path,
                    config.group_key_id,
                );
                emit_uptime_entry(
                    &state,
                    config.group_id,
                    server.server_id,
                    0,
                    server_start.elapsed().as_millis() as i32,
                    request_id,
                    &request_model,
                );
                // Circuit breaker: record TTFT timeout as error
                if has_cb {
                    let tripped = circuit_breaker::record_error(
                        &state.redis,
                        config.group_id,
                        server.server_id,
                        request_model.as_deref(),
                        server.cb_max_failures.unwrap(),
                        server.cb_window_seconds.unwrap(),
                        server.cb_cooldown_seconds.unwrap(),
                        cb_probe,
                    )
                    .await;
                    if tripped {
                        spawn_cb_alert(&state, &config, server, request_model.as_deref());
                    }
                }
                continue;
            }

            let remaining_ms = timeout_ms - elapsed_ms;
            match tokio::time::timeout(
                std::time::Duration::from_millis(remaining_ms),
                stream.next(),
            )
            .await
            {
                Ok(Some(Ok(first_chunk))) => {
                    // First chunk received within timeout
                    if cb_probe {
                        spawn_cb_probe_success(&state, &config, server, request_model.as_deref());
                    }
                    let ttft_ms = server_start.elapsed().as_millis() as i32;
                    emit_ttft_entry(
                        &state,
                        config.group_id,
                        server.server_id,
                        &request_model,
                        Some(ttft_ms),
                        false,
                        &request_path,
                        config.group_key_id,
                    );
                    emit_uptime_entry(
                        &state,
                        config.group_id,
                        server.server_id,
                        status as i16,
                        server_latency,
                        request_id,
                        &request_model,
                    );

                    // Log failover chain if this wasn't the first server tried
                    if failover_chain.len() > 1 {
                        emit_log_entry(
                            &state,
                            &config,
                            &parsed.group_key,
                            last_server_id,
                            &last_server_name,
                            &request_path,
                            &request_method,
                            status as i16,
                            "failover_success",
                            loop_start.elapsed().as_millis() as i32,
                            &failover_chain,
                            &request_model,
                            None,
                            None,
                            None,
                        );
                    }

                    let first = futures_util::stream::iter(std::iter::once(
                        Ok::<_, std::io::Error>(first_chunk),
                    ));
                    let rest = stream.map(|chunk| chunk.map_err(std::io::Error::other));
                    let combined = first.chain(rest);
                    let body = if is_billing_endpoint(&request_path) {
                        let is_dk = parsed.dynamic_keys.contains_key(&server.short_id);
                        let kh = {
                            let raw = if let Some(dk) = parsed.dynamic_keys.get(&server.short_id) {
                                dk.clone()
                            } else {
                                server.api_key.clone().unwrap_or_default()
                            };
                            if raw.is_empty() {
                                None
                            } else {
                                Some(hash_key(&raw))
                            }
                        };
                        let parser = SseUsageParser::new();
                        Body::from_stream(wrap_stream_with_usage_tracking(
                            combined,
                            state.clone(),
                            config.group_id,
                            server.server_id,
                            request_model.clone(),
                            is_dk,
                            kh,
                            config.group_key_id,
                            selected_subscription_id,
                            None,
                            selected_tpm_limit,
                            server.rate_input.unwrap_or(1.0),
                            server.rate_output.unwrap_or(1.0),
                            server.rate_cache_write.unwrap_or(1.0),
                            server.rate_cache_read.unwrap_or(1.0),
                            server.normalize_cache_read,
                            parser,
                            content_hash.clone(),
                        ))
                    } else {
                        Body::from_stream(combined)
                    };
                    let mut resp = Response::builder().status(resp_status);
                    *resp.headers_mut().unwrap() = response_headers;
                    return resp
                        .body(body)
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
                }
                Ok(Some(Err(_))) | Ok(None) => {
                    // Empty stream or stream error — treat as connection error, try next
                    if let Some(last) = failover_chain.last_mut() {
                        last.status = 0;
                    }
                    emit_ttft_entry(
                        &state,
                        config.group_id,
                        server.server_id,
                        &request_model,
                        None,
                        false,
                        &request_path,
                        config.group_key_id,
                    );
                    emit_uptime_entry(
                        &state,
                        config.group_id,
                        server.server_id,
                        0,
                        server_start.elapsed().as_millis() as i32,
                        request_id,
                        &request_model,
                    );
                    if has_cb {
                        let tripped = circuit_breaker::record_error(
                            &state.redis,
                            config.group_id,
                            server.server_id,
                            request_model.as_deref(),
                            server.cb_max_failures.unwrap(),
                            server.cb_window_seconds.unwrap(),
                            server.cb_cooldown_seconds.unwrap(),
                            cb_probe,
                        )
                        .await;
                        if tripped {
                            spawn_cb_alert(&state, &config, server, request_model.as_deref());
                        }
                    }
                    continue;
                }
                Err(_) => {
                    // Timeout — record timed_out, drop stream, try next server
                    drop(stream);
                    if let Some(last) = failover_chain.last_mut() {
                        last.status = 0;
                    }
                    emit_ttft_entry(
                        &state,
                        config.group_id,
                        server.server_id,
                        &request_model,
                        None,
                        true,
                        &request_path,
                        config.group_key_id,
                    );
                    emit_uptime_entry(
                        &state,
                        config.group_id,
                        server.server_id,
                        0,
                        server_start.elapsed().as_millis() as i32,
                        request_id,
                        &request_model,
                    );
                    if has_cb {
                        let tripped = circuit_breaker::record_error(
                            &state.redis,
                            config.group_id,
                            server.server_id,
                            request_model.as_deref(),
                            server.cb_max_failures.unwrap(),
                            server.cb_window_seconds.unwrap(),
                            server.cb_cooldown_seconds.unwrap(),
                            cb_probe,
                        )
                        .await;
                        if tripped {
                            spawn_cb_alert(&state, &config, server, request_model.as_deref());
                        }
                    }
                    continue;
                }
            }
        } else {
            // No timeout: measure TTFT but always wait
            match stream.next().await {
                Some(Ok(first_chunk)) => {
                    if cb_probe {
                        spawn_cb_probe_success(&state, &config, server, request_model.as_deref());
                    }
                    let ttft_ms = server_start.elapsed().as_millis() as i32;
                    emit_ttft_entry(
                        &state,
                        config.group_id,
                        server.server_id,
                        &request_model,
                        Some(ttft_ms),
                        false,
                        &request_path,
                        config.group_key_id,
                    );
                    emit_uptime_entry(
                        &state,
                        config.group_id,
                        server.server_id,
                        status as i16,
                        server_latency,
                        request_id,
                        &request_model,
                    );

                    // Log failover chain if this wasn't the first server tried
                    if failover_chain.len() > 1 {
                        emit_log_entry(
                            &state,
                            &config,
                            &parsed.group_key,
                            last_server_id,
                            &last_server_name,
                            &request_path,
                            &request_method,
                            status as i16,
                            "failover_success",
                            loop_start.elapsed().as_millis() as i32,
                            &failover_chain,
                            &request_model,
                            None,
                            None,
                            None,
                        );
                    }

                    let first = futures_util::stream::iter(std::iter::once(
                        Ok::<_, std::io::Error>(first_chunk),
                    ));
                    let rest = stream.map(|chunk| chunk.map_err(std::io::Error::other));
                    let combined = first.chain(rest);
                    let body = if is_billing_endpoint(&request_path) {
                        let is_dk = parsed.dynamic_keys.contains_key(&server.short_id);
                        let kh = {
                            let raw = if let Some(dk) = parsed.dynamic_keys.get(&server.short_id) {
                                dk.clone()
                            } else {
                                server.api_key.clone().unwrap_or_default()
                            };
                            if raw.is_empty() {
                                None
                            } else {
                                Some(hash_key(&raw))
                            }
                        };
                        let parser = SseUsageParser::new();
                        Body::from_stream(wrap_stream_with_usage_tracking(
                            combined,
                            state.clone(),
                            config.group_id,
                            server.server_id,
                            request_model.clone(),
                            is_dk,
                            kh,
                            config.group_key_id,
                            selected_subscription_id,
                            None,
                            selected_tpm_limit,
                            server.rate_input.unwrap_or(1.0),
                            server.rate_output.unwrap_or(1.0),
                            server.rate_cache_write.unwrap_or(1.0),
                            server.rate_cache_read.unwrap_or(1.0),
                            server.normalize_cache_read,
                            parser,
                            content_hash.clone(),
                        ))
                    } else {
                        Body::from_stream(combined)
                    };
                    let mut resp = Response::builder().status(resp_status);
                    *resp.headers_mut().unwrap() = response_headers;
                    return resp
                        .body(body)
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
                }
                Some(Err(_)) | None => {
                    // Empty stream or error — treat as connection error
                    emit_ttft_entry(
                        &state,
                        config.group_id,
                        server.server_id,
                        &request_model,
                        None,
                        false,
                        &request_path,
                        config.group_key_id,
                    );
                    emit_uptime_entry(
                        &state,
                        config.group_id,
                        server.server_id,
                        0,
                        server_start.elapsed().as_millis() as i32,
                        request_id,
                        &request_model,
                    );
                    if has_cb {
                        let tripped = circuit_breaker::record_error(
                            &state.redis,
                            config.group_id,
                            server.server_id,
                            request_model.as_deref(),
                            server.cb_max_failures.unwrap(),
                            server.cb_window_seconds.unwrap(),
                            server.cb_cooldown_seconds.unwrap(),
                            cb_probe,
                        )
                        .await;
                        if tripped {
                            spawn_cb_alert(&state, &config, server, request_model.as_deref());
                        }
                    }
                    continue;
                }
            }
        }
    }

    // All servers rate-limited — return 429 with rate_limit_error
    if !any_server_attempted && any_rate_limited {
        return fallback_or_subscription_error(
            &state,
            &user_endpoints,
            protocol,
            &original_uri,
            &method,
            &headers,
            &body_bytes,
            &request_model,
            &config,
            &content_hash,
            "Rate limit exceeded",
        )
        .await;
    }

    // All servers skipped — no key available for any server
    if !any_server_attempted {
        return fallback_or_error(
            &state,
            &user_endpoints,
            protocol,
            &original_uri,
            &method,
            &headers,
            &body_bytes,
            &request_model,
            &config,
            &content_hash,
            "authentication_error",
            "No server keys configured",
        )
        .await;
    }

    // All servers with keys exhausted (failover codes or connection errors)
    let all_connection_errors = failover_chain.iter().all(|a| a.status == 0);
    let error_type = if all_connection_errors {
        "connection_error"
    } else {
        "all_servers_exhausted"
    };
    // Use the last non-zero status from the chain, or 429 if all were connection errors
    let final_status: i16 = failover_chain
        .iter()
        .rev()
        .find(|a| a.status != 0)
        .map(|a| a.status as i16)
        .unwrap_or(429);

    emit_log_entry(
        &state,
        &config,
        &parsed.group_key,
        last_server_id,
        &last_server_name,
        &request_path,
        &request_method,
        final_status,
        error_type,
        loop_start.elapsed().as_millis() as i32,
        &failover_chain,
        &request_model,
        None,
        None,
        None,
    );

    if is_billing_endpoint(&request_path) && final_status != 0 {
        let db = state.db.clone();
        let redis = state.redis.clone();
        let http_client = state.http_client.clone();
        let server_id = last_server_id;
        let server_name = last_server_name.clone();
        let group_name = config.group_name.clone();
        let latency = loop_start.elapsed().as_millis() as i32;
        tokio::spawn(telegram_notifier::maybe_alert(
            telegram_notifier::AlertContext {
                db,
                redis,
                http_client,
                server_id,
                server_name,
                group_name,
                status_code: final_status as u16,
                latency_ms: latency,
            },
        ));
    }

    return fallback_or_overloaded_error(
        &state,
        &user_endpoints,
        protocol,
        &original_uri,
        &method,
        &headers,
        &body_bytes,
        &request_model,
        &config,
        &content_hash,
    )
    .await;
}

use std::pin::Pin;
use std::task::{Context, Poll};

struct UsageTrackingStream<S> {
    inner: S,
    parser: Option<SseUsageParser>,
    state: AppState,
    group_id: uuid::Uuid,
    server_id: uuid::Uuid,
    model: Option<String>,
    is_dynamic_key: bool,
    key_hash: Option<String>,
    group_key_id: Option<uuid::Uuid>,
    subscription_id: Option<uuid::Uuid>,
    user_endpoint_id: Option<uuid::Uuid>,
    tpm_limit: Option<f64>,
    rate_input: f64,
    rate_output: f64,
    rate_cache_write: f64,
    rate_cache_read: f64,
    normalize_cache_read: bool,
    content_hash: Option<String>,
    done: bool,
}

impl<S> futures_util::Stream for UsageTrackingStream<S>
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(ref mut parser) = this.parser {
                    parser.feed(&chunk);
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.parser = None;
                this.done = true;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.done = true;
                if let Some(parser) = this.parser.take()
                    && let Some(usage) = parser.finish()
                {
                    let state = this.state.clone();
                    let group_id = this.group_id;
                    let server_id = this.server_id;
                    let model = this.model.clone();
                    let is_dynamic_key = this.is_dynamic_key;
                    let key_hash = this.key_hash.clone();
                    let group_key_id = this.group_key_id;
                    let subscription_id = this.subscription_id;
                    let user_endpoint_id = this.user_endpoint_id;
                    let tpm_limit = this.tpm_limit;
                    let rate_input = this.rate_input;
                    let rate_output = this.rate_output;
                    let rate_cache_write = this.rate_cache_write;
                    let rate_cache_read = this.rate_cache_read;
                    let normalize_cache_read = this.normalize_cache_read;
                    let content_hash = this.content_hash.clone();
                    let input_tokens = usage.input_tokens;
                    let output_tokens = usage.output_tokens;
                    let cache_creation_tokens = usage.cache_creation_tokens;
                    let cache_read_tokens = usage.cache_read_tokens;

                    // Spawn async task for cost calculation and usage tracking
                    tokio::spawn(async move {
                        let cost_usd = if let Some(ref model_name) = model {
                            if let Some(sub_id) = subscription_id {
                                if let Ok(sub) =
                                    sqlx::query_as::<_, crate::models::KeySubscription>(
                                        "SELECT * FROM key_subscriptions WHERE id = $1",
                                    )
                                    .bind(sub_id)
                                    .fetch_one(&state.db)
                                    .await
                                {
                                    // Bonus subscriptions: calculate cost for tracking, but no activation or counters
                                    if sub.sub_type == "bonus" {
                                        let pricing_cache = state.pricing_cache.read().await;
                                        if let Some(pricing) = pricing_cache.get(model_name) {
                                            let c = crate::subscription::calculate_cost(
                                                pricing,
                                                rate_input,
                                                rate_output,
                                                rate_cache_write,
                                                rate_cache_read,
                                                input_tokens,
                                                output_tokens,
                                                cache_creation_tokens,
                                                cache_read_tokens,
                                                normalize_cache_read,
                                            );
                                            drop(pricing_cache);
                                            Some(c)
                                        } else {
                                            drop(pricing_cache);
                                            None
                                        }
                                    } else {
                                        let cost = if sub.sub_type == "pay_per_request" {
                                            sub.model_request_costs
                                                .as_object()
                                                .and_then(|m| m.get(model_name.as_str()))
                                                .and_then(|v| v.as_f64())
                                                .unwrap_or(0.0)
                                        } else {
                                            let pricing_cache = state.pricing_cache.read().await;
                                            if let Some(pricing) = pricing_cache.get(model_name) {
                                                let c = crate::subscription::calculate_cost(
                                                    pricing,
                                                    rate_input,
                                                    rate_output,
                                                    rate_cache_write,
                                                    rate_cache_read,
                                                    input_tokens,
                                                    output_tokens,
                                                    cache_creation_tokens,
                                                    cache_read_tokens,
                                                    normalize_cache_read,
                                                );
                                                drop(pricing_cache);
                                                c
                                            } else {
                                                drop(pricing_cache);
                                                0.0
                                            }
                                        };

                                        crate::subscription::ensure_activated(
                                            &state,
                                            sub_id,
                                            sub.duration_days,
                                        )
                                        .await;

                                        crate::subscription::update_cost_counters(
                                            &state,
                                            sub_id,
                                            model_name,
                                            cost,
                                            sub.reset_hours,
                                            sub.weekly_cost_limit_usd,
                                        )
                                        .await;

                                        if tpm_limit.is_some() {
                                            crate::subscription::increment_tpm(
                                                &state,
                                                sub_id,
                                                input_tokens,
                                                output_tokens,
                                            )
                                            .await;
                                        }

                                        Some(cost)
                                    }
                                } else {
                                    None
                                }
                            } else {
                                // No subscription — still calculate cost for logging
                                let pricing_cache = state.pricing_cache.read().await;
                                if let Some(pricing) = pricing_cache.get(model_name) {
                                    let cost = crate::subscription::calculate_cost(
                                        pricing,
                                        rate_input,
                                        rate_output,
                                        rate_cache_write,
                                        rate_cache_read,
                                        input_tokens,
                                        output_tokens,
                                        cache_creation_tokens,
                                        cache_read_tokens,
                                        normalize_cache_read,
                                    );
                                    drop(pricing_cache);
                                    Some(cost)
                                } else {
                                    None
                                }
                            }
                        } else {
                            None
                        };

                        let entry = TokenUsageEntry {
                            group_id,
                            server_id,
                            model,
                            input_tokens,
                            output_tokens,
                            cache_creation_tokens,
                            cache_read_tokens,
                            is_dynamic_key,
                            key_hash,
                            group_key_id,
                            cost_usd,
                            subscription_id,
                            user_endpoint_id,
                            created_at: Utc::now(),
                            content_hash,
                        };
                        if state.usage_tx.try_send(entry).is_err() {
                            tracing::warn!("Usage buffer full, dropping token usage entry");
                        }
                    });
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn wrap_stream_with_usage_tracking<S>(
    stream: S,
    state: AppState,
    group_id: uuid::Uuid,
    server_id: uuid::Uuid,
    model: Option<String>,
    is_dynamic_key: bool,
    key_hash: Option<String>,
    group_key_id: Option<uuid::Uuid>,
    subscription_id: Option<uuid::Uuid>,
    user_endpoint_id: Option<uuid::Uuid>,
    tpm_limit: Option<f64>,
    rate_input: f64,
    rate_output: f64,
    rate_cache_write: f64,
    rate_cache_read: f64,
    normalize_cache_read: bool,
    parser: SseUsageParser,
    content_hash: Option<String>,
) -> UsageTrackingStream<S>
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    UsageTrackingStream {
        inner: stream,
        parser: Some(parser),
        state,
        group_id,
        server_id,
        model,
        is_dynamic_key,
        key_hash,
        group_key_id,
        subscription_id,
        user_endpoint_id,
        tpm_limit,
        rate_input,
        rate_output,
        rate_cache_write,
        rate_cache_read,
        normalize_cache_read,
        content_hash,
        done: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_log_entry(
    state: &AppState,
    config: &GroupConfig,
    group_api_key: &str,
    server_id: uuid::Uuid,
    server_name: &str,
    request_path: &str,
    request_method: &str,
    status_code: i16,
    error_type: &str,
    latency_ms: i32,
    failover_chain: &[FailoverAttempt],
    request_model: &Option<String>,
    request_body: Option<serde_json::Value>,
    request_headers: Option<serde_json::Value>,
    upstream_url: Option<String>,
) {
    let entry = ProxyLogEntry {
        group_id: config.group_id,
        group_api_key: group_api_key.to_string(),
        server_id,
        server_name: server_name.to_string(),
        request_path: request_path.to_string(),
        request_method: request_method.to_string(),
        status_code,
        error_type: error_type.to_string(),
        latency_ms,
        failover_chain: failover_chain.to_vec(),
        request_model: request_model.clone(),
        request_body,
        request_headers,
        upstream_url,
        created_at: Utc::now(),
    };

    if state.log_tx.try_send(entry).is_err() {
        tracing::warn!("Log buffer full, dropping proxy log entry");
    }
}

/// Parse usage out of a non-streaming 200 body, price it, update subscription
/// counters, and queue a token-usage row.
///
/// Shared by the plain non-streaming path and the thinking-signature retry path,
/// so a retried request is billed exactly like a first-attempt one.
#[allow(clippy::too_many_arguments)]
async fn record_non_stream_usage(
    state: &AppState,
    config: &GroupConfig,
    server: &GroupServerDetail,
    parsed: &crate::routes::key_parser::ParsedKey,
    request_model: &Option<String>,
    selected_subscription_id: Option<uuid::Uuid>,
    selected_tpm_limit: Option<f64>,
    content_hash: &Option<String>,
    body: &[u8],
) {
    let Some(usage) = extract_usage_tokens(body) else {
        return;
    };
    let UsageTokens {
        input_tokens: inp,
        output_tokens: out,
        cache_creation_tokens: cache_creation,
        cache_read_tokens: cache_read,
    } = usage;

    let is_dk = parsed.dynamic_keys.contains_key(&server.short_id);
    let key_hash = {
        let raw = if let Some(dk) = parsed.dynamic_keys.get(&server.short_id) {
            dk.clone()
        } else {
            server.api_key.clone().unwrap_or_default()
        };
        if raw.is_empty() {
            None
        } else {
            Some(hash_key(&raw))
        }
    };

    let ri = server.rate_input.unwrap_or(1.0);
    let ro = server.rate_output.unwrap_or(1.0);
    let rcw = server.rate_cache_write.unwrap_or(1.0);
    let rcr = server.rate_cache_read.unwrap_or(1.0);

    let priced = |pricing: &crate::routes::ModelPricing| {
        crate::subscription::calculate_cost(
            pricing,
            ri,
            ro,
            rcw,
            rcr,
            inp,
            out,
            cache_creation,
            cache_read,
            server.normalize_cache_read,
        )
    };

    let cost_usd = match (request_model.as_ref(), selected_subscription_id) {
        (None, _) => None,
        (Some(model_name), Some(sub_id)) => {
            match sqlx::query_as::<_, crate::models::KeySubscription>(
                "SELECT * FROM key_subscriptions WHERE id = $1",
            )
            .bind(sub_id)
            .fetch_one(&state.db)
            .await
            {
                Ok(sub) => {
                    let cost = if sub.sub_type == "pay_per_request" {
                        // Flat cost from model_request_costs
                        sub.model_request_costs
                            .as_object()
                            .and_then(|m| m.get(model_name.as_str()))
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0)
                    } else {
                        let pricing_cache = state.pricing_cache.read().await;
                        pricing_cache.get(model_name).map(priced).unwrap_or(0.0)
                    };

                    // Lazy activation
                    crate::subscription::ensure_activated(state, sub_id, sub.duration_days).await;

                    crate::subscription::update_cost_counters(
                        state,
                        sub_id,
                        model_name,
                        cost,
                        sub.reset_hours,
                        sub.weekly_cost_limit_usd,
                    )
                    .await;

                    if selected_tpm_limit.is_some() {
                        crate::subscription::increment_tpm(state, sub_id, inp, out).await;
                    }

                    Some(cost)
                }
                Err(_) => None,
            }
        }
        (Some(model_name), None) => {
            // No subscription — still calculate cost for logging
            let pricing_cache = state.pricing_cache.read().await;
            pricing_cache.get(model_name).map(priced)
        }
    };

    let entry = TokenUsageEntry {
        group_id: config.group_id,
        server_id: server.server_id,
        model: request_model.clone(),
        input_tokens: inp,
        output_tokens: out,
        cache_creation_tokens: cache_creation,
        cache_read_tokens: cache_read,
        is_dynamic_key: is_dk,
        key_hash,
        group_key_id: config.group_key_id,
        cost_usd,
        subscription_id: selected_subscription_id,
        user_endpoint_id: None,
        created_at: Utc::now(),
        content_hash: content_hash.clone(),
    };
    if state.usage_tx.try_send(entry).is_err() {
        tracing::warn!("Usage buffer full, dropping token usage entry");
    }
}

/// Build the client response for a successful billing-endpoint call while still
/// accounting for token usage, for cases outside the main waterfall exit — currently
/// the thinking-signature retry, which previously returned the body untracked and so
/// billed nothing for a request the upstream had really served.
///
/// A streamed retry is wrapped with the usage-tracking stream; a non-streamed one has
/// its body parsed directly. No TTFT row is written here: the retry's first-chunk time
/// was never measured, and inventing one would corrupt the percentiles.
#[allow(clippy::too_many_arguments)]
async fn build_tracked_billing_response(
    state: &AppState,
    config: &GroupConfig,
    server: &GroupServerDetail,
    parsed: &crate::routes::key_parser::ParsedKey,
    resp: reqwest::Response,
    request_path: &str,
    request_model: &Option<String>,
    selected_subscription_id: Option<uuid::Uuid>,
    selected_tpm_limit: Option<f64>,
    content_hash: &Option<String>,
    server_start: std::time::Instant,
    non_stream_timeout_ms: Option<i32>,
) -> Response {
    let is_sse = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"));

    let resp_status = StatusCode::from_u16(resp.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response_headers = HeaderMap::new();
    for (name, value) in resp.headers().iter() {
        if let Ok(axum_name) = axum::http::header::HeaderName::from_bytes(name.as_str().as_bytes())
            && let Ok(axum_value) = HeaderValue::from_bytes(value.as_bytes())
        {
            response_headers.insert(axum_name, axum_value);
        }
    }

    if is_sse {
        let is_dk = parsed.dynamic_keys.contains_key(&server.short_id);
        let key_hash = {
            let raw = if let Some(dk) = parsed.dynamic_keys.get(&server.short_id) {
                dk.clone()
            } else {
                server.api_key.clone().unwrap_or_default()
            };
            if raw.is_empty() {
                None
            } else {
                Some(hash_key(&raw))
            }
        };
        let parser = SseUsageParser::new();
        let stream = resp
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other));
        let body = Body::from_stream(wrap_stream_with_usage_tracking(
            stream,
            state.clone(),
            config.group_id,
            server.server_id,
            request_model.clone(),
            is_dk,
            key_hash,
            config.group_key_id,
            selected_subscription_id,
            None,
            selected_tpm_limit,
            server.rate_input.unwrap_or(1.0),
            server.rate_output.unwrap_or(1.0),
            server.rate_cache_write.unwrap_or(1.0),
            server.rate_cache_read.unwrap_or(1.0),
            server.normalize_cache_read,
            parser,
            content_hash.clone(),
        ));
        let mut builder = Response::builder().status(resp_status);
        *builder.headers_mut().unwrap() = response_headers;
        return builder
            .body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    let read_budget_ms = remaining_timeout_ms(
        non_stream_timeout_ms,
        server_start.elapsed().as_millis() as u64,
    );
    let Some(body_bytes) = read_body_with_timeout(resp, read_budget_ms).await else {
        // Nothing left to fail over to at this point — the retry already succeeded
        // upstream, so report the stall rather than pretending it produced a body.
        emit_non_stream_latency_entry(
            state,
            config.group_id,
            server.server_id,
            request_model,
            None,
            true,
            LatencySource::GroupServer,
            request_path,
            config.group_key_id,
        );
        return api_error(
            request_path,
            StatusCode::GATEWAY_TIMEOUT,
            "api_error",
            "Upstream timed out while sending the response body",
        );
    };

    emit_non_stream_latency_entry(
        state,
        config.group_id,
        server.server_id,
        request_model,
        Some(server_start.elapsed().as_millis() as i32),
        false,
        LatencySource::GroupServer,
        request_path,
        config.group_key_id,
    );
    record_non_stream_usage(
        state,
        config,
        server,
        parsed,
        request_model,
        selected_subscription_id,
        selected_tpm_limit,
        content_hash,
        &body_bytes,
    )
    .await;

    let mut builder = Response::builder().status(resp_status);
    *builder.headers_mut().unwrap() = response_headers;
    builder
        .body(Body::from(body_bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Which waterfall a latency row came from. The bonus and user-endpoint paths both
/// write `server_id = nil`, so this is what distinguishes them on the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LatencySource {
    GroupServer,
    Bonus,
    UserEndpoint,
}

impl LatencySource {
    /// Must match the values the admin TTFT query maps to display names.
    fn as_str(self) -> &'static str {
        match self {
            Self::GroupServer => "group_server",
            Self::Bonus => "bonus",
            Self::UserEndpoint => "user_endpoint",
        }
    }
}

/// Emit a TTFT log entry for a streaming response — `ttft_ms` is time-to-first-chunk.
#[allow(clippy::too_many_arguments)]
fn emit_ttft_entry(
    state: &AppState,
    group_id: uuid::Uuid,
    server_id: uuid::Uuid,
    request_model: &Option<String>,
    ttft_ms: Option<i32>,
    timed_out: bool,
    request_path: &str,
    group_key_id: Option<uuid::Uuid>,
) {
    emit_latency_entry(
        state,
        group_id,
        server_id,
        request_model,
        ttft_ms,
        None,
        timed_out,
        true,
        LatencySource::GroupServer,
        request_path,
        group_key_id,
    );
}

/// Emit a latency log entry for a non-streaming response — `total_ms` is the
/// full end-to-end upstream time, not a time-to-first-token measurement.
#[allow(clippy::too_many_arguments)]
fn emit_non_stream_latency_entry(
    state: &AppState,
    group_id: uuid::Uuid,
    server_id: uuid::Uuid,
    request_model: &Option<String>,
    total_ms: Option<i32>,
    timed_out: bool,
    source: LatencySource,
    request_path: &str,
    group_key_id: Option<uuid::Uuid>,
) {
    emit_latency_entry(
        state,
        group_id,
        server_id,
        request_model,
        None,
        total_ms,
        timed_out,
        false,
        source,
        request_path,
        group_key_id,
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_latency_entry(
    state: &AppState,
    group_id: uuid::Uuid,
    server_id: uuid::Uuid,
    request_model: &Option<String>,
    ttft_ms: Option<i32>,
    total_ms: Option<i32>,
    timed_out: bool,
    is_streaming: bool,
    source: LatencySource,
    request_path: &str,
    group_key_id: Option<uuid::Uuid>,
) {
    let entry = TtftLogEntry {
        group_id,
        server_id,
        request_model: request_model.clone(),
        ttft_ms,
        total_ms,
        timed_out,
        is_streaming,
        source: source.as_str().to_string(),
        request_path: request_path.to_string(),
        created_at: Utc::now(),
        group_key_id,
    };

    if state.ttft_tx.try_send(entry).is_err() {
        tracing::warn!("TTFT buffer full, dropping TTFT log entry");
    }
}

fn emit_uptime_entry(
    state: &AppState,
    group_id: uuid::Uuid,
    server_id: uuid::Uuid,
    status_code: i16,
    latency_ms: i32,
    request_id: uuid::Uuid,
    request_model: &Option<String>,
) {
    let entry = UptimeCheckEntry {
        group_id,
        server_id,
        status_code,
        latency_ms,
        request_id,
        request_model: request_model.clone(),
        created_at: Utc::now(),
    };

    if state.uptime_tx.try_send(entry).is_err() {
        tracing::warn!("Uptime buffer full, dropping uptime check entry");
    }
}

async fn build_response(upstream: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let mut response_headers = HeaderMap::new();
    for (name, value) in upstream.headers().iter() {
        if let Ok(axum_name) = axum::http::header::HeaderName::from_bytes(name.as_str().as_bytes())
            && let Ok(axum_value) = HeaderValue::from_bytes(value.as_bytes())
        {
            response_headers.insert(axum_name, axum_value);
        }
    }

    // Check if this is a streaming SSE response
    let is_sse = upstream
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"));

    if is_sse {
        let stream = upstream
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other));
        let body = Body::from_stream(stream);
        let mut resp = Response::builder().status(status);
        *resp.headers_mut().unwrap() = response_headers;
        resp.body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    } else {
        let body_bytes = upstream.bytes().await.unwrap_or_default();
        let mut resp = Response::builder().status(status);
        *resp.headers_mut().unwrap() = response_headers;
        resp.body(Body::from(body_bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;
    use tokio::sync::mpsc;

    // --- upstream_url_for (protocol-driven path rewrite) ---

    fn uri(s: &str) -> axum::http::Uri {
        s.parse().unwrap()
    }

    #[test]
    fn chat_completions_targets_upstream_messages() {
        let url = upstream_url_for(
            ClientProtocol::ChatCompletions,
            "https://up.example.com",
            &uri("/v1/chat/completions"),
        );
        assert_eq!(url, "https://up.example.com/v1/messages");
    }

    #[test]
    fn responses_targets_upstream_messages_without_double_slash() {
        let url = upstream_url_for(
            ClientProtocol::Responses,
            "https://up.example.com/",
            &uri("/v1/responses"),
        );
        assert_eq!(url, "https://up.example.com/v1/messages");
    }

    #[test]
    fn translated_protocol_drops_client_query_string() {
        // An OpenAI query string means nothing to the Anthropic endpoint.
        let url = upstream_url_for(
            ClientProtocol::ChatCompletions,
            "https://up.example.com",
            &uri("/v1/chat/completions?foo=bar"),
        );
        assert_eq!(url, "https://up.example.com/v1/messages");
    }

    #[test]
    fn anthropic_keeps_path_and_query() {
        let url = upstream_url_for(
            ClientProtocol::Anthropic,
            "https://up.example.com",
            &uri("/v1/messages?beta=true"),
        );
        assert_eq!(url, "https://up.example.com/v1/messages?beta=true");
    }

    #[test]
    fn anthropic_preserves_count_tokens_path() {
        // The count-tokens waterfall must reach the upstream's own
        // count_tokens endpoint, not be rewritten to /v1/messages.
        let url = upstream_url_for(
            ClientProtocol::Anthropic,
            "https://up.example.com",
            &uri("/v1/messages/count_tokens"),
        );
        assert_eq!(url, "https://up.example.com/v1/messages/count_tokens");
    }

    // --- billing / error envelope wiring ---

    #[test]
    fn all_three_client_paths_are_billable() {
        assert!(is_billing_endpoint("/v1/messages"));
        assert!(is_billing_endpoint("/v1/chat/completions"));
        assert!(is_billing_endpoint("/v1/responses"));
        assert!(!is_billing_endpoint("/v1/messages/count_tokens"));
    }

    #[test]
    fn api_error_envelope_follows_the_calling_path() {
        // Same failure, three shapes — the discriminator each protocol's
        // clients parse.
        let anthropic = api_error(
            "/v1/messages",
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "Invalid API key",
        );
        assert_eq!(anthropic.status(), StatusCode::UNAUTHORIZED);

        let chat = api_error(
            "/v1/chat/completions",
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "Invalid API key",
        );
        assert_eq!(chat.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn error_envelope_bodies_differ_per_protocol() {
        let anthropic =
            translate::error_envelope(ClientProtocol::Anthropic, "api_error", "boom");
        let chat =
            translate::error_envelope(ClientProtocol::ChatCompletions, "api_error", "boom");
        let responses =
            translate::error_envelope(ClientProtocol::Responses, "api_error", "boom");

        assert_eq!(anthropic["type"], "error");
        assert!(chat.get("type").is_none());
        assert_eq!(responses["object"], "error");
    }

    // --- outbound seam gating ---

    fn chat_ctx(client_model: Option<&str>) -> TranslationContext {
        TranslationContext {
            protocol: ClientProtocol::ChatCompletions,
            include_usage: false,
            json_schema_tool: None,
            client_model: client_model.map(str::to_string),
        }
    }

    #[test]
    fn non_json_success_body_is_forwarded_untouched() {
        // The only thing the seam declines to reshape: a body it cannot parse.
        // It does not inspect shape beyond that — the decision to translate is
        // the caller's, from protocol plus status.
        assert!(translate_success_body(&chat_ctx(None), b"<html>not json</html>").is_none());
    }

    #[test]
    fn anthropic_message_is_translated_by_the_seam() {
        let body = br#"{"id":"msg_1","model":"m","stop_reason":"end_turn",
            "content":[{"type":"text","text":"hi"}],
            "usage":{"input_tokens":1,"output_tokens":1}}"#;
        let out = translate_success_body(&chat_ctx(None), body).expect("translated");
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["choices"][0]["message"]["content"], "hi");
    }

    #[test]
    fn seam_echoes_the_client_model_not_the_upstream_one() {
        // The client asked for gpt-4o; a per-server mapping sent
        // claude-sonnet-4-6 upstream. OpenAI SDKs assert on the requested name.
        let body = br#"{"id":"msg_1","model":"claude-sonnet-4-6","stop_reason":"end_turn",
            "content":[{"type":"text","text":"hi"}],
            "usage":{"input_tokens":1,"output_tokens":1}}"#;
        let out = translate_success_body(&chat_ctx(Some("gpt-4o")), body).expect("translated");
        assert_eq!(out["model"], "gpt-4o");
    }

    #[test]
    fn seam_falls_back_to_upstream_model_when_client_sent_none() {
        let body = br#"{"id":"msg_1","model":"claude-sonnet-4-6","stop_reason":"end_turn",
            "content":[{"type":"text","text":"hi"}],
            "usage":{"input_tokens":1,"output_tokens":1}}"#;
        let out = translate_success_body(&chat_ctx(None), body).expect("translated");
        assert_eq!(out["model"], "claude-sonnet-4-6");
    }

    // --- router topology: do the two new paths actually reach this handler? ---

    /// Build an `AppState` that never touches a real database or Redis.
    ///
    /// `connect_lazy` defers the TCP connect to first query, and the channels
    /// are plain in-memory senders — enough for a request to be *routed* to
    /// `proxy_handler`, which is what these tests are about. The handler then
    /// fails auth (there is no key), and that 401 is itself the proof it was
    /// reached: an unrouted path would 404 from the SPA fallback instead.
    fn offline_state() -> AppState {
        // A tiny acquire timeout matters: sqlx's default is 30s, and the handler
        // does hit the DB (blocked-paths lookup) before failing open. Without
        // this the test would sit for half a minute proving nothing.
        let db = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(1))
            .connect_lazy("postgres://localhost/viber_router_test")
            .expect("lazy pool");
        // Port 1 has nothing listening and is refused immediately, which keeps
        // these tests fast; every Redis read in the handler fails open.
        let mut redis_cfg = deadpool_redis::Config::from_url("redis://127.0.0.1:1");
        let mut pool_cfg = deadpool_redis::PoolConfig::new(1);
        pool_cfg.timeouts.wait = Some(std::time::Duration::from_millis(1));
        pool_cfg.timeouts.create = Some(std::time::Duration::from_millis(1));
        redis_cfg.pool = Some(pool_cfg);
        let redis = redis_cfg
            .create_pool(Some(deadpool_redis::Runtime::Tokio1))
            .expect("redis pool");
        let (log_tx, _log_rx) = mpsc::channel(1);
        let (ttft_tx, _ttft_rx) = mpsc::channel(1);
        let (usage_tx, _usage_rx) = mpsc::channel(1);
        let (uptime_tx, _uptime_rx) = mpsc::channel(1);
        AppState {
            db,
            redis,
            admin_token: "test".to_string(),
            http_client: reqwest::Client::new(),
            log_tx,
            ttft_tx,
            usage_tx,
            uptime_tx,
            pricing_cache: Default::default(),
            unlocked_servers: Default::default(),
        }
    }

    /// Send one request through the *whole* app router, exactly as the binary
    /// wires it — nest("/v1", proxy::router().layer(cors)) included.
    async fn route_through_app(method: Method, path: &str) -> Response {
        use tower::ServiceExt;
        crate::routes::router(offline_state())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router responds")
    }

    #[tokio::test]
    async fn chat_completions_path_reaches_the_proxy_handler() {
        // 401 (not 404) proves the request landed on proxy_handler: only that
        // handler answers with an auth error. The path is served by the `/v1`
        // fallback route, not by an explicit `.route()` entry.
        let resp = route_through_app(Method::POST, "/v1/chat/completions").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn responses_path_reaches_the_proxy_handler() {
        let resp = route_through_app(Method::POST, "/v1/responses").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn chat_completions_auth_error_is_openai_shaped_end_to_end() {
        // The same 401 as above, but checking the body: an OpenAI client must
        // get an OpenAI envelope even for a relay-generated auth failure.
        let resp = route_through_app(Method::POST, "/v1/chat/completions").await;
        let body: Value = serde_json::from_str(&read_body(resp).await).unwrap();
        assert!(body.get("type").is_none());
        assert_eq!(body["error"]["type"], "authentication_error");
    }

    #[tokio::test]
    async fn responses_auth_error_is_responses_shaped_end_to_end() {
        let resp = route_through_app(Method::POST, "/v1/responses").await;
        let body: Value = serde_json::from_str(&read_body(resp).await).unwrap();
        assert_eq!(body["object"], "error");
        assert_eq!(body["error"]["type"], "authentication_error");
    }

    #[tokio::test]
    async fn messages_path_still_reaches_the_proxy_handler_in_anthropic_shape() {
        let resp = route_through_app(Method::POST, "/v1/messages").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body: Value = serde_json::from_str(&read_body(resp).await).unwrap();
        assert_eq!(body["type"], "error");
    }

    #[tokio::test]
    async fn cors_preflight_is_answered_by_the_cors_layer_not_the_handler() {
        // A browser-issued preflight carries Origin + Access-Control-Request-Method
        // and must be answered 200 by CorsLayer without ever reaching auth.
        use tower::ServiceExt;
        let resp = crate::routes::router(offline_state())
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/v1/chat/completions")
                    .header("origin", "https://example.com")
                    .header("access-control-request-method", "POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router responds");

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()
                .get("access-control-allow-origin")
                .is_some(),
            "preflight must carry CORS headers"
        );
    }

    // --- outbound seam, end to end through translate_client_response ---

    fn ctx_for(protocol: ClientProtocol) -> TranslationContext {
        TranslationContext {
            protocol,
            include_usage: false,
            json_schema_tool: None,
            client_model: None,
        }
    }

    /// Build a buffered JSON response the way an exhausted waterfall does.
    fn json_response(status: StatusCode, body: &str) -> Response {
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    /// Build a streaming SSE response the way the streaming exits do.
    fn sse_response(body: &'static str) -> Response {
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body))
            .unwrap()
    }

    async fn read_body(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body readable");
        String::from_utf8(bytes.to_vec()).expect("body is utf-8")
    }

    /// An upstream Anthropic error body, as Anthropic really returns them.
    const UPSTREAM_400: &str =
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"model not found"}}"#;

    #[tokio::test]
    async fn upstream_json_error_reaches_chat_client_in_openai_shape() {
        let resp = translate_client_response(
            &ctx_for(ClientProtocol::ChatCompletions),
            json_response(StatusCode::BAD_REQUEST, UPSTREAM_400),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_str(&read_body(resp).await).unwrap();
        // OpenAI shape: no top-level `type`, everything under `error`.
        assert!(body.get("type").is_none());
        assert_eq!(body["error"]["message"], "model not found");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(body["error"].get("code").is_some());
    }

    #[tokio::test]
    async fn upstream_json_error_reaches_responses_client_in_responses_shape() {
        let resp = translate_client_response(
            &ctx_for(ClientProtocol::Responses),
            json_response(StatusCode::UNAUTHORIZED, UPSTREAM_400),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body: Value = serde_json::from_str(&read_body(resp).await).unwrap();
        assert_eq!(body["object"], "error");
        assert_eq!(body["error"]["message"], "model not found");
    }

    #[tokio::test]
    async fn upstream_json_error_reaches_anthropic_client_untouched() {
        let resp = translate_client_response(
            &ctx_for(ClientProtocol::Anthropic),
            json_response(StatusCode::TOO_MANY_REQUESTS, UPSTREAM_400),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        // Byte-identical: the Anthropic path must not be reshaped at all.
        assert_eq!(read_body(resp).await, UPSTREAM_400);
    }

    #[tokio::test]
    async fn upstream_429_body_is_reshaped_but_status_is_preserved() {
        // Status codes drive client retry logic, so the seam must never rewrite
        // them while reshaping the body.
        for protocol in [ClientProtocol::ChatCompletions, ClientProtocol::Responses] {
            let resp = translate_client_response(
                &ctx_for(protocol),
                json_response(StatusCode::TOO_MANY_REQUESTS, UPSTREAM_400),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
            let body: Value = serde_json::from_str(&read_body(resp).await).unwrap();
            assert_eq!(body["error"]["message"], "model not found");
        }
    }

    #[tokio::test]
    async fn non_anthropic_error_body_still_reaches_openai_client_shaped() {
        // A gateway in front of the upstream can return HTML. The client still
        // needs a parseable envelope, with the raw text as the message.
        let resp = translate_client_response(
            &ctx_for(ClientProtocol::ChatCompletions),
            json_response(StatusCode::BAD_GATEWAY, "<html>502 Bad Gateway</html>"),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body: Value = serde_json::from_str(&read_body(resp).await).unwrap();
        assert_eq!(body["error"]["message"], "<html>502 Bad Gateway</html>");
        assert_eq!(body["error"]["type"], "api_error");
    }

    /// An upstream stream that starts fine, then fails partway through.
    const UPSTREAM_SSE_ERROR: &str = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"model":"claude-opus-4-6","usage":{"input_tokens":5}}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
        "\n\n",
        "event: error\n",
        r#"data: {"type":"error","error":{"type":"overloaded_error","message":"upstream overloaded"}}"#,
        "\n\n",
    );

    #[tokio::test]
    async fn mid_stream_error_reaches_chat_client_as_error_chunk_then_done() {
        let resp = translate_client_response(
            &ctx_for(ClientProtocol::ChatCompletions),
            sse_response(UPSTREAM_SSE_ERROR),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        // The partial content the client already paid for survives.
        assert!(body.contains("partial"));
        // Then an OpenAI-shaped error, then a clean terminator.
        assert!(body.contains(r#""message":"upstream overloaded""#));
        assert!(body.trim_end().ends_with("data: [DONE]"));
    }

    #[tokio::test]
    async fn mid_stream_error_reaches_responses_client_as_response_failed() {
        let resp = translate_client_response(
            &ctx_for(ClientProtocol::Responses),
            sse_response(UPSTREAM_SSE_ERROR),
        )
        .await;

        let body = read_body(resp).await;
        assert!(body.contains("response.output_text.delta"));
        assert!(body.contains("response.failed"));
        // Responses has no [DONE] sentinel, and a failed stream never completes.
        assert!(!body.contains("response.completed"));
        assert!(!body.contains("[DONE]"));
    }

    #[tokio::test]
    async fn mid_stream_error_reaches_anthropic_client_untouched() {
        let resp = translate_client_response(
            &ctx_for(ClientProtocol::Anthropic),
            sse_response(UPSTREAM_SSE_ERROR),
        )
        .await;

        assert_eq!(read_body(resp).await, UPSTREAM_SSE_ERROR);
    }

    #[tokio::test]
    async fn error_status_with_sse_content_type_is_still_reshaped_as_json() {
        // Some upstreams answer a streaming request with an error while the
        // content-type still says event-stream. The client asked to stream but
        // got a failure, so it needs a JSON error envelope, not an SSE frame.
        let resp = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(UPSTREAM_400))
            .unwrap();
        let resp = translate_client_response(&ctx_for(ClientProtocol::ChatCompletions), resp).await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body: Value = serde_json::from_str(&read_body(resp).await).unwrap();
        assert_eq!(body["error"]["message"], "model not found");
    }

    #[tokio::test]
    async fn successful_stream_reaches_chat_client_translated() {
        const UPSTREAM_SSE_OK: &str = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"model":"claude-opus-4-6","usage":{"input_tokens":5}}}"#,
            "\n\n",
            "event: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
            "\n\n",
            "event: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
            "\n\n",
            "event: message_stop\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );

        let resp = translate_client_response(
            &ctx_for(ClientProtocol::ChatCompletions),
            sse_response(UPSTREAM_SSE_OK),
        )
        .await;

        let body = read_body(resp).await;
        assert!(body.contains("chat.completion.chunk"));
        assert!(body.contains(r#""content":"hello""#));
        assert!(body.contains(r#""finish_reason":"stop""#));
        assert!(body.trim_end().ends_with("data: [DONE]"));
    }

    #[test]
    fn test_transform_model_with_mapping() {
        let body = br#"{"model":"claude-opus-4-6","messages":[]}"#;
        let mappings = serde_json::json!({"claude-opus-4-6": "my-opus"});
        let result = transform_model(body, &mappings);
        let parsed: Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(parsed["model"], "my-opus");
    }

    #[test]
    fn test_transform_model_no_mapping() {
        let body = br#"{"model":"claude-haiku-4-5","messages":[]}"#;
        let mappings = serde_json::json!({"claude-opus-4-6": "my-opus"});
        let result = transform_model(body, &mappings);
        let parsed: Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(parsed["model"], "claude-haiku-4-5");
    }

    #[test]
    fn test_transform_model_empty_mappings() {
        let body = br#"{"model":"claude-opus-4-6","messages":[]}"#;
        let mappings = serde_json::json!({});
        let result = transform_model(body, &mappings);
        assert_eq!(result, body);
    }

    #[test]
    fn test_transform_model_no_model_field() {
        let body = br#"{"messages":[]}"#;
        let mappings = serde_json::json!({"claude-opus-4-6": "my-opus"});
        let result = transform_model(body, &mappings);
        let parsed: Value = serde_json::from_slice(&result).unwrap();
        assert!(parsed.get("model").is_none());
    }

    #[test]
    fn test_transform_model_invalid_json() {
        let body = b"not json";
        let mappings = serde_json::json!({"claude-opus-4-6": "my-opus"});
        let result = transform_model(body, &mappings);
        assert_eq!(result, body);
    }

    #[test]
    fn test_failover_status_code_matching() {
        let codes = vec![429u16, 500, 502, 503];
        assert!(codes.contains(&429));
        assert!(codes.contains(&500));
        assert!(!codes.contains(&400));
        assert!(!codes.contains(&200));
    }

    #[test]
    fn test_is_thinking_signature_error_with_signature() {
        let body =
            br#"{"error":{"type":"<nil>","message":"Invalid `signature` in `thinking` block"}}"#;
        assert!(is_thinking_signature_error(body));
    }

    #[test]
    fn test_is_thinking_signature_error_with_thinking() {
        let body = br#"{"error":{"message":"Invalid thinking block content"}}"#;
        assert!(is_thinking_signature_error(body));
    }

    #[test]
    fn test_is_thinking_signature_error_unrelated() {
        let body = br#"{"error":{"message":"Invalid model specified"}}"#;
        assert!(!is_thinking_signature_error(body));
    }

    #[test]
    fn test_strip_thinking_blocks_removes_thinking() {
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "claude-opus-4-6",
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "let me think", "signature": "abc123"},
                    {"type": "text", "text": "Hi there"}
                ]},
                {"role": "user", "content": "follow up"}
            ]
        }))
        .unwrap();

        let result = strip_thinking_blocks(&body).unwrap();
        let parsed: Value = serde_json::from_slice(&result).unwrap();
        let assistant_content = parsed["messages"][1]["content"].as_array().unwrap();
        assert_eq!(assistant_content.len(), 1);
        assert_eq!(assistant_content[0]["type"], "text");
    }

    #[test]
    fn test_strip_thinking_blocks_no_thinking_returns_none() {
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "claude-opus-4-6",
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Hi there"}
                ]}
            ]
        }))
        .unwrap();

        assert!(strip_thinking_blocks(&body).is_none());
    }

    #[test]
    fn test_strip_thinking_blocks_preserves_user_messages() {
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "claude-opus-4-6",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hello"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": ""},
                    {"type": "text", "text": "response"}
                ]}
            ]
        }))
        .unwrap();

        let result = strip_thinking_blocks(&body).unwrap();
        let parsed: Value = serde_json::from_slice(&result).unwrap();
        // User message content untouched
        let user_content = parsed["messages"][0]["content"].as_array().unwrap();
        assert_eq!(user_content.len(), 1);
        assert_eq!(user_content[0]["type"], "text");
    }

    #[test]
    fn test_merge_system_prompts_client_string_server_string() {
        let client = Some(&Value::String("You are a coding assistant".to_string()));
        let server = Some("Always respond in Vietnamese");
        let result = merge_system_prompts(client, server).unwrap();
        assert_eq!(
            result,
            "You are a coding assistant\n\nAlways respond in Vietnamese"
        );
    }

    #[test]
    fn test_merge_system_prompts_client_array_with_cache_control_server_string() {
        let client = Some(&serde_json::json!([
            {"type": "text", "text": "Block 1", "cache_control": {"type": "ephemeral"}}
        ]));
        let server = Some("Always respond in Vietnamese");
        let result = merge_system_prompts(client, server).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["text"], "Block 1\n\nAlways respond in Vietnamese");
        assert_eq!(arr[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_merge_system_prompts_client_array_without_cache_control_server_string() {
        let client = Some(&serde_json::json!([
            {"type": "text", "text": "Block 1"}
        ]));
        let server = Some("Always respond in Vietnamese");
        let result = merge_system_prompts(client, server).unwrap();
        assert_eq!(result, "Block 1\n\nAlways respond in Vietnamese");
    }

    #[test]
    fn test_merge_system_prompts_only_server() {
        let client = None;
        let server = Some("Always respond in Vietnamese");
        let result = merge_system_prompts(client, server).unwrap();
        assert_eq!(result, "Always respond in Vietnamese");
    }

    #[test]
    fn test_merge_system_prompts_only_client() {
        let client = Some(&Value::String("You are helpful".to_string()));
        let server = None;
        let result = merge_system_prompts(client, server).unwrap();
        assert_eq!(result, "You are helpful");
    }

    #[test]
    fn test_merge_system_prompts_neither() {
        let client = None;
        let server = None;
        let result = merge_system_prompts(client, server);
        assert!(result.is_none());
    }

    #[test]
    fn test_has_cache_control_true() {
        let system = serde_json::json!([
            {"type": "text", "text": "Block 1", "cache_control": {"type": "ephemeral"}}
        ]);
        assert!(has_cache_control(&system));
    }

    #[test]
    fn test_has_cache_control_false() {
        let system = serde_json::json!([
            {"type": "text", "text": "Block 1"}
        ]);
        assert!(!has_cache_control(&system));
    }

    #[test]
    fn test_extract_system_text() {
        let system = serde_json::json!([
            {"type": "text", "text": "Block 1"},
            {"type": "text", "text": "Block 2"}
        ]);
        let result = extract_system_text(&system).unwrap();
        assert_eq!(result, "Block 1\n\nBlock 2");
    }

    #[test]
    fn test_transform_request_body_with_model_mapping_and_system_merge() {
        let body = br#"{"model":"claude-opus-4-6","system":"You are helpful","messages":[]}"#;
        let mappings = serde_json::json!({"claude-opus-4-6": "my-opus"});
        let result = transform_request_body(
            body,
            &mappings,
            Some("Always respond in Vietnamese"),
            "/v1/messages",
            false,
        );
        let parsed: Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(parsed["model"], "my-opus");
        assert_eq!(
            parsed["system"],
            "You are helpful\n\nAlways respond in Vietnamese"
        );
    }

    #[test]
    fn test_transform_request_body_no_merge_on_non_messages_endpoint() {
        let body = br#"{"model":"claude-opus-4-6","system":"You are helpful","messages":[]}"#;
        let mappings = serde_json::json!({});
        let result = transform_request_body(
            body,
            &mappings,
            Some("Always respond in Vietnamese"),
            "/v1/other",
            false,
        );
        let parsed: Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(parsed["system"], "You are helpful");
    }

    #[test]
    fn test_transform_request_body_strips_empty_signature_thinking() {
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "claude-opus-4-7",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "stale", "signature": ""},
                    {"type": "text", "text": "Hello"}
                ]},
                {"role": "user", "content": "again"}
            ]
        }))
        .unwrap();
        let result =
            transform_request_body(&body, &serde_json::json!({}), None, "/v1/messages", false);
        let parsed: Value = serde_json::from_slice(&result).unwrap();
        let assistant = parsed["messages"][1]["content"].as_array().unwrap();
        assert_eq!(
            assistant.len(),
            1,
            "empty-signature thinking must be stripped"
        );
        assert_eq!(assistant[0]["type"], "text");
    }

    #[test]
    fn test_transform_request_body_strips_missing_signature_thinking() {
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "claude-opus-4-7",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "no sig at all"},
                    {"type": "text", "text": "Hi"}
                ]}
            ]
        }))
        .unwrap();
        let result =
            transform_request_body(&body, &serde_json::json!({}), None, "/v1/messages", false);
        let parsed: Value = serde_json::from_slice(&result).unwrap();
        let assistant = parsed["messages"][0]["content"].as_array().unwrap();
        assert_eq!(assistant.len(), 1);
        assert_eq!(assistant[0]["type"], "text");
    }

    #[test]
    fn test_transform_request_body_keeps_valid_signature_thinking() {
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "claude-opus-4-7",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "valid", "signature": "abc123"},
                    {"type": "text", "text": "Hi"}
                ]}
            ]
        }))
        .unwrap();
        let result =
            transform_request_body(&body, &serde_json::json!({}), None, "/v1/messages", false);
        let parsed: Value = serde_json::from_slice(&result).unwrap();
        let assistant = parsed["messages"][0]["content"].as_array().unwrap();
        assert_eq!(
            assistant.len(),
            2,
            "valid-signature thinking must be preserved (cache-friendly)"
        );
    }

    #[test]
    fn test_transform_request_body_does_not_touch_user_thinking_like_blocks() {
        // A user message echoing a thinking-shaped block (rare, but must not be mutated).
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "claude-opus-4-7",
            "messages": [
                {"role": "user", "content": [
                    {"type": "thinking", "thinking": "verbatim", "signature": ""}
                ]}
            ]
        }))
        .unwrap();
        let result =
            transform_request_body(&body, &serde_json::json!({}), None, "/v1/messages", false);
        let parsed: Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(
            parsed["messages"][0]["content"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn test_transform_request_body_strips_mixed_signatures() {
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "claude-opus-4-7",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "first", "signature": "good"},
                    {"type": "thinking", "thinking": "second", "signature": ""},
                    {"type": "text", "text": "done"}
                ]}
            ]
        }))
        .unwrap();
        let result =
            transform_request_body(&body, &serde_json::json!({}), None, "/v1/messages", false);
        let parsed: Value = serde_json::from_slice(&result).unwrap();
        let assistant = parsed["messages"][0]["content"].as_array().unwrap();
        assert_eq!(assistant.len(), 2);
        assert_eq!(assistant[0]["signature"], "good");
        assert_eq!(assistant[1]["type"], "text");
    }

    #[test]
    fn test_transform_request_body_strips_empty_text_blocks() {
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "claude-opus-4-7",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": ""},
                    {"type": "tool_use", "id": "tu_1", "name": "Read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "text", "text": "keep me"},
                    {"type": "text", "text": ""}
                ]}
            ]
        }))
        .unwrap();
        let result =
            transform_request_body(&body, &serde_json::json!({}), None, "/v1/messages", false);
        let parsed: Value = serde_json::from_slice(&result).unwrap();
        let assistant = parsed["messages"][0]["content"].as_array().unwrap();
        assert_eq!(assistant.len(), 1);
        assert_eq!(assistant[0]["type"], "tool_use");
        let user = parsed["messages"][1]["content"].as_array().unwrap();
        assert_eq!(user.len(), 1);
        assert_eq!(user[0]["text"], "keep me");
    }

    #[test]
    fn test_strip_thinking_blocks_full_strip_simulates_failover_path() {
        // Failover path reuses strip_thinking_blocks to remove all thinking blocks,
        // including those with valid signatures (signatures are account-bound).
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "claude-opus-4-7",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "from account A", "signature": "validForA"},
                    {"type": "text", "text": "response"}
                ]}
            ]
        }))
        .unwrap();
        let result = strip_thinking_blocks(&body).expect("should strip on failover");
        let parsed: Value = serde_json::from_slice(&result).unwrap();
        let assistant = parsed["messages"][0]["content"].as_array().unwrap();
        assert_eq!(assistant.len(), 1);
        assert_eq!(assistant[0]["type"], "text");
    }

    // --- client_wants_stream ---

    #[test]
    fn test_client_wants_stream_explicit_true() {
        let body = br#"{"model":"claude-opus-4-7","stream":true}"#;
        assert!(client_wants_stream(body));
    }

    #[test]
    fn test_client_wants_stream_explicit_false() {
        let body = br#"{"model":"claude-opus-4-7","stream":false}"#;
        assert!(!client_wants_stream(body));
    }

    #[test]
    fn test_client_wants_stream_field_absent() {
        // Anthropic defaults to non-streaming when `stream` is omitted.
        let body = br#"{"model":"claude-opus-4-7","messages":[]}"#;
        assert!(!client_wants_stream(body));
    }

    #[test]
    fn test_client_wants_stream_non_bool_value() {
        // A string "true" is not a bool; upstream would reject it. Treat as non-streaming.
        let body = br#"{"stream":"true"}"#;
        assert!(!client_wants_stream(body));
    }

    #[test]
    fn test_client_wants_stream_invalid_json() {
        assert!(!client_wants_stream(b"not json at all"));
        assert!(!client_wants_stream(b""));
    }

    // --- remaining_timeout_ms ---

    #[test]
    fn test_remaining_timeout_none_when_unconfigured() {
        assert_eq!(remaining_timeout_ms(None, 0), None);
        assert_eq!(remaining_timeout_ms(None, 5_000), None);
    }

    #[test]
    fn test_remaining_timeout_subtracts_elapsed() {
        assert_eq!(remaining_timeout_ms(Some(30_000), 0), Some(30_000));
        assert_eq!(remaining_timeout_ms(Some(30_000), 10_000), Some(20_000));
    }

    #[test]
    fn test_remaining_timeout_zero_when_budget_spent() {
        // Budget already gone — Some(0) means "fail over now", distinct from None.
        assert_eq!(remaining_timeout_ms(Some(30_000), 30_000), Some(0));
        assert_eq!(remaining_timeout_ms(Some(30_000), 45_000), Some(0));
    }

    #[test]
    fn test_remaining_timeout_treats_non_positive_config_as_disabled() {
        // Guards against a 0 or negative value written straight into the DB, which
        // would otherwise time out every request instantly.
        assert_eq!(remaining_timeout_ms(Some(0), 0), None);
        assert_eq!(remaining_timeout_ms(Some(-1), 0), None);
    }

    // --- extract_usage_tokens ---

    #[test]
    fn test_extract_usage_tokens_anthropic() {
        let body = br#"{"usage":{"input_tokens":100,"output_tokens":50,
            "cache_creation_input_tokens":20,"cache_read_input_tokens":10}}"#;
        let u = extract_usage_tokens(body).expect("anthropic usage");
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.cache_creation_tokens, Some(20));
        assert_eq!(u.cache_read_tokens, Some(10));
    }

    #[test]
    fn test_extract_usage_tokens_anthropic_without_cache_fields() {
        let body = br#"{"usage":{"input_tokens":7,"output_tokens":3}}"#;
        let u = extract_usage_tokens(body).expect("anthropic usage");
        assert_eq!((u.input_tokens, u.output_tokens), (7, 3));
        assert_eq!(u.cache_creation_tokens, None);
        assert_eq!(u.cache_read_tokens, None);
    }

    #[test]
    fn test_extract_usage_tokens_ignores_openai_field_names() {
        // Upstreams are always Anthropic now: an OpenAI-shaped usage object
        // carries no Anthropic field names, so it must not be billed as if it
        // did rather than silently reading zeros.
        let body = br#"{"usage":{"prompt_tokens":200,"completion_tokens":80,
            "prompt_tokens_details":{"cached_tokens":30}}}"#;
        assert!(extract_usage_tokens(body).is_none());
    }

    // --- effective_non_stream_timeout_ms (per-entity over global default) ---

    #[test]
    fn test_effective_timeout_prefers_per_entity_value() {
        // An explicit per-entity setting always wins over the global default.
        assert_eq!(
            effective_non_stream_timeout_ms(Some(15_000), Some(600_000)),
            Some(15_000)
        );
        // Including when it is longer than the default.
        assert_eq!(
            effective_non_stream_timeout_ms(Some(900_000), Some(600_000)),
            Some(900_000)
        );
    }

    #[test]
    fn test_effective_timeout_falls_back_to_global_default() {
        // Nothing configured on the entity — the global default bounds it, so an
        // unconfigured upstream can no longer hold a request for the full 8h.
        assert_eq!(
            effective_non_stream_timeout_ms(None, Some(600_000)),
            Some(600_000)
        );
    }

    #[test]
    fn test_effective_timeout_unbounded_only_when_both_absent() {
        // Clearing the global default is the deliberate opt-out back to unbounded.
        assert_eq!(effective_non_stream_timeout_ms(None, None), None);
    }

    #[test]
    fn test_effective_timeout_ignores_non_positive_values() {
        // A 0 or negative in either place would abort every request instantly; treat
        // it as unset and let the other level decide.
        assert_eq!(
            effective_non_stream_timeout_ms(Some(0), Some(600_000)),
            Some(600_000)
        );
        assert_eq!(
            effective_non_stream_timeout_ms(Some(-1), Some(600_000)),
            Some(600_000)
        );
        assert_eq!(effective_non_stream_timeout_ms(Some(0), Some(0)), None);
        assert_eq!(effective_non_stream_timeout_ms(None, Some(-5)), None);
    }

    // --- timeout budget across the send/read split ---

    #[test]
    fn test_non_stream_budget_carries_over_from_send_to_read() {
        // The send leg gets the full budget; the read leg gets what is left, so a
        // server cannot spend the timeout twice by stalling in both phases.
        let configured = Some(30_000);
        assert_eq!(remaining_timeout_ms(configured, 0), Some(30_000));
        // 12s spent waiting for headers leaves 18s for the body.
        assert_eq!(remaining_timeout_ms(configured, 12_000), Some(18_000));
        // Headers arrived only just inside the budget — the read must not get a fresh one.
        assert_eq!(remaining_timeout_ms(configured, 29_999), Some(1));
    }

    #[test]
    fn test_non_stream_budget_absent_leaves_read_unbounded() {
        // With no timeout configured, neither leg is bounded — preserving the previous
        // behaviour for anyone who has not set the column.
        assert_eq!(remaining_timeout_ms(None, 12_000), None);
    }

    #[test]
    fn test_upstream_ignoring_stream_request_is_still_non_streaming() {
        // A client can ask for `stream: true` and get a non-SSE body back. The send-side
        // timeout is skipped for such a request (the response kind is unknown until
        // headers land), but the read side must still bound it — so the read budget is
        // derived from the same column rather than from what the client asked for.
        let body = br#"{"model":"claude-opus-4-7","stream":true}"#;
        assert!(client_wants_stream(body));
        // The read leg consults the server column directly, independent of `wants_stream`.
        assert_eq!(remaining_timeout_ms(Some(45_000), 5_000), Some(40_000));
    }

    #[test]
    fn test_extract_usage_tokens_missing_or_partial() {
        // No usage object at all.
        assert!(extract_usage_tokens(br#"{"id":"msg_1"}"#).is_none());
        // Usage present but output_tokens missing — cannot bill a half-known request.
        assert!(extract_usage_tokens(br#"{"usage":{"input_tokens":5}}"#).is_none());
        // Not JSON.
        assert!(extract_usage_tokens(b"<html>502</html>").is_none());
    }
}
