//! Browser instance pool management.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use {
    chromiumoxide::{
        Browser, BrowserConfig as CdpBrowserConfig, Page,
        cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams, handler::HandlerConfig,
    },
    futures::StreamExt,
    sysinfo::System,
    tokio::sync::{Mutex, RwLock, broadcast},
    tracing::{debug, info, warn},
};

use crate::{
    container::BrowserContainer,
    error::Error,
    types::{BrowserConfig, BrowserPreference},
    virtual_display::VirtualDisplay,
};

/// Get current system memory usage as a percentage (0-100).
fn get_memory_usage_percent() -> u8 {
    let mut sys = System::new();
    sys.refresh_memory();

    let total = sys.total_memory();
    if total == 0 {
        return 0;
    }

    let used = sys.used_memory();
    let percent = (used as f64 / total as f64 * 100.0) as u8;
    percent.min(100)
}

/// Returns memory-saving Chrome flags when `total_mb` is below `threshold_mb`.
///
/// Returns an empty slice when the threshold is 0 (disabled) or when the system
/// has enough memory.
#[must_use]
pub(crate) fn low_memory_chrome_args(total_mb: u64, threshold_mb: u64) -> &'static [&'static str] {
    if threshold_mb == 0 || total_mb >= threshold_mb {
        return &[];
    }
    &[
        "--single-process",
        "--renderer-process-limit=1",
        "--js-flags=--max-old-space-size=128",
    ]
}

/// Sanitize user-provided Chrome args for safer stealth operation.
///
/// - Removes duplicate args while preserving order.
/// - Removes `--enable-automation` in stealth mode.
#[must_use]
fn sanitize_user_chrome_args(args: &[String], stealth_enabled: bool) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut sanitized = Vec::with_capacity(args.len());
    for arg in args {
        if stealth_enabled
            && (arg == "--enable-automation" || arg.starts_with("--enable-automation="))
        {
            continue;
        }
        if seen.insert(arg.clone()) {
            sanitized.push(arg.clone());
        }
    }
    sanitized
}

/// A pooled browser instance with one or more pages.
struct BrowserInstance {
    browser: Browser,
    pages: HashMap<String, Page>,
    last_used: Instant,
    /// Whether this instance is running in sandbox mode.
    #[allow(dead_code)]
    sandboxed: bool,
    /// Container for sandboxed instances (None for host browser).
    #[allow(dead_code)]
    container: Option<BrowserContainer>,
    /// Active tab name (key into `pages`). Defaults to `"main"`.
    active_tab: String,
    /// Last known mouse cursor position (x, y) for bezier movement continuity.
    current_mouse_pos: (f64, f64),
    /// Active request interception state.
    interception: crate::network::InterceptionState,
    /// Passive API capture state for request-shape inference.
    api_capture: crate::api_capture::ApiCaptureRuntime,
    /// Active screencast handle (if screencast is running).
    screencast_handle: Option<crate::screencast::ScreencastHandle>,
    /// Optional virtual display backing headful runs without visible UI.
    #[allow(dead_code)]
    virtual_display: Option<VirtualDisplay>,
}

/// Pool of browser instances for reuse.
pub struct BrowserPool {
    config: BrowserConfig,
    instances: RwLock<HashMap<String, Arc<Mutex<BrowserInstance>>>>,
    #[cfg(feature = "metrics")]
    active_count: std::sync::atomic::AtomicUsize,
}

