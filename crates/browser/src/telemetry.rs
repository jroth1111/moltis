//! Telemetry helpers for permissioned browser measurement.
//!
//! These types support safe, owned-site measurement of browser-visible
//! identity and interaction distributions so regressions can be detected
//! before anti-bot changes reach production targets.

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
            http::HeaderMap,
            response::Html,
            routing::{get, post},
        },
        serde_json::json,
        std::{
            collections::HashMap,
            sync::{Arc, Mutex as StdMutex},
        },
        tokio::{
            net::TcpListener,
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

    #[derive(Clone, Default)]
    struct ProbeState {
        fingerprints: Arc<StdMutex<HashMap<String, CapturedFingerprint>>>,
        behaviors: Arc<StdMutex<HashMap<String, Vec<CapturedBehavior>>>>,
    }

    impl ProbeState {
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
    }

    #[derive(Debug, Deserialize)]
    struct BehaviorBatch {
        session_id: String,
        #[allow(dead_code)]
        ts: f64,
        points: Vec<BehaviorPoint>,
    }

    async fn new_session() -> Json<serde_json::Value> {
        Json(json!({ "session_id": uuid::Uuid::new_v4().to_string() }))
    }

    async fn collect_fp(
        State(state): State<ProbeState>,
        headers: HeaderMap,
        Json(payload): Json<FingerprintSnapshot>,
    ) -> Json<serde_json::Value> {
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
        Json(batch): Json<BehaviorBatch>,
    ) -> Json<serde_json::Value> {
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

    async fn fingerprint_probe_page() -> Html<&'static str> {
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
        const response = await fetch('/session');
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

        await fetch('/fp', {
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

    async fn behavior_probe_page() -> Html<&'static str> {
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
        const response = await fetch('/session');
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
        await fetch('/behavior', {
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

    async fn start_probe_server() -> Result<(String, ProbeState, JoinHandle<()>), Box<dyn std::error::Error>> {
        let state = ProbeState::default();
        let app = Router::new()
            .route("/session", get(new_session))
            .route("/fp", post(collect_fp))
            .route("/behavior", post(collect_behavior))
            .route("/fp-probe", get(fingerprint_probe_page))
            .route("/behavior-probe", get(behavior_probe_page))
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

    #[tokio::test(flavor = "multi_thread")]
    async fn browser_manager_probe_captures_identity_behavior_and_sanitizes_snapshot()
    -> Result<(), Box<dyn std::error::Error>> {
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
}
