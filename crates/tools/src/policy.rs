use {
    serde::{Deserialize, Serialize},
    tracing::debug,
};

/// Glob-based allow/deny policy for tool access.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolPolicy {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

/// Context for resolving which policy layers apply.
#[derive(Debug, Clone, Default)]
pub struct PolicyContext {
    pub agent_id: String,
    pub provider: Option<String>,
    pub channel: Option<String>,
    pub group_id: Option<String>,
    pub sender_id: Option<String>,
    pub sandboxed: bool,
}

/// Predefined tool profiles.
pub fn profile_tools(profile: &str) -> ToolPolicy {
    match profile {
        "minimal" => ToolPolicy {
            allow: vec!["exec".into()],
            deny: Vec::new(),
        },
        "coding" => ToolPolicy {
            allow: vec!["exec".into(), "browser".into(), "memory".into()],
            deny: Vec::new(),
        },
        "full" => ToolPolicy {
            allow: vec!["*".into()],
            deny: Vec::new(),
        },
        _ => ToolPolicy::default(),
    }
}

/// Check if a tool name matches a glob pattern (supports `*` wildcard).
fn pattern_matches(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    pattern == name
}

impl ToolPolicy {
    /// Returns true if the given tool name is allowed by this policy.
    /// Deny always wins over allow.
    pub fn is_allowed(&self, tool_name: &str) -> bool {
        // Check deny first — deny wins.
        for pattern in &self.deny {
            if pattern_matches(pattern, tool_name) {
                debug!(tool = tool_name, pattern, "tool denied by deny list");
                return false;
            }
        }
        // If allow is empty, everything not denied is allowed.
        if self.allow.is_empty() {
            return true;
        }
        // Otherwise, must match an allow pattern.
        for pattern in &self.allow {
            if pattern_matches(pattern, tool_name) {
                return true;
            }
        }
        debug!(tool = tool_name, "tool denied: not in allow list");
        false
    }

    /// Merge another policy on top of this one (the `other` has higher precedence).
    /// Non-empty lists from `other` replace those from `self`.
    pub fn merge_with(&self, other: &ToolPolicy) -> ToolPolicy {
        ToolPolicy {
            allow: if other.allow.is_empty() {
                self.allow.clone()
            } else {
                other.allow.clone()
            },
            deny: {
                let mut combined = self.deny.clone();
                combined.extend(other.deny.iter().cloned());
                combined
            },
        }
    }
}

/// Resolve the effective policy by merging layers from config.
///
/// Layer precedence (later wins for allow, deny always accumulates):
/// 1. Global
/// 2. Per-provider
/// 3. Per-agent
/// 4. Per-group
/// 5. Per-sender in group
/// 6. Sandbox-specific
pub fn resolve_effective_policy(config: &serde_json::Value, context: &PolicyContext) -> ToolPolicy {
    let mut effective = ToolPolicy::default();

    // Layer 1: Global — tools.policy
    if let Some(global) = config.pointer("/tools/policy")
        && let Ok(p) = serde_json::from_value::<ToolPolicy>(global.clone())
    {
        effective = effective.merge_with(&p);
        debug!("policy: applied global layer");
    }

    // Layer 2: Per-provider — tools.providers.<provider>.policy
    if let Some(ref provider) = context.provider {
        let pointer = format!("/tools/providers/{}/policy", provider);
        if let Some(prov) = config.pointer(&pointer)
            && let Ok(p) = serde_json::from_value::<ToolPolicy>(prov.clone())
        {
            effective = effective.merge_with(&p);
            debug!(provider, "policy: applied provider layer");
        }
    }

    // Layer 3: Per-agent — agents.list[agent_id].tools.policy
    // We scan the agents list for a matching agent_id.
    if let Some(agents) = config.pointer("/agents/list").and_then(|v| v.as_array()) {
        for agent in agents {
            let id = agent.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id == context.agent_id {
                if let Some(agent_policy) = agent.pointer("/tools/policy")
                    && let Ok(p) = serde_json::from_value::<ToolPolicy>(agent_policy.clone())
                {
                    effective = effective.merge_with(&p);
                    debug!(agent_id = %context.agent_id, "policy: applied agent layer");
                }
                break;
            }
        }
    }

    // Layer 4: Per-group — channels.<ch>.groups.<gid>.tools.policy
    if let Some(ref channel) = context.channel
        && let Some(ref group_id) = context.group_id
    {
        let pointer = format!("/channels/{}/groups/{}/tools/policy", channel, group_id);
        if let Some(group) = config.pointer(&pointer)
            && let Ok(p) = serde_json::from_value::<ToolPolicy>(group.clone())
        {
            effective = effective.merge_with(&p);
            debug!(channel, group_id, "policy: applied group layer");
        }
    }

    // Layer 5: Per-sender — channels.<ch>.groups.<gid>.tools.bySender.<sender>
    if let Some(ref channel) = context.channel
        && let Some(ref group_id) = context.group_id
        && let Some(ref sender_id) = context.sender_id
    {
        let pointer = format!(
            "/channels/{}/groups/{}/tools/bySender/{}",
            channel, group_id, sender_id
        );
        if let Some(sender) = config.pointer(&pointer)
            && let Ok(p) = serde_json::from_value::<ToolPolicy>(sender.clone())
        {
            effective = effective.merge_with(&p);
            debug!(channel, group_id, sender_id, "policy: applied sender layer");
        }
    }

    // Layer 6: Sandbox overrides
    if context.sandboxed
        && let Some(sandbox_policy) = config.pointer("/tools/exec/sandbox/tools")
        && let Ok(p) = serde_json::from_value::<ToolPolicy>(sandbox_policy.clone())
    {
        effective = effective.merge_with(&p);
        debug!("policy: applied sandbox layer");
    }

    effective
}

