//! Passive API traffic capture and endpoint catalog inference.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use {
    base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD},
    chromiumoxide::cdp::browser_protocol::network::{
        self, EventLoadingFailed, EventRequestWillBeSent, EventResponseReceived,
    },
    serde::{Deserialize, Serialize},
    serde_json::{Map, Value},
    tokio::task::JoinHandle,
    url::{Host, Url, form_urlencoded},
    uuid::Uuid,
};

const DEFAULT_MAX_EXAMPLES_PER_ENDPOINT: usize = 3;
const MAX_AGENT_SUMMARY_HOSTS: usize = 5;
const MAX_AGENT_SUMMARY_ENDPOINTS: usize = 25;
const MAX_AGENT_SUMMARY_FIELDS: usize = 20;
const MAX_AGENT_SUMMARY_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone)]
pub struct ApiCaptureConfig {
    pub allowed_hosts: Vec<String>,
    pub url_patterns: Vec<String>,
    pub include_document_requests: bool,
    pub max_examples_per_endpoint: usize,
}

impl Default for ApiCaptureConfig {
    fn default() -> Self {
        Self {
            allowed_hosts: Vec::new(),
            url_patterns: Vec::new(),
            include_document_requests: false,
            max_examples_per_endpoint: DEFAULT_MAX_EXAMPLES_PER_ENDPOINT,
        }
    }
}

#[derive(Debug, Default)]
pub struct ApiCaptureRuntime {
    pub handle: Option<String>,
    pub config: Option<ApiCaptureConfig>,
    pub recorder: Option<ApiCaptureRecorder>,
    pub attached_targets: HashSet<String>,
    pub tasks: Vec<JoinHandle<()>>,
}