impl BrowserPool {
    /// Create a new browser pool with the given configuration.
    pub fn new(config: BrowserConfig) -> Self {
        Self {
            config,
            instances: RwLock::new(HashMap::new()),
            #[cfg(feature = "metrics")]
            active_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Get or create a browser instance for the given session ID.
    /// Returns the session ID for the browser instance.
    ///
    /// The `sandbox` parameter determines whether to run the browser in a
    /// Docker container (true) or on the host (false). This is set when
    /// creating a new session and cannot be changed for existing sessions.
    pub async fn get_or_create(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
        browser: Option<BrowserPreference>,
    ) -> Result<String, Error> {
        // Treat empty string as None (generate new session ID)
        let session_id = session_id.filter(|s| !s.is_empty());

        // Check if we have an existing instance
        if let Some(sid) = session_id {
            let instances = self.instances.read().await;
            if instances.contains_key(sid) {
                debug!(session_id = sid, "reusing existing browser instance");
                return Ok(sid.to_string());
            }
        }

        // Check pool capacity using memory-based limits
        {
            // If max_instances is set (> 0), enforce it as a hard limit
            if self.config.max_instances > 0 {
                let instances = self.instances.read().await;
                if instances.len() >= self.config.max_instances {
                    drop(instances);
                    self.cleanup_idle().await;

                    let instances = self.instances.read().await;
                    if instances.len() >= self.config.max_instances {
                        return Err(Error::PoolExhausted);
                    }
                }
            }

            // Check memory usage - block new instances if above threshold
            let memory_percent = get_memory_usage_percent();
            if memory_percent >= self.config.memory_limit_percent {
                // Try to clean up idle instances first
                self.cleanup_idle().await;

                // Re-check memory after cleanup
                let memory_after = get_memory_usage_percent();
                if memory_after >= self.config.memory_limit_percent {
                    warn!(
                        memory_usage = memory_after,
                        threshold = self.config.memory_limit_percent,
                        "blocking new browser instance due to high memory usage"
                    );
                    return Err(Error::PoolExhausted);
                }
            }
        }

        // Create new instance
        let sid = session_id
            .map(String::from)
            .unwrap_or_else(generate_session_id);

        let instance = self.launch_browser(&sid, sandbox, browser).await?;
        let instance = Arc::new(Mutex::new(instance));

        {
            let mut instances = self.instances.write().await;
            instances.insert(sid.clone(), instance);
        }

        #[cfg(feature = "metrics")]
        {
            self.active_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            moltis_metrics::gauge!(moltis_metrics::browser::INSTANCES_ACTIVE)
                .set(self.active_count.load(std::sync::atomic::Ordering::Relaxed) as f64);
            moltis_metrics::counter!(moltis_metrics::browser::INSTANCES_CREATED_TOTAL).increment(1);
        }

        let mode = if sandbox {
            "sandboxed"
        } else {
            "host"
        };
        info!(session_id = sid, mode, "launched new browser instance");
        Ok(sid)
    }

    /// Get the page for a session, creating one if needed.
    pub async fn get_page(&self, session_id: &str) -> Result<Page, Error> {
        let instances = self.instances.read().await;
        let instance = instances.get(session_id).ok_or(Error::ElementNotFound(0))?;
        let instance_arc = Arc::clone(instance);

        let mut inst = instance.lock().await;
        inst.last_used = Instant::now();

        // Get or create the main page
        if let Some(page) = inst.pages.get("main") {
            debug!(session_id, "reusing existing page");
            return Ok(page.clone());
        }

        // Create a new page
        let page = inst
            .browser
            .new_page("about:blank")
            .await
            .map_err(|e| Error::LaunchFailed(e.to_string()))?;

        // Explicitly set viewport on page to ensure it matches config
        // (browser-level viewport may not always be applied to new pages)
        let viewport_cmd = SetDeviceMetricsOverrideParams::builder()
            .width(self.config.viewport_width)
            .height(self.config.viewport_height)
            .device_scale_factor(self.config.device_scale_factor)
            .mobile(false)
            .build()
            .map_err(|e| Error::Cdp(format!("invalid viewport params: {e}")))?;

        if let Err(e) = page.execute(viewport_cmd).await {
            warn!(session_id, error = %e, "failed to set page viewport");
        }

        info!(
            session_id,
            viewport_width = self.config.viewport_width,
            viewport_height = self.config.viewport_height,
            device_scale_factor = self.config.device_scale_factor,
            "created new page with viewport"
        );

        // Inject JS stealth evasions before first navigation
        #[cfg(feature = "stealth")]
        if self.config.stealth.enabled && self.config.stealth.js_evasion {
            if let Err(e) = crate::stealth::inject_stealth(&page, &self.config.stealth).await {
                warn!(session_id, error = %e, "stealth injection failed, continuing without stealth");
            } else {
                debug!(session_id, "stealth evasions injected");
            }
        }

        #[cfg(feature = "stealth")]
        if self.config.stealth.enabled {
            if let Err(e) = crate::stealth::apply_stealth_headers(&page, &self.config.stealth).await
            {
                warn!(session_id, error = %e, "stealth header setup failed");
            } else {
                debug!(session_id, "stealth headers configured");
            }
        }

        inst.pages.insert("main".to_string(), page.clone());
        let attach_api_capture =
            inst.api_capture.config.is_some() && inst.api_capture.recorder.is_some();
        drop(inst);
        drop(instances);
        if attach_api_capture {
            self.attach_api_capture_to_page(instance_arc, page.clone())
                .await?;
        }
        Ok(page)
    }

    // ── Mouse position tracking ──────────────────────────────────────────────

    /// Get the last known mouse cursor position for a session.
    pub async fn get_mouse_pos(&self, session_id: &str) -> (f64, f64) {
        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(session_id) {
            let inst = instance.lock().await;
            return inst.current_mouse_pos;
        }
        (0.0, 0.0)
    }

    /// Update the last known mouse cursor position for a session.
    pub async fn set_mouse_pos(&self, session_id: &str, pos: (f64, f64)) {
        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(session_id) {
            let mut inst = instance.lock().await;
            inst.current_mouse_pos = pos;
        }
    }

    // ── Network interception & API capture ──────────────────────────────────

    /// Enable network interception for a session.
    ///
    /// Calls `Fetch.enable` on the current page, subscribes to paused-request
    /// events, and auto-continues them after broadcasting each event to callers.
    pub async fn enable_interception(
        &self,
        session_id: &str,
        patterns: Vec<String>,
        extra_headers: HashMap<String, String>,
    ) -> Result<(), Error> {
        use chromiumoxide::cdp::browser_protocol::fetch::EventRequestPaused;

        let page = self.get_page(session_id).await?;

        // Subscribe before enabling Fetch so we do not miss the first matching event.
        let paused_stream = page
            .event_listener::<EventRequestPaused>()
            .await
            .map_err(|e| Error::Cdp(format!("intercept event listener: {e}")))?;

        crate::network::enable_interception(&page, patterns.clone()).await?;

        let (paused_tx, _rx) = broadcast::channel::<Arc<EventRequestPaused>>(32);
        let paused_tx_clone = paused_tx.clone();
        let paused_page = page.clone();

        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(session_id) {
            let instance_arc = Arc::clone(instance);
            let mut inst = instance.lock().await;
            for task in inst.interception.tasks.drain(..) {
                task.abort();
            }
            inst.interception.enabled = true;
            inst.interception.url_patterns = patterns;
            inst.interception.extra_headers = extra_headers;
            inst.interception.paused_tx = Some(paused_tx);

            let paused_task = tokio::spawn(async move {
                let mut stream = paused_stream;
                while let Some(event) = stream.next().await {
                    let extra_headers = {
                        let mut inst = instance_arc.lock().await;
                        let interception_headers = inst.interception.extra_headers.clone();
                        if event.response_status_code.is_none()
                            && let Some(network_id) = event.network_id.as_ref()
                            && let Some(recorder) = inst.api_capture.recorder.as_mut()
                        {
                            recorder.merge_request_headers(network_id, &interception_headers);
                        }
                        if interception_headers.is_empty() {
                            None
                        } else {
                            Some(
                                interception_headers
                                    .iter()
                                    .map(|(name, value)| (name.clone(), value.clone()))
                                    .collect::<Vec<_>>(),
                            )
                        }
                    };
                    // Forward to external subscribers (ignore if none).
                    let _ = paused_tx_clone.send(event.clone());
                    // Auto-continue so the request is never left hanging.
                    let _ = crate::network::continue_request(
                        &paused_page,
                        event.request_id.clone(),
                        extra_headers,
                    )
                    .await;
                }
                debug!("intercept event stream closed");
            });
            inst.interception.tasks = vec![paused_task];
        }

        Ok(())
    }

    /// Disable network interception for a session.
    pub async fn disable_interception(&self, session_id: &str) -> Result<(), Error> {
        let page = self.get_page(session_id).await?;
        crate::network::disable_interception(&page).await?;

        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(session_id) {
            let mut inst = instance.lock().await;
            for task in inst.interception.tasks.drain(..) {
                task.abort();
            }
            inst.interception.paused_tx = None;
            inst.interception.enabled = false;
            inst.interception.url_patterns.clear();
            inst.interception.extra_headers.clear();
        }

        Ok(())
    }

    /// Start passive API capture for the active page in a session.
    pub async fn start_api_capture(
        &self,
        session_id: &str,
        config: crate::api_capture::ApiCaptureConfig,
    ) -> Result<(), Error> {
        self.configure_api_capture(
            session_id,
            config.clone(),
            crate::api_capture::ApiCaptureRecorder::new(config),
        )
        .await
    }

    /// Stop passive API capture and return the inferred API catalog.
    pub async fn stop_api_capture(
        &self,
        session_id: &str,
    ) -> Option<crate::api_capture::ApiCatalog> {
        let instances = self.instances.read().await;
        let instance = instances.get(session_id)?;
        let mut inst = instance.lock().await;

        for task in inst.api_capture.tasks.drain(..) {
            task.abort();
        }
        inst.api_capture.config = None;
        inst.api_capture.attached_targets.clear();

        let mut recorder = inst.api_capture.recorder.take()?;
        recorder.finish();

        #[cfg(feature = "metrics")]
        moltis_metrics::counter!(moltis_metrics::browser::API_CAPTURES_TOTAL).increment(1);

        Some(recorder.build_catalog())
    }

    /// Update extra headers for a session's interception state.
    pub async fn set_extra_headers(&self, session_id: &str, headers: HashMap<String, String>) {
        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(session_id) {
            let mut inst = instance.lock().await;
            inst.interception.extra_headers = headers;
        }
    }

    /// Take interception state out of a session so it can be restored onto
    /// a replacement session after a stale-connection retry.
    pub async fn take_interception_snapshot(
        &self,
        session_id: &str,
    ) -> Option<crate::network::InterceptionSnapshot> {
        let instances = self.instances.read().await;
        let instance = instances.get(session_id)?;
        let mut inst = instance.lock().await;

        if !inst.interception.enabled {
            return None;
        }

        for task in inst.interception.tasks.drain(..) {
            task.abort();
        }
        inst.interception.paused_tx = None;

        Some(crate::network::InterceptionSnapshot {
            enabled: inst.interception.enabled,
            url_patterns: inst.interception.url_patterns.clone(),
            extra_headers: inst.interception.extra_headers.clone(),
        })
    }

    /// Restore interception state onto an already-created session.
    pub async fn restore_interception_snapshot(
        &self,
        session_id: &str,
        snapshot: crate::network::InterceptionSnapshot,
    ) -> Result<(), Error> {
        let crate::network::InterceptionSnapshot {
            enabled,
            url_patterns,
            extra_headers,
        } = snapshot;

        if enabled {
            self.enable_interception(session_id, url_patterns.clone(), extra_headers.clone())
                .await?;
        }

        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(session_id) {
            let mut inst = instance.lock().await;
            if !enabled {
                inst.interception.url_patterns = url_patterns;
                inst.interception.extra_headers = extra_headers;
            }
        }

        Ok(())
    }

    /// Take API capture state out of a session so it can be restored onto
    /// a replacement session after a stale-connection retry.
    pub async fn take_api_capture_snapshot(
        &self,
        session_id: &str,
    ) -> Option<crate::api_capture::ApiCaptureSnapshot> {
        let instances = self.instances.read().await;
        let instance = instances.get(session_id)?;
        let mut inst = instance.lock().await;

        if inst.api_capture.config.is_none() || inst.api_capture.recorder.is_none() {
            return None;
        }

        for task in inst.api_capture.tasks.drain(..) {
            task.abort();
        }
        inst.api_capture.attached_targets.clear();

        let config = inst.api_capture.config.take()?;
        let recorder = inst.api_capture.recorder.take()?;

        Some(crate::api_capture::ApiCaptureSnapshot { config, recorder })
    }

    /// Restore API capture state onto an already-created session.
    pub async fn restore_api_capture_snapshot(
        &self,
        session_id: &str,
        snapshot: crate::api_capture::ApiCaptureSnapshot,
    ) -> Result<(), Error> {
        self.configure_api_capture(session_id, snapshot.config, snapshot.recorder)
            .await
    }

    async fn configure_api_capture(
        &self,
        session_id: &str,
        config: crate::api_capture::ApiCaptureConfig,
        recorder: crate::api_capture::ApiCaptureRecorder,
    ) -> Result<(), Error> {
        let _ = self.get_page(session_id).await?;
        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(session_id) {
            let instance_arc = Arc::clone(instance);

            let mut inst = instance.lock().await;
            for task in inst.api_capture.tasks.drain(..) {
                task.abort();
            }
            inst.api_capture.config = Some(config);
            inst.api_capture.recorder = Some(recorder);
            inst.api_capture.attached_targets.clear();
            let pages: Vec<Page> = inst.pages.values().cloned().collect();
            drop(inst);
            drop(instances);

            for page in pages {
                self.attach_api_capture_to_page(Arc::clone(&instance_arc), page)
                    .await?;
            }
        }

        Ok(())
    }

    async fn attach_api_capture_to_page(
        &self,
        instance: Arc<Mutex<BrowserInstance>>,
        page: Page,
    ) -> Result<(), Error> {
        use chromiumoxide::cdp::browser_protocol::network::{
            EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent,
            EventResponseReceived, GetRequestPostDataParams,
        };

        let target_id = page.target_id().as_ref().to_string();
        {
            let inst = instance.lock().await;
            if inst.api_capture.config.is_none()
                || inst.api_capture.recorder.is_none()
                || inst.api_capture.attached_targets.contains(&target_id)
            {
                return Ok(());
            }
        }

        let request_stream = page
            .event_listener::<EventRequestWillBeSent>()
            .await
            .map_err(|error| Error::Cdp(format!("requestWillBeSent event listener: {error}")))?;
        let response_stream = page
            .event_listener::<EventResponseReceived>()
            .await
            .map_err(|error| Error::Cdp(format!("responseReceived event listener: {error}")))?;
        let loading_failed_stream = page
            .event_listener::<EventLoadingFailed>()
            .await
            .map_err(|error| Error::Cdp(format!("loadingFailed event listener: {error}")))?;
        let loading_finished_stream = page
            .event_listener::<EventLoadingFinished>()
            .await
            .map_err(|error| Error::Cdp(format!("loadingFinished event listener: {error}")))?;

        let request_instance = Arc::clone(&instance);
        let response_instance = Arc::clone(&instance);
        let loading_failed_instance = Arc::clone(&instance);
        let loading_finished_instance = Arc::clone(&instance);

        let request_task = tokio::spawn(async move {
            let mut stream = request_stream;
            while let Some(event) = stream.next().await {
                let method_may_have_body = !matches!(event.request.method.as_str(), "GET" | "HEAD");
                let fallback_request_body = if (event.request.has_post_data.unwrap_or(false)
                    || method_may_have_body)
                    && event
                        .request
                        .post_data_entries
                        .as_ref()
                        .is_none_or(Vec::is_empty)
                {
                    match page
                        .execute(GetRequestPostDataParams::new(event.request_id.clone()))
                        .await
                    {
                        Ok(response) => Some(response.result.post_data),
                        Err(error) => {
                            debug!(request_id = ?event.request_id, ?error, "getRequestPostData failed");
                            None
                        },
                    }
                } else {
                    None
                };
                let mut inst = request_instance.lock().await;
                if let Some(recorder) = inst.api_capture.recorder.as_mut() {
                    recorder.record_request(&event, fallback_request_body);
                }
            }
            debug!("requestWillBeSent event stream closed");
        });

        let response_task = tokio::spawn(async move {
            let mut stream = response_stream;
            while let Some(event) = stream.next().await {
                let mut inst = response_instance.lock().await;
                if let Some(recorder) = inst.api_capture.recorder.as_mut() {
                    recorder.apply_response_received(&event);
                }
            }
            debug!("responseReceived event stream closed");
        });

        let loading_failed_task = tokio::spawn(async move {
            let mut stream = loading_failed_stream;
            while let Some(event) = stream.next().await {
                let mut inst = loading_failed_instance.lock().await;
                if let Some(recorder) = inst.api_capture.recorder.as_mut() {
                    recorder.apply_loading_failed(&event);
                }
            }
            debug!("loadingFailed event stream closed");
        });

        let loading_finished_task = tokio::spawn(async move {
            let mut stream = loading_finished_stream;
            while let Some(event) = stream.next().await {
                let mut inst = loading_finished_instance.lock().await;
                if let Some(recorder) = inst.api_capture.recorder.as_mut() {
                    recorder.apply_loading_finished(&event.request_id);
                }
            }
            debug!("loadingFinished event stream closed");
        });

        let mut inst = instance.lock().await;
        if inst.api_capture.config.is_none() || inst.api_capture.recorder.is_none() {
            request_task.abort();
            response_task.abort();
            loading_failed_task.abort();
            loading_finished_task.abort();
            return Ok(());
        }
        if inst.api_capture.attached_targets.insert(target_id) {
            inst.api_capture.tasks.extend([
                request_task,
                response_task,
                loading_failed_task,
                loading_finished_task,
            ]);
        } else {
            request_task.abort();
            response_task.abort();
            loading_failed_task.abort();
            loading_finished_task.abort();
        }

        Ok(())
    }

    // ── Screencast ────────────────────────────────────────────────────────────

    /// Start a screencast session and store the handle on the instance.
    pub async fn start_screencast(
        &self,
        session_id: &str,
        format: &str,
        quality: u8,
        every_nth: u32,
    ) -> Result<(), Error> {
        let page = self.get_page(session_id).await?;
        let handle = crate::screencast::start_screencast(&page, format, quality, every_nth).await?;

        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(session_id) {
            let mut inst = instance.lock().await;
            inst.screencast_handle = Some(handle);
        }

        Ok(())
    }

    /// Stop the active screencast for a session.
    pub async fn stop_screencast(&self, session_id: &str) -> Result<(), Error> {
        let page = self.get_page(session_id).await?;
        crate::screencast::stop_screencast(&page).await?;

        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(session_id) {
            let mut inst = instance.lock().await;
            inst.screencast_handle = None;
        }

        Ok(())
    }

    /// Retrieve the most recent screencast frame (if any) as base64.
    pub async fn get_screencast_frame(&self, session_id: &str) -> Option<String> {
        use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(session_id) {
            let mut inst = instance.lock().await;
            if let Some(ref mut handle) = inst.screencast_handle {
                // Drain the channel to get the latest frame.
                let mut latest = None;
                while let Ok(frame) = handle.frames_rx.try_recv() {
                    latest = Some(frame);
                }
                return latest.map(|f| BASE64.encode(&f.data));
            }
        }
        None
    }

    // ── Tab management ────────────────────────────────────────────────────────

    /// Get the page for the currently active tab.
    pub async fn get_active_page(&self, session_id: &str) -> Result<Page, Error> {
        let instances = self.instances.read().await;
        let instance = instances.get(session_id).ok_or(Error::ElementNotFound(0))?;

        let mut inst = instance.lock().await;
        inst.last_used = Instant::now();

        let tab = inst.active_tab.clone();

        if let Some(page) = inst.pages.get(&tab) {
            return Ok(page.clone());
        }

        // Tab name exists but page not created yet — fall back to main tab logic.
        drop(inst);
        drop(instances);
        self.get_page(session_id).await
    }

    /// Open a new browser tab named `name` and switch to it.
    pub async fn new_tab(&self, session_id: &str, name: &str) -> Result<(), Error> {
        let instances = self.instances.read().await;
        let instance = instances.get(session_id).ok_or(Error::ElementNotFound(0))?;
        let instance_arc = Arc::clone(instance);

        let mut inst = instance.lock().await;
        inst.last_used = Instant::now();

        let page = inst
            .browser
            .new_page("about:blank")
            .await
            .map_err(|e| Error::LaunchFailed(format!("new_tab failed: {e}")))?;

        inst.pages.insert(name.to_string(), page);
        inst.active_tab = name.to_string();
        let page = inst.pages.get(name).cloned().ok_or_else(|| {
            Error::LaunchFailed(format!("new_tab page '{name}' missing after creation"))
        })?;
        let attach_api_capture =
            inst.api_capture.config.is_some() && inst.api_capture.recorder.is_some();
        drop(inst);
        drop(instances);
        if attach_api_capture {
            self.attach_api_capture_to_page(instance_arc, page).await?;
        }

        Ok(())
    }

    /// List all open tab names for a session.
    pub async fn list_tabs(&self, session_id: &str) -> Vec<String> {
        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(session_id) {
            let inst = instance.lock().await;
            let mut tabs: Vec<String> = inst.pages.keys().cloned().collect();
            tabs.sort();
            return tabs;
        }
        vec![]
    }

    /// Switch the active tab to `name`.
    pub async fn switch_tab(&self, session_id: &str, name: &str) -> Result<(), Error> {
        let instances = self.instances.read().await;
        let instance = instances.get(session_id).ok_or(Error::ElementNotFound(0))?;

        let mut inst = instance.lock().await;
        if !inst.pages.contains_key(name) {
            return Err(Error::InvalidAction(format!("tab '{name}' not found")));
        }
        inst.active_tab = name.to_string();
        Ok(())
    }

    /// Close the tab named `name`.
    ///
    /// If the closed tab was the active tab, switches to `"main"`.
    pub async fn close_tab(&self, session_id: &str, name: &str) -> Result<(), Error> {
        if name == "main" {
            return Err(Error::InvalidAction(
                "cannot close the main tab".to_string(),
            ));
        }

        let instances = self.instances.read().await;
        let instance = instances.get(session_id).ok_or(Error::ElementNotFound(0))?;

        let mut inst = instance.lock().await;
        if let Some(page) = inst.pages.remove(name)
            && let Err(e) = page.close().await
        {
            warn!(tab = name, error = %e, "error closing tab");
        }
        if inst.active_tab == name {
            inst.active_tab = "main".to_string();
        }

        Ok(())
    }

    /// Close a specific browser session.
    pub async fn close_session(&self, session_id: &str) -> Result<(), Error> {
        let instance = {
            let mut instances = self.instances.write().await;
            instances.remove(session_id)
        };

        if let Some(instance) = instance {
            let mut inst = instance.lock().await;
            for task in inst.interception.tasks.drain(..) {
                task.abort();
            }
            inst.interception.paused_tx = None;
            for task in inst.api_capture.tasks.drain(..) {
                task.abort();
            }
            inst.api_capture.attached_targets.clear();
            // Pages are closed when browser is dropped
            drop(inst);

            #[cfg(feature = "metrics")]
            {
                self.active_count
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                moltis_metrics::gauge!(moltis_metrics::browser::INSTANCES_ACTIVE)
                    .set(self.active_count.load(std::sync::atomic::Ordering::Relaxed) as f64);
                moltis_metrics::counter!(moltis_metrics::browser::INSTANCES_DESTROYED_TOTAL)
                    .increment(1);
            }

            info!(session_id, "closed browser session");
        }

        Ok(())
    }

    /// Clean up idle browser instances.
    pub async fn cleanup_idle(&self) {
        let idle_timeout = Duration::from_secs(self.config.idle_timeout_secs);
        let now = Instant::now();

        let mut to_remove = Vec::new();

        {
            let instances = self.instances.read().await;
            for (sid, instance) in instances.iter() {
                if let Ok(inst) = instance.try_lock()
                    && now.duration_since(inst.last_used) > idle_timeout
                {
                    to_remove.push(sid.clone());
                }
            }
        }

        if to_remove.is_empty() {
            return;
        }

        info!(
            count = to_remove.len(),
            sessions = ?to_remove,
            "cleaning up idle browser sessions"
        );

        for sid in to_remove {
            if let Err(e) = self.close_session(&sid).await {
                warn!(session_id = sid, error = %e, "failed to close idle session");
            }
        }
    }

    /// Shut down all browser instances.
    pub async fn shutdown(&self) {
        let sessions: Vec<String> = {
            let instances = self.instances.read().await;
            instances.keys().cloned().collect()
        };

        for sid in sessions {
            let _ = self.close_session(&sid).await;
        }

        info!("browser pool shut down");
    }

    /// Get the number of active instances.
    pub async fn active_count(&self) -> usize {
        self.instances.read().await.len()
    }

    /// Launch a new browser instance.
    async fn launch_browser(
        &self,
        session_id: &str,
        sandbox: bool,
        browser: Option<BrowserPreference>,
    ) -> Result<BrowserInstance, Error> {
        if sandbox {
            self.launch_sandboxed_browser(session_id).await
        } else {
            self.launch_host_browser(session_id, browser).await
        }
    }

    /// Launch a browser inside a container (sandboxed mode).
    async fn launch_sandboxed_browser(&self, session_id: &str) -> Result<BrowserInstance, Error> {
        use crate::container;

        // All container operations (CLI checks, image pulls, container start +
        // readiness polling) use synchronous `std::process::Command` and
        // `std::thread::sleep`.  Run them on the blocking thread-pool so they
        // don't stall the tokio event loop.
        let image = self.config.sandbox_image.clone();
        let prefix = self.config.container_prefix.clone();
        let vw = self.config.viewport_width;
        let vh = self.config.viewport_height;
        let low_mem = self.config.low_memory_threshold_mb;
        let profile_dir = sandbox_profile_dir(self.config.resolved_profile_dir(), session_id);

        let container = tokio::task::spawn_blocking(move || {
            // Check container runtime availability (Docker or Apple Container)
            if !container::is_container_available() {
                return Err(Error::LaunchFailed(
                    "No container runtime available for sandboxed browser. \
                     Please install Docker or Apple Container."
                        .to_string(),
                ));
            }

            // Ensure the container image is available
            let t_image = Instant::now();
            container::ensure_image(&image)
                .map_err(|e| Error::LaunchFailed(format!("failed to ensure browser image: {e}")))?;
            info!(
                elapsed_ms = t_image.elapsed().as_millis() as u64,
                "browser container image ready"
            );

            // Create profile directory on host if needed
            if let Some(ref dir) = profile_dir
                && let Err(e) = std::fs::create_dir_all(dir)
            {
                warn!(
                    path = %dir.display(),
                    error = %e,
                    "failed to create browser profile directory for container"
                );
            }

            // Start the container (includes readiness polling)
            BrowserContainer::start(&image, &prefix, vw, vh, low_mem, profile_dir.as_deref())
                .map_err(|e| Error::LaunchFailed(format!("failed to start browser container: {e}")))
        })
        .await
        .map_err(|e| Error::LaunchFailed(format!("container launch task panicked: {e}")))??;

        let ws_url = container.websocket_url();
        info!(
            session_id,
            container_id = container.id(),
            ws_url,
            "connecting to sandboxed browser"
        );

        // Connect to the containerized browser with custom timeout
        let handler_config = HandlerConfig {
            request_timeout: Duration::from_millis(self.config.navigation_timeout_ms),
            viewport: Some(chromiumoxide::handler::viewport::Viewport {
                width: self.config.viewport_width,
                height: self.config.viewport_height,
                device_scale_factor: Some(self.config.device_scale_factor),
                emulating_mobile: false,
                is_landscape: true,
                has_touch: false,
            }),
            ..Default::default()
        };

        let (browser, mut handler) = Browser::connect_with_config(&ws_url, handler_config)
            .await
            .map_err(|e| {
                Error::LaunchFailed(format!(
                    "failed to connect to containerized browser at {}: {}",
                    ws_url, e
                ))
            })?;

        // Spawn handler to process browser events
        let session_id_clone = session_id.to_string();
        tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                debug!(session_id = session_id_clone, ?event, "browser event");
            }
            // Handler exits when connection closes - this is normal for idle sessions
            debug!(
                session_id = session_id_clone,
                "sandboxed browser event handler exited (connection closed)"
            );
        });

