use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, put},
};
use deadpool_redis::redis::AsyncCommands;
use serde::Deserialize;
use std::collections::HashSet;
use uuid::Uuid;

use crate::models::{
    AssignSubscription, CancelSubscription, KeySubscription, PaginatedResponse, SubscriptionPlan,
    UpdateBonusCustomHeaders, UpdateBonusNonStreamTimeout, UpdateBonusQuotaConfig,
    UpdateBonusSubscription,
};
use crate::routes::AppState;

type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(serde::Serialize)]
struct KeySubscriptionWithUsage {
    #[serde(flatten)]
    sub: KeySubscription,
    cost_used: f64,
}

#[derive(Debug, Deserialize)]
struct ListParams {
    page: Option<i64>,
    limit: Option<i64>,
}

fn err(status: StatusCode, msg: &str) -> ApiError {
    (status, Json(serde_json::json!({"error": msg})))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_subscriptions).post(assign_subscription))
        .route("/reorder", put(reorder_subscriptions))
        .route("/{sub_id}", axum::routing::patch(cancel_subscription))
        .route(
            "/{sub_id}/bonus-allowed-models",
            put(update_bonus_allowed_models),
        )
        .route(
            "/{sub_id}/bonus-custom-headers",
            put(update_bonus_custom_headers),
        )
        .route(
            "/{sub_id}/bonus-quota-config",
            put(update_bonus_quota_config),
        )
        .route(
            "/{sub_id}/bonus-non-stream-timeout",
            put(update_bonus_non_stream_timeout),
        )
}

#[derive(Debug, Deserialize)]
struct ReorderSubscriptions {
    ordered_ids: Vec<Uuid>,
}

/// Pure validation: checks that ordered_ids is a valid full replacement for bonus_ids.
/// Returns Ok(()) or Err with a human-readable message.
fn validate_reorder_set(ordered_ids: &[Uuid], bonus_ids: &[Uuid]) -> Result<(), &'static str> {
    // 1. No duplicates
    let unique: HashSet<&Uuid> = ordered_ids.iter().collect();
    if unique.len() != ordered_ids.len() {
        return Err("ordered_ids contains duplicate ids");
    }

    // 2. Same length
    if ordered_ids.len() != bonus_ids.len() {
        return Err("ordered_ids must contain every bonus subscription for this key");
    }

    // 3. Every ordered id must be in bonus_ids
    let bonus_set: HashSet<&Uuid> = bonus_ids.iter().collect();
    for id in ordered_ids {
        if !bonus_set.contains(id) {
            return Err("ordered_ids contains an id that is not a bonus subscription for this key");
        }
    }

    Ok(())
}

