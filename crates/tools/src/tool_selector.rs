//! Keyword-based tool selector for dynamic tool scoping.
//!
//! Matches task descriptions against a static keyword→category table and
//! returns a filtered tool registry containing only tools whose categories
//! overlap with the matched keywords. Tools with no categories are always
//! included (safe fallback). If no keywords match, all tools are returned.

use std::collections::HashSet;

use moltis_agents::tool_registry::ToolRegistry;

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
    // Session management
    ("session", &["session"]),
    ("branch", &["session"]),
    ("history", &["session"]),
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

/// Select tools relevant to a task description by matching keywords to categories.
///
/// Returns a filtered registry containing only tools whose categories intersect
/// with the matched keyword categories. Tools with no categories (e.g. MCP tools)
/// are always included. If no keywords match, returns all tools (safe fallback).
pub fn select_tools_for_task(task: &str, registry: &ToolRegistry) -> ToolRegistry {
    let lower = task.to_ascii_lowercase();

    let mut matched_categories: HashSet<&str> = HashSet::new();
    for (keyword, categories) in KEYWORD_CATEGORIES {
        if lower.contains(keyword) {
            matched_categories.extend(categories.iter());
        }
    }

    // No keywords matched → return all tools (safe fallback)
    if matched_categories.is_empty() {
        return registry.clone_without(&[]);
    }

    // Keep tools whose categories intersect with matched set OR that have no categories
    registry.clone_allowed_by(|name| {
        let cats = registry.categories_for(name);
        // Tools with no categories are always included
        if cats.is_empty() {
            return true;
        }
        // Include if any category matches
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
        async fn execute(
            &self,
            _params: serde_json::Value,
        ) -> anyhow::Result<serde_json::Value> {
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
            name: "web_fetch",
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
            name: "cron",
            cats: &["scheduling"],
        }));
        reg.register(Box::new(FakeTool {
            name: "mcp_custom",
            cats: &[], // no categories → always included
        }));
        reg
    }

    #[test]
    fn test_web_keywords_select_web_tools() {
        let reg = test_registry();
        let filtered = select_tools_for_task("search the web for rust docs", &reg);
        assert!(filtered.get("web_fetch").is_some());
        assert!(filtered.get("web_search").is_some());
        assert!(
            filtered.get("mcp_custom").is_some(),
            "no-category tools always included"
        );
        assert!(filtered.get("sessions_list").is_none());
        assert!(filtered.get("cron").is_none());
    }

    #[test]
    fn test_code_keywords_select_code_tools() {
        let reg = test_registry();
        let filtered = select_tools_for_task("write a function that sorts", &reg);
        assert!(filtered.get("exec").is_some());
        assert!(filtered.get("web_fetch").is_none());
    }

    #[test]
    fn test_no_keywords_returns_all() {
        let reg = test_registry();
        let filtered = select_tools_for_task("hello world", &reg);
        assert!(filtered.get("exec").is_some());
        assert!(filtered.get("web_fetch").is_some());
        assert!(filtered.get("sessions_list").is_some());
        assert!(filtered.get("cron").is_some());
        assert!(filtered.get("mcp_custom").is_some());
    }

    #[test]
    fn test_multiple_keyword_overlap_gives_union() {
        let reg = test_registry();
        let filtered =
            select_tools_for_task("search the web and write code", &reg);
        assert!(filtered.get("web_fetch").is_some());
        assert!(filtered.get("web_search").is_some());
        assert!(filtered.get("exec").is_some());
        assert!(filtered.get("mcp_custom").is_some());
    }
}
