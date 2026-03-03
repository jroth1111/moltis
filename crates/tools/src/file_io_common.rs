use std::{
    ffi::OsStr,
    path::{Component, Path, PathBuf},
};

use crate::error::Error;

pub(crate) const MAX_FILE_BYTES: usize = 1024 * 1024;
pub(crate) const DEFAULT_READ_LIMIT_BYTES: usize = 64 * 1024;

pub(crate) fn shell_single_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

pub(crate) fn format_with_line_numbers(text: &str, start_line: usize) -> String {
    if text.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    for (idx, line) in text.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let line_no = start_line.saturating_add(idx);
        out.push_str(&format!("{line_no:>6} | {line}"));
    }
    out
}

fn normalize_requested_path(path: &str) -> crate::Result<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(Error::message("missing 'path' parameter"));
    }

    let candidate = PathBuf::from(trimmed);
    for component in candidate.components() {
        if matches!(component, Component::ParentDir | Component::Prefix(_)) {
            return Err(Error::message(
                "path traversal is not allowed outside approved roots",
            ));
        }
    }

    Ok(candidate)
}

fn canonical_allowed_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    let data_dir = moltis_config::data_dir();
    roots.push(data_dir.clone());
    if let Ok(canonical_data_dir) = std::fs::canonicalize(&data_dir)
        && !roots.contains(&canonical_data_dir)
    {
        roots.push(canonical_data_dir);
    }

    if let Ok(cwd) = std::env::current_dir() {
        if !roots.contains(&cwd) {
            roots.push(cwd.clone());
        }
        if let Ok(canonical_cwd) = std::fs::canonicalize(cwd)
        && !roots.contains(&canonical_cwd)
        {
            roots.push(canonical_cwd);
        }
    }

    roots
}

fn ensure_within_allowed_roots(canonical_path: &Path) -> crate::Result<()> {
    let allowed_roots = canonical_allowed_roots();
    if allowed_roots
        .iter()
        .any(|root| canonical_path.starts_with(root))
    {
        return Ok(());
    }

    let roots = if allowed_roots.is_empty() {
        "<none>".to_string()
    } else {
        allowed_roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };

    Err(Error::message(format!(
        "path '{}' is outside allowed roots: {roots}",
        canonical_path.display()
    )))
}

fn ensure_within_allowed_roots_lexical(path: &Path) -> crate::Result<()> {
    let allowed_roots = canonical_allowed_roots();
    if allowed_roots.iter().any(|root| path.starts_with(root)) {
        return Ok(());
    }

    let roots = if allowed_roots.is_empty() {
        "<none>".to_string()
    } else {
        allowed_roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };

    Err(Error::message(format!(
        "path '{}' is outside allowed roots: {roots}",
        path.display()
    )))
}

pub(crate) async fn resolve_host_read_path(path: &str) -> crate::Result<PathBuf> {
    let requested = normalize_requested_path(path)?;
    let absolute = if requested.is_absolute() {
        requested
    } else {
        std::env::current_dir()
            .map_err(|e| Error::message(format!("failed to read current directory: {e}")))?
            .join(requested)
    };

    let canonical = tokio::fs::canonicalize(&absolute).await.map_err(|e| {
        Error::message(format!(
            "failed to resolve file path '{}': {e}",
            absolute.display()
        ))
    })?;

    ensure_within_allowed_roots(&canonical)?;
    Ok(canonical)
}

pub(crate) async fn resolve_host_write_path(path: &str) -> crate::Result<PathBuf> {
    let requested = normalize_requested_path(path)?;
    let absolute = if requested.is_absolute() {
        requested
    } else {
        std::env::current_dir()
            .map_err(|e| Error::message(format!("failed to read current directory: {e}")))?
            .join(requested)
    };

    let parent = absolute
        .parent()
        .ok_or_else(|| Error::message("path must include a parent directory"))?;

    if parent.exists() {
        let canonical_parent = tokio::fs::canonicalize(parent).await.map_err(|e| {
            Error::message(format!(
                "failed to resolve parent directory '{}': {e}",
                parent.display()
            ))
        })?;
        ensure_within_allowed_roots(&canonical_parent)?;
        let file_name = absolute
            .file_name()
            .ok_or_else(|| Error::message("path must point to a file"))?;
        return Ok(canonical_parent.join(file_name));
    }

    // Parent may not exist yet for new files. Validate that the target still
    // stays under an allowed root, then create parents during the write step.
    ensure_within_allowed_roots_lexical(&absolute)?;

    let file_name = absolute
        .file_name()
        .ok_or_else(|| Error::message("path must point to a file"))?;
    Ok(parent.join(file_name))
}

pub(crate) fn normalize_sandbox_path(path: &str) -> crate::Result<String> {
    let requested = normalize_requested_path(path)?;
    let absolute = if requested.is_absolute() {
        requested
    } else {
        PathBuf::from("/home/sandbox").join(requested)
    };

    if !absolute.starts_with("/home/sandbox") && !absolute.starts_with("/tmp") {
        return Err(Error::message(
            "sandbox paths must be under /home/sandbox or /tmp",
        ));
    }

    for component in absolute.components() {
        if matches!(component, Component::ParentDir | Component::Prefix(_)) {
            return Err(Error::message(
                "path traversal is not allowed inside sandbox",
            ));
        }
    }

    Ok(absolute.to_string_lossy().into_owned())
}

pub(crate) fn is_memory_scoped_host_path(path: &Path) -> bool {
    let data_dir = std::fs::canonicalize(moltis_config::data_dir())
        .unwrap_or_else(|_| moltis_config::data_dir());
    let root_memory_dir = data_dir.join("memory");

    if path == data_dir.join("MEMORY.md")
        || path == data_dir.join("memory.md")
        || path.starts_with(&root_memory_dir)
    {
        return true;
    }

    let agents_root = data_dir.join("agents");
    let Ok(relative) = path.strip_prefix(&agents_root) else {
        return false;
    };

    let mut components = relative.components();
    let _agent_id = components.next();
    let Some(second) = components.next() else {
        return false;
    };

    if second == Component::Normal(OsStr::new("memory")) {
        return true;
    }

    if (second == Component::Normal(OsStr::new("MEMORY.md"))
        || second == Component::Normal(OsStr::new("memory.md")))
        && components.next().is_none()
    {
        return true;
    }

    false
}

pub(crate) fn is_memory_scoped_sandbox_path(path: &str) -> bool {
    path == "/home/sandbox/MEMORY.md"
        || path == "/home/sandbox/memory.md"
        || path.starts_with("/home/sandbox/memory/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_with_line_numbers_adds_prefixes() {
        let text = "alpha\nbeta";
        let numbered = format_with_line_numbers(text, 7);
        assert_eq!(numbered, "     7 | alpha\n     8 | beta");
    }

    #[test]
    fn normalize_sandbox_path_rejects_outside_root() {
        let err = normalize_sandbox_path("/etc/passwd").unwrap_err();
        assert!(err.to_string().contains("/home/sandbox"));
    }

    #[test]
    fn normalize_sandbox_path_accepts_relative() {
        let resolved = normalize_sandbox_path("notes/today.md").unwrap();
        assert_eq!(resolved, "/home/sandbox/notes/today.md");
    }

    #[test]
    fn is_memory_scoped_sandbox_path_matches_memory_targets() {
        assert!(is_memory_scoped_sandbox_path("/home/sandbox/MEMORY.md"));
        assert!(is_memory_scoped_sandbox_path("/home/sandbox/memory/notes.md"));
        assert!(!is_memory_scoped_sandbox_path("/home/sandbox/docs/notes.md"));
    }
}
