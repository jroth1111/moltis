use sha2::{Digest, Sha256};

use crate::types::{SkillState, SkillStatus};

pub fn hash_skill_markdown(markdown: &str) -> String {
    hash_bytes(markdown.as_bytes())
}

pub fn hash_adapter_skill(source_file: Option<&str>, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_file.unwrap_or_default().as_bytes());
    hasher.update(b"\n");
    hasher.update(body.as_bytes());
    let digest = hasher.finalize();
    hex_digest(digest.as_ref())
}

pub fn trust_skill(skill: &mut SkillState, audited_ms: u64) {
    skill.status = SkillStatus::Trusted;
    skill.quarantine_reason = None;
    skill.last_audited_ms = Some(audited_ms);
    skill.trusted_hash = skill.content_hash.clone();
}

pub fn untrust_skill(skill: &mut SkillState, audited_ms: u64) {
    skill.status = SkillStatus::Pending;
    skill.enabled = false;
    skill.quarantine_reason = None;
    skill.last_audited_ms = Some(audited_ms);
    skill.trusted_hash = None;
}

pub fn quarantine_skill(skill: &mut SkillState, reason: &str, audited_ms: u64) -> bool {
    let mut changed = false;
    if skill.status != SkillStatus::Quarantined {
        skill.status = SkillStatus::Quarantined;
        changed = true;
    }
    if skill.enabled {
        skill.enabled = false;
        changed = true;
    }
    if skill.quarantine_reason.as_deref() != Some(reason) {
        skill.quarantine_reason = Some(reason.to_string());
        changed = true;
    }
    if changed {
        skill.last_audited_ms = Some(audited_ms);
    }
    changed
}

pub fn integrity_matches_trusted_hash(skill: &SkillState) -> bool {
    match (&skill.content_hash, &skill.trusted_hash) {
        (_, None) => true,
        (Some(content), Some(trusted)) => content == trusted,
        (None, Some(_)) => false,
    }
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex_digest(digest.as_ref())
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_hash_changes_with_source_path() {
        let h1 = hash_adapter_skill(Some("skills/a.md"), "body");
        let h2 = hash_adapter_skill(Some("skills/b.md"), "body");
        assert_ne!(h1, h2);
    }

    #[test]
    fn trust_pins_trusted_hash() {
        let mut skill = SkillState {
            name: "demo".into(),
            relative_path: "demo".into(),
            status: SkillStatus::Pending,
            quarantine_reason: None,
            last_audited_ms: None,
            content_hash: Some(hash_skill_markdown("body")),
            trusted_hash: None,
            enabled: false,
        };
        trust_skill(&mut skill, 7);
        assert_eq!(skill.status, SkillStatus::Trusted);
        assert_eq!(skill.trusted_hash, skill.content_hash);
        assert_eq!(skill.last_audited_ms, Some(7));
    }
}
