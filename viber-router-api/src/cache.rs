use deadpool_redis::Pool;
use deadpool_redis::redis::AsyncCommands;
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::{GroupConfig, UserEndpoint};

const KEY_PREFIX: &str = "group:config:";
const CONFIG_TTL_SECS: i64 = 60;
const USER_ENDPOINTS_PREFIX: &str = "user_eps:";
const USER_ENDPOINTS_TTL_SECS: i64 = 300;

pub async fn get_group_config(redis: &Pool, api_key: &str) -> Option<GroupConfig> {
    let mut conn = redis.get().await.ok()?;
    let data: Option<String> = conn.get(format!("{KEY_PREFIX}{api_key}")).await.ok()?;
    data.and_then(|d| serde_json::from_str(&d).ok())
}

pub async fn set_group_config(redis: &Pool, api_key: &str, config: &GroupConfig) {
    if let Ok(mut conn) = redis.get().await
        && let Ok(data) = serde_json::to_string(config)
    {
        let _: Result<(), _> = conn
            .set_ex(
                format!("{KEY_PREFIX}{api_key}"),
                data,
                CONFIG_TTL_SECS as u64,
            )
            .await;
    }
}

pub async fn invalidate_group_config(redis: &Pool, api_key: &str) {
    if let Ok(mut conn) = redis.get().await {
        let _: Result<(), _> = conn.del(format!("{KEY_PREFIX}{api_key}")).await;
    }
}

pub async fn get_user_endpoints(redis: &Pool, group_key_id: Uuid) -> Option<Vec<UserEndpoint>> {
    let mut conn = redis.get().await.ok()?;
    let data: Option<String> = conn
        .get(format!("{USER_ENDPOINTS_PREFIX}{group_key_id}"))
        .await
        .ok()?;
    data.and_then(|d| serde_json::from_str(&d).ok())
}

pub async fn set_user_endpoints(redis: &Pool, group_key_id: Uuid, endpoints: &[UserEndpoint]) {
    if let Ok(mut conn) = redis.get().await
        && let Ok(data) = serde_json::to_string(endpoints)
    {
        let _: Result<(), _> = conn
            .set_ex(
                format!("{USER_ENDPOINTS_PREFIX}{group_key_id}"),
                data,
                USER_ENDPOINTS_TTL_SECS as u64,
            )
            .await;
    }
}

pub async fn invalidate_user_endpoints(redis: &Pool, group_key_id: Uuid) {
    if let Ok(mut conn) = redis.get().await {
        let _: Result<(), _> = conn
            .del(format!("{USER_ENDPOINTS_PREFIX}{group_key_id}"))
            .await;
    }
}

