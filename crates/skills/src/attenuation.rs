//! Tool attenuation for skills: restrict available tools based on active skill trust levels.

use crate::types::SkillTrust;

/// Tools available in read-only (untrusted/installed) mode.
/// This is the minimum set that lets an installed skill be useful
/// without being able to exfiltrate data or run arbitrary code.
const READ_ONLY_TOOLS: &[&str] = &[
    "memory_search",
    "read_file",
    "list_directory",
    "web_search",
    "echo",
];

/// Compute the effective tool list given a set of active skills and the full tool list.
///
/// Rules:
/// 1. No active skills -> return all tools unchanged.
/// 2. All active skills are `Trusted` -> return union of `allowed_tools` across all skills.
///    If `allowed_tools` is empty for all skills, return all tools (backward compat).
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
            .filter(|name| READ_ONLY_TOOLS.contains(name))
            .collect();
    }

    // All trusted: collect union of allowed_tools
    let allowed: std::collections::HashSet<&str> = active_skills
        .iter()
        .flat_map(|(meta, _)| meta.allowed_tools.iter().map(String::as_str))
        .collect();

    if allowed.is_empty() {
        // No restrictions declared -> all tools available
        return all_tool_names.to_vec();
    }

    all_tool_names
        .iter()
        .copied()
        .filter(|name| allowed.contains(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SkillMetadata, SkillRequirements};
    use std::path::PathBuf;

    fn mock_skill(allowed_tools: Vec<String>) -> SkillMetadata {
        SkillMetadata {
            name: "test".to_string(),
            description: String::new(),
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
    fn trusted_skill_no_allowed_tools_returns_all() {
        let skill = mock_skill(vec![]);
        let all = &["read_file", "exec", "web_search"];
        let result = attenuate_tools(&[(&skill, SkillTrust::Trusted)], all);
        assert_eq!(result, all.to_vec());
    }

    #[test]
    fn any_installed_wins_over_trusted() {
        let trusted = mock_skill(vec!["exec".to_string()]);
        let installed = mock_skill(vec![]);
        let all = &["read_file", "exec", "web_search"];
        let result = attenuate_tools(
            &[(&trusted, SkillTrust::Trusted), (&installed, SkillTrust::Installed)],
            all,
        );
        assert!(!result.contains(&"exec"));
    }
}
