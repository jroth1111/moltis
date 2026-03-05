//! Browser action types and request/response structures.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Stealth / anti-bot configuration.
///
/// These settings control the JS evasions injected before each navigation,
/// the Chrome launch flags that reduce automation signals, and the
/// behavioral (mouse/keyboard) emulation layer.
///
/// All fields default to enabled with sensible defaults. The struct is
/// always compiled regardless of the `stealth` Cargo feature; the feature
/// gate controls the injection code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StealthConfig {
    /// Master switch — disables all stealth when false.
    pub enabled: bool,
    /// Inject the 16-evasion JS script via `addScriptToEvaluateOnNewDocument`.
    pub js_evasion: bool,
    /// Add the 19 stealth Chrome launch flags.
    pub stealth_args: bool,
    /// Use Bezier mouse movement and randomised keyboard timing.
    pub behavioral: bool,
    /// Override User-Agent (default: removes `HeadlessChrome`).
    pub user_agent: Option<String>,
    /// WebGL `UNMASKED_VENDOR_WEBGL` override (default: `"Intel Inc."`).
    pub webgl_vendor: Option<String>,
    /// WebGL `UNMASKED_RENDERER_WEBGL` override (default: `"Intel Iris OpenGL Engine"`).
    pub webgl_renderer: Option<String>,
    /// `navigator.languages` override (default: `["en-US", "en"]`).
    pub languages: Option<Vec<String>>,
    /// `navigator.hardwareConcurrency` override (default: `4`).
    pub hardware_concurrency: Option<u32>,
    /// `navigator.deviceMemory` override in GiB (default: `8`).
    pub device_memory: Option<u8>,
}

impl Default for StealthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            js_evasion: true,
            stealth_args: true,
            behavioral: true,
            user_agent: None,
            webgl_vendor: None,
            webgl_renderer: None,
            languages: None,
            hardware_concurrency: None,
            device_memory: None,
        }
    }
}

/// Virtual display configuration for headful browser launches without visible UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VirtualDisplayConfig {
    /// Enable virtual display management.
    pub enabled: bool,
    /// Force non-headless launch when virtual display is enabled.
    pub force_non_headless: bool,
    /// Xvfb executable path or name.
    pub binary: String,
    /// Virtual display width in pixels.
    pub width: u32,
    /// Virtual display height in pixels.
    pub height: u32,
    /// Color depth (bits per pixel).
    pub color_depth: u8,
    /// Inclusive lower bound for display number scan.
    pub display_min: u16,
    /// Inclusive upper bound for display number scan.
    pub display_max: u16,
    /// Startup timeout for display readiness.
    pub startup_timeout_ms: u64,
}

impl Default for VirtualDisplayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            force_non_headless: false,
            binary: "Xvfb".to_string(),
            width: 2560,
            height: 1440,
            color_depth: 24,
            display_min: 99,
            display_max: 120,
            startup_timeout_ms: 3000,
        }
    }
}

/// Patchright subprocess fallback configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PatchrightFallbackConfig {
    /// Master switch for fallback behavior.
    pub enabled: bool,
    /// Python executable with Patchright installed.
    pub python_binary: String,
    /// Subprocess timeout in milliseconds.
    pub timeout_ms: u64,
    /// Whether Patchright probe should run headless.
    pub headless: bool,
    /// Challenge type allowlist (`kasada`, `imperva`, etc.).
    pub challenge_types: Vec<String>,
    /// Optional domain allowlist for fallback (empty = all).
    pub domains: Vec<String>,
    /// Number of retries for patchright probe (default: 2).
    pub max_retries: u32,
}

impl Default for PatchrightFallbackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            python_binary: "python3".to_string(),
            timeout_ms: 45_000,
            headless: true,
            challenge_types: vec!["kasada".to_string(), "imperva".to_string()],
            domains: Vec::new(),
            max_retries: 2,
        }
    }
}

