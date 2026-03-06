//! Passive API traffic capture and endpoint catalog inference.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use {
    chromiumoxide::cdp::browser_protocol::network::{
        self, EventLoadingFailed, EventRequestWillBeSent, EventResponseReceived,
    },
    serde::Serialize,
    serde_json::{Map, Value},
    tokio::task::JoinHandle,
    url::{Url, form_urlencoded},
    uuid::Uuid,
};

const DEFAULT_MAX_EXAMPLES_PER_ENDPOINT: usize = 3;

#[derive(Debug, Clone)]
pub struct ApiCaptureConfig {
    pub url_patterns: Vec<String>,
    pub include_document_requests: bool,
    pub max_examples_per_endpoint: usize,
}

impl Default for ApiCaptureConfig {
    fn default() -> Self {
        Self {
            url_patterns: Vec::new(),
            include_document_requests: false,
            max_examples_per_endpoint: DEFAULT_MAX_EXAMPLES_PER_ENDPOINT,
        }
    }
}

#[derive(Debug, Default)]
pub struct ApiCaptureRuntime {
    pub config: Option<ApiCaptureConfig>,
    pub recorder: Option<ApiCaptureRecorder>,
    pub tasks: Vec<JoinHandle<()>>,
}

#[derive(Debug)]
pub struct ApiCaptureSnapshot {
    pub config: ApiCaptureConfig,
    pub recorder: ApiCaptureRecorder,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ApiCatalog {
    pub summary: ApiCatalogSummary,
    pub endpoints: Vec<ApiEndpoint>,
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
    pub url: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub query: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

#[derive(Debug)]
pub struct ApiCaptureRecorder {
    config: ApiCaptureConfig,
    observations: Vec<ObservedRequest>,
    pending_requests: HashMap<network::RequestId, ObservedRequest>,
}

impl ApiCaptureRecorder {
    pub fn new(config: ApiCaptureConfig) -> Self {
        Self {
            config,
            observations: Vec::new(),
            pending_requests: HashMap::new(),
        }
    }

    pub fn record_request(&mut self, event: &EventRequestWillBeSent) {
        let Some(observed) = ObservedRequest::from_request_event(event) else {
            return;
        };
        if !should_capture_request(
            &self.config,
            observed.url.as_str(),
            observed.resource_type.as_deref(),
        ) {
            return;
        }
        self.pending_requests
            .insert(event.request_id.clone(), observed);
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
        }
    }

    pub fn apply_response_received(&mut self, event: &EventResponseReceived) {
        if let Some(observed) = self.pending_requests.get_mut(&event.request_id) {
            observed.apply_response_received(event);
        }
    }

    pub fn apply_loading_finished(&mut self, request_id: &network::RequestId) {
        if let Some(observed) = self.pending_requests.remove(request_id) {
            self.observations.push(observed);
        }
    }

    pub fn apply_loading_failed(&mut self, event: &EventLoadingFailed) {
        if let Some(observed) = self.pending_requests.remove(&event.request_id) {
            self.observations.push(observed);
        }
    }

    pub fn finish(&mut self) {
        for (_, observed) in self.pending_requests.drain() {
            self.observations.push(observed);
        }
    }

    #[must_use]
    pub fn build_catalog(&self) -> ApiCatalog {
        let mut grouped = BTreeMap::<EndpointGroupKey, EndpointAccumulator>::new();
        let mut hosts = BTreeSet::new();

        for observed in &self.observations {
            let Some(parsed) = ParsedObservation::from_observed(observed) else {
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
                endpoint_count: self
                    .observations
                    .is_empty()
                    .then_some(0)
                    .unwrap_or_else(|| grouped_endpoint_count(&endpoints)),
                hosts: hosts.into_iter().collect(),
            },
            endpoints,
        }
    }
}

fn grouped_endpoint_count(endpoints: &[ApiEndpoint]) -> usize {
    endpoints.len()
}

#[derive(Debug, Clone)]
struct ObservedRequest {
    method: String,
    url: String,
    request_headers: Vec<(String, String)>,
    request_body: Option<String>,
    request_content_type: Option<String>,
    resource_type: Option<String>,
    status: Option<u16>,
    response_content_type: Option<String>,
}

impl ObservedRequest {
    fn from_request_event(event: &EventRequestWillBeSent) -> Option<Self> {
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
            });

