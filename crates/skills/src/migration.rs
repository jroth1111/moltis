//! One-time migration from separate plugins system to unified skills system.
//!
//! On startup, checks for `plugins-manifest.json` and migrates entries into
//! `skills-manifest.json`, moving directories from `installed-plugins/` to
//! `installed-skills/`. This is idempotent and non-fatal.

use std::path::Path;

use serde::Deserialize;

use crate::{
    formats::PluginFormat,
    manifest::ManifestStore,
    types::{RepoEntry, SkillState, SkillStatus, SkillsManifest},
};

#[derive(Debug, Deserialize)]
struct SkillsManifestV1 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    repos: Vec<RepoEntryV1>,
}

#[derive(Debug, Deserialize)]
struct RepoEntryV1 {
    source: String,
    repo_name: String,
    installed_at_ms: u64,
    #[serde(default)]
    commit_sha: Option<String>,
    #[serde(default)]
    format: PluginFormat,
    #[serde(default)]
    skills: Vec<SkillStateV1>,
}

#[derive(Debug, Deserialize)]
struct SkillStateV1 {
    name: String,
    relative_path: String,
}

#[derive(Debug, Deserialize)]
struct SkillsManifestV2 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    repos: Vec<RepoEntryV2>,
}

#[derive(Debug, Deserialize)]
struct RepoEntryV2 {
    source: String,
    repo_name: String,
    installed_at_ms: u64,
    #[serde(default)]
    commit_sha: Option<String>,
    #[serde(default)]
    format: PluginFormat,
    #[serde(default)]
    skills: Vec<SkillStateV2>,
}

