use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::cache;
use crate::models::{
    CreateUserEndpoint, EndpointModelUsage, EndpointQuotaInfo, PublicUserEndpoint,
    UpdateUserEndpoint, UserEndpoint, json_object_or_default, validate_optional_json_object,
    validate_priority_mode,
};
use crate::routes::AppState;

const MAX_USER_ENDPOINTS: i64 = 10;

#[derive(Debug, Deserialize)]
pub struct PublicKeyParams {
    key: Option<String>,
}

impl PublicKeyParams {
    fn validated_key(&self) -> Result<&str, (StatusCode, Json<serde_json::Value>)> {
        self.key
            .as_deref()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "key parameter is required"))
    }
}

#[derive(sqlx::FromRow)]
struct KeyInfo {
    key_id: Uuid,
}

fn err(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({"error": msg})))
}

async fn resolve_active_key(
    state: &AppState,
    key: Option<String>,
) -> Result<KeyInfo, (StatusCode, Json<serde_json::Value>)> {
    let params = PublicKeyParams { key };
    let key = params.validated_key()?;

    match sqlx::query_as::<_, KeyInfo>(
        "SELECT id as key_id FROM group_keys WHERE api_key = $1 AND is_active = true",
    )
    .bind(key)
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(info)) => Ok(info),
        Ok(None) => Err(err(StatusCode::FORBIDDEN, "Invalid or inactive key")),
        Err(_) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")),
    }
}

pub async fn list_user_endpoints(
    State(state): State<AppState>,
    Query(params): Query<PublicKeyParams>,
) -> impl IntoResponse {
    let key_info = match resolve_active_key(&state, params.key).await {
        Ok(info) => info,
        Err(response) => return response.into_response(),
    };

    match list_endpoint_responses(&state, key_info.key_id).await {
        Ok(endpoints) => Json(endpoints).into_response(),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response(),
    }
}

pub async fn create_user_endpoint(
    State(state): State<AppState>,
    Query(params): Query<PublicKeyParams>,
    Json(input): Json<CreateUserEndpoint>,
) -> impl IntoResponse {
    let key_info = match resolve_active_key(&state, params.key).await {
        Ok(info) => info,
        Err(response) => return response.into_response(),
    };

    if input.name.trim().is_empty() || input.base_url.trim().is_empty() || input.api_key.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "name, base_url, and api_key are required").into_response();
    }

    let count: i64 = match sqlx::query_scalar("SELECT COUNT(*) FROM user_endpoints WHERE group_key_id = $1")
        .bind(key_info.key_id)
        .fetch_one(&state.db)
        .await
    {
        Ok(count) => count,
        Err(error) => {
            tracing::error!(%error, "Failed to count user endpoints before create");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response();
        }
    };
    if count >= MAX_USER_ENDPOINTS {
        return err(StatusCode::BAD_REQUEST, "Maximum of 10 user endpoints reached").into_response();
    }

    let model_mappings = match json_object_or_default(input.model_mappings) {
        Ok(value) => value,
        Err(msg) => return err(StatusCode::BAD_REQUEST, &format!("model_mappings {msg}")).into_response(),
    };
    if let Some(headers) = &input.quota_headers
        && let Err(msg) = validate_optional_json_object(&Some(headers.clone()))
    {
        return err(StatusCode::BAD_REQUEST, &format!("quota_headers {msg}")).into_response();
    }
    if let Some(headers) = &input.custom_headers
        && let Err(msg) = validate_optional_json_object(&Some(headers.clone()))
    {
        return err(StatusCode::BAD_REQUEST, &format!("custom_headers {msg}")).into_response();
    }
    let priority_mode = input.priority_mode.unwrap_or_else(|| "fallback".to_string());
    if !validate_priority_mode(&priority_mode) {
        return err(StatusCode::BAD_REQUEST, "priority_mode must be priority or fallback").into_response();
    }

    let created = sqlx::query_as::<_, UserEndpoint>(
        "INSERT INTO user_endpoints (group_key_id, name, base_url, api_key, model_mappings, quota_url, quota_headers, custom_headers, priority_mode) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING *",
    )
    .bind(key_info.key_id)
    .bind(input.name.trim())
    .bind(input.base_url.trim())
    .bind(input.api_key.trim())
    .bind(model_mappings)
    .bind(input.quota_url.and_then(non_empty_string))
    .bind(input.quota_headers)
    .bind(input.custom_headers)
    .bind(priority_mode)
    .fetch_one(&state.db)
    .await;

    match created {
        Ok(endpoint) => {
            cache::invalidate_user_endpoints(&state.redis, key_info.key_id).await;
            let response = enrich_endpoint(&state, endpoint).await;
            (StatusCode::CREATED, Json(response)).into_response()
        }
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response(),
    }
}

