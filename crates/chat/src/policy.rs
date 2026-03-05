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
    pub decisions: Vec<PolicyDecision>,
}

impl PromptPolicyEvaluation {
    fn surface_allows_private(&self) -> bool {
        self.decisions
            .iter()
            .find_map(|decision| match decision.code {
                PolicyReasonCode::SurfacePrivate | PolicyReasonCode::SurfaceNonChannel => {
                    Some(true)
                },
                PolicyReasonCode::SurfaceNonPrivate | PolicyReasonCode::SurfaceUnclassified => {
                    Some(false)
                },
                _ => None,
            })
            .unwrap_or(true)
    }

    #[must_use]
    pub fn include_private_persona_data(&self) -> bool {
        self.surface_allows_private()
    }

    #[must_use]
    pub fn include_memory_bootstrap(&self) -> bool {
        self.surface_allows_private()
    }

    #[must_use]
    pub fn allow_external_effects(&self) -> bool {
        self.surface_allows_private()
    }

    fn private_default() -> Self {
        Self {
            decisions: vec![PolicyDecision {
                code: PolicyReasonCode::SurfaceNonChannel,
                outcome: PolicyOutcome::Allow,
                detail: "non-channel runtime surface keeps private persona data".to_string(),
                transforms: Vec::new(),
            }],
        }
    }
}

fn private_persona_strip_transforms() -> Vec<PolicyTransform> {
    vec![
        PolicyTransform {
            kind: "strip_private_persona_data",
            detail: "remove USER.md private fields for non-private surfaces".to_string(),
        },
        PolicyTransform {
            kind: "drop_memory_bootstrap",
            detail: "exclude MEMORY.md bootstrap on non-private/unclassified surfaces".to_string(),
        },
    ]
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
            decisions: vec![PolicyDecision {
                code: PolicyReasonCode::SurfaceUnclassified,
                outcome: PolicyOutcome::AllowWithTransforms,
                detail: format!(
                    "channel surface '{channel_type}' has unknown chat type; fail-closed policy applied"
                ),
                transforms: private_persona_strip_transforms(),
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
        decisions: vec![PolicyDecision {
            code: PolicyReasonCode::SurfaceNonPrivate,
            outcome: PolicyOutcome::AllowWithTransforms,
            detail: format!(
                "channel surface '{channel_type}' with chat type '{chat_type}' is non-private"
            ),
            transforms: private_persona_strip_transforms(),
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
        assert!(eval.include_private_persona_data());
        assert!(eval.include_memory_bootstrap());
        assert!(eval.allow_external_effects());
    }

    #[test]
    fn unclassified_channel_surface_is_fail_closed() {
        let eval = evaluate_surface_policy(Some("discord"), None);
        assert!(!eval.include_private_persona_data());
        assert!(!eval.include_memory_bootstrap());
        assert!(!eval.allow_external_effects());
        assert_eq!(
            eval.decisions[0].outcome,
            PolicyOutcome::AllowWithTransforms
        );
        assert!(!eval.decisions[0].transforms.is_empty());
    }

    #[test]
    fn private_channel_surface_allows_private_persona_data() {
        let eval = evaluate_surface_policy(Some("telegram"), Some("private"));
        assert!(eval.include_private_persona_data());
        assert!(eval.include_memory_bootstrap());
        assert!(eval.allow_external_effects());
    }

    #[test]
    fn non_private_channel_surface_blocks_private_persona_data() {
        let eval = evaluate_surface_policy(Some("whatsapp"), Some("group"));
        assert!(!eval.include_private_persona_data());
        assert!(!eval.include_memory_bootstrap());
        assert!(!eval.allow_external_effects());
        assert_eq!(
            eval.decisions[0].outcome,
            PolicyOutcome::AllowWithTransforms
        );
        assert!(!eval.decisions[0].transforms.is_empty());
    }
}
