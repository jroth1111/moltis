use std::path::PathBuf;

use serde_json::Value;

use crate::Result;

/// Strategy for compacting a session when context window fills up.
#[derive(Debug, Clone)]
pub enum CompactionStrategy {
    /// Use an LLM to summarize old messages into a compact narrative.
    Summarize {
        /// How many recent messages to keep verbatim after the summary.
        keep_recent: usize,
    },
    /// Drop the oldest messages, keeping only the N most recent.
    Truncate {
        /// Number of messages to keep.
        keep_recent: usize,
    },
    /// Archive the full history to a daily log and keep only recent messages.
    MoveToWorkspace {
        /// Number of recent messages to keep.
        keep_recent: usize,
    },
}

/// Result of a compaction operation.
pub struct CompactionResult {
    pub messages: Vec<Value>,
    pub strategy_used: String,
    pub messages_removed: usize,
}

/// Compact a session using the given strategy.
///
/// `session_key` is used for naming archive files.
/// `base_dir` is the sessions base directory (for MoveToWorkspace).
pub async fn compact_session(
    messages: &[Value],
    strategy: CompactionStrategy,
    session_key: &str,
    base_dir: Option<PathBuf>,
) -> Result<CompactionResult> {
    match strategy {
        CompactionStrategy::Truncate { keep_recent } => truncate_strategy(messages, keep_recent),
        CompactionStrategy::MoveToWorkspace { keep_recent } => {
            move_to_workspace_strategy(messages, keep_recent, session_key, base_dir).await
        },
        CompactionStrategy::Summarize { keep_recent } => {
            // For Summarize: use truncate as a safe fallback when no provider is available.
            // Full LLM-based summarization requires an LlmProvider, which is orchestrated
            // at a higher level (see chat/src/lib.rs which calls silent_turn.rs directly).
            truncate_strategy(messages, keep_recent)
        },
    }
}

fn truncate_strategy(messages: &[Value], keep_recent: usize) -> Result<CompactionResult> {
    if messages.len() <= keep_recent {
        return Ok(CompactionResult {
            messages: messages.to_vec(),
            strategy_used: "truncate".to_string(),
            messages_removed: 0,
        });
    }
    let removed = messages.len() - keep_recent;
    let kept = messages[messages.len() - keep_recent..].to_vec();
    Ok(CompactionResult {
        messages: kept,
        strategy_used: "truncate".to_string(),
        messages_removed: removed,
    })
}

async fn move_to_workspace_strategy(
    messages: &[Value],
    keep_recent: usize,
    session_key: &str,
    base_dir: Option<PathBuf>,
) -> Result<CompactionResult> {
    let Some(base_dir) = base_dir else {
        // No base directory available — fall back to truncate.
        return truncate_strategy(messages, keep_recent);
    };

    if messages.len() <= keep_recent {
        return Ok(CompactionResult {
            messages: messages.to_vec(),
            strategy_used: "move_to_workspace".to_string(),
            messages_removed: 0,
        });
    }

    let removed = messages.len() - keep_recent;
    let old_messages = &messages[..removed];

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let safe_key = session_key.replace(':', "_");
    let archive_filename = format!("{safe_key}.{ts}.jsonl");
    let archive_dir = base_dir.join("archive");
    std::fs::create_dir_all(&archive_dir)?;

    let archive_path = archive_dir.join(&archive_filename);
    let mut file = std::fs::File::create(&archive_path)?;
    for msg in old_messages {
        let line = serde_json::to_string(msg)?;
        std::io::Write::write_all(&mut file, line.as_bytes())?;
        std::io::Write::write_all(&mut file, b"\n")?;
    }

    let notice = serde_json::json!({
        "role": "system",
        "content": format!("[Archived {} messages to archive/{}]", removed, archive_filename),
    });

    let mut kept = vec![notice];
    kept.extend_from_slice(&messages[messages.len() - keep_recent..]);

    Ok(CompactionResult {
        messages: kept,
        strategy_used: "move_to_workspace".to_string(),
        messages_removed: removed,
    })
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn truncate_keeps_last_n() {
        let messages: Vec<Value> = (0..10)
            .map(|i| json!({"role": "user", "content": format!("msg-{i}")}))
            .collect();
        let result = compact_session(
            &messages,
            CompactionStrategy::Truncate { keep_recent: 3 },
            "test",
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.messages_removed, 7);
        assert_eq!(result.strategy_used, "truncate");
        assert_eq!(result.messages[0]["content"], "msg-7");
        assert_eq!(result.messages[2]["content"], "msg-9");
    }

    #[tokio::test]
    async fn truncate_noop_when_short() {
        let messages: Vec<Value> = (0..3)
            .map(|i| json!({"role": "user", "content": format!("msg-{i}")}))
            .collect();
        let result = compact_session(
            &messages,
            CompactionStrategy::Truncate { keep_recent: 10 },
            "test",
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.messages_removed, 0);
        assert_eq!(result.strategy_used, "truncate");
    }

    #[tokio::test]
    async fn move_to_workspace_creates_archive() {
        let dir = tempfile::tempdir().unwrap();
        let messages: Vec<Value> = (0..10)
            .map(|i| json!({"role": "user", "content": format!("msg-{i}")}))
            .collect();
        let result = compact_session(
            &messages,
            CompactionStrategy::MoveToWorkspace { keep_recent: 3 },
            "test:session",
            Some(dir.path().to_path_buf()),
        )
        .await
        .unwrap();

        // Should have system notice + 3 kept messages = 4 total
        assert_eq!(result.messages.len(), 4);
        assert_eq!(result.messages_removed, 7);
        assert_eq!(result.strategy_used, "move_to_workspace");
        assert_eq!(result.messages[0]["role"], "system");
        assert!(
            result.messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("Archived 7 messages")
        );
        assert_eq!(result.messages[1]["content"], "msg-7");

        // Archive file must exist
        let archive_dir = dir.path().join("archive");
        assert!(archive_dir.exists());
        let entries: Vec<_> = std::fs::read_dir(&archive_dir).unwrap().collect();
        assert_eq!(entries.len(), 1);

        // Archive file should contain 7 lines (the old messages)
        let archive_path = entries[0].as_ref().unwrap().path();
        let content = std::fs::read_to_string(&archive_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 7);
    }

    #[tokio::test]
    async fn move_to_workspace_falls_back_to_truncate_without_base_dir() {
        let messages: Vec<Value> = (0..10)
            .map(|i| json!({"role": "user", "content": format!("msg-{i}")}))
            .collect();
        let result = compact_session(
            &messages,
            CompactionStrategy::MoveToWorkspace { keep_recent: 3 },
            "test",
            None,
        )
        .await
        .unwrap();
        // Falls back to truncate when no base_dir
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.messages_removed, 7);
        assert_eq!(result.strategy_used, "truncate");
    }
}
