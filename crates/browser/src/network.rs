//! Network interception helpers.
//!
//! This module keeps active request interception separate from passive API
//! capture. Interception pauses requests so the caller can inspect or modify
//! them; API capture observes traffic without becoming part of the request path.

use std::{collections::HashMap, sync::Arc};

use chromiumoxide::{
    Page,
    cdp::browser_protocol::{
        fetch::{
            ContinueRequestParams, DisableParams, EnableParams, EventRequestPaused,
            FailRequestParams, FulfillRequestParams, HeaderEntry, RequestId as FetchRequestId,
            RequestPattern,
        },
        network::ErrorReason,
    },
};

use crate::error::Error;

#[derive(Debug)]
pub struct InterceptionSnapshot {
    pub enabled: bool,
    pub url_patterns: Vec<String>,
    pub extra_headers: HashMap<String, String>,
}

/// Tracks the interception state for a browser instance.
#[derive(Debug, Default)]
pub struct InterceptionState {
    /// Whether `Fetch.enable` has been called and interception is active.
    pub enabled: bool,
    /// Last configured request patterns for `Fetch.enable`.
    pub url_patterns: Vec<String>,
    /// Extra headers injected into every intercepted request.
    pub extra_headers: HashMap<String, String>,
    /// Broadcast channel for forwarding paused-request events to callers.
    pub paused_tx: Option<tokio::sync::broadcast::Sender<Arc<EventRequestPaused>>>,
    /// Background tasks that drain CDP interception events.
    pub tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Enable the CDP `Fetch` domain, intercepting requests matching `patterns`.
///
/// When `patterns` is empty every request is intercepted (wildcard).
pub async fn enable_interception(page: &Page, patterns: Vec<String>) -> Result<(), Error> {
    let cdp_patterns: Vec<RequestPattern> = if patterns.is_empty() {
        vec![]
    } else {
        patterns
            .into_iter()
            .map(|pattern| RequestPattern {
                url_pattern: Some(pattern),
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
        .map_err(|error| Error::Cdp(format!("Fetch.enable failed: {error}")))?;

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
        .map_err(|error| Error::Cdp(format!("Fetch.continueRequest failed: {error}")))?;

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

    let body_bin = body.map(|payload| BASE64.encode(payload.as_bytes()).into());

    let mut params = FulfillRequestParams::new(request_id, response_code as i64);
    params.body = body_bin;

    page.execute(params)
        .await
        .map_err(|error| Error::Cdp(format!("Fetch.fulfillRequest failed: {error}")))?;

    Ok(())
}

/// Fail a paused request with a generic network error.
pub async fn fail_request(page: &Page, request_id: FetchRequestId) -> Result<(), Error> {
    let params = FailRequestParams::new(request_id, ErrorReason::Failed);

    page.execute(params)
        .await
        .map_err(|error| Error::Cdp(format!("Fetch.failRequest failed: {error}")))?;

    Ok(())
}

/// Disable the CDP `Fetch` domain (stop interception).
pub async fn disable_interception(page: &Page) -> Result<(), Error> {
    page.execute(DisableParams::default())
        .await
        .map_err(|error| Error::Cdp(format!("Fetch.disable failed: {error}")))?;

    Ok(())
}