pub async fn patch_user_endpoint(
    State(state): State<AppState>,
    Query(params): Query<PublicKeyParams>,
    Path(id): Path<Uuid>,
    Json(input): Json<UpdateUserEndpoint>,
) -> impl IntoResponse {
    let key_info = match resolve_active_key(&state, params.key).await {
        Ok(info) => info,
        Err(response) => return response.into_response(),
    };

    if let Some(value) = &input.model_mappings
        && !value.is_object()
    {
        return err(StatusCode::BAD_REQUEST, "model_mappings must be a JSON object").into_response();
    }
    if let Some(Some(value)) = &input.quota_headers
        && !value.is_object()
    {
        return err(StatusCode::BAD_REQUEST, "quota_headers must be a JSON object").into_response();
    }
    if let Some(Some(value)) = &input.custom_headers
        && !value.is_object()
    {
        return err(StatusCode::BAD_REQUEST, "custom_headers must be a JSON object").into_response();
    }
    if let Some(mode) = &input.priority_mode
        && !validate_priority_mode(mode)
    {
        return err(StatusCode::BAD_REQUEST, "priority_mode must be priority or fallback").into_response();
    }

    let current = match sqlx::query_as::<_, UserEndpoint>(
        "SELECT * FROM user_endpoints WHERE id = $1 AND group_key_id = $2",
    )
    .bind(id)
    .bind(key_info.key_id)
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(endpoint)) => endpoint,
        Ok(None) => return err(StatusCode::NOT_FOUND, "Endpoint not found").into_response(),
        Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response(),
    };

    let name = input.name.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&current.name).to_string();
    let base_url = input.base_url.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&current.base_url).to_string();
    let api_key = input.api_key.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&current.api_key).to_string();
    let model_mappings = input.model_mappings.unwrap_or(current.model_mappings);
    let quota_url = match input.quota_url {
        Some(value) => value.and_then(non_empty_string),
        None => current.quota_url,
    };
    let quota_headers = input.quota_headers.unwrap_or(current.quota_headers);
    let custom_headers = input.custom_headers.unwrap_or(current.custom_headers);
    let priority_mode = input.priority_mode.unwrap_or(current.priority_mode);
    let is_enabled = input.is_enabled.unwrap_or(current.is_enabled);
    let non_stream_timeout_ms = match input.non_stream_timeout_ms {
        Some(value) => value,
        None => current.non_stream_timeout_ms,
    };
    if let Some(ms) = non_stream_timeout_ms
        && ms < 1
    {
        return err(
            StatusCode::BAD_REQUEST,
            "non_stream_timeout_ms must be >= 1",
        )
        .into_response();
    }

    let updated = sqlx::query_as::<_, UserEndpoint>(
        "UPDATE user_endpoints SET name = $3, base_url = $4, api_key = $5, model_mappings = $6, \
         quota_url = $7, quota_headers = $8, custom_headers = $9, priority_mode = $10, is_enabled = $11, \
         non_stream_timeout_ms = $12, updated_at = now() \
         WHERE id = $1 AND group_key_id = $2 RETURNING *",
    )
    .bind(id)
    .bind(key_info.key_id)
    .bind(name)
    .bind(base_url)
    .bind(api_key)
    .bind(model_mappings)
    .bind(quota_url)
    .bind(quota_headers)
    .bind(custom_headers)
    .bind(priority_mode)
    .bind(is_enabled)
    .bind(non_stream_timeout_ms)
    .fetch_one(&state.db)
    .await;

    match updated {
        Ok(endpoint) => {
            cache::invalidate_user_endpoints(&state.redis, key_info.key_id).await;
            Json(enrich_endpoint(&state, endpoint).await).into_response()
        }
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response(),
    }
}

pub async fn delete_user_endpoint(
    State(state): State<AppState>,
    Query(params): Query<PublicKeyParams>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let key_info = match resolve_active_key(&state, params.key).await {
        Ok(info) => info,
        Err(response) => return response.into_response(),
    };

    let deleted = sqlx::query("DELETE FROM user_endpoints WHERE id = $1 AND group_key_id = $2")
        .bind(id)
        .bind(key_info.key_id)
        .execute(&state.db)
        .await;

    match deleted {
        Ok(result) if result.rows_affected() > 0 => {
            cache::invalidate_user_endpoints(&state.redis, key_info.key_id).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(_) => err(StatusCode::NOT_FOUND, "Endpoint not found").into_response(),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response(),
    }
}

pub async fn list_endpoint_responses(
    state: &AppState,
    group_key_id: Uuid,
) -> Result<Vec<PublicUserEndpoint>, sqlx::Error> {
    let endpoints = sqlx::query_as::<_, UserEndpoint>(
        "SELECT * FROM user_endpoints WHERE group_key_id = $1 ORDER BY created_at ASC",
    )
    .bind(group_key_id)
    .fetch_all(&state.db)
    .await?;

    let mut responses = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        responses.push(enrich_endpoint(state, endpoint).await);
    }
    Ok(responses)
}

