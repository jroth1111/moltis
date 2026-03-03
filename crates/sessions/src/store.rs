use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
};

use {
    crate::{Error, Result},
    fd_lock::RwLock,
    serde::{Deserialize, Serialize},
};

/// A single search hit within a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub session_key: String,
    pub snippet: String,
    pub role: String,
    pub message_index: usize,
}

/// Session read output with malformed-line statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResult<T> {
    pub messages: Vec<T>,
    pub skipped_lines: usize,
    pub total_lines: usize,
}

/// Outcome of validating and repairing a session JSONL file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionIntegrityReport {
    pub recovered: bool,
    pub removed_malformed_lines: u64,
    pub appended_assistant_messages: u64,
    pub injected_tool_results: u64,
    pub replay_candidate: Option<String>,
    pub replay_blocked_by_tool_side_effects: bool,
}

/// Append-only JSONL session storage with file locking.
pub struct SessionStore {
    pub base_dir: PathBuf,
}

#[must_use]
fn slice_on_char_boundaries(content: &str, start: usize, end: usize) -> &str {
    let bounded_start = content.floor_char_boundary(start.min(content.len()));
    let bounded_end = content.floor_char_boundary(end.min(content.len()));
    if bounded_start >= bounded_end {
        return "";
    }
    &content[bounded_start..bounded_end]
}

impl SessionStore {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Sanitize a session key for use as a filename.
    pub fn key_to_filename(key: &str) -> String {
        key.replace(':', "_")
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.base_dir
            .join(format!("{}.jsonl", Self::key_to_filename(key)))
    }

    /// Directory for session media files (screenshots, audio, etc.).
    fn media_dir_for(&self, key: &str) -> PathBuf {
        self.base_dir.join("media").join(Self::key_to_filename(key))
    }

    /// Save a media file for a session. Returns the relative path from base_dir.
    pub async fn save_media(&self, key: &str, filename: &str, data: &[u8]) -> Result<String> {
        let dir = self.media_dir_for(key);
        let file_path = dir.join(filename);
        let data = data.to_vec();

        tokio::task::spawn_blocking(move || -> Result<()> {
            fs::create_dir_all(&dir)?;
            fs::write(&file_path, &data)?;
            Ok(())
        })
        .await??;

        let sanitized = Self::key_to_filename(key);
        Ok(format!("media/{sanitized}/{filename}"))
    }

