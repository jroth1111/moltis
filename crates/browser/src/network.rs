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
    chromiumoxide::{
        Page,
        cdp::browser_protocol::{
            fetch::{
                ContinueRequestParams, DisableParams, EnableParams, EventRequestPaused,
                FailRequestParams, FulfillRequestParams, HeaderEntry, RequestId, RequestPattern,
            },
            network::ErrorReason,
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
            started_at: now_ms,
            duration_ms: 0,
        }
    }
}

/// Returns the current time as milliseconds since the Unix epoch.
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

/// Accumulates [`HarEntry`] values and serialises them to HAR 1.2 JSON.
#[derive(Debug)]
pub struct HarRecorder {
    entries: Vec<HarEntry>,
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
            started_at: unix_now_ms(),
        }
    }

    /// Append an entry to the recording.
    pub fn record(&mut self, entry: HarEntry) {
        self.entries.push(entry);
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
                        "content": {
                            "mimeType": "text/plain",
                            "size": e.response_body.as_ref().map(|b| b.len() as i64).unwrap_or(0),
                            "text": e.response_body.as_deref().unwrap_or(""),
                        },
                        "redirectURL": "",
                        "headersSize": -1,
                        "bodySize": -1,
                    },
                    "cache": {},
                    "timings": { "send": 0, "wait": e.duration_ms, "receive": 0 },
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
        self.entries.len()
    }

    /// Whether the recorder has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Tracks the network interception state for a browser instance.
#[derive(Debug)]
pub struct InterceptionState {
    /// Whether `Fetch.enable` has been called and interception is active.
    pub enabled: bool,
    /// Active HAR recorder (Some while recording, None otherwise).
    pub recorder: Option<HarRecorder>,
    /// Extra headers injected into every intercepted request.
    pub extra_headers: HashMap<String, String>,
    /// Broadcast channel for forwarding paused-request events to callers.
    pub paused_tx: Option<tokio::sync::broadcast::Sender<EventRequestPaused>>,
    /// Background task that drains CDP `EventRequestPaused` events.
    pub _task: Option<tokio::task::JoinHandle<()>>,
}

impl Default for InterceptionState {
    fn default() -> Self {
        Self {
            enabled: false,
            recorder: None,
            extra_headers: HashMap::new(),
            paused_tx: None,
            _task: None,
        }
    }
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
    request_id: RequestId,
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
    request_id: RequestId,
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
pub async fn fail_request(page: &Page, request_id: RequestId) -> Result<(), Error> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
            started_at: 0,
            duration_ms: 10,
        });

        assert!(!recorder.is_empty());
        assert_eq!(recorder.len(), 1);
    }
}
