//! Stealth evasion for chromiumoxide — anti-bot detection via JS injection and Chrome flags.
//!
//! Ports the 20-evasion strategy from `agent-browser-stealth/src/stealth-native.ts`
//! to pure Rust + CDP. No Node.js dependency.
//!
//! # What this does
//!
//! 1. Injects a comprehensive JS script via `Page.addScriptToEvaluateOnNewDocument`
//!    that runs before any page scripts on every navigation.
//! 2. Adds Chrome launch flags that reduce automation fingerprinting at the browser level.
//! 3. Optionally overrides User-Agent and request headers when explicitly configured.
//!
//! # Feature gate
//!
//! All code in this module is gated behind `#[cfg(feature = "stealth")]`.

pub mod args;
pub mod behavior;

use chromiumoxide::{
    Page,
    cdp::browser_protocol::network::{Headers, SetExtraHttpHeadersParams},
};

use crate::{error::Error, types::StealthConfig};

/// The evasion JS template — loaded at compile time, placeholders replaced at runtime.
const EVASIONS_TEMPLATE: &str = include_str!("evasions.js");

/// User agent that removes the `HeadlessChrome` indicator.
///
/// Matches a real macOS Chrome 120 install.
pub const STEALTH_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/120.0.0.0 Safari/537.36";

/// Return the set of Chrome launch flags that reduce bot-detection signals.
#[must_use]
pub fn chrome_stealth_args() -> &'static [&'static str] {
    args::STEALTH_ARGS
}

/// Return the default stealth User-Agent string.
#[must_use]
pub fn default_user_agent() -> &'static str {
    STEALTH_USER_AGENT
}

/// Build `sec-ch-ua` from a Chromium-style User-Agent string.
#[must_use]
pub fn build_sec_ch_ua(user_agent: &str) -> Option<String> {
    let marker = "Chrome/";
    let start = user_agent.find(marker)? + marker.len();
    let major: String = user_agent[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if major.is_empty() {
        return None;
    }
    Some(format!(
        "\"Google Chrome\";v=\"{major}\", \"Chromium\";v=\"{major}\", \";Not A Brand\";v=\"99\""
    ))
}

fn infer_sec_ch_platform(user_agent: &str) -> &'static str {
    let lower = user_agent.to_lowercase();
    if lower.contains("mac os x") {
        "\"macOS\""
    } else if lower.contains("windows") {
        "\"Windows\""
    } else if lower.contains("android") {
        "\"Android\""
    } else if lower.contains("linux") {
        "\"Linux\""
    } else {
        "\"Unknown\""
    }
}

fn build_accept_language(config: &StealthConfig) -> String {
    if let Some(langs) = config.languages.as_ref()
        && !langs.is_empty()
    {
        let mut parts = Vec::with_capacity(langs.len());
        for (idx, lang) in langs.iter().enumerate() {
            if idx == 0 {
                parts.push(lang.clone());
            } else {
                let quality = match idx {
                    1 => "0.9",
                    2 => "0.8",
                    3 => "0.7",
                    _ => "0.6",
                };
                parts.push(format!("{lang};q={quality}"));
            }
        }
        return parts.join(",");
    }
    "en-US,en;q=0.9".to_string()
}

/// Build the evasion JS with `config` values substituted into the placeholders.
///
/// Configurable values (WebGL vendor, languages, hardware concurrency, etc.) are
/// defined as constants at the top of `evasions.js` and replaced here.
#[must_use]
pub fn build_evasion_script(config: &StealthConfig) -> String {
    let mut script = EVASIONS_TEMPLATE.to_string();

    if let Some(n) = config.hardware_concurrency {
        script = script.replace(
            "const STEALTH_HARDWARE_CONCURRENCY = 4;",
            &format!("const STEALTH_HARDWARE_CONCURRENCY = {};", n),
        );
    }

    if let Some(n) = config.device_memory {
        script = script.replace(
            "const STEALTH_DEVICE_MEMORY = 8;",
            &format!("const STEALTH_DEVICE_MEMORY = {};", n),
        );
    }

    if let Some(ref v) = config.webgl_vendor {
        // Single-quote escape only — content should not contain single quotes
        let safe = v.replace('\'', "\\'");
        script = script.replace(
            "const STEALTH_WEBGL_VENDOR = 'Intel Inc.';",
            &format!("const STEALTH_WEBGL_VENDOR = '{}';", safe),
        );
    }

    if let Some(ref r) = config.webgl_renderer {
        let safe = r.replace('\'', "\\'");
        script = script.replace(
            "const STEALTH_WEBGL_RENDERER = 'Intel Iris OpenGL Engine';",
            &format!("const STEALTH_WEBGL_RENDERER = '{}';", safe),
        );
    }

    if let Some(ref langs) = config.languages
        && !langs.is_empty()
    {
        let js_array = langs
            .iter()
            .map(|l| format!("'{}'", l.replace('\'', "\\'")))
            .collect::<Vec<_>>()
            .join(", ");
        script = script.replace(
            "const STEALTH_LANGUAGES = ['en-US', 'en'];",
            &format!("const STEALTH_LANGUAGES = [{}];", js_array),
        );
    }

    script
}

