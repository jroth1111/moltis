//! Telemetry helpers for permissioned browser measurement.
//!
//! These types support safe, owned-site measurement of browser-visible
//! identity and interaction distributions so regressions can be detected
//! before anti-bot changes reach production targets.

use crate::types::BrowserBackendKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FingerprintScreen {
    pub width: u32,
    pub height: u32,
    pub avail_width: u32,
    pub avail_height: u32,
    pub dpr: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FingerprintSnapshot {
    pub session_id: String,
    pub ts: f64,
    pub url: String,
    pub user_agent: String,
    #[serde(default)]
    pub webdriver: Option<bool>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub languages: Option<Vec<String>>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub screen: Option<FingerprintScreen>,
    #[serde(default)]
    pub hardware_concurrency: Option<u32>,
    #[serde(default)]
    pub device_memory: Option<f64>,
    #[serde(default)]
    pub webgl_vendor: Option<String>,
    #[serde(default)]
    pub webgl_renderer: Option<String>,
    #[serde(default)]
    pub plugins_count: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FingerprintHeaders {
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub accept_language: Option<String>,
    #[serde(default)]
    pub sec_ch_ua: Option<String>,
    #[serde(default)]
    pub sec_ch_ua_platform: Option<String>,
    #[serde(default)]
    pub sec_fetch_site: Option<String>,
    #[serde(default)]
    pub x_forwarded_for: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BehaviorPoint {
    pub t: f64,
    #[serde(rename = "type")]
    pub kind: String,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub buttons: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BehaviorBatchSummary {
    pub count: usize,
    #[serde(default)]
    pub duration_s: Option<f64>,
    pub path_len_px: f64,
    pub straight_line_px: f64,
    #[serde(default)]
    pub straightness: Option<f64>,
    #[serde(default)]
    pub mean_dt_s: Option<f64>,
    #[serde(default)]
    pub max_idle_gap_s: Option<f64>,
    #[serde(default)]
    pub mean_step_px: Option<f64>,
    #[serde(default)]
    pub mean_speed_px_s: Option<f64>,
    #[serde(default)]
    pub event_rate_hz: Option<f64>,
}

impl BehaviorBatchSummary {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            count: 0,
            duration_s: None,
            path_len_px: 0.0,
            straight_line_px: 0.0,
            straightness: None,
            mean_dt_s: None,
            max_idle_gap_s: None,
            mean_step_px: None,
            mean_speed_px_s: None,
            event_rate_hz: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestSequenceEvent {
    pub run_id: String,
    pub request_index: usize,
    pub request_ts_ms: f64,
    pub path: String,
    pub method: String,
    pub status_code: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestSequenceSummary {
    pub request_count: usize,
    #[serde(default)]
    pub first_path: Option<String>,
    #[serde(default)]
    pub last_path: Option<String>,
    pub distinct_path_count: usize,
    #[serde(default)]
    pub path_sequence: Vec<String>,
    #[serde(default)]
    pub mean_gap_ms: Option<f64>,
    #[serde(default)]
    pub max_gap_ms: Option<f64>,
}

impl RequestSequenceSummary {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            request_count: 0,
            first_path: None,
            last_path: None,
            distinct_path_count: 0,
            path_sequence: Vec::new(),
            mean_gap_ms: None,
            max_gap_ms: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeBrowserFamily {
    Chrome,
    Chromium,
    Edge,
    Brave,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeProxyMode {
    None,
    Residential,
    Datacenter,
    Socks5,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeRunProfile {
    pub browser_family: ProbeBrowserFamily,
    pub browser_version: String,
    pub backend: BrowserBackendKind,
    pub headless: bool,
    pub proxy_mode: ProbeProxyMode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeRunEvidence {
    pub profile: ProbeRunProfile,
    pub fingerprint: FingerprintSnapshot,
    pub headers: FingerprintHeaders,
    pub request_sequence: RequestSequenceSummary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeDriftThresholds {
    pub mean_gap_ratio: f64,
    pub max_gap_ratio: f64,
}

impl Default for ProbeDriftThresholds {
    fn default() -> Self {
        Self {
            mean_gap_ratio: 0.35,
            max_gap_ratio: 0.50,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeDriftKind {
    BrowserFamilyChanged,
    BrowserVersionChanged,
    BackendChanged,
    HeadlessChanged,
    ProxyModeChanged,
    UserAgentChanged,
    AcceptLanguageChanged,
    WebdriverChanged,
    PlatformChanged,
    TimezoneChanged,
    ScreenChanged,
    HardwareConcurrencyChanged,
    DeviceMemoryChanged,
    RequestCountChanged,
    PathSequenceChanged,
    MeanGapDrift,
    MaxGapDrift,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeDriftIssue {
    pub kind: ProbeDriftKind,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeRunDrift {
    pub issues: Vec<ProbeDriftIssue>,
}

impl ProbeRunDrift {
    #[must_use]
    pub fn consistent(&self) -> bool {
        self.issues.is_empty()
    }
}

fn relative_delta(baseline: f64, current: f64) -> f64 {
    let denominator = baseline.abs().max(1.0);
    (current - baseline).abs() / denominator
}

fn push_optional_drift_issue(
    issues: &mut Vec<ProbeDriftIssue>,
    kind: ProbeDriftKind,
    detail: impl Into<String>,
    baseline: Option<f64>,
    current: Option<f64>,
    allowed_ratio: f64,
) {
    let (Some(baseline), Some(current)) = (baseline, current) else {
        return;
    };

    if relative_delta(baseline, current) > allowed_ratio {
        issues.push(ProbeDriftIssue {
            kind,
            detail: detail.into(),
        });
    }
}

#[must_use]
pub fn compare_probe_run_with_thresholds(
    baseline: &ProbeRunEvidence,
    current: &ProbeRunEvidence,
    thresholds: &ProbeDriftThresholds,
) -> ProbeRunDrift {
    let mut issues = Vec::new();

    if baseline.profile.browser_family != current.profile.browser_family {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::BrowserFamilyChanged,
            detail: format!(
                "browser family changed from {:?} to {:?}",
                baseline.profile.browser_family, current.profile.browser_family
            ),
        });
    }
    if baseline.profile.browser_version != current.profile.browser_version {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::BrowserVersionChanged,
            detail: format!(
                "browser version changed from {} to {}",
                baseline.profile.browser_version, current.profile.browser_version
            ),
        });
    }
    if baseline.profile.backend != current.profile.backend {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::BackendChanged,
            detail: format!(
                "backend changed from {} to {}",
                baseline.profile.backend, current.profile.backend
            ),
        });
    }
    if baseline.profile.headless != current.profile.headless {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::HeadlessChanged,
            detail: format!(
                "headless changed from {} to {}",
                baseline.profile.headless, current.profile.headless
            ),
        });
    }
    if baseline.profile.proxy_mode != current.profile.proxy_mode {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::ProxyModeChanged,
            detail: format!(
                "proxy mode changed from {:?} to {:?}",
                baseline.profile.proxy_mode, current.profile.proxy_mode
            ),
        });
    }

    if baseline.fingerprint.user_agent != current.fingerprint.user_agent {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::UserAgentChanged,
            detail: format!(
                "user agent changed from {} to {}",
                baseline.fingerprint.user_agent, current.fingerprint.user_agent
            ),
        });
    }
    if baseline.headers.accept_language != current.headers.accept_language {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::AcceptLanguageChanged,
            detail: format!(
                "accept-language changed from {:?} to {:?}",
                baseline.headers.accept_language, current.headers.accept_language
            ),
        });
    }
    if baseline.fingerprint.webdriver != current.fingerprint.webdriver {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::WebdriverChanged,
            detail: format!(
                "webdriver changed from {:?} to {:?}",
                baseline.fingerprint.webdriver, current.fingerprint.webdriver
            ),
        });
    }
    if baseline.fingerprint.platform != current.fingerprint.platform {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::PlatformChanged,
            detail: format!(
                "platform changed from {:?} to {:?}",
                baseline.fingerprint.platform, current.fingerprint.platform
            ),
        });
    }
    if baseline.fingerprint.timezone != current.fingerprint.timezone {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::TimezoneChanged,
            detail: format!(
                "timezone changed from {:?} to {:?}",
                baseline.fingerprint.timezone, current.fingerprint.timezone
            ),
        });
    }
    if baseline.fingerprint.screen != current.fingerprint.screen {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::ScreenChanged,
            detail: format!(
                "screen changed from {:?} to {:?}",
                baseline.fingerprint.screen, current.fingerprint.screen
            ),
        });
    }
    if baseline.fingerprint.hardware_concurrency != current.fingerprint.hardware_concurrency {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::HardwareConcurrencyChanged,
            detail: format!(
                "hardware concurrency changed from {:?} to {:?}",
                baseline.fingerprint.hardware_concurrency,
                current.fingerprint.hardware_concurrency
            ),
        });
    }
    if baseline.fingerprint.device_memory != current.fingerprint.device_memory {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::DeviceMemoryChanged,
            detail: format!(
                "device memory changed from {:?} to {:?}",
                baseline.fingerprint.device_memory, current.fingerprint.device_memory
            ),
        });
    }

    if baseline.request_sequence.request_count != current.request_sequence.request_count {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::RequestCountChanged,
            detail: format!(
                "request count changed from {} to {}",
                baseline.request_sequence.request_count, current.request_sequence.request_count
            ),
        });
    }
    if baseline.request_sequence.path_sequence != current.request_sequence.path_sequence {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::PathSequenceChanged,
            detail: format!(
                "path sequence changed from {:?} to {:?}",
                baseline.request_sequence.path_sequence, current.request_sequence.path_sequence
            ),
        });
    }

    push_optional_drift_issue(
        &mut issues,
        ProbeDriftKind::MeanGapDrift,
        format!(
            "mean gap drifted from {:?} to {:?}",
            baseline.request_sequence.mean_gap_ms, current.request_sequence.mean_gap_ms
        ),
        baseline.request_sequence.mean_gap_ms,
        current.request_sequence.mean_gap_ms,
        thresholds.mean_gap_ratio,
    );
    push_optional_drift_issue(
        &mut issues,
        ProbeDriftKind::MaxGapDrift,
        format!(
            "max gap drifted from {:?} to {:?}",
            baseline.request_sequence.max_gap_ms, current.request_sequence.max_gap_ms
        ),
        baseline.request_sequence.max_gap_ms,
        current.request_sequence.max_gap_ms,
        thresholds.max_gap_ratio,
    );

    ProbeRunDrift { issues }
}

