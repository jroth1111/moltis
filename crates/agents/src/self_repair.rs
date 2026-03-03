//! Agent self-repair: detect and recover stuck agent runs.
//!
//! A background task scans active session state periodically. When a session
//! has been in an "in-progress" state for more than the configured threshold
//! without any activity update, it is considered stuck and recovery is attempted.
//!
//! State is tracked in the `SessionStateStore` under the `"self_repair"` namespace:
//! - `running_since` — epoch ms when the current run started
//! - `repair_attempts` — number of recovery attempts already made
//!
//! Recovery resets `running_since` to nil, allowing the next heartbeat or user
//! message to re-trigger the agent. After `max_repair_attempts`, the session is
//! marked failed and the caller is notified.
//!
//! When a `SessionStore` is provided, integrity validation is run on recovered
//! sessions: truncated lines are removed, orphaned tool calls get synthetic
//! results, and unanswered user messages are flagged for re-trigger.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use {
    anyhow::Result,
    tracing::{info, warn},
};

use moltis_sessions::{
    integrity::IntegrityReport, state_store::SessionStateStore, store::SessionStore,
};

/// Default threshold before a session is considered stuck.
pub const DEFAULT_STUCK_THRESHOLD: Duration = Duration::from_secs(10 * 60); // 10 minutes

/// Default number of repair attempts before giving up.
pub const DEFAULT_MAX_REPAIR_ATTEMPTS: u32 = 3;

/// Namespace used in `SessionStateStore` for self-repair tracking.
const REPAIR_NAMESPACE: &str = "self_repair";

/// Whether a session is currently marked as running.
pub async fn is_session_running(store: &SessionStateStore, session_key: &str) -> bool {
    store
        .get(session_key, REPAIR_NAMESPACE, "running_since")
        .await
        .ok()
        .flatten()
        .is_some()
}

/// Mark a session as started (begin tracking it for stuck detection).
pub async fn mark_session_started(store: &SessionStateStore, session_key: &str) -> Result<()> {
    let now = now_ms().to_string();
    store
        .set(session_key, REPAIR_NAMESPACE, "running_since", &now)
        .await?;
    Ok(())
}

/// Mark a session as finished (clear running state).
pub async fn mark_session_finished(store: &SessionStateStore, session_key: &str) -> Result<()> {
    store
        .delete(session_key, REPAIR_NAMESPACE, "running_since")
        .await?;
    Ok(())
}

/// A session that has been detected as stuck.
#[derive(Debug, Clone)]
pub struct StuckSession {
    pub session_key: String,
    pub running_since_ms: u64,
    pub elapsed: Duration,
    pub repair_attempts: u32,
}

/// Integrity repair result for a single session.
#[derive(Debug, Clone)]
pub struct SessionIntegrityRepair {
    pub session_key: String,
    pub report: IntegrityReport,
}

/// Result of a repair scan.
#[derive(Debug, Default)]
pub struct RepairScanResult {
    /// Sessions that were found stuck and had recovery attempted.
    pub recovered: Vec<String>,
    /// Sessions that exceeded max repair attempts and were marked failed.
    pub failed: Vec<String>,
    /// Sessions that had integrity issues repaired.
    pub integrity_repaired: Vec<SessionIntegrityRepair>,
}