async fn reorder_subscriptions(
    State(state): State<AppState>,
    Path((_group_id, key_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<ReorderSubscriptions>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Load all subs for this key to validate
    let all_subs = sqlx::query_as::<_, KeySubscription>(
        "SELECT * FROM key_subscriptions WHERE group_key_id = $1",
    )
    .bind(key_id)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    // Separate bonus ids and check sub_type for each ordered id
    let bonus_ids: Vec<Uuid> = all_subs
        .iter()
        .filter(|s| s.sub_type == "bonus")
        .map(|s| s.id)
        .collect();

    // Check for ids that exist for this key but are not bonus type
    let all_ids_set: HashSet<Uuid> = all_subs.iter().map(|s| s.id).collect();
    let bonus_ids_set: HashSet<Uuid> = bonus_ids.iter().copied().collect();
    for id in &input.ordered_ids {
        if all_ids_set.contains(id) && !bonus_ids_set.contains(id) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "ordered_ids contains a non-bonus subscription id; only bonus subscriptions can be reordered",
            ));
        }
    }

    // Run pure validation
    validate_reorder_set(&input.ordered_ids, &bonus_ids)
        .map_err(|msg| err(StatusCode::BAD_REQUEST, msg))?;

    // Apply updates in a single transaction
    let mut tx = state.db.begin().await.map_err(internal)?;
    for (idx, id) in input.ordered_ids.iter().enumerate() {
        sqlx::query("UPDATE key_subscriptions SET sort_order = $1 WHERE id = $2")
            .bind(idx as i32)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
    }
    tx.commit().await.map_err(internal)?;

    // Invalidate Redis cache
    if let Ok(mut conn) = state.redis.get().await {
        let _: Result<(), _> = conn.del(format!("key_subs:{key_id}")).await;
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

async fn assign_subscription(
    State(state): State<AppState>,
    Path((_group_id, key_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<AssignSubscription>,
) -> Result<(StatusCode, Json<KeySubscription>), ApiError> {
    // Detect bonus creation path: has bonus fields, no plan_id
    if input.bonus_name.is_some() || input.bonus_base_url.is_some() || input.bonus_api_key.is_some()
    {
        // Reject custom_expires_at for bonus subscriptions
        if input.custom_expires_at.is_some() {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "custom_expires_at is not supported for bonus subscriptions",
            ));
        }

        // Validate required bonus fields
        let bonus_name = input
            .bonus_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "bonus_name is required"))?;
        let bonus_base_url = input
            .bonus_base_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "bonus_base_url is required"))?;
        let bonus_api_key = input
            .bonus_api_key
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "bonus_api_key is required"))?;

        let sub = sqlx::query_as::<_, KeySubscription>(
            "INSERT INTO key_subscriptions \
             (group_key_id, plan_id, sub_type, cost_limit_usd, model_limits, model_request_costs, \
              reset_hours, duration_days, rpm_limit, tpm_limit, bonus_name, bonus_base_url, bonus_api_key, \
              bonus_quota_url, bonus_quota_headers, bonus_allowed_models, bonus_custom_headers) \
             VALUES ($1, NULL, 'bonus', 0, '{}', '{}', NULL, 36500, NULL, NULL, $2, $3, $4, $5, $6, $7, $8) \
             RETURNING *",
        )
        .bind(key_id)
        .bind(bonus_name)
        .bind(bonus_base_url)
        .bind(bonus_api_key)
        .bind(&input.bonus_quota_url)
        .bind(&input.bonus_quota_headers)
        .bind(&input.bonus_allowed_models)
        .bind(&input.bonus_custom_headers)
        .fetch_one(&state.db)
        .await
        .map_err(internal)?;

        // Invalidate subscription list cache
        if let Ok(mut conn) = state.redis.get().await {
            let _: Result<(), _> = conn.del(format!("key_subs:{key_id}")).await;
        }

        return Ok((StatusCode::CREATED, Json(sub)));
    }

    // Standard plan-based creation
    let plan_id = input.plan_id
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "Either plan_id or bonus fields (bonus_name, bonus_base_url, bonus_api_key) are required"))?;

    let plan =
        sqlx::query_as::<_, SubscriptionPlan>("SELECT * FROM subscription_plans WHERE id = $1")
            .bind(plan_id)
            .fetch_optional(&state.db)
            .await
            .map_err(internal)?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "Plan not found"))?;

    if !plan.is_active {
        return Err(err(StatusCode::BAD_REQUEST, "Plan is not active"));
    }

    // Parse optional custom expiry date
    let custom_expires_utc = match input.custom_expires_at {
        Some(ref date_str) => Some(crate::subscription::parse_custom_expiry(&state, date_str).await?),
        None => None,
    };

    let sub = sqlx::query_as::<_, KeySubscription>(
        "INSERT INTO key_subscriptions (group_key_id, plan_id, sub_type, cost_limit_usd, weekly_cost_limit_usd, model_limits, model_request_costs, reset_hours, duration_days, rpm_limit, tpm_limit, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING *",
    )
    .bind(key_id)
    .bind(plan.id)
    .bind(&plan.sub_type)
    .bind(plan.cost_limit_usd)
    .bind(plan.weekly_cost_limit_usd)
    .bind(&plan.model_limits)
    .bind(&plan.model_request_costs)
    .bind(plan.reset_hours)
    .bind(plan.duration_days)
    .bind(plan.rpm_limit)
    .bind(plan.tpm_limit)
    .bind(custom_expires_utc)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    // Invalidate subscription list cache
    if let Ok(mut conn) = state.redis.get().await {
        let _: Result<(), _> = conn.del(format!("key_subs:{key_id}")).await;
    }

    Ok((StatusCode::CREATED, Json(sub)))
}

async fn update_bonus_allowed_models(
    State(state): State<AppState>,
    Path((_group_id, key_id, sub_id)): Path<(Uuid, Uuid, Uuid)>,
    Json(input): Json<UpdateBonusSubscription>,
) -> Result<Json<KeySubscription>, ApiError> {
    let current = sqlx::query_as::<_, KeySubscription>(
        "SELECT * FROM key_subscriptions WHERE id = $1 AND group_key_id = $2",
    )
    .bind(sub_id)
    .bind(key_id)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "Subscription not found"))?;

    if current.sub_type != "bonus" {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Only bonus subscriptions can update allowed models",
        ));
    }

    if current.status != "active" {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Only active bonus subscriptions can update allowed models",
        ));
    }

    let sub = sqlx::query_as::<_, KeySubscription>(
        "UPDATE key_subscriptions SET bonus_allowed_models = $1 WHERE id = $2 RETURNING *",
    )
    .bind(&input.bonus_allowed_models)
    .bind(sub_id)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    // Invalidate subscription list cache
    if let Ok(mut conn) = state.redis.get().await {
        let _: Result<(), _> = conn.del(format!("key_subs:{key_id}")).await;
    }

    Ok(Json(sub))
}

