use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cache;
use crate::models::Settings;
use crate::routes::AppState;

fn validate_timezone(timezone: &str) -> bool {
    timezone.parse::<chrono_tz::Tz>().is_ok()
}

fn default_settings() -> Settings {
    Settings {
        telegram_bot_token: None,
        telegram_chat_ids: vec![],
        alert_status_codes: vec![500, 502, 503],
        alert_cooldown_mins: 5,
        blocked_paths: vec![],
        timezone: cache::DEFAULT_TIMEZONE.to_string(),
        ct_always_estimate: false,
        ct_anthropic_base_url: None,
        ct_anthropic_api_key: None,
        user_endpoints_enabled: true,
        openai_compat_base_url: None,
        public_base_url: None,
        api_key_prefix: None,
        proxy_log_retention_days: 3,
        log_request_body: false,
        default_non_stream_timeout_ms: Some(600_000),
    }
}

async fn get_settings(
    State(state): State<AppState>,
) -> Result<Json<Settings>, (StatusCode, Json<Value>)> {
    let row = sqlx::query_as::<_, Settings>(
        "SELECT telegram_bot_token, telegram_chat_ids, alert_status_codes, alert_cooldown_mins, blocked_paths, \
         timezone, ct_always_estimate, ct_anthropic_base_url, ct_anthropic_api_key, user_endpoints_enabled, openai_compat_base_url, public_base_url, api_key_prefix, proxy_log_retention_days, log_request_body, default_non_stream_timeout_ms \
         FROM settings WHERE id = 1",
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    Ok(Json(row.unwrap_or_else(default_settings)))
}

#[derive(Debug, Deserialize)]
pub struct UpdateSettings {
    #[serde(default, deserialize_with = "crate::serde_utils::double_option")]
    pub telegram_bot_token: Option<Option<String>>,
    pub telegram_chat_ids: Option<Vec<String>>,
    pub alert_status_codes: Option<Vec<i32>>,
    pub alert_cooldown_mins: Option<i32>,
    pub blocked_paths: Option<Vec<String>>,
    pub timezone: Option<String>,
    pub ct_always_estimate: Option<bool>,
    #[serde(default, deserialize_with = "crate::serde_utils::double_option")]
    pub ct_anthropic_base_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::serde_utils::double_option")]
    pub ct_anthropic_api_key: Option<Option<String>>,
    pub user_endpoints_enabled: Option<bool>,
    #[serde(default, deserialize_with = "crate::serde_utils::double_option")]
    pub openai_compat_base_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::serde_utils::double_option")]
    pub public_base_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::serde_utils::double_option")]
    pub api_key_prefix: Option<Option<String>>,
    pub proxy_log_retention_days: Option<i32>,
    pub log_request_body: Option<bool>,
    /// Nullable: an explicit JSON `null` clears the default (back to unbounded), which
    /// is why this needs double_option rather than a plain Option.
    #[serde(default, deserialize_with = "crate::serde_utils::double_option")]
    pub default_non_stream_timeout_ms: Option<Option<i32>>,
}

async fn put_settings(
    State(state): State<AppState>,
    Json(input): Json<UpdateSettings>,
) -> Result<Json<Settings>, (StatusCode, Json<Value>)> {
    // Fetch current (or defaults) to merge with partial update
    let current = sqlx::query_as::<_, Settings>(
        "SELECT telegram_bot_token, telegram_chat_ids, alert_status_codes, alert_cooldown_mins, blocked_paths, \
         timezone, ct_always_estimate, ct_anthropic_base_url, ct_anthropic_api_key, user_endpoints_enabled, openai_compat_base_url, public_base_url, api_key_prefix, proxy_log_retention_days, log_request_body, default_non_stream_timeout_ms \
         FROM settings WHERE id = 1",
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?
    .unwrap_or_else(default_settings);

    let new_token = match input.telegram_bot_token {
        Some(v) => v,
        None => current.telegram_bot_token,
    };
    let new_chat_ids = input.telegram_chat_ids.unwrap_or(current.telegram_chat_ids);
    let new_status_codes = input
        .alert_status_codes
        .unwrap_or(current.alert_status_codes);
    let new_cooldown = input
        .alert_cooldown_mins
        .unwrap_or(current.alert_cooldown_mins);
    let blocked_paths_changed = input.blocked_paths.is_some();
    let new_blocked_paths = input.blocked_paths.unwrap_or(current.blocked_paths);
    let timezone_changed = input.timezone.is_some();
    let new_timezone = input.timezone.unwrap_or(current.timezone);
    if !validate_timezone(&new_timezone) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "timezone must be a valid IANA timezone"})),
        ));
    }
    let new_ct_always_estimate = input
        .ct_always_estimate
        .unwrap_or(current.ct_always_estimate);
    let new_ct_anthropic_base_url = match input.ct_anthropic_base_url {
        Some(v) => v,
        None => current.ct_anthropic_base_url,
    };
    let new_ct_anthropic_api_key = match input.ct_anthropic_api_key {
        Some(v) => v,
        None => current.ct_anthropic_api_key,
    };
    let user_endpoints_enabled_changed = input.user_endpoints_enabled.is_some();
    let new_user_endpoints_enabled = input
        .user_endpoints_enabled
        .unwrap_or(current.user_endpoints_enabled);
    let new_openai_compat_base_url = match input.openai_compat_base_url {
        Some(v) => v.and_then(|s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }),
        None => current.openai_compat_base_url,
    };
    let new_public_base_url = match input.public_base_url {
        Some(v) => v.and_then(|s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }),
        None => current.public_base_url,
    };
    let new_api_key_prefix = match input.api_key_prefix {
        Some(v) => v.and_then(|s| if s.is_empty() { None } else { Some(s) }),
        None => current.api_key_prefix,
    };
    let new_proxy_log_retention_days = input
        .proxy_log_retention_days
        .unwrap_or(current.proxy_log_retention_days);
    if new_proxy_log_retention_days < 1 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "proxy_log_retention_days must be >= 1"})),
        ));
    }
    let new_log_request_body = input.log_request_body.unwrap_or(current.log_request_body);
    let log_request_body_changed = input.log_request_body.is_some();
    let new_default_non_stream_timeout_ms = match input.default_non_stream_timeout_ms {
        Some(v) => v,
        None => current.default_non_stream_timeout_ms,
    };
    if let Some(ms) = new_default_non_stream_timeout_ms
        && ms < 1
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "default_non_stream_timeout_ms must be >= 1"})),
        ));
    }
    let default_non_stream_timeout_ms_changed = input.default_non_stream_timeout_ms.is_some();

    let updated = sqlx::query_as::<_, Settings>(
        "INSERT INTO settings (id, telegram_bot_token, telegram_chat_ids, alert_status_codes, alert_cooldown_mins, blocked_paths, \
         timezone, ct_always_estimate, ct_anthropic_base_url, ct_anthropic_api_key, user_endpoints_enabled, openai_compat_base_url, public_base_url, api_key_prefix, proxy_log_retention_days, log_request_body, default_non_stream_timeout_ms) \
         VALUES (1, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) \
         ON CONFLICT (id) DO UPDATE SET \
           telegram_bot_token = EXCLUDED.telegram_bot_token, \
           telegram_chat_ids = EXCLUDED.telegram_chat_ids, \
           alert_status_codes = EXCLUDED.alert_status_codes, \
           alert_cooldown_mins = EXCLUDED.alert_cooldown_mins, \
           blocked_paths = EXCLUDED.blocked_paths, \
           timezone = EXCLUDED.timezone, \
           ct_always_estimate = EXCLUDED.ct_always_estimate, \
           ct_anthropic_base_url = EXCLUDED.ct_anthropic_base_url, \
           ct_anthropic_api_key = EXCLUDED.ct_anthropic_api_key, \
           user_endpoints_enabled = EXCLUDED.user_endpoints_enabled, \
           openai_compat_base_url = EXCLUDED.openai_compat_base_url, \
           public_base_url = EXCLUDED.public_base_url, \
           api_key_prefix = EXCLUDED.api_key_prefix, \
           proxy_log_retention_days = EXCLUDED.proxy_log_retention_days, \
           log_request_body = EXCLUDED.log_request_body, \
           default_non_stream_timeout_ms = EXCLUDED.default_non_stream_timeout_ms \
         RETURNING telegram_bot_token, telegram_chat_ids, alert_status_codes, alert_cooldown_mins, blocked_paths, \
         timezone, ct_always_estimate, ct_anthropic_base_url, ct_anthropic_api_key, user_endpoints_enabled, openai_compat_base_url, public_base_url, api_key_prefix, proxy_log_retention_days, log_request_body, default_non_stream_timeout_ms",
    )
    .bind(&new_token)
    .bind(&new_chat_ids)
    .bind(&new_status_codes)
    .bind(new_cooldown)
    .bind(&new_blocked_paths)
    .bind(&new_timezone)
    .bind(new_ct_always_estimate)
    .bind(&new_ct_anthropic_base_url)
    .bind(&new_ct_anthropic_api_key)
    .bind(new_user_endpoints_enabled)
    .bind(&new_openai_compat_base_url)
    .bind(&new_public_base_url)
    .bind(&new_api_key_prefix)
    .bind(new_proxy_log_retention_days)
    .bind(new_log_request_body)
    .bind(new_default_non_stream_timeout_ms)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    if blocked_paths_changed {
        cache::invalidate_blocked_paths(&state.redis).await;
    }
    if timezone_changed {
        cache::invalidate_timezone(&state.redis).await;
    }
    if user_endpoints_enabled_changed {
        cache::invalidate_user_endpoints_enabled(&state.redis).await;
    }
    if log_request_body_changed {
        cache::invalidate_log_request_body(&state.redis).await;
    }
    if default_non_stream_timeout_ms_changed {
        cache::invalidate_default_non_stream_timeout_ms(&state.redis).await;
    }

    Ok(Json(updated))
}