/// Browser action to perform.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BrowserAction {
    /// Navigate to a URL.
    Navigate { url: String },

    /// Take a screenshot of the current page.
    Screenshot {
        #[serde(default)]
        full_page: bool,
        /// Optional: highlight element by ref before screenshot.
        #[serde(default)]
        highlight_ref: Option<u32>,
    },

    /// Get a DOM snapshot with numbered element references.
    Snapshot,

    /// Click an element by its reference number.
    Click { ref_: u32 },

    /// Type text into an element.
    Type { ref_: u32, text: String },

    /// Scroll the page or an element.
    Scroll {
        /// Element ref to scroll (None = viewport).
        #[serde(default)]
        ref_: Option<u32>,
        /// Horizontal scroll delta.
        #[serde(default)]
        x: i32,
        /// Vertical scroll delta.
        #[serde(default)]
        y: i32,
    },

    /// Execute JavaScript in the page context.
    Evaluate { code: String },

    /// Wait for an element to appear (by CSS selector or ref).
    Wait {
        #[serde(default)]
        selector: Option<String>,
        #[serde(default)]
        ref_: Option<u32>,
        #[serde(default = "default_wait_timeout_ms")]
        timeout_ms: u64,
    },

    /// Get the current page URL.
    GetUrl,

    /// Get the page title.
    GetTitle,

    /// Go back in history.
    Back,

    /// Go forward in history.
    Forward,

    /// Refresh the page.
    Refresh,

    /// Hover the mouse over an element (triggers hover CSS and JS events).
    Hover { ref_: u32 },

    /// Double-click an element (fires two click events + a `dblclick` event).
    DoubleClick { ref_: u32 },

    /// Focus an element via keyboard focus without clicking it.
    Focus { ref_: u32 },

    /// Drag from one element to another (pointer events, not HTML5 drag-and-drop).
    Drag { from_ref: u32, to_ref: u32 },

    /// Check a checkbox or radio button. No-op if already checked.
    Check { ref_: u32 },

    /// Uncheck a checkbox. No-op if already unchecked.
    Uncheck { ref_: u32 },

    /// Select an option in a `<select>` element by its value attribute.
    Select { ref_: u32, value: String },

    /// Press a named key on the focused element.
    ///
    /// Use CDP key names: `"Enter"`, `"Escape"`, `"Tab"`, `"Backspace"`,
    /// `"ArrowDown"`, `"ArrowUp"`, `"F5"`, single chars `"a"`, `"1"`, etc.
    Press { key: String },

    /// Upload a file to a `<input type="file">` element.
    ///
    /// `path` must be an absolute path on the machine running the browser.
    Upload { ref_: u32, path: String },

    /// Clear the value of an input or textarea element.
    ///
    /// Uses the native value setter so React's synthetic event system detects
    /// the change, then fires `input` + `change` events.
    Clear { ref_: u32 },

    // ── Phase 5: Network interception & HAR ────────────────────────────────
    /// Enable CDP `Fetch` domain to intercept matching requests.
    ///
    /// `url_patterns` is a list of URL wildcard patterns (e.g. `["*api*"]`).
    /// An empty list intercepts all requests.
    InterceptRequests {
        #[serde(default)]
        url_patterns: Vec<String>,
    },

    /// Disable request interception (stops a running intercept + HAR session).
    StopIntercept,

    /// Inject extra HTTP headers into every subsequent intercepted request.
    SetExtraHeaders {
        headers: std::collections::HashMap<String, String>,
    },

    /// Begin accumulating network requests into a HAR 1.2 log.
    StartHar,

    /// Stop HAR recording and return the captured HAR JSON in `response.result`.
    StopHar,

    // ── Phase 6: Session state ─────────────────────────────────────────────
    /// Capture cookies + storage and save them to disk.
    SaveState {
        name: String,
        #[serde(default)]
        encrypt: bool,
    },

    /// Load a previously saved session state and restore cookies + storage.
    LoadState { name: String },

    /// List saved session state names (returned in `response.result`).
    ListStates,

    /// Delete a saved session state by name.
    DeleteState { name: String },

    // ── Phase 7a: Emulation overrides ──────────────────────────────────────
    /// Override viewport size and device emulation.
    SetDevice {
        width: u32,
        height: u32,
        #[serde(default = "default_device_scale_factor")]
        device_scale_factor: f64,
        #[serde(default)]
        mobile: bool,
    },

    /// Override the GPS location reported to the page.
    SetGeolocation {
        latitude: f64,
        longitude: f64,
        #[serde(default = "default_geo_accuracy")]
        accuracy: f64,
    },

    /// Override the timezone reported to the page.
    SetTimezone { timezone_id: String },

    /// Override the locale reported to the page.
    SetLocale { locale: String },

    /// Clear any active device-metrics override and restore original viewport.
    ClearDevice,

    // ── Phase 7b: Screencast ───────────────────────────────────────────────
    /// Start streaming page frames via the CDP screencast API.
    StartScreencast {
        #[serde(default = "default_screencast_format")]
        format: String,
        #[serde(default = "default_screencast_quality")]
        quality: u8,
        #[serde(default = "default_every_nth")]
        every_nth: u32,
    },

    /// Stop the active screencast session.
    StopScreencast,

    /// Retrieve the most recent screencast frame as a base64 image in `response.result`.
    GetScreencastFrame,

    // ── Phase 7c: Tab management ───────────────────────────────────────────
    /// Open a new browser tab with the given name and switch to it.
    TabNew { name: String },

    /// List all open tab names (returned in `response.result`).
    TabList,

    /// Switch the active tab to `name`.
    TabSwitch { name: String },

    /// Close the tab named `name` (cannot close `"main"`).
    TabClose { name: String },

    /// Close the browser session.
    Close,
}

