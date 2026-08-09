use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::models::{
    AdminGroupServerDetail, CreateGroup, Group, GroupListItem, GroupWithServers, Model,
    PaginatedResponse, UpdateGroup, generate_api_key,
};
use crate::routes::AppState;

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub page: Option<i64>,
    pub limit: Option<i64>,
    pub search: Option<String>,
    pub is_active: Option<bool>,
    pub server_id: Option<Uuid>,
    pub order: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BulkIds {
    pub ids: Vec<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct BulkAssignServer {
    pub group_ids: Vec<Uuid>,
    pub server_id: Uuid,
    pub priority: i32,
    pub model_mappings: Option<serde_json::Value>,
}

type ApiError = (StatusCode, Json<serde_json::Value>);

fn err(status: StatusCode, msg: &str) -> ApiError {
    (status, Json(serde_json::json!({"error": msg})))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_groups).post(create_group))
        .route(
            "/{id}",
            get(get_group).put(update_group).delete(delete_group),
        )
        .route("/{id}/regenerate-key", post(regenerate_key))
        .route("/{id}/circuit-status", get(circuit_status))
        .route("/bulk/activate", post(bulk_activate))
        .route("/bulk/deactivate", post(bulk_deactivate))
        .route("/bulk/delete", post(bulk_delete))
        .route("/bulk/assign-server", post(bulk_assign_server))
        .nest("/{group_id}/servers", super::group_servers::router())
        .nest("/{group_id}/keys", super::group_keys::router())
        .nest(
            "/{group_id}/allowed-models",
            super::group_allowed_models::router(),
        )
        .nest("/{group_id}/uptime", super::uptime::router())
        .nest(
            "/{group_id}/user-agents",
            super::group_user_agents::router(),
        )
}