async fn post_test_alert(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let settings = sqlx::query_as::<_, Settings>(
        "SELECT telegram_bot_token, telegram_chat_ids, alert_status_codes, alert_cooldown_mins, blocked_paths, \
         timezone, ct_always_estimate, ct_anthropic_base_url, ct_anthropic_api_key, user_endpoints_enabled, openai_compat_base_url, public_base_url, api_key_prefix, proxy_log_retention_days, log_request_body, default_non_stream_timeout_ms \
         FROM settings WHERE id = 1",
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?
    .unwrap_or_else(default_settings);

    let token = settings.telegram_bot_token.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Bot token not configured"})),
        )
    })?;

    if settings.telegram_chat_ids.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "No chat IDs configured"})),
        ));
    }

    let text = "🔔 *Test Alert*\nViber Router Telegram alerts are configured correctly\\.";
    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);

    for chat_id in &settings.telegram_chat_ids {
        let resp = state
            .http_client
            .post(&url)
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "text": text,
                "parse_mode": "MarkdownV2"
            }))
            .send()
            .await
            .map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
            })?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": err_text})),
            ));
        }
    }

    Ok(Json(serde_json::json!({"success": true})))
}

#[derive(Debug, Serialize)]
struct TelegramChat {
    chat_id: String,
    first_name: Option<String>,
    username: Option<String>,
}