fn default_device_scale_factor() -> f64 {
    1.0
}

fn default_geo_accuracy() -> f64 {
    1.0
}

fn default_screencast_format() -> String {
    "jpeg".to_string()
}

fn default_screencast_quality() -> u8 {
    80
}

fn default_every_nth() -> u32 {
    1
}

fn default_wait_timeout_ms() -> u64 {
    30000
}

/// Known Chromium-family browser engines we can launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserKind {
    Chrome,
    Chromium,
    Edge,
    Brave,
    Opera,
    Vivaldi,
    Arc,
    Custom,
}

impl BrowserKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Chromium => "chromium",
            Self::Edge => "edge",
            Self::Brave => "brave",
            Self::Opera => "opera",
            Self::Vivaldi => "vivaldi",
            Self::Arc => "arc",
            Self::Custom => "custom",
        }
    }
}

impl fmt::Display for BrowserKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Preferred browser for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BrowserPreference {
    #[default]
    Auto,
    Chrome,
    Chromium,
    Edge,
    Brave,
    Opera,
    Vivaldi,
    Arc,
}

impl BrowserPreference {
    pub fn preferred_kind(self) -> Option<BrowserKind> {
        match self {
            Self::Auto => None,
            Self::Chrome => Some(BrowserKind::Chrome),
            Self::Chromium => Some(BrowserKind::Chromium),
            Self::Edge => Some(BrowserKind::Edge),
            Self::Brave => Some(BrowserKind::Brave),
            Self::Opera => Some(BrowserKind::Opera),
            Self::Vivaldi => Some(BrowserKind::Vivaldi),
            Self::Arc => Some(BrowserKind::Arc),
        }
    }
}

impl fmt::Display for BrowserPreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.preferred_kind() {
            Some(kind) => kind.fmt(f),
            None => f.write_str("auto"),
        }
    }
}