#[must_use]
pub fn compare_probe_run(
    baseline: &ProbeRunEvidence,
    current: &ProbeRunEvidence,
) -> ProbeRunDrift {
    compare_probe_run_with_thresholds(baseline, current, &ProbeDriftThresholds::default())
}

#[must_use]
pub fn summarize_behavior_points(points: &[BehaviorPoint]) -> BehaviorBatchSummary {
    if points.is_empty() {
        return BehaviorBatchSummary::empty();
    }

    if points.len() == 1 {
        return BehaviorBatchSummary {
            count: 1,
            duration_s: Some(0.0),
            path_len_px: 0.0,
            straight_line_px: 0.0,
            straightness: None,
            mean_dt_s: None,
            max_idle_gap_s: None,
            mean_step_px: None,
            mean_speed_px_s: None,
            event_rate_hz: None,
        };
    }

    let mut total_dt_s = 0.0;
    let mut total_step_px = 0.0;
    let mut total_speed_px_s = 0.0;
    let mut max_idle_gap_s = 0.0_f64;
    let mut segment_count = 0usize;

    for window in points.windows(2) {
        let a = &window[0];
        let b = &window[1];
        let dt_s = ((b.t - a.t) / 1000.0).max(0.0);
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let step_px = dx.hypot(dy);
        let speed_px_s = if dt_s > 0.0 { step_px / dt_s } else { 0.0 };

        total_dt_s += dt_s;
        total_step_px += step_px;
        total_speed_px_s += speed_px_s;
        max_idle_gap_s = max_idle_gap_s.max(dt_s);
        segment_count += 1;
    }

    let duration_s = ((points.last().map(|point| point.t).unwrap_or(0.0)
        - points.first().map(|point| point.t).unwrap_or(0.0))
        / 1000.0)
        .max(0.0);
    let straight_line_px = (points.last().map(|point| point.x).unwrap_or(0.0)
        - points.first().map(|point| point.x).unwrap_or(0.0))
    .hypot(
        points.last().map(|point| point.y).unwrap_or(0.0)
            - points.first().map(|point| point.y).unwrap_or(0.0),
    );

    BehaviorBatchSummary {
        count: points.len(),
        duration_s: Some(duration_s),
        path_len_px: total_step_px,
        straight_line_px,
        straightness: (total_step_px > 0.0).then_some(straight_line_px / total_step_px),
        mean_dt_s: Some(total_dt_s / segment_count as f64),
        max_idle_gap_s: Some(max_idle_gap_s),
        mean_step_px: Some(total_step_px / segment_count as f64),
        mean_speed_px_s: Some(total_speed_px_s / segment_count as f64),
        event_rate_hz: (duration_s > 0.0).then_some(points.len() as f64 / duration_s),
    }
}

