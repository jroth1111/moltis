//! Browser automation tool for LLM agents.
//!
//! This tool provides full browser automation capabilities including:
//! - Navigation with JavaScript execution
//! - Screenshots of pages
//! - DOM snapshots with numbered element references
//! - Clicking, typing, scrolling on elements
//! - JavaScript evaluation

use {
    crate::sandbox::SandboxRouter,
    async_trait::async_trait,
    moltis_agents::tool_registry::{AgentTool, ToolEffectClass},
    moltis_browser::{BrowserManager, BrowserRequest},
    std::sync::Arc,
    tokio::sync::RwLock,
    tracing::debug,
};

use crate::error::Error;

/// Browser automation tool for interacting with web pages.
///
/// Unlike `web_fetch` which just retrieves page content, this tool allows
/// full browser interaction: clicking buttons, filling forms, taking
/// screenshots, and executing JavaScript.
///
/// This tool automatically tracks and reuses browser session IDs. When
/// the LLM doesn't provide a session_id (or provides empty string), the
/// tool will use the most recently created session. This prevents pool
/// exhaustion from creating new browser instances on every call.
pub struct BrowserTool {
    manager: Arc<BrowserManager>,
    sandbox_router: Option<Arc<SandboxRouter>>,
    /// Track the most recent session ID for automatic reuse.
    /// This prevents pool exhaustion when the LLM forgets to pass session_id.
    last_session_id: RwLock<Option<String>>,
}

impl BrowserTool {
    /// Create a new browser tool wrapping a browser manager.
    pub fn new(manager: Arc<BrowserManager>) -> Self {
        Self {
            manager,
            sandbox_router: None,
            last_session_id: RwLock::new(None),
        }
    }

    /// Attach a sandbox router for per-session sandbox mode resolution.
    pub fn with_sandbox_router(mut self, router: Arc<SandboxRouter>) -> Self {
        self.sandbox_router = Some(router);
        self
    }

    /// Create from config; returns `None` if browser is disabled.
    pub fn from_config(config: &moltis_config::schema::BrowserConfig) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        let browser_config = moltis_browser::BrowserConfig::from(config);
        let manager = Arc::new(BrowserManager::new(browser_config));
        Some(Self::new(manager))
    }

    /// Clear the tracked session ID (e.g., after explicit close).
    async fn clear_session(&self) {
        let mut guard = self.last_session_id.write().await;
        *guard = None;
    }

    /// Save the session ID for future reuse.
    async fn save_session(&self, session_id: &str) {
        if !session_id.is_empty() {
            let mut guard = self.last_session_id.write().await;
            *guard = Some(session_id.to_string());
        }
    }

    /// Get the tracked session ID if available.
    async fn get_saved_session(&self) -> Option<String> {
        let guard = self.last_session_id.read().await;
        guard.clone()
    }
}