impl fmt::Display for BrowserAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Navigate { url } => write!(f, "navigate({})", url),
            Self::Screenshot { full_page, .. } => {
                if *full_page {
                    write!(f, "screenshot(full_page)")
                } else {
                    write!(f, "screenshot")
                }
            },
            Self::Snapshot => write!(f, "snapshot"),
            Self::Click { ref_ } => write!(f, "click(ref={})", ref_),
            Self::Type { ref_, .. } => write!(f, "type(ref={})", ref_),
            Self::Scroll { ref_, x, y } => match ref_ {
                Some(r) => write!(f, "scroll(ref={}, x={}, y={})", r, x, y),
                None => write!(f, "scroll(x={}, y={})", x, y),
            },
            Self::Evaluate { .. } => write!(f, "evaluate"),
            Self::Wait { selector, ref_, .. } => match (selector, ref_) {
                (Some(s), _) => write!(f, "wait(selector={})", s),
                (_, Some(r)) => write!(f, "wait(ref={})", r),
                _ => write!(f, "wait"),
            },
            Self::GetUrl => write!(f, "get_url"),
            Self::GetTitle => write!(f, "get_title"),
            Self::Back => write!(f, "back"),
            Self::Forward => write!(f, "forward"),
            Self::Refresh => write!(f, "refresh"),
            Self::Hover { ref_ } => write!(f, "hover(ref={ref_})"),
            Self::DoubleClick { ref_ } => write!(f, "double_click(ref={ref_})"),
            Self::Focus { ref_ } => write!(f, "focus(ref={ref_})"),
            Self::Drag { from_ref, to_ref } => {
                write!(f, "drag(from={from_ref}, to={to_ref})")
            },
            Self::Check { ref_ } => write!(f, "check(ref={ref_})"),
            Self::Uncheck { ref_ } => write!(f, "uncheck(ref={ref_})"),
            Self::Select { ref_, .. } => write!(f, "select(ref={ref_})"),
            Self::Press { key } => write!(f, "press(key={key})"),
            Self::Upload { ref_, .. } => write!(f, "upload(ref={ref_})"),
            Self::Clear { ref_ } => write!(f, "clear(ref={ref_})"),
            Self::InterceptRequests { url_patterns } => {
                write!(f, "intercept_requests(patterns={})", url_patterns.len())
            },
            Self::StopIntercept => write!(f, "stop_intercept"),
            Self::SetExtraHeaders { headers } => {
                write!(f, "set_extra_headers(count={})", headers.len())
            },
            Self::StartHar => write!(f, "start_har"),
            Self::StopHar => write!(f, "stop_har"),
            Self::SaveState { name, .. } => write!(f, "save_state(name={name})"),
            Self::LoadState { name } => write!(f, "load_state(name={name})"),
            Self::ListStates => write!(f, "list_states"),
            Self::DeleteState { name } => write!(f, "delete_state(name={name})"),
            Self::SetDevice { width, height, .. } => {
                write!(f, "set_device({width}x{height})")
            },
            Self::SetGeolocation {
                latitude,
                longitude,
                ..
            } => {
                write!(f, "set_geolocation({latitude},{longitude})")
            },
            Self::SetTimezone { timezone_id } => write!(f, "set_timezone({timezone_id})"),
            Self::SetLocale { locale } => write!(f, "set_locale({locale})"),
            Self::ClearDevice => write!(f, "clear_device"),
            Self::StartScreencast { format, .. } => {
                write!(f, "start_screencast(format={format})")
            },
            Self::StopScreencast => write!(f, "stop_screencast"),
            Self::GetScreencastFrame => write!(f, "get_screencast_frame"),
            Self::TabNew { name } => write!(f, "tab_new(name={name})"),
            Self::TabList => write!(f, "tab_list"),
            Self::TabSwitch { name } => write!(f, "tab_switch(name={name})"),
            Self::TabClose { name } => write!(f, "tab_close(name={name})"),
            Self::Close => write!(f, "close"),
        }
    }
}

/// Request to the browser service.
#[derive(Debug, Clone, Deserialize)]
pub struct BrowserRequest {
    /// Browser session ID (optional - creates new if missing).
    #[serde(default)]
    pub session_id: Option<String>,

    /// The action to perform.
    #[serde(flatten)]
    pub action: BrowserAction,

    /// Global timeout in milliseconds.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Whether to run in sandbox mode (Docker container).
    /// If None, uses host mode (no sandbox).
    #[serde(default)]
    pub sandbox: Option<bool>,

