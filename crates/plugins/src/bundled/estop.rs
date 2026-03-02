//! Emergency stop hook.
//!
//! Checks an atomic flag (and optionally a sentinel file at `~/.moltis/estop`)
//! and blocks all agent starts and LLM calls when activated.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use moltis_common::{
    Result,
    hooks::{HookAction, HookEvent, HookHandler, HookPayload},
};

pub struct EstopHook {
    pub stopped: Arc<AtomicBool>,
    sentinel_file: Option<std::path::PathBuf>,
}

impl EstopHook {
    pub fn new(stopped: Arc<AtomicBool>) -> Self {
        Self {
            stopped,
            sentinel_file: None,
        }
    }

    pub fn with_sentinel(stopped: Arc<AtomicBool>, sentinel_file: std::path::PathBuf) -> Self {
        Self {
            stopped,
            sentinel_file: Some(sentinel_file),
        }
    }

    pub fn from_file() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        let sentinel = std::path::Path::new(&home).join(".moltis/estop");
        let stopped = sentinel.exists();
        Self::with_sentinel(Arc::new(AtomicBool::new(stopped)), sentinel)
    }

    fn refresh_from_sentinel(&self) {
        if let Some(path) = &self.sentinel_file {
            self.stopped.store(path.exists(), Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl HookHandler for EstopHook {
    fn name(&self) -> &str {
        "estop"
    }

    fn events(&self) -> &[HookEvent] {
        &[HookEvent::BeforeAgentStart, HookEvent::BeforeLLMCall]
    }

    fn priority(&self) -> i32 {
        1000
    }

    async fn handle(&self, _event: HookEvent, _payload: &HookPayload) -> Result<HookAction> {
        self.refresh_from_sentinel();
        if self.stopped.load(Ordering::Acquire) {
            return Ok(HookAction::Block("EMERGENCY STOP active".to_string()));
        }
        Ok(HookAction::Continue)
    }

    fn handle_sync(&self, _event: HookEvent, _payload: &HookPayload) -> Result<HookAction> {
        self.refresh_from_sentinel();
        if self.stopped.load(Ordering::Acquire) {
            return Ok(HookAction::Block("EMERGENCY STOP active".to_string()));
        }
        Ok(HookAction::Continue)
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocks_when_stopped() {
        let hook = EstopHook::new(Arc::new(AtomicBool::new(true)));
        let payload = HookPayload::BeforeAgentStart {
            session_key: "s1".into(),
            model: "claude".into(),
        };
        let result = hook
            .handle(HookEvent::BeforeAgentStart, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Block(ref msg) if msg.contains("EMERGENCY STOP")));
    }

    #[tokio::test]
    async fn continues_when_not_stopped() {
        let hook = EstopHook::new(Arc::new(AtomicBool::new(false)));
        let payload = HookPayload::BeforeLLMCall {
            session_key: "s1".into(),
            provider: "p".into(),
            model: "m".into(),
            messages: serde_json::json!([]),
            tool_count: 0,
            iteration: 1,
        };
        let result = hook
            .handle(HookEvent::BeforeLLMCall, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Continue));
    }

    #[test]
    fn sync_handle_blocks() {
        let hook = EstopHook::new(Arc::new(AtomicBool::new(true)));
        let payload = HookPayload::BeforeLLMCall {
            session_key: "s1".into(),
            provider: "p".into(),
            model: "m".into(),
            messages: serde_json::json!([]),
            tool_count: 0,
            iteration: 1,
        };
        let result = hook
            .handle_sync(HookEvent::BeforeLLMCall, &payload)
            .unwrap();
        assert!(matches!(result, HookAction::Block(_)));
    }
}
