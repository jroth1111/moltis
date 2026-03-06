//! Tool attenuation for skills: restrict available tools based on active skill trust levels.

use std::collections::HashSet;

use crate::types::SkillTrust;

/// Tools available in read-only (pending/installed) mode.
/// This is the minimum set that lets an installed skill be useful
/// without being able to exfiltrate data or run arbitrary code.
const READ_ONLY_TOOLS: &[&str] = &["memory_search", "read_file", "list_directory", "echo"];

/// Result of resolving `allowed_tools` specs against a runtime tool list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedToolsResolution<'a> {
    /// Runtime tool names that matched at least one allowed-tools spec.
    pub matched_tools: Vec<&'a str>,
    /// Allowed-tools specs that did not match any runtime tool name.
    pub unmatched_specs: Vec<String>,
}

fn normalize_tool_token(raw: &str, keep_wildcard: bool) -> String {
    let mut normalized = String::with_capacity(raw.len());
    let mut prev_separator = true;
    let mut prev_lower_or_digit = false;

    for ch in raw.trim().chars() {
        if keep_wildcard && ch == '*' {
            if !normalized.ends_with('*') {
                normalized.push('*');
            }
            prev_separator = false;
            prev_lower_or_digit = false;
            continue;
        }

        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() && prev_lower_or_digit && !normalized.ends_with('_') {
                normalized.push('_');
            }
            normalized.push(ch.to_ascii_lowercase());
            prev_separator = false;
            prev_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
            continue;
        }

        if !prev_separator && !normalized.ends_with('_') && !normalized.ends_with('*') {
            normalized.push('_');
        }
        prev_separator = true;
        prev_lower_or_digit = false;
    }

    normalized.trim_matches('_').to_string()
}

fn expand_allowed_tool_patterns(tool_spec: &str) -> Vec<String> {
    let trimmed = tool_spec.trim();
    if trimmed.contains('(') || trimmed.contains(')') {
        return Vec::new();
    }

    let normalized = normalize_tool_token(trimmed, true);
    if normalized.is_empty() {
        return Vec::new();
    }

    vec![normalized]
}

fn wildcard_match(pattern: &str, candidate: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == candidate;
    }

    // Standard linear wildcard matching with '*' = any sequence.
    let pattern_bytes = pattern.as_bytes();
    let candidate_bytes = candidate.as_bytes();
    let mut pattern_idx = 0usize;
    let mut candidate_idx = 0usize;
    let mut star_idx: Option<usize> = None;
    let mut star_match_idx = 0usize;

    while candidate_idx < candidate_bytes.len() {
        if pattern_idx < pattern_bytes.len()
            && pattern_bytes[pattern_idx] == candidate_bytes[candidate_idx]
        {
            pattern_idx += 1;
            candidate_idx += 1;
            continue;
        }

        if pattern_idx < pattern_bytes.len() && pattern_bytes[pattern_idx] == b'*' {
            star_idx = Some(pattern_idx);
            pattern_idx += 1;
            star_match_idx = candidate_idx;
            continue;
        }

        if let Some(star_pos) = star_idx {
            pattern_idx = star_pos + 1;
            star_match_idx += 1;
            candidate_idx = star_match_idx;
            continue;
        }

        return false;
    }

    while pattern_idx < pattern_bytes.len() && pattern_bytes[pattern_idx] == b'*' {
        pattern_idx += 1;
    }
    pattern_idx == pattern_bytes.len()
}

fn tool_matches_patterns(tool_name: &str, allowed_patterns: &HashSet<String>) -> bool {
    let normalized_tool = normalize_tool_token(tool_name, false);
    allowed_patterns
        .iter()
        .any(|pattern| wildcard_match(pattern, &normalized_tool))
}