    /// Optional browser preference for host mode.
    /// - "auto" (default): first detected installed browser
    /// - specific browser ("brave", "chrome", etc): use that browser
    #[serde(default)]
    pub browser: Option<BrowserPreference>,
}

fn default_timeout_ms() -> u64 {
    60000
}

/// Element reference in a DOM snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct ElementRef {
    /// Unique reference number for this element.
    pub ref_: u32,
    /// Tag name (e.g., "button", "input", "a").
    pub tag: String,
    /// Element's role attribute or inferred role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Visible text content (truncated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Link href (for anchor elements).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    /// Input placeholder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Input value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// aria-label attribute.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aria_label: Option<String>,
    /// Whether the element is visible in the viewport.
    pub visible: bool,
    /// Whether the element is interactive (clickable/editable).
    pub interactive: bool,
    /// Checked state for checkboxes and radio buttons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    /// Whether the element is disabled.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Input type attribute ("text", "email", "password", "submit", etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_type: Option<String>,
    /// Bounding box in viewport coordinates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<ElementBounds>,
}

/// Bounding box for an element.
#[derive(Debug, Clone, Serialize)]
pub struct ElementBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// DOM snapshot with element references.
#[derive(Debug, Clone, Serialize)]
pub struct DomSnapshot {
    /// Current page URL.
    pub url: String,
    /// Page title.
    pub title: String,
    /// Page text content (body innerText, truncated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Interactive elements with reference numbers.
    pub elements: Vec<ElementRef>,
    /// Viewport dimensions.
    pub viewport: ViewportSize,
    /// Total page scroll dimensions.
    pub scroll: ScrollDimensions,
}

/// Viewport size.
#[derive(Debug, Clone, Serialize)]
pub struct ViewportSize {
    pub width: u32,
    pub height: u32,
}

/// Scroll dimensions.
#[derive(Debug, Clone, Serialize)]
pub struct ScrollDimensions {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Response from a browser action.
#[derive(Debug, Clone, Serialize)]
pub struct BrowserResponse {
    /// Whether the action succeeded.
    pub success: bool,

    /// Session ID for this browser instance.
    pub session_id: String,

    /// Whether the browser is running in a sandboxed container.
    pub sandboxed: bool,

    /// Error message if action failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,

    /// Screenshot as base64 PNG (for screenshot action).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,

    /// Device scale factor used for the screenshot (for proper display sizing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot_scale: Option<f64>,

    /// DOM snapshot (for snapshot action).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<DomSnapshot>,

    /// JavaScript evaluation result (for evaluate action).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,

    /// Current URL (for navigate, get_url, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Final URL after navigation resolution/redirects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,

    /// Page title (for get_title, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Title length captured during navigation diagnostics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_len: Option<u64>,

    /// Body text length captured during navigation diagnostics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_text_len: Option<u64>,

    /// Classified challenge type for challenge/interstitial pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub challenge_type: Option<String>,

    /// Matched challenge markers found in page HTML.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub challenge_markers: Option<Vec<String>>,

    /// Duration of the action in milliseconds.
    pub duration_ms: u64,
}

impl BrowserResponse {
    pub fn success(session_id: String, duration_ms: u64, sandboxed: bool) -> Self {
        Self {
            success: true,
            session_id,
            sandboxed,
            error: None,
            screenshot: None,
            screenshot_scale: None,
            snapshot: None,
            result: None,
            url: None,
            final_url: None,
            title: None,
            title_len: None,
            body_text_len: None,
            challenge_type: None,
            challenge_markers: None,
            duration_ms,
        }
    }

    pub fn error(session_id: String, error: impl Into<String>, duration_ms: u64) -> Self {
        Self {
            success: false,
            session_id,
            sandboxed: false,
            error: Some(error.into()),
            screenshot: None,
            screenshot_scale: None,
            snapshot: None,
            result: None,
            url: None,
            final_url: None,
            title: None,
            title_len: None,
            body_text_len: None,
            challenge_type: None,
            challenge_markers: None,
            duration_ms,
        }
    }

