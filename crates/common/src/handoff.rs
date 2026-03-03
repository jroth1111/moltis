use sha2::{Digest, Sha256};

pub const HANDOFF_NAMESPACE: &str = "handoff";
pub const HANDOFF_LATEST_KEY: &str = "latest";
pub const HANDOFF_INBOUND_KEY: &str = "inbound";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffDeadEnd {
    pub tool_name: String,
    pub error_hash: String,
    pub summary: String,
}

impl HandoffDeadEnd {
    #[must_use]
    pub fn fingerprint(tool_name: &str, error: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(tool_name.as_bytes());
        hasher.update(b":");
        hasher.update(error.as_bytes());
        let digest = hasher.finalize();
        hex_lower(&digest[..8])
    }

    #[must_use]
    pub fn from_error(tool_name: &str, error: &str, summary: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            error_hash: Self::fingerprint(tool_name, error),
            summary: summary.into(),
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HandoffContext {
    pub last_action: Option<String>,
    pub observed_error: Option<String>,
    pub dead_ends: Vec<HandoffDeadEnd>,
    pub suggested_next_step: Option<String>,
    pub estimated_tokens: Option<u32>,
}

impl HandoffContext {
    pub fn add_dead_end(&mut self, tool_name: &str, error: &str, summary: impl Into<String>) {
        let dead_end = HandoffDeadEnd::from_error(tool_name, error, summary);
        let exists = self.dead_ends.iter().any(|existing| {
            existing.tool_name == dead_end.tool_name && existing.error_hash == dead_end.error_hash
        });
        if exists {
            return;
        }
        if self.dead_ends.len() >= 50 {
            self.dead_ends.remove(0);
        }
        self.dead_ends.push(dead_end);
    }

    #[must_use]
    pub fn to_message_block(&self) -> String {
        let mut block = String::from("[HandoffContext]\n");
        if let Some(last_action) = &self.last_action {
            block.push_str(&format!("last_action: {last_action}\n"));
        }
        if let Some(error) = &self.observed_error {
            block.push_str(&format!("observed_error: {error}\n"));
        }
        if !self.dead_ends.is_empty() {
            block.push_str("do_not_retry:\n");
            for dead_end in &self.dead_ends {
                block.push_str(&format!(
                    "- {} [{}]: {}\n",
                    dead_end.tool_name, dead_end.error_hash, dead_end.summary
                ));
            }
        }
        if let Some(next_step) = &self.suggested_next_step {
            block.push_str(&format!("suggested_next_step: {next_step}\n"));
        }
        if let Some(tokens) = self.estimated_tokens {
            block.push_str(&format!("estimated_tokens: {tokens}\n"));
        }
        block.push_str("[/HandoffContext]");
        block
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(nibble_to_hex(byte >> 4));
        out.push(nibble_to_hex(byte & 0x0F));
    }
    out
}

fn nibble_to_hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'a' + (nibble - 10)) as char,
        _ => '0',
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deduplicates_dead_ends_by_tool_and_fingerprint() {
        let mut ctx = HandoffContext::default();
        ctx.add_dead_end("exec", "permission denied", "sandbox blocked command");
        ctx.add_dead_end("exec", "permission denied", "sandbox blocked command");
        assert_eq!(ctx.dead_ends.len(), 1);
    }

    #[test]
    fn caps_dead_end_history_at_fifty() {
        let mut ctx = HandoffContext::default();
        for idx in 0..60 {
            ctx.add_dead_end("tool", &format!("error-{idx}"), format!("summary-{idx}"));
        }
        assert_eq!(ctx.dead_ends.len(), 50);
    }

    #[test]
    fn renders_do_not_retry_block() {
        let mut ctx = HandoffContext {
            last_action: Some("attempted codegen".into()),
            observed_error: Some("provider timeout".into()),
            dead_ends: Vec::new(),
            suggested_next_step: Some("switch provider".into()),
            estimated_tokens: Some(1200),
        };
        ctx.add_dead_end("exec", "sandbox timeout", "long-running command timed out");

        let block = ctx.to_message_block();
        assert!(block.contains("do_not_retry"));
        assert!(block.contains("long-running command timed out"));
        assert!(block.contains("switch provider"));
    }
}
