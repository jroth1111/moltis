//! Browser manager providing high-level browser automation actions.

use std::{sync::Arc, time::Instant};

use {
    base64::{Engine, engine::general_purpose::STANDARD as BASE64},
    chromiumoxide::{
        Page,
        cdp::browser_protocol::{
            input::{
                DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams,
                DispatchMouseEventType, MouseButton,
            },
            page::CaptureScreenshotFormat,
        },
    },
    tokio::time::{Duration, sleep, timeout},
    tracing::{debug, info, warn},
};

use crate::{
    challenge::ChallengeType,
    error::Error,
    pool::BrowserPool,
    snapshot::{
        extract_snapshot, find_element_by_ref, focus_element_by_ref, scroll_element_into_view,
    },
    types::{BrowserAction, BrowserConfig, BrowserPreference, BrowserRequest, BrowserResponse},
};

const NAV_DIAGNOSTICS_JS: &str = r#"
(() => {
    const text = (document.body?.innerText || '').replace(/\s+/g, ' ').trim();
    return {
        title_len: (document.title || '').trim().length,
        body_text_len: text.length
    };
})()
"#;

const CHALLENGE_WAIT_MAX_SECONDS: usize = 30;
const CHALLENGE_STABLE_READ_THRESHOLD: usize = 20;

#[derive(Debug, Clone)]
struct NavigationDiagnostics {
    final_url: String,
    title_len: usize,
    body_text_len: usize,
    html_len: usize,
    challenge_type: Option<ChallengeType>,
    challenge_markers: Vec<String>,
}

fn should_suppress_generic_challenge(
    challenge_type: Option<ChallengeType>,
    title_len: usize,
    body_text_len: usize,
) -> bool {
    challenge_type == Some(ChallengeType::GenericChallenge) && (title_len > 0 || body_text_len > 80)
}

/// Extract session_id or return an error for actions that require an existing session.
fn require_session(session_id: Option<&str>, action: &str) -> Result<String, Error> {
    session_id
        .map(String::from)
        .ok_or_else(|| Error::InvalidAction(format!("{action} requires a session_id")))
}

/// Manage Chrome/Chromium instances with CDP.
pub struct BrowserManager {
    pool: Arc<BrowserPool>,
    config: BrowserConfig,
}

impl Default for BrowserManager {
    fn default() -> Self {
        Self::new(BrowserConfig::default())
    }
}

impl BrowserManager {
    /// Create a new browser manager with the given configuration.
    pub fn new(config: BrowserConfig) -> Self {
        match crate::container::cleanup_stale_browser_containers(&config.container_prefix) {
            Ok(removed) if removed > 0 => {
                info!(
                    removed,
                    "removed stale browser containers from previous runs"
                );
            },
            Ok(_) => {},
            Err(e) => {
                warn!(error = %e, "failed to clean stale browser containers at startup");
            },
        }

        info!(
            sandbox_image = %config.sandbox_image,
            "browser manager initialized (sandbox mode controlled per-session)"
        );

        Self {
            pool: Arc::new(BrowserPool::new(config.clone())),
            config,
        }
    }

