//! Pre-warmed sandbox container pool.
//!
//! Maintains a set of ready-to-use sandbox containers. Callers acquire a slot
//! via [`SandboxPool::try_acquire`] and receive a [`PoolGuard`] that returns
//! the slot to the pool on drop.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::sandbox::{Sandbox, SandboxId, SandboxScope};

/// Unique key prefix for pool containers to avoid collision with session containers.
const POOL_KEY_PREFIX: &str = "moltis-pool-";

/// Build a [`SandboxId`] for a pool slot.
fn pool_sandbox_id(index: usize) -> SandboxId {
    SandboxId {
        scope: SandboxScope::Shared,
        key: format!("{POOL_KEY_PREFIX}{index}"),
    }
}

/// A single slot in the sandbox pool.
pub struct PoolSlot {
    /// Pool-scoped index used to generate the container key.
    pub index: usize,
    /// Whether this slot is available for acquisition.
    available: AtomicBool,
}

impl PoolSlot {
    fn sandbox_id(&self) -> SandboxId {
        pool_sandbox_id(self.index)
    }
}

/// RAII guard returned by [`SandboxPool::try_acquire`].
///
/// When dropped, the slot is marked as available again and a replenishment
/// signal is sent so the pool can verify readiness.
pub struct PoolGuard {
    slot: Arc<PoolSlot>,
    replenish_tx: mpsc::Sender<()>,
}

impl PoolGuard {
    /// The [`SandboxId`] for this pool slot.
    pub fn sandbox_id(&self) -> SandboxId {
        self.slot.sandbox_id()
    }

    /// The slot index.
    pub fn index(&self) -> usize {
        self.slot.index
    }
}

impl Drop for PoolGuard {
    fn drop(&mut self) {
        self.slot.available.store(true, Ordering::Release);
        // Best-effort signal — if the channel is full, replenishment will
        // happen on the next drop or periodic check.
        let _ = self.replenish_tx.try_send(());
    }
}

/// Pre-warmed sandbox container pool.
///
/// Created at startup with `min_warm` containers. When a slot is acquired and
/// released, a background task verifies that containers are still ready.
pub struct SandboxPool {
    slots: Vec<Arc<PoolSlot>>,
    replenish_tx: mpsc::Sender<()>,
}

impl SandboxPool {
    /// Create a new pool and start `min_warm` containers.
    ///
    /// `max_slots` defaults to `min_warm * 2` when 0.
    pub async fn new(
        backend: Arc<dyn Sandbox>,
        min_warm: u32,
        max_slots: u32,
        image: Option<&str>,
    ) -> Self {
        let max_slots = if max_slots == 0 {
            min_warm.saturating_mul(2).max(1)
        } else {
            max_slots
        };

        let slots: Vec<Arc<PoolSlot>> = (0..max_slots as usize)
            .map(|i| {
                Arc::new(PoolSlot {
                    index: i,
                    available: AtomicBool::new(false),
                })
            })
            .collect();

        // Warm up min_warm slots concurrently.
        let warm_count = (min_warm as usize).min(slots.len());
        let mut warm_tasks = Vec::with_capacity(warm_count);
        for slot in slots.iter().take(warm_count) {
            let backend = Arc::clone(&backend);
            let sid = slot.sandbox_id();
            let image = image.map(String::from);
            let slot = Arc::clone(slot);
            warm_tasks.push(tokio::spawn(async move {
                match backend.ensure_ready(&sid, image.as_deref()).await {
                    Ok(()) => {
                        slot.available.store(true, Ordering::Release);
                        info!(key = %sid, "pool slot warmed");
                        true
                    }
                    Err(e) => {
                        warn!(key = %sid, error = %e, "failed to warm pool slot");
                        false
                    }
                }
            }));
        }

        let mut warmed = 0u32;
        for task in warm_tasks {
            if let Ok(true) = task.await {
                warmed += 1;
            }
        }
        info!(warmed, max_slots, "sandbox pool initialized");

        let (replenish_tx, mut replenish_rx) = mpsc::channel::<()>(16);

        // Background replenishment task.
        let slots_for_task = slots.clone();
        let backend_for_task = Arc::clone(&backend);
        let image_for_task = image.map(String::from);
        tokio::spawn(async move {
            while replenish_rx.recv().await.is_some() {
                for slot in &slots_for_task {
                    if slot.available.load(Ordering::Acquire) {
                        let sid = slot.sandbox_id();
                        if let Err(e) = backend_for_task
                            .ensure_ready(&sid, image_for_task.as_deref())
                            .await
                        {
                            debug!(key = %sid, error = %e, "pool replenishment check failed");
                        }
                    }
                }
            }
        });

        Self {
            slots,
            replenish_tx,
        }
    }

