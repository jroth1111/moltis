use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use tracing::info;

use crate::{
    Result,
    types::{ContextFile, ScopedRule},
};

/// Names of context files to collect when walking the directory hierarchy.
const CONTEXT_FILE_NAMES: &[&str] = &["CLAUDE.md", "CLAUDE.local.md", "AGENTS.md"];

/// Load all context files for a project directory.
///
/// Walks upward from `project_dir` to the filesystem root, collecting
/// `CLAUDE.md`, `CLAUDE.local.md`, and `AGENTS.md` at each level.
/// Also loads `.claude/rules/*.md` from `project_dir`.
///
/// Files are returned ordered outermost (root) first, innermost (project dir)
/// last, so that project-level files take highest priority when appended.
pub fn load_context_files(project_dir: &Path) -> Result<Vec<ContextFile>> {
    let project_dir = project_dir.canonicalize()?;
    let mut layers: Vec<Vec<ContextFile>> = Vec::new();

    // Walk upward from project dir to root
    let mut current = Some(project_dir.as_path());
    while let Some(dir) = current {
        let mut layer = Vec::new();
        for name in CONTEXT_FILE_NAMES {
            let file_path = dir.join(name);
            if file_path.is_file()
                && let Ok(content) = fs::read_to_string(&file_path)
                && !content.trim().is_empty()
            {
                info!(path = %file_path.display(), "loaded context file");
                layer.push(ContextFile {
                    path: file_path,
                    content,
                });
            }
        }
        if !layer.is_empty() {
            layers.push(layer);
        }
        current = dir.parent();
    }

    // Reverse so outermost comes first, innermost (project dir) last
    layers.reverse();
    let mut files: Vec<ContextFile> = layers.into_iter().flatten().collect();

    // Load .claude/rules/*.md from project root
    let rules_dir = project_dir.join(".claude").join("rules");
    if rules_dir.is_dir() {
        let mut rule_files: Vec<PathBuf> = fs::read_dir(&rules_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
            .collect();
        rule_files.sort();
        for path in rule_files {
            if let Ok(content) = fs::read_to_string(&path)
                && !content.trim().is_empty()
            {
                info!(path = %path.display(), "loaded rule file");
                files.push(ContextFile { path, content });
            }
        }
    }

    Ok(files)
}

/// Parse optional YAML-style front matter from a rules file.
///
/// Front matter is delimited by `---` on its own line at the start of the file.
/// Only the `globs:` key is recognized. Returns `(globs, body)` where body is
/// the content after the closing `---`.
fn parse_rules_front_matter(content: &str) -> (Option<Vec<String>>, &str) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (None, content);
    }
    // Find the closing `---`
    let after_first = &trimmed[3..].trim_start_matches(['\r', '\n']);
    let Some(end) = after_first.find("\n---") else {
        // No closing delimiter — treat entire content as body
        return (None, content);
    };
    let front_matter = &after_first[..end];
    let body_start = end + 4; // skip "\n---"
    let body = after_first[body_start..].trim_start_matches(['\r', '\n']);

    let mut globs = Vec::new();
    let mut in_globs = false;
    for line in front_matter.lines() {
        let stripped = line.trim();
        if stripped.starts_with("globs:") {
            in_globs = true;
            // Inline value on same line: `globs: ["*.rs"]`
            let after_key = stripped.strip_prefix("globs:").unwrap_or("").trim();
            if after_key.starts_with('[') {
                // Simple inline array: globs: ["*.rs", "*.toml"]
                let inner = after_key.trim_matches(|c| c == '[' || c == ']');
                for item in inner.split(',') {
                    let g = item.trim().trim_matches(|c| c == '"' || c == '\'');
                    if !g.is_empty() {
                        globs.push(g.to_string());
                    }
                }
                in_globs = false;
            }
        } else if in_globs && stripped.starts_with('-') {
            let g = stripped
                .strip_prefix('-')
                .unwrap_or("")
                .trim()
                .trim_matches(|c| c == '"' || c == '\'');
            if !g.is_empty() {
                globs.push(g.to_string());
            }
        } else {
            in_globs = false;
        }
    }

    let globs = if globs.is_empty() {
        None
    } else {
        Some(globs)
    };
    (globs, body)
}

/// Check whether a file path matches any of the given glob patterns.
fn matches_any_glob(candidates: &[&Path], globs: &[String]) -> bool {
    for pattern in globs {
        let Ok(glob) = globset::Glob::new(pattern) else {
            continue;
        };
        let matcher = glob.compile_matcher();
        for candidate in candidates {
            if matcher.is_match(candidate) {
                return true;
            }
            // Also try matching against just the file name.
            if let Some(name) = candidate.file_name()
                && matcher.is_match(Path::new(name))
            {
                return true;
            }
        }
    }
    false
}

fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {},
            Component::ParentDir => {
                out.pop();
            },
            Component::Normal(part) => out.push(part),
            Component::RootDir => out.push(Path::new("/")),
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
        }
    }
    out
}