/// Scan all sessions for stuck runs and attempt recovery.
///
/// When `session_store` is provided, integrity validation is run on each
/// recovered session before clearing `running_since`. This detects truncated
/// lines, orphaned tool calls, and unanswered user messages.
///
/// Returns a summary of what was found and acted on.
pub async fn scan_and_repair(
    store: &SessionStateStore,
    session_keys: &[String],
    stuck_threshold: Duration,
    max_repair_attempts: u32,
    session_store: Option<&SessionStore>,
) -> Result<RepairScanResult> {
    let now = now_ms();
    let threshold_ms = stuck_threshold.as_millis() as u64;
    let mut result = RepairScanResult::default();

    for key in session_keys {
        let Some(running_since_str) = store
            .get(key, REPAIR_NAMESPACE, "running_since")
            .await
            .unwrap_or(None)
        else {
            continue; // Not running — skip
        };

        let Ok(running_since_ms) = running_since_str.parse::<u64>() else {
            warn!(session = %key, "could not parse running_since timestamp, clearing");
            let _ = store.delete(key, REPAIR_NAMESPACE, "running_since").await;
            continue;
        };

        let elapsed_ms = now.saturating_sub(running_since_ms);
        if elapsed_ms < threshold_ms {
            continue; // Not stuck yet
        }

        let elapsed = Duration::from_millis(elapsed_ms);
        let attempts: u32 = store
            .get(key, REPAIR_NAMESPACE, "repair_attempts")
            .await
            .unwrap_or(None)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        if attempts >= max_repair_attempts {
            warn!(
                session = %key,
                elapsed_secs = elapsed.as_secs(),
                attempts,
                "session stuck and exceeded max repair attempts — marking failed"
            );
            // Clear running state so it doesn't keep alerting.
            let _ = store.delete(key, REPAIR_NAMESPACE, "running_since").await;
            let _ = store.set(key, REPAIR_NAMESPACE, "failed", "true").await;
            result.failed.push(key.clone());
        } else {
            info!(
                session = %key,
                elapsed_secs = elapsed.as_secs(),
                attempts,
                "stuck session detected — attempting recovery"
            );

            // Run integrity validation and repair if session_store is available.
            if let Some(ss) = session_store {
                match ss.repair_session(key).await {
                    Ok(report) => {
                        if !report.is_clean() {
                            let recovery_type = report.recovery_type_label();
                            info!(
                                session = %key,
                                recovery_type,
                                issues = report.issues.len(),
                                "session integrity repair completed"
                            );

                            #[cfg(feature = "metrics")]
                            {
                                use moltis_metrics::counter;
                                counter!(
                                    moltis_metrics::session::RECOVERY_TOTAL,
                                    "recovery_type" => recovery_type.to_string()
                                )
                                .increment(1);
                            }

                            result.integrity_repaired.push(SessionIntegrityRepair {
                                session_key: key.clone(),
                                report,
                            });
                        }
                    },
                    Err(e) => {
                        warn!(
                            session = %key,
                            error = %e,
                            "session integrity repair failed"
                        );
                    },
                }
            }

            // Recovery: clear running_since (allows next message/heartbeat to restart).
            let _ = store.delete(key, REPAIR_NAMESPACE, "running_since").await;
            let next_attempts = (attempts + 1).to_string();
            let _ = store
                .set(key, REPAIR_NAMESPACE, "repair_attempts", &next_attempts)
                .await;
            result.recovered.push(key.clone());
        }
    }

    Ok(result)
}

/// Check if a session has been permanently failed by self-repair.
pub async fn is_session_failed(store: &SessionStateStore, session_key: &str) -> bool {
    store
        .get(session_key, REPAIR_NAMESPACE, "failed")
        .await
        .ok()
        .flatten()
        .is_some_and(|v| v == "true")
}

/// Information about stuck sessions for health reporting.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StuckSessionInfo {
    pub session_key: String,
    pub running_since_ms: u64,
    pub elapsed_secs: u64,
    pub repair_attempts: u32,
}

/// Get a list of currently stuck sessions (for health endpoint reporting).
pub async fn get_stuck_sessions(
    store: &SessionStateStore,
    session_keys: &[String],
    stuck_threshold: Duration,
) -> Vec<StuckSessionInfo> {
    let now = now_ms();
    let threshold_ms = stuck_threshold.as_millis() as u64;
    let mut stuck = Vec::new();

    for key in session_keys {
        let Some(running_since_str) = store
            .get(key, REPAIR_NAMESPACE, "running_since")
            .await
            .unwrap_or(None)
        else {
            continue;
        };
        let Ok(running_since_ms) = running_since_str.parse::<u64>() else {
            continue;
        };
        let elapsed_ms = now.saturating_sub(running_since_ms);
        if elapsed_ms < threshold_ms {
            continue;
        }
        let repair_attempts = store
            .get(key, REPAIR_NAMESPACE, "repair_attempts")
            .await
            .unwrap_or(None)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        stuck.push(StuckSessionInfo {
            session_key: key.clone(),
            running_since_ms,
            elapsed_secs: elapsed_ms / 1000,
            repair_attempts,
        });
    }

    stuck
}

