use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use crate::{
    audit, integrity,
    manifest::ManifestStore,
    parse,
    types::{RepoEntry, SkillSource, SkillState, SkillStatus, SkillsManifest},
};

const PERSONAL_SOURCE_KEY: &str = "personal";
const PROJECT_SOURCE_KEY: &str = "project";
const PERSONAL_REPO_NAME: &str = "__local-personal__";
const PROJECT_REPO_NAME: &str = "__local-project__";

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn is_local_source_key(source: &str) -> bool {
    matches!(source, PERSONAL_SOURCE_KEY | PROJECT_SOURCE_KEY)
}

pub fn local_source_key(source: &SkillSource) -> Option<&'static str> {
    match source {
        SkillSource::Personal => Some(PERSONAL_SOURCE_KEY),
        SkillSource::Project => Some(PROJECT_SOURCE_KEY),
        _ => None,
    }
}

pub fn local_source_from_key(source: &str) -> Option<SkillSource> {
    match source {
        PERSONAL_SOURCE_KEY => Some(SkillSource::Personal),
        PROJECT_SOURCE_KEY => Some(SkillSource::Project),
        _ => None,
    }
}

pub fn local_repo_root_from_source(source: &SkillSource) -> Option<PathBuf> {
    let data_dir = moltis_config::data_dir();
    match source {
        SkillSource::Personal => Some(data_dir.join("skills")),
        SkillSource::Project => Some(data_dir.join(".moltis/skills")),
        _ => None,
    }
}

pub fn local_repo_root_from_key(source: &str) -> Option<PathBuf> {
    local_source_from_key(source).and_then(|source| local_repo_root_from_source(&source))
}

pub fn local_repo_name(source: &SkillSource) -> Option<&'static str> {
    match source {
        SkillSource::Personal => Some(PERSONAL_REPO_NAME),
        SkillSource::Project => Some(PROJECT_REPO_NAME),
        _ => None,
    }
}

pub fn load_synced_manifest() -> anyhow::Result<SkillsManifest> {
    let store = ManifestStore::new(ManifestStore::default_path()?);
    let mut manifest = store.load()?;
    if sync_local_skill_manifest(&mut manifest, current_time_ms())? {
        store.save(&manifest)?;
    }
    Ok(manifest)
}

pub fn sync_local_skill_manifest(
    manifest: &mut SkillsManifest,
    now_ms: u64,
) -> anyhow::Result<bool> {
    let mut changed = false;
    changed |= sync_local_repo(manifest, SkillSource::Project, now_ms)?;
    changed |= sync_local_repo(manifest, SkillSource::Personal, now_ms)?;
    Ok(changed)
}

pub fn local_skill_state<'a>(
    manifest: &'a SkillsManifest,
    source: &SkillSource,
    skill_name: &str,
) -> Option<&'a SkillState> {
    let source_key = local_source_key(source)?;
    manifest
        .find_repo(source_key)
        .and_then(|repo| repo.skills.iter().find(|skill| skill.name == skill_name))
}

fn sync_local_repo(
    manifest: &mut SkillsManifest,
    source: SkillSource,
    now_ms: u64,
) -> anyhow::Result<bool> {
    let Some(source_key) = local_source_key(&source) else {
        return Ok(false);
    };
    let Some(repo_name) = local_repo_name(&source) else {
        return Ok(false);
    };
    let Some(base_path) = local_repo_root_from_source(&source) else {
        return Ok(false);
    };

    let existing_repo = manifest.find_repo(source_key).cloned();
    let existing_skills: HashMap<String, SkillState> = existing_repo
        .as_ref()
        .map(|repo| {
            repo.skills
                .iter()
                .cloned()
                .map(|skill| (skill.name.clone(), skill))
                .collect()
        })
        .unwrap_or_default();

    let next_skills = scan_local_skills(&base_path, &source, existing_skills);

    if next_skills.is_empty() {
        if manifest.find_repo(source_key).is_some() {
            manifest.remove_repo(source_key);
            return Ok(true);
        }
        return Ok(false);
    }

    let next_repo = RepoEntry {
        source: source_key.to_string(),
        repo_name: repo_name.to_string(),
        installed_at_ms: existing_repo
            .as_ref()
            .map(|repo| repo.installed_at_ms)
            .unwrap_or(now_ms),
        commit_sha: None,
        format: crate::formats::PluginFormat::Skill,
        skills: next_skills,
    };

    if let Some(repo) = manifest.find_repo_mut(source_key) {
        if repo.repo_name == next_repo.repo_name
            && repo.commit_sha == next_repo.commit_sha
            && repo.format == next_repo.format
            && repo.skills == next_repo.skills
        {
            return Ok(false);
        }
        *repo = next_repo;
        return Ok(true);
    }

    manifest.add_repo(next_repo);
    Ok(true)
}