        info!(session_id, "sandboxed browser connected successfully");

        Ok(BrowserInstance {
            browser,
            pages: HashMap::new(),
            last_used: Instant::now(),
            sandboxed: true,
            container: Some(container),
            active_tab: "main".to_string(),
            current_mouse_pos: (0.0, 0.0),
            interception: crate::network::InterceptionState::default(),
            api_capture: crate::api_capture::ApiCaptureRuntime::default(),
            screencast_handle: None,
            virtual_display: None,
        })
    }

    /// Launch a browser on the host (non-sandboxed mode).
    async fn launch_host_browser(
        &self,
        session_id: &str,
        browser: Option<BrowserPreference>,
    ) -> Result<BrowserInstance, Error> {
        let requested_browser = browser.unwrap_or_default();

        // Detect all installed browser candidates.
        let mut detection = crate::detect::detect_browser(self.config.chrome_path.as_deref());
        let mut install_attempt: Option<crate::detect::AutoInstallResult> = None;

        // Auto-install is always on: if none are installed, try to install one.
        if detection.browsers.is_empty() {
            let result = crate::detect::auto_install_browser(requested_browser).await;
            if result.attempted && result.installed {
                info!(details = %result.details, "auto-installed browser on host");
            } else if result.attempted {
                warn!(details = %result.details, "browser auto-install failed");
            } else {
                warn!(
                    details = %result.details,
                    "browser auto-install skipped (installer unavailable)"
                );
            }
            install_attempt = Some(result);
            detection = crate::detect::detect_browser(self.config.chrome_path.as_deref());
        }

        if detection.browsers.is_empty() {
            let mut message = format!("No compatible browser found. {}", detection.install_hint);
            if let Some(attempt) = install_attempt
                && attempt.attempted
            {
                message.push_str("\n\nAuto-install attempt:\n");
                message.push_str(&attempt.details);
            }
            return Err(Error::LaunchFailed(message));
        }

        let selected =
            match crate::detect::pick_browser(&detection.browsers, Some(requested_browser)) {
                Some(browser) => browser,
                None => {
                    let installed = crate::detect::installed_browser_labels(&detection.browsers);
                    let installed_list = if installed.is_empty() {
                        "none".to_string()
                    } else {
                        installed.join(", ")
                    };
                    return Err(Error::LaunchFailed(format!(
                        "requested browser '{}' is not installed. Installed browsers: {}",
                        requested_browser, installed_list
                    )));
                },
            };

        let mut builder = CdpBrowserConfig::builder();
        let force_non_headless =
            self.config.virtual_display.enabled && self.config.virtual_display.force_non_headless;
        let effective_headless = self.config.headless && !force_non_headless;
        let virtual_display = if !effective_headless && self.config.virtual_display.enabled {
            VirtualDisplay::start(&self.config.virtual_display)?
        } else {
            None
        };

        // with_head() shows the browser window (non-headless mode)
        // By default chromiumoxide runs headless, so we only call with_head() when NOT headless
        if !effective_headless {
            builder = builder.with_head();
        }

        info!(
            session_id,
            viewport_width = self.config.viewport_width,
            viewport_height = self.config.viewport_height,
            device_scale_factor = self.config.device_scale_factor,
            headless = self.config.headless,
            effective_headless,
            virtual_display = self.config.virtual_display.enabled,
            "configuring browser viewport"
        );

        builder = builder
            .viewport(chromiumoxide::handler::viewport::Viewport {
                width: self.config.viewport_width,
                height: self.config.viewport_height,
                device_scale_factor: Some(self.config.device_scale_factor),
                emulating_mobile: false,
                is_landscape: true,
                has_touch: false,
            })
            .request_timeout(Duration::from_millis(self.config.navigation_timeout_ms));

        // User agent: only explicit config override.
        let ua = self.config.user_agent.as_deref();
        if let Some(ua) = ua {
            builder = builder.arg(format!("--user-agent={ua}"));
        }
        if let Some(ref display) = virtual_display {
            builder = builder.env("DISPLAY", display.display());
        }
        builder = builder.chrome_executable(selected.path.clone());

        let user_args =
            sanitize_user_chrome_args(&self.config.chrome_args, self.config.stealth.enabled);
        for arg in &user_args {
            builder = builder.arg(arg);
        }

        // Inject stealth Chrome launch flags
        #[cfg(feature = "stealth")]
        if self.config.stealth.enabled && self.config.stealth.stealth_args {
            for arg in crate::stealth::chrome_stealth_args() {
                builder = builder.arg(*arg);
            }
        }

        // Set persistent profile directory if configured
        if let Some(ref profile_path) = self.config.resolved_profile_dir() {
            if let Err(e) = std::fs::create_dir_all(profile_path) {
                warn!(
                    path = %profile_path.display(),
                    error = %e,
                    "failed to create browser profile directory, falling back to ephemeral"
                );
            } else {
                info!(
                    path = %profile_path.display(),
                    "using persistent browser profile"
                );
                builder = builder.user_data_dir(profile_path);
            }
        }

        // Additional security/sandbox args for headless
        builder = builder
            .arg("--disable-gpu")
            .arg("--disable-dev-shm-usage")
            .arg("--disable-software-rasterizer")
            .arg("--no-sandbox")
            .arg("--disable-setuid-sandbox");

        // Auto-inject low-memory flags on constrained systems
        if self.config.low_memory_threshold_mb > 0 {
            let mut sys = System::new();
            sys.refresh_memory();
            let total_mb = sys.total_memory() / (1024 * 1024);
            let extra = low_memory_chrome_args(total_mb, self.config.low_memory_threshold_mb);
            if !extra.is_empty() {
                info!(
                    total_mb,
                    threshold = self.config.low_memory_threshold_mb,
                    "low memory detected, adding constrained Chrome flags"
                );
                for arg in extra {
                    builder = builder.arg(*arg);
                }
            }
        }

        let config = builder
            .build()
            .map_err(|e| Error::LaunchFailed(format!("failed to build browser config: {e}")))?;

        let (browser, mut handler) = Browser::launch(config).await.map_err(|e| {
            // Include install instructions in launch failure messages
            let install_hint = crate::detect::install_instructions();
            Error::LaunchFailed(format!("browser launch failed: {e}\n\n{install_hint}"))
        })?;

        info!(
            session_id,
            browser = %selected.kind,
            path = %selected.path.display(),
            "launched host browser executable"
        );

        // Spawn handler to process browser events
        let session_id_clone = session_id.to_string();
        tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                debug!(session_id = session_id_clone, ?event, "browser event");
            }
        });

        Ok(BrowserInstance {
            browser,
            pages: HashMap::new(),
            last_used: Instant::now(),
            sandboxed: false,
            container: None,
            active_tab: "main".to_string(),
            current_mouse_pos: (0.0, 0.0),
            interception: crate::network::InterceptionState::default(),
            api_capture: crate::api_capture::ApiCaptureRuntime::default(),
            screencast_handle: None,
            virtual_display,
        })
    }
}

