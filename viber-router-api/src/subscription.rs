use std::time::Duration;

use axum::{Json, http::StatusCode};
use chrono::{Datelike, Duration as ChronoDuration, LocalResult, NaiveDate, NaiveTime, TimeZone, Utc};
use deadpool_redis::redis::{AsyncCommands, cmd};
use sqlx::PgPool;
use uuid::Uuid;

use crate::cache;
use crate::models::KeySubscription;
use crate::routes::{AppState, ModelPricing};


#[derive(Debug, Clone)]
pub struct WeeklyWindow {
    pub monday_start_utc: chrono::DateTime<Utc>,
    pub next_monday_start_utc: chrono::DateTime<Utc>,
    pub monday_epoch: i64,
    pub ttl_seconds: i64,
    pub reset_at: chrono::DateTime<Utc>,
}

/// Parse a `YYYY-MM-DD` string into a UTC `DateTime` at end-of-day (23:59:59)
/// in the system's configured timezone. Returns a 400 error if the date is
/// invalid or in the past.
pub async fn parse_custom_expiry(
    state: &AppState,
    date_str: &str,
) -> Result<chrono::DateTime<Utc>, (StatusCode, Json<serde_json::Value>)> {
    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .map_err(|_| err_response(StatusCode::BAD_REQUEST, "Invalid date format, expected YYYY-MM-DD"))?;
    let tz = get_configured_timezone(state).await;
    let end_of_day = NaiveTime::from_hms_opt(23, 59, 59).unwrap();
    let local_dt = match tz.from_local_datetime(&date.and_time(end_of_day)) {
        LocalResult::Single(dt) => dt,
        LocalResult::Ambiguous(earliest, _) => earliest,
        LocalResult::None => {
            return Err(err_response(StatusCode::BAD_REQUEST, "Invalid date in the configured timezone"));
        }
    };
    let utc_dt = local_dt.with_timezone(&Utc);
    if utc_dt <= Utc::now() {
        return Err(err_response(StatusCode::BAD_REQUEST, "Expiry date must be in the future"));
    }
    Ok(utc_dt)
}

fn err_response(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({"error": msg})))
}

pub async fn get_configured_timezone(state: &AppState) -> chrono_tz::Tz {
    let timezone = match cache::get_timezone(&state.redis).await {
        Ok(Some(tz)) => tz,
        Ok(None) | Err(()) => {
            let tz = sqlx::query_scalar::<_, Option<String>>("SELECT timezone FROM settings WHERE id = 1")
                .fetch_optional(&state.db)
                .await
                .ok()
                .flatten()
                .flatten()
                .unwrap_or_else(|| cache::DEFAULT_TIMEZONE.to_string());
            cache::set_timezone(&state.redis, &tz).await;
            tz
        }
    };

    timezone
        .parse::<chrono_tz::Tz>()
        .unwrap_or(chrono_tz::Asia::Ho_Chi_Minh)
}

fn local_midnight(tz: chrono_tz::Tz, date: chrono::NaiveDate) -> chrono::DateTime<chrono_tz::Tz> {
    match tz.from_local_datetime(&date.and_time(NaiveTime::MIN)) {
        LocalResult::Single(dt) => dt,
        LocalResult::Ambiguous(earliest, _) => earliest,
        LocalResult::None => tz
            .from_local_datetime(&date.and_time(NaiveTime::from_hms_opt(1, 0, 0).unwrap()))
            .earliest()
            .unwrap_or_else(|| Utc::now().with_timezone(&tz)),
    }
}

pub fn weekly_window_for(tz: chrono_tz::Tz, now_utc: chrono::DateTime<Utc>) -> WeeklyWindow {
    let now_local = now_utc.with_timezone(&tz);
    let days_from_monday = now_local.weekday().num_days_from_monday() as i64;
    let monday_date = now_local.date_naive() - ChronoDuration::days(days_from_monday);
    let next_monday_date = monday_date + ChronoDuration::days(7);
    let monday_start_utc = local_midnight(tz, monday_date).with_timezone(&Utc);
    let next_monday_start_utc = local_midnight(tz, next_monday_date).with_timezone(&Utc);
    let ttl_seconds = (next_monday_start_utc - now_utc).num_seconds().max(1);

    WeeklyWindow {
        monday_start_utc,
        next_monday_start_utc,
        monday_epoch: monday_start_utc.timestamp(),
        ttl_seconds,
        reset_at: next_monday_start_utc,
    }
}

pub async fn current_weekly_window(state: &AppState) -> WeeklyWindow {
    let tz = get_configured_timezone(state).await;
    weekly_window_for(tz, Utc::now())
}

pub async fn get_weekly_cost(state: &AppState, sub: &KeySubscription) -> f64 {
    if sub.sub_type == "bonus" || sub.weekly_cost_limit_usd.is_none() {
        return 0.0;
    }

    let window = current_weekly_window(state).await;
    let key = format!("sub_weekly_cost:{}:w:{}", sub.id, window.monday_epoch);
    if let Ok(mut conn) = state.redis.get().await {
        let val: Result<Option<f64>, _> = conn.get(&key).await;
        if let Ok(Some(cost)) = val {
            return cost;
        }
    }

    let cost = sqlx::query_scalar::<_, f64>(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM token_usage_logs \
         WHERE subscription_id = $1 AND created_at >= $2 AND created_at < $3",
    )
    .bind(sub.id)
    .bind(window.monday_start_utc)
    .bind(window.next_monday_start_utc)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0.0);

    if let Ok(mut conn) = state.redis.get().await {
        let _: Result<(), _> = conn.set_ex(&key, cost, window.ttl_seconds as u64).await;
    }

    cost
}