fn scan_local_skills(
    base_path: &Path,
    source: &SkillSource,
    existing_skills: HashMap<String, SkillState>,
) -> Vec<SkillState> {
    let entries = match std::fs::read_dir(base_path) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut skills = Vec::new();
    for entry in entries.flatten() {
        let skill_dir = entry.path();
        if !skill_dir.is_dir() {
            continue;
        }
        let Some(skill_name) = skill_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let skill_md = skill_dir.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        if let Err(error) = audit::ensure_not_symlink(&skill_dir) {
            tracing::warn!(?skill_dir, %error, "skipping symlinked local skill directory");
            continue;
        }
        if let Err(error) = audit::ensure_not_symlink(&skill_md) {
            tracing::warn!(?skill_md, %error, "skipping symlinked local SKILL.md");
            continue;
        }

        let raw = match std::fs::read_to_string(&skill_md) {
            Ok(raw) => raw,
            Err(error) => {
                tracing::warn!(?skill_md, %error, "failed to read local SKILL.md");
                continue;
            },
        };
        if let Err(error) = audit::audit_skill_markdown(&skill_dir, &raw, &skill_md) {
            tracing::warn!(?skill_md, %error, "blocked local skill during audit");
            continue;
        }
        if let Err(error) = parse::parse_metadata(&raw, &skill_dir) {
            tracing::warn!(?skill_dir, %error, "failed to parse local SKILL.md");
            continue;
        }

        let existing = existing_skills.get(skill_name).cloned();
        skills.push(refreshed_local_skill_state(
            existing,
            skill_name,
            integrity::hash_skill_markdown(&raw),
        ));
        tracing::info!(
            path = %skill_md.display(),
            source = ?source,
            name = %skill_name,
            "synced local SKILL.md into manifest"
        );
    }

    skills.sort_by(|left, right| left.name.cmp(&right.name));
    skills
}

fn refreshed_local_skill_state(
    existing: Option<SkillState>,
    skill_name: &str,
    content_hash: String,
) -> SkillState {
    let mut state = existing.unwrap_or(SkillState {
        name: skill_name.to_string(),
        relative_path: skill_name.to_string(),
        status: SkillStatus::Pending,
        quarantine_reason: None,
        last_audited_ms: None,
        content_hash: None,
        trusted_hash: None,
        enabled: false,
    });

    let content_changed = state.content_hash.as_deref() != Some(content_hash.as_str());
    state.name = skill_name.to_string();
    state.relative_path = skill_name.to_string();
    state.content_hash = Some(content_hash);

    if content_changed {
        state.status = SkillStatus::Pending;
        state.quarantine_reason = None;
        state.last_audited_ms = None;
        state.trusted_hash = None;
        state.enabled = false;
    }

    state
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::*;

    fn data_dir_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn write_local_skill(base: &Path, name: &str, body: &str) {
        let skill_dir = base.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), body).unwrap();
    }

    fn valid_skill(name: &str, description: &str) -> String {
        format!(
            "---\nversion: 3\nname: {name}\ndescription: {description}\ntriggers:\n  should_trigger:\n    - first\n    - second\n    - third\n  should_not_trigger:\n    - fourth\n    - fifth\n    - sixth\nevals:\n  path: evals/evals.json\n---\n## Purpose\n\n{description}\n\n## Inputs\n\n- request\n- context\n- constraints\n\n## Workflow\n\n- do the work\n\n## Failure Modes\n\n- stop on missing input\n\n## Examples\n\n- Example prompt\n"
        )
    }

    #[test]
    fn sync_local_manifest_adds_pending_disabled_skills() {
        let _guard = data_dir_lock().lock().expect("data dir lock poisoned");
        let tmp = tempfile::tempdir().unwrap();
        moltis_config::set_data_dir(tmp.path().to_path_buf());
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                moltis_config::clear_data_dir();
            }
        }
        let _reset = Reset;

        write_local_skill(
            &tmp.path().join("skills"),
            "demo",
            &valid_skill("demo", "Demo skill"),
        );

        let mut manifest = SkillsManifest::default();
        let changed = sync_local_skill_manifest(&mut manifest, 55).unwrap();
        assert!(changed);

        let repo = manifest.find_repo("personal").expect("personal repo missing");
        assert_eq!(repo.skills.len(), 1);
        assert_eq!(repo.skills[0].name, "demo");
        assert_eq!(repo.skills[0].status, SkillStatus::Pending);
        assert!(!repo.skills[0].enabled);
        assert!(repo.skills[0].content_hash.is_some());
    }

    #[test]
    fn sync_local_manifest_resets_changed_trusted_skill_to_pending() {
        let _guard = data_dir_lock().lock().expect("data dir lock poisoned");
        let tmp = tempfile::tempdir().unwrap();
        moltis_config::set_data_dir(tmp.path().to_path_buf());
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                moltis_config::clear_data_dir();
            }
        }
        let _reset = Reset;

        let original = valid_skill("demo", "Demo skill");
        write_local_skill(&tmp.path().join("skills"), "demo", &original);

        let original_hash = integrity::hash_skill_markdown(&original);
        let mut manifest = SkillsManifest {
            version: 3,
            repos: vec![RepoEntry {
                source: "personal".to_string(),
                repo_name: PERSONAL_REPO_NAME.to_string(),
                installed_at_ms: 1,
                commit_sha: None,
                format: crate::formats::PluginFormat::Skill,
                skills: vec![SkillState {
                    name: "demo".to_string(),
                    relative_path: "demo".to_string(),
                    status: SkillStatus::Trusted,
                    quarantine_reason: None,
                    last_audited_ms: Some(7),
                    content_hash: Some(original_hash.clone()),
                    trusted_hash: Some(original_hash),
                    enabled: true,
                }],
            }],
        };

        let changed = format!("{original}\n<!-- changed -->\n");
        write_local_skill(&tmp.path().join("skills"), "demo", &changed);

        assert!(sync_local_skill_manifest(&mut manifest, 77).unwrap());
        let skill = &manifest.find_repo("personal").unwrap().skills[0];
        assert_eq!(skill.status, SkillStatus::Pending);
        assert!(!skill.enabled);
        assert!(skill.trusted_hash.is_none());
        assert!(skill.last_audited_ms.is_none());
        assert_eq!(
            skill.content_hash.as_deref(),
            Some(integrity::hash_skill_markdown(&changed).as_str())
        );
    }
}