impl Drop for BrowserPool {
    fn drop(&mut self) {
        let instances = self.instances.get_mut();
        let count = instances.len();
        if count > 0 {
            info!(
                count,
                "browser pool dropping, stopping remaining containers"
            );
        }
    }
}

/// Generate a random session ID.
fn generate_session_id() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let id: u64 = rng.random();
    format!("browser-{:016x}", id)
}

/// Sanitize a session identifier to a filesystem-safe single path segment.
fn sanitize_session_component(session_id: &str) -> String {
    let sanitized: String = session_id
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect();

    if sanitized.is_empty() {
        return "session".to_string();
    }

    sanitized
}

/// Derive a per-session sandbox profile directory from a configured profile root.
fn sandbox_profile_dir(profile_root: Option<PathBuf>, session_id: &str) -> Option<PathBuf> {
    profile_root.map(|root| {
        root.join("sandbox")
            .join(sanitize_session_component(session_id))
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        axum::{
            Json, Router,
            extract::State,
            http::{HeaderMap, Uri},
            response::Html,
            routing::{get, post},
        },
        serde_json::json,
        std::sync::{Arc, Mutex as StdMutex},
        tokio::{
            net::TcpListener,
            task::JoinHandle,
            time::{Duration, sleep, timeout},
        },
    };

    #[test]
    fn test_generate_session_id() {
        let id1 = generate_session_id();
        let id2 = generate_session_id();
        assert_ne!(id1, id2);
        assert!(id1.starts_with("browser-"));
    }

    #[test]
    fn sanitize_session_component_replaces_unsafe_chars() {
        let sanitized = sanitize_session_component("discord:moltis:1476434288646815864");
        assert_eq!(sanitized, "discord_moltis_1476434288646815864");
    }

    #[test]
    fn sandbox_profile_dir_is_namespaced_by_session() {
        let base = PathBuf::from("/tmp/moltis-profile");
        let path = sandbox_profile_dir(Some(base), "browser-abc123");
        assert_eq!(
            path,
            Some(PathBuf::from("/tmp/moltis-profile/sandbox/browser-abc123"))
        );
    }

    #[test]
    fn sandbox_profile_dir_none_when_profile_disabled() {
        assert!(sandbox_profile_dir(None, "browser-abc123").is_none());
    }

    fn test_config() -> BrowserConfig {
        BrowserConfig {
            idle_timeout_secs: 60,
            ..BrowserConfig::default()
        }
    }

    #[tokio::test]
    async fn cleanup_idle_empty_pool_returns_early() {
        let pool = BrowserPool::new(test_config());
        // Should not panic — hits the early-return guard.
        pool.cleanup_idle().await;
        assert_eq!(pool.active_count().await, 0);
    }

    #[tokio::test]
    async fn shutdown_empty_pool_is_noop() {
        let pool = BrowserPool::new(test_config());
        pool.shutdown().await;
        assert_eq!(pool.active_count().await, 0);
    }

    #[tokio::test]
    async fn active_count_starts_at_zero() {
        let pool = BrowserPool::new(test_config());
        assert_eq!(pool.active_count().await, 0);
    }

    #[tokio::test]
    async fn close_session_missing_is_ok() {
        let pool = BrowserPool::new(test_config());
        // Closing a non-existent session should succeed (no-op).
        let result = pool.close_session("nonexistent").await;
        assert!(result.is_ok());
    }

    #[test]
    fn drop_empty_pool_does_not_panic() {
        let pool = BrowserPool::new(test_config());
        drop(pool);
    }

    #[test]
    fn low_memory_args_injected_below_threshold() {
        let args = low_memory_chrome_args(1024, 2048);
        assert_eq!(args.len(), 3);
        assert!(args.contains(&"--single-process"));
        assert!(args.contains(&"--renderer-process-limit=1"));
        assert!(args.contains(&"--js-flags=--max-old-space-size=128"));
    }

    #[test]
    fn low_memory_args_empty_at_or_above_threshold() {
        assert!(low_memory_chrome_args(2048, 2048).is_empty());
        assert!(low_memory_chrome_args(4096, 2048).is_empty());
    }

    #[test]
    fn low_memory_args_disabled_when_threshold_zero() {
        assert!(low_memory_chrome_args(512, 0).is_empty());
    }

    #[test]
    fn sanitize_user_chrome_args_removes_enable_automation_in_stealth() {
        let input = vec![
            "--enable-automation".to_string(),
            "--window-size=1280,720".to_string(),
        ];
        let result = sanitize_user_chrome_args(&input, true);
        assert_eq!(result, vec!["--window-size=1280,720".to_string()]);
    }

    #[test]
    fn sanitize_user_chrome_args_keeps_enable_automation_when_not_stealth() {
        let input = vec![
            "--enable-automation".to_string(),
            "--window-size=1280,720".to_string(),
        ];
        let result = sanitize_user_chrome_args(&input, false);
        assert_eq!(result, vec![
            "--enable-automation".to_string(),
            "--window-size=1280,720".to_string()
        ]);
    }

    #[test]
    fn sanitize_user_chrome_args_dedupes_preserving_order() {
        let input = vec![
            "--window-size=1280,720".to_string(),
            "--window-size=1280,720".to_string(),
            "--disable-gpu".to_string(),
        ];
        let result = sanitize_user_chrome_args(&input, true);
        assert_eq!(result, vec![
            "--window-size=1280,720".to_string(),
            "--disable-gpu".to_string()
        ]);
    }

    #[derive(Clone, Debug)]
    struct SeenRequest {
        path: String,
        query: String,
        auth: Option<String>,
        method: String,
    }

    #[derive(Clone, Default)]
    struct PracticalServerState {
        seen: Arc<StdMutex<Vec<SeenRequest>>>,
    }

    async fn page_one() -> Html<&'static str> {
        Html(
            r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <link rel="icon" href="data:,">
    <title>page one</title>
  </head>
  <body>
    <script>
      (async () => {
        await fetch('/api/search?q=milk&filter=fresh&filter=organic');
        await fetch('/graphql', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            operationName: 'SearchProducts',
            query: 'query SearchProducts($query: String!, $page: Int!) { search(query: $query, page: $page) { id } }',
            variables: { query: 'milk', page: 1 }
          })
        });
        document.body.dataset.done = 'true';
      })();
    </script>
  </body>
