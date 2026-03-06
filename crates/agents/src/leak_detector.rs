//! Credential leak detection for tool results and LLM responses.
//!
//! Scans text for 15+ known credential patterns (API keys, tokens, PEM blocks,
//! database URLs, etc.) and optionally applies Shannon entropy detection for
//! high-entropy strings that may be secrets.
//!
//! Ported from IronClaw regex patterns + ZeroClaw entropy heuristics.

use std::sync::OnceLock;

use regex::Regex;

/// Action to take when a credential pattern is detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeakAction {
    /// Block the entire content from being returned.
    Block,
    /// Replace the matched region with a redaction marker.
    Redact,
    /// Log a warning but allow the content through.
    Warn,
}

/// A single credential leak match within scanned content.
#[derive(Debug, Clone)]
pub struct LeakMatch {
    /// Name of the pattern that matched.
    pub pattern_name: &'static str,
    /// Action to take for this match.
    pub action: LeakAction,
    /// Byte offset of the match start.
    pub start: usize,
    /// Byte offset of the match end.
    pub end: usize,
}

type PatternEntry = (Regex, &'static str, LeakAction);
type RawPatternEntry = (&'static str, &'static str, LeakAction);
const DETECTOR_UNAVAILABLE_PATTERN: &str = "leak_detector_unavailable";

/// Compiled credential patterns, initialised once.
fn patterns() -> Result<&'static [PatternEntry], &'static regex::Error> {
    static PATTERNS: OnceLock<Result<Vec<PatternEntry>, regex::Error>> = OnceLock::new();
    match PATTERNS.get_or_init(|| compile_patterns(raw_patterns())) {
        Ok(patterns) => Ok(patterns.as_slice()),
        Err(error) => Err(error),
    }
}

fn compile_patterns(raw: &[RawPatternEntry]) -> Result<Vec<PatternEntry>, regex::Error> {
    raw.iter()
        .map(|(pattern, name, action)| Regex::new(pattern).map(|regex| (regex, *name, *action)))
        .collect()
}

fn raw_patterns() -> &'static [RawPatternEntry] {
    &[
        (r"sk-[A-Za-z0-9]{20,}", "openai_api_key", LeakAction::Redact),
        (
            r"sk-ant-api[0-9]{2}-[A-Za-z0-9\-_]{93,}",
            "anthropic_api_key",
            LeakAction::Block,
        ),
        (r"AKIA[0-9A-Z]{16}", "aws_access_key", LeakAction::Block),
        (
            r"gh[pousr]_[A-Za-z0-9]{36,}",
            "github_pat",
            LeakAction::Block,
        ),
        (
            r"sk_live_[A-Za-z0-9]{24,}",
            "stripe_live_key",
            LeakAction::Redact,
        ),
        (r"-----BEGIN [A-Z ]+-----", "pem_block", LeakAction::Warn),
        (
            r"eyJ[A-Za-z0-9\-_=]+\.eyJ[A-Za-z0-9\-_=]+\.[A-Za-z0-9\-_.+/=]+",
            "jwt",
            LeakAction::Redact,
        ),
        (
            r"[Bb]earer\s+[A-Za-z0-9\-_.~+/=]{20,}",
            "bearer_token",
            LeakAction::Warn,
        ),
        (
            r#"(?i)(api[_\-]?key|secret|token|password|passwd|pwd)\s*[=:]\s*['"]?([A-Za-z0-9\-_.~+/=]{16,})['"]?"#,
            "generic_secret",
            LeakAction::Redact,
        ),
        (
            r"xox[baprs]-[0-9A-Za-z\-]{10,}",
            "slack_token",
            LeakAction::Redact,
        ),
        (
            r#""type"\s*:\s*"service_account""#,
            "gcp_service_account",
            LeakAction::Warn,
        ),
        (
            r"-----BEGIN (RSA|EC|DSA|OPENSSH) PRIVATE KEY",
            "private_key",
            LeakAction::Block,
        ),
        (
            r"(?i)(postgres|mysql|mongodb)://[^:]+:[^@]+@",
            "database_url_with_creds",
            LeakAction::Redact,
        ),
        (r"npm_[A-Za-z0-9]{36}", "npm_token", LeakAction::Redact),
    ]
}

fn detector_unavailable_match(content: &str) -> LeakMatch {
    LeakMatch {
        pattern_name: DETECTOR_UNAVAILABLE_PATTERN,
        action: LeakAction::Block,
        start: 0,
        end: content.len(),
    }
}

