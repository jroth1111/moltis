//! Turn-level undo for in-session history rewinding.
//!
//! `UndoManager` maintains a per-session stack of checkpoints. Each checkpoint
//! captures the message history at a given turn index, allowing the caller to
//! roll back the most recent turn(s) without creating a new branch.
//!
//! State is ephemeral: it lives only for the lifetime of the server process
//! and is not persisted to disk. Sessions that reconnect start with an empty
//! undo stack.

use std::collections::VecDeque;

/// Maximum number of undo checkpoints kept per session.
const DEFAULT_MAX_CHECKPOINTS: usize = 20;

/// A saved snapshot of session messages at a specific turn.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// The full message history at this point in the session.
    pub messages: Vec<serde_json::Value>,
    /// How many agent turns had completed when this snapshot was taken.
    pub turn_index: usize,
}

/// Per-session undo/redo manager.
///
/// Typical usage:
/// 1. After each agent turn completes, call [`push`] to save a checkpoint.
/// 2. When the user requests undo, call [`undo`] to restore the previous state.
/// 3. After undo, the caller should replace the persisted session history with
///    the returned messages.
#[derive(Clone)]
pub struct UndoManager {
    undo_stack: VecDeque<Checkpoint>,
    redo_stack: Vec<Checkpoint>,
    max_checkpoints: usize,
}

impl UndoManager {
    /// Create a new manager with the default checkpoint limit.
    #[must_use]
    pub fn new() -> Self {
        Self::with_max(DEFAULT_MAX_CHECKPOINTS)
    }

    /// Create a new manager with a custom checkpoint limit.
    #[must_use]
    pub fn with_max(max_checkpoints: usize) -> Self {
        Self {
            undo_stack: VecDeque::new(),
            redo_stack: Vec::new(),
            max_checkpoints: max_checkpoints.max(1),
        }
    }

    /// Save a checkpoint after a turn completes.
    ///
    /// Clears the redo stack (you can't redo after new turns).
    pub fn push(&mut self, messages: Vec<serde_json::Value>, turn_index: usize) {
        self.redo_stack.clear();
        self.undo_stack.push_back(Checkpoint {
            messages,
            turn_index,
        });
        while self.undo_stack.len() > self.max_checkpoints {
            self.undo_stack.pop_front();
        }
    }

    /// Roll back the last turn, returning the previous checkpoint.
    ///
    /// Returns `None` if there is nothing to undo (stack is empty or only one entry).
    pub fn undo(&mut self, current_messages: Vec<serde_json::Value>) -> Option<Checkpoint> {
        if self.undo_stack.is_empty() {
            return None;
        }
        // Save current state on redo stack before rolling back.
        let current_turn = self
            .undo_stack
            .back()
            .map_or(0, |c| c.turn_index.saturating_add(1));
        self.redo_stack.push(Checkpoint {
            messages: current_messages,
            turn_index: current_turn,
        });
        self.undo_stack.pop_back()
    }

    /// Re-apply the most recently undone checkpoint.
    ///
    /// Returns `None` if there is nothing to redo.
    pub fn redo(&mut self) -> Option<Checkpoint> {
        let cp = self.redo_stack.pop()?;
        self.undo_stack.push_back(cp.clone());
        Some(cp)
    }

    /// Number of available undo steps.
    #[must_use]
    pub fn undo_depth(&self) -> usize {
        self.undo_stack.len()
    }

    /// Number of available redo steps.
    #[must_use]
    pub fn redo_depth(&self) -> usize {
        self.redo_stack.len()
    }

    /// Clear all undo/redo history.
    pub fn clear(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
    }
}

impl Default for UndoManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use {super::*, serde_json::json};

    fn msgs(n: u32) -> Vec<serde_json::Value> {
        (0..n).map(|i| json!({"role": "user", "idx": i})).collect()
    }

    #[test]
    fn push_and_undo() {
        let mut mgr = UndoManager::new();
        mgr.push(msgs(2), 0);
        mgr.push(msgs(4), 1);

        let restored = mgr.undo(msgs(6)).unwrap();
        assert_eq!(restored.turn_index, 1);
        assert_eq!(restored.messages.len(), 4);
    }

    #[test]
    fn undo_empty_returns_none() {
        let mut mgr = UndoManager::new();
        assert!(mgr.undo(msgs(2)).is_none());
    }

    #[test]
    fn redo_after_undo() {
        let mut mgr = UndoManager::new();
        mgr.push(msgs(2), 0);
        mgr.push(msgs(4), 1);

        let _restored = mgr.undo(msgs(6));
        assert_eq!(mgr.redo_depth(), 1);

        let redone = mgr.redo().unwrap();
        assert_eq!(redone.messages.len(), 6);
    }

    #[test]
    fn redo_stack_cleared_on_new_push() {
        let mut mgr = UndoManager::new();
        mgr.push(msgs(2), 0);
        mgr.push(msgs(4), 1);
        let _undo = mgr.undo(msgs(6));
        // New push should clear redo
        mgr.push(msgs(3), 2);
        assert_eq!(mgr.redo_depth(), 0);
    }

    #[test]
    fn evicts_oldest_when_over_max() {
        let mut mgr = UndoManager::with_max(3);
        mgr.push(msgs(1), 0);
        mgr.push(msgs(2), 1);
        mgr.push(msgs(3), 2);
        mgr.push(msgs(4), 3); // should evict turn_index=0

        assert_eq!(mgr.undo_depth(), 3);
        // The oldest remaining checkpoint should be turn_index=1
        let oldest = mgr.undo_stack.front().unwrap();
        assert_eq!(oldest.turn_index, 1);
    }

    #[test]
    fn clear_resets_both_stacks() {
        let mut mgr = UndoManager::new();
        mgr.push(msgs(2), 0);
        mgr.push(msgs(4), 1);
        let _ = mgr.undo(msgs(6));
        mgr.clear();

        assert_eq!(mgr.undo_depth(), 0);
        assert_eq!(mgr.redo_depth(), 0);
    }

    #[test]
    fn depth_counters() {
        let mut mgr = UndoManager::new();
        assert_eq!(mgr.undo_depth(), 0);
        assert_eq!(mgr.redo_depth(), 0);

        mgr.push(msgs(2), 0);
        assert_eq!(mgr.undo_depth(), 1);

        let _ = mgr.undo(msgs(4));
        assert_eq!(mgr.undo_depth(), 0);
        assert_eq!(mgr.redo_depth(), 1);
    }
}