/// Get the window start epoch for a subscription from Redis.
/// Returns None if no active window exists (key missing or Redis error).
async fn get_window_start(state: &AppState, sub_id: Uuid) -> Option<i64> {
    let key = format!("sub_window_start:{sub_id}");
    let mut conn = state.redis.get().await.ok()?;
    let val: Option<String> = conn.get(&key).await.ok()?;
    val?.parse::<i64>().ok()
}

/// Ensure a window start exists for a subscription, creating one if absent (SETNX semantics).
/// Returns the stored epoch, or None on Redis error.
async fn ensure_window_start(state: &AppState, sub_id: Uuid, reset_hours: i32) -> Option<i64> {
    let key = format!("sub_window_start:{sub_id}");
    let ttl = (reset_hours as i64) * 3600;
    let now_epoch = Utc::now().timestamp();

    let mut conn = match state.redis.get().await {
        Ok(c) => c,
        Err(_) => {
            tracing::warn!("Redis unavailable in ensure_window_start, sub_id={sub_id}");
            return None;
        }
    };

    // SET NX EX — only sets if key does not exist
    let _: Result<(), _> = deadpool_redis::redis::cmd("SET")
        .arg(&key)
        .arg(now_epoch)
        .arg("NX")
        .arg("EX")
        .arg(ttl)
        .query_async(&mut *conn)
        .await;

    // GET the stored value (either the one we just set, or the pre-existing one)
    let val: Option<String> = match conn.get(&key).await {
        Ok(v) => v,
        Err(_) => {
            tracing::warn!("Redis GET failed in ensure_window_start, sub_id={sub_id}");
            return None;
        }
    };
    val?.parse::<i64>().ok()
}

/// A bonus server extracted from a bonus subscription.
pub struct BonusServer {
    pub subscription_id: Uuid,
    pub base_url: String,
    pub api_key: String,
    pub name: String,
    pub allowed_models: Vec<String>,
    pub custom_headers: Option<serde_json::Value>,
    /// Per-bonus non-streaming timeout; `None` defers to the global default.
    pub non_stream_timeout_ms: Option<i32>,
}

/// Result of the pre-request subscription check.
pub enum SubCheckResult {
    /// A subscription was selected for charging.
    Allowed {
        subscription_id: Uuid,
        rpm_limit: Option<f64>,
        tpm_limit: Option<f64>,
    },
    /// All subscriptions are blocked (exhausted/expired/cancelled or per-model limit hit).
    Blocked,
    /// One or more bonus subscriptions are active. Try bonus servers first, then fall back.
    BonusServers {
        servers: Vec<BonusServer>,
        /// The non-bonus subscription to charge if all bonus servers fail (sub_id, rpm_limit, tpm_limit).
        fallback_subscription: Option<(Uuid, Option<f64>, Option<f64>)>,
    },
}

