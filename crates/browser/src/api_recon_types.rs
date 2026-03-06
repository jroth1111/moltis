//! Public types for API reconnaissance.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApiReconMode {
    Off,
    #[default]
    Passive,
    Focused,
}

impl std::fmt::Display for ApiReconMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::Passive => f.write_str("passive"),
            Self::Focused => f.write_str("focused"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ApiReconStats {
    pub observed_requests: u64,
    pub json_bodies: u64,
    pub non_json_bodies: u64,
    pub parse_failures: u64,
    pub body_unavailable: u64,
    pub body_fetch_failures: u64,
    pub body_truncations: u64,
    pub depth_truncations: u64,
    pub key_truncations: u64,
    pub dropped_endpoints: u64,
    pub dropped_pending_requests: u64,
    pub invalid_urls: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiReconStatus {
    pub mode: ApiReconMode,
    pub healthy: bool,
    pub degraded_reasons: Vec<String>,
    pub total_endpoints: usize,
    pub recent_exchange_count: usize,
    pub pending_requests: usize,
    pub stats: ApiReconStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiObservationMarker {
    pub marker_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub tab_id: String,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiObservationDelta {
    pub marker_id: String,
    pub endpoints: Vec<ApiEndpointSummary>,
    pub exchanges: Vec<ApiObservedExchange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiProtocol {
    Rest,
    GraphQl,
    Rpc,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiEndpointRole {
    CollectionRead,
    EntityRead,
    Mutation,
    Auth,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiPaginationStyle {
    Cursor,
    Page,
    OffsetLimit,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiScalarKind {
    Null,
    Boolean,
    Integer,
    Number,
    String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiOpaqueReason {
    EmptyArray,
    DepthLimit,
    KeyLimit,
    MissingField,
    NonJson,
    ParseError,
    Binary,
    EmptyBody,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiEndpointList {
    pub endpoints: Vec<ApiEndpointSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiEndpointSummary {
    pub endpoint_id: String,
    pub protocol: ApiProtocol,
    pub role: ApiEndpointRole,
    pub method: String,
    pub host: String,
    pub route_template: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_name: Option<String>,
    pub sample_count: u64,
    pub data_score: f32,
    pub last_seen_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ApiAuthSignals {
    pub has_cookie_auth: bool,
    pub has_authorization_header: bool,
    pub has_csrf_header: bool,
    pub header_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ApiObjectContract {
    pub fields: BTreeMap<String, ApiFieldContract>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ApiHeaderContract {
    pub required_keys: Vec<String>,
    pub optional_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ApiDataHints {
    pub item_paths: Vec<String>,
    pub id_fields: Vec<String>,
    pub filter_fields: Vec<String>,
    pub sort_fields: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pagination: Option<ApiPaginationHints>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiPaginationHints {
    pub style: ApiPaginationStyle,
    pub request_fields: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiContractNode {
    Scalar {
        kind: ApiScalarKind,
    },
    Nullable {
        value: Box<ApiContractNode>,
    },
    Array {
        items: Box<ApiContractNode>,
    },
    Object {
        fields: BTreeMap<String, ApiFieldContract>,
    },
    OneOf {
        variants: Vec<ApiContractNode>,
    },
    Opaque {
        reason: ApiOpaqueReason,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiFieldContract {
    pub schema: ApiContractNode,
    pub required_rate: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiRequestContract {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub query: ApiObjectContract,
    pub headers: ApiHeaderContract,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<ApiContractNode>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiResponseContract {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub headers: ApiHeaderContract,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<ApiContractNode>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiExampleRef {
    pub exchange_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiEndpointContract {
    pub endpoint_id: String,
    pub protocol: ApiProtocol,
    pub role: ApiEndpointRole,
    pub method: String,
    pub host: String,
    pub route_template: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_name: Option<String>,
    pub auth: ApiAuthSignals,
    pub request: ApiRequestContract,
    pub responses: BTreeMap<u16, ApiResponseContract>,
    pub data_hints: ApiDataHints,
    pub examples: Vec<ApiExampleRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiObservedExchange {
    pub exchange_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marker_id: Option<String>,
    pub tab_id: String,
    pub endpoint_id: String,
    pub started_at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<OffsetDateTime>,
    pub request: ApiObservedRequest,
    pub response: ApiObservedResponse,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiObservedRequest {
    pub method: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_name: Option<String>,
    pub query_keys: Vec<String>,
    pub header_keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_schema: Option<ApiContractNode>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiObservedResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub header_keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_schema: Option<ApiContractNode>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApiCallOverrides {
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub query: BTreeMap<String, Value>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiExtractPlan {
    #[serde(default)]
    pub json_pointer: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiPaginationPlan {
    pub style: ApiPaginationStyle,
    #[serde(default)]
    pub cursor_param: Option<String>,
    #[serde(default)]
    pub page_param: Option<String>,
    #[serde(default)]
    pub page_size_param: Option<String>,
    #[serde(default)]
    pub offset_param: Option<String>,
    #[serde(default)]
    pub limit_param: Option<String>,
    #[serde(default)]
    pub next_cursor_pointer: Option<String>,
    #[serde(default)]
    pub has_more_pointer: Option<String>,
    #[serde(default)]
    pub item_pointer: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiCallResult {
    pub endpoint: ApiEndpointSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extracted: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiCollectResult {
    pub endpoint: ApiEndpointSummary,
    pub pages_fetched: u32,
    pub item_count: usize,
    pub truncated: bool,
    pub items: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ApiObservedRequestInput {
    pub method: String,
    pub url: String,
    pub tab_id: String,
    pub marker_id: Option<String>,
    pub started_at: OffsetDateTime,
    pub query_keys: std::collections::BTreeSet<String>,
    pub header_keys: std::collections::BTreeSet<String>,
    pub header_values: BTreeMap<String, String>,
    pub content_type: Option<String>,
    pub operation_name: Option<String>,
    pub body_schema: Option<ApiContractNode>,
    pub body_value: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ApiObservedResponseInput {
    pub status: Option<u16>,
    pub header_keys: std::collections::BTreeSet<String>,
    pub content_type: Option<String>,
    pub body_schema: Option<ApiContractNode>,
    pub finished_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiBodyClassification {
    Json,
    NonJson,
    ParseError,
    Missing,
}

#[derive(Debug, Clone, Default)]
pub struct ApiInferenceNotes {
    pub body_truncated: bool,
    pub depth_truncations: u64,
    pub key_truncations: u64,
    pub body_fetch_failed: bool,
}

#[derive(Debug, Clone)]
pub struct ApiInference {
    pub classification: ApiBodyClassification,
    pub contract: Option<ApiContractNode>,
    pub notes: ApiInferenceNotes,
}

impl ApiInference {
    #[must_use]
    pub fn missing() -> Self {
        Self {
            classification: ApiBodyClassification::Missing,
            contract: None,
            notes: ApiInferenceNotes::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EndpointIdentity {
    pub endpoint_id: String,
    pub protocol: ApiProtocol,
    pub role: ApiEndpointRole,
    pub host: String,
    pub route_template: String,
    pub operation_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ApiRequestTemplate {
    pub method: String,
    pub url: String,
    pub content_type: Option<String>,
    pub operation_name: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub body: Option<Value>,
}