    pub fn with_screenshot(mut self, screenshot: String, scale: f64) -> Self {
        self.screenshot = Some(screenshot);
        self.screenshot_scale = Some(scale);
        self
    }

    pub fn with_snapshot(mut self, snapshot: DomSnapshot) -> Self {
        self.snapshot = Some(snapshot);
        self
    }

    pub fn with_result(mut self, result: serde_json::Value) -> Self {
        self.result = Some(result);
        self
    }

    pub fn with_url(mut self, url: String) -> Self {
        self.url = Some(url);
        self
    }

    pub fn with_title(mut self, title: String) -> Self {
        self.title = Some(title);
        self
    }

    pub fn with_navigation_diagnostics(
        mut self,
        final_url: String,
        title_len: usize,
        body_text_len: usize,
        challenge_type: Option<String>,
        challenge_markers: Vec<String>,
    ) -> Self {
        self.final_url = Some(final_url);
        self.title_len = Some(title_len as u64);
        self.body_text_len = Some(body_text_len as u64);
        self.challenge_type = challenge_type;
        self.challenge_markers = if challenge_markers.is_empty() {
            None
        } else {
            Some(challenge_markers)
        };
        self
    }
}

/// Browser configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    /// Whether browser support is enabled.
    pub enabled: bool,
    /// Path to Chrome/Chromium binary (auto-detected if not set).
    pub chrome_path: Option<String>,
    /// Whether to run in headless mode.
    pub headless: bool,
    /// Default viewport width.
    pub viewport_width: u32,
    /// Default viewport height.
    pub viewport_height: u32,
    /// Device scale factor for HiDPI/Retina displays.
    pub device_scale_factor: f64,
    /// Maximum concurrent browser instances (0 = unlimited, limited by memory).
    pub max_instances: usize,
    /// System memory usage threshold (0-100) above which new instances are blocked.
    /// Default is 90 (block new instances when memory > 90% used).
    pub memory_limit_percent: u8,
    /// Instance idle timeout in seconds before closing.
    pub idle_timeout_secs: u64,
    /// Default navigation timeout in milliseconds.
    pub navigation_timeout_ms: u64,
    /// User agent string (uses default if not set).
    pub user_agent: Option<String>,
    /// Additional Chrome arguments.
    #[serde(default)]
    pub chrome_args: Vec<String>,
    /// Docker image to use for sandboxed browser.
    /// Sandbox mode is controlled per-session via the request, not globally.
    #[serde(default = "default_sandbox_image")]
    pub sandbox_image: String,
    /// Container name prefix for sandboxed browser instances.
    #[serde(default = "default_container_prefix")]
    pub container_prefix: String,
    /// Allowed domains for navigation (empty = all allowed).
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Total system RAM threshold (MB) below which memory-saving Chrome flags
    /// are injected automatically. Set to 0 to disable. Default: 2048.
    pub low_memory_threshold_mb: u64,
    /// Whether to persist the Chrome user profile across sessions.
    pub persist_profile: bool,
    /// Custom path for the persistent Chrome profile directory.
    pub profile_dir: Option<String>,
    /// Virtual display settings (Linux/Xvfb).
    pub virtual_display: VirtualDisplayConfig,
    /// Patchright fallback settings.
    pub patchright_fallback: PatchrightFallbackConfig,
    /// Stealth / anti-bot-detection configuration.
    pub stealth: StealthConfig,
}

fn default_sandbox_image() -> String {
    "browserless/chrome".to_string()
}

