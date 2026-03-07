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
    time::OffsetDateTime,
    tokio::sync::{Mutex, RwLock, broadcast},
    tracing::{debug, info, warn},
    uuid::Uuid,
};

use crate::{
    container::BrowserContainer,
    error::Error,
    patchright_session::PatchrightSession,
    protection::{PatchrightLaunchProfile, build_patchright_launch_profile_for_browser},
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
/// In-flight request buffered by the API recon CDP listener task.
struct PendingReconRequest {
    method: String,
    url: String,
    tab_id: String,
    started_at: OffsetDateTime,
    request_headers: serde_json::Value,
    request_body_raw: Option<String>,
    response_status: Option<u16>,
    response_headers: Option<serde_json::Value>,
}

struct BrowserInstance {
    browser: Browser,
    pages: HashMap<String, Page>,
    last_used: Instant,
    patchright_launch_profile: PatchrightLaunchProfile,
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
    /// API reconnaissance mode.
    api_recon_mode: crate::api_recon_types::ApiReconMode,
    /// API reconnaissance store (typed schema inference).
    api_recon: crate::api_recon::ApiReconStore,
    /// Pending recon request count (in-flight CDP body fetches).
    pending_recon_count: usize,
    /// Spawned tasks for API recon CDP subscriptions (one per page).
    api_recon_tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Target IDs that already have a recon CDP subscription attached.
    api_recon_attached_targets: HashSet<String>,
}

pub(crate) struct PatchrightInstance {
    pub(crate) session: PatchrightSession,
    last_used: Instant,
    #[allow(dead_code)]
    patchright_launch_profile: PatchrightLaunchProfile,
    interception: crate::network::InterceptionState,
    api_capture: Option<PatchrightApiCaptureState>,
}

struct PatchrightApiCaptureState {
    handle: String,
    config: crate::api_capture::ApiCaptureConfig,
    recorder: crate::api_capture::ApiCaptureRecorder,
}

#[derive(Debug, Default)]
pub(crate) struct SessionTransferState {
    interception: Option<crate::network::InterceptionSnapshot>,
    api_capture: Option<crate::api_capture::ApiCaptureSnapshot>,
    session_state: Option<crate::session_state::SessionState>,
}

/// Pool of browser instances for reuse.
pub struct BrowserPool {
    config: BrowserConfig,
    instances: RwLock<HashMap<String, Arc<Mutex<BrowserInstance>>>>,
    patchright_instances: RwLock<HashMap<String, Arc<Mutex<PatchrightInstance>>>>,
    #[cfg(feature = "metrics")]
    active_count: std::sync::atomic::AtomicUsize,
}

impl BrowserPool {
    /// Create a new browser pool with the given configuration.
    pub fn new(config: BrowserConfig) -> Self {
        Self {
            config,
            instances: RwLock::new(HashMap::new()),
            patchright_instances: RwLock::new(HashMap::new()),
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
            drop(instances);

            let patchright_instances = self.patchright_instances.read().await;
            if patchright_instances.contains_key(sid) {
                debug!(session_id = sid, "reusing existing patchright session");
                return Ok(sid.to_string());
            }
        }

        self.ensure_capacity().await?;

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

    pub(crate) async fn get_or_create_patchright(
        &self,
        session_id: Option<&str>,
        browser: Option<BrowserPreference>,
    ) -> Result<String, Error> {
        let session_id = session_id.filter(|s| !s.is_empty());
        if let Some(sid) = session_id {
            let patchright_instances = self.patchright_instances.read().await;
            if patchright_instances.contains_key(sid) {
                debug!(session_id = sid, "reusing existing patchright session");
                return Ok(sid.to_string());
            }
            drop(patchright_instances);

            let instances = self.instances.read().await;
            if instances.contains_key(sid) {
                return Err(Error::InvalidAction(format!(
                    "session {sid} already exists on chromiumoxide"
                )));
            }
        }

        self.ensure_capacity().await?;

        let sid = session_id
            .map(String::from)
            .unwrap_or_else(generate_session_id);
        let selected = self.resolve_host_browser(browser).await?;
        let launch_profile =
            build_patchright_launch_profile_for_browser(&self.config, Some(&selected));
        let instance = PatchrightInstance {
            session: PatchrightSession::start(&self.config.protection, &launch_profile).await?,
            last_used: Instant::now(),
            patchright_launch_profile: launch_profile,
            interception: crate::network::InterceptionState::default(),
            api_capture: None,
        };
        let instance = Arc::new(Mutex::new(instance));

        {
            let mut patchright_instances = self.patchright_instances.write().await;
            patchright_instances.insert(sid.clone(), instance);
        }

        #[cfg(feature = "metrics")]
        {
            self.active_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            moltis_metrics::gauge!(moltis_metrics::browser::INSTANCES_ACTIVE)
                .set(self.active_count.load(std::sync::atomic::Ordering::Relaxed) as f64);
            moltis_metrics::counter!(moltis_metrics::browser::INSTANCES_CREATED_TOTAL).increment(1);
        }

        info!(
            session_id = sid,
            browser = %selected.kind,
            path = %selected.path.display(),
            "launched new patchright browser session"
        );
        Ok(sid)
    }

    async fn ensure_capacity(&self) -> Result<(), Error> {
        if self.config.max_instances > 0 {
            let instances = self.instances.read().await;
            let patchright_instances = self.patchright_instances.read().await;
            if instances.len() + patchright_instances.len() >= self.config.max_instances {
                drop(instances);
                drop(patchright_instances);
                self.cleanup_idle().await;

                let instances = self.instances.read().await;
                let patchright_instances = self.patchright_instances.read().await;
                if instances.len() + patchright_instances.len() >= self.config.max_instances {
                    return Err(Error::PoolExhausted);
                }
            }
        }

        let memory_percent = get_memory_usage_percent();
        if memory_percent >= self.config.memory_limit_percent {
            self.cleanup_idle().await;

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

        Ok(())
    }

    pub(crate) async fn resolve_host_browser(
        &self,
        browser: Option<BrowserPreference>,
    ) -> Result<crate::detect::DetectedBrowser, Error> {
        let requested_browser = browser.unwrap_or_default();
        let mut detection = crate::detect::detect_browser(self.config.chrome_path.as_deref());
        let mut install_attempt: Option<crate::detect::AutoInstallResult> = None;

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

        crate::detect::pick_browser(&detection.browsers, Some(requested_browser)).ok_or_else(|| {
            let installed = crate::detect::installed_browser_labels(&detection.browsers);
            let installed_list = if installed.is_empty() {
                "none".to_string()
            } else {
                installed.join(", ")
            };
            Error::LaunchFailed(format!(
                "requested browser '{}' is not installed. Installed browsers: {}",
                requested_browser, installed_list
            ))
        })
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
        let attach_api_recon = !matches!(
            inst.api_recon_mode,
            crate::api_recon_types::ApiReconMode::Off
        );
        drop(inst);
        drop(instances);
        if attach_api_capture {
            self.attach_api_capture_to_page(Arc::clone(&instance_arc), page.clone())
                .await?;
        }
        if attach_api_recon {
            self.attach_api_recon_to_page(instance_arc, page.clone())
                .await?;
        }
        Ok(page)
    }

    pub async fn session_uses_patchright(&self, session_id: &str) -> bool {
        let patchright_instances = self.patchright_instances.read().await;
        patchright_instances.contains_key(session_id)
    }

    pub(crate) async fn get_patchright_session(
        &self,
        session_id: &str,
    ) -> Result<Arc<Mutex<PatchrightInstance>>, Error> {
        let patchright_instances = self.patchright_instances.read().await;
        let instance = patchright_instances
            .get(session_id)
            .ok_or_else(|| Error::ElementNotFound(0))?;
        let instance = Arc::clone(instance);
        drop(patchright_instances);
        if let Ok(mut inst) = instance.try_lock() {
            inst.last_used = Instant::now();
        }
        Ok(instance)
    }

    pub(crate) async fn replace_with_patchright(&self, session_id: &str) -> Result<(), Error> {
        let launch_profile = {
            let instances = self.instances.read().await;
            let instance = instances
                .get(session_id)
                .ok_or_else(|| Error::ElementNotFound(0))?;
            let inst = instance.lock().await;
            inst.patchright_launch_profile.clone()
        };
        let staged = PatchrightInstance {
            session: PatchrightSession::start(&self.config.protection, &launch_profile).await?,
            last_used: Instant::now(),
            patchright_launch_profile: launch_profile,
            interception: crate::network::InterceptionState::default(),
            api_capture: None,
        };
        let transfer_state = self.take_transfer_state_from_chromium(session_id).await;
        self.complete_patchright_replacement(session_id, staged, transfer_state)
            .await
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
        if let Some(instance) = self
            .patchright_instances
            .read()
            .await
            .get(session_id)
            .cloned()
        {
            let mut inst = instance.lock().await;
            inst.session
                .enable_interception(patterns.clone(), extra_headers.clone())
                .await?;
            inst.interception.enabled = true;
            inst.interception.url_patterns = patterns;
            inst.interception.extra_headers = extra_headers;
            inst.interception.paused_tx = None;
            inst.interception.tasks.clear();
            return Ok(());
        }

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
        if let Some(instance) = self
            .patchright_instances
            .read()
            .await
            .get(session_id)
            .cloned()
        {
            let mut inst = instance.lock().await;
            inst.session.disable_interception().await?;
            inst.interception.paused_tx = None;
            inst.interception.enabled = false;
            inst.interception.url_patterns.clear();
            inst.interception.extra_headers.clear();
            inst.interception.tasks.clear();
            return Ok(());
        }

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
    ) -> Result<String, Error> {
        let handle = new_api_capture_handle();
        self.configure_api_capture(
            session_id,
            handle.clone(),
            config.clone(),
            crate::api_capture::ApiCaptureRecorder::new(config),
        )
        .await?;
        Ok(handle)
    }

    /// Stop passive API capture and return the inferred API catalog.
    pub async fn stop_api_capture(
        &self,
        session_id: &str,
    ) -> Option<crate::api_capture::ApiCatalog> {
        self.stop_api_capture_with_handle(session_id)
            .await
            .ok()
            .flatten()
            .map(|(_, catalog)| catalog)
    }

    /// Stop passive API capture and return the catalog handle plus inferred API catalog.
    pub async fn stop_api_capture_with_handle(
        &self,
        session_id: &str,
    ) -> Result<Option<(String, crate::api_capture::ApiCatalog)>, Error> {
        if let Some(instance) = self
            .patchright_instances
            .read()
            .await
            .get(session_id)
            .cloned()
        {
            let mut inst = instance.lock().await;
            let Some(state) = inst.api_capture.take() else {
                return Ok(None);
            };
            let mut recorder = state.recorder;
            let records = inst.session.stop_api_capture().await?;
            recorder.append_records(records);
            recorder.finish();
            return Ok(Some((state.handle, recorder.build_catalog())));
        }

        let instances = self.instances.read().await;
        let Some(instance) = instances.get(session_id) else {
            return Ok(None);
        };
        let mut inst = instance.lock().await;

        for task in inst.api_capture.tasks.drain(..) {
            task.abort();
        }
        let Some(handle) = inst.api_capture.handle.take() else {
            return Ok(None);
        };
        inst.api_capture.config = None;
        inst.api_capture.attached_targets.clear();

        let Some(mut recorder) = inst.api_capture.recorder.take() else {
            return Ok(None);
        };
        recorder.finish();

        #[cfg(feature = "metrics")]
        moltis_metrics::counter!(moltis_metrics::browser::API_CAPTURES_TOTAL).increment(1);

        Ok(Some((handle, recorder.build_catalog())))
    }

    // ── API Reconnaissance delegate methods ───────────────────────────────

    pub async fn api_recon_status(
        &self,
        session_id: &str,
    ) -> Result<crate::api_recon_types::ApiReconStatus, Error> {
        let instances = self.instances.read().await;
        let instance = instances
            .get(session_id)
            .ok_or_else(|| Error::InvalidAction(format!("session not found: {session_id}")))?;
        let inst = instance.lock().await;
        Ok(inst
            .api_recon
            .status(inst.api_recon_mode, inst.pending_recon_count))
    }

    pub async fn set_api_recon_mode(
        &self,
        session_id: &str,
        mode: crate::api_recon_types::ApiReconMode,
        reset: bool,
    ) -> Result<(), Error> {
        let instances = self.instances.read().await;
        let instance = instances
            .get(session_id)
            .ok_or_else(|| Error::InvalidAction(format!("session not found: {session_id}")))?;
        let mut inst = instance.lock().await;
        inst.api_recon_mode = mode;
        if reset {
            inst.api_recon.clear();
        }
        Ok(())
    }

    pub async fn api_recon_mark(
        &self,
        session_id: &str,
        label: Option<String>,
    ) -> Result<crate::api_recon_types::ApiObservationMarker, Error> {
        let instances = self.instances.read().await;
        let instance = instances
            .get(session_id)
            .ok_or_else(|| Error::InvalidAction(format!("session not found: {session_id}")))?;
        let mut inst = instance.lock().await;
        let tab_id = inst.active_tab.clone();
        Ok(inst.api_recon.mark(&tab_id, label))
    }

    pub async fn api_recon_wait_for_idle(
        &self,
        session_id: &str,
        _since: Option<&str>,
        quiet_ms: u64,
        timeout_ms: u64,
    ) -> Result<(), Error> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let _quiet = Duration::from_millis(quiet_ms);
        loop {
            {
                let instances = self.instances.read().await;
                let instance = instances.get(session_id).ok_or_else(|| {
                    Error::InvalidAction(format!("session not found: {session_id}"))
                })?;
                let inst = instance.lock().await;
                if inst.pending_recon_count == 0 {
                    if let Some(last) = inst.api_recon.last_network_activity_at() {
                        let elapsed = OffsetDateTime::now_utc() - last;
                        if elapsed >= time::Duration::milliseconds(quiet_ms as i64) {
                            return Ok(());
                        }
                    } else {
                        return Ok(());
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout(format!(
                    "api_recon wait_for_idle timed out after {timeout_ms}ms"
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn api_recon_diff(
        &self,
        session_id: &str,
        marker_id: &str,
    ) -> Result<crate::api_recon_types::ApiObservationDelta, Error> {
        let instances = self.instances.read().await;
        let instance = instances
            .get(session_id)
            .ok_or_else(|| Error::InvalidAction(format!("session not found: {session_id}")))?;
        let inst = instance.lock().await;
        inst.api_recon
            .diff(marker_id)
            .ok_or_else(|| Error::InvalidAction(format!("unknown marker: {marker_id}")))
    }

    pub async fn api_recon_list_endpoints(
        &self,
        session_id: &str,
        since: Option<&str>,
        limit: u32,
    ) -> Result<crate::api_recon_types::ApiEndpointList, Error> {
        let instances = self.instances.read().await;
        let instance = instances
            .get(session_id)
            .ok_or_else(|| Error::InvalidAction(format!("session not found: {session_id}")))?;
        let inst = instance.lock().await;
        Ok(inst.api_recon.list_endpoints(since, limit, true))
    }

    pub async fn api_recon_endpoint_contract(
        &self,
        session_id: &str,
        endpoint_id: &str,
    ) -> Result<crate::api_recon_types::ApiEndpointContract, Error> {
        let instances = self.instances.read().await;
        let instance = instances
            .get(session_id)
            .ok_or_else(|| Error::InvalidAction(format!("session not found: {session_id}")))?;
        let inst = instance.lock().await;
        inst.api_recon
            .endpoint_contract(endpoint_id)
            .ok_or_else(|| Error::InvalidAction(format!("unknown endpoint: {endpoint_id}")))
    }

    pub async fn api_recon_endpoint_template(
        &self,
        session_id: &str,
        endpoint_id: &str,
    ) -> Result<Option<crate::api_recon_types::ApiRequestTemplate>, Error> {
        let instances = self.instances.read().await;
        let instance = instances
            .get(session_id)
            .ok_or_else(|| Error::InvalidAction(format!("session not found: {session_id}")))?;
        let inst = instance.lock().await;
        Ok(inst.api_recon.endpoint_template(endpoint_id))
    }

    pub async fn api_recon_endpoint_summary(
        &self,
        session_id: &str,
        endpoint_id: &str,
    ) -> Result<Option<crate::api_recon_types::ApiEndpointSummary>, Error> {
        let instances = self.instances.read().await;
        let instance = instances
            .get(session_id)
            .ok_or_else(|| Error::InvalidAction(format!("session not found: {session_id}")))?;
        let inst = instance.lock().await;
        Ok(inst.api_recon.endpoint_summary(endpoint_id))
    }

    /// Update extra headers for a session's interception state.
    pub async fn set_extra_headers(
        &self,
        session_id: &str,
        headers: HashMap<String, String>,
    ) -> Result<(), Error> {
        if let Some(instance) = self
            .patchright_instances
            .read()
            .await
            .get(session_id)
            .cloned()
        {
            let mut inst = instance.lock().await;
            inst.session.set_extra_headers(headers.clone()).await?;
            inst.interception.extra_headers = headers;
            return Ok(());
        }

        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(session_id) {
            let mut inst = instance.lock().await;
            inst.interception.extra_headers = headers;
        }
        Ok(())
    }

    pub(crate) async fn take_transfer_state_from_chromium(
        &self,
        session_id: &str,
    ) -> SessionTransferState {
        SessionTransferState {
            interception: self.take_interception_snapshot(session_id).await,
            api_capture: self.take_api_capture_snapshot(session_id).await,
            session_state: self.capture_transfer_session_state(session_id).await,
        }
    }

    pub(crate) async fn restore_transfer_state_to_chromium(
        &self,
        session_id: &str,
        transfer_state: SessionTransferState,
    ) -> Result<(), Error> {
        let SessionTransferState {
            interception,
            api_capture,
            session_state,
        } = transfer_state;
        self.restore_runtime_transfer_state_to_chromium(session_id, interception, api_capture)
            .await?;
        if let Some(state) = session_state {
            let page = self.get_active_page(session_id).await?;
            crate::session_state::restore_state(&page, &state).await?;
        }
        Ok(())
    }

    pub(crate) async fn prepare_transfer_state_for_chromium_retry(
        &self,
        session_id: &str,
        transfer_state: SessionTransferState,
    ) -> Result<Option<crate::session_state::SessionState>, Error> {
        let SessionTransferState {
            interception,
            api_capture,
            session_state,
        } = transfer_state;
        self.restore_runtime_transfer_state_to_chromium(session_id, interception, api_capture)
            .await?;
        if let Some(state) = session_state.as_ref() {
            let page = self.get_active_page(session_id).await?;
            crate::session_state::restore_cookies(&page, state).await?;
        }
        Ok(session_state)
    }

    async fn restore_runtime_transfer_state_to_chromium(
        &self,
        session_id: &str,
        interception: Option<crate::network::InterceptionSnapshot>,
        api_capture: Option<crate::api_capture::ApiCaptureSnapshot>,
    ) -> Result<(), Error> {
        if let Some(snapshot) = interception {
            self.restore_interception_snapshot(session_id, snapshot)
                .await?;
        }
        if let Some(snapshot) = api_capture {
            self.restore_api_capture_snapshot(session_id, snapshot)
                .await?;
        }
        Ok(())
    }

    async fn capture_transfer_session_state(
        &self,
        session_id: &str,
    ) -> Option<crate::session_state::SessionState> {
        let page = self.get_active_page(session_id).await.ok()?;
        match crate::session_state::capture_state(&page).await {
            Ok(state) => Some(state),
            Err(error) => {
                warn!(
                    session_id,
                    error = %error,
                    "failed to capture browser session state for backend handoff"
                );
                None
            },
        }
    }

    async fn apply_transfer_state_to_patchright(
        &self,
        instance: &mut PatchrightInstance,
        transfer_state: SessionTransferState,
    ) -> Result<(), (Error, SessionTransferState)> {
        let SessionTransferState {
            interception,
            api_capture,
            session_state,
        } = transfer_state;
        if let Some(snapshot) = session_state.clone() {
            match instance.session.restore_state(&snapshot).await {
                Ok(()) => {},
                Err(error) => {
                    return Err((
                        error,
                        SessionTransferState {
                            interception,
                            api_capture,
                            session_state: Some(snapshot),
                        },
                    ));
                },
            }
        }

        if let Some(snapshot) = interception {
            let crate::network::InterceptionSnapshot {
                enabled,
                url_patterns,
                extra_headers,
            } = snapshot;
            if enabled
                && let Err(error) = instance
                    .session
                    .enable_interception(url_patterns.clone(), extra_headers.clone())
                    .await
            {
                return Err((
                    error,
                    SessionTransferState {
                        interception: Some(crate::network::InterceptionSnapshot {
                            enabled,
                            url_patterns,
                            extra_headers,
                        }),
                        api_capture,
                        session_state: None,
                    },
                ));
            }
            instance.interception.enabled = enabled;
            instance.interception.url_patterns = url_patterns;
            instance.interception.extra_headers = extra_headers;
            instance.interception.paused_tx = None;
            instance.interception.tasks.clear();
        }

        if let Some(snapshot) = api_capture {
            if let Err(error) = instance.session.start_api_capture(&snapshot.config).await {
                return Err((
                    error,
                    SessionTransferState {
                        interception: None,
                        api_capture: Some(snapshot),
                        session_state: None,
                    },
                ));
            }
            instance.api_capture = Some(PatchrightApiCaptureState {
                handle: snapshot.handle,
                config: snapshot.config,
                recorder: snapshot.recorder,
            });
        }

        Ok(())
    }

    async fn complete_patchright_replacement(
        &self,
        session_id: &str,
        mut staged: PatchrightInstance,
        transfer_state: SessionTransferState,
    ) -> Result<(), Error> {
        if let Err((error, rollback_state)) = self
            .apply_transfer_state_to_patchright(&mut staged, transfer_state)
            .await
        {
            let rollback_result = self
                .restore_transfer_state_to_chromium(session_id, rollback_state)
                .await;
            let _ = staged.session.close().await;
            return match rollback_result {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(Error::InvalidAction(format!(
                    "patchright replacement failed ({error}) and rollback to chromium failed ({rollback_error})"
                ))),
            };
        }

        let staged = Arc::new(Mutex::new(staged));
        let original = {
            let mut instances = self.instances.write().await;
            let original = instances
                .remove(session_id)
                .ok_or_else(|| Error::ElementNotFound(0))?;
            let mut patchright_instances = self.patchright_instances.write().await;
            patchright_instances.insert(session_id.to_string(), staged);
            original
        };
        drop(original);
        Ok(())
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
        if let Some(instance) = self
            .patchright_instances
            .read()
            .await
            .get(session_id)
            .cloned()
        {
            let mut inst = instance.lock().await;
            let state = inst.api_capture.take()?;
            return Some(crate::api_capture::ApiCaptureSnapshot {
                handle: state.handle,
                config: state.config,
                recorder: state.recorder,
            });
        }

        let instances = self.instances.read().await;
        let instance = instances.get(session_id)?;
        let mut inst = instance.lock().await;

        if inst.api_capture.handle.is_none()
            || inst.api_capture.config.is_none()
            || inst.api_capture.recorder.is_none()
        {
            return None;
        }

        for task in inst.api_capture.tasks.drain(..) {
            task.abort();
        }
        inst.api_capture.attached_targets.clear();

        let handle = inst.api_capture.handle.take()?;
        let config = inst.api_capture.config.take()?;
        let recorder = inst.api_capture.recorder.take()?;

        Some(crate::api_capture::ApiCaptureSnapshot {
            handle,
            config,
            recorder,
        })
    }

    /// Restore API capture state onto an already-created session.
    pub async fn restore_api_capture_snapshot(
        &self,
        session_id: &str,
        snapshot: crate::api_capture::ApiCaptureSnapshot,
    ) -> Result<(), Error> {
        self.configure_api_capture(
            session_id,
            snapshot.handle,
            snapshot.config,
            snapshot.recorder,
        )
        .await
    }

    async fn configure_api_capture(
        &self,
        session_id: &str,
        handle: String,
        config: crate::api_capture::ApiCaptureConfig,
        recorder: crate::api_capture::ApiCaptureRecorder,
    ) -> Result<(), Error> {
        if let Some(instance) = self
            .patchright_instances
            .read()
            .await
            .get(session_id)
            .cloned()
        {
            let mut inst = instance.lock().await;
            inst.session.start_api_capture(&config).await?;
            inst.api_capture = Some(PatchrightApiCaptureState {
                handle,
                config,
                recorder,
            });
            return Ok(());
        }

        let _ = self.get_page(session_id).await?;
        let instances = self.instances.read().await;
        if let Some(instance) = instances.get(session_id) {
            let instance_arc = Arc::clone(instance);

            let mut inst = instance.lock().await;
            for task in inst.api_capture.tasks.drain(..) {
                task.abort();
            }
            inst.api_capture.handle = Some(handle);
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
                    recorder.record_request(&event, fallback_request_body).await;
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

    // ── API Reconnaissance CDP subscription ───────────────────────────────────

    async fn attach_api_recon_to_page(
        &self,
        instance: Arc<Mutex<BrowserInstance>>,
        page: Page,
    ) -> Result<(), Error> {
        use chromiumoxide::cdp::browser_protocol::network::{
            EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent,
            EventResponseReceived, GetResponseBodyParams,
        };

        let target_id = page.target_id().as_ref().to_string();
        {
            let inst = instance.lock().await;
            if matches!(
                inst.api_recon_mode,
                crate::api_recon_types::ApiReconMode::Off
            ) || inst.api_recon_attached_targets.contains(&target_id)
            {
                return Ok(());
            }
        }

        let request_stream = page
            .event_listener::<EventRequestWillBeSent>()
            .await
            .map_err(|e| Error::Cdp(format!("recon requestWillBeSent listener: {e}")))?;
        let response_stream = page
            .event_listener::<EventResponseReceived>()
            .await
            .map_err(|e| Error::Cdp(format!("recon responseReceived listener: {e}")))?;
        let failed_stream = page
            .event_listener::<EventLoadingFailed>()
            .await
            .map_err(|e| Error::Cdp(format!("recon loadingFailed listener: {e}")))?;
        let finished_stream = page
            .event_listener::<EventLoadingFinished>()
            .await
            .map_err(|e| Error::Cdp(format!("recon loadingFinished listener: {e}")))?;

        let tab_id = target_id.clone();
        let instance_for_task = Arc::clone(&instance);
        let task = tokio::spawn(async move {
            let instance = instance_for_task;
            let mut request_stream = request_stream;
            let mut response_stream = response_stream;
            let mut failed_stream = failed_stream;
            let mut finished_stream = finished_stream;
            let mut pending: HashMap<String, PendingReconRequest> = HashMap::new();

            loop {
                tokio::select! {
                    event = request_stream.next() => {
                        let Some(event) = event else { break; };
                        let resource_type = event.r#type.as_ref().map(|k| k.as_ref()).unwrap_or("");
                        if !crate::api_recon::should_capture(resource_type, &event.request.url) {
                            continue;
                        }
                        {
                            let mut inst = instance.lock().await;
                            if matches!(inst.api_recon_mode, crate::api_recon_types::ApiReconMode::Off) {
                                continue;
                            }
                            if pending.len() >= crate::api_recon::MAX_PENDING_REQUESTS {
                                inst.api_recon.note_pending_drop();
                                continue;
                            }
                        }

                        // Collect request body from inline post_data_entries if available.
                        let body_raw: Option<String> = event
                            .request
                            .post_data_entries
                            .as_deref()
                            .and_then(|entries| {
                                entries.first().and_then(|entry| {
                                    entry.bytes.as_ref().map(|b| {
                                        let s: &str = b.as_ref();
                                        s.to_string()
                                    })
                                })
                            })
                            .filter(|s| !s.is_empty());

                        let req_id = event.request_id.inner().to_string();
                        pending.insert(req_id, PendingReconRequest {
                            method: event.request.method.clone(),
                            url: event.request.url.clone(),
                            tab_id: tab_id.clone(),
                            started_at: OffsetDateTime::now_utc(),
                            request_headers: event.request.headers.inner().clone(),
                            request_body_raw: body_raw,
                            response_status: None,
                            response_headers: None,
                        });
                        {
                            let mut inst = instance.lock().await;
                            inst.pending_recon_count =
                                inst.pending_recon_count.saturating_add(1);
                        }
                    }
                    event = response_stream.next() => {
                        let Some(event) = event else { break; };
                        let req_id = event.request_id.inner().to_string();
                        if let Some(req) = pending.get_mut(&req_id) {
                            req.response_status = Some(event.response.status as u16);
                            req.response_headers = Some(event.response.headers.inner().clone());
                        }
                    }
                    event = failed_stream.next() => {
                        let Some(event) = event else { break; };
                        let req_id = event.request_id.inner().to_string();
                        if pending.remove(&req_id).is_some() {
                            let mut inst = instance.lock().await;
                            inst.pending_recon_count =
                                inst.pending_recon_count.saturating_sub(1);
                            inst.api_recon.note_pending_drop();
                        }
                    }
                    event = finished_stream.next() => {
                        let Some(event) = event else { break; };
                        let req_id = event.request_id.inner().to_string();
                        let Some(req) = pending.remove(&req_id) else { continue; };

                        // Fetch response body out-of-band.
                        let body_text = match page
                            .execute(GetResponseBodyParams::new(event.request_id.clone()))
                            .await
                        {
                            Ok(resp) => {
                                if resp.result.base64_encoded {
                                    // Binary body — skip schema inference.
                                    None
                                } else {
                                    Some(resp.result.body)
                                }
                            },
                            Err(error) => {
                                debug!(request_id = %req_id, ?error, "recon getResponseBody failed");
                                None
                            },
                        };

                        // Infer schemas.
                        let req_headers_map = req.request_headers.as_object().cloned().unwrap_or_default();
                        let req_ct = crate::api_recon::infer_content_type(&req_headers_map);
                        let req_inference = crate::api_recon_inference::infer_contract_from_body(
                            req.request_body_raw.as_deref(),
                            req_ct.as_deref(),
                        );
                        let resp_headers_map = req
                            .response_headers
                            .as_ref()
                            .and_then(|v| v.as_object().cloned())
                            .unwrap_or_default();
                        let resp_ct = crate::api_recon::infer_content_type(&resp_headers_map);
                        let resp_inference = crate::api_recon_inference::infer_contract_from_body(
                            body_text.as_deref(),
                            resp_ct.as_deref(),
                        );

                        let finished_at = OffsetDateTime::now_utc();
                        let query_keys = crate::api_recon::query_keys_from_url(&req.url);
                        let header_keys =
                            crate::api_recon::header_keys_from_json(&req.request_headers);
                        let header_values: std::collections::BTreeMap<String, String> =
                            req_headers_map
                                .iter()
                                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                                .collect();
                        let operation_name =
                            crate::api_recon::operation_name_from_request_body(
                                req.request_body_raw.as_deref(),
                            );
                        let body_value: Option<serde_json::Value> =
                            req.request_body_raw.as_deref().and_then(|s| {
                                serde_json::from_str(s).ok()
                            });
                        let request_input = crate::api_recon_types::ApiObservedRequestInput {
                            method: req.method,
                            url: req.url,
                            tab_id: req.tab_id,
                            marker_id: None,
                            started_at: req.started_at,
                            query_keys,
                            header_keys,
                            header_values,
                            content_type: req_ct,
                            operation_name,
                            body_schema: req_inference.contract,
                            body_value,
                        };
                        let response_input = crate::api_recon_types::ApiObservedResponseInput {
                            status: req.response_status,
                            header_keys: crate::api_recon::header_keys_from_json(
                                req.response_headers.as_ref().unwrap_or(&serde_json::Value::Null),
                            ),
                            content_type: resp_ct,
                            body_schema: resp_inference.contract,
                            finished_at,
                        };

                        let mut inst = instance.lock().await;
                        inst.pending_recon_count =
                            inst.pending_recon_count.saturating_sub(1);
                        inst.api_recon.note_network_activity(finished_at);
                        if !matches!(inst.api_recon_mode, crate::api_recon_types::ApiReconMode::Off) {
                            inst.api_recon.record(request_input, response_input);
                        }
                    }
                }
            }
            debug!("api recon CDP event streams closed for target {tab_id}");
        });

        let mut inst = instance.lock().await;
        if inst.api_recon_attached_targets.insert(target_id) {
            inst.api_recon_tasks.push(task);
        } else {
            task.abort();
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
        let attach_api_recon = !matches!(
            inst.api_recon_mode,
            crate::api_recon_types::ApiReconMode::Off
        );
        drop(inst);
        drop(instances);
        if attach_api_capture {
            self.attach_api_capture_to_page(Arc::clone(&instance_arc), page.clone())
                .await?;
        }
        if attach_api_recon {
            self.attach_api_recon_to_page(instance_arc, page).await?;
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
            return Ok(());
        }

        let instance = {
            let mut patchright_instances = self.patchright_instances.write().await;
            patchright_instances.remove(session_id)
        };

        if let Some(instance) = instance {
            let mut inst = instance.lock().await;
            let _ = inst.session.close().await;

            #[cfg(feature = "metrics")]
            {
                self.active_count
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                moltis_metrics::gauge!(moltis_metrics::browser::INSTANCES_ACTIVE)
                    .set(self.active_count.load(std::sync::atomic::Ordering::Relaxed) as f64);
                moltis_metrics::counter!(moltis_metrics::browser::INSTANCES_DESTROYED_TOTAL)
                    .increment(1);
            }

            info!(session_id, "closed patchright browser session");
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

        {
            let patchright_instances = self.patchright_instances.read().await;
            for (sid, instance) in patchright_instances.iter() {
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
        let mut sessions: Vec<String> = {
            let instances = self.instances.read().await;
            instances.keys().cloned().collect()
        };
        let patchright_sessions: Vec<String> = {
            let instances = self.patchright_instances.read().await;
            instances.keys().cloned().collect()
        };
        sessions.extend(patchright_sessions);

        for sid in sessions {
            let _ = self.close_session(&sid).await;
        }

        info!("browser pool shut down");
    }

    /// Get the number of active instances.
    pub async fn active_count(&self) -> usize {
        self.instances.read().await.len() + self.patchright_instances.read().await.len()
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
            patchright_launch_profile: build_patchright_launch_profile_for_browser(
                &self.config,
                None,
            ),
            sandboxed: true,
            container: Some(container),
            active_tab: "main".to_string(),
            current_mouse_pos: (0.0, 0.0),
            interception: crate::network::InterceptionState::default(),
            api_capture: crate::api_capture::ApiCaptureRuntime::default(),
            screencast_handle: None,
            virtual_display: None,
            api_recon_mode: crate::api_recon_types::ApiReconMode::default(),
            api_recon: crate::api_recon::ApiReconStore::default(),
            pending_recon_count: 0,
            api_recon_tasks: Vec::new(),
            api_recon_attached_targets: HashSet::new(),
        })
    }

    /// Launch a browser on the host (non-sandboxed mode).
    async fn launch_host_browser(
        &self,
        session_id: &str,
        browser: Option<BrowserPreference>,
    ) -> Result<BrowserInstance, Error> {
        let selected = self.resolve_host_browser(browser).await?;

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
            patchright_launch_profile: build_patchright_launch_profile_for_browser(
                &self.config,
                Some(&selected),
            ),
            sandboxed: false,
            container: None,
            active_tab: "main".to_string(),
            current_mouse_pos: (0.0, 0.0),
            interception: crate::network::InterceptionState::default(),
            api_capture: crate::api_capture::ApiCaptureRuntime::default(),
            screencast_handle: None,
            virtual_display,
            api_recon_mode: crate::api_recon_types::ApiReconMode::default(),
            api_recon: crate::api_recon::ApiReconStore::default(),
            pending_recon_count: 0,
            api_recon_tasks: Vec::new(),
            api_recon_attached_targets: HashSet::new(),
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

fn new_api_capture_handle() -> String {
    format!("api-catalog-{}", Uuid::new_v4().simple())
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
            http::{
                HeaderMap, StatusCode, Uri,
                header::{
                    ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS,
                    ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
                    ACCESS_CONTROL_MAX_AGE, CACHE_CONTROL, CONTENT_TYPE, LOCATION, SET_COOKIE,
                },
            },
            response::{Html, IntoResponse},
            routing::{get, options, post},
        },
        serde_json::{Value, json},
        std::{
            net::SocketAddr,
            sync::{Arc, Mutex as StdMutex, OnceLock},
        },
        tokio::{
            net::TcpListener,
            sync::{Mutex, OwnedMutexGuard},
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

    fn live_test_pool() -> (BrowserPool, tempfile::TempDir) {
        let profile_dir =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir should work: {error}"));
        let config = BrowserConfig {
            idle_timeout_secs: 60,
            persist_profile: false,
            profile_dir: Some(profile_dir.path().display().to_string()),
            ..BrowserConfig::default()
        };
        (BrowserPool::new(config), profile_dir)
    }

    #[tokio::test]
    async fn cleanup_idle_empty_pool_returns_early() {
        let (pool, _profile_dir) = live_test_pool();
        // Should not panic — hits the early-return guard.
        pool.cleanup_idle().await;
        assert_eq!(pool.active_count().await, 0);
    }

    #[tokio::test]
    async fn shutdown_empty_pool_is_noop() {
        let (pool, _profile_dir) = live_test_pool();
        pool.shutdown().await;
        assert_eq!(pool.active_count().await, 0);
    }

    #[tokio::test]
    async fn active_count_starts_at_zero() {
        let (pool, _profile_dir) = live_test_pool();
        assert_eq!(pool.active_count().await, 0);
    }

    #[tokio::test]
    async fn close_session_missing_is_ok() {
        let (pool, _profile_dir) = live_test_pool();
        // Closing a non-existent session should succeed (no-op).
        let result = pool.close_session("nonexistent").await;
        assert!(result.is_ok());
    }

    #[test]
    fn drop_empty_pool_does_not_panic() {
        let (pool, _profile_dir) = live_test_pool();
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
        assert_eq!(
            result,
            vec![
                "--enable-automation".to_string(),
                "--window-size=1280,720".to_string()
            ]
        );
    }

    #[test]
    fn sanitize_user_chrome_args_dedupes_preserving_order() {
        let input = vec![
            "--window-size=1280,720".to_string(),
            "--window-size=1280,720".to_string(),
            "--disable-gpu".to_string(),
        ];
        let result = sanitize_user_chrome_args(&input, true);
        assert_eq!(
            result,
            vec![
                "--window-size=1280,720".to_string(),
                "--disable-gpu".to_string()
            ]
        );
    }

    #[derive(Clone, Debug)]
    struct SeenRequest {
        path: String,
        query: String,
        auth: Option<String>,
        cookie: Option<String>,
        content_type: Option<String>,
        method: String,
        body: String,
    }

    #[derive(Clone, Default)]
    struct LiveServerState {
        seen: Arc<StdMutex<Vec<SeenRequest>>>,
    }

    #[derive(Clone)]
    struct AppServerState {
        api_origin: String,
        shared: LiveServerState,
    }

    #[derive(Clone)]
    struct ApiServerState {
        app_origin: String,
        shared: LiveServerState,
    }

    struct LiveServers {
        app_addr: SocketAddr,
        api_addr: SocketAddr,
        shared: LiveServerState,
        app_server: JoinHandle<()>,
        api_server: JoinHandle<()>,
    }

    impl LiveServers {
        fn app_origin(&self) -> String {
            format!("http://{}", self.app_addr)
        }

        fn api_origin(&self) -> String {
            format!("http://{}", self.api_addr)
        }

        fn abort(self) {
            self.app_server.abort();
            self.api_server.abort();
        }
    }

    fn record_seen_request(
        state: &LiveServerState,
        method: &str,
        uri: &Uri,
        headers: &HeaderMap,
        body: impl Into<String>,
    ) {
        state.seen.lock().unwrap().push(SeenRequest {
            path: uri.path().to_string(),
            query: uri.query().unwrap_or_default().to_string(),
            auth: headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            cookie: headers
                .get("cookie")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            content_type: headers
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            method: method.to_string(),
            body: body.into(),
        });
    }

    fn scripted_page(script: &str) -> Html<String> {
        let mut html = String::from(
            r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <link rel="icon" href="data:,">
  </head>
  <body>
    <script>
"#,
        );
        html.push_str(script);
        html.push_str(
            r#"
    </script>
  </body>
</html>"#,
        );
        Html(html)
    }

    fn live_browser_test_lock() -> Arc<Mutex<()>> {
        static LOCK: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
        Arc::clone(LOCK.get_or_init(|| Arc::new(Mutex::new(()))))
    }

    async fn acquire_live_browser_test_guard() -> OwnedMutexGuard<()> {
        live_browser_test_lock().lock_owned().await
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
        State(state): State<LiveServerState>,
        uri: Uri,
        headers: HeaderMap,
    ) -> Json<Value> {
        record_seen_request(&state, "GET", &uri, &headers, "");
        Json(json!({ "ok": true }))
    }

    async fn record_graphql(
        State(state): State<LiveServerState>,
        uri: Uri,
        headers: HeaderMap,
        body: String,
    ) -> Json<Value> {
        record_seen_request(&state, "POST", &uri, &headers, body);
        Json(json!({ "data": { "search": [{ "id": 1 }] } }))
    }

    async fn start_practical_server()
    -> Result<(SocketAddr, LiveServerState, JoinHandle<()>), Box<dyn std::error::Error>> {
        let state = LiveServerState::default();
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

    async fn remote_httpbin_page() -> Html<String> {
        scripted_page(
            r#"
      (async () => {
        try {
          const getResponse = await fetch('https://httpbin.org/anything?term=milk&filter=fresh&filter=organic');
          const getJson = await getResponse.json();
          const postResponse = await fetch('https://httpbin.org/post', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ message: 'hello', count: 2 })
          });
          const postJson = await postResponse.json();
          window.__captureResults = {
            get: { args: getJson.args, url: getJson.url },
            post: { json: postJson.json, url: postJson.url }
          };
        } catch (error) {
          window.__captureError = String(error && error.message ? error.message : error);
        } finally {
          document.body.dataset.done = 'true';
        }
      })();
"#,
        )
    }

    async fn remote_graphql_page() -> Html<String> {
        scripted_page(
            r#"
      (async () => {
        try {
          const payload = {
            operationName: 'CountryByCode',
            query: 'query CountryByCode($code: ID!) { country(code: $code) { code name } }',
            variables: { code: 'US' }
          };
          const response = await fetch('https://countries.trevorblades.com/', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(payload)
          });
          window.__captureResults = await response.json();
        } catch (error) {
          window.__captureError = String(error && error.message ? error.message : error);
        } finally {
          document.body.dataset.done = 'true';
        }
      })();
"#,
        )
    }

    async fn cors_preflight_page(State(state): State<AppServerState>) -> Html<String> {
        scripted_page(&format!(
            r#"
      (async () => {{
        try {{
          const response = await fetch('{api_origin}/cors/preflight', {{
            method: 'POST',
            headers: {{
              'Content-Type': 'application/json',
              'X-Test-Mode': 'cors-live'
            }},
            body: JSON.stringify({{ term: 'milk', page: 2 }})
          }});
          window.__captureResults = await response.json();
        }} catch (error) {{
          window.__captureError = String(error && error.message ? error.message : error);
        }} finally {{
          document.body.dataset.done = 'true';
        }}
      }})();
"#,
            api_origin = state.api_origin,
        ))
    }

    async fn multipart_upload_page(State(state): State<AppServerState>) -> Html<String> {
        scripted_page(&format!(
            r#"
      (async () => {{
        try {{
          const formData = new FormData();
          formData.append('metadata', 'alpha');
          formData.append(
            'file',
            new Blob(['hello upload'], {{ type: 'text/plain' }}),
            'note.txt'
          );
          const response = await fetch('{api_origin}/upload', {{
            method: 'POST',
            body: formData
          }});
          window.__captureResults = await response.json();
        }} catch (error) {{
          window.__captureError = String(error && error.message ? error.message : error);
        }} finally {{
          document.body.dataset.done = 'true';
        }}
      }})();
"#,
            api_origin = state.api_origin,
        ))
    }

    async fn eventsource_page() -> Html<String> {
        scripted_page(
            r#"
      (() => {
        try {
          const source = new EventSource('/sse?stream=prices');
          source.onmessage = event => {
            window.__captureResults = JSON.parse(event.data);
            document.body.dataset.done = 'true';
            source.close();
          };
          source.onerror = () => {
            window.__captureError = 'eventsource failed';
            document.body.dataset.done = 'true';
            source.close();
          };
        } catch (error) {
          window.__captureError = String(error && error.message ? error.message : error);
          document.body.dataset.done = 'true';
        }
      })();
"#,
        )
    }

    async fn service_worker_page() -> Html<String> {
        scripted_page(
            r#"
      (async () => {
        try {
          await navigator.serviceWorker.register('/sw.js');
          await navigator.serviceWorker.ready;
          if (!navigator.serviceWorker.controller) {
            const reloads = Number(sessionStorage.getItem('sw-reloads') || '0');
            if (reloads < 1) {
              sessionStorage.setItem('sw-reloads', String(reloads + 1));
              location.reload();
              return;
            }
          }
          const response = await fetch('/sw-proxy?term=milk');
          window.__captureResults = await response.json();
        } catch (error) {
          window.__captureError = String(error && error.message ? error.message : error);
        } finally {
          if (navigator.serviceWorker.controller || sessionStorage.getItem('sw-reloads') === '1') {
            document.body.dataset.done = 'true';
          }
        }
      })();
"#,
        )
    }

    async fn service_worker_script() -> impl IntoResponse {
        (
            [(CONTENT_TYPE, "application/javascript")],
            r#"
self.addEventListener('install', event => {
  self.skipWaiting();
});

self.addEventListener('activate', event => {
  event.waitUntil(self.clients.claim());
});

self.addEventListener('fetch', event => {
  const url = new URL(event.request.url);
  if (url.pathname === '/sw-proxy') {
    const term = url.searchParams.get('term') || '';
    event.respondWith(fetch(`/api/sw-data?term=${encodeURIComponent(term)}&via=service-worker`));
  }
});
"#,
        )
    }

    async fn sse_stream(
        State(state): State<AppServerState>,
        uri: Uri,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        record_seen_request(&state.shared, "GET", &uri, &headers, "");
        (
            [
                (CONTENT_TYPE, "text/event-stream"),
                (CACHE_CONTROL, "no-cache"),
            ],
            r#"data: {"stream":"prices","price":42}

"#,
        )
    }

    async fn record_sw_data(
        State(state): State<AppServerState>,
        uri: Uri,
        headers: HeaderMap,
    ) -> Json<Value> {
        record_seen_request(&state.shared, "GET", &uri, &headers, "");
        Json(json!({ "via": "service-worker", "term": "milk" }))
    }

    async fn cors_preflight_options(
        State(state): State<ApiServerState>,
        uri: Uri,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        record_seen_request(&state.shared, "OPTIONS", &uri, &headers, "");
        (
            StatusCode::NO_CONTENT,
            [
                (ACCESS_CONTROL_ALLOW_ORIGIN, state.app_origin),
                (
                    ACCESS_CONTROL_ALLOW_METHODS,
                    "GET, POST, PUT, DELETE, PATCH, OPTIONS".to_string(),
                ),
                (
                    ACCESS_CONTROL_ALLOW_HEADERS,
                    "content-type,x-test-mode".to_string(),
                ),
                (ACCESS_CONTROL_ALLOW_CREDENTIALS, "true".to_string()),
                (ACCESS_CONTROL_MAX_AGE, "3600".to_string()),
            ],
        )
    }

    async fn cors_preflight_post(
        State(state): State<ApiServerState>,
        uri: Uri,
        headers: HeaderMap,
        body: String,
    ) -> impl IntoResponse {
        let payload: Value = serde_json::from_str(&body)
            .unwrap_or_else(|error| panic!("cors payload should parse: {error}"));
        record_seen_request(&state.shared, "POST", &uri, &headers, body);
        (
            StatusCode::OK,
            [(ACCESS_CONTROL_ALLOW_ORIGIN, state.app_origin)],
            Json(json!({
                "ok": true,
                "term": payload["term"],
                "page": payload["page"],
                "mode": headers.get("x-test-mode").and_then(|value| value.to_str().ok())
            })),
        )
    }

    async fn upload_handler(
        State(state): State<ApiServerState>,
        uri: Uri,
        headers: HeaderMap,
        body: String,
    ) -> impl IntoResponse {
        let body_snapshot = body.clone();
        record_seen_request(&state.shared, "POST", &uri, &headers, body);
        (
            StatusCode::OK,
            [(ACCESS_CONTROL_ALLOW_ORIGIN, state.app_origin)],
            Json(json!({
                "uploaded": true,
                "metadata_seen": body_snapshot.contains("name=\"metadata\""),
                "file_seen": body_snapshot.contains("filename=\"note.txt\"")
            })),
        )
    }

    async fn login_start() -> impl IntoResponse {
        (
            StatusCode::FOUND,
            [(LOCATION, "/login-complete")],
            String::new(),
        )
    }

    async fn login_complete() -> impl IntoResponse {
        (
            [(SET_COOKIE, "session=browser-live; Path=/; HttpOnly")],
            scripted_page("document.body.dataset.done = 'true';").0,
        )
    }

    async fn profile_client_page() -> Html<String> {
        scripted_page(
            r#"
      (async () => {
        try {
          const response = await fetch('/api/profile');
          window.__captureResults = await response.json();
        } catch (error) {
          window.__captureError = String(error && error.message ? error.message : error);
        } finally {
          document.body.dataset.done = 'true';
        }
      })();
"#,
        )
    }

    async fn profile_api(
        State(state): State<ApiServerState>,
        uri: Uri,
        headers: HeaderMap,
    ) -> Json<Value> {
        record_seen_request(&state.shared, "GET", &uri, &headers, "");
        Json(json!({
            "authenticated": headers
                .get("cookie")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|cookie| cookie.contains("session=browser-live"))
        }))
    }

    async fn start_live_servers() -> Result<LiveServers, Box<dyn std::error::Error>> {
        let app_listener = TcpListener::bind("127.0.0.1:0").await?;
        let api_listener = TcpListener::bind("127.0.0.1:0").await?;
        let app_addr = app_listener.local_addr()?;
        let api_addr = api_listener.local_addr()?;
        let shared = LiveServerState::default();

        let app = Router::new()
            .route("/remote-httpbin", get(remote_httpbin_page))
            .route("/remote-graphql", get(remote_graphql_page))
            .route("/cors-page", get(cors_preflight_page))
            .route("/upload-page", get(multipart_upload_page))
            .route("/eventsource-page", get(eventsource_page))
            .route("/service-worker-page", get(service_worker_page))
            .route("/sw.js", get(service_worker_script))
            .route("/sse", get(sse_stream))
            .route("/api/sw-data", get(record_sw_data))
            .with_state(AppServerState {
                api_origin: format!("http://{api_addr}"),
                shared: shared.clone(),
            });

        let api = Router::new()
            .route(
                "/cors/preflight",
                options(cors_preflight_options).post(cors_preflight_post),
            )
            .route("/upload", post(upload_handler))
            .route("/login-start", get(login_start))
            .route("/login-complete", get(login_complete))
            .route("/profile-client", get(profile_client_page))
            .route("/api/profile", get(profile_api))
            .with_state(ApiServerState {
                app_origin: format!("http://{app_addr}"),
                shared: shared.clone(),
            });

        let app_server = tokio::spawn(async move {
            axum::serve(app_listener, app)
                .await
                .unwrap_or_else(|error| panic!("live app server should run: {error}"));
        });
        let api_server = tokio::spawn(async move {
            axum::serve(api_listener, api)
                .await
                .unwrap_or_else(|error| panic!("live api server should run: {error}"));
        });

        Ok(LiveServers {
            app_addr,
            api_addr,
            shared,
            app_server,
            api_server,
        })
    }

    async fn wait_for_page_done(page: &Page) -> Result<(), Box<dyn std::error::Error>> {
        timeout(Duration::from_secs(10), async {
            loop {
                let result: Value = page
                    .evaluate("document.body.dataset.done || ''")
                    .await?
                    .into_value()?;
                if result == Value::String("true".to_string()) {
                    return Ok(()) as Result<(), Box<dyn std::error::Error>>;
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await??;

        Ok(())
    }

    async fn page_capture_results(page: &Page) -> Result<Value, Box<dyn std::error::Error>> {
        let error_payload: Value = page
            .evaluate("JSON.stringify(window.__captureError ?? null)")
            .await?
            .into_value()?;
        let error: Value = serde_json::from_str(
            error_payload
                .as_str()
                .ok_or("capture error payload should be a string")?,
        )?;
        if let Some(error) = error.as_str() {
            return Err(format!("page capture failed: {error}").into());
        }
        let results_payload: Value = page
            .evaluate("JSON.stringify(window.__captureResults ?? null)")
            .await?
            .into_value()?;
        let results: Value = serde_json::from_str(
            results_payload
                .as_str()
                .ok_or("capture results payload should be a string")?,
        )?;
        if results.is_null() {
            return Err("page produced no capture results".into());
        }
        Ok(results)
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

    fn find_endpoint_by_method<'a>(
        catalog: &'a crate::api_capture::ApiCatalog,
        method: &str,
        path_template: &str,
    ) -> &'a crate::api_capture::ApiEndpoint {
        catalog
            .endpoints
            .iter()
            .find(|endpoint| endpoint.method == method && endpoint.path_template == path_template)
            .unwrap_or_else(|| panic!("endpoint '{method} {path_template}' should be captured"))
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires local chromium"]
    async fn api_capture_tracks_real_browser_session_usage()
    -> Result<(), Box<dyn std::error::Error>> {
        let _ = tracing_subscriber::fmt::try_init();
        let _browser_guard = acquire_live_browser_test_guard().await;
        let (addr, state, server) = start_practical_server().await?;
        let (pool, _profile_dir) = live_test_pool();

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
            .await?;

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

    #[tokio::test(flavor = "multi_thread")]
    async fn replace_with_patchright_preserves_interception_and_api_capture()
    -> Result<(), Box<dyn std::error::Error>> {
        let _browser_guard = acquire_live_browser_test_guard().await;
        let (addr, state, server) = start_practical_server().await?;
        let (pool, _profile_dir) = live_test_pool();

        let outcome = async {
            let sid = pool
                .get_or_create(None, false, Some(BrowserPreference::Auto))
                .await?;

            pool.start_api_capture(&sid, crate::api_capture::ApiCaptureConfig::default())
                .await?;
            pool.enable_interception(
                &sid,
                vec!["*api/search*".to_string(), "*api/suggest*".to_string()],
                HashMap::new(),
            )
            .await?;
            pool.set_extra_headers(
                &sid,
                HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer initial-token".to_string(),
                )]),
            )
            .await?;

            pool.replace_with_patchright(&sid).await?;
            assert!(pool.session_uses_patchright(&sid).await);

            pool.set_extra_headers(
                &sid,
                HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer patchright-token".to_string(),
                )]),
            )
            .await?;

            {
                let instance = pool.get_patchright_session(&sid).await?;
                let mut inst = instance.lock().await;
                inst.session.goto(&format!("http://{addr}/page1")).await?;
                assert!(
                    inst.session
                        .wait_selector("body[data-done='true']", 10_000)
                        .await?
                );
                inst.session.goto(&format!("http://{addr}/page2")).await?;
                assert!(
                    inst.session
                        .wait_selector("body[data-done='true']", 10_000)
                        .await?
                );
            }

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
        let suggest = find_endpoint(&catalog, "/api/suggest");

        assert!(search.auth.iter().any(|auth| auth.scheme == "bearer"));
        assert!(suggest.auth.iter().any(|auth| auth.scheme == "bearer"));

        let seen = state.seen.lock().unwrap().clone();
        assert!(seen.iter().any(|request| {
            request.path == "/api/search"
                && request.method == "GET"
                && request.auth.as_deref() == Some("Bearer patchright-token")
        }));
        assert!(seen.iter().any(|request| {
            request.path == "/api/suggest"
                && request.method == "GET"
                && request.auth.as_deref() == Some("Bearer patchright-token")
        }));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replace_with_patchright_restores_browser_session_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let _browser_guard = acquire_live_browser_test_guard().await;
        let (addr, _state, server) = start_practical_server().await?;
        let (pool, _profile_dir) = live_test_pool();

        let outcome = async {
            let sid = pool
                .get_or_create(None, false, Some(BrowserPreference::Auto))
                .await?;
            let page = pool.get_page(&sid).await?;
            page.goto(&format!("http://{addr}/page1")).await?;
            wait_for_page_done(&page).await?;
            page.evaluate(
                r#"
                    (() => {
                        localStorage.setItem('handoff-theme', 'dark');
                        sessionStorage.setItem('handoff-auth', 'ready');
                        document.cookie = 'handoff=yes; path=/';
                    })()
                "#,
            )
            .await?;

            pool.replace_with_patchright(&sid).await?;
            let instance = pool.get_patchright_session(&sid).await?;
            let mut inst = instance.lock().await;
            inst.session.goto(&format!("http://{addr}/page1")).await?;
            assert!(
                inst.session
                    .wait_selector("body[data-done='true']", 10_000)
                    .await?
            );
            let local = inst
                .session
                .evaluate("localStorage.getItem('handoff-theme')")
                .await?;
            let session = inst
                .session
                .evaluate("sessionStorage.getItem('handoff-auth')")
                .await?;
            let cookie = inst
                .session
                .evaluate("document.cookie.includes('handoff=yes')")
                .await?;
            inst.session.close().await?;

            Ok::<_, Box<dyn std::error::Error>>((local, session, cookie))
        }
        .await;

        server.abort();

        let (local, session, cookie) = outcome?;
        assert_eq!(local.as_str(), Some("dark"));
        assert_eq!(session.as_str(), Some("ready"));
        assert_eq!(cookie, Value::Bool(true));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prepare_transfer_state_for_chromium_retry_restores_browser_session_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let _browser_guard = acquire_live_browser_test_guard().await;
        let (addr, _state, server) = start_practical_server().await?;
        let (pool, _profile_dir) = live_test_pool();

        let outcome = async {
            let sid = pool
                .get_or_create(None, false, Some(BrowserPreference::Auto))
                .await?;
            let page = pool.get_page(&sid).await?;
            let url = format!("http://{addr}/page1");
            page.goto(&url).await?;
            wait_for_page_done(&page).await?;
            page.evaluate(
                r#"
                    (() => {
                        localStorage.setItem('handoff-theme', 'dark');
                        sessionStorage.setItem('handoff-auth', 'ready');
                        document.cookie = 'handoff=yes; path=/';
                    })()
                "#,
            )
            .await?;

            let transfer_state = pool.take_transfer_state_from_chromium(&sid).await;
            pool.close_session(&sid).await?;

            let new_sid = pool
                .get_or_create(None, false, Some(BrowserPreference::Auto))
                .await?;
            let new_page = pool.get_page(&new_sid).await?;
            let post_navigation_state = pool
                .prepare_transfer_state_for_chromium_retry(&new_sid, transfer_state)
                .await?;

            new_page.goto(&url).await?;
            wait_for_page_done(&new_page).await?;
            if let Some(state) = post_navigation_state.as_ref() {
                crate::session_state::restore_storage(&new_page, state).await?;
                if !state.storage.is_empty() {
                    new_page.reload().await?;
                    let _ = new_page.wait_for_navigation().await;
                    wait_for_page_done(&new_page).await?;
                }
            }

            let local = new_page
                .evaluate("localStorage.getItem('handoff-theme')")
                .await?;
            let session = new_page
                .evaluate("sessionStorage.getItem('handoff-auth')")
                .await?;
            let cookie = new_page
                .evaluate("document.cookie.includes('handoff=yes')")
                .await?;
            pool.close_session(&new_sid).await?;

            Ok::<_, Box<dyn std::error::Error>>((local, session, cookie))
        }
        .await;

        server.abort();

        let (local, session, cookie) = outcome?;
        let local: Value = local.into_value()?;
        let session: Value = session.into_value()?;
        let cookie: Value = cookie.into_value()?;
        assert_eq!(local, Value::String("dark".to_string()));
        assert_eq!(session, Value::String("ready".to_string()));
        assert_eq!(cookie, Value::Bool(true));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replace_with_patchright_rolls_back_when_restore_fails()
    -> Result<(), Box<dyn std::error::Error>> {
        let _browser_guard = acquire_live_browser_test_guard().await;
        let (addr, state, server) = start_practical_server().await?;
        let (pool, _profile_dir) = live_test_pool();

        let outcome = async {
            let sid = pool
                .get_or_create(None, false, Some(BrowserPreference::Auto))
                .await?;

            pool.start_api_capture(&sid, crate::api_capture::ApiCaptureConfig::default())
                .await?;
            pool.enable_interception(
                &sid,
                vec!["*api/search*".to_string(), "*api/suggest*".to_string()],
                HashMap::new(),
            )
            .await?;
            pool.set_extra_headers(
                &sid,
                HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer rollback-token".to_string(),
                )]),
            )
            .await?;

            let launch_profile = {
                let instances = pool.instances.read().await;
                let instance = instances
                    .get(&sid)
                    .unwrap_or_else(|| panic!("chromium instance should exist"));
                let inst = instance.lock().await;
                inst.patchright_launch_profile.clone()
            };
            let transfer_state = pool.take_transfer_state_from_chromium(&sid).await;
            let mut staged = PatchrightInstance {
                session: PatchrightSession::start(&pool.config.protection, &launch_profile).await?,
                last_used: Instant::now(),
                patchright_launch_profile: launch_profile,
                interception: crate::network::InterceptionState::default(),
                api_capture: None,
            };
            staged.session.close().await?;

            let error = pool
                .complete_patchright_replacement(&sid, staged, transfer_state)
                .await
                .expect_err("staged patchright replacement should fail");
            assert!(!pool.session_uses_patchright(&sid).await);
            assert!(matches!(
                error,
                Error::ConnectionClosed(_)
                    | Error::BrowserClosed
                    | Error::Cdp(_)
                    | Error::NavigationFailed(_)
                    | Error::Timeout(_)
                    | Error::InvalidAction(_)
            ));

            let page = pool.get_page(&sid).await?;
            page.goto(&format!("http://{addr}/page1")).await?;
            wait_for_page_done(&page).await?;
            page.goto(&format!("http://{addr}/page2")).await?;
            wait_for_page_done(&page).await?;

            let catalog = pool
                .stop_api_capture(&sid)
                .await
                .unwrap_or_else(|| panic!("api capture should still produce a catalog"));
            pool.disable_interception(&sid).await?;
            pool.close_session(&sid).await?;

            Ok::<_, Box<dyn std::error::Error>>(catalog)
        }
        .await;

        server.abort();

        let catalog = outcome?;
        let search = find_endpoint(&catalog, "/api/search");
        let suggest = find_endpoint(&catalog, "/api/suggest");

        assert!(search.auth.iter().any(|auth| auth.scheme == "bearer"));
        assert!(suggest.auth.iter().any(|auth| auth.scheme == "bearer"));

        let seen = state.seen.lock().unwrap().clone();
        assert!(seen.iter().any(|request| {
            request.path == "/api/search"
                && request.method == "GET"
                && request.auth.as_deref() == Some("Bearer rollback-token")
        }));
        assert!(seen.iter().any(|request| {
            request.path == "/api/suggest"
                && request.method == "GET"
                && request.auth.as_deref() == Some("Bearer rollback-token")
        }));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a live browser and internet access"]
    async fn api_capture_matches_remote_httpbin_echo() -> Result<(), Box<dyn std::error::Error>> {
        let _ = tracing_subscriber::fmt::try_init();
        let _browser_guard = acquire_live_browser_test_guard().await;
        let servers = start_live_servers().await?;
        let (pool, _profile_dir) = live_test_pool();

        let outcome = async {
            let sid = pool
                .get_or_create(None, false, Some(BrowserPreference::Auto))
                .await?;
            pool.start_api_capture(
                &sid,
                crate::api_capture::ApiCaptureConfig {
                    url_patterns: vec!["*httpbin.org/*".to_string()],
                    ..crate::api_capture::ApiCaptureConfig::default()
                },
            )
            .await?;

            let page = pool.get_page(&sid).await?;
            page.goto(&format!("{}/remote-httpbin", servers.app_origin()))
                .await?;
            wait_for_page_done(&page).await?;
            let results = page_capture_results(&page).await?;

            let catalog = pool
                .stop_api_capture(&sid)
                .await
                .unwrap_or_else(|| panic!("httpbin capture should produce a catalog"));
            pool.close_session(&sid).await?;

            Ok::<_, Box<dyn std::error::Error>>((results, catalog))
        }
        .await;

        servers.abort();
        let (results, catalog) = outcome?;
        let get_endpoint = find_endpoint_by_method(&catalog, "GET", "/anything");
        let post_endpoint = find_endpoint_by_method(&catalog, "POST", "/post");
        let preflight_endpoint = find_endpoint_by_method(&catalog, "OPTIONS", "/post");

        assert_eq!(results["get"]["args"]["term"], "milk");
        assert_eq!(results["get"]["args"]["filter"][0], "fresh");
        assert_eq!(results["post"]["json"]["message"], "hello");
        assert_eq!(results["post"]["json"]["count"], 2);

        assert_eq!(get_endpoint.origin, "https://httpbin.org");
        assert!(
            get_endpoint
                .query_params
                .iter()
                .any(|field| field.name == "filter" && field.repeated)
        );
        assert_eq!(post_endpoint.origin, "https://httpbin.org");
        assert_eq!(post_endpoint.body_kind, "json");
        assert!(
            post_endpoint
                .body
                .as_ref()
                .is_some_and(|body| body.fields.iter().any(|field| field.name == "message"))
        );
        assert_eq!(preflight_endpoint.origin, "https://httpbin.org");

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a live browser and internet access"]
    async fn api_capture_matches_live_graphql_endpoint() -> Result<(), Box<dyn std::error::Error>> {
        let _ = tracing_subscriber::fmt::try_init();
        let _browser_guard = acquire_live_browser_test_guard().await;
        let servers = start_live_servers().await?;
        let (pool, _profile_dir) = live_test_pool();

        let outcome = async {
            let sid = pool
                .get_or_create(None, false, Some(BrowserPreference::Auto))
                .await?;
            pool.start_api_capture(
                &sid,
                crate::api_capture::ApiCaptureConfig {
                    url_patterns: vec!["*countries.trevorblades.com*".to_string()],
                    ..crate::api_capture::ApiCaptureConfig::default()
                },
            )
            .await?;

            let page = pool.get_page(&sid).await?;
            page.goto(&format!("{}/remote-graphql", servers.app_origin()))
                .await?;
            wait_for_page_done(&page).await?;
            let results = page_capture_results(&page).await?;

            let catalog = pool
                .stop_api_capture(&sid)
                .await
                .unwrap_or_else(|| panic!("graphql capture should produce a catalog"));
            pool.close_session(&sid).await?;

            Ok::<_, Box<dyn std::error::Error>>((results, catalog))
        }
        .await;

        servers.abort();
        let (results, catalog) = outcome?;
        let graphql_endpoint = find_endpoint_by_method(&catalog, "POST", "/");

        assert_eq!(results["data"]["country"]["code"], "US");
        assert_eq!(results["data"]["country"]["name"], "United States");
        assert_eq!(
            graphql_endpoint.origin,
            "https://countries.trevorblades.com"
        );
        assert_eq!(graphql_endpoint.body_kind, "graphql");
        assert_eq!(
            graphql_endpoint.operation_name.as_deref(),
            Some("CountryByCode")
        );
        assert!(
            graphql_endpoint
                .body
                .as_ref()
                .is_some_and(|body| body.fields.iter().any(|field| field.name == "code"))
        );

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a live browser"]
    async fn api_capture_records_actual_cors_preflight_behavior()
    -> Result<(), Box<dyn std::error::Error>> {
        let _ = tracing_subscriber::fmt::try_init();
        let _browser_guard = acquire_live_browser_test_guard().await;
        let servers = start_live_servers().await?;
        let (pool, _profile_dir) = live_test_pool();

        let outcome = async {
            let sid = pool
                .get_or_create(None, false, Some(BrowserPreference::Auto))
                .await?;
            pool.start_api_capture(&sid, crate::api_capture::ApiCaptureConfig::default())
                .await?;

            let page = pool.get_page(&sid).await?;
            page.goto(&format!("{}/cors-page", servers.app_origin()))
                .await?;
            wait_for_page_done(&page).await?;
            let results = page_capture_results(&page).await?;

            let catalog = pool
                .stop_api_capture(&sid)
                .await
                .unwrap_or_else(|| panic!("cors capture should produce a catalog"));
            pool.close_session(&sid).await?;

            Ok::<_, Box<dyn std::error::Error>>((results, catalog))
        }
        .await;

        let seen = servers.shared.seen.lock().unwrap().clone();
        servers.abort();
        let (results, catalog) = outcome?;
        let options_endpoint = find_endpoint_by_method(&catalog, "OPTIONS", "/cors/preflight");
        let post_endpoint = find_endpoint_by_method(&catalog, "POST", "/cors/preflight");

        assert_eq!(results["ok"], true);
        assert_eq!(results["mode"], "cors-live");
        assert_eq!(options_endpoint.body_kind, "none");
        assert_eq!(post_endpoint.body_kind, "json");
        assert!(
            post_endpoint
                .body
                .as_ref()
                .is_some_and(|body| body.fields.iter().any(|field| field.name == "term"))
        );
        assert!(seen.iter().any(|request| {
            request.path == "/cors/preflight"
                && request.method == "OPTIONS"
                && request.content_type.as_deref().is_none_or(str::is_empty)
        }));
        assert!(seen.iter().any(|request| {
            request.path == "/cors/preflight"
                && request.method == "POST"
                && request.body.contains("\"term\":\"milk\"")
        }));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a live browser"]
    async fn api_capture_tracks_cross_tab_redirected_cookie_auth_flow()
    -> Result<(), Box<dyn std::error::Error>> {
        let _ = tracing_subscriber::fmt::try_init();
        let _browser_guard = acquire_live_browser_test_guard().await;
        let servers = start_live_servers().await?;
        let (pool, _profile_dir) = live_test_pool();

        let outcome = async {
            let sid = pool
                .get_or_create(None, false, Some(BrowserPreference::Auto))
                .await?;
            pool.start_api_capture(&sid, crate::api_capture::ApiCaptureConfig::default())
                .await?;

            let login_page = pool.get_page(&sid).await?;
            login_page
                .goto(&format!("{}/login-start", servers.api_origin()))
                .await?;
            wait_for_page_done(&login_page).await?;

            pool.new_tab(&sid, "profile").await?;
            let profile_page = pool.get_active_page(&sid).await?;
            profile_page
                .goto(&format!("{}/profile-client", servers.api_origin()))
                .await?;
            wait_for_page_done(&profile_page).await?;
            let results = page_capture_results(&profile_page).await?;

            let catalog = pool
                .stop_api_capture(&sid)
                .await
                .unwrap_or_else(|| panic!("cookie flow capture should produce a catalog"));
            pool.close_session(&sid).await?;

            Ok::<_, Box<dyn std::error::Error>>((results, catalog))
        }
        .await;

        let seen = servers.shared.seen.lock().unwrap().clone();
        servers.abort();
        let (results, catalog) = outcome?;
        let profile_endpoint = find_endpoint_by_method(&catalog, "GET", "/api/profile");

        assert_eq!(results["authenticated"], true);
        assert_eq!(profile_endpoint.response.statuses, vec![200]);
        assert!(seen.iter().any(|request| {
            request.path == "/api/profile"
                && request.method == "GET"
                && request
                    .cookie
                    .as_deref()
                    .is_some_and(|cookie| cookie.contains("session=browser-live"))
        }));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a live browser"]
    async fn api_capture_infers_live_multipart_uploads() -> Result<(), Box<dyn std::error::Error>> {
        let _ = tracing_subscriber::fmt::try_init();
        let _browser_guard = acquire_live_browser_test_guard().await;
        let servers = start_live_servers().await?;
        let (pool, _profile_dir) = live_test_pool();

        let outcome = async {
            let sid = pool
                .get_or_create(None, false, Some(BrowserPreference::Auto))
                .await?;
            pool.start_api_capture(&sid, crate::api_capture::ApiCaptureConfig::default())
                .await?;

            let page = pool.get_page(&sid).await?;
            page.goto(&format!("{}/upload-page", servers.app_origin()))
                .await?;
            wait_for_page_done(&page).await?;
            let results = page_capture_results(&page).await?;

            let catalog = pool
                .stop_api_capture(&sid)
                .await
                .unwrap_or_else(|| panic!("multipart capture should produce a catalog"));
            pool.close_session(&sid).await?;

            Ok::<_, Box<dyn std::error::Error>>((results, catalog))
        }
        .await;

        let seen = servers.shared.seen.lock().unwrap().clone();
        servers.abort();
        let (results, catalog) = outcome?;
        let upload_endpoint = find_endpoint_by_method(&catalog, "POST", "/upload");

        assert_eq!(results["uploaded"], true);
        assert_eq!(results["metadata_seen"], true);
        assert_eq!(results["file_seen"], true);
        assert_eq!(upload_endpoint.body_kind, "multipart");
        assert!(
            upload_endpoint
                .body
                .as_ref()
                .is_some_and(|body| body.fields.iter().any(|field| field.name == "metadata"))
        );
        assert!(
            upload_endpoint
                .body
                .as_ref()
                .is_some_and(|body| body.fields.iter().any(|field| field.name == "file"))
        );
        assert!(seen.iter().any(|request| {
            request.path == "/upload"
                && request.method == "POST"
                && request.body.contains("filename=\"note.txt\"")
                && request.body.contains("name=\"metadata\"")
        }));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a live browser"]
    async fn api_capture_records_live_eventsource_requests()
    -> Result<(), Box<dyn std::error::Error>> {
        let _ = tracing_subscriber::fmt::try_init();
        let _browser_guard = acquire_live_browser_test_guard().await;
        let servers = start_live_servers().await?;
        let (pool, _profile_dir) = live_test_pool();

        let outcome = async {
            let sid = pool
                .get_or_create(None, false, Some(BrowserPreference::Auto))
                .await?;
            pool.start_api_capture(&sid, crate::api_capture::ApiCaptureConfig::default())
                .await?;

            let page = pool.get_page(&sid).await?;
            page.goto(&format!("{}/eventsource-page", servers.app_origin()))
                .await?;
            wait_for_page_done(&page).await?;
            let results = page_capture_results(&page).await?;

            let catalog = pool
                .stop_api_capture(&sid)
                .await
                .unwrap_or_else(|| panic!("eventsource capture should produce a catalog"));
            pool.close_session(&sid).await?;

            Ok::<_, Box<dyn std::error::Error>>((results, catalog))
        }
        .await;

        let seen = servers.shared.seen.lock().unwrap().clone();
        servers.abort();
        let (results, catalog) = outcome?;
        let sse_endpoint = find_endpoint_by_method(&catalog, "GET", "/sse");

        assert_eq!(results["stream"], "prices");
        assert_eq!(results["price"], 42);
        assert!(
            sse_endpoint
                .query_params
                .iter()
                .any(|field| field.name == "stream" && field.required)
        );
        assert!(
            sse_endpoint
                .response
                .content_types
                .iter()
                .any(|content_type| content_type == "text/event-stream")
        );
        assert!(seen.iter().any(|request| {
            request.path == "/sse"
                && request.method == "GET"
                && request.query.contains("stream=prices")
        }));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a live browser"]
    async fn api_capture_records_service_worker_network_requests()
    -> Result<(), Box<dyn std::error::Error>> {
        let _ = tracing_subscriber::fmt::try_init();
        let _browser_guard = acquire_live_browser_test_guard().await;
        let servers = start_live_servers().await?;
        let (pool, _profile_dir) = live_test_pool();

        let outcome = async {
            let sid = pool
                .get_or_create(None, false, Some(BrowserPreference::Auto))
                .await?;
            pool.start_api_capture(&sid, crate::api_capture::ApiCaptureConfig::default())
                .await?;

            let page = pool.get_page(&sid).await?;
            page.goto(&format!("{}/service-worker-page", servers.app_origin()))
                .await?;
            wait_for_page_done(&page).await?;
            let results = page_capture_results(&page).await?;

            let catalog = pool
                .stop_api_capture(&sid)
                .await
                .unwrap_or_else(|| panic!("service worker capture should produce a catalog"));
            pool.close_session(&sid).await?;

            Ok::<_, Box<dyn std::error::Error>>((results, catalog))
        }
        .await;

        let seen = servers.shared.seen.lock().unwrap().clone();
        servers.abort();
        let (results, catalog) = outcome?;
        let sw_endpoint = find_endpoint_by_method(&catalog, "GET", "/api/sw-data");

        assert_eq!(results["via"], "service-worker");
        assert_eq!(results["term"], "milk");
        assert!(
            sw_endpoint
                .query_params
                .iter()
                .any(|field| field.name == "term" && field.required)
        );
        assert!(seen.iter().any(|request| {
            request.path == "/api/sw-data"
                && request.method == "GET"
                && request.query.contains("via=service-worker")
        }));

        Ok(())
    }
}