    /// Check if browser support is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Handle a browser request.
    pub async fn handle_request(&self, request: BrowserRequest) -> BrowserResponse {
        if !self.config.enabled {
            return BrowserResponse::error(
                request.session_id.unwrap_or_default(),
                "browser support is disabled",
                0,
            );
        }

        // Determine sandbox mode from request (defaults to false/host)
        let sandbox = request.sandbox.unwrap_or(false);

        // Log the action with execution mode for visibility
        let mode = if sandbox {
            "sandbox"
        } else {
            "host"
        };
        info!(
            action = %request.action,
            session_id = request.session_id.as_deref().unwrap_or("(new)"),
            browser = ?request.browser,
            execution_mode = mode,
            sandbox_image = %self.config.sandbox_image,
            "executing browser action"
        );

        let start = Instant::now();
        let timeout_duration = Duration::from_millis(request.timeout_ms);

        match timeout(
            timeout_duration,
            self.execute_action(
                request.session_id.as_deref(),
                request.action,
                sandbox,
                request.browser,
            ),
        )
        .await
        {
            Ok(result) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                match result {
                    Ok((session_id, response)) => {
                        let mut resp = response;
                        resp.duration_ms = duration_ms;
                        resp.session_id = session_id;
                        resp
                    },
                    Err(e) => {
                        #[cfg(feature = "metrics")]
                        moltis_metrics::counter!(
                            moltis_metrics::browser::ERRORS_TOTAL,
                            "type" => e.to_string()
                        )
                        .increment(1);

                        BrowserResponse::error(
                            request.session_id.unwrap_or_default(),
                            e.to_string(),
                            duration_ms,
                        )
                    },
                }
            },
            Err(_) => {
                #[cfg(feature = "metrics")]
                moltis_metrics::counter!(
                    moltis_metrics::browser::ERRORS_TOTAL,
                    "type" => "timeout"
                )
                .increment(1);

                BrowserResponse::error(
                    request.session_id.unwrap_or_default(),
                    format!("operation timed out after {}ms", request.timeout_ms),
                    request.timeout_ms,
                )
            },
        }
    }

    /// Clean up a session whose CDP connection has died and return an
    /// actionable error the agent can act on.
    async fn cleanup_stale_session(&self, session_id: &str, action: &str) -> Error {
        warn!(
            session_id,
            action, "browser connection dead, closing stale session"
        );
        let _ = self.pool.close_session(session_id).await;
        Error::ConnectionClosed(format!(
            "Browser session {session_id} lost its connection during {action}. \
             Please navigate to the page again to get a fresh session."
        ))
    }

    /// Execute a browser action.
    async fn execute_action(
        &self,
        session_id: Option<&str>,
        action: BrowserAction,
        sandbox: bool,
        browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        // Navigate has its own retry-with-fresh-session logic, so handle it
        // separately to avoid double-cleanup.
        if let BrowserAction::Navigate { ref url } = action {
            return self.navigate(session_id, url, sandbox, browser).await;
        }

        let action_name = action.to_string();

        let result = match action {
            BrowserAction::Navigate { .. } => unreachable!(),
            BrowserAction::Screenshot {
                full_page,
                highlight_ref,
            } => {
                self.screenshot(session_id, full_page, highlight_ref, sandbox, browser)
                    .await
            },
            BrowserAction::Snapshot => self.snapshot(session_id, sandbox, browser).await,
            BrowserAction::Click { ref_ } => self.click(session_id, ref_, sandbox).await,
            BrowserAction::Type { ref_, text } => {
                self.type_text(session_id, ref_, &text, sandbox).await
            },
            BrowserAction::Scroll { ref_, x, y } => {
                self.scroll(session_id, ref_, x, y, sandbox).await
            },
            BrowserAction::Evaluate { code } => self.evaluate(session_id, &code, sandbox).await,
            BrowserAction::Wait {
                selector,
                ref_,
                timeout_ms,
            } => {
                self.wait(session_id, selector, ref_, timeout_ms, sandbox)
                    .await
            },
            BrowserAction::GetUrl => self.get_url(session_id, sandbox).await,
            BrowserAction::GetTitle => self.get_title(session_id, sandbox).await,
            BrowserAction::Back => self.go_back(session_id, sandbox).await,
            BrowserAction::Forward => self.go_forward(session_id, sandbox).await,
            BrowserAction::Refresh => self.refresh(session_id, sandbox).await,
            BrowserAction::Hover { ref_ } => self.hover(session_id, ref_, sandbox).await,
            BrowserAction::DoubleClick { ref_ } => {
                self.double_click(session_id, ref_, sandbox).await
            },
            BrowserAction::Focus { ref_ } => self.focus(session_id, ref_, sandbox).await,
            BrowserAction::Drag { from_ref, to_ref } => {
                self.drag(session_id, from_ref, to_ref, sandbox).await
            },
            BrowserAction::Check { ref_ } => self.check(session_id, ref_, sandbox).await,
            BrowserAction::Uncheck { ref_ } => self.uncheck(session_id, ref_, sandbox).await,
            BrowserAction::Select { ref_, value } => {
                self.select(session_id, ref_, &value, sandbox).await
            },
            BrowserAction::Press { key } => self.press(session_id, &key, sandbox).await,
            BrowserAction::Upload { ref_, path } => {
                self.upload(session_id, ref_, &path, sandbox).await
            },
            BrowserAction::Clear { ref_ } => self.clear(session_id, ref_, sandbox).await,
            // Phase 5: Network
            BrowserAction::InterceptRequests { url_patterns } => {
                self.intercept_requests(session_id, url_patterns, sandbox, browser)
                    .await
            },
            BrowserAction::StopIntercept => self.stop_intercept(session_id, sandbox, browser).await,
            BrowserAction::SetExtraHeaders { headers } => {
                self.set_extra_headers(session_id, headers, sandbox, browser)
                    .await
            },
            BrowserAction::StartHar => self.start_har(session_id, sandbox, browser).await,
            BrowserAction::StopHar => self.stop_har(session_id, sandbox, browser).await,
            // Phase 6: Session state
            BrowserAction::SaveState { name, encrypt } => {
                self.save_state(session_id, &name, encrypt, sandbox, browser)
                    .await
            },
            BrowserAction::LoadState { name } => {
                self.load_state(session_id, &name, sandbox, browser).await
            },
            BrowserAction::ListStates => self.list_states(session_id, sandbox, browser).await,
            BrowserAction::DeleteState { name } => {
                self.delete_state(session_id, &name, sandbox, browser).await
            },
            // Phase 7a: Emulation
            BrowserAction::SetDevice {
                width,
                height,
                device_scale_factor,
                mobile,
            } => {
                self.set_device(
                    session_id,
                    width,
                    height,
                    device_scale_factor,
                    mobile,
                    sandbox,
                )
                .await
            },
            BrowserAction::SetGeolocation {
                latitude,
                longitude,
                accuracy,
            } => {
                self.set_geolocation(session_id, latitude, longitude, accuracy, sandbox)
                    .await
            },
            BrowserAction::SetTimezone { timezone_id } => {
                self.set_timezone(session_id, &timezone_id, sandbox).await
            },
            BrowserAction::SetLocale { locale } => {
                self.set_locale(session_id, &locale, sandbox).await
            },
            BrowserAction::ClearDevice => self.clear_device(session_id, sandbox).await,
            // Phase 7b: Screencast
            BrowserAction::StartScreencast {
                format,
                quality,
                every_nth,
            } => {
                self.action_start_screencast(
                    session_id, &format, quality, every_nth, sandbox, browser,
                )
                .await
            },
            BrowserAction::StopScreencast => {
                self.action_stop_screencast(session_id, sandbox, browser)
                    .await
            },
            BrowserAction::GetScreencastFrame => {
                self.action_get_screencast_frame(session_id, sandbox, browser)
                    .await
            },
            // Phase 7c: Tabs
            BrowserAction::TabNew { name } => {
                self.tab_new(session_id, &name, sandbox, browser).await
            },
            BrowserAction::TabList => self.tab_list(session_id, sandbox, browser).await,
            BrowserAction::TabSwitch { name } => {
                self.tab_switch(session_id, &name, sandbox, browser).await
            },
            BrowserAction::TabClose { name } => {
                self.tab_close(session_id, &name, sandbox, browser).await
            },
            BrowserAction::Close => self.close(session_id, sandbox).await,
        };

        // Detect stale connections for all non-Navigate actions
        match result {
            Err(ref e) if e.is_connection_error() => {
                let sid = session_id.unwrap_or("unknown");
                Err(self.cleanup_stale_session(sid, &action_name).await)
            },
            other => other,
        }
    }

    /// Navigate to a URL.
    async fn navigate(
        &self,
        session_id: Option<&str>,
        url: &str,
        sandbox: bool,
        browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        // Validate URL before navigation
        validate_url(url)?;

        // Check if the domain is allowed
        if !crate::types::is_domain_allowed(url, &self.config.allowed_domains) {
            return Err(Error::NavigationFailed(format!(
                "domain not in allowed list. Allowed domains: {:?}",
                self.config.allowed_domains
            )));
        }

        let sid = self
            .pool
            .get_or_create(session_id, sandbox, browser)
            .await?;
        let page = self.pool.get_page(&sid).await?;
        let mut active_sid = sid;
        let mut active_page = page;

        #[cfg(feature = "metrics")]
        let nav_start = Instant::now();

        // Try navigation, retry with fresh session if connection is dead
        if let Err(e) = active_page.goto(url).await {
            let nav_err = Error::NavigationFailed(e.to_string());
            if nav_err.is_connection_error() {
                warn!(
                    session_id = active_sid,
                    "browser connection dead, closing session and retrying"
                );
                let _ = self.pool.close_session(&active_sid).await;
                // Retry with a fresh session (use same sandbox mode)
                let new_sid = self.pool.get_or_create(None, sandbox, browser).await?;
                let new_page = self.pool.get_page(&new_sid).await?;
                new_page
                    .goto(url)
                    .await
                    .map_err(|e| Error::NavigationFailed(e.to_string()))?;
                active_sid = new_sid;
                active_page = new_page;
            }
            if !nav_err.is_connection_error() {
                return Err(nav_err);
            }
        }

        // Wait for post-navigation lifecycle signals.
        let _ = active_page.wait_for_navigation().await;
        let diagnostics = self
            .wait_for_challenge_resolution_if_needed(&active_page)
            .await;

        #[cfg(feature = "metrics")]
        {
            moltis_metrics::histogram!(moltis_metrics::browser::NAVIGATION_DURATION_SECONDS)
                .record(nav_start.elapsed().as_secs_f64());
        }

        let current_url = diagnostics.final_url.clone();
        let challenge_type = diagnostics
            .challenge_type
            .map(|kind| kind.as_str().to_string());

        if let Some(ref kind) = challenge_type {
            warn!(
                session_id = active_sid,
                url = current_url,
                challenge_type = kind,
                markers = ?diagnostics.challenge_markers,
                title_len = diagnostics.title_len,
                body_text_len = diagnostics.body_text_len,
                "navigated to challenge/interstitial page"
            );
        } else {
            info!(
                session_id = active_sid,
                url = current_url,
                title_len = diagnostics.title_len,
                body_text_len = diagnostics.body_text_len,
                "navigated to URL"
            );
        }

        let response = BrowserResponse::success(active_sid.clone(), 0, sandbox)
            .with_url(current_url.clone())
            .with_navigation_diagnostics(
                current_url,
                diagnostics.title_len,
                diagnostics.body_text_len,
                challenge_type,
                diagnostics.challenge_markers,
            );
        Ok((
            active_sid,
            response,
        ))
    }

    async fn collect_navigation_diagnostics(&self, page: &Page) -> NavigationDiagnostics {
        let final_url = page.url().await.ok().flatten().unwrap_or_default();
        let metrics: serde_json::Value = page
            .evaluate(NAV_DIAGNOSTICS_JS)
            .await
            .ok()
            .and_then(|v| v.into_value().ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let title_len = metrics
            .get("title_len")
            .and_then(|v| v.as_u64())
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(0);
        let body_text_len = metrics
            .get("body_text_len")
            .and_then(|v| v.as_u64())
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(0);
        let html = page.content().await.unwrap_or_default();
        let html_len = html.len();
        let detection = crate::challenge::detect_challenge(&html);
        let mut challenge_type = detection.challenge_type;
        let mut challenge_markers: Vec<String> = detection
            .markers
            .into_iter()
            .map(ToString::to_string)
            .collect();
        if should_suppress_generic_challenge(challenge_type, title_len, body_text_len) {
            challenge_type = None;
            challenge_markers.clear();
        }
        NavigationDiagnostics {
            final_url,
            title_len,
            body_text_len,
            html_len,
            challenge_type,
            challenge_markers,
        }
    }

    fn should_wait_for_challenge_resolution(diagnostics: &NavigationDiagnostics) -> bool {
        diagnostics.challenge_type.is_some()
            || (diagnostics.title_len == 0
                && diagnostics.body_text_len == 0
                && diagnostics.html_len > 0)
    }

    async fn wait_for_challenge_resolution_if_needed(&self, page: &Page) -> NavigationDiagnostics {
        let mut diagnostics = self.collect_navigation_diagnostics(page).await;
        if !Self::should_wait_for_challenge_resolution(&diagnostics) {
            return diagnostics;
        }

        debug!(
            challenge_type = diagnostics.challenge_type.map(ChallengeType::as_str),
            title_len = diagnostics.title_len,
            body_text_len = diagnostics.body_text_len,
            html_len = diagnostics.html_len,
            "starting adaptive challenge-resolution wait loop"
        );

        let mut previous_challenge_len: Option<usize> = None;
        let mut stable_challenge_reads = 0usize;

        for _ in 0..CHALLENGE_WAIT_MAX_SECONDS {
            sleep(Duration::from_secs(1)).await;
            let next = self.collect_navigation_diagnostics(page).await;
            let still_waiting = Self::should_wait_for_challenge_resolution(&next);
            let is_challenge = next.challenge_type.is_some();

            if is_challenge {
                if previous_challenge_len == Some(next.html_len) {
                    stable_challenge_reads += 1;
                } else {
                    stable_challenge_reads = 0;
                }
                previous_challenge_len = Some(next.html_len);
            } else {
                stable_challenge_reads = 0;
                previous_challenge_len = None;
            }

            diagnostics = next;

            if !still_waiting {
                break;
            }
            if is_challenge && stable_challenge_reads >= CHALLENGE_STABLE_READ_THRESHOLD {
                break;
            }
        }

        diagnostics
    }

    /// Take a screenshot of the page.
    async fn screenshot(
        &self,
        session_id: Option<&str>,
        full_page: bool,
        highlight_ref: Option<u32>,
        sandbox: bool,
        browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = self
            .pool
            .get_or_create(session_id, sandbox, browser)
            .await?;
        let page = self.pool.get_page(&sid).await?;

        // Optionally highlight an element before screenshot
        if let Some(ref_) = highlight_ref {
            let _ = self.highlight_element(&page, ref_).await;
        }

        let screenshot = page
            .screenshot(
                chromiumoxide::page::ScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Png)
                    .full_page(full_page)
                    .build(),
            )
            .await
            .map_err(|e| Error::ScreenshotFailed(e.to_string()))?;

        // Remove highlight after screenshot
        if highlight_ref.is_some() {
            let _ = self.remove_highlights(&page).await;
        }

        // Use data URI format so the sanitizer can strip it for LLM context
        // while the UI can still display it as an image
        let data_uri = format!("data:image/png;base64,{}", BASE64.encode(&screenshot));

        #[cfg(feature = "metrics")]
        moltis_metrics::counter!(moltis_metrics::browser::SCREENSHOTS_TOTAL).increment(1);

        // Calculate approximate dimensions from PNG data (width/height are in bytes 16-23)
        let (width, height) = if screenshot.len() > 24 {
            let w = u32::from_be_bytes([
                screenshot[16],
                screenshot[17],
                screenshot[18],
                screenshot[19],
            ]);
            let h = u32::from_be_bytes([
                screenshot[20],
                screenshot[21],
                screenshot[22],
                screenshot[23],
            ]);
            (w, h)
        } else {
            (0, 0)
        };

        info!(
            session_id = sid,
            bytes = screenshot.len(),
            width,
            height,
            full_page,
            "took screenshot"
        );

        Ok((
            sid.clone(),
            BrowserResponse::success(sid, 0, sandbox)
                .with_screenshot(data_uri, self.config.device_scale_factor),
        ))
    }

    /// Get a DOM snapshot with element references.
    ///
    /// Stale-connection errors are detected centrally in `execute_action()`.
    async fn snapshot(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
        browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = self
            .pool
            .get_or_create(session_id, sandbox, browser)
            .await?;
        let page = self.pool.get_page(&sid).await?;

        let snapshot = extract_snapshot(&page).await?;

        debug!(
            session_id = sid,
            elements = snapshot.elements.len(),
            "extracted snapshot"
        );

        Ok((
            sid.clone(),
            BrowserResponse::success(sid, 0, sandbox).with_snapshot(snapshot),
        ))
    }

    /// Click an element by reference.
    async fn click(
        &self,
        session_id: Option<&str>,
        ref_: u32,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "click")?;

        let page = self.pool.get_page(&sid).await?;

        // Scroll element into view first
        scroll_element_into_view(&page, ref_).await?;

        // Small delay for scroll to complete
        sleep(Duration::from_millis(100)).await;

        // Find element center
        let (x, y) = find_element_by_ref(&page, ref_).await?;

        // Dispatch mouse events — behavioral (Bezier + timing) or instant
        #[cfg(feature = "stealth")]
        if self.config.stealth.enabled && self.config.stealth.behavioral {
            let from = self.pool.get_mouse_pos(&sid).await;
            crate::stealth::behavior::realistic_click(&page, from, x, y).await?;
            self.pool.set_mouse_pos(&sid, (x, y)).await;
            #[cfg(feature = "metrics")]
            moltis_metrics::counter!(moltis_metrics::browser::BEHAVIORAL_CLICKS_TOTAL).increment(1);
            debug!(
                session_id = sid,
                ref_ = ref_,
                x = x,
                y = y,
                "clicked element (behavioral)"
            );
        } else {
            self.instant_click(&page, x, y).await?;
            debug!(
                session_id = sid,
                ref_ = ref_,
                x = x,
                y = y,
                "clicked element"
            );
        }

        #[cfg(not(feature = "stealth"))]
        {
            self.instant_click(&page, x, y).await?;
            debug!(
                session_id = sid,
                ref_ = ref_,
                x = x,
                y = y,
                "clicked element"
            );
        }

        Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)))
    }

    /// Type text into an element.
    async fn type_text(
        &self,
        session_id: Option<&str>,
        ref_: u32,
        text: &str,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "type")?;

        let page = self.pool.get_page(&sid).await?;

        // Focus the element
        focus_element_by_ref(&page, ref_).await?;

        // Type text — behavioral (randomised timing) or instant
        #[cfg(feature = "stealth")]
        if self.config.stealth.enabled && self.config.stealth.behavioral {
            crate::stealth::behavior::realistic_type(&page, text).await?;
            #[cfg(feature = "metrics")]
            moltis_metrics::counter!(moltis_metrics::browser::BEHAVIORAL_TYPES_TOTAL).increment(1);
        } else {
            self.instant_type(&page, text).await?;
        }

        #[cfg(not(feature = "stealth"))]
        self.instant_type(&page, text).await?;

        debug!(
            session_id = sid,
            ref_ = ref_,
            chars = text.len(),
            "typed text"
        );

        Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)))
    }

    /// Scroll the page or an element.
    async fn scroll(
        &self,
        session_id: Option<&str>,
        ref_: Option<u32>,
        x: i32,
        y: i32,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "scroll")?;

        let page = self.pool.get_page(&sid).await?;

        let js = if let Some(ref_) = ref_ {
            format!(
                r#"(() => {{
                    const el = document.querySelector(`[data-moltis-ref="{ref_}"]`);
                    if (el) el.scrollBy({x}, {y});
                    return !!el;
                }})()"#
            )
        } else {
            format!("window.scrollBy({x}, {y}); true")
        };

        page.evaluate(js.as_str())
            .await
            .map_err(|e| Error::JsEvalFailed(e.to_string()))?;

        debug!(session_id = sid, ref_ = ?ref_, x = x, y = y, "scrolled");

        Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)))
    }

    /// Execute JavaScript in the page context.
    async fn evaluate(
        &self,
        session_id: Option<&str>,
        code: &str,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "evaluate")?;

        let page = self.pool.get_page(&sid).await?;

        let result: serde_json::Value = page
            .evaluate(code)
            .await
            .map_err(|e| Error::JsEvalFailed(e.to_string()))?
            .into_value()
            .map_err(|e| Error::JsEvalFailed(format!("{e:?}")))?;

        debug!(session_id = sid, "evaluated JavaScript");

        Ok((
            sid.clone(),
            BrowserResponse::success(sid, 0, sandbox).with_result(result),
        ))
    }

    /// Wait for an element to appear.
    async fn wait(
        &self,
        session_id: Option<&str>,
        selector: Option<String>,
        ref_: Option<u32>,
        timeout_ms: u64,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "wait")?;

        let page = self.pool.get_page(&sid).await?;

        let check_js = if let Some(ref selector) = selector {
            format!(
                r#"document.querySelector({}) !== null"#,
                serde_json::to_string(selector).map_err(|e| Error::Cdp(e.to_string()))?
            )
        } else if let Some(ref_) = ref_ {
            format!(r#"document.querySelector('[data-moltis-ref="{ref_}"]') !== null"#)
        } else {
            return Err(Error::InvalidAction("wait requires selector or ref".into()));
        };

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let interval = Duration::from_millis(100);

        while Instant::now() < deadline {
            let found: bool = page
                .evaluate(check_js.as_str())
                .await
                .map_err(|e| Error::JsEvalFailed(e.to_string()))?
                .into_value()
                .unwrap_or(false);

            if found {
                debug!(session_id = sid, "element found");
                return Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)));
            }

            sleep(interval).await;
        }

        Err(Error::Timeout(format!(
            "element not found after {}ms",
            timeout_ms
        )))
    }

    /// Get the current page URL.
    async fn get_url(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "get_url")?;

        let page = self.pool.get_page(&sid).await?;
        let url = page.url().await.ok().flatten().unwrap_or_default();

        Ok((
            sid.clone(),
            BrowserResponse::success(sid, 0, sandbox).with_url(url),
        ))
    }

    /// Get the page title.
    async fn get_title(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "get_title")?;

        let page = self.pool.get_page(&sid).await?;
        let title = page.get_title().await.ok().flatten().unwrap_or_default();

        Ok((
            sid.clone(),
            BrowserResponse::success(sid, 0, sandbox).with_title(title),
        ))
    }

    /// Go back in history.
    async fn go_back(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "back")?;

        let page = self.pool.get_page(&sid).await?;

        page.evaluate("history.back()")
            .await
            .map_err(|e| Error::JsEvalFailed(e.to_string()))?;

        // Wait for navigation
        let _ = page.wait_for_navigation().await;

        let url = page.url().await.ok().flatten().unwrap_or_default();

        Ok((
            sid.clone(),
            BrowserResponse::success(sid, 0, sandbox).with_url(url),
        ))
    }

    /// Go forward in history.
    async fn go_forward(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "forward")?;

        let page = self.pool.get_page(&sid).await?;

        page.evaluate("history.forward()")
            .await
            .map_err(|e| Error::JsEvalFailed(e.to_string()))?;

        // Wait for navigation
        let _ = page.wait_for_navigation().await;

        let url = page.url().await.ok().flatten().unwrap_or_default();

        Ok((
            sid.clone(),
            BrowserResponse::success(sid, 0, sandbox).with_url(url),
        ))
    }

    /// Refresh the page.
    async fn refresh(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "refresh")?;

        let page = self.pool.get_page(&sid).await?;

        page.reload().await.map_err(|e| Error::Cdp(e.to_string()))?;

        // Wait for navigation
        let _ = page.wait_for_navigation().await;

        let url = page.url().await.ok().flatten().unwrap_or_default();

        Ok((
            sid.clone(),
            BrowserResponse::success(sid, 0, sandbox).with_url(url),
        ))
    }

    /// Close the browser session.
    async fn close(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "close")?;

        self.pool.close_session(&sid).await?;

        info!(session_id = sid, "closed browser session");

        Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)))
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Extended actions (Phase 4)
    // ─────────────────────────────────────────────────────────────────────────

    /// Hover the mouse over an element.
    async fn hover(
        &self,
        session_id: Option<&str>,
        ref_: u32,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "hover")?;
        let page = self.pool.get_page(&sid).await?;
        scroll_element_into_view(&page, ref_).await?;
        let (x, y) = find_element_by_ref(&page, ref_).await?;

        #[cfg(feature = "stealth")]
        if self.config.stealth.enabled && self.config.stealth.behavioral {
            let from = self.pool.get_mouse_pos(&sid).await;
            crate::stealth::behavior::bezier_mouse_move(&page, from, (x, y)).await?;
            self.pool.set_mouse_pos(&sid, (x, y)).await;
            #[cfg(feature = "metrics")]
            moltis_metrics::counter!(moltis_metrics::browser::BEHAVIORAL_CLICKS_TOTAL).increment(1);
        } else {
            crate::actions::hover_instant(&page, x, y).await?;
        }

        #[cfg(not(feature = "stealth"))]
        crate::actions::hover_instant(&page, x, y).await?;

        debug!(session_id = sid, ref_ = ref_, x, y, "hovered element");
        Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)))
    }

    /// Double-click an element.
    async fn double_click(
        &self,
        session_id: Option<&str>,
        ref_: u32,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "double_click")?;
        let page = self.pool.get_page(&sid).await?;
        scroll_element_into_view(&page, ref_).await?;
        sleep(Duration::from_millis(100)).await;
        let (x, y) = find_element_by_ref(&page, ref_).await?;

        // Move to element (behavioral or instant) then fire the double-click events
        #[cfg(feature = "stealth")]
        if self.config.stealth.enabled && self.config.stealth.behavioral {
            let from = self.pool.get_mouse_pos(&sid).await;
            crate::stealth::behavior::bezier_mouse_move(&page, from, (x, y)).await?;
            self.pool.set_mouse_pos(&sid, (x, y)).await;
            #[cfg(feature = "metrics")]
            moltis_metrics::counter!(moltis_metrics::browser::BEHAVIORAL_CLICKS_TOTAL).increment(1);
        } else {
            crate::actions::hover_instant(&page, x, y).await?;
        }

        #[cfg(not(feature = "stealth"))]
        crate::actions::hover_instant(&page, x, y).await?;

        crate::actions::double_click_events(&page, x, y).await?;
        debug!(
            session_id = sid,
            ref_ = ref_,
            x,
            y,
            "double-clicked element"
        );
        Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)))
    }

    /// Focus an element via keyboard focus without clicking.
    async fn focus(
        &self,
        session_id: Option<&str>,
        ref_: u32,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "focus")?;
        let page = self.pool.get_page(&sid).await?;
        focus_element_by_ref(&page, ref_).await?;
        debug!(session_id = sid, ref_ = ref_, "focused element");
        Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)))
    }

    /// Drag from one element to another.
    async fn drag(
        &self,
        session_id: Option<&str>,
        from_ref: u32,
        to_ref: u32,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "drag")?;
        let page = self.pool.get_page(&sid).await?;
        let (from_x, from_y) = find_element_by_ref(&page, from_ref).await?;
        let (to_x, to_y) = find_element_by_ref(&page, to_ref).await?;

        #[cfg(feature = "stealth")]
        if self.config.stealth.enabled && self.config.stealth.behavioral {
            // Bezier approach to source → press → bezier drag to destination → release
            let current_pos = self.pool.get_mouse_pos(&sid).await;
            crate::stealth::behavior::bezier_mouse_move(&page, current_pos, (from_x, from_y))
                .await?;
            let press = DispatchMouseEventParams::builder()
                .r#type(DispatchMouseEventType::MousePressed)
                .x(from_x)
                .y(from_y)
                .button(MouseButton::Left)
                .click_count(1)
                .build()
                .map_err(|e| Error::Cdp(e.to_string()))?;
            page.execute(press)
                .await
                .map_err(|e| Error::Cdp(e.to_string()))?;
            crate::stealth::behavior::bezier_mouse_move(&page, (from_x, from_y), (to_x, to_y))
                .await?;
            let release = DispatchMouseEventParams::builder()
                .r#type(DispatchMouseEventType::MouseReleased)
                .x(to_x)
                .y(to_y)
                .button(MouseButton::Left)
                .click_count(1)
                .build()
                .map_err(|e| Error::Cdp(e.to_string()))?;
            page.execute(release)
                .await
                .map_err(|e| Error::Cdp(e.to_string()))?;
            self.pool.set_mouse_pos(&sid, (to_x, to_y)).await;
        } else {
            crate::actions::drag_instant(&page, from_x, from_y, to_x, to_y).await?;
        }

        #[cfg(not(feature = "stealth"))]
        crate::actions::drag_instant(&page, from_x, from_y, to_x, to_y).await?;

        debug!(session_id = sid, from_ref, to_ref, "dragged element");
        Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)))
    }

    /// Check a checkbox or radio element.
    async fn check(
        &self,
        session_id: Option<&str>,
        ref_: u32,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "check")?;
        let page = self.pool.get_page(&sid).await?;
        scroll_element_into_view(&page, ref_).await?;
        crate::actions::check_element(&page, ref_).await?;
        debug!(session_id = sid, ref_ = ref_, "checked element");
        Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)))
    }

    /// Uncheck a checkbox element.
    async fn uncheck(
        &self,
        session_id: Option<&str>,
        ref_: u32,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "uncheck")?;
        let page = self.pool.get_page(&sid).await?;
        scroll_element_into_view(&page, ref_).await?;
        crate::actions::uncheck_element(&page, ref_).await?;
        debug!(session_id = sid, ref_ = ref_, "unchecked element");
        Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)))
    }

    /// Select an option in a `<select>` element by value.
    async fn select(
        &self,
        session_id: Option<&str>,
        ref_: u32,
        value: &str,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "select")?;
        let page = self.pool.get_page(&sid).await?;
        crate::actions::select_option(&page, ref_, value).await?;
        debug!(session_id = sid, ref_ = ref_, value, "selected option");
        Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)))
    }

    /// Press a named key or printable character.
    async fn press(
        &self,
        session_id: Option<&str>,
        key: &str,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "press")?;
        let page = self.pool.get_page(&sid).await?;
        crate::actions::press_key(&page, key).await?;
        debug!(session_id = sid, key, "pressed key");
        Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)))
    }

    /// Upload a file to a file input element.
    async fn upload(
        &self,
        session_id: Option<&str>,
        ref_: u32,
        path: &str,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "upload")?;
        let page = self.pool.get_page(&sid).await?;
        crate::actions::upload_file(&page, ref_, path).await?;
        debug!(session_id = sid, ref_ = ref_, path, "uploaded file");
        Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)))
    }

    /// Clear an input or textarea element.
    async fn clear(
        &self,
        session_id: Option<&str>,
        ref_: u32,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "clear")?;
        let page = self.pool.get_page(&sid).await?;
        scroll_element_into_view(&page, ref_).await?;
        focus_element_by_ref(&page, ref_).await?;
        crate::actions::clear_input(&page, ref_).await?;
        debug!(session_id = sid, ref_ = ref_, "cleared element");
        Ok((sid.clone(), BrowserResponse::success(sid, 0, sandbox)))
    }

    /// Click at (x, y) instantly (no movement emulation).
    async fn instant_click(&self, page: &Page, x: f64, y: f64) -> Result<(), Error> {
        let press_cmd = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MousePressed)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .map_err(|e| Error::Cdp(e.to_string()))?;
        page.execute(press_cmd)
            .await
            .map_err(|e| Error::Cdp(e.to_string()))?;

        let release_cmd = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseReleased)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .map_err(|e| Error::Cdp(e.to_string()))?;
        page.execute(release_cmd)
            .await
            .map_err(|e| Error::Cdp(e.to_string()))?;

        Ok(())
    }

    /// Type `text` instantly with no per-character delay.
    async fn instant_type(&self, page: &Page, text: &str) -> Result<(), Error> {
        for c in text.chars() {
            let key_down = DispatchKeyEventParams::builder()
                .r#type(DispatchKeyEventType::KeyDown)
                .text(c.to_string())
                .build()
                .map_err(|e| Error::Cdp(e.to_string()))?;
            page.execute(key_down)
                .await
                .map_err(|e| Error::Cdp(e.to_string()))?;

            let key_up = DispatchKeyEventParams::builder()
                .r#type(DispatchKeyEventType::KeyUp)
                .text(c.to_string())
                .build()
                .map_err(|e| Error::Cdp(e.to_string()))?;
            page.execute(key_up)
                .await
                .map_err(|e| Error::Cdp(e.to_string()))?;
        }
        Ok(())
    }

    /// Highlight an element (for screenshots).
    async fn highlight_element(&self, page: &Page, ref_: u32) -> Result<(), Error> {
        let js = format!(
            r#"(() => {{
                const el = document.querySelector(`[data-moltis-ref="{ref_}"]`);
                if (el) {{
                    el.style.outline = '3px solid #ff0000';
                    el.style.outlineOffset = '2px';
                }}
            }})()"#
        );

        page.evaluate(js.as_str())
            .await
            .map_err(|e| Error::JsEvalFailed(e.to_string()))?;

        Ok(())
    }

    /// Remove all element highlights.
    async fn remove_highlights(&self, page: &Page) -> Result<(), Error> {
        let js = r#"
            document.querySelectorAll('[data-moltis-ref]').forEach(el => {
                el.style.outline = '';
                el.style.outlineOffset = '';
            });
        "#;

        page.evaluate(js)
            .await
            .map_err(|e| Error::JsEvalFailed(e.to_string()))?;

        Ok(())
    }

    // ── Phase 5: Network interception & HAR ────────────────────────────────

    async fn intercept_requests(
        &self,
        session_id: Option<&str>,
        url_patterns: Vec<String>,
        sandbox: bool,
        browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = self
            .pool
            .get_or_create(session_id, sandbox, browser)
            .await?;

        let start = Instant::now();
        self.pool
            .enable_interception(&sid, url_patterns, Default::default())
            .await?;

        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    async fn stop_intercept(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
        _browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "stop_intercept")?;
        let start = Instant::now();
        self.pool.disable_interception(&sid).await?;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    async fn set_extra_headers(
        &self,
        session_id: Option<&str>,
        headers: std::collections::HashMap<String, String>,
        sandbox: bool,
        browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = self
            .pool
            .get_or_create(session_id, sandbox, browser)
            .await?;
        let start = Instant::now();
        self.pool.set_extra_headers(&sid, headers).await;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    async fn start_har(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
        browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = self
            .pool
            .get_or_create(session_id, sandbox, browser)
            .await?;
        let start = Instant::now();
        self.pool.start_har(&sid).await?;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    async fn stop_har(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
        _browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "stop_har")?;
        let start = Instant::now();
        let har = self.pool.stop_har(&sid).await;
        let mut resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        if let Some(har_json) = har {
            resp = resp.with_result(har_json);
        }
        Ok((sid, resp))
    }

    // ── Phase 6: Session state ──────────────────────────────────────────────

    async fn save_state(
        &self,
        session_id: Option<&str>,
        name: &str,
        encrypt: bool,
        sandbox: bool,
        _browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "save_state")?;
        let start = Instant::now();
        let page = self.pool.get_page(&sid).await?;
        let state = crate::session_state::capture_state(&page).await?;
        let path = crate::session_state::save_to_disk(&state, name, encrypt)?;
        let mut resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        resp = resp.with_url(path.to_string_lossy().into_owned());
        Ok((sid, resp))
    }

    async fn load_state(
        &self,
        session_id: Option<&str>,
        name: &str,
        sandbox: bool,
        _browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "load_state")?;
        let start = Instant::now();
        let state = crate::session_state::load_from_disk(name)?;
        let page = self.pool.get_page(&sid).await?;
        crate::session_state::restore_state(&page, &state).await?;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    async fn list_states(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
        _browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = session_id
            .map(String::from)
            .unwrap_or_else(|| "no-session".to_string());
        let start = Instant::now();
        let names = crate::session_state::list_saved()?;
        let json = serde_json::json!({"states": names});
        let mut resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        resp = resp.with_result(json);
        Ok((sid, resp))
    }

    async fn delete_state(
        &self,
        session_id: Option<&str>,
        name: &str,
        sandbox: bool,
        _browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = session_id
            .map(String::from)
            .unwrap_or_else(|| "no-session".to_string());
        let start = Instant::now();
        crate::session_state::delete_saved(name)?;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    // ── Phase 7a: Emulation ─────────────────────────────────────────────────

    async fn set_device(
        &self,
        session_id: Option<&str>,
        width: u32,
        height: u32,
        device_scale_factor: f64,
        mobile: bool,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "set_device")?;
        let start = Instant::now();
        let page = self.pool.get_page(&sid).await?;
        crate::emulation::set_device(&page, width, height, device_scale_factor, mobile).await?;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    async fn set_geolocation(
        &self,
        session_id: Option<&str>,
        latitude: f64,
        longitude: f64,
        accuracy: f64,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "set_geolocation")?;
        let start = Instant::now();
        let page = self.pool.get_page(&sid).await?;
        crate::emulation::set_geolocation(&page, latitude, longitude, accuracy).await?;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    async fn set_timezone(
        &self,
        session_id: Option<&str>,
        timezone_id: &str,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "set_timezone")?;
        let start = Instant::now();
        let page = self.pool.get_page(&sid).await?;
        crate::emulation::set_timezone(&page, timezone_id).await?;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    async fn set_locale(
        &self,
        session_id: Option<&str>,
        locale: &str,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "set_locale")?;
        let start = Instant::now();
        let page = self.pool.get_page(&sid).await?;
        crate::emulation::set_locale(&page, locale).await?;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    async fn clear_device(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "clear_device")?;
        let start = Instant::now();
        let page = self.pool.get_page(&sid).await?;
        crate::emulation::clear_device_override(&page).await?;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    // ── Phase 7b: Screencast ────────────────────────────────────────────────

    async fn action_start_screencast(
        &self,
        session_id: Option<&str>,
        format: &str,
        quality: u8,
        every_nth: u32,
        sandbox: bool,
        browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = self
            .pool
            .get_or_create(session_id, sandbox, browser)
            .await?;
        let start = Instant::now();
        self.pool
            .start_screencast(&sid, format, quality, every_nth)
            .await?;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    async fn action_stop_screencast(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
        _browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "stop_screencast")?;
        let start = Instant::now();
        self.pool.stop_screencast(&sid).await?;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    async fn action_get_screencast_frame(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
        _browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "get_screencast_frame")?;
        let start = Instant::now();
        let frame_b64 = self.pool.get_screencast_frame(&sid).await;
        let mut resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        if let Some(b64) = frame_b64 {
            resp = resp.with_result(serde_json::json!({"frame": b64}));
        }
        Ok((sid, resp))
    }

    // ── Phase 7c: Tabs ──────────────────────────────────────────────────────

    async fn tab_new(
        &self,
        session_id: Option<&str>,
        name: &str,
        sandbox: bool,
        browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = self
            .pool
            .get_or_create(session_id, sandbox, browser)
            .await?;
        let start = Instant::now();
        self.pool.new_tab(&sid, name).await?;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    async fn tab_list(
        &self,
        session_id: Option<&str>,
        sandbox: bool,
        _browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = session_id
            .map(String::from)
            .unwrap_or_else(|| "no-session".to_string());
        let start = Instant::now();
        let tabs = self.pool.list_tabs(&sid).await;
        let mut resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        resp = resp.with_result(serde_json::json!({"tabs": tabs}));
        Ok((sid, resp))
    }

    async fn tab_switch(
        &self,
        session_id: Option<&str>,
        name: &str,
        sandbox: bool,
        _browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "tab_switch")?;
        let start = Instant::now();
        self.pool.switch_tab(&sid, name).await?;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    async fn tab_close(
        &self,
        session_id: Option<&str>,
        name: &str,
        sandbox: bool,
        _browser: Option<BrowserPreference>,
    ) -> Result<(String, BrowserResponse), Error> {
        let sid = require_session(session_id, "tab_close")?;
        let start = Instant::now();
        self.pool.close_tab(&sid, name).await?;
        let resp =
            BrowserResponse::success(sid.clone(), start.elapsed().as_millis() as u64, sandbox);
        Ok((sid, resp))
    }

    /// Close a specific browser session by ID.
    pub async fn close_session(&self, session_id: &str) {
        if let Err(e) = self.pool.close_session(session_id).await {
            warn!(session_id, error = %e, "failed to close browser session");
        }
    }

    /// Clean up idle browser instances.
    pub async fn cleanup_idle(&self) {
        self.pool.cleanup_idle().await;
    }

    /// Shut down all browser instances.
    pub async fn shutdown(&self) {
        self.pool.shutdown().await;
    }

    /// Get the number of active browser instances.
    pub async fn active_count(&self) -> usize {
        self.pool.active_count().await
    }
}

/// Validate a URL before attempting navigation.
///
/// Checks for:
/// - Valid URL structure (can be parsed)
/// - Allowed schemes (http, https)
/// - Not obviously malformed (LLM garbage in path)
fn validate_url(url: &str) -> Result<(), Error> {
    // Check if URL is empty
    if url.is_empty() {
        return Err(Error::InvalidAction("URL cannot be empty".to_string()));
    }

    // Parse the URL
    let parsed = url::Url::parse(url)
        .map_err(|e| Error::InvalidAction(format!("invalid URL '{}': {}", truncate_url(url), e)))?;

    // Check scheme
    match parsed.scheme() {
        "http" | "https" => {},
        scheme => {
            return Err(Error::InvalidAction(format!(
                "unsupported URL scheme '{}', only http/https allowed",
                scheme
            )));
        },
    }

    // Check for obviously malformed URLs (LLM garbage)
    // Check the original URL string (before normalization) to catch garbage
    let suspicious_patterns = [
        "}}}",           // JSON garbage
        "]}",            // JSON array closing
        "}<",            // Mixed JSON/XML
        "assistant to=", // LLM prompt leakage
        "functions.",    // LLM function call leakage (e.g., "functions.browser")
    ];

    for pattern in suspicious_patterns {
        if url.contains(pattern) {
            warn!(
                url = %truncate_url(url),
                pattern = pattern,
                "rejecting URL with suspicious pattern (likely LLM garbage)"
            );
            return Err(Error::InvalidAction(format!(
                "URL contains invalid characters or LLM garbage: '{}'",
                truncate_url(url)
            )));
        }
    }

    Ok(())
}

/// Truncate a URL for error messages (to avoid huge garbage URLs in logs).
fn truncate_url(url: &str) -> String {
    if url.len() > 100 {
        format!("{}...", &url[..url.floor_char_boundary(100)])
    } else {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = BrowserConfig::default();
        assert!(config.enabled);
        assert!(config.headless);
        assert_eq!(config.max_instances, 0); // 0 = unlimited, limited by memory
        assert_eq!(config.memory_limit_percent, 90);
    }

    #[test]
    fn test_browser_manager_enabled_by_default() {
        let manager = BrowserManager::default();
        assert!(manager.is_enabled());
    }

    #[test]
    fn test_validate_url_valid() {
        assert!(validate_url("https://example.com").is_ok());
        assert!(validate_url("http://localhost:8080/path").is_ok());
        assert!(validate_url("https://www.lemonde.fr/").is_ok());
    }

    #[test]
    fn test_validate_url_empty() {
        assert!(validate_url("").is_err());
    }

    #[test]
    fn test_validate_url_invalid_scheme() {
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn test_validate_url_llm_garbage() {
        // The actual garbage URL from the bug report (contains "assistant to=")
        let garbage = "https://www.lemonde.fr/path>assistant to=functions.browser";
        assert!(validate_url(garbage).is_err());

        // LLM function leakage
        assert!(validate_url("https://example.com/path/functions.browser").is_err());

        // Test with the closing brace pattern from JSON garbage
        // Note: `}}<` would match the `}<` pattern
        assert!(validate_url("https://example.com/path}}<tag").is_err());
    }

    #[test]
    fn test_validate_url_malformed() {
        assert!(validate_url("not a url").is_err());
        assert!(validate_url("://missing.scheme").is_err());
    }

    #[test]
    fn test_truncate_url_handles_multibyte_boundary() {
        let url = format!("https://{}л{}", "a".repeat(91), "tail");
        let truncated = truncate_url(&url);
        let prefix = truncated.strip_suffix("...").unwrap_or("");
        assert_eq!(prefix.len(), 99);
        assert!(!prefix.contains('л'));
        assert!(prefix.ends_with('a'));
    }

    #[test]
    fn suppresses_generic_challenge_when_page_has_real_content() {
        assert!(should_suppress_generic_challenge(
            Some(ChallengeType::GenericChallenge),
            12,
            0
        ));
        assert!(should_suppress_generic_challenge(
            Some(ChallengeType::GenericChallenge),
            0,
            120
        ));
    }

    #[test]
    fn keeps_generic_challenge_for_empty_shells() {
        assert!(!should_suppress_generic_challenge(
            Some(ChallengeType::GenericChallenge),
            0,
            0
        ));
    }

    #[tokio::test]
    async fn manager_close_session_nonexistent_is_noop() {
        let manager = BrowserManager::default();
        // Should not panic — logs a warning and returns.
        manager.close_session("nonexistent").await;
    }

    #[tokio::test]
    async fn manager_cleanup_idle_empty() {
        let manager = BrowserManager::default();
        manager.cleanup_idle().await;
        assert_eq!(manager.active_count().await, 0);
    }

    #[tokio::test]
    async fn manager_shutdown_empty() {
        let manager = BrowserManager::default();
        manager.shutdown().await;
        assert_eq!(manager.active_count().await, 0);
    }

    #[tokio::test]
    async fn cleanup_stale_session_returns_connection_closed() {
        let manager = BrowserManager::default();
        let err = manager.cleanup_stale_session("sess-42", "screenshot").await;
        assert!(
            err.is_connection_error(),
            "cleanup_stale_session must return a connection error"
        );
        let msg = err.to_string();
        assert!(msg.contains("sess-42"), "error should mention session id");
        assert!(
            msg.contains("screenshot"),
            "error should mention the action"
        );
    }
}
