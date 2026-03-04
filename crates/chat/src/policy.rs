use serde::Serialize;

use moltis_agents::prompt::PromptRuntimeContext;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyDecision {
    pub code: &'static str,
    pub outcome: &'static str,
    pub detail: String,
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
                code: "surface_non_channel",
                outcome: "allow",
                detail: "non-channel runtime surface keeps private persona data".to_string(),
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
                code: "surface_unclassified",
                outcome: "deny",
                detail: format!(
                    "channel surface '{channel_type}' has unknown chat type; fail-closed policy applied"
                ),
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
                code: "surface_private",
                outcome: "allow",
                detail: format!(
                    "channel surface '{channel_type}' is explicitly private; private persona data allowed"
                ),
            }],
        };
    }

    PromptPolicyEvaluation {
        include_private_persona_data: false,
        include_memory_bootstrap: false,
        allow_external_effects: false,
        decisions: vec![PolicyDecision {
            code: "surface_non_private",
            outcome: "deny",
            detail: format!(
                "channel surface '{channel_type}' with chat type '{chat_type}' is non-private"
            ),
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