/// Inject the stealth evasion script and configure User-Agent on the given page.
///
/// The script is registered via `Page.addScriptToEvaluateOnNewDocument`, so it
/// runs before any other scripts on every subsequent navigation of this page.
/// Only needs to be called once per page lifetime.
pub async fn inject_stealth(page: &Page, config: &StealthConfig) -> Result<(), Error> {
    let source = build_evasion_script(config);
    page.evaluate_on_new_document(source)
        .await
        .map_err(|e| Error::Cdp(format!("stealth script injection failed: {e}")))?;

    // Keep browser-native UA by default to avoid UA/client-hints mismatches
    // against modern bot defenses. Only override when explicitly configured.
    if let Some(ua) = config.user_agent.as_deref() {
        page.set_user_agent(ua)
            .await
            .map_err(|e| Error::Cdp(format!("failed to set stealth user agent: {e}")))?;
    }

    #[cfg(feature = "metrics")]
    moltis_metrics::counter!(moltis_metrics::browser::STEALTH_INJECTIONS_TOTAL).increment(1);

    Ok(())
}

/// Apply low-risk default headers aligned with stealth settings.
///
/// This sets `Accept-Language` and Chromium client hints (`sec-ch-ua*`) via
/// CDP `Network.setExtraHTTPHeaders`.
pub async fn apply_stealth_headers(page: &Page, config: &StealthConfig) -> Result<(), Error> {
    let user_agent = if let Some(ua) = config.user_agent.as_deref() {
        ua.to_string()
    } else {
        page.user_agent()
            .await
            .map_err(|e| Error::Cdp(format!("failed to read browser user agent: {e}")))?
    };

    let mut headers = serde_json::Map::new();
    headers.insert(
        "Accept-Language".to_string(),
        serde_json::Value::String(build_accept_language(config)),
    );
    if let Some(sec_ch_ua) = build_sec_ch_ua(&user_agent) {
        headers.insert(
            "sec-ch-ua".to_string(),
            serde_json::Value::String(sec_ch_ua),
        );
    }
    headers.insert(
        "sec-ch-ua-mobile".to_string(),
        serde_json::Value::String("?0".to_string()),
    );
    headers.insert(
        "sec-ch-ua-platform".to_string(),
        serde_json::Value::String(infer_sec_ch_platform(&user_agent).to_string()),
    );

    page.execute(SetExtraHttpHeadersParams::new(Headers::new(
        serde_json::Value::Object(headers),
    )))
    .await
    .map_err(|e| Error::Cdp(format!("failed to set stealth headers: {e}")))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use {super::*, crate::types::StealthConfig};

    fn default_config() -> StealthConfig {
        StealthConfig::default()
    }

    #[test]
    fn build_evasion_script_contains_all_20_evasion_markers() {
        let script = build_evasion_script(&default_config());

        // Each marker corresponds to one of the 20 enumerated evasions
        let markers = [
            "navigator.webdriver",
            "chrome.runtime",
            "chrome.app",
            "chrome.csi",
            "chrome.loadTimes",
            "generateMagicArray", // plugins evasion
            "STEALTH_LANGUAGES",
            "navigator.vendor",
            "STEALTH_HARDWARE_CONCURRENCY",
            "navigator.permissions",
            "WebGLRenderingContext",
            "outerWidth",
            "HTMLIFrameElement",
            "canPlayType",
            "STEALTH_DEVICE_MEMORY",
            "navigator.connection",
            "RTCPeerConnection",
            "HTMLCanvasElement.prototype.toDataURL",
            "AudioBuffer.prototype.getChannelData",
            "window.FontFace = WrappedFontFace",
        ];

        for marker in markers {
            assert!(
                script.contains(marker),
                "evasion script missing marker: {marker}"
            );
        }
    }

    #[test]
    fn build_evasion_script_substitutes_hardware_concurrency() {
        let config = StealthConfig {
            hardware_concurrency: Some(8),
            ..default_config()
        };
        let script = build_evasion_script(&config);
        assert!(
            script.contains("const STEALTH_HARDWARE_CONCURRENCY = 8;"),
            "hardware_concurrency not substituted"
        );
    }

    #[test]
    fn build_evasion_script_substitutes_device_memory() {
        let config = StealthConfig {
            device_memory: Some(16),
            ..default_config()
        };
        let script = build_evasion_script(&config);
        assert!(
            script.contains("const STEALTH_DEVICE_MEMORY = 16;"),
            "device_memory not substituted"
        );
    }

    #[test]
    fn build_evasion_script_substitutes_webgl_vendor() {
        let config = StealthConfig {
            webgl_vendor: Some("AMD Inc.".to_string()),
            ..default_config()
        };
        let script = build_evasion_script(&config);
        assert!(
            script.contains("'AMD Inc.'"),
            "webgl_vendor not substituted"
        );
    }

    #[test]
    fn build_evasion_script_substitutes_webgl_renderer() {
        let config = StealthConfig {
            webgl_renderer: Some("Radeon RX 6800".to_string()),
            ..default_config()
        };
        let script = build_evasion_script(&config);
        assert!(
            script.contains("'Radeon RX 6800'"),
            "webgl_renderer not substituted"
        );
    }

    #[test]
    fn build_evasion_script_substitutes_languages() {
        let config = StealthConfig {
            languages: Some(vec!["fr-FR".to_string(), "fr".to_string()]),
            ..default_config()
        };
        let script = build_evasion_script(&config);
        assert!(
            script.contains("'fr-FR', 'fr'"),
            "languages not substituted"
        );
    }

    #[test]
    fn build_evasion_script_default_config_uses_template_values() {
        let script = build_evasion_script(&default_config());
        // Default values should remain as-is
        assert!(script.contains("const STEALTH_HARDWARE_CONCURRENCY = 4;"));
        assert!(script.contains("const STEALTH_DEVICE_MEMORY = 8;"));
        assert!(script.contains("const STEALTH_WEBGL_VENDOR = 'Intel Inc.';"));
        assert!(script.contains("const STEALTH_WEBGL_RENDERER = 'Intel Iris OpenGL Engine';"));
        assert!(script.contains("const STEALTH_LANGUAGES = ['en-US', 'en'];"));
    }

    #[test]
    fn stealth_user_agent_does_not_contain_headless() {
        let ua = default_user_agent();
        assert!(
            !ua.contains("HeadlessChrome"),
            "user agent must not contain HeadlessChrome"
        );
        assert!(ua.contains("Chrome/"), "user agent should contain Chrome/");
    }

    #[test]
    fn chrome_stealth_args_has_automation_controlled_flag() {
        let args = chrome_stealth_args();
        assert!(
            args.iter().any(|a| a.contains("AutomationControlled")),
            "must disable AutomationControlled"
        );
    }

    #[test]
    fn build_sec_ch_ua_extracts_chrome_major_version() {
        let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123.0.0.0 Safari/537.36";
        assert_eq!(
            build_sec_ch_ua(ua).as_deref(),
            Some("\"Google Chrome\";v=\"123\", \"Chromium\";v=\"123\", \";Not A Brand\";v=\"99\"")
        );
    }

    #[test]
    fn build_sec_ch_ua_returns_none_for_non_chromium_ua() {
        assert!(build_sec_ch_ua("Mozilla/5.0 Firefox/123.0").is_none());
    }

    #[test]
    fn build_accept_language_prefers_configured_languages() {
        let config = StealthConfig {
            languages: Some(vec![
                "en-AU".to_string(),
                "en-US".to_string(),
                "en".to_string(),
            ]),
            ..default_config()
        };
        assert_eq!(build_accept_language(&config), "en-AU,en-US;q=0.9,en;q=0.8");
    }
}