/// Load active subscriptions for a key, using Redis cache with DB fallback.
async fn load_subscriptions(state: &AppState, group_key_id: Uuid) -> Vec<KeySubscription> {
    let cache_key = format!("key_subs:{group_key_id}");

    // Try Redis cache
    if let Ok(mut conn) = state.redis.get().await {
        let cached: Result<Option<String>, _> = conn.get(&cache_key).await;
        if let Ok(Some(data)) = cached
            && let Ok(subs) = serde_json::from_str::<Vec<KeySubscription>>(&data)
        {
            return subs;
        }
    }

    // DB fallback
    let subs = sqlx::query_as::<_, KeySubscription>(
        "SELECT * FROM key_subscriptions WHERE group_key_id = $1 ORDER BY sort_order ASC, created_at ASC",
    )
    .bind(group_key_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    // Cache the result
    if let Ok(mut conn) = state.redis.get().await
        && let Ok(data) = serde_json::to_string(&subs)
    {
        let _: Result<(), _> = conn.set_ex(&cache_key, data, 300).await;
    }

    subs
}

/// Get the current cost for a subscription from Redis, rebuilding from DB on cache miss.
pub async fn get_total_cost(state: &AppState, sub: &KeySubscription) -> f64 {
    if sub.reset_hours.is_some() {
        let ws = match get_window_start(state, sub.id).await {
            Some(ws) => ws,
            None => return 0.0, // No active window
        };
        let key = format!("sub_cost:{}:ws:{ws}", sub.id);
        if let Ok(mut conn) = state.redis.get().await {
            let val: Result<Option<f64>, _> = conn.get(&key).await;
            if let Ok(Some(cost)) = val {
                return cost;
            }
        }
        // Rebuild from DB
        rebuild_total_cost(state, sub, ws).await
    } else {
        let key = format!("sub_cost:{}", sub.id);
        if let Ok(mut conn) = state.redis.get().await {
            let val: Result<Option<f64>, _> = conn.get(&key).await;
            if let Ok(Some(cost)) = val {
                return cost;
            }
        }
        // Rebuild from DB
        let cost = rebuild_total_cost_fixed(&state.db, sub).await;
        if let Ok(mut conn) = state.redis.get().await {
            let _: Result<(), _> = conn.set(&key, cost).await;
        }
        cost
    }
}

/// Get per-model cost for a subscription from Redis, rebuilding from DB on cache miss.
async fn get_model_cost(state: &AppState, sub: &KeySubscription, model: &str) -> f64 {
    if sub.reset_hours.is_some() {
        let ws = match get_window_start(state, sub.id).await {
            Some(ws) => ws,
            None => return 0.0, // No active window
        };
        let key = format!("sub_cost:{}:ws:{ws}:m:{model}", sub.id);
        if let Ok(mut conn) = state.redis.get().await {
            let val: Result<Option<f64>, _> = conn.get(&key).await;
            if let Ok(Some(cost)) = val {
                return cost;
            }
        }
        // Rebuild from DB
        rebuild_model_cost(state, sub, model, ws).await
    } else {
        let key = format!("sub_cost:{}:m:{model}", sub.id);
        if let Ok(mut conn) = state.redis.get().await {
            let val: Result<Option<f64>, _> = conn.get(&key).await;
            if let Ok(Some(cost)) = val {
                return cost;
            }
        }
        // Rebuild from DB
        let cost = rebuild_model_cost_fixed(&state.db, sub, model).await;
        if let Ok(mut conn) = state.redis.get().await {
            let _: Result<(), _> = conn.set(&key, cost).await;
        }
        cost
    }
}

/// Rebuild total cost for a windowed subscription from DB, cache the result, and return it.
async fn rebuild_total_cost(state: &AppState, sub: &KeySubscription, window_start: i64) -> f64 {
    let reset_hours = match sub.reset_hours {
        Some(rh) => rh,
        None => return 0.0,
    };
    let ws_dt = match Utc.timestamp_opt(window_start, 0).single() {
        Some(dt) => dt,
        None => return 0.0,
    };
    let we_dt = ws_dt + chrono::Duration::seconds((reset_hours as i64) * 3600);

    let cost = sqlx::query_scalar::<_, f64>(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM token_usage_logs \
         WHERE subscription_id = $1 AND created_at >= $2 AND created_at < $3",
    )
    .bind(sub.id)
    .bind(ws_dt)
    .bind(we_dt)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0.0);

    let key = format!("sub_cost:{}:ws:{window_start}", sub.id);
    if let Ok(mut conn) = state.redis.get().await {
        let _: Result<(), _> = conn.set(&key, cost).await;
    }
    cost
}

/// Rebuild total cost for a fixed (non-windowed) subscription from DB.
async fn rebuild_total_cost_fixed(db: &PgPool, sub: &KeySubscription) -> f64 {
    sqlx::query_scalar::<_, f64>(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM token_usage_logs WHERE subscription_id = $1",
    )
    .bind(sub.id)
    .fetch_one(db)
    .await
    .unwrap_or(0.0)
}

/// Rebuild per-model cost for a windowed subscription from DB, cache the result, and return it.
async fn rebuild_model_cost(
    state: &AppState,
    sub: &KeySubscription,
    model: &str,
    window_start: i64,
) -> f64 {
    let reset_hours = match sub.reset_hours {
        Some(rh) => rh,
        None => return 0.0,
    };
    let ws_dt = match Utc.timestamp_opt(window_start, 0).single() {
        Some(dt) => dt,
        None => return 0.0,
    };
    let we_dt = ws_dt + chrono::Duration::seconds((reset_hours as i64) * 3600);

    let cost = sqlx::query_scalar::<_, f64>(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM token_usage_logs \
         WHERE subscription_id = $1 AND model = $2 AND created_at >= $3 AND created_at < $4",
    )
    .bind(sub.id)
    .bind(model)
    .bind(ws_dt)
    .bind(we_dt)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0.0);

    let key = format!("sub_cost:{}:ws:{window_start}:m:{model}", sub.id);
    if let Ok(mut conn) = state.redis.get().await {
        let _: Result<(), _> = conn.set(&key, cost).await;
    }
    cost
}

/// Rebuild per-model cost for a fixed (non-windowed) subscription from DB.
async fn rebuild_model_cost_fixed(db: &PgPool, sub: &KeySubscription, model: &str) -> f64 {
    sqlx::query_scalar::<_, f64>(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM token_usage_logs \
         WHERE subscription_id = $1 AND model = $2",
    )
    .bind(sub.id)
    .bind(model)
    .fetch_one(db)
    .await
    .unwrap_or(0.0)
}