#[derive(Debug)]
pub struct ApiCaptureSnapshot {
    pub handle: String,
    pub config: ApiCaptureConfig,
    pub recorder: ApiCaptureRecorder,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ApiCatalog {
    pub summary: ApiCatalogSummary,
    pub endpoints: Vec<ApiEndpoint>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ApiCatalogOmittedCounts {
    #[serde(skip_serializing_if = "is_zero")]
    pub hosts: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub endpoints: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub fields: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ApiCatalogAgentSummary {
    pub summary: ApiCatalogSummary,
    pub endpoints: Vec<ApiEndpoint>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "omitted_counts_is_empty")]
    pub omitted_counts: ApiCatalogOmittedCounts,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ApiCatalogSummary {
    pub captured_requests: usize,
    pub endpoint_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ApiEndpoint {
    pub method: String,
    pub origin: String,
    pub path_template: String,
    pub body_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_name: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub auth: Vec<ApiAuthShape>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub query_params: Vec<ApiFieldShape>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<ApiBodyShape>,
    pub response: ApiResponseMeta,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub semantic_hints: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<ApiRequestExample>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ApiAuthShape {
    pub location: String,
    pub name: String,
    pub scheme: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ApiFieldShape {
    pub name: String,
    pub required: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub repeated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub semantic_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ApiBodyShape {
    pub kind: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub content_types: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<ApiFieldShape>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ApiResponseMeta {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub statuses: Vec<u16>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub content_types: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ApiRequestExample {
    pub redacted_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub query: BTreeMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedRequestRecord {
    #[serde(default)]
    pub request_id: String,
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub request_headers: Vec<(String, String)>,
    #[serde(default)]
    pub request_body: Option<String>,
    #[serde(default)]
    pub request_content_type: Option<String>,
    #[serde(default)]
    pub resource_type: Option<String>,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub response_content_type: Option<String>,
}

#[derive(Debug)]
pub struct ApiCaptureRecorder {
    config: ApiCaptureConfig,
    observations: Vec<CapturedRequestRecord>,
    pending_requests: HashMap<network::RequestId, ObservedRequest>,
    pending_response_updates: HashMap<network::RequestId, ResponseUpdate>,
    pending_header_overrides: HashMap<network::RequestId, HashMap<String, String>>,
    terminal_requests: HashSet<network::RequestId>,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn omitted_counts_is_empty(counts: &ApiCatalogOmittedCounts) -> bool {
    counts.hosts == 0 && counts.endpoints == 0 && counts.fields == 0
}

impl ApiCaptureRecorder {
    pub fn new(config: ApiCaptureConfig) -> Self {
        Self {
            config,
            observations: Vec::new(),
            pending_requests: HashMap::new(),
            pending_response_updates: HashMap::new(),
            pending_header_overrides: HashMap::new(),
            terminal_requests: HashSet::new(),
        }
    }

    pub async fn record_request(
        &mut self,
        event: &EventRequestWillBeSent,
        fallback_request_body: Option<String>,
    ) {
        let request_id = event.request_id.clone();
        let Some(mut observed) = ObservedRequest::from_request_event(event, fallback_request_body)
        else {
            return;
        };
        if !should_capture_request(
            &self.config,
            observed.url.as_str(),
            observed.resource_type.as_deref(),
        )
        .await
        {
            return;
        }

        if let Some(header_overrides) = self.pending_header_overrides.remove(&request_id) {
            observed.merge_request_headers(&header_overrides);
        }
        if let Some(update) = self.pending_response_updates.remove(&request_id) {
            observed.apply_response_update(update);
        }

        if self.terminal_requests.remove(&request_id) {
            self.observations.push(observed.into_record());
        } else {
            self.pending_requests.insert(request_id, observed);
        }
    }

    pub fn merge_request_headers(
        &mut self,
        request_id: &network::RequestId,
        extra_headers: &HashMap<String, String>,
    ) {
        if extra_headers.is_empty() {
            return;
        }
        if let Some(observed) = self.pending_requests.get_mut(request_id) {
            observed.merge_request_headers(extra_headers);
            return;
        }
        self.pending_header_overrides
            .entry(request_id.clone())
            .or_default()
            .extend(extra_headers.clone());
    }

    pub fn apply_response_received(&mut self, event: &EventResponseReceived) {
        let update = ResponseUpdate::from_event(event);
        if let Some(observed) = self.pending_requests.get_mut(&event.request_id) {
            observed.apply_response_update(update);
            return;
        }
        if let Some(observed) = self
            .observations
            .iter_mut()
            .rev()
            .find(|observed| observed.request_id == event.request_id.as_ref())
        {
            observed.status = update.status;
            observed.response_content_type = update.response_content_type;
            return;
        }
        self.pending_response_updates
            .insert(event.request_id.clone(), update);
    }

    pub fn apply_loading_finished(&mut self, request_id: &network::RequestId) {
        if let Some(observed) = self.pending_requests.remove(request_id) {
            self.observations.push(observed.into_record());
            return;
        }
        if !self
            .observations
            .iter()
            .any(|observed| observed.request_id == request_id.as_ref())
        {
            self.terminal_requests.insert(request_id.clone());
        }
    }

    pub fn apply_loading_failed(&mut self, event: &EventLoadingFailed) {
        if let Some(observed) = self.pending_requests.remove(&event.request_id) {
            self.observations.push(observed.into_record());
            return;
        }
        if !self
            .observations
            .iter()
            .any(|observed| observed.request_id == event.request_id.as_ref())
        {
            self.terminal_requests.insert(event.request_id.clone());
        }
    }

    pub fn finish(&mut self) {
        for (_, observed) in self.pending_requests.drain() {
            self.observations.push(observed.into_record());
        }
        self.pending_response_updates.clear();
        self.pending_header_overrides.clear();
        self.terminal_requests.clear();
    }

    pub fn append_records(&mut self, records: impl IntoIterator<Item = CapturedRequestRecord>) {
        self.observations.extend(records);
    }

    #[must_use]
    pub fn build_catalog(&self) -> ApiCatalog {
        let mut grouped = BTreeMap::<EndpointGroupKey, EndpointAccumulator>::new();
        let mut hosts = BTreeSet::new();

        for observed in &self.observations {
            let Some(parsed) = ParsedObservation::from_record(observed) else {
                continue;
            };

            let _ = hosts.insert(parsed.origin.clone());
            let key = EndpointGroupKey {
                method: parsed.method.clone(),
                origin: parsed.origin.clone(),
                path_template: parsed.path_template.clone(),
                body_kind: parsed.body_kind.clone(),
                operation_name: parsed.operation_name.clone(),
            };
            grouped
                .entry(key)
                .or_insert_with(|| EndpointAccumulator::new(self.config.max_examples_per_endpoint))
                .record(parsed);
        }

        let endpoints: Vec<ApiEndpoint> = grouped
            .into_iter()
            .map(|(key, acc)| acc.into_endpoint(key))
            .collect();

        ApiCatalog {
            summary: ApiCatalogSummary {
                captured_requests: self.observations.len(),
                endpoint_count: if self.observations.is_empty() {
                    0
                } else {
                    grouped_endpoint_count(&endpoints)
                },
                hosts: hosts.into_iter().collect(),
            },
            endpoints,
        }
    }
}

fn grouped_endpoint_count(endpoints: &[ApiEndpoint]) -> usize {
    endpoints.len()
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone)]
struct ObservedRequest {
    request_id: network::RequestId,
    method: String,
    url: String,
    request_headers: Vec<(String, String)>,
    request_body: Option<String>,
    request_content_type: Option<String>,
    resource_type: Option<String>,
    status: Option<u16>,
    response_content_type: Option<String>,
}

#[derive(Debug, Clone)]
struct ResponseUpdate {
    status: Option<u16>,
    response_content_type: Option<String>,
}

impl ResponseUpdate {
    fn from_event(event: &EventResponseReceived) -> Self {
        Self {
            status: Some(event.response.status.clamp(0, u16::MAX as i64) as u16),
            response_content_type: Some(normalize_content_type(&event.response.mime_type)),
        }
    }
}

impl ObservedRequest {
    fn from_request_event(
        event: &EventRequestWillBeSent,
        fallback_request_body: Option<String>,
    ) -> Option<Self> {
        let request_content_type = header_value(&event.request.headers, "content-type");
        let request_body = event
            .request
            .post_data_entries
            .as_deref()
            .and_then(|entries| {
                entries.first().and_then(|entry| {
                    entry.bytes.as_ref().map(|bytes| {
                        let bytes: &str = bytes.as_ref();
                        bytes.to_string()
                    })
                })
            })
            .or(fallback_request_body)
            .map(|body| normalize_request_body(body, request_content_type.as_deref()));

        Some(Self {
            request_id: event.request_id.clone(),
            method: event.request.method.clone(),
            url: event.request.url.clone(),
            request_headers: headers_to_pairs(&event.request.headers),
            request_body,
            request_content_type,
            resource_type: event.r#type.as_ref().map(|kind| kind.as_ref().to_string()),
            status: None,
            response_content_type: None,
        })
    }

    fn apply_response_update(&mut self, update: ResponseUpdate) {
        self.status = update.status;
        self.response_content_type = update.response_content_type;
    }

    fn merge_request_headers(&mut self, extra_headers: &HashMap<String, String>) {
        for (name, value) in extra_headers {
            if let Some((_, existing_value)) = self
                .request_headers
                .iter_mut()
                .find(|(existing_name, _)| existing_name.eq_ignore_ascii_case(name))
            {
                *existing_value = value.clone();
            } else {
                self.request_headers.push((name.clone(), value.clone()));
            }

            if name.eq_ignore_ascii_case("content-type") {
                self.request_content_type = Some(normalize_content_type(value));
            }
        }
    }

    fn into_record(self) -> CapturedRequestRecord {
        CapturedRequestRecord {
            request_id: self.request_id.as_ref().to_string(),
            method: self.method,
            url: self.url,
            request_headers: self.request_headers,
            request_body: self.request_body,
            request_content_type: self.request_content_type,
            resource_type: self.resource_type,
            status: self.status,
            response_content_type: self.response_content_type,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EndpointGroupKey {
    method: String,
    origin: String,
    path_template: String,
    body_kind: String,
    operation_name: Option<String>,
}

#[derive(Debug)]
struct EndpointAccumulator {
    sample_count: usize,
    max_examples: usize,
    auth: BTreeSet<ApiAuthShape>,
    query_params: BTreeMap<String, FieldAccumulator>,
    body_fields: BTreeMap<String, FieldAccumulator>,
    body_content_types: BTreeSet<String>,
    statuses: BTreeSet<u16>,
    response_content_types: BTreeSet<String>,
    semantic_hints: BTreeSet<String>,
    examples: Vec<ApiRequestExample>,
}

impl EndpointAccumulator {
    fn new(max_examples: usize) -> Self {
        Self {
            sample_count: 0,
            max_examples,
            auth: BTreeSet::new(),
            query_params: BTreeMap::new(),
            body_fields: BTreeMap::new(),
            body_content_types: BTreeSet::new(),
            statuses: BTreeSet::new(),
            response_content_types: BTreeSet::new(),
            semantic_hints: BTreeSet::new(),
            examples: Vec::new(),
        }
    }

    fn record(&mut self, parsed: ParsedObservation) {
        self.sample_count = self.sample_count.saturating_add(1);
        self.auth.extend(parsed.auth);

        for hint in parsed.semantic_hints {
            let _ = self.semantic_hints.insert(hint);
        }

        for field in parsed.query_fields {
            self.query_params
                .entry(field.name.clone())
                .or_default()
                .record(&field);
        }

        if let Some(body) = parsed.body {
            if let Some(content_type) = body.content_type {
                let _ = self.body_content_types.insert(content_type);
            }
            for field in body.fields {
                self.body_fields
                    .entry(field.name.clone())
                    .or_default()
                    .record(&field);
            }
        }

        if let Some(status) = parsed.status {
            let _ = self.statuses.insert(status);
        }
        if let Some(content_type) = parsed.response_content_type {
            let _ = self.response_content_types.insert(content_type);
        }

        if self.examples.len() < self.max_examples {
            self.examples.push(parsed.example);
        }
    }

    fn into_endpoint(self, key: EndpointGroupKey) -> ApiEndpoint {
        let body =
            (!self.body_fields.is_empty() || !self.body_content_types.is_empty()).then(|| {
                ApiBodyShape {
                    kind: key.body_kind.clone(),
                    content_types: self.body_content_types.into_iter().collect(),
                    fields: finalize_fields(self.body_fields, self.sample_count),
                }
            });

        ApiEndpoint {
            method: key.method,
            origin: key.origin,
            path_template: key.path_template,
            body_kind: key.body_kind,
            operation_name: key.operation_name,
            auth: self.auth.into_iter().collect(),
            query_params: finalize_fields(self.query_params, self.sample_count),
            body,
            response: ApiResponseMeta {
                statuses: self.statuses.into_iter().collect(),
                content_types: self.response_content_types.into_iter().collect(),
            },
            semantic_hints: self.semantic_hints.into_iter().collect(),
            examples: self.examples,
        }
    }
}

fn finalize_fields(
    fields: BTreeMap<String, FieldAccumulator>,
    sample_count: usize,
) -> Vec<ApiFieldShape> {
    fields
        .into_iter()
        .map(|(name, field)| ApiFieldShape {
            name,
            required: field.present_count == sample_count,
            repeated: field.repeated,
            types: field.types.into_iter().collect(),
            semantic_hints: field.semantic_hints.into_iter().collect(),
        })
        .collect()
}

#[derive(Debug, Default)]
struct FieldAccumulator {
    present_count: usize,
    repeated: bool,
    types: BTreeSet<String>,
    semantic_hints: BTreeSet<String>,
}

impl FieldAccumulator {
    fn record(&mut self, field: &ObservedField) {
        self.present_count = self.present_count.saturating_add(1);
        self.repeated |= field.repeated;
        let _ = self.types.insert(field.value_type.clone());
        self.semantic_hints
            .extend(field.semantic_hints.iter().cloned());
    }
}

#[derive(Debug)]
struct ParsedObservation {
    method: String,
    origin: String,
    path_template: String,
    body_kind: String,
    operation_name: Option<String>,
    auth: Vec<ApiAuthShape>,
    query_fields: Vec<ObservedField>,
    body: Option<ParsedBody>,
    status: Option<u16>,
    response_content_type: Option<String>,
    semantic_hints: BTreeSet<String>,
    example: ApiRequestExample,
}

impl ParsedObservation {
    fn from_record(observed: &CapturedRequestRecord) -> Option<Self> {
        let url = Url::parse(&observed.url).ok()?;
        let origin = url.origin().ascii_serialization();
        let path_template = normalize_path_template(&url);
        let query_pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        let query_fields =
            observed_fields_from_grouped_params(group_param_pairs(&query_pairs), "string");

        let mut semantic_hints = collect_semantic_hints_for_path(path_template.as_str());
        for field in &query_fields {
            semantic_hints.extend(field.semantic_hints.iter().cloned());
        }

        let body = parse_request_body(
            observed.request_body.as_deref(),
            observed.request_content_type.as_deref(),
            &url,
        );
        if let Some(body) = &body {
            for field in &body.fields {
                semantic_hints.extend(field.semantic_hints.iter().cloned());
            }
        }

        let mut auth = detect_header_auth_shapes(&observed.request_headers);
        auth.extend(detect_param_auth_shapes(&query_pairs, "query"));
        if let Some(body) = &body {
            auth.extend(detect_field_auth_shapes(&body.fields, "body"));
        }

        let body_kind = body
            .as_ref()
            .map(|parsed| parsed.kind.clone())
            .unwrap_or_else(|| "none".to_string());
        let operation_name = body
            .as_ref()
            .and_then(|parsed| parsed.operation_name.clone());

        let example = build_request_example(
            &url,
            &query_pairs,
            observed.request_body.as_deref(),
            body.as_ref(),
        );

        Some(Self {
            method: observed.method.clone(),
            origin,
            path_template,
            body_kind,
            operation_name,
            auth,
            query_fields,
            body,
            status: observed.status,
            response_content_type: observed.response_content_type.clone(),
            semantic_hints,
            example,
        })
    }
}

impl ApiCatalog {
    #[must_use]
    pub fn to_agent_summary(&self) -> ApiCatalogAgentSummary {
        let mut omitted_counts = ApiCatalogOmittedCounts::default();
        let mut summary_hosts = self.summary.hosts.clone();
        if summary_hosts.len() > MAX_AGENT_SUMMARY_HOSTS {
            omitted_counts.hosts = summary_hosts.len() - MAX_AGENT_SUMMARY_HOSTS;
            summary_hosts.truncate(MAX_AGENT_SUMMARY_HOSTS);
        }

        let mut endpoints = self
            .endpoints
            .iter()
            .cloned()
            .map(|mut endpoint| {
                endpoint.examples.clear();
                endpoint.query_params =
                    truncate_fields(endpoint.query_params, &mut omitted_counts.fields);
                if let Some(body) = endpoint.body.as_mut() {
                    body.fields = truncate_fields(
                        std::mem::take(&mut body.fields),
                        &mut omitted_counts.fields,
                    );
                }
                endpoint
            })
            .collect::<Vec<_>>();

        if endpoints.len() > MAX_AGENT_SUMMARY_ENDPOINTS {
            omitted_counts.endpoints = endpoints.len() - MAX_AGENT_SUMMARY_ENDPOINTS;
            endpoints.truncate(MAX_AGENT_SUMMARY_ENDPOINTS);
        }

        let mut summary = ApiCatalogAgentSummary {
            summary: ApiCatalogSummary {
                captured_requests: self.summary.captured_requests,
                endpoint_count: self.summary.endpoint_count,
                hosts: summary_hosts,
            },
            endpoints,
            truncated: !omitted_counts_is_empty(&omitted_counts),
            omitted_counts,
        };

        while serde_json::to_vec(&summary)
            .ok()
            .is_some_and(|bytes| bytes.len() > MAX_AGENT_SUMMARY_BYTES)
        {
            if let Some(endpoint) = summary.endpoints.pop() {
                summary.omitted_counts.endpoints =
                    summary.omitted_counts.endpoints.saturating_add(1);
                summary.omitted_counts.fields = summary
                    .omitted_counts
                    .fields
                    .saturating_add(endpoint.query_params.len());
                if let Some(body) = endpoint.body {
                    summary.omitted_counts.fields = summary
                        .omitted_counts
                        .fields
                        .saturating_add(body.fields.len());
                }
                summary.truncated = true;
                continue;
            }

            if let Some(host) = summary.summary.hosts.pop() {
                let _ = host;
                summary.omitted_counts.hosts = summary.omitted_counts.hosts.saturating_add(1);
                summary.truncated = true;
                continue;
            }

            break;
        }

        summary.truncated = summary.truncated || !omitted_counts_is_empty(&summary.omitted_counts);
        summary
    }
}

fn truncate_fields(
    mut fields: Vec<ApiFieldShape>,
    omitted_field_count: &mut usize,
) -> Vec<ApiFieldShape> {
    if fields.len() > MAX_AGENT_SUMMARY_FIELDS {
        *omitted_field_count =
            omitted_field_count.saturating_add(fields.len() - MAX_AGENT_SUMMARY_FIELDS);
        fields.truncate(MAX_AGENT_SUMMARY_FIELDS);
    }
    fields
}

#[derive(Debug, Clone)]
struct ParsedBody {
    kind: String,
    content_type: Option<String>,
    operation_name: Option<String>,
    fields: Vec<ObservedField>,
}

#[derive(Debug, Clone)]
struct ObservedField {
    name: String,
    value_type: String,
    repeated: bool,
    semantic_hints: BTreeSet<String>,
}

impl ObservedField {
    fn new(name: String, value_type: &str) -> Self {
        Self::with_repeated(name, value_type, false)
    }

    fn with_repeated(name: String, value_type: &str, repeated: bool) -> Self {
        Self {
            repeated,
            semantic_hints: collect_semantic_hints_for_name(&name),
            name,
            value_type: value_type.to_string(),
        }
    }
}

fn group_param_pairs(pairs: &[(String, String)]) -> BTreeMap<String, Vec<String>> {
    let mut grouped = BTreeMap::new();
    for (name, value) in pairs {
        grouped
            .entry(name.clone())
            .or_insert_with(Vec::new)
            .push(value.clone());
    }
    grouped
}

fn observed_fields_from_grouped_params(
    grouped: BTreeMap<String, Vec<String>>,
    value_type: &str,
) -> Vec<ObservedField> {
    grouped
        .into_iter()
        .map(|(name, values)| ObservedField::with_repeated(name, value_type, values.len() > 1))
        .collect()
}

fn build_request_example(
    url: &Url,
    query_pairs: &[(String, String)],
    raw_body: Option<&str>,
    body: Option<&ParsedBody>,
) -> ApiRequestExample {
    let mut query = BTreeMap::new();
    let mut redacted_url = url.clone();
    redacted_url.set_query(None);
    {
        let mut serializer = redacted_url.query_pairs_mut();
        for (name, value) in query_pairs {
            let redacted = redact_example_value(name, value);
            query
                .entry(name.clone())
                .or_insert_with(Vec::new)
                .push(redacted.clone());
            serializer.append_pair(name, &redacted);
        }
    }

    let body = match body {
        Some(parsed) if parsed.kind == "text" => {
            raw_body.map(|_| Value::String("[OMITTED]".to_string()))
        },
        Some(_) => raw_body.and_then(redacted_body_example),
        None => None,
    };

    ApiRequestExample {
        redacted_url: redacted_url.to_string(),
        url: url.path().to_string(),
        query,
        body,
    }
}

fn redact_example_value(name: &str, value: &str) -> String {
    if is_sensitive_name(name) {
        "[REDACTED]".to_string()
    } else {
        value.to_string()
    }
}

fn redacted_body_example(body: &str) -> Option<Value> {
    if let Ok(json) = serde_json::from_str::<Value>(body) {
        return Some(redact_json_value(None, &json));
    }

    let form_pairs = form_urlencoded::parse(body.as_bytes()).collect::<Vec<_>>();
    if !form_pairs.is_empty() {
        let mut object = Map::new();
        for (name, value) in form_pairs {
            let name = name.into_owned();
            let redacted = redact_example_value(&name, value.as_ref());
            insert_object_value(&mut object, name, Value::String(redacted));
        }
        return Some(Value::Object(object));
    }

    if body.trim().is_empty() {
        return None;
    }

    Some(Value::String(body.to_string()))
}

fn insert_object_value(object: &mut Map<String, Value>, name: String, value: Value) {
    match object.get_mut(&name) {
        Some(Value::Array(values)) => values.push(value),
        Some(existing) => {
            let previous = existing.take();
            *existing = Value::Array(vec![previous, value]);
        },
        None => {
            object.insert(name, value);
        },
    }
}

fn parse_request_body(
    body: Option<&str>,
    content_type: Option<&str>,
    url: &Url,
) -> Option<ParsedBody> {
    let body = body?.trim();
    if body.is_empty() {
        return None;
    }

    let content_type = content_type.map(normalize_content_type);
    if is_graphql_body(body, content_type.as_deref(), url.path()) {
        return Some(parse_graphql_body(body, content_type));
    }
    if content_type
        .as_deref()
        .is_some_and(|value| value.contains("x-www-form-urlencoded"))
    {
        return Some(parse_form_urlencoded_body(body, content_type));
    }
    if content_type
        .as_deref()
        .is_some_and(|value| value.contains("multipart/form-data"))
    {
        return Some(parse_multipart_body(body, content_type));
    }
    if content_type
        .as_deref()
        .is_some_and(|value| value.contains("json"))
        || serde_json::from_str::<Value>(body).is_ok()
    {
        return Some(parse_json_body(body, content_type));
    }

    Some(ParsedBody {
        kind: "text".to_string(),
        content_type,
        operation_name: None,
        fields: Vec::new(),
    })
}

fn normalize_request_body(body: String, content_type: Option<&str>) -> String {
    if body.trim().is_empty() || body_looks_structured(body.as_str(), content_type) {
        return body;
    }

    let trimmed = body.trim();
    let Ok(decoded) = BASE64_STANDARD.decode(trimmed) else {
        return body;
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return body;
    };
    if body_looks_structured(decoded.as_str(), content_type) {
        decoded
    } else {
        body
    }
}

fn body_looks_structured(body: &str, content_type: Option<&str>) -> bool {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return false;
    }

    if content_type.is_some_and(|value| value.contains("json") || value.contains("graphql")) {
        return serde_json::from_str::<Value>(trimmed).is_ok();
    }
    if content_type.is_some_and(|value| value.contains("x-www-form-urlencoded")) {
        return trimmed.contains('=');
    }
    if content_type.is_some_and(|value| value.contains("multipart/form-data")) {
        return trimmed.contains("Content-Disposition:");
    }

    serde_json::from_str::<Value>(trimmed).is_ok()
        || trimmed.contains('=')
        || trimmed.starts_with('{')
        || trimmed.starts_with('[')
}

fn parse_json_body(body: &str, content_type: Option<String>) -> ParsedBody {
    let mut fields = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        collect_json_fields(None, &value, &mut fields);
    }

    ParsedBody {
        kind: "json".to_string(),
        content_type,
        operation_name: None,
        fields,
    }
}

fn parse_graphql_body(body: &str, content_type: Option<String>) -> ParsedBody {
    let mut fields = Vec::new();
    let mut operation_name = None;

    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(object) = value.as_object() {
            operation_name = object
                .get("operationName")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .or_else(|| {
                    object
                        .get("query")
                        .and_then(Value::as_str)
                        .and_then(extract_graphql_operation_name)
                });

            if let Some(variables) = object.get("variables") {
                collect_json_fields(None, variables, &mut fields);
            }
        }
    } else {
        operation_name = extract_graphql_operation_name(body);
    }

    ParsedBody {
        kind: "graphql".to_string(),
        content_type,
        operation_name,
        fields,
    }
}

fn parse_form_urlencoded_body(body: &str, content_type: Option<String>) -> ParsedBody {
    let grouped = group_param_pairs(
        &form_urlencoded::parse(body.as_bytes())
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect::<Vec<_>>(),
    );
    let fields = observed_fields_from_grouped_params(grouped, "string");

    ParsedBody {
        kind: "form_urlencoded".to_string(),
        content_type,
        operation_name: None,
        fields,
    }
}

fn parse_multipart_body(body: &str, content_type: Option<String>) -> ParsedBody {
    let boundary = content_type.as_deref().and_then(extract_multipart_boundary);
    let mut fields = Vec::new();

    if let Some(boundary) = boundary {
        for part in body.split(boundary.as_str()) {
            let header_end = part.find("\r\n\r\n").or_else(|| part.find("\n\n"));
            let Some(header_end) = header_end else {
                continue;
            };
            let headers = &part[..header_end];
            let Some(name) = extract_multipart_name(headers) else {
                continue;
            };
            let value_type = if headers.contains("filename=") {
                "file"
            } else {
                "string"
            };
            fields.push(ObservedField::new(name, value_type));
        }
    }

    ParsedBody {
        kind: "multipart".to_string(),
        content_type,
        operation_name: None,
        fields,
    }
}

fn collect_json_fields(prefix: Option<&str>, value: &Value, fields: &mut Vec<ObservedField>) {
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                if let Some(prefix) = prefix {
                    fields.push(ObservedField::new(prefix.to_string(), "object"));
                }
                return;
            }
            for (name, child) in map {
                let path = prefix
                    .map(|parent| format!("{parent}.{name}"))
                    .unwrap_or_else(|| name.clone());
                collect_json_fields(Some(path.as_str()), child, fields);
            }
        },
        Value::Array(items) => {
            let Some(prefix) = prefix else {
                return;
            };
            if items.is_empty() {
                fields.push(ObservedField::new(format!("{prefix}[]"), "unknown"));
                return;
            }
            let mut item_types = BTreeSet::new();
            for item in items.iter().take(4) {
                let _ = item_types.insert(json_type_name(item).to_string());
            }
            for item_type in item_types {
                fields.push(ObservedField::new(
                    format!("{prefix}[]"),
                    item_type.as_str(),
                ));
            }
            if let Some(object_item) = items.iter().find(|item| matches!(item, Value::Object(_))) {
                collect_json_fields(Some(&format!("{prefix}[]")), object_item, fields);
            }
        },
        _ => {
            if let Some(prefix) = prefix {
                fields.push(ObservedField::new(
                    prefix.to_string(),
                    json_type_name(value),
                ));
            }
        },
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn redact_json_value(parent_key: Option<&str>, value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut redacted = Map::new();
            for (key, child) in map {
                if is_sensitive_name(key) {
                    redacted.insert(key.clone(), Value::String("[REDACTED]".to_string()));
                } else {
                    redacted.insert(key.clone(), redact_json_value(Some(key.as_str()), child));
                }
            }
            Value::Object(redacted)
        },
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| redact_json_value(parent_key, item))
                .collect(),
        ),
        Value::String(_) if parent_key.is_some_and(is_sensitive_name) => {
            Value::String("[REDACTED]".to_string())
        },
        _ => value.clone(),
    }
}

fn extract_graphql_operation_name(query: &str) -> Option<String> {
    let mut tokens = query
        .split(|c: char| c.is_whitespace() || matches!(c, '{' | '(' | '}'))
        .filter(|token| !token.is_empty());
    while let Some(token) = tokens.next() {
        if matches!(token, "query" | "mutation" | "subscription") {
            return tokens
                .find(|candidate| !candidate.starts_with('@'))
                .map(ToString::to_string);
        }
    }
    None
}

fn detect_header_auth_shapes(headers: &[(String, String)]) -> Vec<ApiAuthShape> {
    let mut auth = BTreeSet::new();
    for (name, value) in headers {
        let normalized = normalize_token(name);
        let Some(scheme) = auth_scheme_for_name_value(normalized.as_str(), value) else {
            continue;
        };
        let _ = auth.insert(ApiAuthShape {
            location: "header".to_string(),
            name: name.to_ascii_lowercase(),
            scheme,
        });
    }
    auth.into_iter().collect()
}

fn detect_param_auth_shapes(params: &[(String, String)], location: &str) -> Vec<ApiAuthShape> {
    let mut auth = BTreeSet::new();
    for (name, value) in params {
        let normalized = normalize_token(name);
        let Some(scheme) = auth_scheme_for_name_value(normalized.as_str(), value) else {
            continue;
        };
        let _ = auth.insert(ApiAuthShape {
            location: location.to_string(),
            name: name.clone(),
            scheme,
        });
    }
    auth.into_iter().collect()
}

fn detect_field_auth_shapes(fields: &[ObservedField], location: &str) -> Vec<ApiAuthShape> {
    let mut auth = BTreeSet::new();
    for field in fields {
        let normalized = normalize_token(field.name.as_str());
        let Some(scheme) = auth_scheme_for_name_value(normalized.as_str(), "") else {
            continue;
        };
        let _ = auth.insert(ApiAuthShape {
            location: location.to_string(),
            name: field.name.clone(),
            scheme,
        });
    }
    auth.into_iter().collect()
}

fn auth_scheme_for_name_value(name: &str, value: &str) -> Option<String> {
    if name == "authorization" {
        let lower = value.to_ascii_lowercase();
        if lower.starts_with("bearer ") {
            return Some("bearer".to_string());
        }
        if lower.starts_with("basic ") {
            return Some("basic".to_string());
        }
        return Some("authorization".to_string());
    }
    if name == "cookie" {
        return Some("cookie".to_string());
    }
    if name.contains("apikey") || (name.contains("api") && name.contains("key")) {
        return Some("api_key".to_string());
    }
    if name.contains("token") {
        return Some("token".to_string());
    }
    if name.contains("session") {
        return Some("session".to_string());
    }
    None
}

fn normalize_path_template(url: &Url) -> String {
    let Some(segments) = url.path_segments() else {
        return url.path().to_string();
    };

    let normalized = segments
        .map(normalize_path_segment)
        .collect::<Vec<_>>()
        .join("/");
    if normalized.is_empty() {
        "/".to_string()
    } else {
        format!("/{normalized}")
    }
}

fn normalize_path_segment(segment: &str) -> String {
    if segment.is_empty() {
        return String::new();
    }
    if segment.chars().all(|c| c.is_ascii_digit()) {
        return "{int}".to_string();
    }
    if Uuid::parse_str(segment).is_ok() {
        return "{uuid}".to_string();
    }
    if segment.len() >= 8 && segment.chars().all(|c| c.is_ascii_hexdigit()) {
        return "{hex}".to_string();
    }
    if segment.len() >= 8
        && segment.chars().any(|c| c.is_ascii_digit())
        && segment.chars().any(|c| c.is_ascii_alphabetic())
    {
        return "{id}".to_string();
    }

    if let Some((base, ext)) = segment.rsplit_once('.') {
        let normalized_base = normalize_path_segment(base);
        if normalized_base != base {
            return format!("{normalized_base}.{ext}");
        }
    }

    segment.to_string()
}

fn collect_semantic_hints_for_path(path_template: &str) -> BTreeSet<String> {
    let mut hints = BTreeSet::new();
    if path_template.contains("search") {
        let _ = hints.insert("search".to_string());
    }
    if path_template.contains("autocomplete") || path_template.contains("suggest") {
        let _ = hints.insert("autocomplete".to_string());
    }
    hints
}

fn collect_semantic_hints_for_name(name: &str) -> BTreeSet<String> {
    let normalized = normalize_token(name);
    let mut hints = BTreeSet::new();
    if matches!(
        normalized.as_str(),
        "q" | "query" | "search" | "term" | "keyword"
    ) {
        let _ = hints.insert("search".to_string());
    }
    if matches!(normalized.as_str(), "page" | "p" | "pagenum" | "pageno") {
        let _ = hints.insert("pagination".to_string());
        let _ = hints.insert("page".to_string());
    }
    if matches!(
        normalized.as_str(),
        "limit" | "pagesize" | "perpage" | "size" | "take"
    ) {
        let _ = hints.insert("pagination".to_string());
        let _ = hints.insert("limit".to_string());
    }
    if matches!(normalized.as_str(), "offset" | "start" | "skip") {
        let _ = hints.insert("pagination".to_string());
        let _ = hints.insert("offset".to_string());
    }
    if normalized.contains("cursor") || matches!(normalized.as_str(), "after" | "before") {
        let _ = hints.insert("pagination".to_string());
        let _ = hints.insert("cursor".to_string());
    }
    if normalized.contains("sort") || normalized == "order" {
        let _ = hints.insert("sort".to_string());
    }
    if normalized.contains("filter")
        || normalized.contains("facet")
        || matches!(normalized.as_str(), "category" | "categories")
    {
        let _ = hints.insert("filter".to_string());
    }
    hints
}

async fn should_capture_request(
    config: &ApiCaptureConfig,
    url: &str,
    resource_type: Option<&str>,
) -> bool {
    match resource_type {
        Some("Fetch" | "XHR" | "EventSource" | "Other") | None => {},
        Some("Document") if config.include_document_requests => {},
        Some("Document") => return false,
        _ => return false,
    }

    if !matches_url_patterns(url, &config.url_patterns) {
        return false;
    }

    let Ok(parsed) = Url::parse(url) else {
        return false;
    };

    if !matches_allowed_hosts(&parsed, &config.allowed_hosts) {
        return false;
    }

    if config.allowed_hosts.is_empty() {
        return true;
    }

    crate::host_guard::validate_public_url_target(&parsed, "API capture request")
        .await
        .is_ok()
}

fn matches_allowed_hosts(parsed: &Url, allowed_hosts: &[String]) -> bool {
    if allowed_hosts.is_empty() {
        return true;
    }

    let Some(host) = parsed.host() else {
        return false;
    };

    allowed_hosts
        .iter()
        .filter_map(|candidate| normalize_allowed_host(candidate))
        .any(|candidate| host_matches_allowed(&host, candidate.as_str()))
}

fn normalize_allowed_host(candidate: &str) -> Option<String> {
    let trimmed = candidate.trim().trim_matches('.');
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_ascii_lowercase())
}

fn host_matches_allowed(host: &Host<&str>, candidate: &str) -> bool {
    match host {
        Host::Domain(domain) => {
            let domain = domain.to_ascii_lowercase();
            domain == candidate || domain.ends_with(&format!(".{candidate}"))
        },
        Host::Ipv4(ip) => ip.to_string() == candidate,
        Host::Ipv6(ip) => ip.to_string().to_ascii_lowercase() == candidate,
    }
}

fn matches_url_patterns(url: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns
        .iter()
        .any(|pattern| wildcard_match(url, pattern.as_str()))
}

fn wildcard_match(value: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return value == pattern;
    }

    let starts_with_wildcard = pattern.starts_with('*');
    let ends_with_wildcard = pattern.ends_with('*');
    let parts = pattern.split('*').filter(|part| !part.is_empty());

    let mut remainder = value;
    let mut seen_any = false;
    for (idx, part) in parts.enumerate() {
        if !seen_any && !starts_with_wildcard {
            let Some(next) = remainder.strip_prefix(part) else {
                return false;
            };
            remainder = next;
            seen_any = true;
            continue;
        }

        if !ends_with_wildcard && pattern.split('*').filter(|p| !p.is_empty()).count() == idx + 1 {
            return remainder.ends_with(part);
        }

        let Some(position) = remainder.find(part) else {
            return false;
        };
        remainder = &remainder[position + part.len()..];
        seen_any = true;
    }

    starts_with_wildcard || ends_with_wildcard || pattern.ends_with(remainder)
}

fn is_graphql_body(body: &str, content_type: Option<&str>, path: &str) -> bool {
    if content_type.is_some_and(|value| value.contains("graphql")) {
        return true;
    }
    if path.contains("graphql") {
        return true;
    }
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .is_some_and(|object| object.contains_key("query"))
}

fn normalize_content_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase()
}

fn extract_multipart_boundary(content_type: &str) -> Option<String> {
    content_type
        .split(';')
        .map(str::trim)
        .find_map(|part| {
            part.strip_prefix("boundary=")
                .map(|value| value.trim_matches('"'))
        })
        .map(|boundary| format!("--{boundary}"))
}

fn extract_multipart_name(headers: &str) -> Option<String> {
    headers.split(';').find_map(|segment| {
        segment
            .trim()
            .strip_prefix("name=")
            .map(|name| name.trim_matches('"').to_string())
    })
}

fn headers_to_pairs(headers: &network::Headers) -> Vec<(String, String)> {
    headers
        .inner()
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(name, value)| (name.clone(), value.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn header_value(headers: &network::Headers, needle: &str) -> Option<String> {
    headers.inner().as_object().and_then(|map| {
        map.iter().find_map(|(name, value)| {
            name.eq_ignore_ascii_case(needle)
                .then(|| value.as_str().unwrap_or("").to_string())
        })
    })
}

fn normalize_token(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn is_sensitive_name(name: &str) -> bool {
    let normalized = normalize_token(name);
    normalized.contains("authorization")
        || normalized.contains("cookie")
        || normalized.ends_with("token")
        || normalized.ends_with("secret")
        || normalized.ends_with("password")
        || normalized.ends_with("passwd")
        || normalized.ends_with("session")
        || normalized.ends_with("sessionid")
        || normalized.ends_with("apikey")
        || normalized.ends_with("privatekey")
        || normalized.ends_with("csrftoken")
        || normalized == "csrf"
}

#[cfg(test)]
mod tests {
    use {super::*, serde_json::json};

    fn request_event(
        url: &str,
        method: &str,
        resource_type: &str,
        body: Option<Value>,
    ) -> EventRequestWillBeSent {
        let request_id = format!(
            "req-{method}-{resource_type}-{}",
            url.chars()
                .map(|ch| if ch.is_ascii_alphanumeric() {
                    ch
                } else {
                    '_'
                })
                .collect::<String>()
        );
        let request_body = body.map(|value| {
            json!([{
                "bytes": serde_json::to_string(&value).unwrap_or_default()
            }])
        });
        let mut payload = json!({
            "requestId": request_id,
            "loaderId": "loader-1",
            "documentURL": "https://app.example.com/",
            "request": {
                "url": url,
                "method": method,
                "headers": {
                    "Accept": "application/json",
                    "Authorization": "Bearer secret-token"
                },
                "initialPriority": "High",
                "referrerPolicy": "strict-origin-when-cross-origin"
            },
            "timestamp": 1.0,
            "wallTime": 1700000000.0,
            "initiator": { "type": "other" },
            "redirectHasExtraInfo": false,
            "type": resource_type
        });
        if let Some(entries) = request_body {
            payload["request"]["postDataEntries"] = entries;
            payload["request"]["headers"]["Content-Type"] = json!("application/json");
        }
        serde_json::from_value(payload)
            .unwrap_or_else(|error| panic!("requestWillBeSent JSON should deserialize: {error}"))
    }

    fn response_event(
        request_id: &str,
        url: &str,
        mime_type: &str,
        status: u16,
    ) -> EventResponseReceived {
        serde_json::from_value(json!({
            "requestId": request_id,
            "loaderId": "loader-1",
            "timestamp": 2.0,
            "type": "Fetch",
            "response": {
                "url": url,
                "status": status,
                "statusText": "OK",
                "headers": { "Content-Type": mime_type },
                "mimeType": mime_type,
                "charset": "utf-8",
                "connectionReused": false,
                "connectionId": 1.0,
                "encodedDataLength": 32.0,
                "securityState": "secure"
            },
            "hasExtraInfo": false
        }))
        .unwrap_or_else(|error| panic!("responseReceived JSON should deserialize: {error}"))
    }

    fn loading_failed_event(request_id: &str) -> EventLoadingFailed {
        serde_json::from_value(json!({
            "requestId": request_id,
            "timestamp": 2.0,
            "type": "Fetch",
            "errorText": "net::ERR_ABORTED",
            "canceled": true
        }))
        .unwrap_or_else(|error| panic!("loadingFailed JSON should deserialize: {error}"))
    }

    fn complete_successful_request(
        recorder: &mut ApiCaptureRecorder,
        request_id: &network::RequestId,
        url: &str,
        mime_type: &str,
        status: u16,
    ) {
        recorder.apply_response_received(&response_event(
            request_id.as_ref(),
            url,
            mime_type,
            status,
        ));
        recorder.apply_loading_finished(request_id);
    }

    #[tokio::test]
    async fn capture_catalog_infers_search_endpoint() {
        let mut recorder = ApiCaptureRecorder::new(ApiCaptureConfig::default());
        let event_a = request_event(
            "https://api.example.com/search?q=milk&page=1&limit=20",
            "GET",
            "Fetch",
            None,
        );
        let event_b = request_event(
            "https://api.example.com/search?q=bread&page=2&limit=20",
            "GET",
            "Fetch",
            None,
        );
        let req_id_a = event_a.request_id.clone();
        let req_id_b = event_b.request_id.clone();

        recorder.record_request(&event_a, None).await;
        recorder.record_request(&event_b, None).await;
        complete_successful_request(
            &mut recorder,
            &req_id_a,
            "https://api.example.com/search?q=milk&page=1&limit=20",
            "application/json",
            200,
        );
        complete_successful_request(
            &mut recorder,
            &req_id_b,
            "https://api.example.com/search?q=bread&page=2&limit=20",
            "application/json",
            200,
        );

        let catalog = recorder.build_catalog();
        assert_eq!(catalog.summary.captured_requests, 2);
        assert_eq!(catalog.endpoints.len(), 1);

        let endpoint = &catalog.endpoints[0];
        assert_eq!(endpoint.method, "GET");
        assert_eq!(endpoint.path_template, "/search");
        assert_eq!(endpoint.body_kind, "none");
        assert!(endpoint.semantic_hints.iter().any(|hint| hint == "search"));
        assert!(endpoint.auth.iter().any(|auth| auth.scheme == "bearer"));
        assert!(
            endpoint
                .query_params
                .iter()
                .any(|field| field.name == "q" && field.required)
        );
        assert!(
            endpoint
                .query_params
                .iter()
                .any(|field| field.name == "page"
                    && field.semantic_hints.iter().any(|hint| hint == "page"))
        );
        assert!(
            endpoint
                .examples
                .first()
                .and_then(|example| example.query.get("q"))
                .is_some_and(|values| values == &vec!["milk".to_string()])
        );
        assert_eq!(endpoint.response.statuses, vec![200]);
        assert_eq!(endpoint.response.content_types, vec!["application/json"]);
    }

    #[tokio::test]
    async fn capture_catalog_infers_graphql_operation() {
        let mut recorder = ApiCaptureRecorder::new(ApiCaptureConfig::default());
        let event = request_event(
            "https://api.example.com/graphql",
            "POST",
            "Fetch",
            Some(json!({
                "operationName": "SearchProducts",
                "query": "query SearchProducts($query: String!, $page: Int!) { search(query: $query, page: $page) { id } }",
                "variables": {
                    "query": "milk",
                    "page": 1
                }
            })),
        );
        let request_id = event.request_id.clone();

        recorder.record_request(&event, None).await;
        complete_successful_request(
            &mut recorder,
            &request_id,
            "https://api.example.com/graphql",
            "application/json",
            200,
        );

        let catalog = recorder.build_catalog();
        assert_eq!(catalog.endpoints.len(), 1);

        let endpoint = &catalog.endpoints[0];
        assert_eq!(endpoint.body_kind, "graphql");
        assert_eq!(endpoint.operation_name.as_deref(), Some("SearchProducts"));
        assert!(
            endpoint
                .body
                .as_ref()
                .is_some_and(|body| body.fields.iter().any(|field| field.name == "query"))
        );
        assert!(
            endpoint
                .examples
                .first()
                .and_then(|example| example.body.as_ref())
                .is_some_and(|body| body["variables"]["query"].as_str() == Some("milk"))
        );
    }

    #[test]
    fn capture_catalog_redacts_sensitive_values() {
        let catalog = ApiCaptureRecorder::new(ApiCaptureConfig::default());
        let example = build_request_example(
            &Url::parse("https://api.example.com/search?api_key=secret&q=milk")
                .unwrap_or_else(|error| panic!("url should parse: {error}")),
            &[
                ("api_key".to_string(), "secret".to_string()),
                ("q".to_string(), "milk".to_string()),
            ],
            Some(r#"{"password":"hunter2","query":"milk"}"#),
            parse_request_body(
                Some(r#"{"password":"hunter2","query":"milk"}"#),
                Some("application/json"),
                &Url::parse("https://api.example.com/search")
                    .unwrap_or_else(|error| panic!("url should parse: {error}")),
            )
            .as_ref(),
        );
        drop(catalog);

        assert_eq!(
            example.query.get("api_key"),
            Some(&vec!["[REDACTED]".to_string()])
        );
        assert_eq!(
            example
                .body
                .as_ref()
                .and_then(|body| body["password"].as_str()),
            Some("[REDACTED]")
        );
        assert_eq!(
            example
                .body
                .as_ref()
                .and_then(|body| body["query"].as_str()),
            Some("milk")
        );
        assert!(example.redacted_url.contains("api_key=%5BREDACTED%5D"));
        assert!(!example.redacted_url.contains("api_key=secret"));
        assert_eq!(example.url, "/search");
    }

    #[tokio::test]
    async fn loading_failed_flushes_pending_request() {
        let mut recorder = ApiCaptureRecorder::new(ApiCaptureConfig::default());
        let event = request_event(
            "https://api.example.com/search?q=milk",
            "GET",
            "Fetch",
            None,
        );
        let request_id = event.request_id.clone();
        recorder.record_request(&event, None).await;
        recorder.apply_loading_failed(&loading_failed_event(request_id.as_ref()));

        let catalog = recorder.build_catalog();
        assert_eq!(catalog.summary.captured_requests, 1);
        assert_eq!(catalog.endpoints.len(), 1);
    }

    #[tokio::test]
    async fn url_patterns_filter_capture() {
        let mut recorder = ApiCaptureRecorder::new(ApiCaptureConfig {
            url_patterns: vec!["*graphql*".to_string()],
            ..ApiCaptureConfig::default()
        });
        recorder
            .record_request(
                &request_event(
                    "https://api.example.com/search?q=milk",
                    "GET",
                    "Fetch",
                    None,
                ),
                None,
            )
            .await;
        recorder.finish();

        let catalog = recorder.build_catalog();
        assert_eq!(catalog.summary.captured_requests, 0);
        assert!(catalog.endpoints.is_empty());
    }

    #[tokio::test]
    async fn document_requests_are_opt_in() {
        let event = request_event("https://app.example.com/dashboard", "GET", "Document", None);
        let request_id = event.request_id.clone();

        let mut excluded = ApiCaptureRecorder::new(ApiCaptureConfig::default());
        excluded.record_request(&event, None).await;
        complete_successful_request(
            &mut excluded,
            &request_id,
            "https://app.example.com/dashboard",
            "text/html",
            200,
        );
        let excluded_catalog = excluded.build_catalog();
        assert_eq!(excluded_catalog.summary.captured_requests, 0);
        assert!(excluded_catalog.endpoints.is_empty());

        let mut included = ApiCaptureRecorder::new(ApiCaptureConfig {
            include_document_requests: true,
            ..ApiCaptureConfig::default()
        });
        included.record_request(&event, None).await;
        complete_successful_request(
            &mut included,
            &request_id,
            "https://app.example.com/dashboard",
            "text/html",
            200,
        );
        let included_catalog = included.build_catalog();
        assert_eq!(included_catalog.summary.captured_requests, 1);
        assert_eq!(included_catalog.endpoints.len(), 1);
    }

    #[tokio::test]
    async fn max_examples_per_endpoint_is_capped() {
        let mut recorder = ApiCaptureRecorder::new(ApiCaptureConfig {
            max_examples_per_endpoint: 1,
            ..ApiCaptureConfig::default()
        });
        let event_a = request_event(
            "https://api.example.com/search?q=milk&page=1",
            "GET",
            "Fetch",
            None,
        );
        let event_b = request_event(
            "https://api.example.com/search?q=bread&page=2",
            "GET",
            "Fetch",
            None,
        );
        let req_id_a = event_a.request_id.clone();
        let req_id_b = event_b.request_id.clone();

        recorder.record_request(&event_a, None).await;
        recorder.record_request(&event_b, None).await;
        complete_successful_request(
            &mut recorder,
            &req_id_a,
            "https://api.example.com/search?q=milk&page=1",
            "application/json",
            200,
        );
        complete_successful_request(
            &mut recorder,
            &req_id_b,
            "https://api.example.com/search?q=bread&page=2",
            "application/json",
            200,
        );

        let catalog = recorder.build_catalog();
        assert_eq!(catalog.endpoints.len(), 1);
        assert_eq!(catalog.endpoints[0].examples.len(), 1);
    }

    #[tokio::test]
    async fn repeated_query_params_are_preserved_and_marked_repeated() {
        let mut recorder = ApiCaptureRecorder::new(ApiCaptureConfig::default());
        let event = request_event(
            "https://api.example.com/search?q=milk&filter=fresh&filter=organic",
            "GET",
            "Fetch",
            None,
        );
        let request_id = event.request_id.clone();

        recorder.record_request(&event, None).await;
        complete_successful_request(
            &mut recorder,
            &request_id,
            "https://api.example.com/search?q=milk&filter=fresh&filter=organic",
            "application/json",
            200,
        );

        let catalog = recorder.build_catalog();
        let endpoint = &catalog.endpoints[0];
        assert!(
            endpoint
                .query_params
                .iter()
                .any(|field| field.name == "filter" && field.required && field.repeated)
        );
        assert_eq!(
            endpoint.examples[0].query.get("filter"),
            Some(&vec!["fresh".to_string(), "organic".to_string()])
        );
        assert!(endpoint.examples[0].redacted_url.contains("filter=fresh"));
        assert!(endpoint.examples[0].redacted_url.contains("filter=organic"));
    }

    #[tokio::test]
    async fn out_of_order_events_are_reconciled() {
        let mut recorder = ApiCaptureRecorder::new(ApiCaptureConfig::default());
        let event = request_event(
            "https://api.example.com/search?q=milk",
            "GET",
            "Fetch",
            None,
        );
        let request_id = event.request_id.clone();

        recorder.apply_response_received(&response_event(
            request_id.as_ref(),
            "https://api.example.com/search?q=milk",
            "application/json",
            200,
        ));
        recorder.apply_loading_finished(&request_id);
        recorder.record_request(&event, None).await;

        let catalog = recorder.build_catalog();
        assert_eq!(catalog.summary.captured_requests, 1);
        assert_eq!(catalog.endpoints.len(), 1);
        assert_eq!(catalog.endpoints[0].response.statuses, vec![200]);
        assert_eq!(
            catalog.endpoints[0].response.content_types,
            vec!["application/json"]
        );
    }

    #[tokio::test]
    async fn fallback_request_body_infers_graphql_operation() {
        let mut recorder = ApiCaptureRecorder::new(ApiCaptureConfig::default());
        let mut event = request_event("https://api.example.com/graphql", "POST", "Fetch", None);
        event.request.has_post_data = Some(true);
        let request_id = event.request_id.clone();
        let fallback_body = BASE64_STANDARD.encode(
            serde_json::to_vec(&json!({
                "query": "query SearchProducts($query: String!) { search(query: $query) { id } }",
                "variables": {
                    "query": "milk"
                }
            }))
            .unwrap_or_else(|error| panic!("json body should serialize: {error}")),
        );

        recorder.record_request(&event, Some(fallback_body)).await;
        complete_successful_request(
            &mut recorder,
            &request_id,
            "https://api.example.com/graphql",
            "application/json",
            200,
        );

        let catalog = recorder.build_catalog();
        assert_eq!(catalog.endpoints.len(), 1);
        let endpoint = &catalog.endpoints[0];
        assert_eq!(endpoint.body_kind, "graphql");
        assert_eq!(endpoint.operation_name.as_deref(), Some("SearchProducts"));
        assert!(
            endpoint
                .body
                .as_ref()
                .is_some_and(|body| body.fields.iter().any(|field| field.name == "query"))
        );
    }

    #[tokio::test]
    async fn allowed_hosts_filter_capture() {
        let mut recorder = ApiCaptureRecorder::new(ApiCaptureConfig {
            allowed_hosts: vec!["api.example.com".to_string()],
            ..ApiCaptureConfig::default()
        });
        recorder
            .record_request(
                &request_event(
                    "https://api.example.com/search?q=milk",
                    "GET",
                    "Fetch",
                    None,
                ),
                None,
            )
            .await;
        recorder
            .record_request(
                &request_event(
                    "https://cdn.example.net/search?q=milk",
                    "GET",
                    "Fetch",
                    None,
                ),
                None,
            )
            .await;
        recorder.finish();

        let catalog = recorder.build_catalog();
        assert_eq!(catalog.summary.captured_requests, 1);
        assert_eq!(catalog.summary.hosts, vec!["https://api.example.com"]);
    }

    #[tokio::test]
    async fn non_public_targets_are_excluded_from_capture() {
        // Use a non-loopback private IP; literal loopback IPs are allowed in
        // test builds so integration tests can navigate to local servers.
        let mut recorder = ApiCaptureRecorder::new(ApiCaptureConfig {
            allowed_hosts: vec!["10.0.0.5".to_string()],
            ..ApiCaptureConfig::default()
        });
        recorder
            .record_request(
                &request_event("http://10.0.0.5/admin", "GET", "Fetch", None),
                None,
            )
            .await;
        recorder.finish();

        let catalog = recorder.build_catalog();
        assert_eq!(catalog.summary.captured_requests, 0);
        assert!(catalog.endpoints.is_empty());
    }

    #[test]
    fn agent_summary_is_bounded_and_shape_only() {
        let catalog = ApiCatalog {
            summary: ApiCatalogSummary {
                captured_requests: 64,
                endpoint_count: MAX_AGENT_SUMMARY_ENDPOINTS + 5,
                hosts: (0..(MAX_AGENT_SUMMARY_HOSTS + 2))
                    .map(|index| format!("https://api-{index}.example.com"))
                    .collect(),
            },
            endpoints: (0..(MAX_AGENT_SUMMARY_ENDPOINTS + 5))
                .map(|index| ApiEndpoint {
                    method: "GET".to_string(),
                    origin: format!("https://api-{}.example.com", index % 7),
                    path_template: format!("/v1/resource/{index}"),
                    body_kind: "json".to_string(),
                    operation_name: None,
                    auth: Vec::new(),
                    query_params: (0..(MAX_AGENT_SUMMARY_FIELDS + 3))
                        .map(|field_index| ApiFieldShape {
                            name: format!("query_{field_index}"),
                            required: true,
                            repeated: false,
                            types: vec!["string".to_string()],
                            semantic_hints: Vec::new(),
                        })
                        .collect(),
                    body: Some(ApiBodyShape {
                        kind: "json".to_string(),
                        content_types: vec!["application/json".to_string()],
                        fields: (0..(MAX_AGENT_SUMMARY_FIELDS + 4))
                            .map(|field_index| ApiFieldShape {
                                name: format!("body_{field_index}"),
                                required: false,
                                repeated: false,
                                types: vec!["string".to_string()],
                                semantic_hints: Vec::new(),
                            })
                            .collect(),
                    }),
                    response: ApiResponseMeta {
                        statuses: vec![200],
                        content_types: vec!["application/json".to_string()],
                    },
                    semantic_hints: Vec::new(),
                    examples: vec![ApiRequestExample {
                        redacted_url: format!("https://api.example.com/v1/resource/{index}"),
                        url: format!("/v1/resource/{index}"),
                        query: BTreeMap::from([(
                            "token".to_string(),
                            vec!["[REDACTED]".to_string()],
                        )]),
                        body: Some(json!({ "token": "[REDACTED]" })),
                    }],
                })
                .collect(),
        };

        let summary = catalog.to_agent_summary();
        let encoded = serde_json::to_vec(&summary)
            .unwrap_or_else(|error| panic!("summary should serialize: {error}"));

        assert!(summary.truncated);
        assert!(summary.endpoints.len() <= MAX_AGENT_SUMMARY_ENDPOINTS);
        assert!(summary.summary.hosts.len() <= MAX_AGENT_SUMMARY_HOSTS);
        assert!(
            summary
                .endpoints
                .iter()
                .all(|endpoint| endpoint.examples.is_empty())
        );
        assert!(
            summary
                .endpoints
                .iter()
                .all(|endpoint| endpoint.query_params.len() <= MAX_AGENT_SUMMARY_FIELDS)
        );
        assert!(summary.endpoints.iter().all(|endpoint| {
            endpoint
                .body
                .as_ref()
                .is_none_or(|body| body.fields.len() <= MAX_AGENT_SUMMARY_FIELDS)
        }));
        assert!(summary.omitted_counts.hosts > 0);
        assert!(summary.omitted_counts.endpoints > 0);
        assert!(summary.omitted_counts.fields > 0);
        assert!(encoded.len() <= MAX_AGENT_SUMMARY_BYTES);
    }
}