#[must_use]
pub fn summarize_request_sequence(events: &[RequestSequenceEvent]) -> RequestSequenceSummary {
    if events.is_empty() {
        return RequestSequenceSummary::empty();
    }

    let mut sorted = events.to_vec();
    sorted.sort_by(|left, right| left.request_index.cmp(&right.request_index));

    let path_sequence: Vec<String> = sorted.iter().map(|event| event.path.clone()).collect();
    let mut distinct_paths = std::collections::BTreeSet::new();
    for path in &path_sequence {
        distinct_paths.insert(path.clone());
    }

    let gaps_ms: Vec<f64> = sorted
        .windows(2)
        .map(|window| (window[1].request_ts_ms - window[0].request_ts_ms).max(0.0))
        .collect();
    let mean_gap_ms = (!gaps_ms.is_empty())
        .then_some(gaps_ms.iter().sum::<f64>() / gaps_ms.len() as f64);
    let max_gap_ms = gaps_ms.iter().copied().reduce(f64::max);

    RequestSequenceSummary {
        request_count: sorted.len(),
        first_path: sorted.first().map(|event| event.path.clone()),
        last_path: sorted.last().map(|event| event.path.clone()),
        distinct_path_count: distinct_paths.len(),
        path_sequence,
        mean_gap_ms,
        max_gap_ms,
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            BrowserManager, patchright_session::PatchrightSession, protection,
            snapshot::sanitize_dom_text,
            types::{BrowserAction, BrowserConfig, BrowserPreference, BrowserRequest},
        },
        axum::{
            Json, Router,
            extract::State,
            http::{HeaderMap, Uri},
            response::Html,
            routing::{get, post},
        },
        serde_json::json,
        std::{
            collections::HashMap,
            sync::{Arc, Mutex as StdMutex, OnceLock},
            time::Instant,
        },
        tokio::{
            net::TcpListener,
            sync::{Mutex, OwnedMutexGuard},
            task::JoinHandle,
            time::{Duration, sleep},
        },
    };

    #[derive(Debug, Clone)]
    struct CapturedFingerprint {
        body: FingerprintSnapshot,
        headers: FingerprintHeaders,
    }

    #[derive(Debug, Clone)]
    struct CapturedBehavior {
        summary: BehaviorBatchSummary,
        sample: Vec<BehaviorPoint>,
    }

    #[derive(Clone)]
    struct ProbeState {
        started_at: Instant,
        fingerprints: Arc<StdMutex<HashMap<String, CapturedFingerprint>>>,
        behaviors: Arc<StdMutex<HashMap<String, Vec<CapturedBehavior>>>>,
        requests: Arc<StdMutex<HashMap<String, Vec<RequestSequenceEvent>>>>,
    }

    impl ProbeState {
        fn new() -> Self {
            Self {
                started_at: Instant::now(),
                fingerprints: Arc::new(StdMutex::new(HashMap::new())),
                behaviors: Arc::new(StdMutex::new(HashMap::new())),
                requests: Arc::new(StdMutex::new(HashMap::new())),
            }
        }

        fn fingerprint(&self, session_id: &str) -> Option<CapturedFingerprint> {
            self.fingerprints.lock().unwrap().get(session_id).cloned()
        }

        fn behaviors(&self, session_id: &str) -> Vec<CapturedBehavior> {
            self.behaviors
                .lock()
                .unwrap()
                .get(session_id)
                .cloned()
                .unwrap_or_default()
        }

        fn first_fingerprint_session(&self) -> Option<String> {
            self.fingerprints
                .lock()
                .unwrap()
                .keys()
                .next()
                .cloned()
        }

        fn first_behavior_session(&self) -> Option<String> {
            self.behaviors.lock().unwrap().keys().next().cloned()
        }

        fn requests(&self, run_id: &str) -> Vec<RequestSequenceEvent> {
            self.requests
                .lock()
                .unwrap()
                .get(run_id)
                .cloned()
                .unwrap_or_default()
        }

        fn request_summary(&self, run_id: &str) -> RequestSequenceSummary {
            summarize_request_sequence(&self.requests(run_id))
        }
    }

    #[derive(Debug, Deserialize)]
    struct BehaviorBatch {
        session_id: String,
        #[allow(dead_code)]
        ts: f64,
        points: Vec<BehaviorPoint>,
    }

    fn query_value(uri: &Uri, name: &str) -> Option<String> {
        uri.query()?.split('&').find_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?;
            let value = parts.next().unwrap_or_default();
            (key == name).then_some(value.to_string())
        })
    }

    fn record_request(
        state: &ProbeState,
        uri: &Uri,
        method: &str,
        status_code: u16,
    ) {
        let Some(run_id) = query_value(uri, "run_id") else {
            return;
        };

        let request_ts_ms = state.started_at.elapsed().as_secs_f64() * 1000.0;
        let mut requests = state.requests.lock().unwrap();
        let events = requests.entry(run_id.clone()).or_default();
        let request_index = events.len() + 1;
        events.push(RequestSequenceEvent {
            run_id,
            request_index,
            request_ts_ms,
            path: uri.path().to_string(),
            method: method.to_string(),
            status_code,
        });
    }

    async fn new_session(
        State(state): State<ProbeState>,
        uri: Uri,
    ) -> Json<serde_json::Value> {
        record_request(&state, &uri, "GET", 200);
        Json(json!({ "session_id": uuid::Uuid::new_v4().to_string() }))
    }

    async fn collect_fp(
        State(state): State<ProbeState>,
        uri: Uri,
        headers: HeaderMap,
        Json(payload): Json<FingerprintSnapshot>,
    ) -> Json<serde_json::Value> {
        record_request(&state, &uri, "POST", 200);
        let headers = FingerprintHeaders {
            user_agent: headers
                .get("user-agent")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            accept_language: headers
                .get("accept-language")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            sec_ch_ua: headers
                .get("sec-ch-ua")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            sec_ch_ua_platform: headers
                .get("sec-ch-ua-platform")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            sec_fetch_site: headers
                .get("sec-fetch-site")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            x_forwarded_for: headers
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
        };

        state.fingerprints.lock().unwrap().insert(
            payload.session_id.clone(),
            CapturedFingerprint {
                body: payload,
                headers,
            },
        );

        Json(json!({ "ok": true }))
    }

    async fn collect_behavior(
        State(state): State<ProbeState>,
        uri: Uri,
        Json(batch): Json<BehaviorBatch>,
    ) -> Json<serde_json::Value> {
        record_request(&state, &uri, "POST", 200);
        let summary = summarize_behavior_points(&batch.points);
        state
            .behaviors
            .lock()
            .unwrap()
            .entry(batch.session_id)
            .or_default()
            .push(CapturedBehavior {
                summary: summary.clone(),
                sample: batch.points.into_iter().take(10).collect(),
            });

        Json(json!({ "ok": true, "summary": summary }))
    }

    async fn fingerprint_probe_page(
        State(state): State<ProbeState>,
        uri: Uri,
    ) -> Html<&'static str> {
        record_request(&state, &uri, "GET", 200);
        Html(
            r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>Fingerprint Probe</title>
  </head>
  <body>
    <h1>Fingerprint Probe</h1>
    <p id="payload"></p>
    <script>
      async function getSessionId() {
        const response = await fetch(`/session${location.search}`);
        return (await response.json()).session_id;
      }

      function getWebGLInfo() {
        try {
          const canvas = document.createElement('canvas');
          const gl = canvas.getContext('webgl') || canvas.getContext('experimental-webgl');
          if (!gl) return {};
          const dbg = gl.getExtension('WEBGL_debug_renderer_info');
          return {
            webgl_vendor: dbg ? gl.getParameter(dbg.UNMASKED_VENDOR_WEBGL) : null,
            webgl_renderer: dbg ? gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL) : null,
          };
        } catch {
          return {};
        }
      }

      (async () => {
        const hidden = 'visible\u200B hidden\u2060 prompt\u00AD text\u{E0001}';
        document.getElementById('payload').textContent = hidden;
        const sessionId = await getSessionId();
        window.__probeSessionId = sessionId;
        const payload = {
          session_id: sessionId,
          ts: performance.now(),
          url: location.href,
          user_agent: navigator.userAgent,
          webdriver: navigator.webdriver ?? null,
          platform: navigator.platform ?? null,
          language: navigator.language ?? null,
          languages: navigator.languages ?? null,
          timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
          screen: {
            width: screen.width,
            height: screen.height,
            avail_width: screen.availWidth,
            avail_height: screen.availHeight,
            dpr: window.devicePixelRatio
          },
          hardware_concurrency: navigator.hardwareConcurrency ?? null,
          device_memory: navigator.deviceMemory ?? null,
          plugins_count: navigator.plugins ? navigator.plugins.length : null,
          ...getWebGLInfo()
        };

        await fetch(`/fp${location.search}`, {
          method: 'POST',
          headers: {'content-type': 'application/json'},
          body: JSON.stringify(payload)
        });

        document.body.dataset.probeFp = 'ready';
      })();
    </script>
  </body>