/// Pre-request subscription check. Returns which subscription to charge (if any).
pub async fn check_subscriptions(
    state: &AppState,
    group_key_id: Uuid,
    model: Option<&str>,
) -> SubCheckResult {
    let all_subs = load_subscriptions(state, group_key_id).await;
    let active: Vec<_> = all_subs
        .into_iter()
        .filter(|s| s.status == "active")
        .collect();
    if active.is_empty() {
        return SubCheckResult::Blocked;
    }

    // Separate bonus subs from non-bonus subs
    let (bonus_subs, non_bonus_subs): (Vec<_>, Vec<_>) =
        active.into_iter().partition(|s| s.sub_type == "bonus");

    // Run existing non-bonus logic on non-bonus subs to derive fallback_subscription
    // Sort: hourly_reset first, then pay_per_request, then fixed, FIFO within same type
    let mut sorted = non_bonus_subs;
    sorted.sort_by(|a, b| {
        let type_order = |t: &str| -> u8 {
            match t {
                "hourly_reset" => 0,
                "pay_per_request" => 1,
                _ => 2,
            }
        };
        type_order(&a.sub_type)
            .cmp(&type_order(&b.sub_type))
            .then(a.created_at.cmp(&b.created_at))
    });

    let now = Utc::now();

    let mut fallback_subscription: Option<(Uuid, Option<f64>, Option<f64>)> = None;
    for sub in &sorted {
        // Check expiration
        if let Some(expires_at) = sub.expires_at
            && now > expires_at
        {
            // Mark expired in DB (fire-and-forget)
            let db = state.db.clone();
            let sub_id = sub.id;
            let key_id = sub.group_key_id;
            let redis = state.redis.clone();
            tokio::spawn(async move {
                let _ = sqlx::query("UPDATE key_subscriptions SET status = 'expired' WHERE id = $1 AND status = 'active'")
                    .bind(sub_id)
                    .execute(&db)
                    .await;
                if let Ok(mut conn) = redis.get().await {
                    let _: Result<(), _> = conn.del(format!("key_subs:{key_id}")).await;
                }
            });
            continue;
        }

        // Check total budget
        let total_cost = get_total_cost(state, sub).await;
        if total_cost >= sub.cost_limit_usd {
            if sub.reset_hours.is_some() {
                // Window full — skip but don't mark exhausted
                continue;
            }
            // Fixed/pay_per_request subscription exhausted — mark permanently
            let db = state.db.clone();
            let sub_id = sub.id;
            let key_id = sub.group_key_id;
            let redis = state.redis.clone();
            tokio::spawn(async move {
                let _ = sqlx::query("UPDATE key_subscriptions SET status = 'exhausted' WHERE id = $1 AND status = 'active'")
                    .bind(sub_id)
                    .execute(&db)
                    .await;
                if let Ok(mut conn) = redis.get().await {
                    let _: Result<(), _> = conn.del(format!("key_subs:{key_id}")).await;
                }
            });
            continue;
        }

        // Check weekly budget without exhausting the subscription; it resets next week.
        if let Some(weekly_limit) = sub.weekly_cost_limit_usd
            && weekly_limit > 0.0
            && get_weekly_cost(state, sub).await >= weekly_limit
        {
            continue;
        }

        // For pay_per_request: skip if model is not in model_request_costs
        if sub.sub_type == "pay_per_request"
            && let Some(model_name) = model
            && sub
                .model_request_costs
                .as_object()
                .map(|m| !m.contains_key(model_name))
                .unwrap_or(true)
        {
            continue;
        }

        // Check per-model limit
        if let Some(model_name) = model
            && let Some(limits) = sub.model_limits.as_object()
            && let Some(limit_val) = limits.get(model_name).and_then(|v| v.as_f64())
        {
            let model_cost = get_model_cost(state, sub, model_name).await;
            if model_cost >= limit_val {
                continue;
            }
        }

        // Check RPM limit
        if let Some(rpm) = sub.rpm_limit
            && rpm > 0.0
            && is_rpm_limited(state, sub.id, rpm).await
        {
            continue;
        }

        // This non-bonus subscription has budget — it's the fallback
        fallback_subscription = Some((sub.id, sub.rpm_limit, sub.tpm_limit));
        break;
    }

    // Collect active bonus subs sorted by sort_order ASC, created_at ASC
    let bonus_servers: Vec<BonusServer> = bonus_subs
        .into_iter()
        .filter_map(|sub| {
            let allowed_models = sub.bonus_allowed_models.unwrap_or_default();
            if !allowed_models.is_empty() {
                match model {
                    Some(model_name)
                        if allowed_models.iter().any(|allowed| allowed == model_name) => {}
                    Some(_) | None => return None,
                }
            }

            let base_url = sub.bonus_base_url?;
            let api_key = sub.bonus_api_key?;
            let name = sub.bonus_name.unwrap_or_default();
            Some(BonusServer {
                subscription_id: sub.id,
                base_url,
                api_key,
                name,
                allowed_models,
                custom_headers: sub.bonus_custom_headers,
                non_stream_timeout_ms: sub.bonus_non_stream_timeout_ms,
            })
        })
        .collect();
    // bonus_servers already ordered by sort_order ASC, created_at ASC from load_subscriptions

    // If non-empty bonus subs — return BonusServers
    if !bonus_servers.is_empty() {
        return SubCheckResult::BonusServers {
            servers: bonus_servers,
            fallback_subscription,
        };
    }

    // No bonus subs — return existing result based on fallback_subscription
    if let Some((sub_id, rpm_limit, tpm_limit)) = fallback_subscription {
        SubCheckResult::Allowed {
            subscription_id: sub_id,
            rpm_limit,
            tpm_limit,
        }
    } else {
        SubCheckResult::Blocked
    }
}