/// Invalidate cache for a group's master key AND all its sub-keys.
pub async fn invalidate_group_all_keys(redis: &Pool, db: &PgPool, group_id: Uuid) {
    // Get master key
    let master: Option<(String,)> = sqlx::query_as("SELECT api_key FROM groups WHERE id = $1")
        .bind(group_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();

    if let Some((master_key,)) = master {
        invalidate_group_config(redis, &master_key).await;
    }

    // Get all sub-keys
    let sub_keys: Vec<(String,)> =
        sqlx::query_as("SELECT api_key FROM group_keys WHERE group_id = $1")
            .bind(group_id)
            .fetch_all(db)
            .await
            .unwrap_or_default();

    for (api_key,) in sub_keys {
        invalidate_group_config(redis, &api_key).await;
    }
}

pub async fn invalidate_groups_by_server(redis: &Pool, db: &PgPool, server_id: Uuid) {
    // Get all group IDs affected by this server
    let group_ids: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT g.id FROM groups g \
         JOIN group_servers gs ON g.id = gs.group_id \
         WHERE gs.server_id = $1 \
         UNION \
         SELECT g.id FROM groups g \
         WHERE g.count_tokens_server_id = $1",
    )
    .bind(server_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();

    for (gid,) in group_ids {
        invalidate_group_all_keys(redis, db, gid).await;
    }
}

const BLOCKED_PATHS_KEY: &str = "settings:blocked_paths";

const TIMEZONE_KEY: &str = "settings:timezone";
pub const DEFAULT_TIMEZONE: &str = "Asia/Ho_Chi_Minh";

/// Returns Ok(Some(timezone)) on cache hit, Ok(None) on cache miss, Err(()) on Redis failure.
pub async fn get_timezone(redis: &Pool) -> Result<Option<String>, ()> {
    let mut conn = redis.get().await.map_err(|_| ())?;
    let data: Option<String> = conn.get(TIMEZONE_KEY).await.map_err(|_| ())?;
    Ok(data)
}

pub async fn set_timezone(redis: &Pool, timezone: &str) {
    if let Ok(mut conn) = redis.get().await {
        let _: Result<(), _> = conn.set(TIMEZONE_KEY, timezone).await;
    }
}

pub async fn invalidate_timezone(redis: &Pool) {
    if let Ok(mut conn) = redis.get().await {
        let _: Result<(), _> = conn.del(TIMEZONE_KEY).await;
    }
}


/// Returns Ok(Some(paths)) on cache hit, Ok(None) on cache miss, Err(()) on Redis failure.
pub async fn get_blocked_paths(redis: &Pool) -> Result<Option<Vec<String>>, ()> {
    let mut conn = redis.get().await.map_err(|_| ())?;
    let data: Option<String> = conn.get(BLOCKED_PATHS_KEY).await.map_err(|_| ())?;
    Ok(data.and_then(|d| serde_json::from_str(&d).ok()))
}

pub async fn set_blocked_paths(redis: &Pool, paths: &[String]) {
    if let Ok(mut conn) = redis.get().await
        && let Ok(data) = serde_json::to_string(paths)
    {
        let _: Result<(), _> = conn.set(BLOCKED_PATHS_KEY, data).await;
    }
}

pub async fn invalidate_blocked_paths(redis: &Pool) {
    if let Ok(mut conn) = redis.get().await {
        let _: Result<(), _> = conn.del(BLOCKED_PATHS_KEY).await;
    }
}

const USER_ENDPOINTS_ENABLED_KEY: &str = "settings:user_endpoints_enabled";

/// Returns Ok(Some(enabled)) on cache hit, Ok(None) on miss, Err(()) on Redis failure.
pub async fn get_user_endpoints_enabled(redis: &Pool) -> Result<Option<bool>, ()> {
    let mut conn = redis.get().await.map_err(|_| ())?;
    let data: Option<String> = conn.get(USER_ENDPOINTS_ENABLED_KEY).await.map_err(|_| ())?;
    Ok(data.map(|d| d == "1"))
}

pub async fn set_user_endpoints_enabled(redis: &Pool, enabled: bool) {
    if let Ok(mut conn) = redis.get().await {
        let val = if enabled { "1" } else { "0" };
        let _: Result<(), _> = conn.set(USER_ENDPOINTS_ENABLED_KEY, val).await;
    }
}

pub async fn invalidate_user_endpoints_enabled(redis: &Pool) {
    if let Ok(mut conn) = redis.get().await {
        let _: Result<(), _> = conn.del(USER_ENDPOINTS_ENABLED_KEY).await;
    }
}

const LOG_REQUEST_BODY_KEY: &str = "settings:log_request_body";

/// Returns Ok(Some(enabled)) on cache hit, Ok(None) on miss, Err(()) on Redis failure.
pub async fn get_log_request_body(redis: &Pool) -> Result<Option<bool>, ()> {
    let mut conn = redis.get().await.map_err(|_| ())?;
    let data: Option<String> = conn.get(LOG_REQUEST_BODY_KEY).await.map_err(|_| ())?;
    Ok(data.map(|d| d == "1"))
}

pub async fn set_log_request_body(redis: &Pool, enabled: bool) {
    if let Ok(mut conn) = redis.get().await {
        let val = if enabled { "1" } else { "0" };
        let _: Result<(), _> = conn.set(LOG_REQUEST_BODY_KEY, val).await;
    }
}

pub async fn invalidate_log_request_body(redis: &Pool) {
    if let Ok(mut conn) = redis.get().await {
        let _: Result<(), _> = conn.del(LOG_REQUEST_BODY_KEY).await;
    }
}

const DEFAULT_NON_STREAM_TIMEOUT_MS_KEY: &str = "settings:default_non_stream_timeout_ms";
/// Sentinel stored when the setting is NULL (unbounded), so a cache hit can be told
/// apart from a cache miss — `Option<Option<i32>>` doesn't survive a string round trip.
const NON_STREAM_TIMEOUT_NULL_SENTINEL: &str = "null";

/// Returns Ok(Some(value)) on cache hit (value may itself be None), Ok(None) on cache
/// miss, Err(()) on Redis failure.
pub async fn get_default_non_stream_timeout_ms(redis: &Pool) -> Result<Option<Option<i32>>, ()> {
    let mut conn = redis.get().await.map_err(|_| ())?;
    let data: Option<String> = conn
        .get(DEFAULT_NON_STREAM_TIMEOUT_MS_KEY)
        .await
        .map_err(|_| ())?;
    Ok(data.map(|d| {
        if d == NON_STREAM_TIMEOUT_NULL_SENTINEL {
            None
        } else {
            d.parse::<i32>().ok()
        }
    }))
}

pub async fn set_default_non_stream_timeout_ms(redis: &Pool, value: Option<i32>) {
    if let Ok(mut conn) = redis.get().await {
        let val = value.map_or_else(|| NON_STREAM_TIMEOUT_NULL_SENTINEL.to_string(), |v| v.to_string());
        let _: Result<(), _> = conn.set(DEFAULT_NON_STREAM_TIMEOUT_MS_KEY, val).await;
    }
}

pub async fn invalidate_default_non_stream_timeout_ms(redis: &Pool) {
    if let Ok(mut conn) = redis.get().await {
        let _: Result<(), _> = conn.del(DEFAULT_NON_STREAM_TIMEOUT_MS_KEY).await;
    }
}

/// Add a user-agent to the group's seen-UA set.
/// Returns Ok(true) if the UA is new (SADD returned 1), Ok(false) if already present.
pub async fn add_group_ua(
    redis: &Pool,
    group_id: Uuid,
    user_agent: &str,
) -> Result<bool, anyhow::Error> {
    let mut conn = redis.get().await?;
    let key = format!("group:{group_id}:user_agents");
    let added: i64 = conn.sadd(key, user_agent).await?;
    Ok(added == 1)
}
