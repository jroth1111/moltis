//! Challenge-page detection helpers for anti-bot/WAF interstitials.

use serde::{Deserialize, Serialize};

/// Classified challenge page type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChallengeType {
    Imperva,
    Kasada,
    Cloudflare,
    Recaptcha,
    Hcaptcha,
    GenericBrowserCheck,
    GenericChallenge,
}

impl ChallengeType {
    /// Stable lowercase identifier for serialization/logging.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Imperva => "imperva",
            Self::Kasada => "kasada",
            Self::Cloudflare => "cloudflare",
            Self::Recaptcha => "recaptcha",
            Self::Hcaptcha => "hcaptcha",
            Self::GenericBrowserCheck => "generic_browser_check",
            Self::GenericChallenge => "generic_challenge",
        }
    }
}

/// Detection result for HTML challenge classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeDetection {
    pub challenge_type: Option<ChallengeType>,
    pub markers: Vec<&'static str>,
}

const IMPERVA_MARKERS: &[&str] = &[
    "pardon our interruption",
    "_incapsula_resource",
    "incapsula incident id",
    "visid_incap",
    "/_incap",
    "reese84",
];

const KASADA_MARKERS: &[&str] = &["kpsdk", "ips.js?", "x-kpsdk", "kasada"];

const CLOUDFLARE_MARKERS: &[&str] = &[
    "__cf_chl",
    "cf-chl",
    "cloudflare",
    "checking your browser before accessing",
];

const GENERIC_BROWSER_CHECK_MARKERS: &[&str] = &["checking your browser", "browser check"];

const GENERIC_CHALLENGE_MARKERS: &[&str] = &[
    "verify you are human",
    "captcha challenge",
    "are you a robot",
    "request unsuccessful",
    "access denied",
];

fn collect_markers(haystack: &str, candidates: &[&'static str]) -> Vec<&'static str> {
    candidates
        .iter()
        .copied()
        .filter(|marker| haystack.contains(marker))
        .collect()
}

/// Detect challenge type and matched markers from full page HTML.
#[must_use]
pub fn detect_challenge(html: &str) -> ChallengeDetection {
    let lower = html.to_lowercase();

    let imperva = collect_markers(&lower, IMPERVA_MARKERS);
    if !imperva.is_empty() {
        return ChallengeDetection {
            challenge_type: Some(ChallengeType::Imperva),
            markers: imperva,
        };
    }

    let kasada = collect_markers(&lower, KASADA_MARKERS);
    if !kasada.is_empty() {
        return ChallengeDetection {
            challenge_type: Some(ChallengeType::Kasada),
            markers: kasada,
        };
    }

    let cloudflare = collect_markers(&lower, CLOUDFLARE_MARKERS);
    if !cloudflare.is_empty() && lower.contains("challenge") {
        return ChallengeDetection {
            challenge_type: Some(ChallengeType::Cloudflare),
            markers: cloudflare,
        };
    }

    let has_recaptcha_widget = lower.contains("g-recaptcha")
        || lower.contains("recaptcha-checkbox")
        || lower.contains("recaptcha/api.js?render=explicit");
    let recaptcha_shell = lower.len() < 15_000
        && lower.contains("recaptcha")
        && (lower.contains("challenge") || lower.contains("verify you are human"));
    if has_recaptcha_widget || recaptcha_shell {
        return ChallengeDetection {
            challenge_type: Some(ChallengeType::Recaptcha),
            markers: vec!["recaptcha"],
        };
    }

    if lower.contains("hcaptcha") {
        return ChallengeDetection {
            challenge_type: Some(ChallengeType::Hcaptcha),
            markers: vec!["hcaptcha"],
        };
    }

    let browser_check = collect_markers(&lower, GENERIC_BROWSER_CHECK_MARKERS);
    if !browser_check.is_empty() {
        return ChallengeDetection {
            challenge_type: Some(ChallengeType::GenericBrowserCheck),
            markers: browser_check,
        };
    }

    if lower.len() < 50_000 {
        let generic = collect_markers(&lower, GENERIC_CHALLENGE_MARKERS);
        if !generic.is_empty() {
            return ChallengeDetection {
                challenge_type: Some(ChallengeType::GenericChallenge),
                markers: generic,
            };
        }
    }

    ChallengeDetection {
        challenge_type: None,
        markers: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_imperva_marker() {
        let html = r#"<html><script src="/_Incapsula_Resource?x"></script></html>"#;
        let result = detect_challenge(html);
        assert_eq!(result.challenge_type, Some(ChallengeType::Imperva));
        assert!(
            result.markers.iter().any(|m| *m == "_incapsula_resource"),
            "expected incapsula marker in {:?}",
            result.markers
        );
    }

    #[test]
    fn detects_kasada_marker() {
        let html = r#"<html><script>window.KPSDK=true</script></html>"#;
        let result = detect_challenge(html);
        assert_eq!(result.challenge_type, Some(ChallengeType::Kasada));
    }

    #[test]
    fn detects_cloudflare_challenge() {
        let html = "<html>Cloudflare challenge page</html>";
        let result = detect_challenge(html);
        assert_eq!(result.challenge_type, Some(ChallengeType::Cloudflare));
    }

    #[test]
    fn ignores_normal_page() {
        let html = "<html><head><title>Home</title></head><body><nav>Shop</nav></body></html>";
        let result = detect_challenge(html);
        assert_eq!(result.challenge_type, None);
        assert!(result.markers.is_empty());
    }

    #[test]
    fn ignores_passive_recaptcha_v3() {
        let html = r#"<html><body><script src="https://www.google.com/recaptcha/api.js?render=site-key"></script><main>content</main></body></html>"#;
        let result = detect_challenge(html);
        assert_eq!(result.challenge_type, None);
    }
}
