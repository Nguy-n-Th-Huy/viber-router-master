use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct KeySubscription {
    pub id: Uuid,
    pub group_key_id: Uuid,
    pub plan_id: Option<Uuid>,
    pub sub_type: String,
    pub cost_limit_usd: f64,
    pub weekly_cost_limit_usd: Option<f64>,
    pub model_limits: serde_json::Value,
    pub model_request_costs: serde_json::Value,
    pub reset_hours: Option<i32>,
    pub duration_days: i32,
    pub rpm_limit: Option<f64>,
    pub tpm_limit: Option<f64>,
    pub status: String,
    pub activated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub bonus_base_url: Option<String>,
    pub bonus_api_key: Option<String>,
    pub bonus_name: Option<String>,
    pub bonus_quota_url: Option<String>,
    pub bonus_quota_headers: Option<serde_json::Value>,
    pub bonus_allowed_models: Option<Vec<String>>,
    #[serde(default)]
    pub sort_order: i32,
    pub bonus_custom_headers: Option<serde_json::Value>,
    /// Cap on a non-streaming call to this bonus upstream before moving to the next
    /// waterfall entry. NULL falls back to `settings.default_non_stream_timeout_ms`.
    #[sqlx(default)]
    #[serde(default)]
    pub bonus_non_stream_timeout_ms: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct AssignSubscription {
    pub plan_id: Option<Uuid>,
    pub custom_expires_at: Option<String>,
    pub bonus_name: Option<String>,
    pub bonus_base_url: Option<String>,
    pub bonus_api_key: Option<String>,
    pub bonus_quota_url: Option<String>,
    pub bonus_quota_headers: Option<serde_json::Value>,
    pub bonus_allowed_models: Option<Vec<String>>,
    pub bonus_custom_headers: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct CancelSubscription {
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateBonusSubscription {
    pub bonus_allowed_models: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateBonusCustomHeaders {
    pub bonus_custom_headers: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateBonusQuotaConfig {
    pub bonus_quota_url: Option<String>,
    pub bonus_quota_headers: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateBonusNonStreamTimeout {
    /// `None` clears the per-bonus override so the global default applies.
    pub bonus_non_stream_timeout_ms: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_subscription_deserializes_null_tpm_as_unlimited() {
        let sub: KeySubscription = serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "group_key_id": Uuid::new_v4(),
            "plan_id": Uuid::new_v4(),
            "sub_type": "fixed",
            "cost_limit_usd": 10.0,
            "weekly_cost_limit_usd": null,
            "model_limits": {},
            "model_request_costs": {},
            "reset_hours": null,
            "duration_days": 30,
            "rpm_limit": null,
            "tpm_limit": null,
            "status": "active",
            "activated_at": null,
            "expires_at": null,
            "created_at": chrono::Utc::now(),
            "bonus_base_url": null,
            "bonus_api_key": null,
            "bonus_name": null,
            "bonus_quota_url": null,
            "bonus_quota_headers": null,
            "bonus_allowed_models": null,
            "sort_order": 0
        }))
        .unwrap();

        assert_eq!(sub.tpm_limit, None);
    }

    #[test]
    fn key_subscription_deserializes_tpm_limit_value() {
        let sub: KeySubscription = serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "group_key_id": Uuid::new_v4(),
            "plan_id": Uuid::new_v4(),
            "sub_type": "fixed",
            "cost_limit_usd": 10.0,
            "weekly_cost_limit_usd": null,
            "model_limits": {},
            "model_request_costs": {},
            "reset_hours": null,
            "duration_days": 30,
            "rpm_limit": null,
            "tpm_limit": 100000.0,
            "status": "active",
            "activated_at": null,
            "expires_at": null,
            "created_at": chrono::Utc::now(),
            "bonus_base_url": null,
            "bonus_api_key": null,
            "bonus_name": null,
            "bonus_quota_url": null,
            "bonus_quota_headers": null,
            "bonus_allowed_models": null,
            "sort_order": 0
        }))
        .unwrap();

        assert_eq!(sub.tpm_limit, Some(100000.0));
        assert_eq!(sub.weekly_cost_limit_usd, None);
    }
}
