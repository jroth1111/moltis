use serde::Serialize;

use crate::{
    challenge::{ChallengeDetection, ChallengeType},
    detect::{DetectedBrowser, DetectionSource},
    types::{BrowserConfig, BrowserKind, ProtectionConfig, ProtectionTrigger},
};

const UNRESOLVED_BODY_TEXT_THRESHOLD: usize = 64;

#[derive(Debug, Clone)]
pub(crate) struct ProtectionAssessment {
    pub final_url: String,
    pub title_len: usize,
    pub body_text_len: usize,
    pub interactive_element_count: usize,
    pub html_len: usize,
    pub challenge_type: Option<ChallengeType>,
    pub challenge_markers: Vec<String>,
}

impl ProtectionAssessment {
    pub(crate) fn is_content(&self) -> bool {
        self.challenge_type.is_none() && !self.is_unresolved_interstitial()
    }

    pub(crate) fn is_unresolved_interstitial(&self) -> bool {
        self.body_text_len == 0
            || (self.body_text_len < UNRESOLVED_BODY_TEXT_THRESHOLD
                && self.interactive_element_count == 0)
    }

    pub(crate) fn fallback_trigger(&self) -> Option<ProtectionTrigger> {
        self.challenge_type
            .map(ProtectionTrigger::from)
            .or_else(|| {
                self.is_unresolved_interstitial()
                    .then_some(ProtectionTrigger::UnresolvedInterstitial)
            })
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct PatchrightLaunchProfile {
    pub channel: Option<String>,
    pub executable_path: Option<String>,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub device_scale_factor: f64,
    pub locale: String,
    pub user_agent: Option<String>,
}

pub(crate) fn assess_html(
    final_url: String,
    title_len: usize,
    body_text_len: usize,
    interactive_element_count: usize,
    html: &str,
) -> ProtectionAssessment {
    let html_len = html.len();
    let unresolved_interstitial = body_text_len == 0
        || (body_text_len < UNRESOLVED_BODY_TEXT_THRESHOLD && interactive_element_count == 0);
    let detection = crate::challenge::detect_challenge(html);
    let challenge_type = classify_blocking_challenge(&detection, unresolved_interstitial);
    let challenge_markers = challenge_type
        .map(|_| {
            detection
                .markers
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    ProtectionAssessment {
        final_url,
        title_len,
        body_text_len,
        interactive_element_count,
        html_len,
        challenge_type,
        challenge_markers,
    }
}

pub(crate) fn should_wait_for_challenge_resolution(diagnostics: &ProtectionAssessment) -> bool {
    diagnostics.challenge_type.is_some() || diagnostics.is_unresolved_interstitial()
}

pub(crate) fn protection_trigger_for_fallback(
    diagnostics: &ProtectionAssessment,
    sandbox: bool,
    url: &str,
    config: &ProtectionConfig,
) -> Option<ProtectionTrigger> {
    if !config.enabled || sandbox {
        return None;
    }
    if !crate::types::is_domain_allowed(url, &config.domains) {
        return None;
    }

    let trigger = diagnostics.fallback_trigger()?;
    if is_allowed_trigger(trigger, &config.triggers) {
        Some(trigger)
    } else {
        None
    }
}

pub(crate) fn build_patchright_launch_profile_for_browser(
    config: &BrowserConfig,
    selected: Option<&DetectedBrowser>,
) -> PatchrightLaunchProfile {
    let locale = config
        .stealth
        .languages
        .as_ref()
        .and_then(|languages| languages.first().cloned())
        .unwrap_or_else(|| "en-US".to_string());
    let user_agent = config
        .user_agent
        .clone()
        .or_else(|| config.stealth.user_agent.clone());

    let (channel, executable_path) = if let Some(selected) = selected {
        let use_channel = !matches!(
            selected.source,
            DetectionSource::CustomPath | DetectionSource::EnvVar
        );
        let channel = if use_channel {
            patchright_channel_for_browser(selected.kind).map(ToString::to_string)
        } else {
            None
        };
        let executable_path = if channel.is_none() {
            Some(selected.path.to_string_lossy().into_owned())
        } else {
            None
        };
        (channel, executable_path)
    } else {
        (None, None)
    };

    PatchrightLaunchProfile {
        channel,
        executable_path,
        viewport_width: config.viewport_width,
        viewport_height: config.viewport_height,
        device_scale_factor: config.device_scale_factor,
        locale,
        user_agent,
    }
}

fn classify_blocking_challenge(
    detection: &ChallengeDetection,
    unresolved_interstitial: bool,
) -> Option<ChallengeType> {
    match detection.candidate_type {
        Some(
            challenge @ (ChallengeType::Imperva
            | ChallengeType::Kasada
            | ChallengeType::Cloudflare
            | ChallengeType::GenericBrowserCheck
            | ChallengeType::GenericChallenge),
        ) if detection.explicit_challenge || unresolved_interstitial => Some(challenge),
        Some(challenge @ (ChallengeType::Recaptcha | ChallengeType::Hcaptcha))
            if detection.explicit_challenge || detection.challenge_widget =>
        {
            Some(challenge)
        },
        _ => None,
    }
}

fn is_allowed_trigger(trigger: ProtectionTrigger, allowlist: &[ProtectionTrigger]) -> bool {
    if allowlist.is_empty() {
        return true;
    }

    allowlist.contains(&trigger)
}

fn patchright_channel_for_browser(kind: BrowserKind) -> Option<&'static str> {
    match kind {
        BrowserKind::Chrome => Some("chrome"),
        BrowserKind::Chromium => Some("chromium"),
        BrowserKind::Edge => Some("msedge"),
        BrowserKind::Brave | BrowserKind::Opera | BrowserKind::Vivaldi | BrowserKind::Arc => None,
        BrowserKind::Custom => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_script_on_contentful_page_is_not_blocking() {
        let assessment = assess_html(
            "https://shop.example.com".to_string(),
            12,
            400,
            6,
            r#"<html><body><script src="/_Incapsula_Resource"></script><main><a href="/shop">Shop</a></main></body></html>"#,
        );

        assert!(assessment.is_content());
        assert_eq!(assessment.challenge_type, None);
        assert!(assessment.challenge_markers.is_empty());
    }

    #[test]
    fn title_only_page_is_unresolved_interstitial() {
        let assessment = assess_html(
            "https://example.com".to_string(),
            11,
            0,
            0,
            "<html><head><title>Hold tight</title></head><body></body></html>",
        );

        assert!(assessment.is_unresolved_interstitial());
        assert!(!assessment.is_content());
    }

    #[test]
    fn low_body_no_interactive_page_is_unresolved_interstitial() {
        let assessment = assess_html(
            "https://example.com".to_string(),
            5,
            32,
            0,
            "<html><body>loading browser check</body></html>",
        );

        assert!(assessment.is_unresolved_interstitial());
        assert!(!assessment.is_content());
    }

    #[test]
    fn explicit_vendor_challenge_remains_blocking() {
        let imperva = assess_html(
            "https://coles.com.au".to_string(),
            24,
            80,
            0,
            "pardon our interruption _incapsula_resource",
        );
        assert_eq!(imperva.challenge_type, Some(ChallengeType::Imperva));

        let kasada = assess_html(
            "https://realestate.com.au".to_string(),
            24,
            80,
            0,
            "kpsdk please enable javascript",
        );
        assert_eq!(kasada.challenge_type, Some(ChallengeType::Kasada));
    }

    #[test]
    fn fallback_gate_respects_enabled_sandbox_domain_and_challenge() {
        let mut cfg = ProtectionConfig {
            enabled: true,
            ..ProtectionConfig::default()
        };
        cfg.domains = vec!["*.example.com".to_string()];
        cfg.triggers = vec![ProtectionTrigger::Kasada];

        let diagnostics = assess_html("https://shop.example.com".to_string(), 5, 0, 0, "kpsdk");
        assert_eq!(
            protection_trigger_for_fallback(&diagnostics, false, "https://shop.example.com/", &cfg),
            Some(ProtectionTrigger::Kasada)
        );
        assert_eq!(
            protection_trigger_for_fallback(&diagnostics, true, "https://shop.example.com/", &cfg),
            None
        );
        assert_eq!(
            protection_trigger_for_fallback(
                &diagnostics,
                false,
                "https://other.example.net/",
                &cfg
            ),
            None
        );
    }

    #[test]
    fn fallback_gate_supports_unresolved_interstitial_trigger() {
        let mut cfg = ProtectionConfig {
            enabled: true,
            ..ProtectionConfig::default()
        };
        cfg.triggers = vec![ProtectionTrigger::UnresolvedInterstitial];

        let diagnostics = assess_html(
            "https://shop.example.com".to_string(),
            12,
            0,
            0,
            "<html><title>Loading</title></html>",
        );
        assert_eq!(
            protection_trigger_for_fallback(&diagnostics, false, "https://shop.example.com/", &cfg),
            Some(ProtectionTrigger::UnresolvedInterstitial)
        );
    }

    #[test]
    fn waits_for_challenge_resolution_on_unresolved_interstitial() {
        let diagnostics = assess_html(
            "https://example.com".to_string(),
            12,
            0,
            0,
            "<html><title>Loading</title></html>",
        );
        assert!(diagnostics.is_unresolved_interstitial());
        assert!(should_wait_for_challenge_resolution(&diagnostics));
    }
}