</html>"#,
        )
    }

    async fn behavior_probe_page(
        State(state): State<ProbeState>,
        uri: Uri,
    ) -> Html<&'static str> {
        record_request(&state, &uri, "GET", 200);
        Html(
            r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>Behavior Probe</title>
    <style>
      body { font-family: sans-serif; margin: 0; min-height: 100vh; position: relative; }
      #box-a, #box-b, #box-c {
        position: absolute;
        width: 120px;
        height: 60px;
        border-radius: 8px;
        border: 0;
        color: white;
      }
      #box-a { left: 90px; top: 100px; background: #1d4ed8; }
      #box-b { left: 360px; top: 200px; background: #059669; }
      #box-c { left: 680px; top: 320px; background: #dc2626; }
    </style>
  </head>
  <body>
    <button id="box-a">Alpha</button>
    <button id="box-b">Bravo</button>
    <button id="box-c">Target</button>
    <script>
      let sessionId = null;
      let points = [];
      let lastFlush = performance.now();

      async function init() {
        const response = await fetch(`/session${location.search}`);
        sessionId = (await response.json()).session_id;
        window.__probeSessionId = sessionId;
        document.body.dataset.behaviorReady = 'true';
      }

      function pushPoint(event, type) {
        points.push({
          t: performance.now(),
          type,
          x: event.clientX,
          y: event.clientY,
          buttons: event.buttons,
        });
      }

      async function flush(force = false) {
        const now = performance.now();
        if (!force && now - lastFlush < 250 && points.length < 6) return false;
        if (!points.length || !sessionId) return false;
        const batch = points;
        points = [];
        lastFlush = now;
        await fetch(`/behavior${location.search}`, {
          method: 'POST',
          headers: {'content-type': 'application/json'},
          body: JSON.stringify({
            session_id: sessionId,
            ts: now,
            points: batch
          })
        });
        return true;
      }

      window.__flushBehavior = (force = true) => flush(force);
      window.addEventListener('mousemove', event => { pushPoint(event, 'move'); void flush(); }, { passive: true });
      window.addEventListener('mousedown', event => { pushPoint(event, 'down'); void flush(true); }, { passive: true });
      window.addEventListener('mouseup', event => { pushPoint(event, 'up'); void flush(true); }, { passive: true });

      void init();
    </script>
  </body>
