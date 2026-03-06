use std::path::{Path, PathBuf};

use crate::types::SkillsManifest;

/// Persistent manifest storage with atomic writes.
pub struct ManifestStore {
    path: PathBuf,
}

impl ManifestStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Default manifest path: `~/.moltis/skills-manifest.json`.
    pub fn default_path() -> anyhow::Result<PathBuf> {
        Ok(moltis_config::data_dir().join("skills-manifest.json"))
    }

    /// Load manifest from disk, returning a default if missing.
    pub fn load(&self) -> anyhow::Result<SkillsManifest> {
        if !self.path.exists() {
            return Ok(SkillsManifest::default());
        }
        let data = std::fs::read_to_string(&self.path)?;
        if let Some(migrated) = crate::migration::migrate_manifest_v1_to_v2(&data)? {
            // One-time in-place upgrade to schema v3.
            self.save(&migrated)?;
            return Ok(migrated);
        }
        let manifest: SkillsManifest = serde_json::from_str(&data)?;
        Ok(manifest)
    }

    /// Save manifest atomically via temp file + rename.
    pub fn save(&self, manifest: &SkillsManifest) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(manifest)?;
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::types::{RepoEntry, SkillState, SkillStatus},
    };

    #[test]
    fn test_load_missing_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ManifestStore::new(tmp.path().join("missing.json"));
        let m = store.load().unwrap();
        assert_eq!(m.version, 3);
        assert!(m.repos.is_empty());
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ManifestStore::new(tmp.path().join("manifest.json"));

        let mut manifest = SkillsManifest::default();
        manifest.add_repo(RepoEntry {
            source: "owner/repo".into(),
            repo_name: "repo".into(),
            installed_at_ms: 1234567890,
            commit_sha: Some("abc123".into()),
            format: Default::default(),
            skills: vec![SkillState {
                name: "my-skill".into(),
                relative_path: "skills/my-skill".into(),
                status: SkillStatus::Trusted,
                quarantine_reason: None,
                last_audited_ms: None,
                content_hash: None,
                trusted_hash: None,
                enabled: true,
            }],
        });

        store.save(&manifest).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.repos.len(), 1);
        assert_eq!(loaded.repos[0].source, "owner/repo");
        assert_eq!(loaded.repos[0].skills[0].name, "my-skill");
        assert!(loaded.repos[0].skills[0].enabled);
    }

    #[test]
    fn test_manifest_set_skill_enabled() {
        let mut m = SkillsManifest::default();
        m.add_repo(RepoEntry {
            source: "a/b".into(),
            repo_name: "b".into(),
            installed_at_ms: 0,
            commit_sha: None,
            format: Default::default(),
            skills: vec![
                SkillState {
                    name: "s1".into(),
                    relative_path: "s1".into(),
                    status: SkillStatus::Trusted,
                    quarantine_reason: None,
                    last_audited_ms: None,
                    content_hash: None,
                    trusted_hash: None,
                    enabled: true,
                },
                SkillState {
                    name: "s2".into(),
                    relative_path: "s2".into(),
                    status: SkillStatus::Trusted,
                    quarantine_reason: None,
                    last_audited_ms: None,
                    content_hash: None,
                    trusted_hash: None,
                    enabled: true,
                },
            ],
        });

        assert!(m.set_skill_enabled("a/b", "s1", false));
        assert!(!m.find_repo("a/b").unwrap().skills[0].enabled);
        assert!(m.find_repo("a/b").unwrap().skills[1].enabled);

        // Non-existent skill returns false.
        assert!(!m.set_skill_enabled("a/b", "nope", false));
        // Non-existent repo returns false.
        assert!(!m.set_skill_enabled("x/y", "s1", false));
    }

    #[test]
    fn test_manifest_set_skill_status() {
        let mut m = SkillsManifest::default();
        m.add_repo(RepoEntry {
            source: "a/b".into(),
            repo_name: "b".into(),
            installed_at_ms: 0,
            commit_sha: None,
            format: Default::default(),
            skills: vec![SkillState {
                name: "s1".into(),
                relative_path: "s1".into(),
                status: SkillStatus::Pending,
                quarantine_reason: None,
                last_audited_ms: None,
                content_hash: None,
                trusted_hash: None,
                enabled: false,
            }],
        });

        assert!(m.set_skill_status("a/b", "s1", SkillStatus::Trusted));
        assert_eq!(
            m.find_repo("a/b").unwrap().skills[0].status,
            SkillStatus::Trusted
        );
        assert!(!m.set_skill_status("a/b", "missing", SkillStatus::Trusted));
    }

    #[test]
    fn test_manifest_remove_repo() {
        let mut m = SkillsManifest::default();
        m.add_repo(RepoEntry {
            source: "a/b".into(),
            repo_name: "b".into(),
            installed_at_ms: 0,
            commit_sha: None,
            format: Default::default(),
            skills: vec![],
        });
        m.add_repo(RepoEntry {
            source: "c/d".into(),
            repo_name: "d".into(),
            installed_at_ms: 0,
            commit_sha: None,
            format: Default::default(),
            skills: vec![],
        });

        m.remove_repo("a/b");
        assert_eq!(m.repos.len(), 1);
        assert_eq!(m.repos[0].source, "c/d");
    }

    #[test]
    fn test_load_migrates_v1_manifest_to_v3() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skills-manifest.json");
        std::fs::write(
            &path,
            r#"{
              "version": 1,
              "repos": [
                {
                  "source": "owner/repo",
                  "repo_name": "owner-repo",
                  "installed_at_ms": 1,
                  "format": "skill",
                  "skills": [
                    {
                      "name": "s",
                      "relative_path": "owner-repo/s",
                      "trusted": true,
                      "enabled": true
                    }
                  ]
                }
              ]
            }"#,
        )
        .unwrap();

        let store = ManifestStore::new(path.clone());
        let loaded = store.load().unwrap();
        assert_eq!(loaded.version, 3);
        assert_eq!(loaded.repos[0].skills[0].status, SkillStatus::Pending);
        assert!(!loaded.repos[0].skills[0].enabled);

        let rewritten = std::fs::read_to_string(path).unwrap();
        assert!(rewritten.contains("\"version\": 3"));
        assert!(rewritten.contains("\"status\": \"pending\""));
    }
}
