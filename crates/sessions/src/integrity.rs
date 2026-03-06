//! Session integrity validation and recovery.
//!
//! Detects corrupted or interrupted sessions and provides recovery strategies:
//! - Truncated JSONL lines → remove them
//! - User messages without assistant responses → flag for re-trigger
//! - Tool calls without results → inject synthetic error results
//!
//! Designed to be called from the self-repair background task during
//! stuck-session recovery.

use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
};

use fd_lock::RwLock;
use serde::{Deserialize, Serialize};

use crate::{Result, message::PersistedMessage, store::SessionStore};

/// Types of integrity issues found in a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityIssue {
    /// A JSONL line could not be parsed (truncated or corrupted).
    TruncatedLine { line_number: usize },
    /// A user message at the given index has no following assistant response.
    UserWithoutResponse { message_index: usize },
    /// An assistant tool call has no matching tool/tool_result message.
    ToolCallWithoutResult {
        message_index: usize,
        tool_call_id: String,
    },
}

/// Recovery actions that were applied to fix integrity issues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    /// Removed N truncated/malformed lines from the session file.
    RemovedTruncatedLines { count: usize },
    /// Flagged the session for agent re-trigger (unanswered user message).
    FlaggedForRetrigger,
    /// Injected synthetic error tool results for orphaned tool calls.
    InjectedSyntheticToolResults { count: usize },
}

/// Result of a session integrity validation.
#[derive(Debug, Clone, Default)]
pub struct IntegrityReport {
    /// Issues detected during validation.
    pub issues: Vec<IntegrityIssue>,
    /// Recovery actions that were applied (populated after `repair`).
    pub actions: Vec<RecoveryAction>,
    /// Whether the session needs an agent re-trigger (user message without response).
    pub needs_retrigger: bool,
}

impl IntegrityReport {
    /// Whether any issues were found.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }

    /// Metric label for the dominant recovery type.
    #[must_use]
    pub fn recovery_type_label(&self) -> &'static str {
        if self
            .issues
            .iter()
            .any(|i| matches!(i, IntegrityIssue::TruncatedLine { .. }))
        {
            "truncated_line"
        } else if self
            .issues
            .iter()
            .any(|i| matches!(i, IntegrityIssue::ToolCallWithoutResult { .. }))
        {
            "orphaned_tool_call"
        } else if self
            .issues
            .iter()
            .any(|i| matches!(i, IntegrityIssue::UserWithoutResponse { .. }))
        {
            "unanswered_user"
        } else {
            "none"
        }
    }
}

impl SessionStore {
    /// Validate session integrity by scanning the JSONL file for issues.
    ///
    /// This performs a raw line scan (not using `read_typed`) so it can detect
    /// truncated/malformed lines that would be silently skipped.
    pub async fn validate_integrity_issues(&self, key: &str) -> Result<IntegrityReport> {
        let path = self.path_for(key);

        tokio::task::spawn_blocking(move || -> Result<IntegrityReport> {
            let mut report = IntegrityReport::default();

            if !path.exists() {
                return Ok(report);
            }

            let file = File::open(&path)?;
            let reader = BufReader::new(file);

            // Phase 1: parse all lines, tracking truncated ones.
            let mut messages: Vec<PersistedMessage> = Vec::new();
            let mut truncated_count = 0usize;

            for (line_idx, line) in reader.lines().enumerate() {
                let line = line?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<PersistedMessage>(trimmed) {
                    Ok(msg) => messages.push(msg),
                    Err(_) => {
                        report.issues.push(IntegrityIssue::TruncatedLine {
                            line_number: line_idx + 1,
                        });
                        truncated_count += 1;
                    },
                }
            }

            if truncated_count > 0 {
                report.actions.push(RecoveryAction::RemovedTruncatedLines {
                    count: truncated_count,
                });
            }

            // Phase 2: check for user messages without assistant responses.
            check_user_without_response(&messages, &mut report);

            // Phase 3: check for tool calls without results.
            check_tool_calls_without_results(&messages, &mut report);

            Ok(report)
        })
        .await?
    }

