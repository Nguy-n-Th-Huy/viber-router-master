use serde::{Deserialize, Serialize};
use sqlx::FromRow;

fn default_timezone() -> String {
    "Asia/Ho_Chi_Minh".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Settings {
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_ids: Vec<String>,
    pub alert_status_codes: Vec<i32>,
    pub alert_cooldown_mins: i32,
    #[serde(default)]
    pub blocked_paths: Vec<String>,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub ct_always_estimate: bool,
    pub ct_anthropic_base_url: Option<String>,
    pub ct_anthropic_api_key: Option<String>,
    #[serde(default = "default_user_endpoints_enabled")]
    pub user_endpoints_enabled: bool,
    pub openai_compat_base_url: Option<String>,
    pub public_base_url: Option<String>,
    pub api_key_prefix: Option<String>,
    #[serde(default = "default_proxy_log_retention_days")]
    pub proxy_log_retention_days: i32,
    #[serde(default)]
    pub log_request_body: bool,
    /// Fallback cap on non-streaming upstream calls when the per-server, per-bonus, or
    /// per-endpoint column is NULL. `None` restores unbounded behaviour.
    ///
    /// `sqlx(default)` matters here: telegram_notifier::load_settings selects only a
    /// subset of columns, so this must decode as absent rather than failing the query.
    #[sqlx(default)]
    #[serde(default = "default_non_stream_timeout_ms")]
    pub default_non_stream_timeout_ms: Option<i32>,
}

fn default_user_endpoints_enabled() -> bool {
    true
}

fn default_proxy_log_retention_days() -> i32 {
    3
}

fn default_non_stream_timeout_ms() -> Option<i32> {
    Some(600_000)
}