</html>"#,
        )
    }

    async fn page_two() -> Html<&'static str> {
        Html(
            r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <link rel="icon" href="data:,">
    <title>page two</title>
  </head>
  <body>
    <script>
      (async () => {
        await fetch('/api/suggest?term=bread&api_key=top-secret');
        document.body.dataset.done = 'true';
      })();
    </script>
  </body>
</html>"#,
        )
    }

    async fn record_get(
        State(state): State<PracticalServerState>,
        uri: Uri,
        headers: HeaderMap,
    ) -> Json<serde_json::Value> {
        state.seen.lock().unwrap().push(SeenRequest {
            path: uri.path().to_string(),
            query: uri.query().unwrap_or_default().to_string(),
            auth: headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            method: "GET".to_string(),
        });
        Json(json!({ "ok": true }))
    }

    async fn record_graphql(
        State(state): State<PracticalServerState>,
        uri: Uri,
        headers: HeaderMap,
        body: String,
    ) -> Json<serde_json::Value> {
        let _ = body;
        state.seen.lock().unwrap().push(SeenRequest {
            path: uri.path().to_string(),
            query: uri.query().unwrap_or_default().to_string(),
            auth: headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            method: "POST".to_string(),
        });
        Json(json!({ "data": { "search": [{ "id": 1 }] } }))
    }

    async fn start_practical_server() -> Result<
        (std::net::SocketAddr, PracticalServerState, JoinHandle<()>),
        Box<dyn std::error::Error>,
    > {
        let state = PracticalServerState::default();
        let app = Router::new()
            .route("/page1", get(page_one))
            .route("/page2", get(page_two))
            .route("/api/search", get(record_get))
            .route("/api/suggest", get(record_get))
            .route("/graphql", post(record_graphql))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .unwrap_or_else(|error| panic!("practical server should run: {error}"));
        });

        Ok((addr, state, server))
    }

    async fn wait_for_page_done(page: &Page) -> Result<(), Box<dyn std::error::Error>> {
        timeout(Duration::from_secs(10), async {
            loop {
                let result: serde_json::Value = page
                    .evaluate("document.body.dataset.done || ''")
                    .await?
                    .into_value()?;
                if result == serde_json::Value::String("true".to_string()) {
                    return Ok(()) as Result<(), Box<dyn std::error::Error>>;
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await??;

        Ok(())
    }

    fn find_endpoint<'a>(
        catalog: &'a crate::api_capture::ApiCatalog,
        path_template: &str,
    ) -> &'a crate::api_capture::ApiEndpoint {
        catalog
            .endpoints
            .iter()
            .find(|endpoint| endpoint.path_template == path_template)
            .unwrap_or_else(|| panic!("endpoint '{path_template}' should be captured"))
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires local chromium"]
    async fn api_capture_tracks_real_browser_session_usage()
    -> Result<(), Box<dyn std::error::Error>> {
        let _ = tracing_subscriber::fmt::try_init();
        let (addr, state, server) = start_practical_server().await?;
        let pool = BrowserPool::new(test_config());

        let outcome = async {
            let sid = pool
                .get_or_create(None, false, Some(BrowserPreference::Auto))
                .await?;

            pool.start_api_capture(&sid, crate::api_capture::ApiCaptureConfig::default())
                .await?;
            pool.enable_interception(&sid, vec!["*api/search*".to_string()], HashMap::new())
                .await?;
            pool.set_extra_headers(
                &sid,
                HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer secret-token".to_string(),
                )]),
            )
            .await;

            let page = pool.get_page(&sid).await?;
            page.goto(&format!("http://{addr}/page1")).await?;
            wait_for_page_done(&page).await?;

            pool.new_tab(&sid, "secondary").await?;
            let secondary = pool.get_active_page(&sid).await?;
            secondary.goto(&format!("http://{addr}/page2")).await?;
            wait_for_page_done(&secondary).await?;

            let catalog = pool
                .stop_api_capture(&sid)
                .await
                .unwrap_or_else(|| panic!("api capture should produce a catalog"));
            pool.disable_interception(&sid).await?;
            pool.close_session(&sid).await?;

            Ok::<_, Box<dyn std::error::Error>>(catalog)
        }
        .await;

        server.abort();

        let catalog = outcome?;
        let search = find_endpoint(&catalog, "/api/search");
        let graphql = find_endpoint(&catalog, "/graphql");
        let suggest = find_endpoint(&catalog, "/api/suggest");

        assert!(
            catalog.summary.captured_requests >= 3,
            "expected at least 3 API requests, got {}",
            catalog.summary.captured_requests
        );
        assert!(catalog.endpoints.iter().all(|endpoint| {
            endpoint.path_template != "/page1" && endpoint.path_template != "/page2"
        }));

        assert!(search.auth.iter().any(|auth| auth.scheme == "bearer"));
        assert!(
            search
                .query_params
                .iter()
                .any(|field| field.name == "filter" && field.required && field.repeated)
        );
        assert_eq!(
            search.examples[0].query.get("filter"),
            Some(&vec!["fresh".to_string(), "organic".to_string()])
        );

        assert_eq!(graphql.body_kind, "graphql");
        assert_eq!(graphql.operation_name.as_deref(), Some("SearchProducts"));
        assert!(graphql.auth.is_empty());

        assert!(suggest.auth.iter().any(|auth| auth.scheme == "api_key"));
        assert!(
            suggest.examples[0]
                .redacted_url
                .contains("api_key=%5BREDACTED%5D")
        );
        assert!(!suggest.examples[0].redacted_url.contains("top-secret"));

        let seen = state.seen.lock().unwrap().clone();
        assert!(seen.iter().any(|request| {
            request.path == "/api/search"
                && request.method == "GET"
                && request.auth.as_deref() == Some("Bearer secret-token")
                && request.query.contains("filter=fresh")
        }));
        assert!(seen.iter().any(|request| {
            request.path == "/graphql" && request.method == "POST" && request.auth.is_none()
        }));
        assert!(seen.iter().any(|request| {
            request.path == "/api/suggest"
                && request.method == "GET"
                && request.auth.is_none()
                && request.query.contains("api_key=top-secret")
        }));

        Ok(())
    }
}