    /// Repair a session by applying fixes for detected integrity issues.
    ///
    /// - Removes truncated lines (by re-writing only valid messages)
    /// - Injects synthetic error tool results for orphaned tool calls
    ///
    /// Returns the updated integrity report with actions taken.
    pub async fn repair_session(&self, key: &str) -> Result<IntegrityReport> {
        let path = self.path_for(key);

        tokio::task::spawn_blocking(move || -> Result<IntegrityReport> {
            let mut report = IntegrityReport::default();
            if !path.exists() {
                return Ok(report);
            }

            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let mut lock = RwLock::new(file);
            let mut guard = lock
                .write()
                .map_err(|e| crate::Error::lock_failed(e.to_string()))?;

            // Parse all lines while holding the same write lock that append/replace use.
            let reader_file = guard.try_clone()?;
            let reader = BufReader::new(reader_file);
            let mut messages: Vec<PersistedMessage> = Vec::new();
            let mut truncated_count = 0usize;

            for (line_idx, line) in reader.lines().enumerate() {
                let line = line?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<PersistedMessage>(trimmed) {
                    Ok(msg) => messages.push(msg),
                    Err(_) => {
                        report.issues.push(IntegrityIssue::TruncatedLine {
                            line_number: line_idx + 1,
                        });
                        truncated_count += 1;
                    },
                }
            }

            if truncated_count > 0 {
                report.actions.push(RecoveryAction::RemovedTruncatedLines {
                    count: truncated_count,
                });
            }

            check_user_without_response(&messages, &mut report);
            check_tool_calls_without_results(&messages, &mut report);

            if report.is_clean() {
                return Ok(report);
            }

            let orphaned_ids: Vec<String> = report
                .issues
                .iter()
                .filter_map(|i| match i {
                    IntegrityIssue::ToolCallWithoutResult { tool_call_id, .. } => {
                        Some(tool_call_id.clone())
                    },
                    _ => None,
                })
                .collect();

            if !orphaned_ids.is_empty() {
                let synthetic_count = orphaned_ids.len();
                for tool_call_id in orphaned_ids {
                    messages.push(PersistedMessage::tool_result(
                        &tool_call_id,
                        "unknown",
                        None,
                        false,
                        None,
                        Some("session interrupted — synthetic recovery result".to_string()),
                    ));
                }
                report
                    .actions
                    .push(RecoveryAction::InjectedSyntheticToolResults {
                        count: synthetic_count,
                    });
            }

            // Atomic rewrite under the same lock used for parsing.
            guard.set_len(0)?;
            guard.seek(SeekFrom::Start(0))?;
            for msg in &messages {
                let line = serde_json::to_string(msg)?;
                writeln!(*guard, "{line}")?;
            }
            guard.sync_data()?;

            Ok(report)
        })
        .await?
    }
}

/// Check for user messages that are not followed by an assistant response.
///
/// Only flags the *last* user message if it has no response, since mid-conversation
/// user messages without responses may be valid (e.g., system injections, notices
/// between them).
fn check_user_without_response(messages: &[PersistedMessage], report: &mut IntegrityReport) {
    // Find the last user message index.
    let last_user_idx = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| matches!(m, PersistedMessage::User { .. }));

    if let Some((idx, _)) = last_user_idx {
        // Check if any assistant message follows it.
        let has_response = messages[idx + 1..]
            .iter()
            .any(|m| matches!(m, PersistedMessage::Assistant { .. }));

        if !has_response {
            report
                .issues
                .push(IntegrityIssue::UserWithoutResponse { message_index: idx });
            report.needs_retrigger = true;
            report.actions.push(RecoveryAction::FlaggedForRetrigger);
        }
    }
}