/// Start the self-repair background task.
///
/// Scans for stuck sessions every `scan_interval`, using the provided
/// `session_keys_fn` to get the current list of active session keys.
/// When `session_store` is provided, integrity validation is run on
/// recovered sessions.
pub fn start_background_task(
    store: Arc<SessionStateStore>,
    scan_interval: Duration,
    stuck_threshold: Duration,
    max_repair_attempts: u32,
    session_store: Option<Arc<SessionStore>>,
    // Callback invoked when sessions are repaired or fail: (recovered, failed)
    on_scan: Option<Arc<dyn Fn(RepairScanResult) + Send + Sync>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(scan_interval).await;

            // Get all session keys from the state store.
            // In a full integration the store would expose a method to list active sessions.
            // For now we scan the store for any session that has running_since set.
            // This is a simplified implementation — in production, integrate with
            // the session metadata store to get all known session keys.
            let keys = store.list_running_sessions().await.unwrap_or_default();

            if keys.is_empty() {
                continue;
            }

            let ss_ref = session_store.as_deref();
            match scan_and_repair(&store, &keys, stuck_threshold, max_repair_attempts, ss_ref)
                .await
            {
                Ok(result) => {
                    if !result.recovered.is_empty()
                        || !result.failed.is_empty()
                        || !result.integrity_repaired.is_empty()
                    {
                        info!(
                            recovered = result.recovered.len(),
                            failed = result.failed.len(),
                            integrity_repaired = result.integrity_repaired.len(),
                            "self-repair scan complete"
                        );
                        if let Some(ref cb) = on_scan {
                            cb(result);
                        }
                    }
                },
                Err(e) => {
                    warn!(error = %e, "self-repair scan failed");
                },
            }
        }
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
<<<<<<< HEAD
    use std::io::Write;

    use moltis_sessions::message::{PersistedFunction, PersistedMessage, PersistedToolCall};

    use super::*;
=======
    use {super::*, std::time::Duration};