    /// Read a media file. Returns raw bytes.
    pub async fn read_media(&self, key: &str, filename: &str) -> Result<Vec<u8>> {
        let file_path = self.media_dir_for(key).join(filename);

        tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let data = fs::read(&file_path)?;
            Ok(data)
        })
        .await?
    }

    /// Append a message (JSON value) as a single line to the session file.
    pub async fn append(&self, key: &str, message: &serde_json::Value) -> Result<()> {
        let path = self.path_for(key);
        let line = serde_json::to_string(message)?;

        tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            let mut lock = RwLock::new(file);
            let mut guard = lock
                .write()
                .map_err(|e| Error::lock_failed(e.to_string()))?;
            writeln!(*guard, "{line}")?;
            guard.sync_data()?;
            Ok(())
        })
        .await??;

        Ok(())
    }

    /// Read all messages from a session file and return skip statistics.
    pub async fn read_with_stats(&self, key: &str) -> Result<ReadResult<serde_json::Value>> {
        let path = self.path_for(key);

        let result = tokio::task::spawn_blocking(move || -> Result<ReadResult<serde_json::Value>> {
            if !path.exists() {
                return Ok(ReadResult {
                    messages: vec![],
                    skipped_lines: 0,
                    total_lines: 0,
                });
            }
            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            let mut messages = Vec::new();
            let mut skipped_lines = 0usize;
            let mut total_lines = 0usize;
            for line in reader.lines() {
                let line = line?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                total_lines = total_lines.saturating_add(1);
                match serde_json::from_str(trimmed) {
                    Ok(val) => messages.push(val),
                    Err(_) => {
                        skipped_lines = skipped_lines.saturating_add(1);
                    },
                }
            }
            Ok(ReadResult {
                messages,
                skipped_lines,
                total_lines,
            })
        })
        .await??;

        #[cfg(feature = "metrics")]
        if result.skipped_lines > 0 {
            moltis_metrics::counter!(moltis_metrics::session::JSONL_LINES_SKIPPED)
                .increment(result.skipped_lines as u64);
        }

        Ok(result)
    }

    /// Read all messages from a session file.
    pub async fn read(&self, key: &str) -> Result<Vec<serde_json::Value>> {
        let result = self.read_with_stats(key).await?;
        if result.skipped_lines > 0 {
            tracing::warn!(
                session = %key,
                skipped_lines = result.skipped_lines,
                total_lines = result.total_lines,
                "session JSONL read skipped malformed lines"
            );
        }
        Ok(result.messages)
    }

    /// Read all messages from a session that match a given `run_id`.
    pub async fn read_by_run_id(&self, key: &str, run_id: &str) -> Result<Vec<serde_json::Value>> {
        let all = self.read(key).await?;
        let run_id = run_id.to_string();
        Ok(all
            .into_iter()
            .filter(|msg| msg.get("run_id").and_then(|v| v.as_str()) == Some(&run_id))
            .collect())
    }

    /// Validate and repair a session JSONL file in place.
    ///
    /// Repairs include:
    /// - dropping malformed JSONL lines
    /// - appending synthetic tool_result rows for tool calls that never completed
    /// - appending a synthetic assistant message when the file ends with a user message
    pub async fn validate_session_integrity(&self, key: &str) -> Result<SessionIntegrityReport> {
        let path = self.path_for(key);

        tokio::task::spawn_blocking(move || -> Result<SessionIntegrityReport> {
            if !path.exists() {
                return Ok(SessionIntegrityReport::default());
            }

            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            let mut valid_lines: Vec<String> = Vec::new();
            let mut valid_values: Vec<serde_json::Value> = Vec::new();
            let mut removed_malformed = 0_u64;
            let mut pending_tool_calls: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();

            for line in reader.lines() {
                let line = line?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(value) => {
                        match value.get("role").and_then(|v| v.as_str()) {
                            Some("assistant") => {
                                if let Some(tool_calls) =
                                    value.get("tool_calls").and_then(|v| v.as_array())
                                {
                                    for call in tool_calls {
                                        let Some(id) = call.get("id").and_then(|v| v.as_str())
                                        else {
                                            continue;
                                        };
                                        let tool_name = call
                                            .get("function")
                                            .and_then(|f| f.get("name"))
                                            .and_then(|v| v.as_str())
                                            .or_else(|| call.get("name").and_then(|v| v.as_str()))
                                            .unwrap_or("unknown");
                                        pending_tool_calls
                                            .insert(id.to_string(), tool_name.to_string());
                                    }
                                }
                            },
                            Some("tool") | Some("tool_result") => {
                                if let Some(tool_call_id) =
                                    value.get("tool_call_id").and_then(|v| v.as_str())
                                {
                                    pending_tool_calls.remove(tool_call_id);
                                }
                            },
                            _ => {},
                        }

                        valid_lines.push(trimmed.to_string());
                        valid_values.push(value);
                    },
                    Err(_) => {
                        removed_malformed = removed_malformed.saturating_add(1);
                    },
                }
            }

            let ends_with_user = valid_values
                .last()
                .and_then(|v| v.get("role"))
                .and_then(|v| v.as_str())
                == Some("user");

            let tail_user_text = valid_values.last().and_then(|value| {
                let role = value.get("role").and_then(|v| v.as_str())?;
                if role != "user" {
                    return None;
                }
                if let Some(text) = value.get("content").and_then(|v| v.as_str()) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
                value
                    .get("content")
                    .and_then(|v| v.as_array())
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| {
                                if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                                    part.get("text").and_then(|v| v.as_str())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .and_then(|text| {
                        let trimmed = text.trim().to_string();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed)
                        }
                    })
            });

            let replay_blocked_by_tool_side_effects = !pending_tool_calls.is_empty();

            let mut injected_tool_results = 0_u64;
            for (tool_call_id, tool_name) in pending_tool_calls {
                let synthetic = crate::message::PersistedMessage::tool_result(
                    tool_call_id,
                    tool_name,
                    None,
                    false,
                    None,
                    Some(
                        "Recovered: tool call interrupted before result was persisted".to_string(),
                    ),
                );
                let value = serde_json::to_value(&synthetic)?;
                valid_lines.push(serde_json::to_string(&value)?);
                valid_values.push(value);
                injected_tool_results = injected_tool_results.saturating_add(1);
            }

            let mut appended_assistant_messages = 0_u64;
            let mut replay_candidate: Option<String> = None;
            if ends_with_user {
                if replay_blocked_by_tool_side_effects {
                    let synthetic = crate::message::PersistedMessage::assistant(
                        "Recovered from an interrupted run. Automatic replay was skipped because a prior tool call may have produced unresolved side effects.",
                        "recovery",
                        "system",
                        0,
                        0,
                        None,
                    );
                    let value = serde_json::to_value(&synthetic)?;
                    valid_lines.push(serde_json::to_string(&value)?);
                    appended_assistant_messages = 1;
                } else {
                    if let Some(text) = tail_user_text {
                        replay_candidate = Some(text);
                    } else {
                        let synthetic = crate::message::PersistedMessage::assistant(
                            "Recovered from an interrupted run. Automatic replay could not be prepared because the latest user message had no text payload.",
                            "recovery",
                            "system",
                            0,
                            0,
                            None,
                        );
                        let value = serde_json::to_value(&synthetic)?;
                        valid_lines.push(serde_json::to_string(&value)?);
                        appended_assistant_messages = 1;
                    }
                }
            }

            let recovered = removed_malformed > 0
                || appended_assistant_messages > 0
                || injected_tool_results > 0
                || replay_candidate.is_some();
            if !recovered {
                return Ok(SessionIntegrityReport::default());
            }

            let requires_rewrite =
                removed_malformed > 0 || appended_assistant_messages > 0 || injected_tool_results > 0;
            if requires_rewrite {
                let file = OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(&path)?;
                let mut lock = RwLock::new(file);
                let mut guard = lock
                    .write()
                    .map_err(|e| Error::lock_failed(e.to_string()))?;
                for line in &valid_lines {
                    writeln!(*guard, "{line}")?;
                }
                guard.sync_data()?;
            }

            Ok(SessionIntegrityReport {
                recovered,
                removed_malformed_lines: removed_malformed,
                appended_assistant_messages,
                injected_tool_results,
                replay_candidate,
                replay_blocked_by_tool_side_effects,
            })
        })
        .await?
    }

    /// Remove malformed JSONL lines from a session file and rewrite it atomically.
    ///
    /// Returns the number of malformed lines removed.
    pub async fn repair(&self, key: &str) -> Result<usize> {
        let path = self.path_for(key);

        tokio::task::spawn_blocking(move || -> Result<usize> {
            if !path.exists() {
                return Ok(0);
            }

            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            let mut valid_lines: Vec<String> = Vec::new();
            let mut removed = 0usize;

            for line in reader.lines() {
                let line = line?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(_) => valid_lines.push(trimmed.to_string()),
                    Err(_) => {
                        removed = removed.saturating_add(1);
                    },
                }
            }

            if removed == 0 {
                return Ok(0);
            }

            let temp_path = path.with_extension(format!("jsonl.repair.{}", uuid::Uuid::new_v4()));
            let tmp = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp_path)?;
            let mut lock = RwLock::new(tmp);
            let mut guard = lock
                .write()
                .map_err(|e| Error::lock_failed(e.to_string()))?;
            for line in &valid_lines {
                writeln!(*guard, "{line}")?;
            }
            guard.sync_data()?;
            drop(guard);
            fs::rename(&temp_path, &path)?;

            Ok(removed)
        })
        .await?
    }

    /// Read the last N messages from a session file.
    pub async fn read_last_n(&self, key: &str, n: usize) -> Result<Vec<serde_json::Value>> {
        let path = self.path_for(key);

        tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>> {
            if !path.exists() {
                return Ok(vec![]);
            }
            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            let mut all: Vec<serde_json::Value> = Vec::new();
            for line in reader.lines() {
                let line = line?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(val) = serde_json::from_str(trimmed) {
                    all.push(val);
                }
            }
            let start = all.len().saturating_sub(n);
            Ok(all[start..].to_vec())
        })
        .await?
    }

    /// Delete the session file and its media directory.
    pub async fn clear(&self, key: &str) -> Result<()> {
        let path = self.path_for(key);
        let media_dir = self.media_dir_for(key);

        tokio::task::spawn_blocking(move || -> Result<()> {
            if path.exists() {
                fs::remove_file(&path)?;
            }
            if media_dir.exists() {
                let _ = fs::remove_dir_all(&media_dir);
            }
            Ok(())
        })
        .await??;

        Ok(())
    }

    /// Archive messages to a cold-store JSONL file.
    ///
    /// Creates `{base_dir}/archive/{session_key}.{timestamp}.jsonl` containing
    /// the provided messages. The active session file is NOT modified; callers
    /// are responsible for replacing the active history afterward.
    ///
    /// Returns the archive filename (relative to `{base_dir}/archive/`).
    pub async fn archive_to_cold_store(
        &self,
        key: &str,
        messages: &[serde_json::Value],
    ) -> Result<String> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let safe_key = Self::key_to_filename(key);
        let archive_dir = self.base_dir.join("archive");
        let messages = messages.to_vec();

        let archive_filename = tokio::task::spawn_blocking(move || -> Result<String> {
            fs::create_dir_all(&archive_dir)?;
            loop {
                let archive_filename = format!("{safe_key}.{ts}.{}.jsonl", uuid::Uuid::new_v4());
                let archive_path = archive_dir.join(&archive_filename);
                let file = match OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&archive_path)
                {
                    Ok(file) => file,
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                        continue;
                    },
                    Err(err) => return Err(err.into()),
                };

                let mut lock = RwLock::new(file);
                let mut guard = lock
                    .write()
                    .map_err(|e| Error::lock_failed(e.to_string()))?;
                for msg in &messages {
                    let line = serde_json::to_string(msg)?;
                    writeln!(*guard, "{line}")?;
                }
                guard.sync_data()?;
                return Ok(archive_filename);
            }
        })
        .await??;

        Ok(archive_filename)
    }

    /// List all session keys by scanning JSONL files in the base directory.
    pub fn list_keys(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.base_dir) else {
            return vec![];
        };
        entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.strip_suffix(".jsonl").map(|s| s.replace('_', ":"))
            })
            .collect()
    }

    /// Search all sessions for messages containing `query` (case-insensitive).
    /// Returns up to `max_results` hits, at most one per session.
    pub async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>> {
        let base = self.base_dir.clone();
        let query = query.to_lowercase();

        tokio::task::spawn_blocking(move || {
            let mut results = Vec::new();
            let entries = fs::read_dir(&base)?;

            for entry in entries.flatten() {
                if results.len() >= max_results {
                    break;
                }
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let Some(key_raw) = name.strip_suffix(".jsonl") else {
                    continue;
                };
                let session_key = key_raw.replace('_', ":");

                let Ok(file) = File::open(&path) else {
                    continue;
                };
                let reader = BufReader::new(file);
                for (idx, line) in reader.lines().enumerate() {
                    let Ok(line) = line else {
                        continue;
                    };
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                        continue;
                    };
                    let content = val.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    if content.to_lowercase().contains(&query) {
                        let role = val
                            .get("role")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();

                        // Build a snippet: find the match position and extract context.
                        let lower = content.to_lowercase();
                        let pos = lower.find(&query).unwrap_or(0);
                        let start = pos.saturating_sub(40);
                        let end = pos.saturating_add(query.len()).saturating_add(60);
                        let snippet = slice_on_char_boundaries(content, start, end).to_string();

                        results.push(SearchResult {
                            session_key: session_key.clone(),
                            snippet,
                            role,
                            message_index: idx,
                        });
                        // One hit per session is enough for autocomplete.
                        break;
                    }
                }
            }

            Ok(results)
        })
        .await?
    }

    /// Replace the entire session history with the given messages.
    pub async fn replace_history(&self, key: &str, messages: Vec<serde_json::Value>) -> Result<()> {
        let path = self.path_for(key);

        tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)?;
            let mut lock = RwLock::new(file);
            let mut guard = lock
                .write()
                .map_err(|e| Error::lock_failed(e.to_string()))?;
            for msg in &messages {
                let line = serde_json::to_string(msg)?;
                writeln!(*guard, "{line}")?;
            }
            guard.sync_data()?;
            Ok(())
        })
        .await??;

        Ok(())
    }

    /// Keep only the last N messages in the session file.
    /// Returns the number of messages that were removed.
    pub async fn truncate_messages(&self, key: &str, keep_n: usize) -> Result<usize> {
        let all = self.read(key).await?;
        if all.len() <= keep_n {
            return Ok(0);
        }
        let removed = all.len() - keep_n;
        let kept = all[all.len() - keep_n..].to_vec();
        self.replace_history(key, kept).await?;
        Ok(removed)
    }

    /// Read all messages as typed [`PersistedMessage`] values.
    ///
    /// Lines that fail to deserialize into `PersistedMessage` are skipped
    /// (with a warning), matching the behavior of [`read`].
    pub async fn read_typed(&self, key: &str) -> Result<Vec<crate::message::PersistedMessage>> {
        let path = self.path_for(key);

        tokio::task::spawn_blocking(move || -> Result<Vec<crate::message::PersistedMessage>> {
            if !path.exists() {
                return Ok(vec![]);
            }
            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            let mut messages = Vec::new();
            for line in reader.lines() {
                let line = line?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str(trimmed) {
                    Ok(msg) => messages.push(msg),
                    Err(e) => {
                        tracing::warn!("skipping malformed JSONL line (typed): {e}");
                    },
                }
            }
            Ok(messages)
        })
        .await?
    }

    /// Read the last N messages as typed [`PersistedMessage`] values.
    pub async fn read_last_n_typed(
        &self,
        key: &str,
        n: usize,
    ) -> Result<Vec<crate::message::PersistedMessage>> {
        let path = self.path_for(key);

        tokio::task::spawn_blocking(move || -> Result<Vec<crate::message::PersistedMessage>> {
            if !path.exists() {
                return Ok(vec![]);
            }
            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            let mut all: Vec<crate::message::PersistedMessage> = Vec::new();
            for line in reader.lines() {
                let line = line?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(msg) = serde_json::from_str(trimmed) {
                    all.push(msg);
                }
            }
            let start = all.len().saturating_sub(n);
            Ok(all[start..].to_vec())
        })
        .await?
    }

    /// Replace the entire session history with typed messages.
    pub async fn replace_history_typed(
        &self,
        key: &str,
        messages: &[crate::message::PersistedMessage],
    ) -> Result<()> {
        let path = self.path_for(key);
        let values: Vec<serde_json::Value> = messages.iter().map(|m| m.to_value()).collect();

        tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)?;
            let mut lock = RwLock::new(file);
            let mut guard = lock
                .write()
                .map_err(|e| Error::lock_failed(e.to_string()))?;
            for msg in &values {
                let line = serde_json::to_string(msg)?;
                writeln!(*guard, "{line}")?;
            }
            guard.sync_data()?;
            Ok(())
        })
        .await??;

        Ok(())
    }

    /// Append a typed message to the session file.
    pub async fn append_typed(
        &self,
        key: &str,
        message: &crate::message::PersistedMessage,
    ) -> Result<()> {
        self.append(key, &message.to_value()).await
    }

    /// Count messages in a session file without parsing them.
    pub async fn count(&self, key: &str) -> Result<u32> {
        let path = self.path_for(key);

        tokio::task::spawn_blocking(move || -> Result<u32> {
            if !path.exists() {
                return Ok(0);
            }
            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            let count = reader
                .lines()
                .map_while(std::result::Result::ok)
                .filter(|l| !l.trim().is_empty())
                .count();
            Ok(count as u32)
        })
        .await?
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use {super::*, serde_json::json};

    fn temp_store() -> (SessionStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        (store, dir)
    }

    #[test]
    fn slice_on_char_boundaries_handles_multibyte_boundary() {
        let content = format!("{}л{}", "a".repeat(39), "z".repeat(20));
        let snippet = slice_on_char_boundaries(&content, 0, 40);
        assert_eq!(snippet.len(), 39);
        assert!(snippet.chars().all(|c| c == 'a'));
    }

    #[tokio::test]
    async fn test_append_and_read() {
        let (store, _dir) = temp_store();

        store
            .append("main", &json!({"role": "user", "content": "hello"}))
            .await
            .unwrap();
        store
            .append("main", &json!({"role": "assistant", "content": "hi"}))
            .await
            .unwrap();

        let msgs = store.read("main").await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
    }

    #[tokio::test]
    async fn test_read_empty() {
        let (store, _dir) = temp_store();
        let msgs = store.read("nonexistent").await.unwrap();
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn test_read_with_stats_counts_malformed_lines() {
        let (store, dir) = temp_store();
        let path = dir.path().join("stats.jsonl");
        let raw = format!(
            "{}\n{}\n{}\n\n",
            json!({"role": "user", "content": "ok"}),
            "{not valid json}",
            json!({"role": "assistant", "content": "still ok"})
        );
        fs::write(path, raw).unwrap();

        let result = store.read_with_stats("stats").await.unwrap();
        assert_eq!(result.total_lines, 3);
        assert_eq!(result.skipped_lines, 1);
        assert_eq!(result.messages.len(), 2);
    }

    #[tokio::test]
    async fn test_read_last_n() {
        let (store, _dir) = temp_store();

        for i in 0..10 {
            store.append("test", &json!({"i": i})).await.unwrap();
        }

        let last3 = store.read_last_n("test", 3).await.unwrap();
        assert_eq!(last3.len(), 3);
        assert_eq!(last3[0]["i"], 7);
        assert_eq!(last3[2]["i"], 9);
    }

    #[tokio::test]
    async fn test_clear() {
        let (store, _dir) = temp_store();

        store
            .append("main", &json!({"role": "user", "content": "hello"}))
            .await
            .unwrap();
        assert_eq!(store.read("main").await.unwrap().len(), 1);

        store.clear("main").await.unwrap();
        assert!(store.read("main").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_count() {
        let (store, _dir) = temp_store();

        assert_eq!(store.count("main").await.unwrap(), 0);
        store
            .append("main", &json!({"role": "user"}))
            .await
            .unwrap();
        store
            .append("main", &json!({"role": "assistant"}))
            .await
            .unwrap();
        assert_eq!(store.count("main").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_search_matching() {
        let (store, _dir) = temp_store();

        store
            .append("s1", &json!({"role": "user", "content": "hello world"}))
            .await
            .unwrap();
        store
            .append("s1", &json!({"role": "assistant", "content": "hi there"}))
            .await
            .unwrap();
        store
            .append("s2", &json!({"role": "user", "content": "goodbye world"}))
            .await
            .unwrap();

        let results = store.search("hello", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_key, "s1");
        assert_eq!(results[0].role, "user");
        assert!(results[0].snippet.contains("hello"));
    }

    #[tokio::test]
    async fn test_search_case_insensitive() {
        let (store, _dir) = temp_store();

        store
            .append("s1", &json!({"role": "user", "content": "Hello World"}))
            .await
            .unwrap();

        let results = store.search("hello world", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_key, "s1");
    }

    #[tokio::test]
    async fn test_search_no_match() {
        let (store, _dir) = temp_store();

        store
            .append("s1", &json!({"role": "user", "content": "hello"}))
            .await
            .unwrap();

        let results = store.search("xyz", 10).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_empty_query() {
        let (store, _dir) = temp_store();

        store
            .append("s1", &json!({"role": "user", "content": "hello"}))
            .await
            .unwrap();

        // Empty query should match nothing (caller should guard against this)
        let results = store.search("", 10).await.unwrap();
        // Empty string is contained in every string, so it would match.
        // The frontend guards against empty queries, but the store doesn't — that's fine.
        assert!(!results.is_empty());
    }

    #[tokio::test]
    async fn test_search_across_sessions() {
        let (store, _dir) = temp_store();

        store
            .append("s1", &json!({"role": "user", "content": "rust is great"}))
            .await
            .unwrap();
        store
            .append(
                "s2",
                &json!({"role": "assistant", "content": "rust is awesome"}),
            )
            .await
            .unwrap();
        store
            .append("s3", &json!({"role": "user", "content": "python is nice"}))
            .await
            .unwrap();

        let results = store.search("rust", 10).await.unwrap();
        assert_eq!(results.len(), 2);
        let keys: Vec<&str> = results.iter().map(|r| r.session_key.as_str()).collect();
        assert!(keys.contains(&"s1"));
        assert!(keys.contains(&"s2"));
    }

    #[tokio::test]
    async fn test_search_max_results() {
        let (store, _dir) = temp_store();

        for i in 0..10 {
            let key = format!("s{i}");
            store
                .append(&key, &json!({"role": "user", "content": "common term"}))
                .await
                .unwrap();
        }

        let results = store.search("common", 3).await.unwrap();
        assert!(results.len() <= 3);
    }

    #[tokio::test]
    async fn test_replace_history() {
        let (store, _dir) = temp_store();

        store
            .append("main", &json!({"role": "user", "content": "hello"}))
            .await
            .unwrap();
        store
            .append("main", &json!({"role": "assistant", "content": "hi"}))
            .await
            .unwrap();
        assert_eq!(store.read("main").await.unwrap().len(), 2);

        let new_history = vec![json!({"role": "assistant", "content": "summary"})];
        store.replace_history("main", new_history).await.unwrap();

        let msgs = store.read("main").await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"], "summary");
    }

    #[tokio::test]
    async fn test_replace_history_empty() {
        let (store, _dir) = temp_store();

        store
            .append("main", &json!({"role": "user", "content": "hello"}))
            .await
            .unwrap();

        store.replace_history("main", vec![]).await.unwrap();
        assert!(store.read("main").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_key_sanitization() {
        let (store, _dir) = temp_store();

        store
            .append("session:abc-123", &json!({"role": "user"}))
            .await
            .unwrap();
        let msgs = store.read("session:abc-123").await.unwrap();
        assert_eq!(msgs.len(), 1);
    }

    #[tokio::test]
    async fn test_save_and_read_media() {
        let (store, _dir) = temp_store();
        let data = b"fake png data";

        let path = store.save_media("main", "call_1.png", data).await.unwrap();
        assert_eq!(path, "media/main/call_1.png");

        let read_back = store.read_media("main", "call_1.png").await.unwrap();
        assert_eq!(read_back, data);
    }

    #[tokio::test]
    async fn test_save_media_with_colon_key() {
        let (store, _dir) = temp_store();
        let data = b"screenshot bytes";

        let path = store
            .save_media("session:abc", "shot.png", data)
            .await
            .unwrap();
        assert_eq!(path, "media/session_abc/shot.png");

        let read_back = store.read_media("session:abc", "shot.png").await.unwrap();
        assert_eq!(read_back, data);
    }

    #[tokio::test]
    async fn test_read_media_missing_file() {
        let (store, _dir) = temp_store();
        let result = store.read_media("main", "nonexistent.png").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_clear_removes_media_dir() {
        let (store, dir) = temp_store();

        // Create a session and media.
        store
            .append("main", &json!({"role": "user", "content": "hello"}))
            .await
            .unwrap();
        store
            .save_media("main", "shot.png", b"img data")
            .await
            .unwrap();

        let media_dir = dir.path().join("media").join("main");
        assert!(media_dir.exists());

        store.clear("main").await.unwrap();

        assert!(!media_dir.exists());
        assert!(store.read("main").await.unwrap().is_empty());
    }

    // --- Typed API tests ---

    #[tokio::test]
    async fn test_append_typed_and_read_typed() {
        use crate::message::PersistedMessage;

        let (store, _dir) = temp_store();

        store
            .append_typed("main", &PersistedMessage::user("hello"))
            .await
            .unwrap();
        store
            .append_typed(
                "main",
                &PersistedMessage::assistant("hi", "gpt-4o", "openai", 10, 5, None),
            )
            .await
            .unwrap();

        let msgs = store.read_typed("main").await.unwrap();
        assert_eq!(msgs.len(), 2);
        match &msgs[0] {
            PersistedMessage::User { content, .. } => {
                assert!(matches!(content, crate::message::MessageContent::Text(t) if t == "hello"));
            },
            _ => panic!("expected User message"),
        }
        match &msgs[1] {
            PersistedMessage::Assistant { content, model, .. } => {
                assert_eq!(content, "hi");
                assert_eq!(model.as_deref(), Some("gpt-4o"));
            },
            _ => panic!("expected Assistant message"),
        }
    }

    #[tokio::test]
    async fn test_read_typed_empty() {
        let (store, _dir) = temp_store();
        let msgs = store.read_typed("nonexistent").await.unwrap();
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn test_read_last_n_typed() {
        use crate::message::PersistedMessage;

        let (store, _dir) = temp_store();

        for i in 0..5 {
            store
                .append_typed("test", &PersistedMessage::user(format!("msg-{i}")))
                .await
                .unwrap();
        }

        let last2 = store.read_last_n_typed("test", 2).await.unwrap();
        assert_eq!(last2.len(), 2);
        match &last2[0] {
            PersistedMessage::User { content, .. } => {
                assert!(matches!(content, crate::message::MessageContent::Text(t) if t == "msg-3"));
            },
            _ => panic!("expected User message"),
        }
        match &last2[1] {
            PersistedMessage::User { content, .. } => {
                assert!(matches!(content, crate::message::MessageContent::Text(t) if t == "msg-4"));
            },
            _ => panic!("expected User message"),
        }
    }

    #[tokio::test]
    async fn test_replace_history_typed() {
        use crate::message::PersistedMessage;

        let (store, _dir) = temp_store();

        store
            .append_typed("main", &PersistedMessage::user("old"))
            .await
            .unwrap();
        assert_eq!(store.count("main").await.unwrap(), 1);

        let new_history = vec![
            PersistedMessage::user("new1"),
            PersistedMessage::assistant("new2", "gpt-4o", "openai", 10, 5, None),
        ];
        store
            .replace_history_typed("main", &new_history)
            .await
            .unwrap();

        let msgs = store.read_typed("main").await.unwrap();
        assert_eq!(msgs.len(), 2);
        match &msgs[0] {
            PersistedMessage::User { content, .. } => {
                assert!(matches!(content, crate::message::MessageContent::Text(t) if t == "new1"));
            },
            _ => panic!("expected User message"),
        }
    }

    #[tokio::test]
    async fn test_typed_roundtrip_with_value_api() {
        use crate::message::PersistedMessage;

        let (store, _dir) = temp_store();

        // Write with typed API, read with Value API.
        store
            .append_typed("main", &PersistedMessage::user("typed write"))
            .await
            .unwrap();
        let values = store.read("main").await.unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["role"], "user");
        assert_eq!(values[0]["content"], "typed write");

        // Write with Value API, read with typed API.
        store
            .append(
                "main",
                &json!({"role": "assistant", "content": "value write"}),
            )
            .await
            .unwrap();
        let typed = store.read_typed("main").await.unwrap();
        assert_eq!(typed.len(), 2);
        match &typed[1] {
            PersistedMessage::Assistant { content, .. } => {
                assert_eq!(content, "value write");
            },
            _ => panic!("expected Assistant message"),
        }
    }

    #[tokio::test]
    async fn test_archive_to_cold_store_creates_file() {
        let (store, dir) = temp_store();

        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
        ];

        let filename = store
            .archive_to_cold_store("session:abc", &messages)
            .await
            .unwrap();

        // Filename must not be empty and must be a .jsonl file.
        assert!(!filename.is_empty());
        assert!(
            filename.ends_with(".jsonl"),
            "expected .jsonl, got {filename}"
        );

        // Archive file must exist under archive/.
        let archive_path = dir.path().join("archive").join(&filename);
        assert!(
            archive_path.exists(),
            "archive file not found: {archive_path:?}"
        );

        // Archive file must contain the same number of lines as messages.
        let content = fs::read_to_string(&archive_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), messages.len());

        // Active session must not be modified.
        let active = store.read("session:abc").await.unwrap();
        assert!(active.is_empty(), "archive should not touch active session");
    }

    #[tokio::test]
    async fn test_archive_to_cold_store_colon_key_sanitized() {
        let (store, dir) = temp_store();

        let messages = vec![json!({"role": "user", "content": "msg"})];
        let filename = store
            .archive_to_cold_store("foo:bar", &messages)
            .await
            .unwrap();

        // Colons must be replaced by underscores in the filename.
        assert!(
            !filename.contains(':'),
            "filename must not contain colons: {filename}"
        );
        let archive_path = dir.path().join("archive").join(&filename);
        assert!(archive_path.exists());
    }

    #[tokio::test]
    async fn test_archive_to_cold_store_same_second_unique_filenames() {
        let (store, dir) = temp_store();

        fn extract_unix_seconds(filename: &str) -> Option<u64> {
            filename
                .split('.')
                .find(|segment| segment.chars().all(|c| c.is_ascii_digit()))
                .and_then(|value| value.parse::<u64>().ok())
        }

        let mut same_second_pair: Option<(String, String)> = None;
        for _ in 0..32 {
            let first = store
                .archive_to_cold_store(
                    "session:abc",
                    &[json!({"role": "user", "content": "first"})],
                )
                .await
                .unwrap();
            let second = store
                .archive_to_cold_store(
                    "session:abc",
                    &[
                        json!({"role": "user", "content": "second-a"}),
                        json!({"role": "assistant", "content": "second-b"}),
                    ],
                )
                .await
                .unwrap();

            if extract_unix_seconds(&first) == extract_unix_seconds(&second) {
                same_second_pair = Some((first, second));
                break;
            }
        }

        let (first, second) =
            same_second_pair.expect("expected to create two archives within the same second");
        assert_ne!(first, second, "archive filenames must be unique");

        let first_path = dir.path().join("archive").join(&first);
        let second_path = dir.path().join("archive").join(&second);
        assert!(first_path.exists());
        assert!(second_path.exists());

        let first_lines = fs::read_to_string(first_path).unwrap().lines().count();
        let second_lines = fs::read_to_string(second_path).unwrap().lines().count();
        assert_eq!(first_lines, 1, "first archive content must remain intact");
        assert_eq!(second_lines, 2, "second archive content must remain intact");
    }

    #[tokio::test]
    async fn test_truncate_messages() {
        let (store, _dir) = temp_store();

        for i in 0..10 {
            store
                .append(
                    "main",
                    &json!({"role": "user", "content": format!("msg-{i}")}),
                )
                .await
                .unwrap();
        }

        let removed = store.truncate_messages("main", 3).await.unwrap();
        assert_eq!(removed, 7);

        let msgs = store.read("main").await.unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["content"], "msg-7");
        assert_eq!(msgs[2]["content"], "msg-9");
    }

    #[tokio::test]
    async fn test_truncate_messages_noop_when_short() {
        let (store, _dir) = temp_store();

        for i in 0..3 {
            store
                .append(
                    "main",
                    &json!({"role": "user", "content": format!("msg-{i}")}),
                )
                .await
                .unwrap();
        }

        let removed = store.truncate_messages("main", 10).await.unwrap();
        assert_eq!(removed, 0);

        let msgs = store.read("main").await.unwrap();
        assert_eq!(msgs.len(), 3);
    }

    #[tokio::test]
    async fn test_validate_session_integrity_repairs_interrupted_session() {
        let (store, _dir) = temp_store();
        let path = store.path_for("repair:case");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let assistant_with_tool_call = serde_json::json!({
            "role": "assistant",
            "content": "running tool",
            "tool_calls": [{
                "id": "tool-call-1",
                "type": "function",
                "function": {
                    "name": "exec",
                    "arguments": "{}"
                }
            }]
        });
        let user_message = serde_json::json!({
            "role": "user",
            "content": "continue"
        });
        let mut raw = String::new();
        raw.push_str(&serde_json::to_string(&assistant_with_tool_call).unwrap());
        raw.push('\n');
        raw.push_str("{not valid json}\n");
        raw.push_str(&serde_json::to_string(&user_message).unwrap());
        raw.push('\n');
        fs::write(&path, raw).unwrap();

        let report = store
            .validate_session_integrity("repair:case")
            .await
            .unwrap();
        assert!(report.recovered);
        assert_eq!(report.removed_malformed_lines, 1);
        assert_eq!(report.injected_tool_results, 1);
        assert_eq!(report.appended_assistant_messages, 1);
        assert!(report.replay_candidate.is_none());
        assert!(report.replay_blocked_by_tool_side_effects);

        let repaired = store.read("repair:case").await.unwrap();
        assert_eq!(repaired.len(), 4);
        assert!(
            repaired
                .iter()
                .any(|msg| msg.get("role").and_then(|v| v.as_str()) == Some("tool_result"))
        );
        assert_eq!(
            repaired
                .last()
                .and_then(|msg| msg.get("role"))
                .and_then(|v| v.as_str()),
            Some("assistant")
        );
    }

    #[tokio::test]
    async fn test_validate_session_integrity_noop_for_clean_session() {
        let (store, _dir) = temp_store();
        store
            .append("clean", &json!({"role": "user", "content": "hello"}))
            .await
            .unwrap();
        store
            .append("clean", &json!({"role": "assistant", "content": "hi"}))
            .await
            .unwrap();

        let report = store.validate_session_integrity("clean").await.unwrap();
        assert!(!report.recovered);
        assert_eq!(report.removed_malformed_lines, 0);
        assert_eq!(report.injected_tool_results, 0);
        assert_eq!(report.appended_assistant_messages, 0);
        assert!(report.replay_candidate.is_none());
        assert!(!report.replay_blocked_by_tool_side_effects);
    }

    #[tokio::test]
    async fn test_validate_session_integrity_sets_replay_candidate_for_tail_user() {
        let (store, _dir) = temp_store();
        store
            .append("replay", &json!({"role": "user", "content": "please continue"}))
            .await
            .unwrap();

        let report = store.validate_session_integrity("replay").await.unwrap();
        assert!(report.recovered);
        assert_eq!(
            report.replay_candidate.as_deref(),
            Some("please continue")
        );
        assert_eq!(report.appended_assistant_messages, 0);
        assert!(!report.replay_blocked_by_tool_side_effects);

        let after = store.read("replay").await.unwrap();
        assert_eq!(after.len(), 1, "replay candidate should not rewrite history");
        assert_eq!(after[0]["role"], "user");
    }

    #[tokio::test]
    async fn test_repair_removes_malformed_lines() {
        let (store, dir) = temp_store();
        let path = dir.path().join("repair.jsonl");
        let raw = format!(
            "{}\n{}\n{}\n",
            json!({"role": "user", "content": "ok"}),
            "{not valid json}",
            json!({"role": "assistant", "content": "still ok"})
        );
        fs::write(path, raw).unwrap();

        let removed = store.repair("repair").await.unwrap();
        assert_eq!(removed, 1);

        let repaired = store.read("repair").await.unwrap();
        assert_eq!(repaired.len(), 2);
        assert_eq!(repaired[0]["content"], "ok");
        assert_eq!(repaired[1]["content"], "still ok");
    }

    #[tokio::test]
    async fn test_repair_noop_for_clean_or_missing_file() {
        let (store, _dir) = temp_store();
        assert_eq!(store.repair("missing").await.unwrap(), 0);

        store
            .append("clean", &json!({"role": "user", "content": "ok"}))
            .await
            .unwrap();
        assert_eq!(store.repair("clean").await.unwrap(), 0);
        let cleaned = store.read("clean").await.unwrap();
        assert_eq!(cleaned.len(), 1);
    }
}
