use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct UserEndpoint {
    pub id: Uuid,
    pub group_key_id: Uuid,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub model_mappings: serde_json::Value,
    pub quota_url: Option<String>,
    pub quota_headers: Option<serde_json::Value>,
    pub custom_headers: Option<serde_json::Value>,
    pub priority_mode: String,
    pub is_enabled: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Cap on a non-streaming call to this endpoint before moving to the next waterfall
    /// entry. NULL falls back to `settings.default_non_stream_timeout_ms`.
    #[sqlx(default)]
    #[serde(default)]
    pub non_stream_timeout_ms: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct CreateUserEndpoint {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub model_mappings: Option<serde_json::Value>,
    pub quota_url: Option<String>,
    pub quota_headers: Option<serde_json::Value>,
    pub custom_headers: Option<serde_json::Value>,
    pub priority_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateUserEndpoint {
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model_mappings: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "crate::serde_utils::double_option")]
    pub quota_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::serde_utils::double_option")]
    pub quota_headers: Option<Option<serde_json::Value>>,
    #[serde(default, deserialize_with = "crate::serde_utils::double_option")]
    pub custom_headers: Option<Option<serde_json::Value>>,
    pub priority_mode: Option<String>,
    pub is_enabled: Option<bool>,
    #[serde(default, deserialize_with = "crate::serde_utils::double_option")]
    pub non_stream_timeout_ms: Option<Option<i32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointModelUsage {
    pub model: String,
    pub request_count: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointQuotaInfo {
    pub name: String,
    pub utilization: f64,
    pub reset_at: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicUserEndpoint {
    pub id: Uuid,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub model_mappings: serde_json::Value,
    pub quota_url: Option<String>,
    pub quota_headers: Option<serde_json::Value>,
    pub custom_headers: Option<serde_json::Value>,
    pub priority_mode: String,
    pub is_enabled: bool,
    pub quotas: Option<Vec<EndpointQuotaInfo>>,
    pub usage: Vec<EndpointModelUsage>,
}

impl From<UserEndpoint> for PublicUserEndpoint {
    fn from(endpoint: UserEndpoint) -> Self {
        Self {
            id: endpoint.id,
            name: endpoint.name,
            base_url: endpoint.base_url,
            api_key: endpoint.api_key,
            model_mappings: endpoint.model_mappings,
            quota_url: endpoint.quota_url,
            quota_headers: endpoint.quota_headers,
            custom_headers: endpoint.custom_headers,
            priority_mode: endpoint.priority_mode,
            is_enabled: endpoint.is_enabled,
            quotas: None,
            usage: vec![],
        }
    }
}

pub fn json_object_or_default(value: Option<serde_json::Value>) -> Result<serde_json::Value, &'static str> {
    match value {
        Some(value) if value.is_object() => Ok(value),
        Some(_) => Err("must be a JSON object"),
        None => Ok(serde_json::json!({})),
    }
}

pub fn validate_optional_json_object(value: &Option<serde_json::Value>) -> Result<(), &'static str> {
    match value {
        Some(value) if !value.is_object() => Err("must be a JSON object"),
        _ => Ok(()),
    }
}

pub fn validate_priority_mode(mode: &str) -> bool {
    matches!(mode, "priority" | "fallback")
}

pub fn endpoint_accepts_model(endpoint: &UserEndpoint, request_model: Option<&str>) -> bool {
    if !endpoint.is_enabled {
        return false;
    }

    mappings_accept_model(&endpoint.model_mappings, request_model)
}

pub fn mappings_accept_model(model_mappings: &serde_json::Value, request_model: Option<&str>) -> bool {
    let Some(obj) = model_mappings.as_object() else {
        return true;
    };
    if obj.is_empty() {
        return true;
    }
    request_model.is_some_and(|model| obj.contains_key(model))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(model_mappings: serde_json::Value, is_enabled: bool) -> UserEndpoint {
        UserEndpoint {
            id: Uuid::new_v4(),
            group_key_id: Uuid::new_v4(),
            name: "Endpoint".to_string(),
            base_url: "https://example.test".to_string(),
            api_key: "secret".to_string(),
            model_mappings,
            quota_url: None,
            quota_headers: None,
            custom_headers: None,
            priority_mode: "fallback".to_string(),
            is_enabled,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            non_stream_timeout_ms: None,
        }
    }

    #[test]
    fn empty_mappings_accept_any_model() {
        let ep = endpoint(serde_json::json!({}), true);
        assert!(endpoint_accepts_model(&ep, Some("gpt-4o")));
    }

    #[test]
    fn mapping_key_accepts_source_model() {
        let ep = endpoint(serde_json::json!({"claude-3-5-sonnet": "provider-sonnet"}), true);
        assert!(endpoint_accepts_model(&ep, Some("claude-3-5-sonnet")));
    }

    #[test]
    fn mapping_without_source_model_rejects() {
        let ep = endpoint(serde_json::json!({"claude-3-5-sonnet": "provider-sonnet"}), true);
        assert!(!endpoint_accepts_model(&ep, Some("gpt-4o")));
    }

    #[test]
    fn disabled_endpoint_is_rejected() {
        let ep = endpoint(serde_json::json!({}), false);
        assert!(!endpoint_accepts_model(&ep, Some("gpt-4o")));
    }

    #[test]
    fn mapped_model_can_be_read_by_transform_request_body() {
        let mappings = serde_json::json!({"source-model": "target-model"});
        assert!(mappings_accept_model(&mappings, Some("source-model")));
        assert_eq!(mappings["source-model"], "target-model");
    }
}
