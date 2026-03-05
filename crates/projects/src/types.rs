use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A project represents a codebase directory that moltis can work with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub label: String,
    pub directory: PathBuf,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub auto_worktree: bool,
    #[serde(default)]
    pub setup_command: Option<String>,
    #[serde(default)]
    pub teardown_command: Option<String>,
    #[serde(default)]
    pub branch_prefix: Option<String>,
    #[serde(default)]
    pub sandbox_image: Option<String>,
    #[serde(default)]
    pub detected: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

/// A context file loaded from a project directory hierarchy.
#[derive(Debug, Clone)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

/// A path-scoped rule loaded from a `.rules.md` file in the directory hierarchy.
///
/// Rules are discovered by walking from the project root down to the target
/// file's parent directory. Each level may contain `.rules.md` and/or
/// `.claude/rules/*.md` files. An optional `globs:` front-matter key limits
/// the rule to matching file paths.
#[derive(Debug, Clone)]
pub struct ScopedRule {
    /// Path of the `.rules.md` (or `.claude/rules/X.md`) file.
    pub source_path: PathBuf,
    /// Rule body with front matter stripped.
    pub body: String,
    /// Glob patterns from front matter. `None` means the rule applies unconditionally.
    pub globs: Option<Vec<String>>,
}

/// Aggregated context for a project: the project itself plus all loaded context files.
#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub project: Project,
    /// Context files ordered from outermost (root) to innermost (project dir).
    pub context_files: Vec<ContextFile>,
    /// Active worktree directory, if one exists for this session.
    pub worktree_dir: Option<PathBuf>,
}

impl ProjectContext {
    /// Build the combined context string suitable for system prompt injection.
    pub fn to_prompt_section(&self) -> String {
        let mut out = format!(
            "# Project: {}\nDirectory: {}\n\n",
            self.project.label,
            self.project.directory.display()
        );
        if let Some(ref wt_dir) = self.worktree_dir {
            out.push_str(&format!(
                "Working directory (worktree): {}\n\n",
                wt_dir.display()
            ));
        }
        if let Some(ref prompt) = self.project.system_prompt {
            out.push_str(prompt);
            out.push_str("\n\n");
        }
        for (idx, cf) in self.context_files.iter().enumerate() {
            let layer_rank = idx + 1;
            let rendered_path = cf
                .path
                .strip_prefix(&self.project.directory)
                .map(|relative| relative.display().to_string())
                .unwrap_or_else(|_| cf.path.display().to_string());
            out.push_str(&format!(
                "## Context [{layer_rank}]: {rendered_path}\n\n{}\n\n",
                cf.content
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_project() -> Project {
        Project {
            id: "test".into(),
            label: "Test Project".into(),
            directory: PathBuf::from("/projects/test"),
            system_prompt: None,
            auto_worktree: false,
            setup_command: None,
            teardown_command: None,
            branch_prefix: None,
            sandbox_image: None,
            detected: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn test_prompt_section_without_worktree() {
        let ctx = ProjectContext {
            project: test_project(),
            context_files: vec![],
            worktree_dir: None,
        };
        let section = ctx.to_prompt_section();
        assert!(section.contains("# Project: Test Project"));
        assert!(section.contains("Directory: /projects/test"));
        assert!(!section.contains("worktree"));
    }

    #[test]
    fn test_prompt_section_with_worktree() {
        let ctx = ProjectContext {
            project: test_project(),
            context_files: vec![],
            worktree_dir: Some(PathBuf::from("/projects/test/.moltis-worktrees/session1")),
        };
        let section = ctx.to_prompt_section();
        assert!(section.contains("Working directory (worktree):"));
        assert!(section.contains(".moltis-worktrees/session1"));
    }

    #[test]
    fn test_prompt_section_with_context_files() {
        let ctx = ProjectContext {
            project: test_project(),
            context_files: vec![ContextFile {
                path: PathBuf::from("/projects/test/CLAUDE.md"),
                content: "Hello world".into(),
            }],
            worktree_dir: None,
        };
        let section = ctx.to_prompt_section();
        assert!(section.contains("## Context [1]: CLAUDE.md"));
        assert!(section.contains("Hello world"));
    }
}