async fn get_telegram_chats(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let settings = sqlx::query_as::<_, Settings>(
        "SELECT telegram_bot_token, telegram_chat_ids, alert_status_codes, alert_cooldown_mins, blocked_paths, \
         timezone, ct_always_estimate, ct_anthropic_base_url, ct_anthropic_api_key, user_endpoints_enabled, openai_compat_base_url, public_base_url, api_key_prefix, proxy_log_retention_days, log_request_body, default_non_stream_timeout_ms \
         FROM settings WHERE id = 1",
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?
    .unwrap_or_else(default_settings);

    let token = settings.telegram_bot_token.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Bot token not configured"})),
        )
    })?;

    let url = format!("https://api.telegram.org/bot{}/getUpdates?limit=100", token);

    let resp = state.http_client.get(&url).send().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    if !resp.status().is_success() {
        let err_text = resp.text().await.unwrap_or_default();
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": err_text})),
        ));
    }

    let body: Value = resp.json().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    let mut seen_ids = std::collections::HashSet::new();
    let mut chats: Vec<TelegramChat> = Vec::new();

    if let Some(results) = body.get("result").and_then(|r| r.as_array()) {
        for update in results {
            // Check message.chat or my_chat_member.chat etc.
            let chat = update
                .get("message")
                .and_then(|m| m.get("chat"))
                .or_else(|| update.get("channel_post").and_then(|m| m.get("chat")));

            if let Some(chat) = chat {
                let chat_id = match chat.get("id").and_then(|id| id.as_i64()) {
                    Some(id) => id.to_string(),
                    None => continue,
                };
                if seen_ids.insert(chat_id.clone()) {
                    chats.push(TelegramChat {
                        chat_id,
                        first_name: chat
                            .get("first_name")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        username: chat
                            .get("username")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                    });
                }
            }
        }
    }

    Ok(Json(serde_json::json!({"chats": chats})))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(get_settings).put(put_settings))
        .route("/test", post(post_test_alert))
        .route("/telegram-chats", get(get_telegram_chats))
}
