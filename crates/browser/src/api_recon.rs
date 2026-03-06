//! API reconnaissance store, classification, and scoring.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde_json::{Map, Value};
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use crate::api_recon_inference::{AggregateNode, aggregate_to_public_node, merge_optional_node};
use crate::api_recon_types::{
    ApiAuthSignals, ApiContractNode, ApiDataHints, ApiEndpointContract, ApiEndpointList,
    ApiEndpointRole, ApiEndpointSummary, ApiExampleRef, ApiFieldContract, ApiHeaderContract,
    ApiObjectContract, ApiObservationDelta, ApiObservationMarker, ApiObservedExchange,
    ApiObservedRequest, ApiObservedRequestInput, ApiObservedResponse, ApiObservedResponseInput,
    ApiOpaqueReason, ApiPaginationHints, ApiPaginationStyle, ApiProtocol, ApiReconMode,
    ApiReconStats, ApiReconStatus, ApiRequestContract, ApiRequestTemplate, ApiResponseContract,
    ApiScalarKind, EndpointIdentity,
};

pub const MAX_TRACKED_ENDPOINTS: usize = 2_000;
pub const MAX_PENDING_REQUESTS: usize = 2_000;
pub const MAX_TRACKED_EXCHANGES: usize = 4_000;
pub const MAX_TRACKED_MARKERS: usize = 256;
pub const MAX_COLLECT_ITEMS: usize = 5_000;

#[derive(Debug, Default)]
pub struct ApiReconStore {
    endpoints: BTreeMap<String, ApiEndpointAggregate>,
    exchanges: VecDeque<ApiObservedExchange>,
    markers: VecDeque<ApiObservationMarker>,
    stats: ApiReconStats,
    degraded_reasons: BTreeSet<String>,
    last_network_activity_at: Option<OffsetDateTime>,
}

#[derive(Debug)]
struct ApiEndpointAggregate {
    summary: ApiEndpointSummary,
    query_keys: KeyPresenceAggregate,
    request_headers: KeyPresenceAggregate,
    request_content_types: BTreeMap<String, u64>,
    request_body: Option<AggregateNode>,
    last_request_headers: BTreeMap<String, String>,
    last_request_body: Option<Value>,
    responses: BTreeMap<u16, ApiResponseAggregate>,
    examples: VecDeque<ApiExampleRef>,
}

#[derive(Debug)]
struct ApiResponseAggregate {
    sample_count: u64,
    headers: KeyPresenceAggregate,
    content_types: BTreeMap<String, u64>,
    body: Option<AggregateNode>,
}

