//! Keyword-based tool selector for dynamic tool scoping.
//!
//! Matches task descriptions against a static keyword→category table and
//! returns a filtered tool registry containing only tools whose categories
//! overlap with the matched keywords. Uncategorized extension tools (MCP/WASM)
//! are retained, while uncategorized built-ins are excluded when scoping is
//! active. If no keywords match, all tools are returned.

use std::collections::HashSet;

use moltis_agents::tool_registry::{ToolRegistry, ToolSource};
#[cfg(test)]
use crate::tool_names::WEB_FETCH;
/// Static keyword→category mapping. Each entry maps a lowercase keyword
/// (or phrase) to the tool categories it implies.
const KEYWORD_CATEGORIES: &[(&str, &[&str])] = &[
    // Web
    ("search", &["web"]),
    ("browse", &["web"]),
    ("fetch", &["web"]),
    ("url", &["web"]),
    ("http", &["web"]),
    ("website", &["web"]),
    ("web page", &["web"]),
    ("scrape", &["web"]),
    ("download", &["web"]),
    // Code / files
    ("code", &["code", "files"]),
    ("write", &["code", "files"]),
    ("edit", &["code", "files"]),
    ("refactor", &["code", "files"]),
    ("implement", &["code", "files"]),
    ("compile", &["code"]),
    ("build", &["code"]),
    ("run", &["code"]),
    ("execute", &["code"]),
    ("test", &["code"]),
    ("debug", &["code"]),
    ("fix", &["code", "files"]),
    ("file", &["code", "files"]),
    ("create", &["code", "files"]),
    ("install", &["code"]),
    ("package", &["code"]),
    ("deploy", &["code"]),
    // Skills
    ("skill", &["skills"]),
    ("skills", &["skills"]),
    // Session management
    ("session", &["session"]),
    ("branch", &["session"]),
    ("history", &["session"]),
    // Destructive intent
    ("delete", &["destructive"]),
    ("remove", &["destructive"]),
    ("destroy", &["destructive"]),
    ("drop", &["destructive"]),
    // Scheduling
    ("schedule", &["scheduling"]),
    ("cron", &["scheduling"]),
    ("timer", &["scheduling"]),
    ("recurring", &["scheduling"]),
    // Location
    ("map", &["location"]),
    ("location", &["location"]),
    ("address", &["location"]),
    ("directions", &["location"]),
    // Orchestration
    ("delegate", &["orchestration"]),
    ("spawn", &["orchestration"]),
    ("sub-agent", &["orchestration"]),
    ("task", &["orchestration"]),
    ("coordinate", &["orchestration"]),
    // Images
    ("image", &["files"]),
    ("photo", &["files"]),
    ("picture", &["files"]),
    ("screenshot", &["files"]),
    // Memory
    ("remember", &["memory"]),
    ("recall", &["memory"]),
    ("memory", &["memory"]),
];