async fn create_group(
    State(state): State<AppState>,
    Json(input): Json<CreateGroup>,
) -> Result<(StatusCode, Json<Group>), ApiError> {
    let api_key = generate_api_key();
    let codes = input
        .failover_status_codes
        .unwrap_or_else(|| vec![429, 500, 502, 503]);
    let codes_json = serde_json::to_value(&codes).map_err(internal)?;

    let group = sqlx::query_as::<_, Group>(
        "INSERT INTO groups (name, api_key, failover_status_codes) VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(&input.name)
    .bind(&api_key)
    .bind(&codes_json)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    Ok((StatusCode::CREATED, Json(group)))
}

async fn list_groups(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> Result<Json<PaginatedResponse<GroupListItem>>, ApiError> {
    let page = params.page.unwrap_or(1).max(1);
    let limit = params.limit.unwrap_or(20).clamp(1, 100);
    let offset = (page - 1) * limit;
    let order = if params.order.as_deref() == Some("asc") {
        "ASC"
    } else {
        "DESC"
    };

    let mut conditions = vec!["1=1".to_string()];
    let mut bind_idx = 0u32;

    // Build dynamic query
    let search_pattern = params.search.as_ref().map(|s| format!("%{s}%"));
    if search_pattern.is_some() {
        bind_idx += 1;
        conditions.push(format!("g.name ILIKE ${bind_idx}"));
    }
    if params.is_active.is_some() {
        bind_idx += 1;
        conditions.push(format!("g.is_active = ${bind_idx}"));
    }
    if params.server_id.is_some() {
        bind_idx += 1;
        conditions.push(format!(
            "EXISTS (SELECT 1 FROM group_servers gs WHERE gs.group_id = g.id AND gs.server_id = ${bind_idx})"
        ));
    }

    let where_clause = conditions.join(" AND ");
    let count_sql = format!("SELECT COUNT(*) FROM groups g WHERE {where_clause}");
    let data_sql = format!(
        "SELECT g.*, COALESCE((SELECT COUNT(*) FROM group_servers gs2 WHERE gs2.group_id = g.id), 0) as servers_count \
         FROM groups g WHERE {where_clause} ORDER BY g.created_at {order} LIMIT ${} OFFSET ${}",
        bind_idx + 1,
        bind_idx + 2
    );

    // Use raw query builder to bind dynamically
    let mut count_query = sqlx::query_as::<_, (i64,)>(&count_sql);
    let mut data_query = sqlx::query_as::<_, GroupListItem>(&data_sql);

    if let Some(ref pattern) = search_pattern {
        count_query = count_query.bind(pattern);
        data_query = data_query.bind(pattern);
    }
    if let Some(is_active) = params.is_active {
        count_query = count_query.bind(is_active);
        data_query = data_query.bind(is_active);
    }
    if let Some(server_id) = params.server_id {
        count_query = count_query.bind(server_id);
        data_query = data_query.bind(server_id);
    }

    data_query = data_query.bind(limit).bind(offset);

    let (total,) = count_query.fetch_one(&state.db).await.map_err(internal)?;
    let groups = data_query.fetch_all(&state.db).await.map_err(internal)?;
    let total_pages = (total as f64 / limit as f64).ceil() as i64;

    Ok(Json(PaginatedResponse {
        data: groups,
        total,
        page,
        total_pages,
    }))
}

async fn get_group(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<GroupWithServers>, ApiError> {
    let group = sqlx::query_as::<_, Group>("SELECT * FROM groups WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Group not found"))?;

    let unlocked = state.unlocked_servers.read().await;

    let raw_servers: Vec<AdminGroupServerDetail> = sqlx::query_as::<_, AdminGroupServerDetail>(
        "SELECT gs.server_id, s.short_id, s.name as server_name, s.base_url, s.api_key, gs.priority, gs.model_mappings, gs.is_enabled, \
         gs.cb_max_failures, gs.cb_window_seconds, gs.cb_cooldown_seconds, \
         gs.rate_input, gs.rate_output, gs.rate_cache_write, gs.rate_cache_read, \
         gs.max_requests, gs.rate_window_seconds, gs.normalize_cache_read, gs.max_input_tokens, gs.min_input_tokens, gs.supported_models, s.password_hash, \
         gs.per_key_max_requests, gs.per_key_rate_window_seconds, \
         gs.active_hours_start, gs.active_hours_end, gs.active_hours_timezone, \
         gs.retry_status_codes, gs.retry_count, gs.retry_delay_seconds, \
         s.custom_headers, gs.non_stream_timeout_ms \
         FROM group_servers gs JOIN servers s ON s.id = gs.server_id \
         WHERE gs.group_id = $1 ORDER BY gs.priority",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    // Mask credentials for locked servers
    let servers: Vec<AdminGroupServerDetail> = raw_servers
        .into_iter()
        .map(|mut s| {
            let is_locked = s.password_hash.is_some() && !unlocked.contains(&s.server_id);
            if is_locked {
                s.base_url = None;
                s.api_key = None;
            }
            s
        })
        .collect();

    let allowed_models = sqlx::query_as::<_, Model>(
        "SELECT m.* FROM models m \
         JOIN group_allowed_models gam ON m.id = gam.model_id \
         WHERE gam.group_id = $1 ORDER BY m.name",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    Ok(Json(GroupWithServers {
        group,
        servers,
        allowed_models,
    }))
}

#[derive(Debug, serde::Serialize)]
struct CircuitStatusEntry {
    server_id: Uuid,
    model: String,
    remaining_seconds: i64,
    /// "open" (cooling down) or "half_open" (probing for recovery)
    state: String,
}

async fn circuit_status(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<CircuitStatusEntry>>, ApiError> {
    // Get all servers with CB configured for this group
    let rows = sqlx::query_as::<_, (Uuid,)>(
        "SELECT server_id FROM group_servers WHERE group_id = $1 AND cb_max_failures IS NOT NULL",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    let mut entries = Vec::new();
    if let Ok(mut conn) = state.redis.get().await {
        for (server_id,) in rows {
            let open_prefix = format!("cb:open:{id}:{server_id}:");
            let mut open_slots = Vec::new();
            let keys: Vec<String> = deadpool_redis::redis::cmd("KEYS")
                .arg(format!("{open_prefix}*"))
                .query_async(&mut conn)
                .await
                .unwrap_or_default();
            for key in keys {
                let ttl: i64 = deadpool_redis::redis::cmd("TTL")
                    .arg(&key)
                    .query_async(&mut conn)
                    .await
                    .unwrap_or(-2);
                if ttl <= 0 {
                    continue;
                }
                let model = key.strip_prefix(&open_prefix).unwrap_or("_any").to_string();
                open_slots.push(model.clone());
                entries.push(CircuitStatusEntry {
                    server_id,
                    model,
                    remaining_seconds: ttl,
                    state: "open".to_string(),
                });
            }

            // Half-open slots: half marker present without an open key
            let half_prefix = format!("cb:half:{id}:{server_id}:");
            let half_keys: Vec<String> = deadpool_redis::redis::cmd("KEYS")
                .arg(format!("{half_prefix}*"))
                .query_async(&mut conn)
                .await
                .unwrap_or_default();
            for key in half_keys {
                let model = key.strip_prefix(&half_prefix).unwrap_or("_any").to_string();
                if open_slots.contains(&model) {
                    continue; // still cooling down — reported as "open" above
                }
                let ttl: i64 = deadpool_redis::redis::cmd("TTL")
                    .arg(&key)
                    .query_async(&mut conn)
                    .await
                    .unwrap_or(-2);
                if ttl <= 0 {
                    continue;
                }
                entries.push(CircuitStatusEntry {
                    server_id,
                    model,
                    remaining_seconds: ttl,
                    state: "half_open".to_string(),
                });
            }
        }
    }

    Ok(Json(entries))
}

async fn update_group(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(input): Json<UpdateGroup>,
) -> Result<Json<Group>, ApiError> {
    let codes_json = input
        .failover_status_codes
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(internal)?;

    // For ttft_timeout_ms: None means "don't change", Some(None) means "set to NULL", Some(Some(v)) means "set to v"
    let (update_ttft, ttft_val) = match input.ttft_timeout_ms {
        Some(v) => (true, v),
        None => (false, None),
    };

    // Same pattern for count_tokens_server_id
    let (update_ct_server, ct_server_val) = match input.count_tokens_server_id {
        Some(v) => (true, v),
        None => (false, None),
    };

    // For count_tokens_model_mappings: None means "don't change", Some(v) means "set to v"
    let update_ct_mappings = input.count_tokens_model_mappings.is_some();
    let ct_mappings_val = input.count_tokens_model_mappings.unwrap_or_default();

    let group = sqlx::query_as::<_, Group>(
        "UPDATE groups SET \
         name = COALESCE($1, name), \
         failover_status_codes = COALESCE($2, failover_status_codes), \
         is_active = COALESCE($3, is_active), \
         ttft_timeout_ms = CASE WHEN $5 THEN $6 ELSE ttft_timeout_ms END, \
         count_tokens_server_id = CASE WHEN $7 THEN $8 ELSE count_tokens_server_id END, \
         count_tokens_model_mappings = CASE WHEN $9 THEN $10 ELSE count_tokens_model_mappings END, \
         updated_at = now() \
         WHERE id = $4 RETURNING *",
    )
    .bind(&input.name)
    .bind(&codes_json)
    .bind(input.is_active)
    .bind(id)
    .bind(update_ttft)
    .bind(ttft_val)
    .bind(update_ct_server)
    .bind(ct_server_val)
    .bind(update_ct_mappings)
    .bind(&ct_mappings_val)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "Group not found"))?;

    crate::cache::invalidate_group_all_keys(&state.redis, &state.db, id).await;
    Ok(Json(group))
}

async fn delete_group(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    // Verify group exists
    let exists = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM groups WHERE id = $1)")
        .bind(id)
        .fetch_one(&state.db)
        .await
        .map_err(internal)?;
    if !exists {
        return Err(err(StatusCode::NOT_FOUND, "Group not found"));
    }

    // Invalidate before delete (sub-keys will be cascade-deleted)
    crate::cache::invalidate_group_all_keys(&state.redis, &state.db, id).await;

    sqlx::query("DELETE FROM groups WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(internal)?;

    Ok(StatusCode::NO_CONTENT)
}

async fn regenerate_key(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Group>, ApiError> {
    let old_group = sqlx::query_as::<_, Group>("SELECT * FROM groups WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Group not found"))?;

    let new_key = generate_api_key();
    let group = sqlx::query_as::<_, Group>(
        "UPDATE groups SET api_key = $1, updated_at = now() WHERE id = $2 RETURNING *",
    )
    .bind(&new_key)
    .bind(id)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    // Invalidate old master key + all sub-keys (sub-keys cache the same GroupConfig)
    crate::cache::invalidate_group_config(&state.redis, &old_group.api_key).await;
    crate::cache::invalidate_group_all_keys(&state.redis, &state.db, id).await;
    Ok(Json(group))
}

async fn bulk_activate(
    State(state): State<AppState>,
    Json(input): Json<BulkIds>,
) -> Result<StatusCode, ApiError> {
    let groups = sqlx::query_as::<_, Group>(
        "UPDATE groups SET is_active = true, updated_at = now() WHERE id = ANY($1) RETURNING *",
    )
    .bind(&input.ids)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    for g in &groups {
        crate::cache::invalidate_group_all_keys(&state.redis, &state.db, g.id).await;
    }
    Ok(StatusCode::OK)
}

async fn bulk_deactivate(
    State(state): State<AppState>,
    Json(input): Json<BulkIds>,
) -> Result<StatusCode, ApiError> {
    let groups = sqlx::query_as::<_, Group>(
        "UPDATE groups SET is_active = false, updated_at = now() WHERE id = ANY($1) RETURNING *",
    )
    .bind(&input.ids)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    for g in &groups {
        crate::cache::invalidate_group_all_keys(&state.redis, &state.db, g.id).await;
    }
    Ok(StatusCode::OK)
}

async fn bulk_delete(
    State(state): State<AppState>,
    Json(input): Json<BulkIds>,
) -> Result<StatusCode, ApiError> {
    let groups = sqlx::query_as::<_, Group>("SELECT * FROM groups WHERE id = ANY($1)")
        .bind(&input.ids)
        .fetch_all(&state.db)
        .await
        .map_err(internal)?;

    // Invalidate before delete
    for g in &groups {
        crate::cache::invalidate_group_all_keys(&state.redis, &state.db, g.id).await;
    }

    sqlx::query("DELETE FROM groups WHERE id = ANY($1)")
        .bind(&input.ids)
        .execute(&state.db)
        .await
        .map_err(internal)?;

    Ok(StatusCode::OK)
}

async fn bulk_assign_server(
    State(state): State<AppState>,
    Json(input): Json<BulkAssignServer>,
) -> Result<StatusCode, ApiError> {
    let mappings = input.model_mappings.unwrap_or(serde_json::json!({}));

    for group_id in &input.group_ids {
        sqlx::query(
            "INSERT INTO group_servers (group_id, server_id, priority, model_mappings, is_enabled) \
             VALUES ($1, $2, $3, $4, true) \
             ON CONFLICT (group_id, server_id) DO UPDATE SET priority = $3, model_mappings = $4, is_enabled = true",
        )
        .bind(group_id)
        .bind(input.server_id)
        .bind(input.priority)
        .bind(&mappings)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    }

    // Invalidate cache for all affected groups
    for group_id in &input.group_ids {
        crate::cache::invalidate_group_all_keys(&state.redis, &state.db, *group_id).await;
    }
    Ok(StatusCode::OK)
}
