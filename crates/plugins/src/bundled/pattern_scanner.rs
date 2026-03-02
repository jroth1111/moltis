//! Shared pattern scanner using Aho-Corasick for literal matching
//! and a regex for high-entropy string detection.

use aho_corasick::{AhoCorasick, MatchKind};
use regex::Regex;
use unicode_normalization::UnicodeNormalization;

pub struct PatternScanner {
    literals: Option<AhoCorasick>,
    high_entropy: Option<Regex>,
}

impl PatternScanner {
    pub fn new(literals: &[&str]) -> Self {
        let normalized_literals: Vec<String> = literals
            .iter()
            .map(|s| normalize_nfkc_lowercase(s))
            .collect();
        let literals = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostFirst)
            .build(normalized_literals)
            .ok();
        let high_entropy = Regex::new(r"[A-Za-z0-9+/]{32,}={0,2}|[A-Za-z0-9_\-]{40,}").ok();
        Self {
            literals,
            high_entropy,
        }
    }

    pub fn matches(&self, text: &str) -> bool {
        let normalized = normalize_nfkc_lowercase(text);

        if let Some(ac) = &self.literals
            && ac.is_match(&normalized)
        {
            return true;
        }

        self.high_entropy
            .as_ref()
            .is_some_and(|regex| regex.is_match(&normalized))
    }
}

fn normalize_nfkc_lowercase(input: &str) -> String {
    input.nfkc().flat_map(|c| c.to_lowercase()).collect()
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_match() {
        let scanner = PatternScanner::new(&["sk-", "api_key"]);
        assert!(scanner.matches("my sk-1234 token"));
        assert!(scanner.matches("API_KEY=secret"));
        assert!(!scanner.matches("nothing here"));
    }

    #[test]
    fn high_entropy_match() {
        let scanner = PatternScanner::new(&[]);
        // 40-char alphanumeric string
        assert!(scanner.matches("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklm"));
        assert!(!scanner.matches("short"));
    }

    #[test]
    fn unicode_normalization_match() {
        let scanner = PatternScanner::new(&["ignore previous instructions"]);
        let input = "Ignore\u{00A0}Previous\u{00A0}Instructions";
        assert!(scanner.matches(input));
    }
}