#[derive(Debug, Deserialize)]
struct SkillStateV2 {
    name: String,
    relative_path: String,
    status: LegacySkillStatus,
    #[serde(default)]
    quarantine_reason: Option<String>,
    #[serde(default)]
    last_audited_ms: Option<u64>,
    #[serde(default)]
    content_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacySkillStatus {
    Trusted,
    Untrusted,
    Quarantined,
}

/// Parse and migrate an older manifest payload into v3.
///
/// Returns `Ok(Some(_))` when migration is needed, `Ok(None)` when payload is
/// already v3+.
pub fn migrate_manifest_v1_to_v2(raw_json: &str) -> anyhow::Result<Option<SkillsManifest>> {
    let value: serde_json::Value = serde_json::from_str(raw_json)?;
    let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(1);
    if version >= 3 {
        return Ok(None);
    }

    let repos = if version == 1 {
        let legacy: SkillsManifestV1 = serde_json::from_value(value)?;
        legacy
            .repos
            .into_iter()
            .map(|repo| RepoEntry {
                source: repo.source,
                repo_name: repo.repo_name,
                installed_at_ms: repo.installed_at_ms,
                commit_sha: repo.commit_sha,
                format: repo.format,
                skills: repo
                    .skills
                    .into_iter()
                    .map(|skill| SkillState {
                        name: skill.name,
                        relative_path: skill.relative_path,
                        status: SkillStatus::Pending,
                        quarantine_reason: None,
                        last_audited_ms: None,
                        content_hash: None,
                        trusted_hash: None,
                        enabled: false,
                    })
                    .collect(),
            })
            .collect()
    } else {
        let legacy: SkillsManifestV2 = serde_json::from_value(value)?;
        legacy
            .repos
            .into_iter()
            .map(|repo| RepoEntry {
                source: repo.source,
                repo_name: repo.repo_name,
                installed_at_ms: repo.installed_at_ms,
                commit_sha: repo.commit_sha,
                format: repo.format,
                skills: repo
                    .skills
                    .into_iter()
                    .map(|skill| SkillState {
                        name: skill.name,
                        relative_path: skill.relative_path,
                        status: if matches!(skill.status, LegacySkillStatus::Quarantined) {
                            SkillStatus::Quarantined
                        } else {
                            SkillStatus::Pending
                        },
                        quarantine_reason: skill.quarantine_reason,
                        last_audited_ms: skill.last_audited_ms,
                        content_hash: skill.content_hash,
                        trusted_hash: None,
                        enabled: false,
                    })
                    .collect(),
            })
            .collect()
    };

    Ok(Some(SkillsManifest { version: 3, repos }))
}

/// Migrate plugins data into the unified skills system.
///
/// - Merges repos from `plugins-manifest.json` into `skills-manifest.json`
/// - Moves directories from `installed-plugins/` to `installed-skills/`
/// - Preserves all fields: `status`, `enabled`, `commit_sha`, `format`, `relative_path`
/// - Skips entries already in skills manifest (idempotent)
/// - Deletes old manifest + empty old dir after successful migration
///
/// Non-fatal: logs a warning if migration fails.
pub async fn migrate_plugins_to_skills(data_dir: &Path) {
    if let Err(e) = try_migrate(data_dir).await {
        tracing::warn!(%e, "plugins-to-skills migration failed (non-fatal)");
    }
}

async fn try_migrate(data_dir: &Path) -> anyhow::Result<()> {
    let plugins_manifest_path = data_dir.join("plugins-manifest.json");
    if !plugins_manifest_path.exists() {
        return Ok(());
    }

    tracing::info!("migrating plugins to unified skills system");

    let plugins_store = ManifestStore::new(plugins_manifest_path.clone());
    let plugins_manifest = plugins_store.load()?;

    if plugins_manifest.repos.is_empty() {
        // Empty manifest — just clean up.
        cleanup_old_files(&plugins_manifest_path, data_dir).await;
        return Ok(());
    }

    let skills_manifest_path = data_dir.join("skills-manifest.json");
    let skills_store = ManifestStore::new(skills_manifest_path);
    let mut skills_manifest = skills_store.load()?;

    let plugins_dir = data_dir.join("installed-plugins");
    let skills_dir = data_dir.join("installed-skills");
    tokio::fs::create_dir_all(&skills_dir).await?;

    let mut migrated = 0usize;
    for repo in &plugins_manifest.repos {
        // Skip if already in skills manifest.
        if skills_manifest.find_repo(&repo.source).is_some() {
            tracing::debug!(source = %repo.source, "already in skills manifest, skipping");
            continue;
        }

        // Move directory from installed-plugins/ to installed-skills/.
        let src_dir = plugins_dir.join(&repo.repo_name);
        let dst_dir = skills_dir.join(&repo.repo_name);

        if src_dir.is_dir()
            && !dst_dir.exists()
            && let Err(e) = tokio::fs::rename(&src_dir, &dst_dir).await
        {
            // rename fails across filesystems; fall back to copy + remove.
            tracing::debug!(%e, "rename failed, trying copy");
            copy_dir_recursive(&src_dir, &dst_dir).await?;
            let _ = tokio::fs::remove_dir_all(&src_dir).await;
        }

        let mut migrated_repo = repo.clone();
        for skill in &mut migrated_repo.skills {
            skill.status = SkillStatus::Pending;
            skill.enabled = false;
        }
        skills_manifest.add_repo(migrated_repo);
        migrated += 1;
    }

    if migrated > 0 {
        skills_store.save(&skills_manifest)?;
        tracing::info!(migrated, "migrated plugin repos into skills manifest");
    }

    cleanup_old_files(&plugins_manifest_path, data_dir).await;
    Ok(())
}

async fn cleanup_old_files(plugins_manifest_path: &Path, data_dir: &Path) {
    let _ = tokio::fs::remove_file(plugins_manifest_path).await;
    let plugins_dir = data_dir.join("installed-plugins");
    // Only remove if empty.
    if plugins_dir.is_dir()
        && let Ok(mut entries) = tokio::fs::read_dir(&plugins_dir).await
        && entries.next_entry().await.ok().flatten().is_none()
    {
        let _ = tokio::fs::remove_dir(&plugins_dir).await;
    }
}

async fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(dst).await?;
    let mut entries = tokio::fs::read_dir(src).await?;
    while let Some(entry) = entries.next_entry().await? {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            Box::pin(copy_dir_recursive(&src_path, &dst_path)).await?;
        } else {
            tokio::fs::copy(&src_path, &dst_path).await?;
        }
    }
    Ok(())
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use crate::{
        formats::PluginFormat,
        types::{RepoEntry, SkillState, SkillStatus, SkillsManifest},
    };

    use super::*;

    #[tokio::test]
    async fn test_migration_moves_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();

        // Set up plugins manifest.
        let plugins_manifest = SkillsManifest {
            version: 3,
            repos: vec![RepoEntry {
                source: "anthropics/claude-plugins-official".into(),
                repo_name: "anthropics-claude-plugins-official".into(),
                installed_at_ms: 1000,
                commit_sha: Some("abc123def456".into()),
                format: PluginFormat::ClaudeCode,
                skills: vec![SkillState {
                    name: "pr-review-toolkit:code-reviewer".into(),
                    relative_path: "anthropics-claude-plugins-official".into(),
                    status: SkillStatus::Pending,
                    quarantine_reason: None,
                    last_audited_ms: None,
                    content_hash: None,
                    trusted_hash: None,
                    enabled: false,
                }],
            }],
        };
        let plugins_store = ManifestStore::new(data_dir.join("plugins-manifest.json"));
        plugins_store.save(&plugins_manifest).unwrap();

        // Set up installed-plugins directory.
        let plugins_dir = data_dir.join("installed-plugins/anthropics-claude-plugins-official");
        std::fs::create_dir_all(plugins_dir.join(".claude-plugin")).unwrap();
        std::fs::write(
            plugins_dir.join(".claude-plugin/plugin.json"),
            r#"{"name":"test"}"#,
        )
        .unwrap();

        // Set up empty skills manifest.
        let skills_store = ManifestStore::new(data_dir.join("skills-manifest.json"));
        skills_store.save(&SkillsManifest::default()).unwrap();

        // Run migration.
        try_migrate(data_dir).await.unwrap();

        // Verify skills manifest has the migrated repo.
        let skills_manifest = skills_store.load().unwrap();
        assert_eq!(skills_manifest.repos.len(), 1);
        let repo = &skills_manifest.repos[0];
        assert_eq!(repo.source, "anthropics/claude-plugins-official");
        assert_eq!(repo.commit_sha.as_deref(), Some("abc123def456"));
        assert_eq!(repo.format, PluginFormat::ClaudeCode);
        assert_eq!(repo.skills[0].status, SkillStatus::Pending);
        assert!(!repo.skills[0].enabled);

        // Verify directory moved.
        assert!(
            data_dir
                .join(
                    "installed-skills/anthropics-claude-plugins-official/.claude-plugin/plugin.json"
                )
                .exists()
        );
        assert!(
            !data_dir
                .join("installed-plugins/anthropics-claude-plugins-official")
                .exists()
        );

        // Verify old manifest deleted.
        assert!(!data_dir.join("plugins-manifest.json").exists());
    }

    #[tokio::test]
    async fn test_migration_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();

        let repo = RepoEntry {
            source: "owner/repo".into(),
            repo_name: "owner-repo".into(),
            installed_at_ms: 500,
            commit_sha: None,
            format: PluginFormat::ClaudeCode,
            skills: vec![SkillState {
                name: "plugin:skill".into(),
                relative_path: "owner-repo".into(),
                status: SkillStatus::Pending,
                quarantine_reason: None,
                last_audited_ms: None,
                content_hash: None,
                trusted_hash: None,
                enabled: false,
            }],
        };

        // Plugins manifest has the repo.
        let plugins_manifest = SkillsManifest {
            version: 3,
            repos: vec![repo.clone()],
        };
        let plugins_store = ManifestStore::new(data_dir.join("plugins-manifest.json"));
        plugins_store.save(&plugins_manifest).unwrap();

        // Skills manifest already has it too.
        let mut skills_manifest = SkillsManifest::default();
        skills_manifest.add_repo(repo);
        let skills_store = ManifestStore::new(data_dir.join("skills-manifest.json"));
        skills_store.save(&skills_manifest).unwrap();

        // Run migration — should not duplicate.
        try_migrate(data_dir).await.unwrap();

        let loaded = skills_store.load().unwrap();
        assert_eq!(loaded.repos.len(), 1);
    }

    #[tokio::test]
    async fn test_migration_noop_when_no_plugins_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        // No plugins-manifest.json — migration should be a no-op.
        try_migrate(tmp.path()).await.unwrap();
    }

    #[tokio::test]
    async fn test_migration_empty_manifest_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();

        // Empty plugins manifest.
        let plugins_store = ManifestStore::new(data_dir.join("plugins-manifest.json"));
        plugins_store.save(&SkillsManifest::default()).unwrap();
        assert!(data_dir.join("plugins-manifest.json").exists());

        // Empty plugins dir.
        std::fs::create_dir_all(data_dir.join("installed-plugins")).unwrap();

        try_migrate(data_dir).await.unwrap();

        // Old files cleaned up.
        assert!(!data_dir.join("plugins-manifest.json").exists());
        assert!(!data_dir.join("installed-plugins").exists());
    }

    #[test]
    fn test_migrate_manifest_v1_to_v2_resets_to_pending() {
        let raw = r#"{
            "version": 1,
            "repos": [
                {
                    "source": "owner/repo",
                    "repo_name": "owner-repo",
                    "installed_at_ms": 42,
                    "commit_sha": "abc",
                    "format": "skill",
                    "skills": [
                        {
                            "name": "safe",
                            "relative_path": "owner-repo/skills/safe",
                            "trusted": true,
                            "enabled": true
                        },
                        {
                            "name": "new",
                            "relative_path": "owner-repo/skills/new",
                            "trusted": false,
                            "enabled": false
                        }
                    ]
                }
            ]
        }"#;

        let migrated = migrate_manifest_v1_to_v2(raw).unwrap().unwrap();
        assert_eq!(migrated.version, 3);
        assert_eq!(migrated.repos.len(), 1);
        assert_eq!(migrated.repos[0].skills.len(), 2);
        assert_eq!(migrated.repos[0].skills[0].status, SkillStatus::Pending);
        assert_eq!(migrated.repos[0].skills[1].status, SkillStatus::Pending);
        assert!(!migrated.repos[0].skills[0].enabled);
        assert!(!migrated.repos[0].skills[1].enabled);
        assert!(migrated.repos[0].skills[0].quarantine_reason.is_none());
        assert!(migrated.repos[0].skills[0].content_hash.is_none());
    }

    #[test]
    fn test_migrate_manifest_v1_to_v2_upgrades_v2_to_v3() {
        let raw = r#"{"version":2,"repos":[]}"#;
        let migrated = migrate_manifest_v1_to_v2(raw).unwrap().unwrap();
        assert_eq!(migrated.version, 3);
        assert!(migrated.repos.is_empty());
    }
}
