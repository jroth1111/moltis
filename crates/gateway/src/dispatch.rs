//! Intent dispatch loop — hypervisor for long-running, multi-shift agent work.
//!
//! ## Overview
//!
//! The dispatch loop runs as a background task. Every
//! `config.tasks.dispatch_poll_interval_secs` seconds it:
//!
//! 1. Queries `tasks WHERE is_intent = 1 AND state_name IN ('Pending', 'Active')`.
//! 2. For each intent:
//!    - Claims it (`Pending → Active`) if needed.
//!    - Guards against duplicate shifts with [`TaskStore::has_non_terminal_child`].
//!    - Creates a bounded *shift* task (child of the intent).
//!    - Updates [`IntentStore`] with the active shift ID.
//!    - Calls `chat.send_sync` with a scoped session key, blocking until the
//!      shift agent completes.
//!    - Finalizes atomically: shift `→ Complete`, token ledger, snapshot update.
//!    - Checks for spin (no measurable progress) or budget exhaustion; escalates
//!      to `AwaitingHuman` when either condition triggers.
//!
//! ## Session keys
//!
//! Each shift runs in an isolated session:
//! `dispatch:intent:{intent_id}:shift:{n}` — never the user's primary session.
//!
//! ## Mid-turn escalation
//!
//! If the shift agent calls `task_list escalate` mid-turn, the shift's state
//! transitions to `AwaitingHuman` before the dispatch loop reads it. The
//! finalization path re-reads the shift state inside the transaction and skips
//! the `Complete` transition, ensuring consistency.

use std::sync::Arc;

use {
    moltis_config::schema::TasksConfig,
    moltis_service_traits::ServiceError,
    moltis_tasks::{
        AutonomyTier, HandoffContext, IntentStore, ObjectiveSnapshot, RuntimeState, Task, TaskSpec,
        TaskStore, TerminalState, TransitionError, TransitionEvent,
    },
    serde_json::Value,
    tracing::{debug, info, warn},
};

use crate::state::GatewayState;

// ── Public entry point ─────────────────────────────────────────────────────────

/// Start the intent dispatch loop as a long-running background task.
///
/// Spawns a `tokio::spawn`-able future. Returns immediately; the loop runs
/// until the process exits.
pub async fn run_dispatch_loop(
    state: Arc<GatewayState>,
    task_store: Arc<TaskStore>,
    config: TasksConfig,
) {
    let intent_store = IntentStore::from_pool(task_store.pool().clone());
    let poll_secs = config.dispatch_poll_interval_secs.max(1);

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_secs));
    interval.tick().await; // skip the first immediate tick

    loop {
        interval.tick().await;

        let chat = state.chat().await;
        let ctx = DispatchContext {
            task_store: Arc::clone(&task_store),
            intent_store: intent_store.clone(),
            chat,
            config: config.clone(),
        };

        match run_cycle(&ctx).await {
            Ok(n) if n > 0 => debug!(shifts_dispatched = n, "dispatch cycle complete"),
            Ok(_) => {},
            Err(e) => warn!(error = %e, "dispatch cycle error"),
        }
    }
}

// ── Internal types ────────────────────────────────────────────────────────────

struct DispatchContext {
    task_store: Arc<TaskStore>,
    intent_store: IntentStore,
    chat: Arc<dyn crate::services::ChatService>,
    config: TasksConfig,
}

// ── Cycle ─────────────────────────────────────────────────────────────────────

/// Run one full scan-and-dispatch cycle. Returns the number of shifts started.
async fn run_cycle(ctx: &DispatchContext) -> Result<usize, anyhow::Error> {
    let intents = ctx.task_store.list_actionable_intents().await?;
    let mut dispatched = 0usize;

    for intent in intents {
        let intent_id = intent.id.0.clone();
        match process_intent(ctx, intent).await {
            Ok(true) => dispatched += 1,
            Ok(false) => {},
            Err(e) => {
                warn!(intent_id = %intent_id, error = %e, "intent processing error — skipping");
            },
        }
    }

    Ok(dispatched)
}

// ── Intent processor ──────────────────────────────────────────────────────────

