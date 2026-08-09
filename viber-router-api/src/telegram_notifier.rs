use deadpool_redis::Pool;
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::Settings;

fn default_settings() -> Settings {
    Settings {
        telegram_bot_token: None,
        telegram_chat_ids: vec![],
        alert_status_codes: vec![500, 502, 503],
        alert_cooldown_mins: 5,
        blocked_paths: vec![],
        timezone: crate::cache::DEFAULT_TIMEZONE.to_string(),
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

/// Escape special characters for Telegram MarkdownV2.
fn escape_md(s: &str) -> String {
    let special = [
        '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!',
    ];
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if special.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub struct AlertContext {
    pub db: PgPool,
    pub redis: Pool,
    pub http_client: reqwest::Client,
    pub server_id: Uuid,
    pub server_name: String,
    pub group_name: String,
    pub status_code: u16,
    pub latency_ms: i32,
}

pub async fn maybe_alert(ctx: AlertContext) {
    let settings = match load_settings(&ctx.db).await {
        Some(s) => s,
        None => return,
    };

    // Skip if no token configured
    let token = match settings.telegram_bot_token {
        Some(t) => t,
        None => return,
    };

    // Skip if status code not in alert list
    if !settings
        .alert_status_codes
        .contains(&(ctx.status_code as i32))
    {
        return;
    }

    // Check Redis cooldown key
    let cooldown_key = format!("tg:cooldown:{}:{}", ctx.server_id, ctx.status_code);
    let ttl_secs = settings.alert_cooldown_mins * 60;

    let should_send = match check_and_set_cooldown(&ctx.redis, &cooldown_key, ttl_secs).await {
        Ok(acquired) => acquired,
        Err(e) => {
            tracing::warn!(
                "telegram_notifier: Redis cooldown check failed (fail open): {}",
                e
            );
            true // fail open
        }
    };

    if !should_send {
        return;
    }

    // Build message
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
    let text = format!(
        "🚨 *Upstream Error*\n*Server:* {}\n*Group:* {}\n*Status:* {}\n*Latency:* {}ms\n*Time:* {}",
        escape_md(&ctx.server_name),
        escape_md(&ctx.group_name),
        escape_md(&ctx.status_code.to_string()),
        escape_md(&ctx.latency_ms.to_string()),
        escape_md(&now.to_string()),
    );

    send_to_chats(&ctx.http_client, &token, &settings.telegram_chat_ids, &text).await;
}

/// Returns true if the cooldown key was newly set (i.e., alert should be sent).
/// Returns false if the key already existed (cooldown active).
async fn check_and_set_cooldown(
    redis: &Pool,
    key: &str,
    ttl_secs: i32,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = redis.get().await?;
    let result: Option<String> = deadpool_redis::redis::cmd("SET")
        .arg(key)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(ttl_secs)
        .query_async(&mut conn)
        .await?;
    Ok(result.is_some())
}

/// Context for circuit breaker trip alert.
pub struct CircuitBreakerAlertContext {
    pub db: PgPool,
    pub redis: Pool,
    pub http_client: reqwest::Client,
    pub server_name: String,
    pub group_name: String,
    pub group_id: Uuid,
    pub server_id: Uuid,
    pub model: Option<String>,
    pub error_count: i32,
    pub window_seconds: i32,
    pub cooldown_seconds: i32,
}

/// Send alert when circuit breaker trips.
pub async fn send_circuit_breaker_alert(ctx: CircuitBreakerAlertContext) {
    let settings = match load_settings(&ctx.db).await {
        Some(s) => s,
        None => return,
    };
    let token = match settings.telegram_bot_token {
        Some(t) => t,
        None => return,
    };

    let model_slot = ctx.model.as_deref().unwrap_or("_any");
    let cooldown_key = format!("tg:cooldown:cb:{}:{}:{}", ctx.group_id, ctx.server_id, model_slot);
    let ttl_secs = settings.alert_cooldown_mins * 60;

    let should_send = match check_and_set_cooldown(&ctx.redis, &cooldown_key, ttl_secs).await {
        Ok(acquired) => acquired,
        Err(e) => {
            tracing::warn!(
                "telegram_notifier: CB cooldown check failed (fail open): {}",
                e
            );
            true
        }
    };
    if !should_send {
        return;
    }

    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
    let model_line = match &ctx.model {
        Some(m) => format!("\n*Model:* {}", escape_md(m)),
        None => String::new(),
    };
    let text = format!(
        "⚡ *Circuit Breaker Tripped*\n*Server:* {}\n*Group:* {}{}\n*Errors:* {} in {}s\n*Cooldown:* {}s\n*Time:* {}",
        escape_md(&ctx.server_name),
        escape_md(&ctx.group_name),
        model_line,
        escape_md(&ctx.error_count.to_string()),
        escape_md(&ctx.window_seconds.to_string()),
        escape_md(&ctx.cooldown_seconds.to_string()),
        escape_md(&now.to_string()),
    );

    send_to_chats(&ctx.http_client, &token, &settings.telegram_chat_ids, &text).await;
}

/// Context for circuit breaker re-enable alert.
pub struct CircuitReEnableAlertContext {
    pub db: PgPool,
    pub http_client: reqwest::Client,
    pub server_name: String,
    pub group_name: String,
    pub model: Option<String>,
}

/// Send alert when circuit breaker re-enables a server.
pub async fn send_circuit_re_enable_alert(ctx: CircuitReEnableAlertContext) {
    let settings = match load_settings(&ctx.db).await {
        Some(s) => s,
        None => return,
    };
    let token = match settings.telegram_bot_token {
        Some(t) => t,
        None => return,
    };

    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
    let model_line = match &ctx.model {
        Some(m) => format!("\n*Model:* {}", escape_md(m)),
        None => String::new(),
    };
    let text = format!(
        "✅ *Circuit Breaker Re\\-enabled*\n*Server:* {}\n*Group:* {}{}\n*Time:* {}",
        escape_md(&ctx.server_name),
        escape_md(&ctx.group_name),
        model_line,
        escape_md(&now.to_string()),
    );

    send_to_chats(&ctx.http_client, &token, &settings.telegram_chat_ids, &text).await;
}

async fn load_settings(db: &PgPool) -> Option<Settings> {
    // Must list every Settings column: sqlx's FromRow errors on a missing field unless it
    // is marked #[sqlx(default)], and most of these are not. The previous narrow list
    // failed to decode on `user_endpoints_enabled`, so every alert silently took the Err
    // branch below and logged a warning instead of sending.
    match sqlx::query_as::<_, Settings>(
        "SELECT telegram_bot_token, telegram_chat_ids, alert_status_codes, alert_cooldown_mins, blocked_paths, \
         timezone, ct_always_estimate, ct_anthropic_base_url, ct_anthropic_api_key, user_endpoints_enabled, \
         openai_compat_base_url, public_base_url, api_key_prefix, proxy_log_retention_days, log_request_body, \
         default_non_stream_timeout_ms \
         FROM settings WHERE id = 1",
    )
    .fetch_optional(db)
    .await
    {
        Ok(row) => Some(row.unwrap_or_else(default_settings)),
        Err(e) => {
            tracing::warn!("telegram_notifier: failed to load settings: {}", e);
            None
        }
    }
}

async fn send_to_chats(
    http_client: &reqwest::Client,
    token: &str,
    chat_ids: &[String],
    text: &str,
) {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);

    let futures: Vec<_> = chat_ids
        .iter()
        .map(|chat_id| {
            let client = http_client.clone();
            let url = url.clone();
            let text = text.to_string();
            let chat_id = chat_id.clone();
            async move {
                let result = client
                    .post(&url)
                    .json(&serde_json::json!({
                        "chat_id": chat_id,
                        "text": text,
                        "parse_mode": "MarkdownV2"
                    }))
                    .send()
                    .await;
                (chat_id, result)
            }
        })
        .collect();

    let results = futures_util::future::join_all(futures).await;
    for (chat_id, result) in results {
        match result {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!(
                    "telegram_notifier: delivery failed to chat_id={} status={} body={}",
                    chat_id,
                    status,
                    body
                );
            }
            Err(e) => {
                tracing::warn!(
                    "telegram_notifier: delivery error to chat_id={}: {}",
                    chat_id,
                    e
                );
            }
        }
    }
}