/// Check for assistant tool calls that have no matching tool/tool_result message.
fn check_tool_calls_without_results(messages: &[PersistedMessage], report: &mut IntegrityReport) {
    // Collect all tool result IDs.
    let result_ids: HashSet<&str> = messages
        .iter()
        .filter_map(|m| match m {
            PersistedMessage::Tool { tool_call_id, .. }
            | PersistedMessage::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();

    // Find tool calls without matching results.
    for (idx, msg) in messages.iter().enumerate() {
        if let PersistedMessage::Assistant {
            tool_calls: Some(calls),
            ..
        } = msg
        {
            for call in calls {
                if !result_ids.contains(call.id.as_str()) {
                    report.issues.push(IntegrityIssue::ToolCallWithoutResult {
                        message_index: idx,
                        tool_call_id: call.id.clone(),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::Write;

    use {super::*, crate::message::*, serde_json::json};

    fn temp_store() -> (SessionStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        (store, dir)
    }

    #[tokio::test]
    async fn clean_session_reports_no_issues() {
        let (store, _dir) = temp_store();

        store
            .append_typed("s1", &PersistedMessage::user("hello"))
            .await
            .unwrap();
        store
            .append_typed(
                "s1",
                &PersistedMessage::assistant("hi there", "gpt-4o", "openai", 10, 5, None),
            )
            .await
            .unwrap();

        let report = store.validate_integrity_issues("s1").await.unwrap();
        assert!(report.is_clean());
        assert!(!report.needs_retrigger);
    }

    #[tokio::test]
    async fn empty_session_is_clean() {
        let (store, _dir) = temp_store();
        let report = store
            .validate_integrity_issues("nonexistent")
            .await
            .unwrap();
        assert!(report.is_clean());
    }

    #[tokio::test]
    async fn detects_truncated_lines() {
        let (store, dir) = temp_store();

        // Write a valid message then a truncated line.
        store
            .append_typed("s1", &PersistedMessage::user("hello"))
            .await
            .unwrap();

        // Manually append a truncated line.
        let path = dir.path().join("s1.jsonl");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"role\": \"assistant\", \"content\": \"trun\n")
            .unwrap();

        let report = store.validate_integrity_issues("s1").await.unwrap();
        assert!(!report.is_clean());
        assert_eq!(
            report
                .issues
                .iter()
                .filter(|i| matches!(i, IntegrityIssue::TruncatedLine { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn detects_user_without_response() {
        let (store, _dir) = temp_store();

        store
            .append_typed("s1", &PersistedMessage::user("hello"))
            .await
            .unwrap();

        let report = store.validate_integrity_issues("s1").await.unwrap();
        assert!(report.needs_retrigger);
        assert!(
            report
                .issues
                .iter()
                .any(|i| matches!(i, IntegrityIssue::UserWithoutResponse { message_index: 0 }))
        );
    }

    #[tokio::test]
    async fn user_with_response_is_clean() {
        let (store, _dir) = temp_store();

        store
            .append_typed("s1", &PersistedMessage::user("hello"))
            .await
            .unwrap();
        store
            .append_typed(
                "s1",
                &PersistedMessage::assistant("world", "gpt-4o", "openai", 10, 5, None),
            )
            .await
            .unwrap();

        let report = store.validate_integrity_issues("s1").await.unwrap();
        assert!(!report.needs_retrigger);
    }

    #[tokio::test]
    async fn detects_tool_call_without_result() {
        let (store, _dir) = temp_store();

        // Assistant message with a tool call but no matching tool result.
        let assistant_with_tool = PersistedMessage::Assistant {
            content: String::new(),
            created_at: Some(1000),
            model: Some("gpt-4o".to_string()),
            provider: Some("openai".to_string()),
            input_tokens: Some(10),
            output_tokens: Some(5),
            duration_ms: None,
            request_input_tokens: None,
            request_output_tokens: None,
            tool_calls: Some(vec![PersistedToolCall {
                id: "call_orphan".to_string(),
                call_type: "function".to_string(),
                function: PersistedFunction {
                    name: "exec".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                },
            }]),
            reasoning: None,
            llm_api_response: None,
            audio: None,
            seq: None,
            run_id: None,
        };

        store
            .append_typed("s1", &PersistedMessage::user("run ls"))
            .await
            .unwrap();
        store
            .append_typed("s1", &assistant_with_tool)
            .await
            .unwrap();

        let report = store.validate_integrity_issues("s1").await.unwrap();
        assert!(report.issues.iter().any(|i| matches!(
            i,
            IntegrityIssue::ToolCallWithoutResult { tool_call_id, .. } if tool_call_id == "call_orphan"
        )));
    }

    #[tokio::test]
    async fn tool_call_with_result_is_clean() {
        let (store, _dir) = temp_store();

        let assistant_with_tool = PersistedMessage::Assistant {
            content: String::new(),
            created_at: Some(1000),
            model: Some("gpt-4o".to_string()),
            provider: Some("openai".to_string()),
            input_tokens: Some(10),
            output_tokens: Some(5),
            duration_ms: None,
            request_input_tokens: None,
            request_output_tokens: None,
            tool_calls: Some(vec![PersistedToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: PersistedFunction {
                    name: "exec".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                },
            }]),
            reasoning: None,
            llm_api_response: None,
            audio: None,
            seq: None,
            run_id: None,
        };

        store
            .append_typed("s1", &PersistedMessage::user("run ls"))
            .await
            .unwrap();
        store
            .append_typed("s1", &assistant_with_tool)
            .await
            .unwrap();
        store
            .append_typed(
                "s1",
                &PersistedMessage::tool_result(
                    "call_1",
                    "exec",
                    None,
                    true,
                    Some(json!({"stdout": "file.txt"})),
                    None,
                ),
            )
            .await
            .unwrap();
        store
            .append_typed(
                "s1",
                &PersistedMessage::assistant("found file.txt", "gpt-4o", "openai", 20, 10, None),
            )
            .await
            .unwrap();

        let report = store.validate_integrity_issues("s1").await.unwrap();
        assert!(report.is_clean());
    }

    #[tokio::test]
    async fn repair_removes_truncated_and_injects_results() {
        let (store, dir) = temp_store();

        // Build a session with: user, assistant(tool_call), truncated line.
        store
            .append_typed("s1", &PersistedMessage::user("run something"))
            .await
            .unwrap();

        let assistant_with_tool = PersistedMessage::Assistant {
            content: String::new(),
            created_at: Some(1000),
            model: Some("gpt-4o".to_string()),
            provider: Some("openai".to_string()),
            input_tokens: Some(10),
            output_tokens: Some(5),
            duration_ms: None,
            request_input_tokens: None,
            request_output_tokens: None,
            tool_calls: Some(vec![PersistedToolCall {
                id: "call_repair".to_string(),
                call_type: "function".to_string(),
                function: PersistedFunction {
                    name: "exec".to_string(),
                    arguments: r#"{"command":"echo hi"}"#.to_string(),
                },
            }]),
            reasoning: None,
            llm_api_response: None,
            audio: None,
            seq: None,
            run_id: None,
        };
        store
            .append_typed("s1", &assistant_with_tool)
            .await
            .unwrap();

        // Append a truncated line.
        use std::io::Write;
        let path = dir.path().join("s1.jsonl");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"role\": \"tool_result\", \"incomplete\n")
            .unwrap();

        // Repair.
        let report = store.repair_session("s1").await.unwrap();

        // Should have removed truncated lines.
        assert!(
            report
                .actions
                .iter()
                .any(|a| matches!(a, RecoveryAction::RemovedTruncatedLines { count: 1 }))
        );

        // Should have injected a synthetic tool result.
        assert!(
            report
                .actions
                .iter()
                .any(|a| matches!(a, RecoveryAction::InjectedSyntheticToolResults { count: 1 }))
        );

        // Verify the session is now valid.
        let messages = store.read_typed("s1").await.unwrap();
        // user + assistant + synthetic tool_result = 3
        assert_eq!(messages.len(), 3);

        // The synthetic result should reference the orphaned tool call.
        match &messages[2] {
            PersistedMessage::ToolResult {
                tool_call_id,
                success,
                error,
                ..
            } => {
                assert_eq!(tool_call_id, "call_repair");
                assert!(!success);
                assert!(error.as_deref().unwrap_or("").contains("recovery"));
            },
            _ => panic!("expected synthetic ToolResult"),
        }
    }

    #[tokio::test]
    async fn repair_noop_on_clean_session() {
        let (store, _dir) = temp_store();

        store
            .append_typed("s1", &PersistedMessage::user("hello"))
            .await
            .unwrap();
        store
            .append_typed(
                "s1",
                &PersistedMessage::assistant("hi", "gpt-4o", "openai", 10, 5, None),
            )
            .await
            .unwrap();

        let report = store.repair_session("s1").await.unwrap();
        assert!(report.is_clean());
        assert!(report.actions.is_empty());
    }

    #[tokio::test]
    async fn recovery_type_label_reflects_issues() {
        let mut report = IntegrityReport::default();
        assert_eq!(report.recovery_type_label(), "none");

        report
            .issues
            .push(IntegrityIssue::TruncatedLine { line_number: 1 });
        assert_eq!(report.recovery_type_label(), "truncated_line");

        let mut report2 = IntegrityReport::default();
        report2.issues.push(IntegrityIssue::ToolCallWithoutResult {
            message_index: 0,
            tool_call_id: "c1".to_string(),
        });
        assert_eq!(report2.recovery_type_label(), "orphaned_tool_call");

        let mut report3 = IntegrityReport::default();
        report3
            .issues
            .push(IntegrityIssue::UserWithoutResponse { message_index: 0 });
        assert_eq!(report3.recovery_type_label(), "unanswered_user");
    }
}
