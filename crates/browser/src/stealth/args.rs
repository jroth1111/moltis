//! Stealth Chrome launch arguments.
//!
//! These flags reduce detectable automation signals at the browser level.
//! Applied when `StealthConfig::stealth_args` is enabled.

/// 19 Chrome launch flags that reduce bot-detection signals.
pub const STEALTH_ARGS: &[&str] = &[
    // CRITICAL: removes the `AutomationControlled` feature flag
    "--disable-blink-features=AutomationControlled",
    // Disable features that can reveal isolation/automation
    "--disable-features=IsolateOrigins,site-per-process",
    // Make window.chrome available (needed for chrome.runtime polyfill)
    "--enable-features=NetworkService,NetworkServiceInProcess",
    // Suppress infobars ("Chrome is being controlled by automated test software")
    "--disable-infobars",
    // No extensions in automation — matches a headful user that hasn't installed any
    "--disable-extensions",
    "--disable-default-apps",
    "--disable-component-extensions-with-background-pages",
    // Suppress background network requests that reveal automation
    "--disable-background-networking",
    "--disable-sync",
    "--no-first-run",
    "--disable-translate",
    // Suppress metrics / telemetry side-channels
    "--metrics-recording-only",
    "--disable-hang-monitor",
    "--disable-prompt-on-repost",
    // Disable phishing detection (makes network calls)
    "--disable-client-side-phishing-detection",
    // Disable popup blocking (tests may open windows)
    "--disable-popup-blocking",
    // Suppress component updates that make outbound requests
    "--disable-component-update",
    // Use basic password storage (avoids OS keychain prompts)
    "--password-store=basic",
    "--use-mock-keychain",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stealth_args_count_is_nineteen() {
        // Exact count check — update this test if args are intentionally added/removed
        assert_eq!(STEALTH_ARGS.len(), 19);
    }

    #[test]
    fn no_duplicate_args() {
        let mut seen = std::collections::HashSet::new();
        for arg in STEALTH_ARGS {
            assert!(seen.insert(*arg), "duplicate stealth arg: {arg}");
        }
    }

    #[test]
    fn automation_controlled_flag_present() {
        assert!(
            STEALTH_ARGS
                .iter()
                .any(|a| a.contains("AutomationControlled")),
            "AutomationControlled disablement must be present"
        );
    }

    #[test]
    fn no_overlap_with_security_args() {
        // These security args are added unconditionally in pool.rs;
        // confirm they don't duplicate stealth args.
        let security = [
            "--disable-gpu",
            "--disable-dev-shm-usage",
            "--disable-software-rasterizer",
            "--no-sandbox",
            "--disable-setuid-sandbox",
        ];
        for s in security {
            assert!(!STEALTH_ARGS.contains(&s), "overlap with security arg: {s}");
        }
    }
}