fn canonicalize_with_missing(path: &Path) -> std::io::Result<PathBuf> {
    let mut missing: Vec<OsString> = Vec::new();
    let mut cursor = path;

    while !cursor.exists() {
        let Some(name) = cursor.file_name() else {
            break;
        };
        missing.push(name.to_os_string());
        let Some(parent) = cursor.parent() else {
            break;
        };
        cursor = parent;
    }

    let mut canonical = if cursor.exists() {
        cursor.canonicalize()?
    } else {
        normalize_path(path)
    };
    for part in missing.iter().rev() {
        canonical.push(part);
    }
    Ok(canonical)
}

/// Collect `.rules.md` and `.claude/rules/*.md` files from a single directory.
fn collect_rules_at(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let rules_file = dir.join(".rules.md");
    if rules_file.is_file()
        && let Ok(content) = fs::read_to_string(&rules_file)
        && !content.trim().is_empty()
    {
        out.push((rules_file, content));
    }
    let rules_dir = dir.join(".claude").join("rules");
    if rules_dir.is_dir()
        && let Ok(entries) = fs::read_dir(&rules_dir)
    {
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
            .collect();
        paths.sort();
        for path in paths {
            if let Ok(content) = fs::read_to_string(&path)
                && !content.trim().is_empty()
            {
                out.push((path, content));
            }
        }
    }
    out
}