/// Process a single intent task. Returns `true` if a shift was dispatched.
async fn process_intent(ctx: &DispatchContext, intent: Task) -> Result<bool, anyhow::Error> {
    let list_id = intent.list_id.clone();
    let intent_id = intent.id.0.clone();

    // ── 1. Claim Pending intent → Active. ─────────────────────────────────────
    let intent = if matches!(intent.runtime.state, RuntimeState::Pending) {
        ctx.task_store
            .apply_transition(
                &list_id,
                &intent_id,
                None,
                &TransitionEvent::Claim {
                    owner: "dispatch".into(),
                    lease_duration_secs: None,
                },
            )
            .await?
    } else {
        intent
    };

    // ── 2. Classify child shifts to decide how to proceed. ────────────────────
    let shift = match classify_shifts(&ctx.task_store, &intent_id).await? {
        ShiftClassification::InProgress => {
            debug!(intent_id = %intent_id, "intent has an active/escalated shift — skipping");
            return Ok(false);
        },
        ShiftClassification::RetryPending => {
            debug!(
                intent_id = %intent_id,
                "intent has a retrying shift — waiting for retry timer"
            );
            return Ok(false);
        },
        ShiftClassification::RecoverPending(pending_shift) => {
            // Recovery path: a shift from a previous attempt was promoted back
            // to Pending by the retry-promotion sweep. Reclaim and re-run it
            // instead of creating a duplicate.
            info!(
                intent_id = %intent_id,
                shift_id = %pending_shift.id.0,
                "recovering pending shift from previous attempt"
            );
            pending_shift
        },
        ShiftClassification::NeedsNew => {
            // ── 3. Get or create IntentState. ─────────────────────────────────
            let intent_state = match ctx.intent_store.get(&intent_id).await? {
                Some(s) => s,
                None => {
                    ctx.intent_store
                        .create(
                            &intent_id,
                            &list_id,
                            ctx.config.intent_token_budget,
                            Some(ctx.config.intent_spin_threshold),
                        )
                        .await?
                },
            };

            // ── 4. Budget check before spawning shift. ────────────────────────
            if intent_state.is_over_budget() {
                escalate(ctx, &list_id, &intent_id, "Token budget exhausted").await?;
                return Ok(false);
            }

            // ── 5. Create new shift task. ──────────────────────────────────────
            // shift_count+1 gives the next shift number; used for subject only.
            let next_num = intent_state.shift_count + 1;
            let mut shift_spec = TaskSpec::new(
                format!("{}: shift {}", intent.spec.subject, next_num),
                "Dispatch-managed execution shift.",
            );
            shift_spec.parent_task = Some(intent.id.clone());
            ctx.task_store.create(&list_id, shift_spec, vec![]).await?
        },
    };

    // ── 3/5 (continued). Claim the shift (Pending → Active). ─────────────────
    // Applies whether the shift is freshly-created or recovered from a retry.
    ctx.task_store
        .apply_transition(
            &list_id,
            &shift.id.0,
            None,
            &TransitionEvent::Claim {
                owner: "dispatch".into(),
                lease_duration_secs: Some(ctx.config.lease_duration_secs),
            },
        )
        .await?;

    // ── 6. Get current IntentState (create if first shift) and register shift. ─
    // Read unconditionally: both `NeedsNew` and `RecoverPending` paths need it.
    let intent_state = match ctx.intent_store.get(&intent_id).await? {
        Some(s) => s,
        None => {
            ctx.intent_store
                .create(
                    &intent_id,
                    &list_id,
                    ctx.config.intent_token_budget,
                    Some(ctx.config.intent_spin_threshold),
                )
                .await?
        },
    };

    // shift_count is the number of shifts dispatched so far; +1 is this shift.
    let shift_num = intent_state.shift_count + 1;

    let intent_state = ctx
        .intent_store
        .set_active_shift(&intent_id, &shift.id.0, intent_state.version)
        .await?;

    info!(
        intent_id = %intent_id,
        shift_id = %shift.id.0,
        shift_num = shift_num,
        "dispatching shift"
    );

    // ── 7. Build shift session key and system prompt. ─────────────────────────
    let session_key = format!("dispatch:intent:{intent_id}:shift:{shift_num}");
    let prompt = build_shift_prompt(
        &intent.spec.subject,
        &intent.spec.description,
        &intent.runtime.handoff,
    );

    // ── 8. Run the shift — blocks until agent completes. ─────────────────────
    let deny_list = denied_tools_for_tier(intent.spec.autonomy_tier);
    let params = serde_json::json!({
        "text":                prompt,
        "_session_key":        session_key,
        "_source":             "dispatch",
        "_dispatch_tool_deny": deny_list,
    });

    let shift_result = ctx.chat.send_sync(params).await;

    // ── 9. Extract token usage (best-effort; 0 on parse failure). ─────────────
    let tokens_used = match &shift_result {
        Ok(v) => extract_tokens(v),
        Err(_) => 0,
    };

    // ── 10. Build structural snapshot for spin detection. ─────────────────────
    let new_snapshot = build_snapshot(ctx, &intent_id).await?;

    // ── 11. Atomic finalization. ──────────────────────────────────────────────
    let mut tx = ctx.task_store.begin_tx().await?;

    // Re-read shift state: agent may have escalated mid-turn.
    let shift_now = TaskStore::get_tx(&mut tx, &list_id, &shift.id.0)
        .await?
        .ok_or_else(|| TransitionError::NotFound(shift.id.0.clone()))?;

    if matches!(shift_now.runtime.state, RuntimeState::Active { .. }) {
        // Normal path: shift ran to completion.
        TaskStore::apply_transition_tx(
            &mut tx,
            &list_id,
            &shift.id.0,
            None,
            &TransitionEvent::Complete,
        )
        .await?;
    }
    // If shift is already terminal or AwaitingHuman (mid-turn escalation), skip.

    let (final_state, is_spinning) = IntentStore::finalize_shift_tx(
        &mut tx,
        &intent_id,
        new_snapshot,
        tokens_used,
        intent_state.version,
    )
    .await?;

    tx.commit()
        .await
        .map_err(|e| anyhow::anyhow!("finalize commit: {e}"))?;

    // ── 12. Log shift result. ─────────────────────────────────────────────────
    match &shift_result {
        Ok(_) => info!(
            intent_id = %intent_id,
            shift_num = shift_num,
            tokens_used = tokens_used,
            spin_count = final_state.spin_count,
            "shift completed"
        ),
        Err(e) => warn!(
            intent_id = %intent_id,
            shift_num = shift_num,
            error = %e,
            "shift agent returned error"
        ),
    }

    // ── 13. Post-finalization escalation checks. ──────────────────────────────
    if is_spinning {
        escalate(
            ctx,
            &list_id,
            &intent_id,
            "No measurable progress after consecutive shifts",
        )
        .await?;
    } else if final_state.is_over_budget() {
        escalate(
            ctx,
            &list_id,
            &intent_id,
            "Token budget exhausted after shift",
        )
        .await?;
    } else if let Err(ServiceError::Message { message }) = &shift_result {
        // Permanent provider failure → escalate immediately.
        if message.contains("billing") || message.contains("invalid_api_key") {
            escalate(ctx, &list_id, &intent_id, message).await?;
        }
    }

    Ok(true)
}

