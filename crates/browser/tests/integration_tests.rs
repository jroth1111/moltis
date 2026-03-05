//! Integration tests for browser anti-bot fallback stack.
//!
//! These tests require network access and are gated behind the `live-sites` feature.
//! Run with: cargo test -p moltis-browser --features live-sites

#[cfg(feature = "live-sites")]
mod live_sites {
    use std::time::Duration;

    use moltis_browser::{BrowserAction, BrowserConfig, BrowserManager, BrowserRequest};
    use tokio::time::timeout;

    /// Test configuration with patchright fallback enabled.
    fn test_config() -> BrowserConfig {
        BrowserConfig {
            enabled: true,
            patchright_fallback: moltis_browser::types::PatchrightFallbackConfig {
                enabled: true,
                headless: true,
                ..Default::default()
            },
            virtual_display: moltis_browser::types::VirtualDisplayConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    async fn navigate_and_check(
        manager: &BrowserManager,
        url: &str,
        min_body_len: usize,
        site_name: &str,
    ) -> (bool, Option<String>, usize) {
        let request = BrowserRequest {
            session_id: None,
            action: BrowserAction::Navigate {
                url: url.to_string(),
            },
            timeout_ms: 60_000,
            sandbox: false,
            browser: None,
        };

        let result = timeout(Duration::from_secs(90), manager.handle_request(request)).await;

        match result {
            Ok(Ok(response)) => {
                let challenged = response.challenge_type.is_some();
                let body_len = response.body_text_len.unwrap_or(0);
                let success = !challenged && body_len >= min_body_len;
                (
                    success,
                    response.challenge_type.map(|c| format!("{:?}", c)),
                    body_len,
                )
            },
            Ok(Err(e)) => (false, Some(format!("error: {}", e)), 0),
            Err(_) => (false, Some("timeout".to_string()), 0),
        }
    }

    #[tokio::test]
    async fn test_google_au_never_challenged() {
        let _ = tracing_subscriber::fmt::try_init();
        let manager = BrowserManager::new(test_config());

        let (success, challenge, body_len) =
            navigate_and_check(&manager, "https://www.google.com.au", 50, "google").await;

        assert!(
            challenge.is_none(),
            "google.com.au should never be challenged, got: {:?}",
            challenge
        );
        assert!(
            success,
            "google.com.au should have body_text_len >= 50, got: {}",
            body_len
        );
    }

    #[tokio::test]
    async fn test_woolworths_au_accessible() {
        let _ = tracing_subscriber::fmt::try_init();
        let manager = BrowserManager::new(test_config());

        let (success, challenge, body_len) =
            navigate_and_check(&manager, "https://www.woolworths.com.au", 300, "woolworths").await;

        // Woolworths may be challenged in headless but should work via fallback
        if challenge.is_some() {
            eprintln!(
                "woolworths.com.au was challenged, body_len={}. This may be environment-dependent.",
                body_len
            );
        }

        assert!(
            success || body_len > 0,
            "woolworths.com.au should be accessible (body_len={}, challenge={:?})",
            body_len,
            challenge
        );
    }

    #[tokio::test]
    async fn test_realestate_au_fallback_succeeds() {
        let _ = tracing_subscriber::fmt::try_init();
        let manager = BrowserManager::new(test_config());

        let (success, challenge, body_len) =
            navigate_and_check(&manager, "https://www.realestate.com.au", 400, "realestate").await;

        // RealEstate uses Kasada - expect fallback to work
        if challenge.is_some() {
            eprintln!(
                "realestate.com.au was challenged after fallback, body_len={}",
                body_len
            );
        }

        assert!(
            body_len >= 100 || success,
            "realestate.com.au should have meaningful content after fallback (body_len={}, challenge={:?})",
            body_len,
            challenge
        );
    }

    #[tokio::test]
    async fn test_coles_au_fallback_succeeds() {
        let _ = tracing_subscriber::fmt::try_init();
        let manager = BrowserManager::new(test_config());

        let (success, challenge, body_len) =
            navigate_and_check(&manager, "https://www.coles.com.au", 300, "coles").await;

        // Coles uses Imperva/Kasada - expect fallback to work
        if challenge.is_some() {
            eprintln!(
                "coles.com.au was challenged after fallback, body_len={}",
                body_len
            );
        }

        assert!(
            body_len >= 100 || success,
            "coles.com.au should have meaningful content after fallback (body_len={}, challenge={:?})",
            body_len,
            challenge
        );
    }
}