    /// Try to acquire an available slot.
    ///
    /// Returns `None` if no slots are available.
    pub fn try_acquire(&self) -> Option<PoolGuard> {
        for slot in &self.slots {
            if slot
                .available
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(PoolGuard {
                    slot: Arc::clone(slot),
                    replenish_tx: self.replenish_tx.clone(),
                });
            }
        }
        None
    }

    /// Number of currently available slots.
    pub fn available_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.available.load(Ordering::Acquire))
            .count()
    }

    /// Total number of pool slots.
    pub fn total_slots(&self) -> usize {
        self.slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    use crate::exec::{ExecOpts, ExecResult};

    struct MockSandbox {
        ensure_count: AtomicU32,
    }

    #[async_trait::async_trait]
    impl Sandbox for MockSandbox {
        fn backend_name(&self) -> &'static str {
            "mock"
        }

        async fn ensure_ready(
            &self,
            _id: &SandboxId,
            _image: Option<&str>,
        ) -> crate::Result<()> {
            self.ensure_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn exec(
            &self,
            _id: &SandboxId,
            _command: &str,
            _opts: &ExecOpts,
        ) -> crate::Result<ExecResult> {
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            })
        }

        async fn cleanup(&self, _id: &SandboxId) -> crate::Result<()> {
            Ok(())
        }
    }

    fn mock_backend() -> (Arc<MockSandbox>, Arc<dyn Sandbox>) {
        let concrete = Arc::new(MockSandbox {
            ensure_count: AtomicU32::new(0),
        });
        let dyn_ref: Arc<dyn Sandbox> = Arc::clone(&concrete) as _;
        (concrete, dyn_ref)
    }

    #[tokio::test]
    async fn test_pool_creation_warms_slots() {
        let (backend, dyn_backend) = mock_backend();
        let pool = SandboxPool::new(dyn_backend, 2, 4, None).await;

        assert_eq!(pool.total_slots(), 4);
        assert_eq!(pool.available_count(), 2);
        assert!(backend.ensure_count.load(Ordering::Relaxed) >= 2);
    }

    #[tokio::test]
    async fn test_acquire_and_release() {
        let (_backend, dyn_backend) = mock_backend();
        let pool = SandboxPool::new(dyn_backend, 2, 2, None).await;
        assert_eq!(pool.available_count(), 2);

        let guard1 = pool.try_acquire().unwrap();
        assert_eq!(pool.available_count(), 1);
        assert!(guard1.sandbox_id().key.starts_with(POOL_KEY_PREFIX));

        let guard2 = pool.try_acquire().unwrap();
        assert_eq!(pool.available_count(), 0);

        // No more slots available.
        assert!(pool.try_acquire().is_none());

        // Release one.
        drop(guard1);
        tokio::task::yield_now().await;
        assert_eq!(pool.available_count(), 1);

        drop(guard2);
        tokio::task::yield_now().await;
        assert_eq!(pool.available_count(), 2);
    }

    #[tokio::test]
    async fn test_default_max_slots() {
        let (_backend, dyn_backend) = mock_backend();
        // max_slots=0 should default to min_warm * 2
        let pool = SandboxPool::new(dyn_backend, 3, 0, None).await;
        assert_eq!(pool.total_slots(), 6);
    }
}
