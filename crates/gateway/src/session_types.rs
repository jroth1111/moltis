//! Typed parameter structs for session RPC methods.
//!
//! Complex handlers keep parameter semantics here (defaults, null-vs-absent,
//! precedence logic, and legacy aliases). Session methods parse via
//! `parse_params(...)` and avoid ad-hoc JSON traversal.

use serde::Deserialize;
use serde_json::Value;

use crate::services::ServiceError;

/// Params for `session.patch`.
///
/// All fields except `key` are optional — only provided fields are updated.
///
/// Fields that can be cleared (set to null) use `Option<Option<String>>`:
/// - outer `None` → field was absent from the request (no-op)
/// - `Some(None)` → field was explicitly `null` (clear it)
/// - `Some(Some(v))` → field was set to value `v`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchParams {
    pub key: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default, deserialize_with = "double_option", alias = "project_id")]
    pub project_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option", alias = "worktree_branch")]
    pub worktree_branch: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option", alias = "sandbox_image")]
    pub sandbox_image: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option", alias = "mcp_disabled")]
    pub mcp_disabled: Option<Option<bool>>,
    #[serde(default, deserialize_with = "double_option", alias = "sandbox_enabled")]
    pub sandbox_enabled: Option<Option<bool>>,
}

/// Deserialize a field as `Some(inner)` when present (even if null),
/// vs `None` when absent (via `#[serde(default)]`).
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<T>::deserialize(deserializer)?))
}

/// Human-readable JSON type name for diagnostic logging.
fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Lossy optional string: non-string values are treated as absent.
fn optional_string_lossy<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<Value>::deserialize(deserializer)?;
    Ok(raw.and_then(|value| {
        if let Some(s) = value.as_str() {
            Some(s.to_owned())
        } else {
            tracing::debug!(
                actual_type = value_type_name(&value),
                "ignoring wrong-type field in session params, expected string"
            );
            None
        }
    }))
}

/// Lossy optional bool: non-bool values are treated as absent.
fn optional_bool_lossy<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<Value>::deserialize(deserializer)?;
    Ok(raw.and_then(|value| {
        if let Some(b) = value.as_bool() {
            Some(b)
        } else {
            tracing::debug!(
                actual_type = value_type_name(&value),
                "ignoring wrong-type field in session params, expected bool"
            );
            None
        }
    }))
}

/// Lossy optional u64: non-u64 values are treated as absent.
fn optional_u64_lossy<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<Value>::deserialize(deserializer)?;
    Ok(raw.and_then(|value| {
        if let Some(n) = value.as_u64() {
            Some(n)
        } else {
            tracing::debug!(
                actual_type = value_type_name(&value),
                "ignoring wrong-type field in session params, expected u64"
            );
            None
        }
    }))
}

/// Params for handlers that only need a session `key`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionKeyParams {
    pub key: String,
}

/// Params for `session.preview`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewParams {
    pub key: String,
    #[serde(default, deserialize_with = "optional_u64_lossy")]
    pub limit: Option<u64>,
}

impl PreviewParams {
    #[must_use]
    pub fn limit_or_default(&self) -> usize {
        self.limit.unwrap_or(5) as usize
    }
}

/// Params for `session.resolve`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveParams {
    pub key: String,
    #[serde(
        default,
        alias = "inherit_agent_from",
        deserialize_with = "optional_string_lossy"
    )]
    pub inherit_agent_from: Option<String>,
}

impl ResolveParams {
    #[must_use]
    pub fn inherit_from_key(&self) -> Option<&str> {
        self.inherit_agent_from
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

/// Params for `session.share_create`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareCreateParams {
    pub key: String,
    #[serde(default, deserialize_with = "optional_string_lossy")]
    pub visibility: Option<String>,
}

/// Params for `session.share_revoke`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareRevokeParams {
    pub id: String,
}

/// Params for `session.delete`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteParams {
    pub key: String,
    #[serde(default, deserialize_with = "optional_bool_lossy")]
    pub force: Option<bool>,
}

impl DeleteParams {
    #[must_use]
    pub fn force_or_default(&self) -> bool {
        self.force.unwrap_or(false)
    }
}

/// Params for `session.fork`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkParams {
    pub key: String,
    #[serde(default, deserialize_with = "optional_string_lossy")]
    pub label: Option<String>,
    #[serde(default, alias = "fork_point", deserialize_with = "optional_u64_lossy")]
    pub fork_point: Option<u64>,
}