>>>>>>> fix/audit-hardening-phase1

    async fn make_store() -> (SessionStateStore, sqlx::SqlitePool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect(&format!("sqlite://{}?mode=rwc", db_path.display()))
            .await
            .unwrap();
        // Create the session_state table inline (avoids depending on a #[cfg(test)]
        // helper defined in moltis-sessions, which is not visible across crate boundaries).
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS session_state (
                session_key TEXT NOT NULL,
                namespace   TEXT NOT NULL,
                key         TEXT NOT NULL,
                value       TEXT NOT NULL,
                updated_at  INTEGER NOT NULL,
                PRIMARY KEY (session_key, namespace, key)
            )"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let store = SessionStateStore::new(pool.clone());
        (store, pool, dir)
    }

    #[tokio::test]
    async fn mark_and_check_running() {
        let (store, _, _dir) = make_store().await;
        assert!(!is_session_running(&store, "s1").await);
        mark_session_started(&store, "s1").await.unwrap();
        assert!(is_session_running(&store, "s1").await);
        mark_session_finished(&store, "s1").await.unwrap();
        assert!(!is_session_running(&store, "s1").await);
    }

    #[tokio::test]
    async fn scan_ignores_fresh_sessions() {
        let (store, _, _dir) = make_store().await;
        mark_session_started(&store, "s1").await.unwrap();
        // Use a 10-minute threshold — freshly started session should not be stuck.
        let result =
            scan_and_repair(&store, &["s1".to_string()], DEFAULT_STUCK_THRESHOLD, 3, None)
                .await
                .unwrap();
        assert!(result.recovered.is_empty());
        assert!(result.failed.is_empty());
    }

    #[tokio::test]
    async fn scan_detects_old_session() {
        let (store, _, _dir) = make_store().await;
        // Manually write a very old running_since timestamp.
        store
            .set("s1", REPAIR_NAMESPACE, "running_since", "1000") // epoch 1s — ancient
            .await
            .unwrap();

        let result =
            scan_and_repair(&store, &["s1".to_string()], Duration::from_secs(60), 3, None)
                .await
                .unwrap();
        assert_eq!(result.recovered, vec!["s1".to_string()]);
        // Should have cleared running_since
        assert!(!is_session_running(&store, "s1").await);
    }

    #[tokio::test]
    async fn max_attempts_marks_failed() {
        let (store, _, _dir) = make_store().await;
        store
            .set("s1", REPAIR_NAMESPACE, "running_since", "1000")
            .await
            .unwrap();
        store
            .set("s1", REPAIR_NAMESPACE, "repair_attempts", "3")
            .await
            .unwrap();

        let result =
            scan_and_repair(&store, &["s1".to_string()], Duration::from_secs(60), 3, None)
                .await
                .unwrap();
        assert!(result.recovered.is_empty());
        assert_eq!(result.failed, vec!["s1".to_string()]);
        assert!(is_session_failed(&store, "s1").await);
    }

    #[tokio::test]
    async fn scan_repairs_integrity_with_session_store() {
        let (state_store, _, _dir) = make_store().await;
        let session_dir = tempfile::tempdir().unwrap();
        let session_store = SessionStore::new(session_dir.path().to_path_buf());

        // Mark session as stuck (ancient timestamp).
        state_store
            .set("s1", REPAIR_NAMESPACE, "running_since", "1000")
            .await
            .unwrap();

        // Create a session with an orphaned tool call.
        session_store
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
        session_store
            .append_typed("s1", &assistant_with_tool)
            .await
            .unwrap();

        let result = scan_and_repair(
            &state_store,
            &["s1".to_string()],
            Duration::from_secs(60),
            3,
            Some(&session_store),
        )
        .await
        .unwrap();

        // Session should be recovered.
        assert_eq!(result.recovered, vec!["s1".to_string()]);
        // Integrity should have been repaired.
        assert_eq!(result.integrity_repaired.len(), 1);
        assert!(!result.integrity_repaired[0].report.is_clean());

        // The session should now have a synthetic tool result.
        let messages = session_store.read_typed("s1").await.unwrap();
        assert_eq!(messages.len(), 3); // user + assistant + synthetic tool_result
    }

    #[tokio::test]
    async fn scan_with_session_store_clean_session() {
        let (state_store, _, _dir) = make_store().await;
        let session_dir = tempfile::tempdir().unwrap();
        let session_store = SessionStore::new(session_dir.path().to_path_buf());

        // Mark session as stuck.
        state_store
            .set("s1", REPAIR_NAMESPACE, "running_since", "1000")
            .await
            .unwrap();

        // Create a clean session.
        session_store
            .append_typed("s1", &PersistedMessage::user("hello"))
            .await
            .unwrap();
        session_store
            .append_typed(
                "s1",
                &PersistedMessage::assistant("hi", "gpt-4o", "openai", 10, 5, None),
            )
            .await
            .unwrap();

        let result = scan_and_repair(
            &state_store,
            &["s1".to_string()],
            Duration::from_secs(60),
            3,
            Some(&session_store),
        )
        .await
        .unwrap();

        // Session recovered but no integrity issues.
        assert_eq!(result.recovered, vec!["s1".to_string()]);
        assert!(result.integrity_repaired.is_empty());
    }

    #[tokio::test]
    async fn scan_repairs_truncated_lines() {
        let (state_store, _, _dir) = make_store().await;
        let session_dir = tempfile::tempdir().unwrap();
        let session_store = SessionStore::new(session_dir.path().to_path_buf());

        // Mark session as stuck.
        state_store
            .set("s1", REPAIR_NAMESPACE, "running_since", "1000")
            .await
            .unwrap();

        // Create a session with valid message + truncated line.
        session_store
            .append_typed("s1", &PersistedMessage::user("hello"))
            .await
            .unwrap();
        session_store
            .append_typed(
                "s1",
                &PersistedMessage::assistant("hi", "gpt-4o", "openai", 10, 5, None),
            )
            .await
            .unwrap();

        // Append a truncated line directly to the file.
        let path = session_dir.path().join("s1.jsonl");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"role\": \"assistant\", \"content\": \"trun\n")
            .unwrap();

        let result = scan_and_repair(
            &state_store,
            &["s1".to_string()],
            Duration::from_secs(60),
            3,
            Some(&session_store),
        )
        .await
        .unwrap();

        assert_eq!(result.recovered, vec!["s1".to_string()]);
        assert_eq!(result.integrity_repaired.len(), 1);
        assert_eq!(
            result.integrity_repaired[0].report.recovery_type_label(),
            "truncated_line"
        );

        // Session should now only have 2 valid messages.
        let messages = session_store.read_typed("s1").await.unwrap();
        assert_eq!(messages.len(), 2);
    }
}