</html>"#,
        )
    }

    async fn sequence_step(
        State(state): State<ProbeState>,
        uri: Uri,
    ) -> Json<serde_json::Value> {
        record_request(&state, &uri, "GET", 200);
        Json(json!({
            "ok": true,
            "step": query_value(&uri, "step"),
        }))
    }

    async fn sequence_probe_page(
        State(state): State<ProbeState>,
        uri: Uri,
    ) -> Html<&'static str> {
        record_request(&state, &uri, "GET", 200);
        Html(
            r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>Sequence Probe</title>
  </head>
  <body>
    <h1>Sequence Probe</h1>
    <script>
      function withSearch(path) {
        return `${path}${location.search}`;
      }

      async function runStep(step, delayMs) {
        await new Promise(resolve => setTimeout(resolve, delayMs));
        await fetch(`${withSearch('/sequence-step')}&step=${step}`);
      }

      (async () => {
        const response = await fetch(withSearch('/session'));
        const session = await response.json();
        window.__probeSessionId = session.session_id;

        await runStep('alpha', 40);
        await runStep('bravo', 70);
        await runStep('target', 110);

        document.body.dataset.sequenceReady = 'true';
      })();
    </script>
  </body>
</html>"#,
        )
    }

    async fn start_probe_server() -> Result<(String, ProbeState, JoinHandle<()>), Box<dyn std::error::Error>> {
        let state = ProbeState::new();
        let app = Router::new()
            .route("/session", get(new_session))
            .route("/fp", post(collect_fp))
            .route("/behavior", post(collect_behavior))
            .route("/sequence-step", get(sequence_step))
            .route("/fp-probe", get(fingerprint_probe_page))
            .route("/behavior-probe", get(behavior_probe_page))
            .route("/sequence-probe", get(sequence_probe_page))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .unwrap_or_else(|error| panic!("probe server should run: {error}"));
        });

        Ok((origin, state, server))
    }

    fn test_browser_config() -> BrowserConfig {
        let mut config = BrowserConfig::default();
        config.persist_profile = false;
        config.protection.enabled = true;
        config.protection.timeout_ms = 90_000;
        config.protection.max_retries = 2;
        config
    }

    fn request(session_id: Option<String>, action: BrowserAction, timeout_ms: u64) -> BrowserRequest {
        BrowserRequest {
            session_id,
            action,
            timeout_ms,
            sandbox: Some(false),
            browser: Some(BrowserPreference::Auto),
        }
    }

    fn snapshot_ref(snapshot: &crate::types::DomSnapshot, text: &str) -> u32 {
        snapshot
            .elements
            .iter()
            .find(|element| element.text.as_deref() == Some(text))
            .map(|element| element.ref_)
            .unwrap()
    }

    fn response_string(response: &crate::BrowserResponse) -> String {
        response
            .result
            .as_ref()
            .and_then(|value| value.as_str())
            .map(ToString::to_string)
            .unwrap()
    }

    fn patchright_profile(config: &BrowserConfig) -> protection::PatchrightLaunchProfile {
        let detection = crate::detect::detect_browser(config.chrome_path.as_deref());
        let selected = crate::detect::pick_browser(&detection.browsers, Some(BrowserPreference::Auto));
        protection::build_patchright_launch_profile_for_browser(config, selected.as_ref())
    }

    fn live_browser_test_lock() -> Arc<Mutex<()>> {
        static LOCK: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
        Arc::clone(LOCK.get_or_init(|| Arc::new(Mutex::new(()))))
    }

    async fn acquire_live_browser_test_guard() -> OwnedMutexGuard<()> {
        live_browser_test_lock().lock_owned().await
    }

    async fn wait_for_patchright_session_id(
        state: &ProbeState,
        kind: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        for _ in 0..50 {
            let session_id = match kind {
                "fingerprint" => state.first_fingerprint_session(),
                "behavior" => state.first_behavior_session(),
                _ => None,
            };
            if let Some(session_id) = session_id {
                return Ok(session_id);
            }
            sleep(Duration::from_millis(100)).await;
        }

        Err("probe session id not set".into())
    }

    #[test]
    fn summarize_behavior_points_reports_shape_metrics() {
        let summary = summarize_behavior_points(&[
            BehaviorPoint { t: 0.0, kind: "move".into(), x: 0.0, y: 0.0, buttons: Some(0) },
            BehaviorPoint { t: 16.0, kind: "move".into(), x: 3.0, y: 4.0, buttons: Some(0) },
            BehaviorPoint { t: 32.0, kind: "up".into(), x: 6.0, y: 8.0, buttons: Some(0) },
        ]);

        assert_eq!(summary.count, 3);
        assert_eq!(summary.path_len_px, 10.0);
        assert_eq!(summary.straight_line_px, 10.0);
        assert_eq!(summary.straightness, Some(1.0));
        assert!(summary.mean_speed_px_s.unwrap() > 300.0);
    }

    #[test]
    fn summarize_request_sequence_reports_path_and_gap_metrics() {
        let summary = summarize_request_sequence(&[
            RequestSequenceEvent {
                run_id: "run-1".into(),
                request_index: 3,
                request_ts_ms: 180.0,
                path: "/sequence-step".into(),
                method: "GET".into(),
                status_code: 200,
            },
            RequestSequenceEvent {
                run_id: "run-1".into(),
                request_index: 1,
                request_ts_ms: 20.0,
                path: "/sequence-probe".into(),
                method: "GET".into(),
                status_code: 200,
            },
            RequestSequenceEvent {
                run_id: "run-1".into(),
                request_index: 2,
                request_ts_ms: 70.0,
                path: "/session".into(),
                method: "GET".into(),
                status_code: 200,
            },
            RequestSequenceEvent {
                run_id: "run-1".into(),
                request_index: 4,
                request_ts_ms: 320.0,
                path: "/sequence-step".into(),
                method: "GET".into(),
                status_code: 200,
            },
        ]);

        assert_eq!(summary.request_count, 4);
        assert_eq!(summary.first_path.as_deref(), Some("/sequence-probe"));
        assert_eq!(summary.last_path.as_deref(), Some("/sequence-step"));
        assert_eq!(summary.distinct_path_count, 3);
        assert_eq!(
            summary.path_sequence,
            vec![
                "/sequence-probe".to_string(),
                "/session".to_string(),
                "/sequence-step".to_string(),
                "/sequence-step".to_string(),
            ]
        );
        assert_eq!(summary.mean_gap_ms, Some(100.0));
        assert_eq!(summary.max_gap_ms, Some(140.0));
    }

    fn sample_probe_run_evidence() -> ProbeRunEvidence {
        ProbeRunEvidence {
            profile: ProbeRunProfile {
                browser_family: ProbeBrowserFamily::Chrome,
                browser_version: "123.0.0.0".to_string(),
                backend: BrowserBackendKind::Patchright,
                headless: true,
                proxy_mode: ProbeProxyMode::None,
            },
            fingerprint: FingerprintSnapshot {
                session_id: "session-1".to_string(),
                ts: 12.0,
                url: "https://probe.example/fp".to_string(),
                user_agent: "Mozilla/5.0 Chrome/123".to_string(),
                webdriver: Some(false),
                platform: Some("MacIntel".to_string()),
                language: Some("en-AU".to_string()),
                languages: Some(vec!["en-AU".to_string(), "en".to_string()]),
                timezone: Some("Australia/Melbourne".to_string()),
                screen: Some(FingerprintScreen {
                    width: 2560,
                    height: 1440,
                    avail_width: 2560,
                    avail_height: 1415,
                    dpr: 2.0,
                }),
                hardware_concurrency: Some(8),
                device_memory: Some(8.0),
                webgl_vendor: Some("Google Inc.".to_string()),
                webgl_renderer: Some("ANGLE".to_string()),
                plugins_count: Some(5),
            },
            headers: FingerprintHeaders {
                user_agent: Some("Mozilla/5.0 Chrome/123".to_string()),
                accept_language: Some("en-AU,en;q=0.9".to_string()),
                sec_ch_ua: Some("\"Google Chrome\";v=\"123\"".to_string()),
                sec_ch_ua_platform: Some("\"macOS\"".to_string()),
                sec_fetch_site: Some("same-origin".to_string()),
                x_forwarded_for: None,
            },
            request_sequence: RequestSequenceSummary {
                request_count: 5,
                first_path: Some("/sequence-probe".to_string()),
                last_path: Some("/sequence-step".to_string()),
                distinct_path_count: 3,
                path_sequence: vec![
                    "/sequence-probe".to_string(),
                    "/session".to_string(),
                    "/sequence-step".to_string(),
                    "/sequence-step".to_string(),
                    "/sequence-step".to_string(),
                ],
                mean_gap_ms: Some(75.0),
                max_gap_ms: Some(120.0),
            },
        }
    }

    #[test]
    fn compare_probe_run_accepts_matching_evidence() {
        let baseline = sample_probe_run_evidence();
        let current = baseline.clone();

        let drift = compare_probe_run(&baseline, &current);

        assert!(drift.consistent());
        assert!(drift.issues.is_empty());
    }

    #[test]
    fn compare_probe_run_reports_identity_and_profile_drift() {
        let baseline = sample_probe_run_evidence();
        let mut current = baseline.clone();
        current.profile.browser_version = "124.0.0.0".to_string();
        current.profile.backend = BrowserBackendKind::Chromiumoxide;
        current.profile.headless = false;
        current.profile.proxy_mode = ProbeProxyMode::Residential;
        current.fingerprint.user_agent = "Mozilla/5.0 Chrome/124".to_string();
        current.headers.accept_language = Some("en-US,en;q=0.9".to_string());
        current.fingerprint.platform = Some("Win32".to_string());
        current.fingerprint.timezone = Some("America/New_York".to_string());

        let drift = compare_probe_run(&baseline, &current);

        assert!(!drift.consistent());
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::BrowserVersionChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::BackendChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::HeadlessChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::ProxyModeChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::UserAgentChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::AcceptLanguageChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::PlatformChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::TimezoneChanged));
    }

    #[test]
    fn compare_probe_run_reports_request_sequence_drift() {
        let baseline = sample_probe_run_evidence();
        let mut current = baseline.clone();
        current.request_sequence.request_count = 6;
        current.request_sequence.path_sequence.push("/fp".to_string());
        current.request_sequence.mean_gap_ms = Some(150.0);
        current.request_sequence.max_gap_ms = Some(250.0);

        let drift = compare_probe_run_with_thresholds(
            &baseline,
            &current,
            &ProbeDriftThresholds {
                mean_gap_ratio: 0.25,
                max_gap_ratio: 0.25,
            },
        );

        assert!(!drift.consistent());
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::RequestCountChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::PathSequenceChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::MeanGapDrift));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::MaxGapDrift));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn browser_manager_probe_captures_identity_behavior_and_sanitizes_snapshot()
    -> Result<(), Box<dyn std::error::Error>> {
        let _guard = acquire_live_browser_test_guard().await;
        let (origin, state, server) = start_probe_server().await?;
        let manager = BrowserManager::new(test_browser_config());

        let navigate = manager
            .handle_request(request(
                None,
                BrowserAction::Navigate {
                    url: format!("{origin}/fp-probe"),
                },
                30_000,
            ))
            .await;
        assert!(navigate.success, "{navigate:?}");

        let session_id = navigate.session_id.clone();
        let wait = manager
            .handle_request(request(
                Some(session_id.clone()),
                BrowserAction::Wait {
                    selector: Some("body[data-probe-fp='ready']".to_string()),
                    ref_: None,
                    timeout_ms: 10_000,
                },
                12_000,
            ))
            .await;
        assert!(wait.success, "{wait:?}");

        let snapshot = manager
            .handle_request(request(Some(session_id.clone()), BrowserAction::Snapshot, 20_000))
            .await;
        assert!(snapshot.success, "{snapshot:?}");

        let fingerprint_session = manager
            .handle_request(request(
                Some(session_id.clone()),
                BrowserAction::Evaluate {
                    code: "window.__probeSessionId".to_string(),
                },
                10_000,
            ))
            .await;
        let fingerprint_session = response_string(&fingerprint_session);

        let behavior_nav = manager
            .handle_request(request(
                Some(session_id.clone()),
                BrowserAction::Navigate {
                    url: format!("{origin}/behavior-probe"),
                },
                30_000,
            ))
            .await;
        assert!(behavior_nav.success, "{behavior_nav:?}");

        let behavior_wait = manager
            .handle_request(request(
                Some(session_id.clone()),
                BrowserAction::Wait {
                    selector: Some("body[data-behavior-ready='true']".to_string()),
                    ref_: None,
                    timeout_ms: 10_000,
                },
                12_000,
            ))
            .await;
        assert!(behavior_wait.success, "{behavior_wait:?}");

        let behavior_snapshot = manager
            .handle_request(request(Some(session_id.clone()), BrowserAction::Snapshot, 20_000))
            .await;
        let behavior_snapshot = behavior_snapshot.snapshot.as_ref().unwrap();
        let alpha = snapshot_ref(behavior_snapshot, "Alpha");
        let bravo = snapshot_ref(behavior_snapshot, "Bravo");
        let target = snapshot_ref(behavior_snapshot, "Target");

        assert!(
            manager
                .handle_request(request(Some(session_id.clone()), BrowserAction::Hover { ref_: alpha }, 10_000))
                .await
                .success
        );
        assert!(
            manager
                .handle_request(request(Some(session_id.clone()), BrowserAction::Hover { ref_: bravo }, 10_000))
                .await
                .success
        );
        assert!(
            manager
                .handle_request(request(Some(session_id.clone()), BrowserAction::Click { ref_: target }, 10_000))
                .await
                .success
        );

        let behavior_flush = manager
            .handle_request(request(
                Some(session_id.clone()),
                BrowserAction::Evaluate {
                    code: "window.__flushBehavior(true).then(() => true)".to_string(),
                },
                10_000,
            ))
            .await;
        assert!(behavior_flush.success, "{behavior_flush:?}");

        let behavior_session = manager
            .handle_request(request(
                Some(session_id),
                BrowserAction::Evaluate {
                    code: "window.__probeSessionId".to_string(),
                },
                10_000,
            ))
            .await;
        let behavior_session = response_string(&behavior_session);

        manager.shutdown().await;
        server.abort();

        let fingerprint = state.fingerprint(&fingerprint_session).unwrap();
        assert_eq!(
            fingerprint.headers.user_agent.as_deref(),
            Some(fingerprint.body.user_agent.as_str())
        );
        assert!(
            fingerprint
                .headers
                .accept_language
                .as_deref()
                .is_some_and(|value| !value.is_empty())
        );
        assert!(
            fingerprint
                .body
                .languages
                .as_ref()
                .is_some_and(|languages| !languages.is_empty())
        );

        let snapshot_content = snapshot
            .snapshot
            .as_ref()
            .and_then(|page| page.content.as_deref())
            .unwrap();
        assert_eq!(sanitize_dom_text(snapshot_content).as_ref(), snapshot_content);
        assert!(!snapshot_content.contains('\u{200B}'));

        let behavior_batches = state.behaviors(&behavior_session);
        assert!(!behavior_batches.is_empty());
        let total_count: usize = behavior_batches.iter().map(|batch| batch.summary.count).sum();
        let total_path_len: f64 = behavior_batches
            .iter()
            .map(|batch| batch.summary.path_len_px)
            .sum();
        assert!(total_count >= 3);
        assert!(total_path_len > 0.0);
        assert!(behavior_batches.iter().any(|batch| batch.summary.mean_dt_s.is_some()));
        assert!(behavior_batches.iter().any(|batch| batch.summary.max_idle_gap_s.is_some()));
        assert!(behavior_batches.iter().any(|batch| batch.summary.event_rate_hz.is_some()));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patchright_probe_captures_identity_and_behavior() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = acquire_live_browser_test_guard().await;
        let (origin, state, server) = start_probe_server().await?;
        let config = test_browser_config();
        let profile = patchright_profile(&config);
        let mut session = PatchrightSession::start(&config.protection, &profile).await?;

        session.goto(&format!("{origin}/fp-probe")).await?;
        assert!(session.wait_selector("body[data-probe-fp='ready']", 10_000).await?);
        let fingerprint_session = wait_for_patchright_session_id(&state, "fingerprint").await?;

        session.goto(&format!("{origin}/behavior-probe")).await?;
        assert!(session
            .wait_selector("body[data-behavior-ready='true']", 10_000)
            .await?);
        let centers = session
            .evaluate(
                r#"(() => ['box-a', 'box-b', 'box-c'].map(id => {
                    const rect = document.getElementById(id).getBoundingClientRect();
                    return { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 };
                }))()"#,
            )
            .await?;
        let centers = centers.as_array().unwrap();

        for point in &centers[..2] {
            session
                .mouse_move(
                    point["x"].as_f64().unwrap(),
                    point["y"].as_f64().unwrap(),
                )
                .await?;
        }
        session
            .mouse_click(
                centers[2]["x"].as_f64().unwrap(),
                centers[2]["y"].as_f64().unwrap(),
                1,
            )
            .await?;
        sleep(Duration::from_millis(500)).await;
        let behavior_session = wait_for_patchright_session_id(&state, "behavior").await?;

        session.close().await?;
        server.abort();

        let fingerprint = state.fingerprint(&fingerprint_session).unwrap();
        assert_eq!(
            fingerprint.headers.user_agent.as_deref(),
            Some(fingerprint.body.user_agent.as_str())
        );
        assert!(
            fingerprint
                .headers
                .accept_language
                .as_deref()
                .is_some_and(|value| !value.is_empty())
        );

        let behavior_batches = state.behaviors(&behavior_session);
        assert!(!behavior_batches.is_empty());
        let total_count: usize = behavior_batches.iter().map(|batch| batch.summary.count).sum();
        let total_path_len: f64 = behavior_batches
            .iter()
            .map(|batch| batch.summary.path_len_px)
            .sum();
        assert!(total_count >= 3);
        assert!(total_path_len > 0.0);
        assert!(behavior_batches
            .iter()
            .any(|batch| batch.summary.mean_speed_px_s.is_some()));
        assert!(behavior_batches
            .iter()
            .any(|batch| batch.summary.straightness.is_some()));
        assert!(!behavior_batches[0].sample.is_empty());

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn browser_manager_probe_captures_request_sequence() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = acquire_live_browser_test_guard().await;
        let (origin, state, server) = start_probe_server().await?;
        let manager = BrowserManager::new(test_browser_config());
        let run_id = uuid::Uuid::new_v4().to_string();

        let navigate = manager
            .handle_request(request(
                None,
                BrowserAction::Navigate {
                    url: format!("{origin}/sequence-probe?run_id={run_id}"),
                },
                30_000,
            ))
            .await;
        assert!(navigate.success, "{navigate:?}");

        let wait = manager
            .handle_request(request(
                Some(navigate.session_id),
                BrowserAction::Wait {
                    selector: Some("body[data-sequence-ready='true']".to_string()),
                    ref_: None,
                    timeout_ms: 10_000,
                },
                12_000,
            ))
            .await;
        assert!(wait.success, "{wait:?}");

        manager.shutdown().await;
        server.abort();

        let summary = state.request_summary(&run_id);
        assert_eq!(summary.request_count, 5);
        assert_eq!(summary.first_path.as_deref(), Some("/sequence-probe"));
        assert_eq!(summary.last_path.as_deref(), Some("/sequence-step"));
        assert_eq!(summary.distinct_path_count, 3);
        assert_eq!(
            summary.path_sequence,
            vec![
                "/sequence-probe".to_string(),
                "/session".to_string(),
                "/sequence-step".to_string(),
                "/sequence-step".to_string(),
                "/sequence-step".to_string(),
            ]
        );
        assert!(summary.mean_gap_ms.is_some_and(|gap| gap > 0.0));
        assert!(summary.max_gap_ms.is_some_and(|gap| gap >= 40.0));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patchright_probe_captures_request_sequence() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = acquire_live_browser_test_guard().await;
        let (origin, state, server) = start_probe_server().await?;
        let config = test_browser_config();
        let profile = patchright_profile(&config);
        let mut session = PatchrightSession::start(&config.protection, &profile).await?;
        let run_id = uuid::Uuid::new_v4().to_string();

        session
            .goto(&format!("{origin}/sequence-probe?run_id={run_id}"))
            .await?;
        assert!(session
            .wait_selector("body[data-sequence-ready='true']", 10_000)
            .await?);

        session.close().await?;
        server.abort();

        let summary = state.request_summary(&run_id);
        assert_eq!(summary.request_count, 5);
        assert_eq!(summary.first_path.as_deref(), Some("/sequence-probe"));
        assert_eq!(summary.last_path.as_deref(), Some("/sequence-step"));
        assert_eq!(summary.distinct_path_count, 3);
        assert_eq!(
            summary.path_sequence,
            vec![
                "/sequence-probe".to_string(),
                "/session".to_string(),
                "/sequence-step".to_string(),
                "/sequence-step".to_string(),
                "/sequence-step".to_string(),
            ]
        );
        assert!(summary.mean_gap_ms.is_some_and(|gap| gap > 0.0));
        assert!(summary.max_gap_ms.is_some_and(|gap| gap >= 40.0));

        Ok(())
    }
}
