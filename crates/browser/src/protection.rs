use serde::Serialize;

use crate::{
    challenge::ChallengeType,
    detect::{DetectedBrowser, DetectionSource},
    types::{BrowserConfig, BrowserKind, ProtectionConfig, ProtectionTrigger},
};

const CONTENTFUL_CHALLENGE_BODY_THRESHOLD: usize = 1_000;
const CONTENTFUL_CHALLENGE_TITLE_THRESHOLD: usize = 20;
const CONTENTFUL_CHALLENGE_SOFT_BODY_THRESHOLD: usize = 400;

#[derive(Debug, Clone)]
pub(crate) struct ProtectionAssessment {
    pub final_url: String,
    pub title_len: usize,
    pub body_text_len: usize,
    pub html_len: usize,
    pub challenge_type: Option<ChallengeType>,
    pub challenge_markers: Vec<String>,
}

impl ProtectionAssessment {
    pub(crate) fn is_content(&self) -> bool {
        self.challenge_type.is_none() && !self.is_empty_shell()
    }

    #[cfg(test)]
    pub(crate) fn is_better_than(&self, other: &Self) -> bool {
        self.body_text_len > other.body_text_len || self.title_len > other.title_len
    }

    pub(crate) fn is_empty_shell(&self) -> bool {
        self.challenge_type.is_none()
            && self.title_len == 0
            && self.body_text_len == 0
            && self.html_len > 0
    }

    pub(crate) fn fallback_trigger(&self) -> Option<ProtectionTrigger> {
        self.challenge_type
            .map(ProtectionTrigger::from)
            .or_else(|| {
                self.is_empty_shell()
                    .then_some(ProtectionTrigger::EmptyShell)
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
    html: &str,
) -> ProtectionAssessment {
    let html_len = html.len();
    let detection = crate::challenge::detect_challenge(html);
    let mut challenge_type = detection.challenge_type;
    let mut challenge_markers: Vec<String> = detection
        .markers
        .into_iter()
        .map(ToString::to_string)
        .collect();

    if should_suppress_challenge(challenge_type, title_len, body_text_len) {
        challenge_type = None;
        challenge_markers.clear();
    }

    ProtectionAssessment {
        final_url,
        title_len,
        body_text_len,
        html_len,
        challenge_type,
        challenge_markers,
    }
}

pub(crate) fn should_wait_for_challenge_resolution(diagnostics: &ProtectionAssessment) -> bool {
    diagnostics.challenge_type.is_some() || diagnostics.is_empty_shell()
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
        let use_channel =
            !matches!(selected.source, DetectionSource::CustomPath | DetectionSource::EnvVar);
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

fn should_suppress_challenge(
    challenge_type: Option<ChallengeType>,
    title_len: usize,
    body_text_len: usize,
) -> bool {
    match challenge_type {
        Some(ChallengeType::Imperva)
        | Some(ChallengeType::Kasada)
        | Some(ChallengeType::Cloudflare)
        | Some(ChallengeType::GenericBrowserCheck) => {
            body_text_len >= CONTENTFUL_CHALLENGE_BODY_THRESHOLD
                || (title_len >= CONTENTFUL_CHALLENGE_TITLE_THRESHOLD
                    && body_text_len >= CONTENTFUL_CHALLENGE_SOFT_BODY_THRESHOLD)
        },
        _ => false,
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
    fn suppresses_contentful_vendor_challenge_pages() {
        let imperva = assess_html(
            "https://coles.com.au".to_string(),
            41,
            6_316,
            "_incapsula_resource pardon our interruption",
        );
        assert!(imperva.is_content());

        let kasada = assess_html(
            "https://realestate.com.au".to_string(),
            60,
            2_385,
            "kpsdk checking your browser",
        );
        assert!(kasada.is_content());
    }

    #[test]
    fn keeps_empty_challenge_shells_flagged() {
        let imperva = assess_html(
            "https://coles.com.au".to_string(),
            0,
            0,
            "_incapsula_resource pardon our interruption",
        );
        assert_eq!(imperva.challenge_type, Some(ChallengeType::Imperva));

        let kasada = assess_html(
            "https://realestate.com.au".to_string(),
            5,
            100,
            "kpsdk checking your browser",
        );
        assert_eq!(kasada.challenge_type, Some(ChallengeType::Kasada));
    }

    #[test]
    fn generic_challenge_pages_remain_flagged() {
        let generic = assess_html(
            "https://example.com".to_string(),
            12,
            150,
            "verify you are human",
        );
        assert_eq!(
            generic.challenge_type,
            Some(ChallengeType::GenericChallenge)
        );
        assert!(!generic.is_content());
    }

    #[test]
    fn patchright_fallback_gate_respects_enabled_sandbox_domain_and_challenge() {
        let mut cfg = ProtectionConfig {
            enabled: true,
            ..ProtectionConfig::default()
        };
        cfg.domains = vec!["*.example.com".to_string()];
        cfg.triggers = vec![ProtectionTrigger::Kasada];

        let kasada = ProtectionAssessment {
            final_url: "https://shop.example.com/".to_string(),
            title_len: 0,
            body_text_len: 0,
            html_len: 128,
            challenge_type: Some(ChallengeType::Kasada),
            challenge_markers: vec!["kpsdk".to_string()],
        };

        assert_eq!(
            protection_trigger_for_fallback(&kasada, false, "https://shop.example.com/", &cfg),
            Some(ProtectionTrigger::Kasada)
        );
        assert_eq!(
            protection_trigger_for_fallback(&kasada, true, "https://shop.example.com/", &cfg),
            None
        );
        assert_eq!(
            protection_trigger_for_fallback(&kasada, false, "https://www.other.com/", &cfg),
            None
        );

        let imperva = ProtectionAssessment {
            challenge_type: Some(ChallengeType::Imperva),
            challenge_markers: vec!["incapsula".to_string()],
            ..kasada.clone()
        };
        assert_eq!(
            protection_trigger_for_fallback(&imperva, false, "https://shop.example.com/", &cfg),
            None
        );

        let empty_shell = ProtectionAssessment {
            final_url: "https://shop.example.com/".to_string(),
            title_len: 0,
            body_text_len: 0,
            html_len: 120,
            challenge_type: None,
            challenge_markers: Vec::new(),
        };
        cfg.triggers = vec![ProtectionTrigger::EmptyShell];
        assert_eq!(
            protection_trigger_for_fallback(&empty_shell, false, "https://shop.example.com/", &cfg),
            Some(ProtectionTrigger::EmptyShell)
        );
    }

    #[test]
    fn waits_for_challenge_resolution_on_empty_shell() {
        let diagnostics = ProtectionAssessment {
            final_url: "https://example.com".to_string(),
            title_len: 0,
            body_text_len: 0,
            html_len: 120,
            challenge_type: None,
            challenge_markers: Vec::new(),
        };
        assert!(should_wait_for_challenge_resolution(&diagnostics));
        assert!(diagnostics.is_empty_shell());
    }

    #[test]
    fn prefers_assessment_with_more_content() {
        let baseline = ProtectionAssessment {
            final_url: "https://example.com".to_string(),
            title_len: 10,
            body_text_len: 200,
            html_len: 400,
            challenge_type: Some(ChallengeType::Kasada),
            challenge_markers: vec!["kpsdk".to_string()],
        };
        let richer = ProtectionAssessment {
            title_len: 20,
            body_text_len: 400,
            ..baseline.clone()
        };

        assert!(richer.is_better_than(&baseline));
    }
}
