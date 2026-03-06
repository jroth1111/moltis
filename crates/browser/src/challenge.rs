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
    pub candidate_type: Option<ChallengeType>,
    pub markers: Vec<&'static str>,
    pub explicit_challenge: bool,
    pub challenge_widget: bool,
}

const IMPERVA_MARKERS: &[&str] =
    &["_incapsula_resource", "incapsula incident id", "visid_incap", "/_incap", "reese84"];
const IMPERVA_EXPLICIT_MARKERS: &[&str] = &[
    "pardon our interruption",
    "the website is using a security service",
    "please enable cookies",
];

const KASADA_MARKERS: &[&str] = &["kpsdk", "ips.js?", "x-kpsdk", "kasada"];
const KASADA_EXPLICIT_MARKERS: &[&str] = &[
    "please enable javascript",
    "please enable cookies",
    "access to this page has been denied",
];

const CLOUDFLARE_MARKERS: &[&str] = &["__cf_chl", "cf-chl", "cloudflare"];
const CLOUDFLARE_EXPLICIT_MARKERS: &[&str] = &[
    "checking your browser before accessing",
    "just a moment",
    "verify you are human",
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
    let imperva_explicit = collect_markers(&lower, IMPERVA_EXPLICIT_MARKERS);
    if !imperva.is_empty() || !imperva_explicit.is_empty() {
        let mut markers = imperva;
        markers.extend(imperva_explicit.iter().copied());
        return ChallengeDetection {
            candidate_type: Some(ChallengeType::Imperva),
            markers,
            explicit_challenge: !imperva_explicit.is_empty(),
            challenge_widget: false,
        };
    }

    let kasada = collect_markers(&lower, KASADA_MARKERS);
    let kasada_explicit = collect_markers(&lower, KASADA_EXPLICIT_MARKERS);
    if !kasada.is_empty() || !kasada_explicit.is_empty() {
        let mut markers = kasada;
        markers.extend(kasada_explicit.iter().copied());
        return ChallengeDetection {
            candidate_type: Some(ChallengeType::Kasada),
            markers,
            explicit_challenge: !kasada_explicit.is_empty(),
            challenge_widget: false,
        };
    }

    let cloudflare = collect_markers(&lower, CLOUDFLARE_MARKERS);
    let cloudflare_explicit = collect_markers(&lower, CLOUDFLARE_EXPLICIT_MARKERS);
    if !cloudflare.is_empty() || !cloudflare_explicit.is_empty() {
        let mut markers = cloudflare;
        markers.extend(cloudflare_explicit.iter().copied());
        return ChallengeDetection {
            candidate_type: Some(ChallengeType::Cloudflare),
            markers,
            explicit_challenge: !cloudflare_explicit.is_empty(),
            challenge_widget: false,
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
            candidate_type: Some(ChallengeType::Recaptcha),
            markers: vec!["recaptcha"],
            explicit_challenge: recaptcha_shell,
            challenge_widget: has_recaptcha_widget,
        };
    }

    let has_hcaptcha_widget = lower.contains("hcaptcha")
        && (lower.contains("hcaptcha.com/1/api.js")
            || lower.contains("class=\"h-captcha\"")
            || lower.contains("data-hcaptcha-response")
            || lower.contains("iframe"));
    let hcaptcha_shell = lower.contains("hcaptcha")
        && (lower.contains("verify you are human") || lower.contains("checkbox"));
    if has_hcaptcha_widget || hcaptcha_shell {
        return ChallengeDetection {
            candidate_type: Some(ChallengeType::Hcaptcha),
            markers: vec!["hcaptcha"],
            explicit_challenge: hcaptcha_shell,
            challenge_widget: has_hcaptcha_widget,
        };
    }

    let browser_check = collect_markers(&lower, GENERIC_BROWSER_CHECK_MARKERS);
    if !browser_check.is_empty() {
        return ChallengeDetection {
            candidate_type: Some(ChallengeType::GenericBrowserCheck),
            markers: browser_check,
            explicit_challenge: true,
            challenge_widget: false,
        };
    }

    if lower.len() < 50_000 {
        let generic = collect_markers(&lower, GENERIC_CHALLENGE_MARKERS);
        if !generic.is_empty() {
            return ChallengeDetection {
                candidate_type: Some(ChallengeType::GenericChallenge),
                markers: generic,
                explicit_challenge: true,
                challenge_widget: false,
            };
        }
    }

    ChallengeDetection {
        candidate_type: None,
        markers: Vec::new(),
        explicit_challenge: false,
        challenge_widget: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_imperva_marker() {
        let html = r#"<html><script src="/_Incapsula_Resource?x"></script></html>"#;
        let result = detect_challenge(html);
        assert_eq!(result.candidate_type, Some(ChallengeType::Imperva));
        assert!(
            result.markers.iter().any(|m| *m == "_incapsula_resource"),
            "expected incapsula marker in {:?}",
            result.markers
        );
        assert!(!result.explicit_challenge);
    }

    #[test]
    fn detects_kasada_marker() {
        let html = r#"<html><script>window.KPSDK=true</script></html>"#;
        let result = detect_challenge(html);
        assert_eq!(result.candidate_type, Some(ChallengeType::Kasada));
        assert!(!result.explicit_challenge);
    }

    #[test]
    fn detects_cloudflare_challenge() {
        let html = "<html>Cloudflare checking your browser before accessing</html>";
        let result = detect_challenge(html);
        assert_eq!(result.candidate_type, Some(ChallengeType::Cloudflare));
        assert!(result.explicit_challenge);
    }

    #[test]
    fn ignores_normal_page() {
        let html = "<html><head><title>Home</title></head><body><nav>Shop</nav></body></html>";
        let result = detect_challenge(html);
        assert_eq!(result.candidate_type, None);
        assert!(result.markers.is_empty());
    }

    #[test]
    fn ignores_passive_recaptcha_v3() {
        let html = r#"<html><body><script src="https://www.google.com/recaptcha/api.js?render=site-key"></script><main>content</main></body></html>"#;
        let result = detect_challenge(html);
        assert_eq!(result.candidate_type, None);
    }
}
