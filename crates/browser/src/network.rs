//! Network interception and HAR 1.2 recording.
//!
//! Provides helpers for enabling/disabling the CDP `Fetch` domain (request
//! interception) and accumulating request/response data into a HAR 1.2 log.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use {
    base64::{Engine, engine::general_purpose::STANDARD as BASE64},
    chromiumoxide::{
        Page,
        cdp::browser_protocol::{
            fetch::{
                ContinueRequestParams, DisableParams, EnableParams, EventRequestPaused,
                FailRequestParams, FulfillRequestParams, HeaderEntry, RequestId as FetchRequestId,
                RequestPattern,
            },
            network::{
                self, ErrorReason, EventLoadingFailed, EventRequestWillBeSent,
                EventResponseReceived, GetResponseBodyParams,
            },
        },
    },
    serde_json::json,
};

use crate::error::Error;

/// A single entry in a HAR 1.2 log.
#[derive(Debug, Clone)]
pub struct HarEntry {
    /// HTTP method (GET, POST, …).
    pub method: String,
    /// Full request URL.
    pub url: String,
    /// HTTP response status code (0 when unknown / not yet received).
    pub status: u16,
    /// Request headers as (name, value) pairs.
    pub request_headers: Vec<(String, String)>,
    /// Response headers as (name, value) pairs.
    pub response_headers: Vec<(String, String)>,
    /// Optional request body (POST data).
    pub request_body: Option<String>,
    /// Optional decoded response body text.
    pub response_body: Option<String>,
    /// Encoding for `response_body` when the payload is preserved as base64.
    pub response_body_encoding: Option<String>,
    /// Response MIME type if known.
    pub mime_type: Option<String>,
    /// Chromium network resource type if known (`Document`, `XHR`, `Fetch`, ...).
    pub resource_type: Option<String>,
    /// Unix timestamp (milliseconds) when the request started.
    pub started_at: u64,
    /// Round-trip duration in milliseconds.
    pub duration_ms: u64,
}