async fn enrich_endpoint(state: &AppState, endpoint: UserEndpoint) -> PublicUserEndpoint {
    let usage = endpoint_usage(state, endpoint.id).await;
    let quotas = if let Some(quota_url) = endpoint.quota_url.as_ref() {
        fetch_endpoint_quotas(state, quota_url, endpoint.quota_headers.as_ref()).await
    } else {
        None
    };

    let mut response = PublicUserEndpoint::from(endpoint);
    response.usage = usage;
    response.quotas = quotas;
    response
}

async fn endpoint_usage(state: &AppState, endpoint_id: Uuid) -> Vec<EndpointModelUsage> {
    let rows: Vec<(Option<String>, i64, f64)> = sqlx::query_as(
        "SELECT model, COUNT(*)::bigint as request_count, COALESCE(SUM(cost_usd), 0)::float8 as cost_usd \
         FROM token_usage_logs \
         WHERE user_endpoint_id = $1 AND created_at >= now() - interval '30 days' \
         GROUP BY model ORDER BY model",
    )
    .bind(endpoint_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    rows.into_iter()
        .filter_map(|(model, request_count, cost_usd)| {
            model.map(|model| EndpointModelUsage {
                model,
                request_count,
                cost_usd,
            })
        })
        .collect()
}

pub async fn fetch_endpoint_quotas(
    state: &AppState,
    quota_url: &str,
    quota_headers: Option<&serde_json::Value>,
) -> Option<Vec<EndpointQuotaInfo>> {
    let mut req = state
        .http_client
        .get(quota_url)
        .timeout(std::time::Duration::from_secs(5));

    if let Some(headers_val) = quota_headers
        && let Some(obj) = headers_val.as_object()
    {
        for (key, val) in obj {
            if let Some(val_str) = val.as_str()
                && let (Ok(header_name), Ok(header_val)) = (
                    reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                    reqwest::header::HeaderValue::from_str(val_str),
                )
            {
                req = req.header(header_name, header_val);
            }
        }
    }

    let resp = match req.send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(_) | Err(_) => return Some(vec![]),
    };

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return Some(vec![]),
    };

    let quotas_arr = match body.get("quotas").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return Some(vec![]),
    };

    Some(
        quotas_arr
            .iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?.to_string();
                let utilization = item.get("utilization")?.as_f64()?;
                let reset_at = item
                    .get("reset_at")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let description = item
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                Some(EndpointQuotaInfo {
                    name,
                    utilization,
                    reset_at,
                    description,
                })
            })
            .collect(),
    )
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{json_object_or_default, validate_priority_mode};

    #[test]
    fn max_user_endpoints_is_ten() {
        assert_eq!(MAX_USER_ENDPOINTS, 10);
    }

    #[test]
    fn validated_key_rejects_missing_key() {
        let params = PublicKeyParams { key: None };

        assert!(params.validated_key().is_err());
    }

    #[test]
    fn validated_key_rejects_empty_key() {
        let params = PublicKeyParams {
            key: Some("   ".to_string()),
        };

        assert!(params.validated_key().is_err());
    }

    #[test]
    fn validated_key_accepts_non_empty_key() {
        let params = PublicKeyParams {
            key: Some("sub-key".to_string()),
        };

        assert_eq!(params.validated_key().unwrap(), "sub-key");
    }

    #[test]
    fn model_mappings_defaults_to_empty_object() {
        assert_eq!(json_object_or_default(None).unwrap(), serde_json::json!({}));
    }

    #[test]
    fn model_mappings_accepts_json_object() {
        let mappings = serde_json::json!({"claude-source": "provider-target"});

        assert_eq!(json_object_or_default(Some(mappings.clone())).unwrap(), mappings);
    }

    #[test]
    fn model_mappings_rejects_non_object_json() {
        assert!(json_object_or_default(Some(serde_json::json!(["not", "object"]))).is_err());
    }

    #[test]
    fn priority_mode_accepts_priority_and_fallback() {
        assert!(validate_priority_mode("priority"));
        assert!(validate_priority_mode("fallback"));
    }

    #[test]
    fn priority_mode_rejects_other_values() {
        assert!(!validate_priority_mode("primary"));
        assert!(!validate_priority_mode(""));
    }
}
