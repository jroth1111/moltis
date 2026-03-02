//! Structured handoff context for inter-session coordination.
//!
//! When a session ends (or sends a message to another session), it can attach
//! a [`HandoffContext`] that summarises the work done, errors observed, and
//! dead-ends encountered. The receiving session injects dead-ends as "do not
//! retry" constraints in its system prompt, avoiding redundant work.

use std::collections::HashSet;

use {
    serde::{Deserialize, Serialize},
    sha2::{Digest, Sha256},
};

/// Maximum number of dead-ends kept in a single handoff context.
pub const MAX_DEAD_ENDS: usize = 50;

/// Namespace used when persisting handoff summaries in `SessionStateStore`.
pub const HANDOFF_NAMESPACE: &str = "handoff";

/// Key under which the serialised [`HandoffContext`] is stored.
pub const HANDOFF_CONTEXT_KEY: &str = "context";

// ── DeadEnd ────────────────────────────────────────────────────────────────

/// A single dead-end: an action that was tried and failed in a way that
/// should not be retried by a successor session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeadEnd {
    /// The tool or action name that produced the failure.
    pub tool_name: String,
    /// Human-readable description of what was attempted.
    pub description: String,
    /// Stable fingerprint for deduplication: `sha256(tool_name + '\0' + error)`.
    pub fingerprint: String,
    /// The raw error message (truncated to 512 chars for storage).
    pub error: String,
}

impl DeadEnd {
    /// Create a new dead-end, computing its fingerprint.
    pub fn new(tool_name: impl Into<String>, description: impl Into<String>, error: impl Into<String>) -> Self {
        let tool_name = tool_name.into();
        let error = truncate_string(error.into(), 512);
        let fingerprint = compute_fingerprint(&tool_name, &error);
        Self {
            tool_name,
            description: description.into(),
            fingerprint,
            error,
        }
    }
}

// ── HandoffContext ──────────────────────────────────────────────────────────

/// Structured context attached to inter-session messages.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct HandoffContext {
    /// What the session was last doing when it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action: Option<String>,

    /// The error that triggered the handoff (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_error: Option<String>,

    /// Dead-ends encountered during the session. Capped at [`MAX_DEAD_ENDS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dead_ends: Vec<DeadEnd>,

    /// Recommended next step for the receiving session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_next_step: Option<String>,

    /// Estimated token count of the source session's final context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_tokens: Option<u64>,
}

impl HandoffContext {
    /// Add a dead-end, deduplicating by fingerprint.
    ///
    /// Returns `true` if the dead-end was actually inserted (not a duplicate
    /// and not over the cap).
    pub fn add_dead_end(&mut self, dead_end: DeadEnd) -> bool {
        // Deduplicate.
        if self.dead_ends.iter().any(|d| d.fingerprint == dead_end.fingerprint) {
            return false;
        }
        // Cap.
        if self.dead_ends.len() >= MAX_DEAD_ENDS {
            return false;
        }
        self.dead_ends.push(dead_end);
        true
    }

    /// Merge another context's dead-ends into this one (deduplicating).
    pub fn merge_dead_ends(&mut self, other: &HandoffContext) {
        let existing: HashSet<String> = self
            .dead_ends
            .iter()
            .map(|d| d.fingerprint.clone())
            .collect();
        for dead_end in &other.dead_ends {
            if self.dead_ends.len() >= MAX_DEAD_ENDS {
                break;
            }
            if !existing.contains(&dead_end.fingerprint) {
                self.dead_ends.push(dead_end.clone());
            }
        }
    }

    /// Returns `true` if this context carries no meaningful information.
    pub fn is_empty(&self) -> bool {
        self.last_action.is_none()
            && self.observed_error.is_none()
            && self.dead_ends.is_empty()
            && self.suggested_next_step.is_none()
            && self.estimated_tokens.is_none()
    }

    /// Format dead-ends as system prompt constraints for injection.
    ///
    /// Returns `None` if there are no dead-ends.
    #[must_use]
    pub fn dead_end_constraints(&self) -> Option<String> {
        if self.dead_ends.is_empty() {
            return None;
        }
        let mut lines = Vec::with_capacity(self.dead_ends.len() + 2);
        lines.push("## Dead-ends from prior session (do NOT retry these)".to_string());
        lines.push(String::new());
        for (i, de) in self.dead_ends.iter().enumerate() {
            lines.push(format!(
                "{}. **{}** - {} (error: {})",
                i + 1,
                de.tool_name,
                de.description,
                de.error,
            ));
        }
        Some(lines.join("\n"))
    }

    /// Serialize to JSON string for storage in `SessionStateStore`.
    pub fn to_json(&self) -> crate::Result<String> {
        serde_json::to_string(self).map_err(|e| crate::Error::message(format!("handoff serialization failed: {e}")))
    }