impl HarEntry {
    /// Build a minimal `HarEntry` from an intercepted [`EventRequestPaused`].
    ///
    /// Only request-stage data is available here; response fields default to
    /// empty and must be filled in by the caller when the response arrives.
    pub fn from_event(event: &EventRequestPaused) -> Self {
        let req = &event.request;
        let now_ms = unix_now_ms();

        let request_headers = req
            .headers
            .inner()
            .as_object()
            .map(|map| {
                map.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let status = event.response_status_code.map(|c| c as u16).unwrap_or(0);

        let response_headers = event
            .response_headers
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|h| (h.name.clone(), h.value.clone()))
            .collect();

        // Extract POST data from post_data_entries if present.
        let request_body = req.post_data_entries.as_deref().and_then(|entries| {
            entries
                .first()
                .and_then(|e| e.bytes.as_ref().map(|b| (b.as_ref() as &str).to_string()))
        });

        Self {
            method: req.method.clone(),
            url: req.url.clone(),
            status,
            request_headers,
            response_headers,
            request_body,
            response_body: None,
            response_body_encoding: None,
            mime_type: None,
            resource_type: Some(event.resource_type.as_ref().to_string()),
            started_at: now_ms,
            duration_ms: 0,
        }
    }

    /// Build a pending entry from a `Network.requestWillBeSent` event.
    pub fn from_request_event(event: &EventRequestWillBeSent) -> Self {
        let request_headers = headers_to_pairs(&event.request.headers);
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
        let started_at = seconds_to_unix_ms(*event.wall_time.inner()).unwrap_or_else(unix_now_ms);

        Self {
            method: event.request.method.clone(),
            url: event.request.url.clone(),
            status: 0,
            request_headers,
            response_headers: Vec::new(),
            request_body,
            response_body: None,
            response_body_encoding: None,
            mime_type: None,
            resource_type: event.r#type.as_ref().map(|kind| kind.as_ref().to_string()),
            started_at,
            duration_ms: 0,
        }
    }

    fn apply_response(
        &mut self,
        response: &network::Response,
        resource_type: Option<&network::ResourceType>,
    ) {
        self.status = response.status.clamp(0, u16::MAX as i64) as u16;
        self.response_headers = headers_to_pairs(&response.headers);
        self.mime_type = Some(response.mime_type.clone());
        if self.resource_type.is_none() {
            self.resource_type = resource_type.map(|kind| kind.as_ref().to_string());
        }
    }

    fn apply_fetch_response_pause(&mut self, event: &EventRequestPaused) {
        if let Some(status) = event.response_status_code {
            self.status = status.clamp(0, u16::MAX as i64) as u16;
        }
        if let Some(headers) = event.response_headers.as_deref() {
            self.response_headers = header_entries_to_pairs(headers);
        }
        if self.resource_type.is_none() {
            self.resource_type = Some(event.resource_type.as_ref().to_string());
        }
    }

    fn apply_response_body(&mut self, body: RecordedResponseBody) {
        self.response_body = Some(body.text);
        self.response_body_encoding = body.encoding;
    }
}

#[derive(Debug, Clone)]
struct PendingHarEntry {
    entry: HarEntry,
    started_monotonic: Option<f64>,
    finished_monotonic: Option<f64>,
    failed: bool,
}

impl PendingHarEntry {
    fn new(entry: HarEntry, started_monotonic: Option<f64>) -> Self {
        Self {
            entry,
            started_monotonic,
            finished_monotonic: None,
            failed: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedResponseBody {
    pub text: String,
    pub encoding: Option<String>,
}

#[derive(Debug)]
pub struct InterceptionSnapshot {
    pub enabled: bool,
    pub url_patterns: Vec<String>,
    pub extra_headers: HashMap<String, String>,
    pub recorder: Option<HarRecorder>,
}

/// Returns the current time as milliseconds since the Unix epoch.
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

fn seconds_to_unix_ms(seconds: f64) -> Option<u64> {
    if !seconds.is_finite() || seconds.is_sign_negative() {
        return None;
    }
    Some((seconds * 1000.0).round() as u64)
}

fn monotonic_elapsed_ms(start_seconds: f64, end_seconds: f64) -> u64 {
    if !start_seconds.is_finite() || !end_seconds.is_finite() || end_seconds < start_seconds {
        return 0;
    }
    ((end_seconds - start_seconds) * 1000.0).round() as u64
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

fn header_entries_to_pairs(headers: &[HeaderEntry]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|header| (header.name.clone(), header.value.clone()))
        .collect()
}

fn should_capture_resource_body(
    resource_type: Option<&str>,
    mime_type: Option<&str>,
    url: &str,
) -> bool {
    if let Some(mime_type) = mime_type {
        let mime_type = mime_type.to_ascii_lowercase();
        if mime_type.starts_with("text/")
            || mime_type.contains("json")
            || mime_type.contains("xml")
            || mime_type.contains("html")
            || mime_type.contains("javascript")
            || mime_type.contains("graphql")
        {
            return true;
        }
    }

    matches!(
        resource_type,
        Some("Document" | "XHR" | "Fetch" | "Script" | "EventSource" | "Other") | None
    ) && !url.starts_with("data:")
}

fn decode_response_body(body: String, base64_encoded: bool) -> RecordedResponseBody {
    if !base64_encoded {
        return RecordedResponseBody {
            text: body,
            encoding: None,
        };
    }

    match BASE64.decode(body.as_bytes()) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => RecordedResponseBody {
                text,
                encoding: None,
            },
            Err(error) => RecordedResponseBody {
                text: BASE64.encode(error.into_bytes()),
                encoding: Some("base64".to_string()),
            },
        },
        Err(_) => RecordedResponseBody {
            text: body,
            encoding: Some("base64".to_string()),
        },
    }
}

/// Accumulates [`HarEntry`] values and serialises them to HAR 1.2 JSON.
#[derive(Debug)]
pub struct HarRecorder {
    entries: Vec<HarEntry>,
    pending_entries: HashMap<network::RequestId, PendingHarEntry>,
    /// Recording start time (Unix ms), used in the HAR `log.pages` entry.
    started_at: u64,
}

impl Default for HarRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl HarRecorder {
    /// Create a new recorder, capturing the current time as the start time.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            pending_entries: HashMap::new(),
            started_at: unix_now_ms(),
        }
    }