/// Params for `session.search`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchParams {
    #[serde(default, deserialize_with = "optional_string_lossy")]
    pub query: Option<String>,
    #[serde(default, deserialize_with = "optional_u64_lossy")]
    pub limit: Option<u64>,
}

impl SearchParams {
    #[must_use]
    pub fn normalized_query(&self) -> &str {
        self.query.as_deref().unwrap_or("").trim()
    }

    #[must_use]
    pub fn limit_or_default(&self) -> usize {
        self.limit.unwrap_or(20) as usize
    }
}

/// Params for `session.run_detail`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunDetailParams {
    #[serde(alias = "session_key")]
    pub session_key: String,
    #[serde(alias = "run_id")]
    pub run_id: String,
}

/// Params for `session.voice_generate`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceGenerateParams {
    pub key: String,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub message_index: Option<usize>,
    #[serde(default)]
    pub history_index: Option<usize>,
}

impl VoiceGenerateParams {
    /// Resolve the target specification. `run_id` takes precedence.
    pub fn target(&self) -> Result<VoiceTarget, &'static str> {
        if let Some(ref id) = self.run_id {
            let trimmed = id.trim();
            if !trimmed.is_empty() {
                return Ok(VoiceTarget::ByRunId(trimmed.to_string()));
            }
        }
        if let Some(idx) = self.message_index.or(self.history_index) {
            return Ok(VoiceTarget::ByMessageIndex(idx));
        }
        Err("missing 'messageIndex' or 'runId' parameter")
    }
}

/// How to locate the target assistant message for voice generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceTarget {
    /// Locate by agent run ID (stable across inserted tool_result messages).
    ByRunId(String),
    /// Locate by raw message index in the history array.
    ByMessageIndex(usize),
}