// ── Recovery: shift classification ────────────────────────────────────────────

/// Outcome of classifying a set of child shifts for dispatch purposes.
enum ShiftClassification {
    /// A shift is currently Active or AwaitingHuman — skip this cycle.
    InProgress,
    /// A shift is in Retrying state — wait for the retry timer to fire.
    RetryPending,
    /// An existing Pending shift (from a previous retry) is ready to reclaim.
    RecoverPending(Task),
    /// No non-terminal children — dispatch loop should create a new shift.
    NeedsNew,
}

/// Inspect all child shifts of an intent and decide what the dispatch loop
/// should do.
///
/// Priority:
/// 1. Any Active / AwaitingHuman → `InProgress` (wait, don't double-dispatch)
/// 2. Any Retrying → `RetryPending` (retry timer hasn't fired yet)
/// 3. Any Pending → `RecoverPending` (reclaim and re-run the existing shift)
/// 4. Otherwise → `NeedsNew` (all children are terminal)
async fn classify_shifts(
    store: &Arc<TaskStore>,
    intent_id: &str,
) -> Result<ShiftClassification, anyhow::Error> {
    let children = store.list_shifts_for_intent(intent_id).await?;

    for child in &children {
        match &child.runtime.state {
            RuntimeState::Active { .. } | RuntimeState::AwaitingHuman { .. } => {
                return Ok(ShiftClassification::InProgress);
            },
            RuntimeState::Retrying { .. } => {
                // Keep scanning — an Active shift takes priority over Retrying.
                continue;
            },
            _ => {},
        }
    }

    // Second pass: check for Retrying (after confirming no Active/AwaitingHuman).
    for child in &children {
        if matches!(child.runtime.state, RuntimeState::Retrying { .. }) {
            return Ok(ShiftClassification::RetryPending);
        }
    }

    // Third pass: look for an existing Pending shift to recover.
    for child in children {
        if matches!(child.runtime.state, RuntimeState::Pending) {
            return Ok(ShiftClassification::RecoverPending(child));
        }
    }

    Ok(ShiftClassification::NeedsNew)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return the list of tool names to deny for a shift running at `tier`.
///
/// Tools are classified into three tiers:
/// - `Auto`    (0): read-only — web_fetch, web_search, calc, memory reads.
/// - `Confirm` (1): local-write — exec, skill management, sandbox, sessions manage.
/// - `Approve` (2): external-write — send_image, sessions_send, cron writes, spawn_agent.
///
/// Shifts are denied all tools *above* the intent's tier. `task_list` is always
/// allowed so the shift agent can signal completion or escalation.
fn denied_tools_for_tier(tier: AutonomyTier) -> Vec<&'static str> {
    match tier {
        AutonomyTier::Auto => vec![
            // Confirm-tier (local-write)
            "exec",
            "create_skill",
            "update_skill",
            "delete_skill",
            "sessions_create",
            "sessions_delete",
            "sandbox_packages",
            // Approve-tier (external-write)
            "send_image",
            "sessions_send",
            "spawn_agent",
            "cron",
            "process",
        ],
        AutonomyTier::Confirm => vec![
            // Approve-tier only (external-write)
            "send_image",
            "sessions_send",
            "spawn_agent",
            "cron",
            "process",
        ],
        AutonomyTier::Approve => vec![],
    }
}