    /// Append an entry to the recording.
    pub fn record(&mut self, entry: HarEntry) {
        self.entries.push(entry);
    }

    /// Record a request-stage `Fetch.requestPaused` event.
    pub fn record_request(&mut self, event: &EventRequestPaused) {
        let Some(network_id) = event.network_id.clone() else {
            return;
        };
        self.pending_entries.insert(
            network_id,
            PendingHarEntry::new(HarEntry::from_event(event), None),
        );
    }

    /// Backfill start timing details from `Network.requestWillBeSent`.
    pub fn apply_request_will_be_sent(&mut self, event: &EventRequestWillBeSent) {
        if let Some(pending) = self.pending_entries.get_mut(&event.request_id) {
            pending.started_monotonic = Some(*event.timestamp.inner());
            pending.entry.started_at =
                seconds_to_unix_ms(*event.wall_time.inner()).unwrap_or(pending.entry.started_at);
            if pending.entry.resource_type.is_none() {
                pending.entry.resource_type =
                    event.r#type.as_ref().map(|kind| kind.as_ref().to_string());
            }
        }
    }

    /// Merge response metadata from `Fetch.requestPaused` response-stage events.
    pub fn apply_fetch_response(&mut self, event: &EventRequestPaused) {
        let Some(network_id) = event.network_id.clone() else {
            return;
        };
        if let Some(pending) = self.pending_entries.get_mut(&network_id) {
            pending.entry.apply_fetch_response_pause(event);
            self.finalize_if_ready(&network_id);
        }
    }