async fn update_bonus_custom_headers(
    State(state): State<AppState>,
    Path((_group_id, key_id, sub_id)): Path<(Uuid, Uuid, Uuid)>,
    Json(input): Json<UpdateBonusCustomHeaders>,
) -> Result<Json<KeySubscription>, ApiError> {
    let current = sqlx::query_as::<_, KeySubscription>(
        "SELECT * FROM key_subscriptions WHERE id = $1 AND group_key_id = $2",
    )
    .bind(sub_id)
    .bind(key_id)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "Subscription not found"))?;

    if current.sub_type != "bonus" {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Only bonus subscriptions can update custom headers",
        ));
    }

    let sub = sqlx::query_as::<_, KeySubscription>(
        "UPDATE key_subscriptions SET bonus_custom_headers = $1 WHERE id = $2 RETURNING *",
    )
    .bind(&input.bonus_custom_headers)
    .bind(sub_id)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    if let Ok(mut conn) = state.redis.get().await {
        let _: Result<(), _> = conn.del(format!("key_subs:{key_id}")).await;
    }

    Ok(Json(sub))
}

async fn update_bonus_non_stream_timeout(
    State(state): State<AppState>,
    Path((_group_id, key_id, sub_id)): Path<(Uuid, Uuid, Uuid)>,
    Json(input): Json<UpdateBonusNonStreamTimeout>,
) -> Result<Json<KeySubscription>, ApiError> {
    let current = sqlx::query_as::<_, KeySubscription>(
        "SELECT * FROM key_subscriptions WHERE id = $1 AND group_key_id = $2",
    )
    .bind(sub_id)
    .bind(key_id)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "Subscription not found"))?;

    if current.sub_type != "bonus" {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Only bonus subscriptions can set a non-streaming timeout",
        ));
    }

    if let Some(ms) = input.bonus_non_stream_timeout_ms
        && ms < 1
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "bonus_non_stream_timeout_ms must be >= 1",
        ));
    }

    let sub = sqlx::query_as::<_, KeySubscription>(
        "UPDATE key_subscriptions SET bonus_non_stream_timeout_ms = $1 WHERE id = $2 RETURNING *",
    )
    .bind(input.bonus_non_stream_timeout_ms)
    .bind(sub_id)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    if let Ok(mut conn) = state.redis.get().await {
        let _: Result<(), _> = conn.del(format!("key_subs:{key_id}")).await;
    }

    Ok(Json(sub))
}

async fn update_bonus_quota_config(
    State(state): State<AppState>,
    Path((_group_id, key_id, sub_id)): Path<(Uuid, Uuid, Uuid)>,
    Json(input): Json<UpdateBonusQuotaConfig>,
) -> Result<Json<KeySubscription>, ApiError> {
    let current = sqlx::query_as::<_, KeySubscription>(
        "SELECT * FROM key_subscriptions WHERE id = $1 AND group_key_id = $2",
    )
    .bind(sub_id)
    .bind(key_id)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "Subscription not found"))?;

    if current.sub_type != "bonus" {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Only bonus subscriptions can update quota config",
        ));
    }

    let sub = sqlx::query_as::<_, KeySubscription>(
        "UPDATE key_subscriptions SET bonus_quota_url = $1, bonus_quota_headers = $2 WHERE id = $3 RETURNING *",
    )
    .bind(&input.bonus_quota_url)
    .bind(&input.bonus_quota_headers)
    .bind(sub_id)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    if let Ok(mut conn) = state.redis.get().await {
        let _: Result<(), _> = conn.del(format!("key_subs:{key_id}")).await;
    }

    Ok(Json(sub))
}