/// Compute Shannon entropy (bits per character) of a string.
fn shannon_entropy(s: &str) -> f64 {
    let bytes = s.as_bytes();
    let len = bytes.len() as f64;
    if len == 0.0 {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// Credential leak detector with configurable sensitivity.
pub struct LeakDetector {
    sensitivity: f64,
}

impl LeakDetector {
    /// Create a new detector.
    ///
    /// `sensitivity` is clamped to `[0.0, 1.0]`.  At 0.0 detection is off.
    /// At > 0.5, Shannon entropy heuristics are also enabled.
    #[must_use]
    pub fn new(sensitivity: f64) -> Self {
        Self {
            sensitivity: sensitivity.clamp(0.0, 1.0),
        }
    }

    /// Scan `content` and return all credential leak matches.
    #[must_use]
    pub fn scan(&self, content: &str) -> Vec<LeakMatch> {
        if self.sensitivity <= 0.0 {
            return Vec::new();
        }

        let mut matches = Vec::new();

        // Regex-based pattern matching.
        let compiled_patterns = match patterns() {
            Ok(patterns) => patterns,
            Err(_) => return vec![detector_unavailable_match(content)],
        };
        for (re, name, action) in compiled_patterns {
            for m in re.find_iter(content) {
                matches.push(LeakMatch {
                    pattern_name: name,
                    action: *action,
                    start: m.start(),
                    end: m.end(),
                });
            }
        }

        // Shannon entropy detection (only when sensitivity > 0.5).
        if self.sensitivity > 0.5 {
            // Find contiguous runs of non-whitespace chars >= 20 long.
            let mut start = None;
            for (i, ch) in content.char_indices() {
                if !ch.is_whitespace() {
                    if start.is_none() {
                        start = Some(i);
                    }
                } else if let Some(s) = start {
                    let end = i;
                    let segment = &content[s..end];
                    if segment.len() >= 20 && shannon_entropy(segment) > 4.2 {
                        // Only add if not already covered by a regex match.
                        let already_covered = matches.iter().any(|m| m.start <= s && m.end >= end);
                        if !already_covered {
                            matches.push(LeakMatch {
                                pattern_name: "high_entropy_string",
                                action: LeakAction::Redact,
                                start: s,
                                end,
                            });
                        }
                    }
                    start = None;
                }
            }
            // Handle trailing segment.
            if let Some(s) = start {
                let end = content.len();
                let segment = &content[s..end];
                if segment.len() >= 20 && shannon_entropy(segment) > 4.2 {
                    let already_covered = matches.iter().any(|m| m.start <= s && m.end >= end);
                    if !already_covered {
                        matches.push(LeakMatch {
                            pattern_name: "high_entropy_string",
                            action: LeakAction::Redact,
                            start: s,
                            end,
                        });
                    }
                }
            }
        }

        // Sort by start offset for deterministic processing.
        matches.sort_by_key(|m| m.start);
        matches
    }

    /// Apply redactions to `content`.
    ///
    /// - `Redact` matches are replaced with `[REDACTED_<PATTERN_NAME>]`.
    /// - `Warn` matches are left in place (caller should log).
    /// - `Block` matches cause the entire content to be rejected — an `Err`
    ///   is returned containing the blocking pattern name.
    pub fn apply(&self, content: &str) -> Result<String, String> {
        let matches = self.scan(content);
        if matches.is_empty() {
            return Ok(content.to_string());
        }

        // Check for any Block action first.
        for m in &matches {
            if m.action == LeakAction::Block {
                return Err(m.pattern_name.to_string());
            }
        }

        // Build redacted output.
        let mut result = String::with_capacity(content.len());
        let mut cursor = 0;

        for m in &matches {
            if m.action == LeakAction::Warn {
                // Warn-only: leave content in place.
                continue;
            }

            // m.action == LeakAction::Redact
            if m.start > cursor {
                result.push_str(&content[cursor..m.start]);
            } else if m.start < cursor {
                // Overlapping match already handled — skip.
                continue;
            }

            let upper_name = m.pattern_name.to_uppercase();
            result.push_str(&format!("[REDACTED_{upper_name}]"));
            cursor = m.end;
        }

        // Append remaining content after last redaction.
        if cursor < content.len() {
            result.push_str(&content[cursor..]);
        }

        Ok(result)
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    // ── Pattern matching tests ─────────────────────────────────────

    #[test]
    fn detects_openai_key() {
        let detector = LeakDetector::new(0.5);
        let input = "key is sk-abcdefghijklmnopqrstuvwxyz";
        let matches = detector.scan(input);
        assert!(!matches.is_empty());
        assert_eq!(matches[0].pattern_name, "openai_api_key");
        assert_eq!(matches[0].action, LeakAction::Redact);
    }

    #[test]
    fn detects_anthropic_key() {
        let detector = LeakDetector::new(0.5);
        let key = format!("sk-ant-api03-{}", "a".repeat(93));
        let input = format!("here: {key}");
        let matches = detector.scan(&input);
        assert!(
            matches
                .iter()
                .any(|m| m.pattern_name == "anthropic_api_key"),
            "should detect Anthropic key: {matches:?}"
        );
    }

    #[test]
    fn detects_aws_key() {
        let detector = LeakDetector::new(0.5);
        let input = "AKIAIOSFODNN7EXAMPLE";
        let matches = detector.scan(input);
        assert!(matches.iter().any(|m| m.pattern_name == "aws_access_key"));
        assert_eq!(
            matches
                .iter()
                .find(|m| m.pattern_name == "aws_access_key")
                .unwrap()
                .action,
            LeakAction::Block
        );
    }

    #[test]
    fn detects_github_pat() {
        let detector = LeakDetector::new(0.5);
        let token = format!("ghp_{}", "a".repeat(36));
        let matches = detector.scan(&token);
        assert!(matches.iter().any(|m| m.pattern_name == "github_pat"));
    }

    #[test]
    fn detects_stripe_live_key() {
        let detector = LeakDetector::new(0.5);
        let key = format!("sk_live_{}", "x".repeat(24));
        let matches = detector.scan(&key);
        assert!(matches.iter().any(|m| m.pattern_name == "stripe_live_key"));
    }

    #[test]
    fn detects_pem_block() {
        let detector = LeakDetector::new(0.5);
        let input = "-----BEGIN CERTIFICATE-----\nMIIBxTCCAW...";
        let matches = detector.scan(input);
        assert!(matches.iter().any(|m| m.pattern_name == "pem_block"));
        assert_eq!(
            matches
                .iter()
                .find(|m| m.pattern_name == "pem_block")
                .unwrap()
                .action,
            LeakAction::Warn
        );
    }

    #[test]
    fn detects_jwt() {
        let detector = LeakDetector::new(0.5);
        let input = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abc123def456";
        let matches = detector.scan(input);
        assert!(matches.iter().any(|m| m.pattern_name == "jwt"));
    }

    #[test]
    fn detects_bearer_token() {
        let detector = LeakDetector::new(0.5);
        let input = "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
        let matches = detector.scan(input);
        assert!(matches.iter().any(|m| m.pattern_name == "bearer_token"));
    }

    #[test]
    fn detects_generic_secret() {
        let detector = LeakDetector::new(0.5);
        let input = "api_key = 'abcdef1234567890xx'";
        let matches = detector.scan(input);
        assert!(matches.iter().any(|m| m.pattern_name == "generic_secret"));
    }

    #[test]
    fn detects_slack_token() {
        let detector = LeakDetector::new(0.5);
        let input = "xoxb-1234567890-abcdef";
        let matches = detector.scan(input);
        assert!(matches.iter().any(|m| m.pattern_name == "slack_token"));
    }

    #[test]
    fn detects_gcp_service_account() {
        let detector = LeakDetector::new(0.5);
        let input = r#"{"type": "service_account", "project_id": "foo"}"#;
        let matches = detector.scan(input);
        assert!(
            matches
                .iter()
                .any(|m| m.pattern_name == "gcp_service_account")
        );
    }

    #[test]
    fn detects_private_key() {
        let detector = LeakDetector::new(0.5);
        let input = "-----BEGIN RSA PRIVATE KEY-----\nMIIEo...";
        let matches = detector.scan(input);
        assert!(matches.iter().any(|m| m.pattern_name == "private_key"));
        assert_eq!(
            matches
                .iter()
                .find(|m| m.pattern_name == "private_key")
                .unwrap()
                .action,
            LeakAction::Block
        );
    }

    #[test]
    fn detects_database_url() {
        let detector = LeakDetector::new(0.5);
        let input = "postgres://admin:s3cret@db.example.com:5432/app";
        let matches = detector.scan(input);
        assert!(
            matches
                .iter()
                .any(|m| m.pattern_name == "database_url_with_creds")
        );
    }

    #[test]
    fn detects_npm_token() {
        let detector = LeakDetector::new(0.5);
        let token = format!("npm_{}", "a".repeat(36));
        let matches = detector.scan(&token);
        assert!(matches.iter().any(|m| m.pattern_name == "npm_token"));
    }

    // ── Entropy detection tests ────────────────────────────────────

    #[test]
    fn entropy_detection_above_threshold() {
        let detector = LeakDetector::new(0.8);
        // A random-looking high-entropy string.
        let input = "config: aB3$xY9!kL7@mN2#pQ5&rT8";
        let matches = detector.scan(input);
        assert!(
            matches
                .iter()
                .any(|m| m.pattern_name == "high_entropy_string"),
            "should detect high-entropy string: {matches:?}"
        );
    }

    #[test]
    fn entropy_detection_disabled_below_threshold() {
        let detector = LeakDetector::new(0.3);
        // Same high-entropy string, but sensitivity too low.
        let input = "aB3$xY9!kL7@mN2#pQ5&rT8";
        let matches = detector.scan(input);
        assert!(
            !matches
                .iter()
                .any(|m| m.pattern_name == "high_entropy_string"),
            "should not detect entropy at low sensitivity"
        );
    }

    #[test]
    fn low_entropy_string_not_flagged() {
        let detector = LeakDetector::new(1.0);
        let input = "aaaaaaaaaaaaaaaaaaaaaaaaa";
        let matches = detector.scan(input);
        assert!(
            !matches
                .iter()
                .any(|m| m.pattern_name == "high_entropy_string"),
            "low-entropy repeated chars should not trigger"
        );
    }

    // ── apply() tests ──────────────────────────────────────────────

    #[test]
    fn apply_redacts_openai_key() {
        let detector = LeakDetector::new(0.5);
        let input = "The key is sk-abcdefghijklmnopqrstuvwxyz end";
        let result = detector.apply(input).unwrap();
        assert!(result.contains("[REDACTED_OPENAI_API_KEY]"));
        assert!(!result.contains("sk-abcdefghijklmnopqrstuvwxyz"));
        assert!(result.contains("The key is "));
        assert!(result.contains(" end"));
    }

    #[test]
    fn apply_blocks_on_aws_key() {
        let detector = LeakDetector::new(0.5);
        let input = "creds: AKIAIOSFODNN7EXAMPLE";
        let result = detector.apply(input);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "aws_access_key");
    }

    #[test]
    fn apply_blocks_on_private_key() {
        let detector = LeakDetector::new(0.5);
        let input = "-----BEGIN RSA PRIVATE KEY-----\nMIIEo...";
        let result = detector.apply(input);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "private_key");
    }

    #[test]
    fn apply_warns_without_redaction() {
        let detector = LeakDetector::new(0.5);
        let input = "-----BEGIN CERTIFICATE-----\ndata";
        let result = detector.apply(input).unwrap();
        // Warn action: content should be unchanged.
        assert_eq!(result, input);
    }

    #[test]
    fn apply_clean_content_unchanged() {
        let detector = LeakDetector::new(1.0);
        let input = "Hello, world!";
        let result = detector.apply(input).unwrap();
        assert_eq!(result, "Hello, world!");
    }

    #[test]
    fn compile_patterns_returns_error_for_invalid_regex() {
        let result = compile_patterns(&[("(", "broken", LeakAction::Block)]);

        assert!(result.is_err());
    }

    #[test]
    fn detector_unavailable_match_blocks_entire_content() {
        let matched = detector_unavailable_match("secret");

        assert_eq!(matched.pattern_name, DETECTOR_UNAVAILABLE_PATTERN);
        assert_eq!(matched.action, LeakAction::Block);
        assert_eq!(matched.start, 0);
        assert_eq!(matched.end, 6);
    }

    #[test]
    fn sensitivity_zero_disables_detection() {
        let detector = LeakDetector::new(0.0);
        let input = "AKIAIOSFODNN7EXAMPLE sk-abcdefghijklmnopqrstuvwxyz";
        let matches = detector.scan(input);
        assert!(matches.is_empty());
    }

    // ── Shannon entropy unit test ──────────────────────────────────

    #[test]
    fn entropy_of_uniform_string() {
        // "aaaa..." has 0 entropy.
        let e = shannon_entropy("aaaaaaaaaa");
        assert!((e - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn entropy_of_binary_string() {
        // "abababab" has 1.0 bits/char entropy.
        let e = shannon_entropy("abababab");
        assert!((e - 1.0).abs() < 0.01);
    }

    #[test]
    fn entropy_empty_string() {
        assert!((shannon_entropy("") - 0.0).abs() < f64::EPSILON);
    }
}
