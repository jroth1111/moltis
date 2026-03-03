//! Shared pattern scanner using Aho-Corasick for literal matching
//! and a regex for high-entropy string detection.

use {
    aho_corasick::{AhoCorasick, MatchKind},
    regex::Regex,
    unicode_normalization::UnicodeNormalization,
};

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

/// Removes `data:image/...;base64,<payload>` sequences from the input string.
///
/// Used before pattern scanning to avoid false positives from screenshot
/// base64 payloads injected by `screenshot_resolver`.
pub fn strip_image_data_uris(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    let tag = "data:image/";

    while let Some(start) = rest.find(tag) {
        output.push_str(&rest[..start]);
        let after = &rest[start..];
        if let Some(base64_pos) = after.find("base64,") {
            let payload_start = start + base64_pos + "base64,".len();
            let payload = &rest[payload_start..];
            let payload_len = payload
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
                .count();
            let drop_len = (payload_start - start) + payload_len;
            rest = &rest[start + drop_len..];
        } else {
            output.push_str(tag);
            rest = &rest[start + tag.len()..];
        }
    }

    output.push_str(rest);
    output
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