impl Default for ApiResponseAggregate {
    fn default() -> Self {
        Self {
            sample_count: 0,
            headers: KeyPresenceAggregate::default(),
            content_types: BTreeMap::new(),
            body: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct KeyPresenceAggregate {
    sample_count: u64,
    keys: BTreeMap<String, u64>,
}

impl ApiReconStore {
    pub fn clear(&mut self) {
        self.endpoints.clear();
        self.exchanges.clear();
        self.markers.clear();
        self.stats = ApiReconStats::default();
        self.degraded_reasons.clear();
        self.last_network_activity_at = None;
    }

    pub fn note_pending_drop(&mut self) {
        self.stats.dropped_pending_requests = self.stats.dropped_pending_requests.saturating_add(1);
    }

    pub fn note_capture_failure(&mut self, reason: impl Into<String>) {
        self.degraded_reasons.insert(reason.into());
    }

    pub fn note_network_activity(&mut self, at: OffsetDateTime) {
        self.last_network_activity_at = Some(at);
    }

    pub fn last_network_activity_at(&self) -> Option<OffsetDateTime> {
        self.last_network_activity_at
    }

    pub fn mark(&mut self, tab_id: &str, label: Option<String>) -> ApiObservationMarker {
        let marker = ApiObservationMarker {
            marker_id: format!(
                "mk_{}",
                OffsetDateTime::now_utc()
                    .unix_timestamp_nanos()
                    .unsigned_abs()
            ),
            label,
            tab_id: tab_id.to_string(),
            created_at: OffsetDateTime::now_utc(),
        };
        self.markers.push_back(marker.clone());
        while self.markers.len() > MAX_TRACKED_MARKERS {
            let _ = self.markers.pop_front();
        }
        marker
    }

    pub fn status(&self, mode: ApiReconMode, pending_requests: usize) -> ApiReconStatus {
        ApiReconStatus {
            mode,
            healthy: self.degraded_reasons.is_empty(),
            degraded_reasons: self.degraded_reasons.iter().cloned().collect(),
            total_endpoints: self.endpoints.len(),
            recent_exchange_count: self.exchanges.len(),
            pending_requests,
            stats: self.stats.clone(),
        }
    }

    pub fn marker(&self, marker_id: &str) -> Option<ApiObservationMarker> {
        self.markers
            .iter()
            .find(|marker| marker.marker_id == marker_id)
            .cloned()
    }

    pub fn record(
        &mut self,
        request: ApiObservedRequestInput,
        response: ApiObservedResponseInput,
    ) -> Option<ApiObservedExchange> {
        self.stats.observed_requests = self.stats.observed_requests.saturating_add(1);
        let Some(identity) = endpoint_identity(
            &request.method,
            &request.url,
            request.content_type.as_deref(),
            request.operation_name.as_deref(),
        ) else {
            self.stats.invalid_urls = self.stats.invalid_urls.saturating_add(1);
            return None;
        };

        let request_body = request.body_schema.clone();
        let response_body = response.body_schema.clone();

        update_stats(&mut self.stats, request_body.as_ref());
        update_stats(&mut self.stats, response_body.as_ref());

        if !self.endpoints.contains_key(&identity.endpoint_id)
            && self.endpoints.len() >= MAX_TRACKED_ENDPOINTS
        {
            self.stats.dropped_endpoints = self.stats.dropped_endpoints.saturating_add(1);
            return None;
        }

        let summary = ApiEndpointSummary {
            endpoint_id: identity.endpoint_id.clone(),
            protocol: identity.protocol,
            role: identity.role,
            method: request.method.to_ascii_uppercase(),
            host: identity.host.clone(),
            route_template: identity.route_template.clone(),
            operation_name: identity.operation_name.clone(),
            sample_count: 0,
            data_score: 0.0,
            last_seen_at: response.finished_at,
        };

        let aggregate = self
            .endpoints
            .entry(identity.endpoint_id.clone())
            .or_insert_with(|| ApiEndpointAggregate {
                summary,
                query_keys: KeyPresenceAggregate::default(),
                request_headers: KeyPresenceAggregate::default(),
                request_content_types: BTreeMap::new(),
                request_body: None,
                last_request_headers: BTreeMap::new(),
                last_request_body: None,
                responses: BTreeMap::new(),
                examples: VecDeque::new(),
            });

        aggregate.summary.sample_count = aggregate.summary.sample_count.saturating_add(1);
        aggregate.summary.last_seen_at = response.finished_at;
        aggregate
            .query_keys
            .observe(request.query_keys.iter().cloned());
        aggregate
            .request_headers
            .observe(request.header_keys.iter().cloned());
        aggregate.last_request_headers = request.header_values.clone();
        if let Some(content_type) = request.content_type.as_deref() {
            *aggregate
                .request_content_types
                .entry(content_type.to_string())
                .or_insert(0) += 1;
        }
        if let Some(body) = request_body.as_ref() {
            merge_optional_node(&mut aggregate.request_body, body);
        }
        aggregate.last_request_body = request.body_value.clone();

        if let Some(status) = response.status {
            let entry = aggregate
                .responses
                .entry(status)
                .or_insert_with(ApiResponseAggregate::default);
            entry.sample_count = entry.sample_count.saturating_add(1);
            entry.headers.observe(response.header_keys.iter().cloned());
            if let Some(content_type) = response.content_type.as_deref() {
                *entry
                    .content_types
                    .entry(content_type.to_string())
                    .or_insert(0) += 1;
            }
            if let Some(body) = response_body.as_ref() {
                merge_optional_node(&mut entry.body, body);
            }
        }

        let exchange = ApiObservedExchange {
            exchange_id: format!(
                "ex_{}",
                response.finished_at.unix_timestamp_nanos().unsigned_abs()
            ),
            marker_id: request.marker_id.clone(),
            tab_id: request.tab_id.clone(),
            endpoint_id: identity.endpoint_id.clone(),
            started_at: request.started_at,
            finished_at: Some(response.finished_at),
            request: ApiObservedRequest {
                method: request.method.clone(),
                url: request.url.clone(),
                operation_name: request.operation_name.clone(),
                query_keys: request.query_keys.iter().cloned().collect(),
                header_keys: request.header_keys.iter().cloned().collect(),
                content_type: request.content_type.clone(),
                body_schema: request_body,
            },
            response: ApiObservedResponse {
                status: response.status,
                header_keys: response.header_keys.iter().cloned().collect(),
                content_type: response.content_type.clone(),
                body_schema: response_body,
            },
        };

        aggregate.examples.push_back(ApiExampleRef {
            exchange_id: exchange.exchange_id.clone(),
            status: exchange.response.status,
        });
        while aggregate.examples.len() > 5 {
            let _ = aggregate.examples.pop_front();
        }

        aggregate.summary.data_score = compute_data_score(aggregate);

        self.exchanges.push_back(exchange.clone());
        while self.exchanges.len() > MAX_TRACKED_EXCHANGES {
            let _ = self.exchanges.pop_front();
        }
        self.last_network_activity_at = Some(response.finished_at);

        Some(exchange)
    }

    pub fn diff(&self, marker_id: &str) -> Option<ApiObservationDelta> {
        let marker = self.marker(marker_id)?;
        let exchanges = self
            .exchanges
            .iter()
            .filter(|exchange| exchange.started_at >= marker.created_at)
            .cloned()
            .collect::<Vec<_>>();
        let mut endpoint_ids = BTreeSet::new();
        endpoint_ids.extend(
            exchanges
                .iter()
                .map(|exchange| exchange.endpoint_id.clone()),
        );
        let endpoints = endpoint_ids
            .into_iter()
            .filter_map(|endpoint_id| self.endpoints.get(&endpoint_id))
            .map(ApiEndpointAggregate::summary)
            .collect();

        Some(ApiObservationDelta {
            marker_id: marker_id.to_string(),
            endpoints,
            exchanges,
        })
    }

    pub fn list_endpoints(
        &self,
        since: Option<&str>,
        limit: u32,
        data_only: bool,
    ) -> ApiEndpointList {
        let since_marker = since.and_then(|id| self.marker(id));
        let since_time = since_marker.as_ref().map(|marker| marker.created_at);
        let mut endpoints = self
            .endpoints
            .values()
            .filter(|endpoint| {
                (!data_only || endpoint.summary.data_score >= 0.45)
                    && since_time
                        .is_none_or(|created_at| endpoint.summary.last_seen_at >= created_at)
            })
            .map(ApiEndpointAggregate::summary)
            .collect::<Vec<_>>();

        endpoints.sort_by(|left, right| {
            right
                .data_score
                .total_cmp(&left.data_score)
                .then_with(|| right.sample_count.cmp(&left.sample_count))
                .then_with(|| left.endpoint_id.cmp(&right.endpoint_id))
        });
        endpoints.truncate(usize::try_from(limit).unwrap_or(usize::MAX));

        ApiEndpointList { endpoints }
    }

    pub fn endpoint_contract(&self, endpoint_id: &str) -> Option<ApiEndpointContract> {
        self.endpoints
            .get(endpoint_id)
            .map(ApiEndpointAggregate::contract)
    }

    pub fn endpoint_summary(&self, endpoint_id: &str) -> Option<ApiEndpointSummary> {
        self.endpoints
            .get(endpoint_id)
            .map(ApiEndpointAggregate::summary)
    }

    pub fn endpoint_template(&self, endpoint_id: &str) -> Option<ApiRequestTemplate> {
        self.exchanges
            .iter()
            .rev()
            .find(|exchange| exchange.endpoint_id == endpoint_id)
            .map(|exchange| ApiRequestTemplate {
                method: exchange.request.method.clone(),
                url: exchange.request.url.clone(),
                content_type: exchange.request.content_type.clone(),
                operation_name: exchange.request.operation_name.clone(),
                headers: self
                    .endpoints
                    .get(endpoint_id)
                    .map(|endpoint| endpoint.last_request_headers.clone())
                    .unwrap_or_default(),
                body: self
                    .endpoints
                    .get(endpoint_id)
                    .and_then(|endpoint| endpoint.last_request_body.clone()),
            })
    }
}

// ── Classification ──────────────────────────────────────────────────────────

pub fn endpoint_identity(
    method: &str,
    raw_url: &str,
    content_type: Option<&str>,
    operation_name: Option<&str>,
) -> Option<EndpointIdentity> {
    let parsed = Url::parse(raw_url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    let route_template = normalize_path(parsed.path());
    let protocol = detect_protocol(&parsed, content_type, operation_name);
    let normalized_operation = operation_name
        .map(str::trim)
        .filter(|op| !op.is_empty())
        .map(str::to_string);
    let endpoint_id = match protocol {
        ApiProtocol::GraphQl => format!(
            "{} {}{}#{}",
            method.to_ascii_uppercase(),
            host,
            route_template,
            normalized_operation
                .clone()
                .unwrap_or_else(|| "anonymous".to_string())
        ),
        _ => format!("{} {}{}", method.to_ascii_uppercase(), host, route_template),
    };
    let role = detect_role(method, &route_template, normalized_operation.as_deref());

    Some(EndpointIdentity {
        endpoint_id,
        protocol,
        role,
        host,
        route_template,
        operation_name: normalized_operation,
    })
}

fn detect_protocol(
    parsed: &Url,
    content_type: Option<&str>,
    operation_name: Option<&str>,
) -> ApiProtocol {
    let path = parsed.path().to_ascii_lowercase();
    let ct = content_type.unwrap_or_default().to_ascii_lowercase();
    if path.contains("/graphql") || operation_name.is_some() || ct.contains("graphql") {
        ApiProtocol::GraphQl
    } else if path.contains("/rpc/")
        || path.ends_with("/rpc")
        || path.contains("/svc/")
        || path.contains("/service/")
    {
        ApiProtocol::Rpc
    } else if path.contains("/api/")
        || path == "/api"
        || parsed
            .host_str()
            .is_some_and(|host| host.starts_with("api.") || host.contains(".api."))
    {
        ApiProtocol::Rest
    } else {
        ApiProtocol::Unknown
    }
}

fn detect_role(
    method: &str,
    route_template: &str,
    operation_name: Option<&str>,
) -> ApiEndpointRole {
    let method = method.to_ascii_uppercase();
    let route = route_template.to_ascii_lowercase();
    let operation = operation_name.unwrap_or_default().to_ascii_lowercase();

    if route.contains("/auth")
        || route.contains("/login")
        || route.contains("/logout")
        || operation.contains("login")
        || operation.contains("logout")
    {
        return ApiEndpointRole::Auth;
    }
    if matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE") {
        return ApiEndpointRole::Mutation;
    }
    if route.ends_with("/:id")
        || route.ends_with("/:uuid")
        || route.ends_with("/:slug")
        || operation.contains("detail")
        || operation.contains("get")
    {
        return ApiEndpointRole::EntityRead;
    }
    if method == "GET" {
        return ApiEndpointRole::CollectionRead;
    }
    ApiEndpointRole::Unknown
}

pub fn normalize_path(path: &str) -> String {
    let raw_parts = path
        .trim()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let mut parts = Vec::new();
    for (index, segment) in raw_parts.iter().enumerate() {
        let normalized = if is_numeric_segment(segment) {
            ":id".to_string()
        } else if Uuid::parse_str(segment).is_ok() {
            ":uuid".to_string()
        } else if is_opaque_token(segment) {
            ":token".to_string()
        } else if looks_like_slug(segment, raw_parts.get(index.wrapping_sub(1)).copied()) {
            ":slug".to_string()
        } else {
            (*segment).to_string()
        };
        parts.push(normalized);
    }

    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

fn is_numeric_segment(segment: &str) -> bool {
    segment.chars().all(|ch| ch.is_ascii_digit())
}

fn is_opaque_token(segment: &str) -> bool {
    segment.len() >= 12
        && (segment.chars().all(|ch| ch.is_ascii_hexdigit())
            || segment
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')))
}

fn looks_like_slug(segment: &str, previous: Option<&str>) -> bool {
    let is_plain_slug = segment.len() >= 3
        && segment
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
    if !is_plain_slug {
        return false;
    }
    let Some(previous) = previous else {
        return false;
    };
    if !previous.ends_with('s') {
        return false;
    }
    !matches!(
        segment,
        "search"
            | "list"
            | "count"
            | "stats"
            | "summary"
            | "details"
            | "create"
            | "update"
            | "delete"
            | "login"
            | "logout"
    )
}

// ── Scoring ─────────────────────────────────────────────────────────────────

fn compute_data_score(aggregate: &ApiEndpointAggregate) -> f32 {
    let mut score: f32 = 0.0;
    match aggregate.summary.role {
        ApiEndpointRole::CollectionRead => score += 0.55,
        ApiEndpointRole::EntityRead => score += 0.35,
        ApiEndpointRole::Mutation => score -= 0.1,
        ApiEndpointRole::Auth => score -= 0.2,
        ApiEndpointRole::Unknown => {},
    }
    if aggregate.summary.method == "GET" {
        score += 0.25;
    }
    let hints = derive_data_hints(aggregate);
    if !hints.item_paths.is_empty() {
        score += 0.35;
    }
    if hints.pagination.is_some() {
        score += 0.1;
    }
    score.clamp(0.0, 1.0)
}

// ── Data hints ──────────────────────────────────────────────────────────────

fn derive_auth_signals(headers: &ApiHeaderContract) -> ApiAuthSignals {
    let mut all_keys = headers.required_keys.clone();
    all_keys.extend(headers.optional_keys.iter().cloned());
    let lower = all_keys
        .iter()
        .map(|key| key.to_ascii_lowercase())
        .collect::<Vec<_>>();
    ApiAuthSignals {
        has_cookie_auth: lower
            .iter()
            .any(|key| key == "cookie" || key == "set-cookie"),
        has_authorization_header: lower
            .iter()
            .any(|key| key == "authorization" || key == "x-api-key"),
        has_csrf_header: lower
            .iter()
            .any(|key| key.contains("csrf") || key.contains("xsrf")),
        header_keys: all_keys,
    }
}

fn derive_data_hints(aggregate: &ApiEndpointAggregate) -> ApiDataHints {
    let mut hints = ApiDataHints::default();
    if let Some(request_body) = aggregate.request_body.as_ref() {
        collect_id_and_filter_fields(request_body, &mut hints.id_fields, &mut hints.filter_fields);
    }
    for query_key in aggregate.query_keys.keys.keys() {
        let lower = query_key.to_ascii_lowercase();
        if lower.contains("sort") {
            hints.sort_fields.push(query_key.clone());
        }
        if lower.contains("filter") || lower == "q" || lower == "search" {
            hints.filter_fields.push(query_key.clone());
        }
    }

    let mut item_paths = BTreeSet::new();
    let mut next_cursor_path = None;
    let mut has_more_path = None;
    for response in aggregate.responses.values() {
        if let Some(body) = response.body.as_ref() {
            collect_array_paths(body, "", &mut item_paths);
            if next_cursor_path.is_none() {
                next_cursor_path =
                    find_first_matching_path(body, "", &["nextCursor", "cursor", "next_cursor"]);
            }
            if has_more_path.is_none() {
                has_more_path = find_first_matching_path(body, "", &["hasMore", "has_more"]);
            }
        }
    }
    hints.item_paths = item_paths.into_iter().collect();

    let query_keys = aggregate
        .query_keys
        .keys
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    hints.pagination = derive_pagination_hints(
        &query_keys,
        &hints.item_paths,
        next_cursor_path,
        has_more_path,
    );
    hints
}

fn derive_pagination_hints(
    query_keys: &BTreeSet<String>,
    item_paths: &[String],
    next_cursor_path: Option<String>,
    has_more_path: Option<String>,
) -> Option<ApiPaginationHints> {
    let lower = query_keys
        .iter()
        .map(|key| key.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();

    if lower.contains("cursor") || lower.contains("next_cursor") {
        return Some(ApiPaginationHints {
            style: ApiPaginationStyle::Cursor,
            request_fields: vec!["cursor".to_string()],
            next_cursor_path,
            has_more_path,
            item_path: item_paths.first().cloned(),
        });
    }
    if lower.contains("page") || lower.contains("page_size") || lower.contains("per_page") {
        return Some(ApiPaginationHints {
            style: ApiPaginationStyle::Page,
            request_fields: vec!["page".to_string(), "page_size".to_string()],
            next_cursor_path: None,
            has_more_path,
            item_path: item_paths.first().cloned(),
        });
    }
    if lower.contains("offset") || lower.contains("limit") {
        return Some(ApiPaginationHints {
            style: ApiPaginationStyle::OffsetLimit,
            request_fields: vec!["offset".to_string(), "limit".to_string()],
            next_cursor_path: None,
            has_more_path,
            item_path: item_paths.first().cloned(),
        });
    }
    None
}

fn collect_id_and_filter_fields(
    node: &AggregateNode,
    id_fields: &mut Vec<String>,
    filter_fields: &mut Vec<String>,
) {
    if let AggregateNode::Object { fields, .. } = node {
        for field in fields.keys() {
            let lower = field.to_ascii_lowercase();
            if lower == "id" || lower.ends_with("_id") {
                id_fields.push(field.clone());
            }
            if lower.contains("filter") || lower.contains("query") || lower == "search" {
                filter_fields.push(field.clone());
            }
        }
    }
}

fn collect_array_paths(node: &AggregateNode, prefix: &str, output: &mut BTreeSet<String>) {
    match node {
        AggregateNode::Array { items } => {
            output.insert(prefix.to_string());
            collect_array_paths(items, prefix, output);
        },
        AggregateNode::Object { fields, .. } => {
            for (field, child) in fields {
                let next = if prefix.is_empty() {
                    format!("/{field}")
                } else {
                    format!("{prefix}/{field}")
                };
                collect_array_paths(&child.node, &next, output);
            }
        },
        AggregateNode::Nullable(value) => collect_array_paths(value, prefix, output),
        AggregateNode::OneOf(nodes) => {
            for node in nodes {
                collect_array_paths(node, prefix, output);
            }
        },
        AggregateNode::Scalar(_) | AggregateNode::Opaque(_) => {},
    }
}

fn find_first_matching_path(node: &AggregateNode, prefix: &str, names: &[&str]) -> Option<String> {
    match node {
        AggregateNode::Object { fields, .. } => {
            for (field, child) in fields {
                let next = if prefix.is_empty() {
                    format!("/{field}")
                } else {
                    format!("{prefix}/{field}")
                };
                if names.iter().any(|name| field.eq_ignore_ascii_case(name)) {
                    return Some(next);
                }
                if let Some(found) = find_first_matching_path(&child.node, &next, names) {
                    return Some(found);
                }
            }
            None
        },
        AggregateNode::Array { items } => find_first_matching_path(items, prefix, names),
        AggregateNode::Nullable(value) => find_first_matching_path(value, prefix, names),
        AggregateNode::OneOf(nodes) => nodes
            .iter()
            .find_map(|node| find_first_matching_path(node, prefix, names)),
        AggregateNode::Scalar(_) | AggregateNode::Opaque(_) => None,
    }
}

// ── Aggregate helpers ───────────────────────────────────────────────────────

impl ApiEndpointAggregate {
    fn summary(&self) -> ApiEndpointSummary {
        self.summary.clone()
    }

    fn contract(&self) -> ApiEndpointContract {
        let query = self.query_keys.to_object_contract();
        let request_headers = self.request_headers.to_header_contract();
        let auth = derive_auth_signals(&request_headers);
        let request_body = self.request_body.as_ref().map(aggregate_to_public_node);

        let responses = self
            .responses
            .iter()
            .map(|(status, response)| {
                (
                    *status,
                    ApiResponseContract {
                        content_type: most_common_key(&response.content_types),
                        headers: response.headers.to_header_contract(),
                        body: response.body.as_ref().map(aggregate_to_public_node),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();

        ApiEndpointContract {
            endpoint_id: self.summary.endpoint_id.clone(),
            protocol: self.summary.protocol,
            role: self.summary.role,
            method: self.summary.method.clone(),
            host: self.summary.host.clone(),
            route_template: self.summary.route_template.clone(),
            operation_name: self.summary.operation_name.clone(),
            auth,
            request: ApiRequestContract {
                content_type: most_common_key(&self.request_content_types),
                query,
                headers: request_headers,
                body: request_body,
            },
            responses,
            data_hints: derive_data_hints(self),
            examples: self.examples.iter().cloned().collect(),
        }
    }
}

impl KeyPresenceAggregate {
    fn observe<I>(&mut self, keys: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.sample_count = self.sample_count.saturating_add(1);
        for key in keys {
            *self.keys.entry(key).or_insert(0) += 1;
        }
    }

    fn to_object_contract(&self) -> ApiObjectContract {
        ApiObjectContract {
            fields: self
                .keys
                .iter()
                .map(|(key, count)| {
                    (
                        key.clone(),
                        ApiFieldContract {
                            schema: ApiContractNode::Scalar {
                                kind: ApiScalarKind::String,
                            },
                            required_rate: if self.sample_count == 0 {
                                0.0
                            } else {
                                *count as f32 / self.sample_count as f32
                            },
                        },
                    )
                })
                .collect(),
        }
    }

    fn to_header_contract(&self) -> ApiHeaderContract {
        let mut required_keys = Vec::new();
        let mut optional_keys = Vec::new();
        for (key, count) in &self.keys {
            let rate = if self.sample_count == 0 {
                0.0
            } else {
                *count as f32 / self.sample_count as f32
            };
            if rate >= 0.95 {
                required_keys.push(key.clone());
            } else {
                optional_keys.push(key.clone());
            }
        }
        ApiHeaderContract {
            required_keys,
            optional_keys,
        }
    }
}

fn update_stats(stats: &mut ApiReconStats, node: Option<&ApiContractNode>) {
    match node {
        Some(ApiContractNode::Opaque {
            reason: ApiOpaqueReason::ParseError,
        }) => {
            stats.parse_failures = stats.parse_failures.saturating_add(1);
        },
        Some(ApiContractNode::Opaque {
            reason: ApiOpaqueReason::NonJson,
        })
        | Some(ApiContractNode::Opaque {
            reason: ApiOpaqueReason::Binary,
        }) => {
            stats.non_json_bodies = stats.non_json_bodies.saturating_add(1);
        },
        Some(ApiContractNode::Opaque {
            reason: ApiOpaqueReason::EmptyBody,
        }) => {
            stats.body_unavailable = stats.body_unavailable.saturating_add(1);
        },
        Some(_) => {
            stats.json_bodies = stats.json_bodies.saturating_add(1);
        },
        None => {
            stats.body_unavailable = stats.body_unavailable.saturating_add(1);
        },
    }
}

fn most_common_key(map: &BTreeMap<String, u64>) -> Option<String> {
    map.iter()
        .max_by(|left, right| left.1.cmp(right.1).then_with(|| right.0.cmp(left.0)))
        .map(|(key, _)| key.clone())
}

// ── Utilities ───────────────────────────────────────────────────────────────

pub fn query_keys_from_url(raw_url: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    if let Ok(parsed) = Url::parse(raw_url) {
        for (key, _) in parsed.query_pairs() {
            let key = key.trim().to_ascii_lowercase();
            if !key.is_empty() {
                keys.insert(key);
            }
        }
    }
    keys
}

pub fn header_keys_from_json(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .map(|map| {
            map.keys()
                .map(|key| key.to_ascii_lowercase())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default()
}

pub fn header_value_from_json(value: &Value, name: &str) -> Option<String> {
    let target = name.to_ascii_lowercase();
    value.as_object().and_then(|map| {
        map.iter().find_map(|(key, value)| {
            if key.eq_ignore_ascii_case(&target) {
                value
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| value.as_i64().map(|n| n.to_string()))
            } else {
                None
            }
        })
    })
}

pub fn operation_name_from_request_body(body: Option<&str>) -> Option<String> {
    let body = body?.trim();
    if body.is_empty() {
        return None;
    }
    let value = serde_json::from_str::<Value>(body).ok()?;
    let operation_name = value
        .get("operationName")
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())?;
    Some(operation_name.to_string())
}

pub fn should_capture(resource_type: &str, raw_url: &str) -> bool {
    let resource_type = resource_type.trim().to_ascii_lowercase();
    if resource_type != "xhr" && resource_type != "fetch" && resource_type != "document" {
        return false;
    }

    let Ok(parsed) = Url::parse(raw_url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }

    let path = parsed.path().to_ascii_lowercase();
    if path.is_empty() || path == "/" {
        return false;
    }
    if is_static_asset_path(&path) {
        return false;
    }
    true
}

fn is_static_asset_path(path: &str) -> bool {
    [
        ".css", ".js", ".mjs", ".map", ".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".ico",
        ".woff", ".woff2", ".ttf", ".eot", ".mp4", ".webm", ".pdf",
    ]
    .iter()
    .any(|ext| path.ends_with(ext))
}

pub fn json_pointer_get(value: &Value, pointer: Option<&str>) -> Option<Value> {
    pointer
        .filter(|p| !p.is_empty())
        .and_then(|p| value.pointer(p))
        .cloned()
        .or_else(|| pointer.is_none().then(|| value.clone()))
}

pub fn query_map_to_pairs(map: &BTreeMap<String, Value>) -> Vec<(String, String)> {
    map.iter()
        .map(|(key, value)| {
            (
                key.clone(),
                match value {
                    Value::String(text) => text.clone(),
                    _ => value.to_string(),
                },
            )
        })
        .collect()
}

pub fn merge_query_pairs(raw_url: &str, overrides: &BTreeMap<String, Value>) -> Option<String> {
    let mut parsed = Url::parse(raw_url).ok()?;
    let mut pairs = parsed
        .query_pairs()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect::<BTreeMap<_, _>>();
    for (key, value) in query_map_to_pairs(overrides) {
        pairs.insert(key, value);
    }

    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(&key, &value);
    }
    let query = serializer.finish();
    parsed.set_query((!query.is_empty()).then_some(query.as_str()));
    Some(parsed.to_string())
}

pub fn infer_response_status(value: &Value) -> Option<u16> {
    value
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
}

pub fn infer_content_type(headers: &Map<String, Value>) -> Option<String> {
    headers.iter().find_map(|(key, value)| {
        if key.eq_ignore_ascii_case("content-type") {
            value.as_str().map(str::to_string)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_recon_inference::infer_contract_from_body;
    use serde_json::json;

    #[test]
    fn normalize_path_rewrites_ids_tokens_and_slugs() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(normalize_path("/api/items/42"), "/api/items/:id");
        assert_eq!(
            normalize_path(&format!("/api/users/{uuid}/detail")),
            "/api/users/:uuid/detail"
        );
        assert_eq!(normalize_path("/api/users/alice"), "/api/users/:slug");
        assert_eq!(
            normalize_path("/api/assets/abc123def456"),
            "/api/assets/:token"
        );
    }

    #[test]
    fn endpoint_identity_uses_graphql_operation_name() {
        let identity = endpoint_identity(
            "POST",
            "https://example.com/graphql",
            Some("application/json"),
            Some("ViewerQuery"),
        )
        .expect("identity should parse");
        assert_eq!(identity.protocol, ApiProtocol::GraphQl);
        assert!(identity.endpoint_id.ends_with("#ViewerQuery"));
    }

    #[test]
    fn should_capture_accepts_api_like_fetches_and_rejects_static_assets() {
        assert!(should_capture(
            "XHR",
            "https://example.com/api/v1/items?page=2"
        ));
        assert!(should_capture(
            "Document",
            "https://example.com/data/bootstrap.json"
        ));
        assert!(!should_capture("XHR", "https://example.com/static/app.js"));
    }

    #[test]
    fn store_diff_groups_exchanges_since_marker() {
        let mut store = ApiReconStore::default();
        let marker = store.mark("main", Some("after-click".to_string()));
        let request = ApiObservedRequestInput {
            method: "GET".to_string(),
            url: "https://example.com/api/items?page=1".to_string(),
            tab_id: "main".to_string(),
            marker_id: Some(marker.marker_id.clone()),
            started_at: OffsetDateTime::now_utc(),
            query_keys: BTreeSet::from(["page".to_string()]),
            header_keys: BTreeSet::from(["accept".to_string()]),
            header_values: BTreeMap::new(),
            content_type: None,
            operation_name: None,
            body_schema: None,
            body_value: None,
        };
        let response = ApiObservedResponseInput {
            status: Some(200),
            header_keys: BTreeSet::from(["content-type".to_string()]),
            content_type: Some("application/json".to_string()),
            body_schema: infer_contract_from_body(
                Some(r#"{"items":[{"id":1}],"nextCursor":"abc"}"#),
                Some("application/json"),
            )
            .contract,
            finished_at: OffsetDateTime::now_utc(),
        };
        let _ = store.record(request, response);

        let delta = store.diff(&marker.marker_id).expect("delta should exist");
        assert_eq!(delta.endpoints.len(), 1);
        assert_eq!(delta.exchanges.len(), 1);
    }

    #[test]
    fn list_data_sources_prefers_collection_reads() {
        let mut store = ApiReconStore::default();
        let marker = store.mark("main", None);
        for (url, status, body) in [
            (
                "https://example.com/api/items?page=1",
                200,
                r#"{"items":[{"id":1}],"nextCursor":"abc"}"#,
            ),
            ("https://example.com/api/login", 200, r#"{"ok":true}"#),
        ] {
            let request = ApiObservedRequestInput {
                method: if url.ends_with("login") {
                    "POST"
                } else {
                    "GET"
                }
                .to_string(),
                url: url.to_string(),
                tab_id: "main".to_string(),
                marker_id: Some(marker.marker_id.clone()),
                started_at: OffsetDateTime::now_utc(),
                query_keys: query_keys_from_url(url),
                header_keys: BTreeSet::from(["accept".to_string()]),
                header_values: BTreeMap::new(),
                content_type: Some("application/json".to_string()),
                operation_name: None,
                body_schema: None,
                body_value: None,
            };
            let response = ApiObservedResponseInput {
                status: Some(status),
                header_keys: BTreeSet::from(["content-type".to_string()]),
                content_type: Some("application/json".to_string()),
                body_schema: infer_contract_from_body(Some(body), Some("application/json"))
                    .contract,
                finished_at: OffsetDateTime::now_utc(),
            };
            let _ = store.record(request, response);
        }

        let endpoints = store.list_endpoints(None, 10, true).endpoints;
        assert_eq!(
            endpoints.first().map(|e| e.route_template.as_str()),
            Some("/api/items")
        );
    }

    #[test]
    fn json_pointer_get_extracts_values() {
        let value = json!({"data":{"items":[1,2,3]}});
        let extracted =
            json_pointer_get(&value, Some("/data/items")).expect("pointer should resolve");
        assert_eq!(extracted, json!([1, 2, 3]));
    }
}