/// Handle lazy activation for a subscription.
pub async fn ensure_activated(state: &AppState, sub_id: Uuid, duration_days: i32) {
    let activation_key = format!("sub_activated:{sub_id}");
    let now = Utc::now();
    let now_str = now.to_rfc3339();
    let ttl_secs = (duration_days as i64) * 86400 + 3600; // duration + 1h buffer

    if let Ok(mut conn) = state.redis.get().await {
        // SETNX with TTL — only first request sets the activation
        let set: Result<bool, _> = deadpool_redis::redis::cmd("SET")
            .arg(&activation_key)
            .arg(&now_str)
            .arg("NX")
            .arg("EX")
            .arg(ttl_secs)
            .query_async(&mut *conn)
            .await;

        if let Ok(true) = set {
            // We won the race — update DB
            // Use COALESCE to preserve a custom expires_at if already set
            let expires_at = now + chrono::Duration::days(duration_days as i64);
            let _ = sqlx::query(
                "UPDATE key_subscriptions SET activated_at = $1, expires_at = COALESCE(expires_at, $2) WHERE id = $3 AND activated_at IS NULL",
            )
            .bind(now)
            .bind(expires_at)
            .bind(sub_id)
            .execute(&state.db)
            .await;

            // Invalidate subscription list cache
            if let Ok(gk_id) = sqlx::query_scalar::<_, Uuid>(
                "SELECT group_key_id FROM key_subscriptions WHERE id = $1",
            )
            .bind(sub_id)
            .fetch_one(&state.db)
            .await
            {
                let _: Result<(), _> = conn.del(format!("key_subs:{gk_id}")).await;
            }
        }
        // SETNX loser or already activated — no action needed.
        // The caller re-fetches the subscription from DB to get the stored activated_at.
    } else {
        // Redis unavailable — fall back to DB-only activation
        // Use COALESCE to preserve a custom expires_at if already set
        let expires_at = now + chrono::Duration::days(duration_days as i64);
        let _ = sqlx::query(
            "UPDATE key_subscriptions SET activated_at = $1, expires_at = COALESCE(expires_at, $2) WHERE id = $3 AND activated_at IS NULL",
        )
        .bind(now)
        .bind(expires_at)
        .bind(sub_id)
        .execute(&state.db)
        .await;
        tracing::warn!("Redis unavailable for subscription activation, used DB-only fallback");
    }
}

/// Calculate cost for a request using model pricing and server rates.
#[allow(clippy::too_many_arguments)]
pub fn calculate_cost(
    pricing: &ModelPricing,
    rate_input: f64,
    rate_output: f64,
    rate_cache_write: f64,
    rate_cache_read: f64,
    input_tokens: i32,
    output_tokens: i32,
    cache_creation_tokens: Option<i32>,
    cache_read_tokens: Option<i32>,
    normalize_cache_read: bool,
) -> f64 {
    let inp = (input_tokens as f64) * pricing.input_1m_usd * rate_input;
    let out = (output_tokens as f64) * pricing.output_1m_usd * rate_output;
    let cw =
        (cache_creation_tokens.unwrap_or(0) as f64) * pricing.cache_write_1m_usd * rate_cache_write;
    let cr = if normalize_cache_read {
        (cache_read_tokens.unwrap_or(0) as f64) * pricing.input_1m_usd * rate_input
    } else {
        (cache_read_tokens.unwrap_or(0) as f64) * pricing.cache_read_1m_usd * rate_cache_read
    };
    (inp + out + cw + cr) / 1_000_000.0
}

/// Update Redis cost counters after a successful request.
pub async fn update_cost_counters(
    state: &AppState,
    sub_id: Uuid,
    model: &str,
    cost: f64,
    reset_hours: Option<i32>,
    weekly_cost_limit_usd: Option<f64>,
) {
    let Ok(mut conn) = state.redis.get().await else {
        tracing::warn!("Redis unavailable for cost counter update, sub_id={sub_id}, cost={cost}");
        return;
    };

    if let Some(weekly_limit) = weekly_cost_limit_usd
        && weekly_limit > 0.0
    {
        let window = current_weekly_window(state).await;
        let weekly_key = format!("sub_weekly_cost:{sub_id}:w:{}", window.monday_epoch);
        let _: Result<f64, _> = deadpool_redis::redis::cmd("INCRBYFLOAT")
            .arg(&weekly_key)
            .arg(cost)
            .query_async(&mut *conn)
            .await;
        let _: Result<(), _> = conn.expire(&weekly_key, window.ttl_seconds).await;
    }

    if let Some(rh) = reset_hours {
        // Demand-driven windowing: ensure a window start exists
        let ws = match ensure_window_start(state, sub_id, rh).await {
            Some(ws) => ws,
            None => {
                tracing::warn!(
                    "Could not ensure window start for sub_id={sub_id}, skipping cost counter update"
                );
                return;
            }
        };
        let ttl = (rh as i64) * 3600;

        // Window total
        let wkey = format!("sub_cost:{sub_id}:ws:{ws}");
        let _: Result<f64, _> = deadpool_redis::redis::cmd("INCRBYFLOAT")
            .arg(&wkey)
            .arg(cost)
            .query_async(&mut *conn)
            .await;
        let _: Result<(), _> = conn.expire(&wkey, ttl).await;

        // Window per-model
        let wmkey = format!("sub_cost:{sub_id}:ws:{ws}:m:{model}");
        let _: Result<f64, _> = deadpool_redis::redis::cmd("INCRBYFLOAT")
            .arg(&wmkey)
            .arg(cost)
            .query_async(&mut *conn)
            .await;
        let _: Result<(), _> = conn.expire(&wmkey, ttl).await;
    } else {
        // Fixed total
        let tkey = format!("sub_cost:{sub_id}");
        let _: Result<f64, _> = deadpool_redis::redis::cmd("INCRBYFLOAT")
            .arg(&tkey)
            .arg(cost)
            .query_async(&mut *conn)
            .await;

        // Fixed per-model
        let mkey = format!("sub_cost:{sub_id}:m:{model}");
        let _: Result<f64, _> = deadpool_redis::redis::cmd("INCRBYFLOAT")
            .arg(&mkey)
            .arg(cost)
            .query_async(&mut *conn)
            .await;
    }
}