/// Resolve `allowed-tools` specs against runtime tool names.
///
/// Empty `allowed_specs` means no explicit skill-level restriction.
#[must_use]
pub fn resolve_allowed_tools<'a>(
    allowed_specs: &[String],
    all_tool_names: &[&'a str],
) -> AllowedToolsResolution<'a> {
    if allowed_specs.is_empty() {
        return AllowedToolsResolution {
            matched_tools: all_tool_names.to_vec(),
            unmatched_specs: Vec::new(),
        };
    }

    let spec_patterns: Vec<(&str, Vec<String>)> = allowed_specs
        .iter()
        .map(|spec| (spec.as_str(), expand_allowed_tool_patterns(spec)))
        .collect();
    let allowed_patterns: HashSet<String> = spec_patterns
        .iter()
        .flat_map(|(_, patterns)| patterns.iter().cloned())
        .collect();

    let matched_tools: Vec<&str> = all_tool_names
        .iter()
        .copied()
        .filter(|name| tool_matches_patterns(name, &allowed_patterns))
        .collect();

    let unmatched_specs: Vec<String> = spec_patterns
        .iter()
        .filter_map(|(spec, patterns)| {
            let has_match = patterns
                .iter()
                .any(|pattern| matched_tools.iter().any(|tool| wildcard_match(pattern, tool)));
            if has_match {
                None
            } else {
                Some((*spec).to_string())
            }
        })
        .collect();

    AllowedToolsResolution {
        matched_tools,
        unmatched_specs,
    }
}

fn is_read_only_tool(tool_name: &str) -> bool {
    let normalized_tool = normalize_tool_token(tool_name, false);
    READ_ONLY_TOOLS
        .iter()
        .copied()
        .any(|allowed| allowed == normalized_tool)
}