        Some(Self {
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

    fn apply_response_received(&mut self, event: &EventResponseReceived) {
        self.status = Some(event.response.status.clamp(0, u16::MAX as i64) as u16);
        self.response_content_type = Some(normalize_content_type(&event.response.mime_type));
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
            types: field.types.into_iter().collect(),
            semantic_hints: field.semantic_hints.into_iter().collect(),
        })
        .collect()
}

#[derive(Debug, Default)]
struct FieldAccumulator {
    present_count: usize,
    types: BTreeSet<String>,
    semantic_hints: BTreeSet<String>,
}

impl FieldAccumulator {
    fn record(&mut self, field: &ObservedField) {
        self.present_count = self.present_count.saturating_add(1);
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
    fn from_observed(observed: &ObservedRequest) -> Option<Self> {
        let url = Url::parse(&observed.url).ok()?;
        let origin = url.origin().ascii_serialization();
        let path_template = normalize_path_template(&url);
        let query_pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        let query_fields = query_pairs
            .iter()
            .map(|(name, _)| ObservedField::new(name.clone(), "string"))
            .collect::<Vec<_>>();

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

        let example = build_request_example(&url, observed.request_body.as_deref(), body.as_ref());

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
    semantic_hints: BTreeSet<String>,
}

impl ObservedField {
    fn new(name: String, value_type: &str) -> Self {
        Self {
            semantic_hints: collect_semantic_hints_for_name(&name),
            name,
            value_type: value_type.to_string(),
        }
    }
}

fn build_request_example(
    url: &Url,
    raw_body: Option<&str>,
    body: Option<&ParsedBody>,
) -> ApiRequestExample {
    let mut query = BTreeMap::new();
    for (name, value) in url.query_pairs() {
        let name = name.into_owned();
        let redacted = if is_sensitive_name(&name) {
            "[REDACTED]".to_string()
        } else {
            value.into_owned()
        };
        query.insert(name, redacted);
    }

    let body = match body {
        Some(parsed) if parsed.kind == "text" => {
            raw_body.map(|value| Value::String(value.to_string()))
        },
        Some(_) => raw_body.and_then(redacted_body_example),
        None => None,
    };

    ApiRequestExample {
        url: url.as_str().to_string(),
        query,
        body,
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
            let redacted = if is_sensitive_name(&name) {
                "[REDACTED]".to_string()
            } else {
                value.into_owned()
            };
            object.insert(name, Value::String(redacted));
        }
        return Some(Value::Object(object));
    }

    if body.trim().is_empty() {
        return None;
    }

    Some(Value::String(body.to_string()))
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
    let fields = form_urlencoded::parse(body.as_bytes())
        .map(|(name, _)| ObservedField::new(name.into_owned(), "string"))
        .collect();

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

fn should_capture_request(
    config: &ApiCaptureConfig,
    url: &str,
    resource_type: Option<&str>,
) -> bool {
    if !matches_url_patterns(url, &config.url_patterns) {
        return false;
    }

    match resource_type {
        Some("Fetch" | "XHR" | "EventSource" | "Other") | None => true,
        Some("Document") => config.include_document_requests,
        _ => false,
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

    #[test]
    fn capture_catalog_infers_search_endpoint() {
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

        recorder.record_request(&event_a);
        recorder.record_request(&event_b);
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
        assert_eq!(endpoint.response.statuses, vec![200]);
        assert_eq!(endpoint.response.content_types, vec!["application/json"]);
    }

    #[test]
    fn capture_catalog_infers_graphql_operation() {
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

        recorder.record_request(&event);
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
            example.query.get("api_key").map(String::as_str),
            Some("[REDACTED]")
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
    }

    #[test]
    fn loading_failed_flushes_pending_request() {
        let mut recorder = ApiCaptureRecorder::new(ApiCaptureConfig::default());
        let event = request_event(
            "https://api.example.com/search?q=milk",
            "GET",
            "Fetch",
            None,
        );
        let request_id = event.request_id.clone();
        recorder.record_request(&event);
        recorder.apply_loading_failed(&loading_failed_event(request_id.as_ref()));

        let catalog = recorder.build_catalog();
        assert_eq!(catalog.summary.captured_requests, 1);
        assert_eq!(catalog.endpoints.len(), 1);
    }

    #[test]
    fn url_patterns_filter_capture() {
        let mut recorder = ApiCaptureRecorder::new(ApiCaptureConfig {
            url_patterns: vec!["*graphql*".to_string()],
            ..ApiCaptureConfig::default()
        });
        recorder.record_request(&request_event(
            "https://api.example.com/search?q=milk",
            "GET",
            "Fetch",
            None,
        ));
        recorder.finish();

        let catalog = recorder.build_catalog();
        assert_eq!(catalog.summary.captured_requests, 0);
        assert!(catalog.endpoints.is_empty());
    }

    #[test]
    fn document_requests_are_opt_in() {
        let event = request_event("https://app.example.com/dashboard", "GET", "Document", None);
        let request_id = event.request_id.clone();

        let mut excluded = ApiCaptureRecorder::new(ApiCaptureConfig::default());
        excluded.record_request(&event);
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
        included.record_request(&event);
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

    #[test]
    fn max_examples_per_endpoint_is_capped() {
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

        recorder.record_request(&event_a);
        recorder.record_request(&event_b);
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
}