    /// Deserialize from a JSON string.
    pub fn from_json(json: &str) -> crate::Result<Self> {
        serde_json::from_str(json).map_err(|e| crate::Error::message(format!("handoff deserialization failed: {e}")))
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Compute a stable fingerprint from `(tool_name, error)`.
fn compute_fingerprint(tool_name: &str, error: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tool_name.as_bytes());
    hasher.update(b"\0");
    hasher.update(error.as_bytes());
    let hash = hasher.finalize();
    // Use first 16 bytes (32 hex chars) for a compact but collision-resistant ID.
    hex_encode(&hash[..16])
}

/// Encode bytes as lowercase hex.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Truncate a string to at most `max_len` characters.
fn truncate_string(s: String, max_len: usize) -> String {
    if s.len() <= max_len {
        return s;
    }
    let mut end = max_len;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = s;
    result.truncate(end);
    result
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dead_end_fingerprint_is_deterministic() {
        let a = DeadEnd::new("exec", "ran build", "exit code 1");
        let b = DeadEnd::new("exec", "different description", "exit code 1");
        // Same tool_name + error => same fingerprint.
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn dead_end_fingerprint_differs_for_different_errors() {
        let a = DeadEnd::new("exec", "ran build", "exit code 1");
        let b = DeadEnd::new("exec", "ran build", "exit code 2");
        assert_ne!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn dead_end_fingerprint_differs_for_different_tools() {
        let a = DeadEnd::new("exec", "ran build", "exit code 1");
        let b = DeadEnd::new("web_fetch", "ran build", "exit code 1");
        assert_ne!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn add_dead_end_deduplicates() {
        let mut ctx = HandoffContext::default();
        let de1 = DeadEnd::new("exec", "first attempt", "fail");
        let de2 = DeadEnd::new("exec", "second attempt same error", "fail");
        assert!(ctx.add_dead_end(de1));
        assert!(!ctx.add_dead_end(de2)); // Same fingerprint.
        assert_eq!(ctx.dead_ends.len(), 1);
    }

    #[test]
    fn add_dead_end_caps_at_max() {
        let mut ctx = HandoffContext::default();
        for i in 0..MAX_DEAD_ENDS {
            let de = DeadEnd::new("tool", "desc", format!("error-{i}"));
            assert!(ctx.add_dead_end(de));
        }
        assert_eq!(ctx.dead_ends.len(), MAX_DEAD_ENDS);
        let overflow = DeadEnd::new("tool", "desc", "error-overflow");
        assert!(!ctx.add_dead_end(overflow));
        assert_eq!(ctx.dead_ends.len(), MAX_DEAD_ENDS);
    }

    #[test]
    fn merge_dead_ends_deduplicates() {
        let mut ctx_a = HandoffContext::default();
        ctx_a.add_dead_end(DeadEnd::new("exec", "a", "err-1"));
        ctx_a.add_dead_end(DeadEnd::new("exec", "b", "err-2"));

        let mut ctx_b = HandoffContext::default();
        ctx_b.add_dead_end(DeadEnd::new("exec", "c", "err-1")); // Same fingerprint as a.
        ctx_b.add_dead_end(DeadEnd::new("exec", "d", "err-3")); // New.

        ctx_a.merge_dead_ends(&ctx_b);
        assert_eq!(ctx_a.dead_ends.len(), 3);
    }

    #[test]
    fn serialization_roundtrip() {
        let mut ctx = HandoffContext {
            last_action: Some("running tests".into()),
            observed_error: Some("test failed".into()),
            dead_ends: Vec::new(),
            suggested_next_step: Some("fix the test".into()),
            estimated_tokens: Some(4096),
        };
        ctx.add_dead_end(DeadEnd::new("exec", "cargo test", "assertion failed"));

        let json = ctx.to_json().unwrap();
        let restored = HandoffContext::from_json(&json).unwrap();
        assert_eq!(ctx, restored);
    }

    #[test]
    fn empty_context_detection() {
        let ctx = HandoffContext::default();
        assert!(ctx.is_empty());

        let ctx2 = HandoffContext {
            last_action: Some("something".into()),
            ..Default::default()
        };
        assert!(!ctx2.is_empty());
    }

    #[test]
    fn dead_end_constraints_formatting() {
        let mut ctx = HandoffContext::default();
        ctx.add_dead_end(DeadEnd::new("exec", "cargo build", "compile error"));
        ctx.add_dead_end(DeadEnd::new("web_fetch", "fetch docs", "404 not found"));

        let constraints = ctx.dead_end_constraints().unwrap();
        assert!(constraints.contains("Dead-ends from prior session"));
        assert!(constraints.contains("**exec**"));
        assert!(constraints.contains("**web_fetch**"));
        assert!(constraints.contains("compile error"));
        assert!(constraints.contains("404 not found"));
    }

    #[test]
    fn dead_end_constraints_returns_none_when_empty() {
        let ctx = HandoffContext::default();
        assert!(ctx.dead_end_constraints().is_none());
    }

    #[test]
    fn error_truncation() {
        let long_error = "x".repeat(1000);
        let de = DeadEnd::new("tool", "desc", long_error);
        assert!(de.error.len() <= 512);
    }

    #[test]
    fn serde_skips_empty_optional_fields() {
        let ctx = HandoffContext::default();
        let json = serde_json::to_value(&ctx).unwrap();
        // Default context should only have empty/no fields.
        assert!(!json.as_object().unwrap().contains_key("last_action"));
        assert!(!json.as_object().unwrap().contains_key("dead_ends"));
    }
}