/// Compute the effective tool list given a set of active skills and the full tool list.
///
/// Rules:
/// 1. No active skills -> return all tools unchanged.
/// 2. All active skills are `Trusted` -> return union of `allowed_tools` across all skills.
///    If `allowed_tools` is empty for all skills, return no tools (default-deny).
/// 3. Any `Installed` skill is active -> return read-only subset only.
#[must_use]
pub fn attenuate_tools<'a>(
    active_skills: &[(&crate::types::SkillMetadata, SkillTrust)],
    all_tool_names: &[&'a str],
) -> Vec<&'a str> {
    if active_skills.is_empty() {
        return all_tool_names.to_vec();
    }

    let has_installed = active_skills
        .iter()
        .any(|(_, trust)| *trust == SkillTrust::Installed);

    if has_installed {
        return all_tool_names
            .iter()
            .copied()
            .filter(|name| is_read_only_tool(name))
            .collect();
    }

    // All trusted: collect union of allowed_tools
    let allowed_patterns: HashSet<String> = active_skills
        .iter()
        .flat_map(|(meta, _)| meta.allowed_tools.iter())
        .flat_map(|tool_spec| expand_allowed_tool_patterns(tool_spec))
        .collect();

    if allowed_patterns.is_empty() {
        return Vec::new();
    }

    all_tool_names
        .iter()
        .copied()
        .filter(|name| tool_matches_patterns(name, &allowed_patterns))
        .collect()
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::types::{SkillEvals, SkillMetadata, SkillPermissions, SkillRequirements, SkillTriggers},
        std::path::PathBuf,
    };

    fn mock_skill(allowed_tools: Vec<String>) -> SkillMetadata {
        let permissions = SkillPermissions {
            allowed_tools: allowed_tools.clone(),
        };
        SkillMetadata {
            version: 3,
            name: "test".to_string(),
            description: String::new(),
            triggers: SkillTriggers::default(),
            evals: SkillEvals::default(),
            permissions,
            homepage: None,
            license: None,
            compatibility: None,
            allowed_tools,
            dockerfile: None,
            requires: SkillRequirements::default(),
            path: PathBuf::new(),
            source: None,
        }
    }

    #[test]
    fn no_active_skills_returns_all() {
        let all = &["read_file", "exec", "web_search"];
        let result = attenuate_tools(&[], all);
        assert_eq!(result, all.to_vec());
    }

    #[test]
    fn installed_skill_restricts_to_read_only() {
        let skill = mock_skill(vec![]);
        let all = &["read_file", "exec", "web_search", "memory_search"];
        let result = attenuate_tools(&[(&skill, SkillTrust::Installed)], all);
        assert!(!result.contains(&"exec"));
        assert!(result.contains(&"read_file"));
        assert!(!result.contains(&"web_search"));
    }

    #[test]
    fn trusted_skill_with_allowed_tools_restricts() {
        let skill = mock_skill(vec!["read_file".to_string(), "web_search".to_string()]);
        let all = &["read_file", "exec", "web_search", "memory_search"];
        let result = attenuate_tools(&[(&skill, SkillTrust::Trusted)], all);
        assert!(result.contains(&"read_file"));
        assert!(result.contains(&"web_search"));
        assert!(!result.contains(&"exec"));
        assert!(!result.contains(&"memory_search"));
    }

    #[test]
    fn trusted_skill_no_allowed_tools_returns_none() {
        let skill = mock_skill(vec![]);
        let all = &["read_file", "exec", "web_search"];
        let result = attenuate_tools(&[(&skill, SkillTrust::Trusted)], all);
        assert!(result.is_empty());
    }

    #[test]
    fn any_installed_wins_over_trusted() {
        let trusted = mock_skill(vec!["exec".to_string()]);
        let installed = mock_skill(vec![]);
        let all = &["read_file", "exec", "web_search"];
        let result = attenuate_tools(
            &[
                (&trusted, SkillTrust::Trusted),
                (&installed, SkillTrust::Installed),
            ],
            all,
        );
        assert!(!result.contains(&"exec"));
    }

    #[test]
    fn trusted_skill_allowed_tools_rejects_wrapped_specs() {
        let skill = mock_skill(vec![
            "Bash(git:*)".to_string(),
            "web_fetch".to_string(),
        ]);
        let all = &[
            "exec",
            "read_file",
            "list_directory",
            "web_fetch",
            "web_search",
        ];

        let result = attenuate_tools(&[(&skill, SkillTrust::Trusted)], all);

        assert!(result.contains(&"web_fetch"));
        assert!(!result.contains(&"exec"));
        assert!(!result.contains(&"read_file"));
        assert!(!result.contains(&"list_directory"));
        assert!(!result.contains(&"web_search"));
    }

    #[test]
    fn trusted_skill_allowed_tools_support_wildcards() {
        let skill = mock_skill(vec!["web*".to_string()]);
        let all = &["web_fetch", "web_search", "exec"];

        let result = attenuate_tools(&[(&skill, SkillTrust::Trusted)], all);

        assert!(result.contains(&"web_fetch"));
        assert!(result.contains(&"web_search"));
        assert!(!result.contains(&"exec"));
    }

    #[test]
    fn resolve_allowed_tools_rejects_wrapped_specs() {
        let specs = vec!["Bash(git:*)".to_string(), "web_fetch".to_string()];
        let all = &[
            "exec",
            "read_file",
            "list_directory",
            "web_fetch",
            "web_search",
        ];

        let resolved = resolve_allowed_tools(&specs, all);

        assert_eq!(resolved.matched_tools, vec!["web_fetch"]);
        assert_eq!(resolved.unmatched_specs, vec!["Bash(git:*)".to_string()]);
    }

    #[test]
    fn resolve_allowed_tools_tracks_unmatched_specs() {
        let specs = vec!["web_search".to_string(), "Bash(git:*)".to_string()];
        let all = &["exec", "read_file"];

        let resolved = resolve_allowed_tools(&specs, all);

        assert!(resolved.matched_tools.is_empty());
        assert_eq!(
            resolved.unmatched_specs,
            vec!["web_search".to_string(), "Bash(git:*)".to_string()]
        );
    }

    #[test]
    fn resolve_allowed_tools_empty_specs_is_unrestricted() {
        let all = &["exec", "web_fetch"];

        let resolved = resolve_allowed_tools(&[], all);

        assert_eq!(resolved.matched_tools, all.to_vec());
        assert!(resolved.unmatched_specs.is_empty());
    }

    #[test]
    fn trusted_skill_matches_normalized_runtime_names() {
        let skill = mock_skill(vec!["web_fetch".to_string()]);
        let all = &["WebFetch", "web-fetch", "exec"];

        let result = attenuate_tools(&[(&skill, SkillTrust::Trusted)], all);

        assert!(result.contains(&"WebFetch"));
        assert!(result.contains(&"web-fetch"));
        assert!(!result.contains(&"exec"));
    }

    #[test]
    fn installed_skill_profile_excludes_network_egress() {
        let skill = mock_skill(vec![]);
        let all = &[
            "read_file",
            "list_directory",
            "memory_search",
            "echo",
            "web_search",
            "web_fetch",
            "exec",
        ];

        let result = attenuate_tools(&[(&skill, SkillTrust::Installed)], all);

        assert!(result.contains(&"read_file"));
        assert!(result.contains(&"list_directory"));
        assert!(result.contains(&"memory_search"));
        assert!(result.contains(&"echo"));
        assert!(!result.contains(&"web_search"));
        assert!(!result.contains(&"web_fetch"));
        assert!(!result.contains(&"exec"));
    }
}