/// Load path-scoped rules applicable to a given file.
///
/// Walks each directory from `project_dir` down to `file_path`'s parent,
/// collecting `.rules.md` and `.claude/rules/*.md` at each level. Rules with
/// `globs:` front matter are only included when `file_path` matches at least
/// one pattern. Results are ordered outermost first, innermost last.
pub fn load_path_scoped_rules(project_dir: &Path, file_path: &Path) -> Result<Vec<ScopedRule>> {
    let project_dir = project_dir.canonicalize()?;
    let raw_target = if file_path.is_absolute() {
        file_path.to_path_buf()
    } else {
        project_dir.join(file_path)
    };
    let file_path = if raw_target.exists() {
        raw_target.canonicalize()?
    } else {
        canonicalize_with_missing(&raw_target)?
    };

    // file_path must be under project_dir
    if !file_path.starts_with(&project_dir) {
        return Ok(Vec::new());
    }

    let target_dir = if file_path.is_dir() {
        file_path.clone()
    } else {
        file_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(file_path.clone())
    };

    // Build the list of directories from project_dir down to target_dir
    let relative = target_dir
        .strip_prefix(&project_dir)
        .unwrap_or(Path::new(""));
    let mut dirs = vec![project_dir.clone()];
    let mut current = project_dir.clone();
    for component in relative.components() {
        current = current.join(component);
        dirs.push(current.clone());
    }

    let mut rules = Vec::new();
    for dir in &dirs {
        for (source_path, content) in collect_rules_at(dir) {
            let (globs, body) = parse_rules_front_matter(&content);
            // If globs are specified, only include if file matches
            if let Some(ref patterns) = globs {
                let mut candidates: Vec<&Path> = vec![file_path.as_path()];
                if let Ok(rel) = file_path.strip_prefix(&project_dir) {
                    candidates.push(rel);
                }
                if let Ok(rel) = file_path.strip_prefix(dir) {
                    candidates.push(rel);
                }
                if !matches_any_glob(&candidates, patterns) {
                    continue;
                }
            }
            if !body.trim().is_empty() {
                info!(path = %source_path.display(), "loaded scoped rule");
                rules.push(ScopedRule {
                    source_path,
                    body: body.to_string(),
                    globs,
                });
            }
        }
    }

    Ok(rules)
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_context_files_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let files = load_context_files(dir.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_load_claude_md() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "# Project rules").unwrap();
        let files = load_context_files(dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].path.ends_with("CLAUDE.md"));
        assert_eq!(files[0].content, "# Project rules");
    }

    #[test]
    fn test_load_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "# Agents").unwrap();
        let files = load_context_files(dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].path.ends_with("AGENTS.md"));
    }

    #[test]
    fn test_load_multiple_context_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "claude").unwrap();
        fs::write(dir.path().join("CLAUDE.local.md"), "local").unwrap();
        fs::write(dir.path().join("AGENTS.md"), "agents").unwrap();
        let files = load_context_files(dir.path()).unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn test_load_rules_dir() {
        let dir = tempfile::tempdir().unwrap();
        let rules = dir.path().join(".claude").join("rules");
        fs::create_dir_all(&rules).unwrap();
        fs::write(rules.join("style.md"), "# Style guide").unwrap();
        fs::write(rules.join("security.md"), "# Security rules").unwrap();
        let files = load_context_files(dir.path()).unwrap();
        assert_eq!(files.len(), 2);
        // Should be sorted alphabetically
        assert!(files[0].path.ends_with("security.md"));
        assert!(files[1].path.ends_with("style.md"));
    }

    #[test]
    fn test_ignores_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "   \n  ").unwrap();
        let files = load_context_files(dir.path()).unwrap();
        assert!(files.is_empty());
    }

    // --- Path-scoped rule tests ---

    #[test]
    fn test_no_rules_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src").join("main.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "fn main() {}").unwrap();
        let rules = load_path_scoped_rules(dir.path(), &file).unwrap();
        assert!(rules.is_empty());
    }

    #[test]
    fn test_root_rules_md_applies() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".rules.md"), "Always use snake_case").unwrap();
        let file = dir.path().join("src").join("lib.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "").unwrap();
        let rules = load_path_scoped_rules(dir.path(), &file).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].body, "Always use snake_case");
        assert!(rules[0].globs.is_none());
    }

    #[test]
    fn test_nested_rules_ordering() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".rules.md"), "root rule").unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join(".rules.md"), "src rule").unwrap();
        let file = src.join("main.rs");
        fs::write(&file, "").unwrap();
        let rules = load_path_scoped_rules(dir.path(), &file).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].body, "root rule");
        assert_eq!(rules[1].body, "src rule");
    }

    #[test]
    fn test_glob_filter_includes_matching() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".rules.md"),
            "---\nglobs:\n  - \"*.rs\"\n---\nRust-only rule",
        )
        .unwrap();
        let file = dir.path().join("main.rs");
        fs::write(&file, "").unwrap();
        let rules = load_path_scoped_rules(dir.path(), &file).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].body, "Rust-only rule");
        assert_eq!(rules[0].globs.as_ref().unwrap(), &["*.rs"]);
    }

    #[test]
    fn test_glob_filter_excludes_non_matching() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".rules.md"),
            "---\nglobs:\n  - \"*.py\"\n---\nPython-only rule",
        )
        .unwrap();
        let file = dir.path().join("main.rs");
        fs::write(&file, "").unwrap();
        let rules = load_path_scoped_rules(dir.path(), &file).unwrap();
        assert!(rules.is_empty());
    }

    #[test]
    fn test_glob_inline_array() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".rules.md"),
            "---\nglobs: [\"*.rs\", \"*.toml\"]\n---\nRust+TOML rule",
        )
        .unwrap();
        let file = dir.path().join("Cargo.toml");
        fs::write(&file, "").unwrap();
        let rules = load_path_scoped_rules(dir.path(), &file).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].body, "Rust+TOML rule");
    }

    #[test]
    fn test_file_outside_project_returns_empty() {
        let project = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let file = other.path().join("outside.rs");
        fs::write(&file, "").unwrap();
        let rules = load_path_scoped_rules(project.path(), &file).unwrap();
        assert!(rules.is_empty());
    }

    #[test]
    fn test_claude_rules_dir_at_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        let rules_dir = sub.join(".claude").join("rules");
        fs::create_dir_all(&rules_dir).unwrap();
        fs::write(rules_dir.join("style.md"), "# Sub-dir style").unwrap();
        let file = sub.join("code.rs");
        fs::write(&file, "").unwrap();
        let rules = load_path_scoped_rules(dir.path(), &file).unwrap();
        assert_eq!(rules.len(), 1);
        assert!(rules[0].source_path.ends_with("style.md"));
        assert_eq!(rules[0].body, "# Sub-dir style");
    }

    #[test]
    fn test_parse_front_matter_no_delimiters() {
        let (globs, body) = parse_rules_front_matter("just a body");
        assert!(globs.is_none());
        assert_eq!(body, "just a body");
    }

    #[test]
    fn test_parse_front_matter_with_globs() {
        let content = "---\nglobs:\n  - \"*.rs\"\n  - \"*.toml\"\n---\nBody here";
        let (globs, body) = parse_rules_front_matter(content);
        assert_eq!(globs.unwrap(), vec!["*.rs", "*.toml"]);
        assert_eq!(body, "Body here");
    }

    #[test]
    fn test_glob_matches_project_relative_path_pattern() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".rules.md"),
            "---\nglobs:\n  - \"src/**/*.rs\"\n---\nRust tree rule",
        )
        .unwrap();
        let nested = dir.path().join("src").join("core");
        fs::create_dir_all(&nested).unwrap();
        let file = nested.join("mod.rs");
        fs::write(&file, "").unwrap();

        let rules = load_path_scoped_rules(dir.path(), &file).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].body, "Rust tree rule");
    }

    #[test]
    fn test_nonexistent_target_path_still_loads_matching_rules() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".rules.md"),
            "---\nglobs:\n  - \"*.rs\"\n---\nApplies to new Rust files",
        )
        .unwrap();

        let file = dir.path().join("src").join("new_file.rs");
        // Intentionally do not create the file.
        let rules = load_path_scoped_rules(dir.path(), &file).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].body, "Applies to new Rust files");
    }
}