/// Resolve the top-level tool policy from typed config (profile + allow/deny).
///
/// This is the simplified single-layer resolution used at session boundaries.
/// For multi-layer context-aware resolution, use [`resolve_effective_policy`].
pub fn effective_tool_policy(config: &moltis_config::MoltisConfig) -> ToolPolicy {
    let mut effective = ToolPolicy::default();
    if let Some(profile) = config.tools.policy.profile.as_deref()
        && !profile.is_empty()
    {
        effective = effective.merge_with(&profile_tools(profile));
    }
    let configured = ToolPolicy {
        allow: config.tools.policy.allow.clone(),
        deny: config.tools.policy.deny.clone(),
    };
    effective.merge_with(&configured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allow_all() {
        let policy = ToolPolicy {
            allow: vec!["*".into()],
            deny: Vec::new(),
        };
        assert!(policy.is_allowed("exec"));
        assert!(policy.is_allowed("browser"));
    }

    #[test]
    fn test_deny_wins() {
        let policy = ToolPolicy {
            allow: vec!["*".into()],
            deny: vec!["exec".into()],
        };
        assert!(!policy.is_allowed("exec"));
        assert!(policy.is_allowed("browser"));
    }

    #[test]
    fn test_prefix_pattern() {
        let policy = ToolPolicy {
            allow: vec!["browser*".into()],
            deny: Vec::new(),
        };
        assert!(policy.is_allowed("browser"));
        assert!(policy.is_allowed("browser.fetch"));
        assert!(!policy.is_allowed("exec"));
    }

    #[test]
    fn test_empty_allow_permits_all() {
        let policy = ToolPolicy {
            allow: Vec::new(),
            deny: Vec::new(),
        };
        assert!(policy.is_allowed("exec"));
    }

    #[test]
    fn test_merge_deny_accumulates() {
        let base = ToolPolicy {
            allow: vec!["*".into()],
            deny: vec!["dangerous".into()],
        };
        let overlay = ToolPolicy {
            allow: Vec::new(),
            deny: vec!["exec".into()],
        };
        let merged = base.merge_with(&overlay);
        assert!(!merged.is_allowed("dangerous"));
        assert!(!merged.is_allowed("exec"));
        assert!(merged.is_allowed("browser"));
    }

    #[test]
    fn test_profiles() {
        let minimal = profile_tools("minimal");
        assert!(minimal.is_allowed("exec"));
        assert!(!minimal.is_allowed("browser"));

        let full = profile_tools("full");
        assert!(full.is_allowed("anything"));
    }

    #[test]
    fn test_resolve_global_policy() {
        let config = serde_json::json!({
            "tools": {
                "policy": {
                    "allow": ["exec"],
                    "deny": ["dangerous"]
                }
            }
        });
        let ctx = PolicyContext {
            agent_id: "test".into(),
            ..Default::default()
        };
        let policy = resolve_effective_policy(&config, &ctx);
        assert!(policy.is_allowed("exec"));
        assert!(!policy.is_allowed("dangerous"));
        assert!(!policy.is_allowed("browser"));
    }

    #[test]
    fn test_resolve_deny_wins_across_layers() {
        let config = serde_json::json!({
            "tools": {
                "policy": {
                    "allow": ["*"],
                    "deny": []
                },
                "providers": {
                    "openai": {
                        "policy": {
                            "deny": ["exec"]
                        }
                    }
                }
            }
        });
        let ctx = PolicyContext {
            agent_id: "test".into(),
            provider: Some("openai".into()),
            ..Default::default()
        };
        let policy = resolve_effective_policy(&config, &ctx);
        assert!(!policy.is_allowed("exec"));
        assert!(policy.is_allowed("browser"));
    }
}
