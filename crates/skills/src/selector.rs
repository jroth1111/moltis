//! Deterministic skill selection: score skills by keyword/tag/regex match,
//! then apply a token budget cap.

use crate::types::SkillMetadata;

/// Score a skill against a user message.
/// Returns 0 if no match, higher = better match.
pub fn score_skill(skill: &SkillMetadata, message: &str) -> u32 {
    let lower = message.to_lowercase();
    let mut score = 0u32;
    let name_lower = skill.name.to_lowercase();

    // Keyword match: skill name appears in message (+10, capped at 10)
    if lower.contains(&name_lower) {
        score = score.saturating_add(10);
    }

    // Description keyword match (+3 per word, cap at 9)
    let desc_words: Vec<&str> = skill
        .description
        .split_whitespace()
        .filter(|w| w.len() > 4)
        .collect();
    let word_matches = desc_words
        .iter()
        .filter(|&&w| lower.contains(&w.to_lowercase()))
        .count()
        .min(3); // cap at 3 words
    score = score.saturating_add(word_matches as u32 * 3);

    score
}

/// Select skills that are relevant to the message, respecting a token budget.
///
/// Returns skill indices sorted by score descending.
/// `token_budget` limits total estimated skill prompt tokens (default 4000).
pub fn select_skills<'a>(
    skills: &'a [SkillMetadata],
    message: &str,
    token_budget: usize,
) -> Vec<&'a SkillMetadata> {
    let mut scored: Vec<(u32, usize)> = skills
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let score = score_skill(s, message);
            if score > 0 {
                Some((score, i))
            } else {
                None
            }
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));

    let mut result = Vec::new();
    let mut used_tokens = 0usize;
    for (_, idx) in scored {
        let skill = &skills[idx];
        // Estimate ~4 bytes per token, skill prompt ~= description length
        let estimated_tokens = (skill.description.len() / 4).max(50);
        if used_tokens + estimated_tokens > token_budget {
            continue; // skip over-budget skill; a smaller one later may still fit
        }
        used_tokens += estimated_tokens;
        result.push(skill);
    }
    result
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::types::{SkillMetadata, SkillRequirements},
        std::path::PathBuf,
    };

    fn mock_skill(name: &str, description: &str) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            description: description.to_string(),
            homepage: None,
            license: None,
            compatibility: None,
            allowed_tools: vec![],
            dockerfile: None,
            requires: SkillRequirements::default(),
            path: PathBuf::new(),
            source: None,
        }
    }

    #[test]
    fn name_match_scores_10() {
        let skill = mock_skill("git-helper", "Git workflow assistant");
        assert_eq!(score_skill(&skill, "use git-helper for this"), 10);
    }

    #[test]
    fn no_match_scores_0() {
        let skill = mock_skill("billing", "Invoice and payment");
        assert_eq!(score_skill(&skill, "how do I write tests"), 0);
    }

    #[test]
    fn select_skills_respects_budget() {
        let skills = vec![
            mock_skill("a", &"a".repeat(1000)),
            mock_skill("b", &"b".repeat(1000)),
        ];
        // With a tiny budget only one skill fits
        let msg = "use a and b";
        let selected = select_skills(&skills, msg, 300);
        assert!(selected.len() <= 1);
    }

    #[test]
    fn select_skills_skips_oversized_top_skill_and_keeps_scanning() {
        let skills = vec![
            mock_skill(
                "oversized",
                &format!(
                    "{} {} {} {}",
                    "oversized-description",
                    "keywordalpha",
                    "keywordbeta",
                    "x".repeat(2200)
                ),
            ),
            mock_skill("compact", "short description"),
        ];
        let msg = "use oversized keywordalpha keywordbeta and compact";

        let selected = select_skills(&skills, msg, 120);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "compact");
    }
}
