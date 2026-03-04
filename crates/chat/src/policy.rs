use serde::Serialize;

use moltis_agents::prompt::PromptRuntimeContext;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyOutcome {
    Allow,
    Deny,
    AllowWithTransforms,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyReasonCode {
    SurfaceNonChannel,
    SurfaceUnclassified,
    SurfacePrivate,
    SurfaceNonPrivate,
    UnknownToolSideEffect,
    MissingExplicitIntent,
    SchemaInvalid,
    InvalidSoulMarker,
    OrphanSoulMarker,
    DuplicateSoulSection,
    MemoryIrrelevant,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyTransform {
    pub kind: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyDecision {
    pub code: PolicyReasonCode,
    pub outcome: PolicyOutcome,
    pub detail: String,
    pub transforms: Vec<PolicyTransform>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptPolicyEvaluation {
    pub include_private_persona_data: bool,
    pub include_memory_bootstrap: bool,
    pub allow_external_effects: bool,
    pub decisions: Vec<PolicyDecision>,
}

impl PromptPolicyEvaluation {
    fn private_default() -> Self {
        Self {
            include_private_persona_data: true,
            include_memory_bootstrap: true,
            allow_external_effects: true,
            decisions: vec![PolicyDecision {
                code: PolicyReasonCode::SurfaceNonChannel,
                outcome: PolicyOutcome::Allow,
                detail: "non-channel runtime surface keeps private persona data".to_string(),
                transforms: Vec::new(),
            }],
        }
    }
}

pub fn evaluate_surface_policy(
    channel_type: Option<&str>,
    channel_chat_type: Option<&str>,
) -> PromptPolicyEvaluation {
    let Some(channel_type) = channel_type else {
        return PromptPolicyEvaluation::private_default();
    };

    let channel_type = channel_type.to_ascii_lowercase();
    let Some(chat_type) = channel_chat_type else {
        return PromptPolicyEvaluation {
            include_private_persona_data: false,
            include_memory_bootstrap: false,
            allow_external_effects: false,
            decisions: vec![PolicyDecision {
                code: PolicyReasonCode::SurfaceUnclassified,
                outcome: PolicyOutcome::Deny,
                detail: format!(
                    "channel surface '{channel_type}' has unknown chat type; fail-closed policy applied"
                ),
                transforms: Vec::new(),
            }],
        };
    };

    let is_private = match channel_type.as_str() {
        "telegram" | "whatsapp" | "discord" | "msteams" => {
            chat_type.eq_ignore_ascii_case("private")
        },
        _ => false,
    };

    if is_private {
        return PromptPolicyEvaluation {
            include_private_persona_data: true,
            include_memory_bootstrap: true,
            allow_external_effects: true,
            decisions: vec![PolicyDecision {
                code: PolicyReasonCode::SurfacePrivate,
                outcome: PolicyOutcome::Allow,
                detail: format!(
                    "channel surface '{channel_type}' is explicitly private; private persona data allowed"
                ),
                transforms: Vec::new(),
            }],
        };
    }

    PromptPolicyEvaluation {
        include_private_persona_data: false,
        include_memory_bootstrap: false,
        allow_external_effects: false,
        decisions: vec![PolicyDecision {
            code: PolicyReasonCode::SurfaceNonPrivate,
            outcome: PolicyOutcome::Deny,
            detail: format!(
                "channel surface '{channel_type}' with chat type '{chat_type}' is non-private"
            ),
            transforms: Vec::new(),
        }],
    }
}

pub fn evaluate_runtime_policy(
    runtime_context: Option<&PromptRuntimeContext>,
) -> PromptPolicyEvaluation {
    let Some(runtime) = runtime_context else {
        return PromptPolicyEvaluation::private_default();
    };
    evaluate_surface_policy(
        runtime.host.channel_type.as_deref(),
        runtime.host.channel_chat_type.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_channel_surface_keeps_private_persona_data() {
        let eval = evaluate_surface_policy(None, None);
        assert!(eval.include_private_persona_data);
        assert!(eval.include_memory_bootstrap);
        assert!(eval.allow_external_effects);
    }

    #[test]
    fn unclassified_channel_surface_is_fail_closed() {
        let eval = evaluate_surface_policy(Some("discord"), None);
        assert!(!eval.include_private_persona_data);
        assert!(!eval.include_memory_bootstrap);
        assert!(!eval.allow_external_effects);
    }

    #[test]
    fn private_channel_surface_allows_private_persona_data() {
        let eval = evaluate_surface_policy(Some("telegram"), Some("private"));
        assert!(eval.include_private_persona_data);
        assert!(eval.include_memory_bootstrap);
        assert!(eval.allow_external_effects);
    }

    #[test]
    fn non_private_channel_surface_blocks_private_persona_data() {
        let eval = evaluate_surface_policy(Some("whatsapp"), Some("group"));
        assert!(!eval.include_private_persona_data);
        assert!(!eval.include_memory_bootstrap);
        assert!(!eval.allow_external_effects);
    }
}