fn contains_keyword(haystack: &str, keyword: &str) -> bool {
    if keyword.is_empty() {
        return false;
    }

    // Phrases/special tokens are matched as-is.
    if keyword
        .bytes()
        .any(|b| !b.is_ascii_lowercase() && !b.is_ascii_digit())
    {
        return haystack.contains(keyword);
    }

    for (idx, _) in haystack.match_indices(keyword) {
        let before_ok = idx == 0
            || !haystack
                .as_bytes()
                .get(idx.wrapping_sub(1))
                .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        let end = idx + keyword.len();
        let after_ok = end == haystack.len()
            || !haystack
                .as_bytes()
                .get(end)
                .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

/// Select tools relevant to a task description by matching keywords to categories.
///
/// Returns a filtered registry containing only tools whose categories intersect
/// with the matched keyword categories. Uncategorized extension tools (e.g. MCP)
/// are retained during scoping. If no keywords match, returns all tools.
pub fn select_tools_for_task(task: &str, registry: &ToolRegistry) -> ToolRegistry {
    let lower = task.to_ascii_lowercase();

    let mut matched_categories: HashSet<&str> = HashSet::new();
    for (keyword, categories) in KEYWORD_CATEGORIES {
        if contains_keyword(&lower, keyword) {
            matched_categories.extend(categories.iter());
        }
    }

    // No keywords matched → return all tools (safe fallback)
    if matched_categories.is_empty() {
        return registry.clone_without(&[]);
    }

    // Keep tools whose categories intersect with matched set.
    // Uncategorized built-ins are excluded when scoping is active.
    registry.clone_allowed_by(|name| {
        let cats = registry.categories_for(name);
        // Keep uncategorized extension tools (MCP/WASM), but not uncategorized built-ins.
        if cats.is_empty() {
            return matches!(
                registry.source_for(name),
                Some(ToolSource::Mcp { .. }) | Some(ToolSource::Wasm { .. })
            );
        }
        let has_destructive_category = cats.contains(&"destructive");
        if has_destructive_category {
            // Destructive tools require explicit destructive intent, and at
            // least one matching non-destructive domain category.
            if !matched_categories.contains("destructive") {
                return false;
            }
            let has_domain_match = cats
                .iter()
                .any(|c| *c != "destructive" && matched_categories.contains(c));
            if !has_domain_match {
                return false;
            }
        }
        // Include if any category matches.
        cats.iter().any(|c| matched_categories.contains(c))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct FakeTool {
        name: &'static str,
        cats: &'static [&'static str],
    }

    #[async_trait::async_trait]
    impl moltis_agents::tool_registry::AgentTool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "test"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn execute(&self, _params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
            Ok(json!("ok"))
        }
        fn categories(&self) -> &'static [&'static str] {
            self.cats
        }
    }

    fn test_registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(FakeTool {
            name: "exec",
            cats: &["code", "files"],
        }));
        reg.register(Box::new(FakeTool {
            name: WEB_FETCH,
            cats: &["web"],
        }));
        reg.register(Box::new(FakeTool {
            name: "web_search",
            cats: &["web"],
        }));
        reg.register(Box::new(FakeTool {
            name: "sessions_list",
            cats: &["session"],
        }));
        reg.register(Box::new(FakeTool {
            name: "sessions_delete",
            cats: &["session", "destructive"],
        }));
        reg.register(Box::new(FakeTool {
            name: "create_skill",
            cats: &["skills"],
        }));
        reg.register(Box::new(FakeTool {
            name: "delete_skill",
            cats: &["skills", "destructive"],
        }));
        reg.register(Box::new(FakeTool {
            name: "cron",
            cats: &["scheduling"],
        }));
        reg.register(Box::new(FakeTool {
            name: "mcp_custom",
            cats: &[],
        }));
        reg.register_mcp(
            Box::new(FakeTool {
                name: "mcp_server_tool",
                cats: &[],
            }),
            "test-server".to_string(),
        );
        reg
    }

    #[test]
    fn test_web_keywords_select_web_tools() {
        let reg = test_registry();
        let filtered = select_tools_for_task("search the web for rust docs", &reg);
        assert!(filtered.get(WEB_FETCH).is_some());
        assert!(filtered.get("web_search").is_some());
        assert!(
            filtered.get("mcp_server_tool").is_some(),
            "uncategorized mcp tool should remain available"
        );
        assert!(filtered.get("mcp_custom").is_none());
        assert!(filtered.get("sessions_list").is_none());
        assert!(filtered.get("cron").is_none());
    }

    #[test]
    fn test_code_keywords_select_code_tools() {
        let reg = test_registry();
        let filtered = select_tools_for_task("write a function that sorts", &reg);
        assert!(filtered.get("exec").is_some());
        assert!(filtered.get("create_skill").is_none());
        assert!(filtered.get("delete_skill").is_none());
        assert!(filtered.get(WEB_FETCH).is_none());
        assert!(filtered.get("mcp_custom").is_none());
        assert!(filtered.get("mcp_server_tool").is_some());
    }

    #[test]
    fn test_no_keywords_returns_all() {
        let reg = test_registry();
        let filtered = select_tools_for_task("hello world", &reg);
        assert!(filtered.get("exec").is_some());
        assert!(filtered.get(WEB_FETCH).is_some());
        assert!(filtered.get("sessions_list").is_some());
        assert!(filtered.get("sessions_delete").is_some());
        assert!(filtered.get("create_skill").is_some());
        assert!(filtered.get("delete_skill").is_some());
        assert!(filtered.get("cron").is_some());
        assert!(filtered.get("mcp_custom").is_some());
        assert!(filtered.get("mcp_server_tool").is_some());
    }

    #[test]
    fn test_multiple_keyword_overlap_gives_union() {
        let reg = test_registry();
        let filtered = select_tools_for_task("search the web and write code", &reg);
        assert!(filtered.get(WEB_FETCH).is_some());
        assert!(filtered.get("web_search").is_some());
        assert!(filtered.get("exec").is_some());
        assert!(filtered.get("mcp_custom").is_none());
        assert!(filtered.get("mcp_server_tool").is_some());
    }

    #[test]
    fn test_word_boundary_matching_avoids_false_positives() {
        let reg = test_registry();
        let filtered = select_tools_for_task("show my session profile fields", &reg);
        // "profile" should not match keyword "file" and enable code/file tools.
        assert!(filtered.get("exec").is_none());
        assert!(filtered.get(WEB_FETCH).is_none());
        assert!(filtered.get("sessions_list").is_some());
        assert!(filtered.get("cron").is_none());
        assert!(filtered.get("mcp_server_tool").is_some());
    }

    #[test]
    fn test_destructive_tools_not_selected_without_destructive_intent() {
        let reg = test_registry();
        let filtered = select_tools_for_task("show session history", &reg);
        assert!(filtered.get("sessions_list").is_some());
        assert!(filtered.get("sessions_delete").is_none());
    }

    #[test]
    fn test_destructive_tools_require_domain_and_intent() {
        let reg = test_registry();

        let filtered = select_tools_for_task("delete this session", &reg);
        assert!(filtered.get("sessions_delete").is_some());
        assert!(filtered.get("delete_skill").is_none());

        let filtered = select_tools_for_task("delete old files", &reg);
        assert!(filtered.get("sessions_delete").is_none());
        assert!(filtered.get("delete_skill").is_none());
    }
}