fn default_container_prefix() -> String {
    "moltis-browser".to_string()
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            chrome_path: None,
            headless: true,
            viewport_width: 2560,
            viewport_height: 1440,
            device_scale_factor: 2.0,
            max_instances: 0, // 0 = unlimited, limited by memory
            memory_limit_percent: 90,
            idle_timeout_secs: 300,
            navigation_timeout_ms: 30000,
            user_agent: None,
            chrome_args: Vec::new(),
            sandbox_image: default_sandbox_image(),
            container_prefix: default_container_prefix(),
            allowed_domains: Vec::new(),
            low_memory_threshold_mb: 2048,
            persist_profile: true,
            profile_dir: None,
            virtual_display: VirtualDisplayConfig::default(),
            patchright_fallback: PatchrightFallbackConfig::default(),
            stealth: StealthConfig::default(),
        }
    }
}

impl BrowserConfig {
    /// Resolve the effective Chrome profile directory, if profile persistence is enabled.
    ///
    /// Returns `Some(path)` when either `profile_dir` is set or `persist_profile` is true.
    /// Returns `None` when profiles should be ephemeral.
    #[must_use]
    pub fn resolved_profile_dir(&self) -> Option<std::path::PathBuf> {
        if let Some(ref dir) = self.profile_dir {
            Some(std::path::PathBuf::from(dir))
        } else if self.persist_profile {
            Some(moltis_config::data_dir().join("browser").join("profile"))
        } else {
            None
        }
    }
}

impl From<&moltis_config::schema::BrowserConfig> for BrowserConfig {
    fn from(cfg: &moltis_config::schema::BrowserConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            chrome_path: cfg.chrome_path.clone(),
            headless: cfg.headless,
            viewport_width: cfg.viewport_width,
            viewport_height: cfg.viewport_height,
            device_scale_factor: cfg.device_scale_factor,
            max_instances: cfg.max_instances,
            memory_limit_percent: cfg.memory_limit_percent,
            idle_timeout_secs: cfg.idle_timeout_secs,
            navigation_timeout_ms: cfg.navigation_timeout_ms,
            user_agent: cfg.user_agent.clone(),
            chrome_args: cfg.chrome_args.clone(),
            sandbox_image: cfg.sandbox_image.clone(),
            container_prefix: default_container_prefix(),
            allowed_domains: cfg.allowed_domains.clone(),
            low_memory_threshold_mb: cfg.low_memory_threshold_mb,
            persist_profile: cfg.persist_profile,
            profile_dir: cfg.profile_dir.clone(),
            virtual_display: VirtualDisplayConfig {
                enabled: cfg.virtual_display.enabled,
                force_non_headless: cfg.virtual_display.force_non_headless,
                binary: cfg.virtual_display.binary.clone(),
                width: cfg.virtual_display.width,
                height: cfg.virtual_display.height,
                color_depth: cfg.virtual_display.color_depth,
                display_min: cfg.virtual_display.display_min,
                display_max: cfg.virtual_display.display_max,
                startup_timeout_ms: cfg.virtual_display.startup_timeout_ms,
            },
            patchright_fallback: PatchrightFallbackConfig {
                enabled: cfg.patchright_fallback.enabled,
                python_binary: cfg.patchright_fallback.python_binary.clone(),
                timeout_ms: cfg.patchright_fallback.timeout_ms,
                headless: cfg.patchright_fallback.headless,
                challenge_types: cfg.patchright_fallback.challenge_types.clone(),
                domains: cfg.patchright_fallback.domains.clone(),
                max_retries: cfg.patchright_fallback.max_retries,
            },
            stealth: StealthConfig {
                enabled: cfg.stealth.enabled,
                js_evasion: cfg.stealth.js_evasion,
                stealth_args: cfg.stealth.stealth_args,
                behavioral: cfg.stealth.behavioral,
                user_agent: cfg.stealth.user_agent.clone(),
                webgl_vendor: cfg.stealth.webgl_vendor.clone(),
                webgl_renderer: cfg.stealth.webgl_renderer.clone(),
                languages: cfg.stealth.languages.clone(),
                hardware_concurrency: cfg.stealth.hardware_concurrency,
                device_memory: cfg.stealth.device_memory,
            },
        }
    }
}