async fn list_subscriptions(
    State(state): State<AppState>,
    Path((_group_id, key_id)): Path<(Uuid, Uuid)>,
    Query(params): Query<ListParams>,
) -> Result<Json<PaginatedResponse<KeySubscriptionWithUsage>>, ApiError> {
    let page = params.page.unwrap_or(1).max(1);
    let limit = params.limit.unwrap_or(10).clamp(1, 100);
    let offset = (page - 1) * limit;

    let (total,) = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM key_subscriptions WHERE group_key_id = $1",
    )
    .bind(key_id)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    let subs = sqlx::query_as::<_, KeySubscription>(
        "SELECT * FROM key_subscriptions WHERE group_key_id = $1 ORDER BY (sub_type = 'bonus') DESC, sort_order ASC, created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(key_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    let mut data = Vec::with_capacity(subs.len());
    for sub in subs {
        let cost_used = crate::subscription::get_total_cost(&state, &sub).await;
        data.push(KeySubscriptionWithUsage { sub, cost_used });
    }

    let total_pages = (total as f64 / limit as f64).ceil() as i64;
    Ok(Json(PaginatedResponse {
        data,
        total,
        page,
        total_pages,
    }))
}

async fn cancel_subscription(
    State(state): State<AppState>,
    Path((_group_id, key_id, sub_id)): Path<(Uuid, Uuid, Uuid)>,
    Json(input): Json<CancelSubscription>,
) -> Result<Json<KeySubscription>, ApiError> {
    if input.status != "cancelled" {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Only 'cancelled' status is supported",
        ));
    }

    let current = sqlx::query_as::<_, KeySubscription>(
        "SELECT * FROM key_subscriptions WHERE id = $1 AND group_key_id = $2",
    )
    .bind(sub_id)
    .bind(key_id)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "Subscription not found"))?;

    if current.status != "active" {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Only active subscriptions can be cancelled",
        ));
    }

    let sub = sqlx::query_as::<_, KeySubscription>(
        "UPDATE key_subscriptions SET status = 'cancelled' WHERE id = $1 RETURNING *",
    )
    .bind(sub_id)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    // Invalidate subscription list cache
    if let Ok(mut conn) = state.redis.get().await {
        let _: Result<(), _> = conn.del(format!("key_subs:{key_id}")).await;
    }

    Ok(Json(sub))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reorder_validation_rejects_duplicate_ids() {
        let b1 = Uuid::new_v4();
        let b2 = Uuid::new_v4();
        let bonus_ids = vec![b1, b2];
        let ordered = vec![b1, b1, b2];
        let result = validate_reorder_set(&ordered, &bonus_ids);
        assert_eq!(result, Err("ordered_ids contains duplicate ids"));
    }

    #[test]
    fn test_reorder_validation_rejects_missing_ids() {
        let b1 = Uuid::new_v4();
        let b2 = Uuid::new_v4();
        let bonus_ids = vec![b1, b2];
        let ordered = vec![b1]; // missing b2
        let result = validate_reorder_set(&ordered, &bonus_ids);
        assert_eq!(
            result,
            Err("ordered_ids must contain every bonus subscription for this key")
        );
    }

    #[test]
    fn test_reorder_validation_rejects_extra_ids() {
        let b1 = Uuid::new_v4();
        let bonus_ids = vec![b1];
        let extra = Uuid::new_v4();
        let ordered = vec![b1, extra]; // extra id not in bonus set
        let result = validate_reorder_set(&ordered, &bonus_ids);
        assert_eq!(
            result,
            Err("ordered_ids must contain every bonus subscription for this key")
        );
    }

    #[test]
    fn test_reorder_validation_rejects_non_bonus_ids() {
        // Non-bonus check happens in the handler before validate_reorder_set,
        // but if a non-bonus id sneaks through, it fails the "not in bonus set" check.
        let b1 = Uuid::new_v4();
        let non_bonus = Uuid::new_v4();
        let bonus_ids = vec![b1];
        let ordered = vec![non_bonus]; // non_bonus is not in bonus_ids
        let result = validate_reorder_set(&ordered, &bonus_ids);
        assert_eq!(
            result,
            Err("ordered_ids contains an id that is not a bonus subscription for this key")
        );
    }

    #[test]
    fn test_reorder_validation_rejects_foreign_key_ids() {
        let b1 = Uuid::new_v4();
        let b2 = Uuid::new_v4();
        let foreign = Uuid::new_v4();
        let bonus_ids = vec![b1, b2];
        let ordered = vec![b1, foreign]; // foreign not in bonus_ids, same length
        let result = validate_reorder_set(&ordered, &bonus_ids);
        assert_eq!(
            result,
            Err("ordered_ids contains an id that is not a bonus subscription for this key")
        );
    }

    #[test]
    fn test_reorder_validation_accepts_correct_full_set() {
        let b1 = Uuid::new_v4();
        let b2 = Uuid::new_v4();
        let b3 = Uuid::new_v4();
        let bonus_ids = vec![b1, b2, b3];
        // Reordered
        let ordered = vec![b3, b1, b2];
        let result = validate_reorder_set(&ordered, &bonus_ids);
        assert_eq!(result, Ok(()));
    }
}