#[async_trait]
impl AgentTool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "Control a real browser to interact with web pages.\n\n\
         USE THIS TOOL when the user says 'browse', 'browser', 'open in browser', \
         or needs interaction (clicking, forms, screenshots, JavaScript-heavy pages).\n\n\
         REQUIRED: You MUST specify an 'action' parameter. Example:\n\
         {\"action\": \"navigate\", \"url\": \"https://example.com\"}\n\n\
         Core actions: navigate, screenshot, snapshot, click, type, scroll, evaluate, wait\n\
         Extended actions: hover, double_click, focus, drag, check, uncheck, select, press, upload, clear\n\
         Navigation: back, forward, refresh, get_url, get_title, close\n\
         Network: intercept_requests, stop_intercept, set_extra_headers, start_har, stop_har\n\
         Session: save_state, load_state, list_states, delete_state\n\
         Emulation: set_device, set_geolocation, set_timezone, set_locale, clear_device\n\
         Screencast: start_screencast, stop_screencast, get_screencast_frame\n\
         Tabs: tab_new, tab_list, tab_switch, tab_close\n\n\
         BROWSER CHOICE: optionally set \"browser\" to choose one (auto, chrome, chromium, \
         edge, brave, opera, vivaldi, arc). If no browser is installed, Moltis will try \
         to auto-install one.\n\n\
         SESSION: The browser session is automatically tracked. After 'navigate', \
         subsequent actions will reuse the same browser. No need to pass session_id.\n\n\
         WORKFLOW:\n\
         1. {\"action\": \"navigate\", \"url\": \"...\"} - opens URL in browser\n\
         2. {\"action\": \"snapshot\"} - get interactive elements with ref numbers\n\
         3. {\"action\": \"click\", \"ref_\": N} - click element by ref number\n\
         4. {\"action\": \"screenshot\"} - capture the current view\n\
         5. {\"action\": \"close\"} - close the browser when done\n\n\
         EXTENDED EXAMPLES:\n\
         - {\"action\": \"hover\", \"ref_\": N} - move mouse over element\n\
         - {\"action\": \"double_click\", \"ref_\": N} - double-click element\n\
         - {\"action\": \"drag\", \"from_ref\": N, \"to_ref\": M} - drag between elements\n\
         - {\"action\": \"check\", \"ref_\": N} - check a checkbox\n\
         - {\"action\": \"select\", \"ref_\": N, \"value\": \"option-value\"} - select dropdown option\n\
         - {\"action\": \"press\", \"key\": \"Enter\"} - press a key (Enter/Escape/Tab/Arrow*)\n\
         - {\"action\": \"upload\", \"ref_\": N, \"path\": \"/abs/path/file.pdf\"} - file upload\n\
         - {\"action\": \"clear\", \"ref_\": N} - clear an input field\n\
         - {\"action\": \"intercept_requests\", \"url_patterns\": [\"*api*\"]} - intercept network requests\n\
         - {\"action\": \"start_har\"} / {\"action\": \"stop_har\"} - record network as HAR 1.2\n\
         - {\"action\": \"save_state\", \"name\": \"mysession\"} - persist cookies+storage\n\
         - {\"action\": \"set_device\", \"width\": 375, \"height\": 812, \"mobile\": true} - emulate device\n\
         - {\"action\": \"tab_new\", \"tab_name\": \"sidebar\"} - open new tab"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "navigate", "screenshot", "snapshot",
                        "click", "double_click", "hover",
                        "type", "clear",
                        "scroll", "evaluate", "wait",
                        "focus", "drag",
                        "check", "uncheck", "select",
                        "press", "upload",
                        "get_url", "get_title", "back", "forward", "refresh", "close",
                        "intercept_requests", "stop_intercept", "set_extra_headers",
                        "start_har", "stop_har",
                        "save_state", "load_state", "list_states", "delete_state",
                        "set_device", "set_geolocation", "set_timezone", "set_locale", "clear_device",
                        "start_screencast", "stop_screencast", "get_screencast_frame",
                        "tab_new", "tab_list", "tab_switch", "tab_close"
                    ],
                    "description": "REQUIRED. The browser action to perform. Use 'navigate' with 'url' to open a page, 'snapshot' to see elements, 'screenshot' to capture."
                },
                "session_id": {
                    "type": "string",
                    "description": "Browser session ID (omit to create new session, or reuse existing)"
                },
                "browser": {
                    "type": "string",
                    "enum": ["auto", "chrome", "chromium", "edge", "brave", "opera", "vivaldi", "arc"],
                    "description": "Browser to use for host mode. Default: auto (first installed browser)."
                },
                "url": {
                    "type": "string",
                    "description": "URL to navigate to (for 'navigate' action)"
                },
                "ref_": {
                    "type": "integer",
                    "description": "Element reference number from snapshot (for click/type/hover/double_click/focus/check/uncheck/select/upload/clear/scroll)"
                },
                "from_ref": {
                    "type": "integer",
                    "description": "Source element reference number (for 'drag' action)"
                },
                "to_ref": {
                    "type": "integer",
                    "description": "Destination element reference number (for 'drag' action)"
                },
                "text": {
                    "type": "string",
                    "description": "Text to type (for 'type' action)"
                },
                "value": {
                    "type": "string",
                    "description": "Option value to select (for 'select' action — matches the HTML value attribute)"
                },
                "key": {
                    "type": "string",
                    "description": "Key name to press (for 'press' action). Examples: Enter, Escape, Tab, Backspace, ArrowDown, F5, or a single character like 'a'"
                },
                "path": {
                    "type": "string",
                    "description": "Absolute file path to upload (for 'upload' action)"
                },
                "code": {
                    "type": "string",
                    "description": "JavaScript code to execute (for 'evaluate' action)"
                },
                "x": {
                    "type": "integer",
                    "description": "Horizontal scroll pixels (for 'scroll' action)"
                },
                "y": {
                    "type": "integer",
                    "description": "Vertical scroll pixels (for 'scroll' action)"
                },
                "full_page": {
                    "type": "boolean",
                    "description": "Capture full page screenshot vs viewport only"
                },
                "selector": {
                    "type": "string",
                    "description": "CSS selector to wait for (for 'wait' action)"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default: 60000)"
                },
                "url_patterns": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "URL glob patterns to intercept (empty = all requests). For 'intercept_requests'."
                },
                "headers": {
                    "type": "object",
                    "description": "HTTP headers map to inject into intercepted requests. For 'set_extra_headers'."
                },
                "name": {
                    "type": "string",
                    "description": "Name for save_state/load_state/delete_state operations."
                },
                "encrypt": {
                    "type": "boolean",
                    "description": "Encrypt the saved session state on disk. For 'save_state'."
                },
                "width": {
                    "type": "integer",
                    "description": "Viewport width in CSS pixels. For 'set_device'."
                },
                "height": {
                    "type": "integer",
                    "description": "Viewport height in CSS pixels. For 'set_device'."
                },
                "device_scale_factor": {
                    "type": "number",
                    "description": "Device pixel ratio (e.g. 2.0 for Retina). For 'set_device'."
                },
                "mobile": {
                    "type": "boolean",
                    "description": "Emulate mobile device touch events. For 'set_device'."
                },
                "latitude": {
                    "type": "number",
                    "description": "GPS latitude in degrees. For 'set_geolocation'."
                },
                "longitude": {
                    "type": "number",
                    "description": "GPS longitude in degrees. For 'set_geolocation'."
                },
                "accuracy": {
                    "type": "number",
                    "description": "Geolocation accuracy radius in metres. For 'set_geolocation'."
                },
                "timezone_id": {
                    "type": "string",
                    "description": "IANA timezone identifier (e.g. 'America/New_York'). For 'set_timezone'."
                },
                "locale": {
                    "type": "string",
                    "description": "BCP-47 locale string (e.g. 'fr-FR'). For 'set_locale'."
                },
                "format": {
                    "type": "string",
                    "enum": ["jpeg", "png"],
                    "description": "Image format for screencast frames. For 'start_screencast'."
                },
                "quality": {
                    "type": "integer",
                    "description": "JPEG quality 0–100 for screencast. For 'start_screencast'."
                },
                "every_nth": {
                    "type": "integer",
                    "description": "Deliver every Nth screencast frame (1 = every frame). For 'start_screencast'."
                },
                "tab_name": {
                    "type": "string",
                    "description": "Tab identifier name. For 'tab_new', 'tab_switch', 'tab_close'."
                }
            }
        })
    }

    fn side_effect_class(&self) -> ToolEffectClass {
        ToolEffectClass::ExternalEffect
    }

    async fn execute(&self, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let mut params = params;

        // Browser sandbox mode follows the session sandbox mode from the shared router.
        let session_key = params
            .get("_session_key")
            .and_then(|v| v.as_str())
            .unwrap_or("main");
        let sandbox_mode = if let Some(ref router) = self.sandbox_router {
            router.is_sandboxed(session_key).await
        } else {
            debug!(
                session_key,
                "browser running in host mode (no container backend)"
            );
            false
        };

        // Inject saved session_id if LLM didn't provide one (or provided empty string)
        if let Some(obj) = params.as_object_mut() {
            let needs_session = match obj.get("session_id") {
                None => true,
                Some(serde_json::Value::String(s)) if s.is_empty() => true,
                Some(serde_json::Value::Null) => true,
                _ => false,
            };

            if needs_session && let Some(saved_sid) = self.get_saved_session().await {
                debug!(
                    session_id = %saved_sid,
                    "injecting saved session_id (LLM didn't provide one)"
                );
                obj.insert("session_id".to_string(), serde_json::json!(saved_sid));
            }

            // Inject sandbox mode from session context
            obj.insert("sandbox".to_string(), serde_json::json!(sandbox_mode));
        }

        // Check if this is a "close" action - we'll clear saved session after
        let is_close = params
            .get("action")
            .and_then(|a| a.as_str())
            .is_some_and(|a| a == "close");

        // Try to parse the request, defaulting to "navigate" if action is missing
        let request: BrowserRequest = match serde_json::from_value(params.clone()) {
            Ok(req) => req,
            Err(e) if e.to_string().contains("missing field `action`") => {
                // Default to navigate action if action is missing but url is present
                if let Some(obj) = params.as_object_mut() {
                    if obj.contains_key("url") {
                        obj.insert("action".to_string(), serde_json::json!("navigate"));
                        serde_json::from_value(params)?
                    } else {
                        // No URL either - return helpful error
                        return Err(Error::message(
                            "Missing required 'action' field. Use: \
                             {\"action\": \"navigate\", \"url\": \"https://...\"} to open a page",
                        )
                        .into());
                    }
                } else {
                    return Err(e.into());
                }
            },
            Err(e) => return Err(e.into()),
        };

        let response = self.manager.handle_request(request).await;

        // Track the session ID for future reuse
        if response.success {
            if is_close {
                self.clear_session().await;
            } else {
                self.save_session(&response.session_id).await;
            }
        }

        Ok(serde_json::to_value(&response)?)
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_name() {
        let config = moltis_config::schema::BrowserConfig {
            enabled: true,
            ..Default::default()
        };
        let tool = BrowserTool::from_config(&config).unwrap();
        assert_eq!(tool.name(), "browser");
    }

    #[test]
    fn test_disabled_returns_none() {
        let config = moltis_config::schema::BrowserConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(BrowserTool::from_config(&config).is_none());
    }

    #[test]
    fn test_parameters_schema_has_required_action() {
        let config = moltis_config::schema::BrowserConfig {
            enabled: true,
            ..Default::default()
        };
        let tool = BrowserTool::from_config(&config).unwrap();
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(
            required.iter().any(|v| v == "action"),
            "action should be in required fields"
        );
    }

    #[test]
    fn test_schema_includes_extended_actions() {
        let config = moltis_config::schema::BrowserConfig {
            enabled: true,
            ..Default::default()
        };
        let tool = BrowserTool::from_config(&config).unwrap();
        let schema = tool.parameters_schema();
        let action_enum = schema["properties"]["action"]["enum"].as_array().unwrap();
        let actions: Vec<&str> = action_enum.iter().filter_map(|v| v.as_str()).collect();

        for expected in [
            "hover",
            "double_click",
            "focus",
            "drag",
            "check",
            "uncheck",
            "select",
            "press",
            "upload",
            "clear",
        ] {
            assert!(
                actions.contains(&expected),
                "extended action '{expected}' must be in schema enum"
            );
        }
    }

    #[test]
    fn test_schema_includes_phase5_7_actions() {
        let config = moltis_config::schema::BrowserConfig {
            enabled: true,
            ..Default::default()
        };
        let tool = BrowserTool::from_config(&config).unwrap();
        let schema = tool.parameters_schema();
        let action_enum = schema["properties"]["action"]["enum"].as_array().unwrap();
        let actions: Vec<&str> = action_enum.iter().filter_map(|v| v.as_str()).collect();

        for expected in [
            "intercept_requests",
            "stop_intercept",
            "set_extra_headers",
            "start_har",
            "stop_har",
            "save_state",
            "load_state",
            "list_states",
            "delete_state",
            "set_device",
            "set_geolocation",
            "set_timezone",
            "set_locale",
            "clear_device",
            "start_screencast",
            "stop_screencast",
            "get_screencast_frame",
            "tab_new",
            "tab_list",
            "tab_switch",
            "tab_close",
        ] {
            assert!(
                actions.contains(&expected),
                "Phase 5-7 action '{expected}' must be in schema enum"
            );
        }
    }

    #[test]
    fn test_schema_has_extended_action_properties() {
        let config = moltis_config::schema::BrowserConfig {
            enabled: true,
            ..Default::default()
        };
        let tool = BrowserTool::from_config(&config).unwrap();
        let schema = tool.parameters_schema();
        let props = schema["properties"].as_object().unwrap();

        for expected in ["from_ref", "to_ref", "key", "path", "value"] {
            assert!(
                props.contains_key(expected),
                "schema must include property '{expected}'"
            );
        }
    }

    #[test]
    fn test_schema_has_phase5_7_properties() {
        let config = moltis_config::schema::BrowserConfig {
            enabled: true,
            ..Default::default()
        };
        let tool = BrowserTool::from_config(&config).unwrap();
        let schema = tool.parameters_schema();
        let props = schema["properties"].as_object().unwrap();

        for expected in [
            "url_patterns",
            "headers",
            "name",
            "encrypt",
            "width",
            "height",
            "device_scale_factor",
            "mobile",
            "latitude",
            "longitude",
            "accuracy",
            "timezone_id",
            "locale",
            "format",
            "quality",
            "every_nth",
            "tab_name",
        ] {
            assert!(
                props.contains_key(expected),
                "Phase 5-7 property '{expected}' must be in schema"
            );
        }
    }
}