/// Escalate an intent to `AwaitingHuman` with a reason.
async fn escalate(
    ctx: &DispatchContext,
    list_id: &str,
    intent_id: &str,
    reason: &str,
) -> Result<(), TransitionError> {
    info!(intent_id = %intent_id, reason = reason, "escalating intent to AwaitingHuman");
    ctx.task_store
        .apply_transition(
            list_id,
            intent_id,
            None,
            &TransitionEvent::Escalate {
                question: reason.to_string(),
                handoff: HandoffContext::default(),
            },
        )
        .await
        .map(|_| ())
}

/// Build the system prompt injected into the shift session.
fn build_shift_prompt(
    subject: &str,
    description: &str,
    handoff: &Option<HandoffContext>,
) -> String {
    let mut parts = Vec::with_capacity(4);

    parts.push(format!("## Objective\n{subject}"));

    if !description.is_empty() {
        parts.push(format!("## Details\n{description}"));
    }

    if let Some(h) = handoff {
        let ctx = h.as_prompt_context();
        if !ctx.is_empty() {
            parts.push(format!("## Previous Attempt Context\n{ctx}"));
        }
    }

    parts.push(
        "When the objective is fully complete, call `task_list` with \
         `action: complete` on this intent task ID to close the dispatch loop."
            .into(),
    );

    parts.join("\n\n")
}

/// Build an [`ObjectiveSnapshot`] by counting the intent's child shift tasks.
async fn build_snapshot(
    ctx: &DispatchContext,
    intent_id: &str,
) -> Result<ObjectiveSnapshot, TransitionError> {
    let children = ctx.task_store.list_shifts_for_intent(intent_id).await?;
    let mut snapshot = ObjectiveSnapshot::default();

    for child in &children {
        match &child.runtime.state {
            RuntimeState::Pending => snapshot.child_pending += 1,
            RuntimeState::Active { .. } => snapshot.child_active += 1,
            RuntimeState::Terminal(ts) => match ts {
                TerminalState::Completed => snapshot.child_completed += 1,
                TerminalState::Failed { .. } | TerminalState::Canceled { .. } => {
                    snapshot.child_failed += 1
                },
            },
            RuntimeState::Retrying { .. } | RuntimeState::AwaitingHuman { .. } => {},
            RuntimeState::Blocked { .. } => {},
        }
    }

    Ok(snapshot)
}

