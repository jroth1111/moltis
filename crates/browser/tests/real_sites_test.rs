//! Integration tests for browser anti-detection against real websites.
//!
//! Tests navigation to target sites with challenge detection and content access validation.
//! Run with: cargo test -p moltis-browser --test real_sites_test -- --nocapture

use std::time::Duration;

use moltis_browser::{
    BrowserManager,
    types::{
        BrowserAction, BrowserConfig, BrowserPreference, BrowserRequest, PatchrightFallbackConfig,
    },
};
use tokio::time::timeout;

/// Target site configuration for anti-detection validation.
struct TargetSite {
    name: &'static str,
    url: &'static str,
    /// Minimum expected body text length (proxy for successful content access).
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
    config
}

/// Create a browser config with patchright fallback enabled for hard sites.
fn config_with_patchright_fallback() -> BrowserConfig {
    let mut config = base_test_config();
    config.patchright_fallback = PatchrightFallbackConfig {
        enabled: true,
        python_binary: "python3".to_string(),
        timeout_ms: 90_000,
        headless: true,
        challenge_types: vec!["kasada".to_string(), "imperva".to_string()],
        domains: vec![],
        max_retries: 3,
    };
    config
}

fn config_for_site(site: &TargetSite) -> BrowserConfig {
    match site.name {
        "coles" | "realestate" => config_with_patchright_fallback(),
        _ => base_test_config(),
    }
}

/// Test result for a single target site.
#[derive(Debug)]
struct SiteTestResult {
    name: String,
    success: bool,
    challenge_type: Option<String>,
    title_len: u64,
    body_text_len: u64,
    final_url: String,
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
        writeln!(f, "  challenge_type: {:?}", self.challenge_type)?;
        writeln!(f, "  title_len: {}", self.title_len)?;
        writeln!(f, "  body_text_len: {}", self.body_text_len)?;
        writeln!(f, "  final_url: {}", self.final_url)?;
        if let Some(err) = &self.error {
            writeln!(f, "  error: {}", err)?;
        }
        Ok(())
    }
}

async fn test_site(site: &TargetSite) -> SiteTestResult {
    let manager = BrowserManager::new(config_for_site(site));
    let request = BrowserRequest {
        session_id: None,
        action: BrowserAction::Navigate {
            url: site.url.to_string(),
        },
        timeout_ms: 60_000,
        sandbox: Some(false),
        browser: Some(BrowserPreference::Auto),
    };

    let response = manager.handle_request(request).await;
    manager.shutdown().await;

    SiteTestResult {
        name: site.name.to_string(),
        success: response.success
            && response.challenge_type.is_none()
            && response.body_text_len.unwrap_or(0) as usize >= site.min_body_text_len,
        challenge_type: response.challenge_type,
        title_len: response.title_len.unwrap_or(0),
        body_text_len: response.body_text_len.unwrap_or(0),
        final_url: response.final_url.unwrap_or_default(),
        error: response.error,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_google_au_navigation() {
    let _ = tracing_subscriber::fmt::try_init();

    let site = &TARGET_SITES[0]; // google_au
    let result = timeout(Duration::from_secs(90), test_site(site))
        .await
        .expect("test timed out");

    println!("{}", result);

    assert!(
        result.success,
        "google.com.au navigation failed: {:?}",
        result.error
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
}

#[tokio::test(flavor = "multi_thread")]
async fn test_woolworths_navigation() {
    let _ = tracing_subscriber::fmt::try_init();

    let site = &TARGET_SITES[1]; // woolworths
    let result = timeout(Duration::from_secs(90), test_site(site))
        .await
        .expect("test timed out");

    println!("{}", result);

    assert!(
        result.success,
        "woolworths.com.au navigation failed: {:?}",
        result.error
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
}

#[tokio::test(flavor = "multi_thread")]
async fn test_coles_navigation() {
    let _ = tracing_subscriber::fmt::try_init();

    let site = &TARGET_SITES[2]; // coles
    let result = timeout(Duration::from_secs(120), test_site(site))
        .await
        .expect("test timed out");

    println!("{}", result);

    assert!(
        result.success,
        "coles.com.au navigation failed: {:?}",
        result.error
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
}

#[tokio::test(flavor = "multi_thread")]
async fn test_realestate_navigation() {
    let _ = tracing_subscriber::fmt::try_init();

    let site = &TARGET_SITES[3]; // realestate
    let result = timeout(Duration::from_secs(120), test_site(site))
        .await
        .expect("test timed out");

    println!("{}", result);

    assert!(
        result.success,
        "realestate.com.au navigation failed: {:?}",
        result.error
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
}

/// Run all target sites and print a summary report.
#[tokio::test(flavor = "multi_thread")]
async fn test_all_target_sites_summary() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut results = Vec::new();

    for site in TARGET_SITES {
        let site_timeout = match site.name {
            "coles" | "realestate" => Duration::from_secs(120),
            _ => Duration::from_secs(90),
        };
        let result = timeout(site_timeout, test_site(site))
            .await
            .expect("test timed out");
        results.push(result);
    }

    println!("\n=== ANTI-DETECTION TEST SUMMARY ===\n");

    let mut pass_count = 0;
    let mut fail_count = 0;

    for result in &results {
        println!("{}", result);
        if result.success {
            pass_count += 1;
        } else {
            fail_count += 1;
        }
    }

    println!(
        "=== RESULTS: {} passed, {} failed ===\n",
        pass_count, fail_count
    );

    assert_eq!(fail_count, 0, "all target sites should pass");
}
