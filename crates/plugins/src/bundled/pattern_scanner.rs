//! Shared pattern scanner using Aho-Corasick for literal matching
//! and a regex for high-entropy string detection.

use regex::Regex;

pub struct PatternScanner {
    literals: Vec<String>,
    high_entropy: Regex,
}

impl PatternScanner {
    pub fn new(literals: &[&str]) -> Self {
        let literals = literals.iter().map(|s| s.to_lowercase()).collect();
        let high_entropy = Regex::new(
            r"[A-Za-z0-9+/]{32,}={0,2}|[A-Za-z0-9_\-]{40,}",
        )
        .unwrap_or_else(|_| Regex::new("a]^").unwrap_or_else(|_| unreachable!()));
        Self {
            literals,
            high_entropy,
        }
    }

    pub fn matches(&self, text: &str) -> bool {
        let normalized = text.to_lowercase();
        for lit in &self.literals {
            if normalized.contains(lit.as_str()) {
                return true;
            }
        }
        self.high_entropy.is_match(text)
    }
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
}