/// Compute the RPM window parameters from a float RPM value.
/// Returns (window_seconds, max_requests).
/// RPM < 1: scale window up, max = 1. RPM >= 1: window = 60s, max = floor(rpm).
pub fn compute_rpm_window(rpm: f64) -> (i64, i64) {
    if rpm < 1.0 {
        let window = (60.0 / rpm).ceil() as i64;
        (window, 1)
    } else {
        (60, rpm.floor() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::ModelPricing;

    #[test]
    fn test_weekly_window_for_uses_configured_monday_boundary() {
        let tz = chrono_tz::Asia::Ho_Chi_Minh;
        let now = Utc.with_ymd_and_hms(2026, 5, 1, 3, 0, 0).unwrap();
        let window = weekly_window_for(tz, now);

        assert_eq!(window.monday_start_utc, Utc.with_ymd_and_hms(2026, 4, 26, 17, 0, 0).unwrap());
        assert_eq!(window.next_monday_start_utc, Utc.with_ymd_and_hms(2026, 5, 3, 17, 0, 0).unwrap());
        assert_eq!(window.monday_epoch, Utc.with_ymd_and_hms(2026, 4, 26, 17, 0, 0).unwrap().timestamp());
        assert_eq!(window.reset_at, window.next_monday_start_utc);
        assert!(window.ttl_seconds > 0);
    }

    #[test]
    fn test_compute_rpm_window_above_one() {
        assert_eq!(compute_rpm_window(5.0), (60, 5));
        assert_eq!(compute_rpm_window(1.0), (60, 1));
        assert_eq!(compute_rpm_window(10.5), (60, 10));
    }

    #[test]
    fn test_compute_rpm_window_below_one() {
        assert_eq!(compute_rpm_window(0.5), (120, 1));
        assert_eq!(compute_rpm_window(0.1), (600, 1));
    }

    #[test]
    fn test_tpm_increment_amount_uses_actual_input_plus_output_tokens() {
        assert_eq!(tpm_increment_amount(1000, 250), Some(1250));
    }

    #[test]
    fn test_tpm_increment_amount_skips_missing_or_empty_usage() {
        assert_eq!(tpm_increment_amount(0, 0), None);
        assert_eq!(tpm_increment_amount(-10, 10), None);
    }

    #[test]
    fn test_calculate_cost_basic() {
        let pricing = ModelPricing {
            input_1m_usd: 3.0,
            output_1m_usd: 15.0,
            cache_write_1m_usd: 3.75,
            cache_read_1m_usd: 0.30,
        };
        let cost = calculate_cost(
            &pricing, 1.0, 1.0, 1.0, 1.0, // rates = 1x
            1000, 500, None, None, false,
        );
        // input: 1000 * 3.0 / 1M = 0.003
        // output: 500 * 15.0 / 1M = 0.0075
        let expected = (1000.0 * 3.0 + 500.0 * 15.0) / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_calculate_cost_with_cache_tokens() {
        let pricing = ModelPricing {
            input_1m_usd: 3.0,
            output_1m_usd: 15.0,
            cache_write_1m_usd: 3.75,
            cache_read_1m_usd: 0.30,
        };
        let cost = calculate_cost(
            &pricing,
            1.0,
            1.0,
            1.0,
            1.0,
            1000,
            500,
            Some(200),
            Some(300),
            false,
        );
        let expected = (1000.0 * 3.0 + 500.0 * 15.0 + 200.0 * 3.75 + 300.0 * 0.30) / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_calculate_cost_with_rates() {
        let pricing = ModelPricing {
            input_1m_usd: 3.0,
            output_1m_usd: 15.0,
            cache_write_1m_usd: 3.75,
            cache_read_1m_usd: 0.30,
        };
        let cost = calculate_cost(
            &pricing, 0.5, 2.0, 1.0, 1.0, // input at 50%, output at 200%
            1000, 500, None, None, false,
        );
        let expected = (1000.0 * 3.0 * 0.5 + 500.0 * 15.0 * 2.0) / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_calculate_cost_normalize_cache_read() {
        let pricing = ModelPricing {
            input_1m_usd: 3.0,
            output_1m_usd: 15.0,
            cache_write_1m_usd: 3.75,
            cache_read_1m_usd: 0.30,
        };
        // With normalize_cache_read=true, cache_read uses input price instead
        let cost = calculate_cost(
            &pricing,
            1.0,
            1.0,
            1.0,
            1.0,
            1000,
            500,
            None,
            Some(300),
            true,
        );
        // cache_read: 300 * input_1m_usd(3.0) * rate_input(1.0) instead of cache_read_1m_usd
        let expected = (1000.0 * 3.0 + 500.0 * 15.0 + 300.0 * 3.0) / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_bonus_server_from_key_subscription() {
        let sub = KeySubscription {
            id: Uuid::new_v4(),
            group_key_id: Uuid::new_v4(),
            plan_id: None,
            sub_type: "bonus".to_string(),
            cost_limit_usd: 0.0,
            weekly_cost_limit_usd: None,
            model_limits: serde_json::json!({}),
            model_request_costs: serde_json::json!({}),
            reset_hours: None,
            duration_days: 36500,
            rpm_limit: None,
            tpm_limit: None,
            status: "active".to_string(),
            activated_at: None,
            expires_at: None,
            created_at: chrono::Utc::now(),
            bonus_base_url: Some("https://api.anthropic.com".to_string()),
            bonus_api_key: Some("sk-ant-test123".to_string()),
            bonus_name: Some("Claude Code Max".to_string()),
            bonus_quota_url: None,
            bonus_quota_headers: None,
            bonus_allowed_models: None,
            sort_order: 0,
            bonus_custom_headers: None,
            bonus_non_stream_timeout_ms: Some(45_000),
        };

        // Simulate the filter_map logic from check_subscriptions
        let server = BonusServer {
            subscription_id: sub.id,
            base_url: sub.bonus_base_url.clone().unwrap(),
            api_key: sub.bonus_api_key.clone().unwrap(),
            name: sub.bonus_name.clone().unwrap_or_default(),
            allowed_models: sub.bonus_allowed_models.clone().unwrap_or_default(),
            custom_headers: sub.bonus_custom_headers.clone(),
            non_stream_timeout_ms: sub.bonus_non_stream_timeout_ms,
        };

        assert_eq!(server.base_url, "https://api.anthropic.com");
        assert_eq!(server.api_key, "sk-ant-test123");
        assert_eq!(server.name, "Claude Code Max");
        assert_eq!(server.subscription_id, sub.id);
        // The per-bonus timeout must reach the waterfall, otherwise a stalled bonus
        // upstream would silently fall back to the global default.
        assert_eq!(server.non_stream_timeout_ms, Some(45_000));
    }

    #[test]
    fn test_bonus_server_skipped_when_missing_fields() {
        let sub = KeySubscription {
            id: Uuid::new_v4(),
            group_key_id: Uuid::new_v4(),
            plan_id: None,
            sub_type: "bonus".to_string(),
            cost_limit_usd: 0.0,
            weekly_cost_limit_usd: None,
            model_limits: serde_json::json!({}),
            model_request_costs: serde_json::json!({}),
            reset_hours: None,
            duration_days: 36500,
            rpm_limit: None,
            tpm_limit: None,
            status: "active".to_string(),
            activated_at: None,
            expires_at: None,
            created_at: chrono::Utc::now(),
            bonus_base_url: None, // Missing!
            bonus_api_key: Some("sk-ant-test123".to_string()),
            bonus_name: Some("Broken Bonus".to_string()),
            bonus_quota_url: None,
            bonus_quota_headers: None,
            bonus_allowed_models: None,
            sort_order: 0,
            bonus_custom_headers: None,
            bonus_non_stream_timeout_ms: None,
        };

        // Simulate the filter_map — should return None due to missing base_url
        let result = sub.bonus_base_url.as_ref().and_then(|base_url| {
            sub.bonus_api_key.as_ref().map(|api_key| BonusServer {
                subscription_id: sub.id,
                base_url: base_url.clone(),
                api_key: api_key.clone(),
                name: sub.bonus_name.clone().unwrap_or_default(),
                allowed_models: sub.bonus_allowed_models.clone().unwrap_or_default(),
                custom_headers: sub.bonus_custom_headers.clone(),
                non_stream_timeout_ms: sub.bonus_non_stream_timeout_ms,
            })
        });

        assert!(result.is_none());
    }

    #[test]
    fn test_bonus_default_name_when_missing() {
        let sub = KeySubscription {
            id: Uuid::new_v4(),
            group_key_id: Uuid::new_v4(),
            plan_id: None,
            sub_type: "bonus".to_string(),
            cost_limit_usd: 0.0,
            weekly_cost_limit_usd: None,
            model_limits: serde_json::json!({}),
            model_request_costs: serde_json::json!({}),
            reset_hours: None,
            duration_days: 36500,
            rpm_limit: None,
            tpm_limit: None,
            status: "active".to_string(),
            activated_at: None,
            expires_at: None,
            created_at: chrono::Utc::now(),
            bonus_base_url: Some("https://api.example.com".to_string()),
            bonus_api_key: Some("key-123".to_string()),
            bonus_name: None, // Name is None
            bonus_quota_url: None,
            bonus_quota_headers: None,
            bonus_allowed_models: None,
            sort_order: 0,
            bonus_custom_headers: None,
            bonus_non_stream_timeout_ms: None,
        };

        let name = sub.bonus_name.unwrap_or_default();
        assert_eq!(name, "");
    }
}

/// Check if a subscription has exceeded its TPM limit.
/// Returns Ok(Some(ttl_seconds)) when limited and Redis has a positive TTL.
/// Returns Ok(None) when under limit, unlimited, or Redis errors require fail-open behavior.
pub async fn is_tpm_limited(state: &AppState, sub_id: Uuid, tpm: f64) -> Result<Option<i64>, ()> {
    if tpm <= 0.0 {
        return Ok(None);
    }

    let key = format!("sub_tpm:{sub_id}");
    let Ok(mut conn) = state.redis.get().await else {
        tracing::warn!("Redis unavailable in is_tpm_limited, sub_id={sub_id}");
        return Ok(None); // fail open
    };

    let count: Option<i64> = match cmd("GET").arg(&key).query_async(&mut *conn).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Redis GET failed in is_tpm_limited, sub_id={sub_id}: {e}");
            return Ok(None); // fail open
        }
    };

    if (count.unwrap_or(0) as f64) < tpm {
        return Ok(None);
    }

    let ttl: i64 = match cmd("TTL").arg(&key).query_async(&mut *conn).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("Redis TTL failed in is_tpm_limited, sub_id={sub_id}: {e}");
            return Ok(None); // fail open
        }
    };

    if ttl > 0 { Ok(Some(ttl)) } else { Ok(None) }
}

/// Wait for a selected subscription's TPM fixed window to reset.
/// Returns Err when the subscription remains limited after five wait retries.
pub async fn wait_for_tpm(state: &AppState, sub_id: Uuid, tpm: Option<f64>) -> Result<(), ()> {
    let Some(tpm) = tpm else {
        return Ok(());
    };

    for attempt in 0..=5 {
        let Some(ttl) = is_tpm_limited(state, sub_id, tpm).await? else {
            return Ok(());
        };

        if attempt == 5 {
            tracing::warn!(
                %sub_id,
                tpm_limit = tpm,
                "wait_for_tpm: budget exhausted after 5 retries, giving up"
            );
            return Err(());
        }

        tracing::warn!(
            %sub_id,
            tpm_limit = tpm,
            attempt,
            sleep_secs = ttl,
            "wait_for_tpm: TPM budget exhausted, sleeping until window resets"
        );
        tokio::time::sleep(Duration::from_secs(ttl as u64)).await;
    }

    Err(())
}

fn tpm_increment_amount(input_tokens: i32, output_tokens: i32) -> Option<i64> {
    let added = i64::from(input_tokens) + i64::from(output_tokens);
    (added > 0).then_some(added)
}

/// Increment the TPM counter for a subscription after actual token usage is parsed.
/// Sets the 60 second TTL only when this INCRBY created a fresh fixed window.
pub async fn increment_tpm(state: &AppState, sub_id: Uuid, input_tokens: i32, output_tokens: i32) {
    let Some(added) = tpm_increment_amount(input_tokens, output_tokens) else {
        return;
    };

    let key = format!("sub_tpm:{sub_id}");
    let Ok(mut conn) = state.redis.get().await else {
        tracing::warn!("Redis unavailable for TPM counter update, sub_id={sub_id}, added={added}");
        return;
    };

    let total: i64 = match cmd("INCRBY")
        .arg(&key)
        .arg(added)
        .query_async(&mut *conn)
        .await
    {
        Ok(total) => total,
        Err(e) => {
            tracing::warn!("Redis INCRBY failed for TPM counter, sub_id={sub_id}: {e}");
            return;
        }
    };

    if total == added {
        let _: Result<(), _> = cmd("EXPIRE")
            .arg(&key)
            .arg(60)
            .query_async(&mut *conn)
            .await;
    }
}

/// Check if a subscription has exceeded its RPM limit.
/// Fails open on Redis errors (returns false).
async fn is_rpm_limited(state: &AppState, sub_id: Uuid, rpm: f64) -> bool {
    let key = format!("sub_rpm:{sub_id}");
    let Ok(mut conn) = state.redis.get().await else {
        return false; // fail open
    };
    let count: Option<i64> = match cmd("GET").arg(&key).query_async(&mut *conn).await {
        Ok(c) => c,
        Err(_) => return false, // fail open
    };
    let (_, max_requests) = compute_rpm_window(rpm);
    count.unwrap_or(0) >= max_requests
}

/// Increment the RPM counter for a subscription after selecting it for a request.
/// Always sets TTL to ensure the key expires even if a previous EXPIRE failed.
/// Silently skips on Redis errors.
pub async fn increment_rpm(state: &AppState, sub_id: Uuid, rpm: f64) {
    let key = format!("sub_rpm:{sub_id}");
    let Ok(mut conn) = state.redis.get().await else {
        return;
    };
    let _: Result<i64, _> = cmd("INCR").arg(&key).query_async(&mut *conn).await;
    let (window_seconds, _) = compute_rpm_window(rpm);
    let _: Result<(), _> = cmd("EXPIRE")
        .arg(&key)
        .arg(window_seconds)
        .query_async(&mut *conn)
        .await;
}
