//! Integration tests for browser anti-detection against real websites.
//!
//! Tests navigation to target sites with challenge detection and verifies that
//! the returned session is still usable for follow-up actions.

use std::time::Duration;

use moltis_browser::{
    BrowserBackendKind, BrowserManager, NavigationVerdict, ProtectionConfig,
    types::{BrowserAction, BrowserConfig, BrowserPreference, BrowserRequest},
};
use tokio::time::timeout;

struct TargetSite {
    name: &'static str,
    url: &'static str,
    min_body_text_len: usize,
}

const TARGET_SITES: &[TargetSite] = &[
    TargetSite {
        name: "google_au",
        url: "https://google.com.au",
        min_body_text_len: 100,
    },
    TargetSite {
        name: "woolworths",
        url: "https://woolworths.com.au",
        min_body_text_len: 150,
    },
    TargetSite {
        name: "coles",
        url: "https://coles.com.au",
        min_body_text_len: 100,
    },
    TargetSite {
        name: "realestate",
        url: "https://realestate.com.au",
        min_body_text_len: 100,
    },
];

fn base_test_config() -> BrowserConfig {
    let mut config = BrowserConfig::default();
    config.persist_profile = false;
    config.protection = ProtectionConfig {
        enabled: true,
        python_binary: "python3".to_string(),
        timeout_ms: 90_000,
        max_retries: 3,
        ..ProtectionConfig::default()
    };
    config
}

#[derive(Debug)]
struct SiteTestResult {
    name: String,
    success: bool,
    backend: BrowserBackendKind,
    challenge_type: Option<String>,
    title_len: u64,
    body_text_len: u64,
    final_url: String,
    page_title: String,
    snapshot_elements: usize,
    error: Option<String>,
}

impl std::fmt::Display for SiteTestResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = if self.success {
            "PASS"
        } else {
            "FAIL"
        };
        writeln!(f, "=== {} [{}] ===", self.name, status)?;
        writeln!(f, "  backend: {}", self.backend)?;
        writeln!(f, "  challenge_type: {:?}", self.challenge_type)?;
        writeln!(f, "  title_len: {}", self.title_len)?;
        writeln!(f, "  body_text_len: {}", self.body_text_len)?;
        writeln!(f, "  final_url: {}", self.final_url)?;
        writeln!(f, "  page_title: {}", self.page_title)?;
        writeln!(f, "  snapshot_elements: {}", self.snapshot_elements)?;
        if let Some(err) = &self.error {
            writeln!(f, "  error: {}", err)?;
        }
        Ok(())
    }
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

async fn test_site(site: &TargetSite) -> SiteTestResult {
    let manager = BrowserManager::new(base_test_config());

    let navigate = manager
        .handle_request(request(
            None,
            BrowserAction::Navigate {
                url: site.url.to_string(),
            },
            60_000,
        ))
        .await;
    let session_id = navigate.session_id.clone();

    let snapshot = manager
        .handle_request(request(
            Some(session_id.clone()),
            BrowserAction::Snapshot,
            30_000,
        ))
        .await;
    let title = manager
        .handle_request(request(
            Some(session_id.clone()),
            BrowserAction::GetTitle,
            30_000,
        ))
        .await;
    let url = manager
        .handle_request(request(Some(session_id), BrowserAction::GetUrl, 30_000))
        .await;

    manager.shutdown().await;

    let navigation = navigate.navigation.as_ref();
    let challenge_type = navigation
        .and_then(|nav| nav.challenge.as_ref())
        .map(|challenge| challenge.challenge_type.as_str().to_string());
    let title_len = navigation.map(|nav| nav.title_len).unwrap_or(0);
    let body_text_len = navigation.map(|nav| nav.body_text_len).unwrap_or(0);
    let final_url = navigation
        .map(|nav| nav.final_url.clone())
        .unwrap_or_default();
    let snapshot_elements = snapshot
        .snapshot
        .as_ref()
        .map(|dom| dom.elements.len())
        .unwrap_or(0);

    SiteTestResult {
        name: site.name.to_string(),
        success: navigate.success
            && navigation.map(|nav| nav.verdict) == Some(NavigationVerdict::Content)
            && challenge_type.is_none()
            && body_text_len as usize >= site.min_body_text_len
            && snapshot.success
            && snapshot.snapshot.is_some()
            && snapshot_elements > 0
            && title.success
            && !title.title.clone().unwrap_or_default().is_empty()
            && url.success
            && !url.url.clone().unwrap_or_default().is_empty(),
        backend: navigate.backend,
        challenge_type,
        title_len,
        body_text_len,
        final_url,
        page_title: title.title.unwrap_or_default(),
        snapshot_elements,
        error: navigate
            .error
            .or(snapshot.error)
            .or(title.error)
            .or(url.error),
    }
}

async fn assert_site(site: &TargetSite, timeout_secs: u64) {
    let result = timeout(Duration::from_secs(timeout_secs), test_site(site))
        .await
        .expect("test timed out");

    println!("{}", result);

    assert!(
        result.success,
        "{} navigation failed: {:?}",
        site.url, result.error
    );
    assert!(
        result.challenge_type.is_none(),
        "unexpected challenge: {:?}",
        result.challenge_type
    );
    assert!(
        result.body_text_len >= site.min_body_text_len as u64,
        "body_text_len {} below minimum {}",
        result.body_text_len,
        site.min_body_text_len
    );
    assert!(
        result.snapshot_elements > 0,
        "snapshot returned no interactive elements"
    );
    assert!(!result.page_title.is_empty(), "title lookup returned empty");
    assert!(
        !result.final_url.is_empty(),
        "final_url should not be empty"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_google_au_navigation() {
    let _ = tracing_subscriber::fmt::try_init();
    assert_site(&TARGET_SITES[0], 90).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_woolworths_navigation() {
    let _ = tracing_subscriber::fmt::try_init();
    assert_site(&TARGET_SITES[1], 90).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_coles_navigation() {
    let _ = tracing_subscriber::fmt::try_init();
    assert_site(&TARGET_SITES[2], 120).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_realestate_navigation() {
    let _ = tracing_subscriber::fmt::try_init();
    assert_site(&TARGET_SITES[3], 120).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_all_target_sites_summary() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut results = Vec::new();

    for site in TARGET_SITES {
        let timeout_secs = match site.name {
            "coles" | "realestate" => 120,
            _ => 90,
        };
        let result = timeout(Duration::from_secs(timeout_secs), test_site(site))
            .await
            .expect("summary test timed out");
        println!("{}", result);
        results.push(result);
    }

    let pass_count = results.iter().filter(|result| result.success).count();
    let fail_count = results.len() - pass_count;

    println!(
        "=== RESULTS: {} passed, {} failed ===\n",
        pass_count, fail_count
    );

    assert_eq!(fail_count, 0, "all target sites should pass");
}