/// Check if a URL is allowed based on the allowed domains list.
/// Returns true if allowed, false if blocked.
pub fn is_domain_allowed(url: &str, allowed_domains: &[String]) -> bool {
    if allowed_domains.is_empty() {
        return true; // No restrictions
    }

    let Ok(parsed) = url::Url::parse(url) else {
        return false; // Invalid URL, block it
    };

    let Some(host) = parsed.host_str() else {
        return false; // No host, block it
    };

    for pattern in allowed_domains {
        if pattern.starts_with("*.") {
            // Wildcard: *.example.com matches foo.example.com, bar.example.com
            let suffix = &pattern[1..]; // .example.com
            if host.ends_with(suffix) || host == &pattern[2..] {
                return true;
            }
        } else if host == pattern {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_domain_allowed_empty_list() {
        // Empty allowed_domains means all domains are allowed
        assert!(is_domain_allowed("https://example.com", &[]));
        assert!(is_domain_allowed("https://evil.com", &[]));
    }

    #[test]
    fn test_domain_allowed_exact_match() {
        let allowed = vec!["example.com".to_string()];
        assert!(is_domain_allowed("https://example.com/path", &allowed));
        assert!(!is_domain_allowed("https://other.com", &allowed));
        assert!(!is_domain_allowed("https://sub.example.com", &allowed));
    }

    #[test]
    fn test_domain_allowed_wildcard() {
        let allowed = vec!["*.example.com".to_string()];
        assert!(is_domain_allowed("https://sub.example.com", &allowed));
        assert!(is_domain_allowed("https://foo.bar.example.com", &allowed));
        // Wildcard also matches the base domain
        assert!(is_domain_allowed("https://example.com", &allowed));
        assert!(!is_domain_allowed("https://notexample.com", &allowed));
    }

    #[test]
    fn test_domain_allowed_multiple() {
        let allowed = vec!["example.com".to_string(), "*.trusted.org".to_string()];
        assert!(is_domain_allowed("https://example.com", &allowed));
        assert!(is_domain_allowed("https://sub.trusted.org", &allowed));
        assert!(!is_domain_allowed("https://evil.com", &allowed));
    }

    #[test]
    fn test_domain_allowed_invalid_url() {
        let allowed = vec!["example.com".to_string()];
        assert!(!is_domain_allowed("not-a-url", &allowed));
        assert!(!is_domain_allowed("", &allowed));
    }

    #[test]
    fn test_browser_preference_default_is_auto() {
        assert_eq!(BrowserPreference::default(), BrowserPreference::Auto);
    }

    #[test]
    fn test_browser_preference_deserialize() {
        let value: BrowserPreference = match serde_json::from_str("\"brave\"") {
            Ok(value) => value,
            Err(error) => panic!("failed to deserialize browser preference: {error}"),
        };
        assert_eq!(value, BrowserPreference::Brave);
    }

    #[test]
    fn resolved_profile_dir_returns_path_by_default() {
        // Default config has persist_profile = true
        let config = BrowserConfig::default();
        let dir = config.resolved_profile_dir();
        assert!(dir.is_some());
        let path = dir.unwrap_or_default();
        assert!(path.ends_with("browser/profile"));
    }

    #[test]
    fn resolved_profile_dir_returns_none_when_disabled() {
        let config = BrowserConfig {
            persist_profile: false,
            ..BrowserConfig::default()
        };
        assert!(config.resolved_profile_dir().is_none());
    }

    #[test]
    fn resolved_profile_dir_uses_custom_path() {
        let config = BrowserConfig {
            profile_dir: Some("/custom/path".to_string()),
            ..BrowserConfig::default()
        };
        let dir = config.resolved_profile_dir();
        assert_eq!(dir, Some(std::path::PathBuf::from("/custom/path")));
    }

    #[test]
    fn resolved_profile_dir_custom_overrides_persist_flag() {
        let config = BrowserConfig {
            persist_profile: false,
            profile_dir: Some("/override".to_string()),
            ..BrowserConfig::default()
        };
        // profile_dir takes precedence, implicitly enabling persistence
        assert!(config.resolved_profile_dir().is_some());
    }
}