/// Parse a `serde_json::Value` into a typed param struct, mapping
/// deserialization errors to the service error format.
pub fn parse_params<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, ServiceError> {
    serde_json::from_value(params).map_err(ServiceError::message)
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use {super::*, serde_json::json};

    #[test]
    fn patch_params_minimal() {
        let p: PatchParams = serde_json::from_value(json!({"key": "main"})).unwrap();
        assert_eq!(p.key, "main");
        assert!(p.label.is_none());
        assert!(p.model.is_none());
        assert!(p.project_id.is_none());
        assert!(p.sandbox_enabled.is_none());
    }

    #[test]
    fn patch_params_with_fields() {
        let p: PatchParams = serde_json::from_value(json!({
            "key": "main",
            "label": "My Chat",
            "model": "gpt-4o",
            "sandboxEnabled": true,
            "mcpDisabled": false,
        }))
        .unwrap();
        assert_eq!(p.label.as_deref(), Some("My Chat"));
        assert_eq!(p.model.as_deref(), Some("gpt-4o"));
        assert_eq!(p.sandbox_enabled, Some(Some(true)));
        assert_eq!(p.mcp_disabled, Some(Some(false)));
    }

    #[test]
    fn patch_params_sandbox_enabled_false() {
        let p: PatchParams = serde_json::from_value(json!({
            "key": "main",
            "sandboxEnabled": false,
        }))
        .unwrap();
        assert_eq!(p.sandbox_enabled, Some(Some(false)));
    }

    #[test]
    fn patch_params_sandbox_enabled_null_clears() {
        let p: PatchParams = serde_json::from_value(json!({
            "key": "main",
            "sandboxEnabled": null,
        }))
        .unwrap();
        assert_eq!(p.sandbox_enabled, Some(None));
    }

    #[test]
    fn patch_params_accepts_legacy_snake_case_fields() {
        let p: PatchParams = serde_json::from_value(json!({
            "key": "main",
            "project_id": "proj-1",
            "worktree_branch": "feature/abc",
            "sandbox_image": "custom:latest",
            "sandbox_enabled": false,
            "mcp_disabled": true,
        }))
        .unwrap();
        assert_eq!(p.project_id, Some(Some("proj-1".to_string())));
        assert_eq!(p.worktree_branch, Some(Some("feature/abc".to_string())));
        assert_eq!(p.sandbox_image, Some(Some("custom:latest".to_string())));
        assert_eq!(p.sandbox_enabled, Some(Some(false)));
        assert_eq!(p.mcp_disabled, Some(Some(true)));
    }

    #[test]
    fn patch_params_null_project_id() {
        let p: PatchParams = serde_json::from_value(json!({
            "key": "main",
            "projectId": null,
        }))
        .unwrap();
        // Outer Some = field was present; inner None = value was null (clear).
        assert!(matches!(p.project_id, Some(None)));
    }

    #[test]
    fn patch_params_set_project_id() {
        let p: PatchParams = serde_json::from_value(json!({
            "key": "main",
            "projectId": "proj-1",
        }))
        .unwrap();
        assert_eq!(p.project_id, Some(Some("proj-1".to_string())));
    }

    #[test]
    fn voice_generate_run_id_precedence() {
        let p: VoiceGenerateParams = serde_json::from_value(json!({
            "key": "main",
            "runId": "run-abc",
            "messageIndex": 5,
        }))
        .unwrap();
        assert_eq!(p.target().unwrap(), VoiceTarget::ByRunId("run-abc".into()));
    }

    #[test]
    fn voice_generate_index_fallback() {
        let p: VoiceGenerateParams = serde_json::from_value(json!({
            "key": "main",
            "messageIndex": 3,
        }))
        .unwrap();
        assert_eq!(p.target().unwrap(), VoiceTarget::ByMessageIndex(3));
    }

    #[test]
    fn voice_generate_history_index_fallback() {
        let p: VoiceGenerateParams = serde_json::from_value(json!({
            "key": "main",
            "historyIndex": 7,
        }))
        .unwrap();
        assert_eq!(p.target().unwrap(), VoiceTarget::ByMessageIndex(7));
    }

    #[test]
    fn voice_generate_no_target() {
        let p: VoiceGenerateParams = serde_json::from_value(json!({"key": "main"})).unwrap();
        assert!(p.target().is_err());
    }

    #[test]
    fn voice_generate_blank_run_id_falls_back_to_index() {
        let p: VoiceGenerateParams = serde_json::from_value(json!({
            "key": "main",
            "runId": "  ",
            "messageIndex": 2,
        }))
        .unwrap();
        assert_eq!(p.target().unwrap(), VoiceTarget::ByMessageIndex(2));
    }

    #[test]
    fn parse_params_helper() {
        let v = json!({"key": "main"});
        let p: PatchParams = parse_params(v).unwrap();
        assert_eq!(p.key, "main");
    }

    #[test]
    fn parse_params_error() {
        let v = json!({"not_key": true});
        let err = parse_params::<PatchParams>(v).unwrap_err();
        assert!(err.to_string().contains("key"));
    }

    #[test]
    fn preview_params_limit_defaults_and_invalid_limit_is_ignored() {
        let missing: PreviewParams = serde_json::from_value(json!({ "key": "main" })).unwrap();
        assert_eq!(missing.limit_or_default(), 5);

        let invalid: PreviewParams =
            serde_json::from_value(json!({ "key": "main", "limit": "bad" })).unwrap();
        assert_eq!(invalid.limit, None);
        assert_eq!(invalid.limit_or_default(), 5);
    }

    #[test]
    fn resolve_params_accepts_aliases_and_filters_blank_inherit() {
        let legacy: ResolveParams = serde_json::from_value(json!({
            "key": "main",
            "inherit_agent_from": "parent-1",
        }))
        .unwrap();
        assert_eq!(legacy.inherit_from_key(), Some("parent-1"));

        let camel: ResolveParams = serde_json::from_value(json!({
            "key": "main",
            "inheritAgentFrom": "   ",
        }))
        .unwrap();
        assert_eq!(camel.inherit_from_key(), None);
    }

    #[test]
    fn search_params_invalid_types_fall_back_to_defaults() {
        let p: SearchParams =
            serde_json::from_value(json!({ "query": 123, "limit": "many" })).unwrap();
        assert_eq!(p.normalized_query(), "");
        assert_eq!(p.limit_or_default(), 20);
    }

    #[test]
    fn delete_params_force_defaults_false_on_invalid_type() {
        let p: DeleteParams =
            serde_json::from_value(json!({ "key": "main", "force": "true" })).unwrap();
        assert_eq!(p.force, None);
        assert!(!p.force_or_default());
    }

    #[test]
    fn fork_params_accepts_legacy_fork_point_alias() {
        let p: ForkParams = serde_json::from_value(json!({
            "key": "main",
            "label": 1234,
            "fork_point": 9,
        }))
        .unwrap();
        assert_eq!(p.label, None);
        assert_eq!(p.fork_point, Some(9));
    }

    #[test]
    fn run_detail_params_accepts_legacy_snake_case_fields() {
        let p: RunDetailParams = serde_json::from_value(json!({
            "session_key": "main",
            "run_id": "run-abc",
        }))
        .unwrap();
        assert_eq!(p.session_key, "main");
        assert_eq!(p.run_id, "run-abc");
    }
}