/// Extract total tokens used from a `send_sync` response.
/// Returns 0 if the fields are absent or not numeric.
fn extract_tokens(result: &Value) -> u64 {
    let input = result
        .get("inputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = result
        .get("outputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    input.saturating_add(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use {crate::services::ServiceResult, moltis_tasks::FailureClass};

    // ── classify_shifts ───────────────────────────────────────────────────

    async fn make_store() -> Arc<TaskStore> {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let db_path = dir.path().join("tasks").join("tasks.db");
        let store = TaskStore::open(&db_path).await.expect("open store");
        // Keep dir alive by leaking (acceptable in tests).
        std::mem::forget(dir);
        Arc::new(store)
    }

    #[tokio::test]
    async fn classify_shifts_needs_new_when_no_children() {
        let store = make_store().await;
        let result = classify_shifts(&store, "no-such-intent")
            .await
            .expect("classify");
        assert!(matches!(result, ShiftClassification::NeedsNew));
    }

    #[tokio::test]
    async fn classify_shifts_in_progress_when_active_child() {
        let store = make_store().await;

        // Create an intent and an Active shift.
        let intent_spec = {
            let mut s = TaskSpec::new("intent", "desc");
            s.is_intent = true;
            s
        };
        let intent = store
            .create("default", intent_spec, vec![])
            .await
            .expect("create intent");
        store
            .apply_transition(
                "default",
                &intent.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "dispatch".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim intent");

        let mut shift_spec = TaskSpec::new("shift 1", "");
        shift_spec.parent_task = Some(intent.id.clone());
        let shift = store
            .create("default", shift_spec, vec![])
            .await
            .expect("create shift");
        store
            .apply_transition(
                "default",
                &shift.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "dispatch".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim shift");

        let result = classify_shifts(&store, &intent.id.0)
            .await
            .expect("classify");
        assert!(matches!(result, ShiftClassification::InProgress));
    }

    #[tokio::test]
    async fn classify_shifts_recover_pending_after_retry_promotion() {
        let store = make_store().await;

        // Create an intent.
        let intent_spec = {
            let mut s = TaskSpec::new("intent", "");
            s.is_intent = true;
            s
        };
        let intent = store
            .create("default", intent_spec, vec![])
            .await
            .expect("create intent");
        store
            .apply_transition(
                "default",
                &intent.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "dispatch".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim intent");

        // Create a shift, claim it, then fail it (→ Retrying).
        let mut shift_spec = TaskSpec::new("shift 1", "");
        shift_spec.parent_task = Some(intent.id.clone());
        let shift = store
            .create("default", shift_spec, vec![])
            .await
            .expect("create shift");
        store
            .apply_transition(
                "default",
                &shift.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "dispatch".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim shift");
        store
            .apply_transition(
                "default",
                &shift.id.0,
                None,
                &TransitionEvent::Fail {
                    class: FailureClass::AgentError,
                    handoff: HandoffContext::default(),
                    retry_after: None,
                },
            )
            .await
            .expect("fail shift");

        // While Retrying → dispatch should wait.
        let result = classify_shifts(&store, &intent.id.0)
            .await
            .expect("classify");
        assert!(
            matches!(result, ShiftClassification::RetryPending),
            "expected RetryPending while shift is Retrying"
        );

        // Promote back to Pending (simulates retry-promotion sweep).
        store
            .apply_transition("default", &shift.id.0, None, &TransitionEvent::PromoteRetry)
            .await
            .expect("promote retry");

        // Now dispatch should recover the Pending shift.
        let result = classify_shifts(&store, &intent.id.0)
            .await
            .expect("classify after promotion");
        assert!(
            matches!(result, ShiftClassification::RecoverPending(_)),
            "expected RecoverPending after promotion"
        );
    }

    #[tokio::test]
    async fn classify_shifts_needs_new_when_all_terminal() {
        let store = make_store().await;

        let intent_spec = {
            let mut s = TaskSpec::new("intent", "");
            s.is_intent = true;
            s
        };
        let intent = store
            .create("default", intent_spec, vec![])
            .await
            .expect("create intent");
        store
            .apply_transition(
                "default",
                &intent.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "dispatch".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim intent");

        // Create and complete a shift.
        let mut shift_spec = TaskSpec::new("shift 1", "");
        shift_spec.parent_task = Some(intent.id.clone());
        let shift = store
            .create("default", shift_spec, vec![])
            .await
            .expect("create shift");
        store
            .apply_transition(
                "default",
                &shift.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "dispatch".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim shift");
        store
            .apply_transition("default", &shift.id.0, None, &TransitionEvent::Complete)
            .await
            .expect("complete shift");

        let result = classify_shifts(&store, &intent.id.0)
            .await
            .expect("classify");
        assert!(
            matches!(result, ShiftClassification::NeedsNew),
            "expected NeedsNew when all shifts are terminal"
        );
    }

    // ── denied_tools_for_tier ──────────────────────────────────────────────

    #[test]
    fn auto_tier_denies_exec_and_send() {
        let denied = denied_tools_for_tier(AutonomyTier::Auto);
        assert!(denied.contains(&"exec"), "exec must be denied at Auto tier");
        assert!(
            denied.contains(&"send_image"),
            "send_image must be denied at Auto tier"
        );
        assert!(
            denied.contains(&"spawn_agent"),
            "spawn_agent must be denied at Auto tier"
        );
        assert!(
            !denied.contains(&"task_list"),
            "task_list must never be denied"
        );
    }

    #[test]
    fn confirm_tier_denies_only_approve_tools() {
        let denied = denied_tools_for_tier(AutonomyTier::Confirm);
        assert!(
            denied.contains(&"send_image"),
            "send_image must be denied at Confirm tier"
        );
        assert!(
            denied.contains(&"spawn_agent"),
            "spawn_agent must be denied at Confirm tier"
        );
        assert!(
            !denied.contains(&"exec"),
            "exec must be allowed at Confirm tier"
        );
        assert!(
            !denied.contains(&"task_list"),
            "task_list must never be denied"
        );
    }

    #[test]
    fn approve_tier_denies_nothing() {
        let denied = denied_tools_for_tier(AutonomyTier::Approve);
        assert!(denied.is_empty(), "Approve tier must deny no tools");
    }

    // ── extract_tokens ────────────────────────────────────────────────────

    #[test]
    fn extract_tokens_both_fields() {
        let v = serde_json::json!({ "inputTokens": 100, "outputTokens": 200 });
        assert_eq!(extract_tokens(&v), 300);
    }

    #[test]
    fn extract_tokens_missing_fields() {
        let v = serde_json::json!({ "text": "ok" });
        assert_eq!(extract_tokens(&v), 0);
    }

    #[test]
    fn extract_tokens_partial() {
        let v = serde_json::json!({ "inputTokens": 50 });
        assert_eq!(extract_tokens(&v), 50);
    }

    #[test]
    fn build_shift_prompt_no_handoff() {
        let p = build_shift_prompt("Find restaurants", "Near London", &None);
        assert!(p.contains("Find restaurants"));
        assert!(p.contains("Near London"));
        assert!(p.contains("task_list"));
        assert!(!p.contains("Previous Attempt"));
    }

    #[test]
    fn build_shift_prompt_with_handoff() {
        let h = HandoffContext {
            last_action: "searched Google".into(),
            observed_error: "403 forbidden".into(),
            dead_ends: vec![],
            suggested_next_step: "try Bing".into(),
        };
        let p = build_shift_prompt("Find restaurants", "", &Some(h));
        assert!(p.contains("searched Google"));
        assert!(p.contains("403 forbidden"));
        assert!(p.contains("try Bing"));
    }

    #[test]
    fn build_shift_prompt_empty_description_omitted() {
        let p = build_shift_prompt("subject only", "", &None);
        assert!(!p.contains("## Details"));
    }

    // ── E2E: full dispatch cycle ──────────────────────────────────────────────

    use async_trait::async_trait;

    struct OkChatService;

    #[async_trait]
    impl crate::services::ChatService for OkChatService {
        async fn send(&self, _p: Value) -> ServiceResult {
            Ok(serde_json::json!({ "inputTokens": 100, "outputTokens": 50 }))
        }
        async fn abort(&self, _p: Value) -> ServiceResult {
            Ok(serde_json::json!({}))
        }
        async fn cancel_queued(&self, _p: Value) -> ServiceResult {
            Ok(serde_json::json!({ "cleared": 0 }))
        }
        async fn history(&self, _p: Value) -> ServiceResult {
            Ok(serde_json::json!([]))
        }
        async fn inject(&self, _p: Value) -> ServiceResult {
            Ok(serde_json::json!({}))
        }
        async fn clear(&self, _p: Value) -> ServiceResult {
            Ok(serde_json::json!({ "ok": true }))
        }
        async fn compact(&self, _p: Value) -> ServiceResult {
            Ok(serde_json::json!({}))
        }
        async fn context(&self, _p: Value) -> ServiceResult {
            Ok(serde_json::json!({}))
        }
        async fn raw_prompt(&self, _p: Value) -> ServiceResult {
            Ok(serde_json::json!({ "prompt": "" }))
        }
        async fn full_context(&self, _p: Value) -> ServiceResult {
            Ok(serde_json::json!([]))
        }
    }

    #[tokio::test]
    async fn run_cycle_claims_pending_intent_and_creates_shift() {
        let store = make_store().await;

        // Create a pending intent task.
        let mut spec = TaskSpec::new("E2E intent", "do some work");
        spec.is_intent = true;
        let intent = store
            .create("default", spec, vec![])
            .await
            .expect("create intent");

        let intent_store = IntentStore::from_pool(store.pool().clone());
        let ctx = DispatchContext {
            task_store: Arc::clone(&store),
            intent_store,
            chat: Arc::new(OkChatService),
            config: TasksConfig::default(),
        };

        let dispatched = run_cycle(&ctx).await.expect("run_cycle");
        assert_eq!(dispatched, 1, "one intent should have been dispatched");

        // Intent should be Active now.
        let updated_intent = store
            .get("default", &intent.id.0)
            .await
            .expect("get intent")
            .expect("intent exists");
        assert!(
            matches!(updated_intent.runtime.state, RuntimeState::Active { .. }),
            "intent should be Active after dispatch"
        );

        // A child shift should have been created and completed.
        let shifts = store
            .list_shifts_for_intent(&intent.id.0)
            .await
            .expect("list shifts");
        assert_eq!(shifts.len(), 1, "exactly one shift created");
        assert!(
            matches!(
                shifts[0].runtime.state,
                RuntimeState::Terminal(TerminalState::Completed)
            ),
            "shift should be Completed; got {:?}",
            shifts[0].runtime.state
        );
    }

    #[tokio::test]
    async fn run_cycle_skips_intent_with_active_shift() {
        let store = make_store().await;

        // Create intent and manually give it an active child shift.
        let mut spec = TaskSpec::new("double-dispatch test", "");
        spec.is_intent = true;
        let intent = store
            .create("default", spec, vec![])
            .await
            .expect("create intent");
        store
            .apply_transition(
                "default",
                &intent.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "dispatch".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim intent");

        let mut shift_spec = TaskSpec::new("shift 1", "");
        shift_spec.parent_task = Some(intent.id.clone());
        let shift = store
            .create("default", shift_spec, vec![])
            .await
            .expect("create shift");
        store
            .apply_transition(
                "default",
                &shift.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "dispatch".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim shift");

        let intent_store = IntentStore::from_pool(store.pool().clone());
        let ctx = DispatchContext {
            task_store: Arc::clone(&store),
            intent_store,
            chat: Arc::new(OkChatService),
            config: TasksConfig::default(),
        };

        let dispatched = run_cycle(&ctx).await.expect("run_cycle");
        assert_eq!(
            dispatched, 0,
            "should skip intent that already has an active shift"
        );
    }

    #[tokio::test]
    async fn run_cycle_empty_store_dispatches_nothing() {
        let store = make_store().await;
        let intent_store = IntentStore::from_pool(store.pool().clone());
        let ctx = DispatchContext {
            task_store: Arc::clone(&store),
            intent_store,
            chat: Arc::new(OkChatService),
            config: TasksConfig::default(),
        };
        let dispatched = run_cycle(&ctx).await.expect("run_cycle");
        assert_eq!(dispatched, 0);
    }
}