    /// Merge response metadata from `Network.responseReceived`.
    pub fn apply_response_received(&mut self, event: &EventResponseReceived) {
        if let Some(pending) = self.pending_entries.get_mut(&event.request_id) {
            pending
                .entry
                .apply_response(&event.response, Some(&event.r#type));
            self.finalize_if_ready(&event.request_id);
        }
    }

    /// Finalize a request when the network stack reports completion.
    pub fn apply_loading_finished(
        &mut self,
        request_id: &network::RequestId,
        finished_timestamp: f64,
        body: Option<RecordedResponseBody>,
    ) {
        if let Some(pending) = self.pending_entries.get_mut(request_id) {
            pending.finished_monotonic = Some(finished_timestamp);
            if let Some(body) = body {
                pending.entry.apply_response_body(body);
            }
            self.finalize_if_ready(request_id);
        }
    }

    /// Finalize a failed request without a response body.
    pub fn apply_loading_failed(&mut self, event: &EventLoadingFailed) {
        if let Some(pending) = self.pending_entries.get_mut(&event.request_id) {
            pending.failed = true;
            pending.finished_monotonic = Some(*event.timestamp.inner());
            self.finalize_if_ready(&event.request_id);
        }
    }

    /// Whether a completed request should fetch the body from `Network.getResponseBody`.
    pub fn should_capture_body(&self, request_id: &network::RequestId) -> Option<bool> {
        self.pending_entries.get(request_id).map(|pending| {
            should_capture_resource_body(
                pending.entry.resource_type.as_deref(),
                pending.entry.mime_type.as_deref(),
                pending.entry.url.as_str(),
            )
        })
    }

    /// Flush partial entries so `stop_har` still returns useful data even if some
    /// requests are mid-flight when recording stops.
    pub fn finish(&mut self) {
        for (_, mut pending) in self.pending_entries.drain() {
            if let (Some(started_monotonic), Some(finished_monotonic)) =
                (pending.started_monotonic, pending.finished_monotonic)
            {
                pending.entry.duration_ms =
                    monotonic_elapsed_ms(started_monotonic, finished_monotonic);
            }
            self.entries.push(pending.entry);
        }
    }

    fn finalize_if_ready(&mut self, request_id: &network::RequestId) {
        let ready = self.pending_entries.get(request_id).is_some_and(|pending| {
            pending.failed || (pending.finished_monotonic.is_some() && pending.entry.status != 0)
        });
        if !ready {
            return;
        }

        if let Some(mut pending) = self.pending_entries.remove(request_id) {
            if let (Some(started_monotonic), Some(finished_monotonic)) =
                (pending.started_monotonic, pending.finished_monotonic)
            {
                pending.entry.duration_ms =
                    monotonic_elapsed_ms(started_monotonic, finished_monotonic);
            }
            self.entries.push(pending.entry);
        }
    }

    /// Serialise all recorded entries to a HAR 1.2 JSON document.
    pub fn to_har_json(&self) -> serde_json::Value {
        // Local helper: convert Unix-ms timestamp to ISO-8601 for HAR 1.2.
        fn ms_to_rfc3339(ms: u64) -> String {
            use time::OffsetDateTime;
            let ns = ms as i128 * 1_000_000;
            OffsetDateTime::from_unix_timestamp_nanos(ns)
                .map(|dt| {
                    format!(
                        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                        dt.year(),
                        dt.month() as u8,
                        dt.day(),
                        dt.hour(),
                        dt.minute(),
                        dt.second(),
                        dt.millisecond(),
                    )
                })
                .unwrap_or_else(|_| format!("{ms}"))
        }
        let entries: Vec<serde_json::Value> = self
            .entries
            .iter()
            .map(|e| {
                let mut content = json!({
                    "mimeType": e.mime_type.as_deref().unwrap_or("text/plain"),
                    "size": e.response_body.as_ref().map(|b| b.len() as i64).unwrap_or(0),
                    "text": e.response_body.as_deref().unwrap_or(""),
                });
                if let Some(ref encoding) = e.response_body_encoding {
                    content["encoding"] = json!(encoding);
                }

                json!({
                    "startedDateTime": ms_to_rfc3339(e.started_at),
                    "time": e.duration_ms,
                    "request": {
                        "method": e.method,
                        "url": e.url,
                        "httpVersion": "HTTP/1.1",
                        "headers": e.request_headers
                            .iter()
                            .map(|(k, v)| json!({"name": k, "value": v}))
                            .collect::<Vec<_>>(),
                        "queryString": [],
                        "cookies": [],
                        "headersSize": -1,
                        "bodySize": e.request_body.as_ref().map(|b| b.len() as i64).unwrap_or(-1),
                        "postData": e.request_body.as_ref().map(|b| json!({"mimeType": "", "text": b})),
                    },
                    "response": {
                        "status": e.status,
                        "statusText": "",
                        "httpVersion": "HTTP/1.1",
                        "headers": e.response_headers
                            .iter()
                            .map(|(k, v)| json!({"name": k, "value": v}))
                            .collect::<Vec<_>>(),
                        "cookies": [],
                        "content": content,
                        "redirectURL": "",
                        "headersSize": -1,
                        "bodySize": -1,
                    },
                    "cache": {},
                    "timings": { "send": 0, "wait": e.duration_ms, "receive": 0 },
                    "_resourceType": e.resource_type,
                })
            })
            .collect();

        json!({
            "log": {
                "version": "1.2",
                "creator": {
                    "name": "moltis-browser",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "pages": [{
                    "startedDateTime": ms_to_rfc3339(self.started_at),
                    "id": "page_1",
                    "title": "",
                    "pageTimings": {},
                }],
                "entries": entries,
            }
        })
    }

    /// Number of recorded entries.
    pub fn len(&self) -> usize {
        self.entries.len() + self.pending_entries.len()
    }

    /// Whether the recorder has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.pending_entries.is_empty()
    }
}

/// Tracks the network interception state for a browser instance.
#[derive(Debug, Default)]
pub struct InterceptionState {
    /// Whether `Fetch.enable` has been called and interception is active.
    pub enabled: bool,
    /// Last configured request patterns for `Fetch.enable`.
    pub url_patterns: Vec<String>,
    /// Active HAR recorder (Some while recording, None otherwise).
    pub recorder: Option<HarRecorder>,
    /// Extra headers injected into every intercepted request.
    pub extra_headers: HashMap<String, String>,
    /// Broadcast channel for forwarding paused-request events to callers.
    pub paused_tx: Option<tokio::sync::broadcast::Sender<Arc<EventRequestPaused>>>,
    /// Background tasks that drain CDP network/interception events.
    pub tasks: Vec<tokio::task::JoinHandle<()>>,
}

// ── CDP helpers ──────────────────────────────────────────────────────────────

/// Enable the CDP `Fetch` domain, intercepting requests matching `patterns`.
///
/// When `patterns` is empty every request is intercepted (wildcard).
pub async fn enable_interception(page: &Page, patterns: Vec<String>) -> Result<(), Error> {
    let cdp_patterns: Vec<RequestPattern> = if patterns.is_empty() {
        // Omitting patterns means intercept everything.
        vec![]
    } else {
        patterns
            .into_iter()
            .map(|p| RequestPattern {
                url_pattern: Some(p),
                ..Default::default()
            })
            .collect()
    };

    let enable = EnableParams {
        patterns: if cdp_patterns.is_empty() {
            None
        } else {
            Some(cdp_patterns)
        },
        handle_auth_requests: Some(false),
    };

    page.execute(enable)
        .await
        .map_err(|e| Error::Cdp(format!("Fetch.enable failed: {e}")))?;

    #[cfg(feature = "metrics")]
    moltis_metrics::counter!(moltis_metrics::browser::INTERCEPTIONS_TOTAL).increment(1);

    Ok(())
}

/// Continue a paused request, optionally injecting extra headers.
pub async fn continue_request(
    page: &Page,
    request_id: FetchRequestId,
    extra_headers: Option<Vec<(String, String)>>,
) -> Result<(), Error> {
    let headers = extra_headers.map(|pairs| {
        pairs
            .into_iter()
            .map(|(name, value)| HeaderEntry { name, value })
            .collect::<Vec<_>>()
    });

    let mut params = ContinueRequestParams::new(request_id);
    params.headers = headers;

    page.execute(params)
        .await
        .map_err(|e| Error::Cdp(format!("Fetch.continueRequest failed: {e}")))?;

    Ok(())
}

/// Fulfill a paused request with a synthetic response body.
pub async fn fulfill_request(
    page: &Page,
    request_id: FetchRequestId,
    response_code: u16,
    body: Option<String>,
) -> Result<(), Error> {
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

    // CDP expects the body as a base64-encoded string in the Binary wrapper.
    let body_bin = body.map(|b| BASE64.encode(b.as_bytes()).into());

    let mut params = FulfillRequestParams::new(request_id, response_code as i64);
    params.body = body_bin;

    page.execute(params)
        .await
        .map_err(|e| Error::Cdp(format!("Fetch.fulfillRequest failed: {e}")))?;

    Ok(())
}

/// Fail a paused request with a generic network error.
pub async fn fail_request(page: &Page, request_id: FetchRequestId) -> Result<(), Error> {
    let params = FailRequestParams::new(request_id, ErrorReason::Failed);

    page.execute(params)
        .await
        .map_err(|e| Error::Cdp(format!("Fetch.failRequest failed: {e}")))?;

    Ok(())
}

/// Disable the CDP `Fetch` domain (stop interception).
pub async fn disable_interception(page: &Page) -> Result<(), Error> {
    page.execute(DisableParams::default())
        .await
        .map_err(|e| Error::Cdp(format!("Fetch.disable failed: {e}")))?;

    Ok(())
}

/// Read a completed response body via `Network.getResponseBody`.
pub async fn get_response_body(
    page: &Page,
    request_id: network::RequestId,
) -> Result<RecordedResponseBody, Error> {
    let response = page
        .execute(GetResponseBodyParams::new(request_id))
        .await
        .map_err(|e| Error::Cdp(format!("Network.getResponseBody failed: {e}")))?;

    Ok(decode_response_body(
        response.body.clone(),
        response.base64_encoded,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fetch_request_paused_event() -> EventRequestPaused {
        match serde_json::from_value(json!({
            "requestId": "fetch-1",
            "request": {
                "url": "https://api.example.com/products",
                "method": "GET",
                "headers": { "Accept": "application/json" },
                "initialPriority": "High",
                "referrerPolicy": "strict-origin-when-cross-origin"
            },
            "frameId": "frame-1",
            "resourceType": "Fetch",
            "networkId": "network-1"
        })) {
            Ok(event) => event,
            Err(error) => panic!("requestPaused JSON should deserialize: {error}"),
        }
    }

    fn request_will_be_sent_event() -> EventRequestWillBeSent {
        match serde_json::from_value(json!({
            "requestId": "network-1",
            "loaderId": "loader-1",
            "documentURL": "https://app.example.com/",
            "request": {
                "url": "https://api.example.com/products",
                "method": "GET",
                "headers": { "Accept": "application/json" },
                "initialPriority": "High",
                "referrerPolicy": "strict-origin-when-cross-origin"
            },
            "timestamp": 1.0,
            "wallTime": 1700000000.0,
            "initiator": { "type": "other" },
            "redirectHasExtraInfo": false,
            "type": "Fetch"
        })) {
            Ok(event) => event,
            Err(error) => panic!("requestWillBeSent JSON should deserialize: {error}"),
        }
    }

    fn response_received_event() -> EventResponseReceived {
        match serde_json::from_value(json!({
            "requestId": "network-1",
            "loaderId": "loader-1",
            "timestamp": 2.0,
            "type": "Fetch",
            "response": {
                "url": "https://api.example.com/products",
                "status": 200,
                "statusText": "OK",
                "headers": { "Content-Type": "application/json" },
                "mimeType": "application/json",
                "charset": "utf-8",
                "connectionReused": false,
                "connectionId": 1.0,
                "encodedDataLength": 32.0,
                "securityState": "secure"
            },
            "hasExtraInfo": false
        })) {
            Ok(event) => event,
            Err(error) => panic!("responseReceived JSON should deserialize: {error}"),
        }
    }

    #[test]
    fn test_har_recorder_empty_json() {
        let recorder = HarRecorder::new();
        let har = recorder.to_har_json();

        let log = &har["log"];
        assert_eq!(log["version"].as_str(), Some("1.2"));
        assert_eq!(log["creator"]["name"].as_str(), Some("moltis-browser"));
        assert_eq!(
            log["entries"].as_array().map(|a| a.is_empty()),
            Some(true),
            "empty recorder should produce zero entries"
        );
    }

    #[test]
    fn test_har_recorder_single_entry() {
        let mut recorder = HarRecorder::new();
        recorder.record(HarEntry {
            method: "GET".to_string(),
            url: "https://example.com/".to_string(),
            status: 200,
            request_headers: vec![("Accept".to_string(), "*/*".to_string())],
            response_headers: vec![("Content-Type".to_string(), "text/html".to_string())],
            request_body: None,
            response_body: Some("<html></html>".to_string()),
            response_body_encoding: None,
            mime_type: Some("text/html".to_string()),
            resource_type: Some("Document".to_string()),
            started_at: 1_700_000_000_000,
            duration_ms: 42,
        });

        let har = recorder.to_har_json();
        let Some(entries) = har["log"]["entries"].as_array() else {
            panic!("entries must be a JSON array");
        };

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["request"]["method"].as_str(), Some("GET"));
        assert_eq!(
            entries[0]["request"]["url"].as_str(),
            Some("https://example.com/")
        );
        assert_eq!(entries[0]["response"]["status"].as_u64(), Some(200));
        assert_eq!(entries[0]["time"].as_u64(), Some(42));
        assert!(
            entries[0]["startedDateTime"]
                .as_str()
                .is_some_and(|s| s.contains('T')),
            "startedDateTime must be ISO-8601"
        );
        assert_eq!(
            entries[0]["response"]["content"]["mimeType"].as_str(),
            Some("text/html")
        );
    }

    #[test]
    fn test_har_recorder_len_and_is_empty() {
        let mut recorder = HarRecorder::new();
        assert!(recorder.is_empty());
        assert_eq!(recorder.len(), 0);

        recorder.record(HarEntry {
            method: "POST".to_string(),
            url: "https://api.example.com/data".to_string(),
            status: 201,
            request_headers: vec![],
            response_headers: vec![],
            request_body: Some(r#"{"key":"value"}"#.to_string()),
            response_body: None,
            response_body_encoding: None,
            mime_type: Some("application/json".to_string()),
            resource_type: Some("Fetch".to_string()),
            started_at: 0,
            duration_ms: 10,
        });

        assert!(!recorder.is_empty());
        assert_eq!(recorder.len(), 1);
    }

    #[test]
    fn test_decode_response_body_handles_base64_text_and_binary() {
        let text_body = decode_response_body(BASE64.encode(br#"{"ok":true}"#), true);
        assert_eq!(text_body.text, r#"{"ok":true}"#);
        assert_eq!(text_body.encoding, None);

        let binary_body = decode_response_body(BASE64.encode([0xff, 0xd8, 0xff]), true);
        assert_eq!(binary_body.encoding.as_deref(), Some("base64"));
        assert_eq!(binary_body.text, BASE64.encode([0xff, 0xd8, 0xff]));
    }

    #[test]
    fn test_har_recorder_merges_request_response_and_body() {
        let mut recorder = HarRecorder::new();
        let paused = fetch_request_paused_event();
        recorder.record_request(&paused);
        recorder.apply_request_will_be_sent(&request_will_be_sent_event());
        recorder.apply_response_received(&response_received_event());
        recorder.apply_loading_finished(
            &network::RequestId::new("network-1"),
            3.0,
            Some(RecordedResponseBody {
                text: r#"{"products":[1,2]}"#.to_string(),
                encoding: None,
            }),
        );

        assert_eq!(recorder.entries.len(), 1);
        assert!(recorder.pending_entries.is_empty());
        assert_eq!(recorder.entries[0].duration_ms, 2000);
        assert_eq!(
            recorder.entries[0].mime_type.as_deref(),
            Some("application/json")
        );
        assert_eq!(
            recorder.entries[0].response_body.as_deref(),
            Some(r#"{"products":[1,2]}"#)
        );

        let har = recorder.to_har_json();
        let entry = &har["log"]["entries"][0];
        assert_eq!(
            entry["response"]["content"]["mimeType"].as_str(),
            Some("application/json")
        );
        assert_eq!(
            entry["response"]["content"]["text"].as_str(),
            Some(r#"{"products":[1,2]}"#)
        );
        assert_eq!(entry["_resourceType"].as_str(), Some("Fetch"));
    }

    #[test]
    fn test_har_recorder_handles_loading_finished_before_response_received() {
        let mut recorder = HarRecorder::new();
        recorder.record_request(&fetch_request_paused_event());
        recorder.apply_request_will_be_sent(&request_will_be_sent_event());
        recorder.apply_loading_finished(
            &network::RequestId::new("network-1"),
            3.0,
            Some(RecordedResponseBody {
                text: r#"{"products":[]}"#.to_string(),
                encoding: None,
            }),
        );

        assert!(
            recorder.entries.is_empty(),
            "should wait for response metadata"
        );
        assert_eq!(recorder.pending_entries.len(), 1);

        recorder.apply_response_received(&response_received_event());

        assert_eq!(recorder.entries.len(), 1);
        assert!(recorder.pending_entries.is_empty());
        assert_eq!(
            recorder.entries[0].response_body.as_deref(),
            Some(r#"{"products":[]}"#)
        );
    }
}
