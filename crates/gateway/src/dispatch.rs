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
    futures::future::join_all,
    moltis_agents::runner::classify_error as classify_shift_error,
    moltis_config::schema::TasksConfig,
    moltis_tasks::{
        AutonomyTier, FailureClass, HandoffContext, IntentStore, ObjectiveSnapshot, OutputStore,
        RuntimeState, ShiftOutput, Task, TaskId, TaskSpec, TaskStore, TerminalState,
        TransitionError, TransitionEvent,
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
    if !config.dispatch_enabled {
        debug!("dispatch loop disabled (tasks.dispatch_enabled = false) — not starting");
        return;
    }

    let intent_store = IntentStore::from_pool(task_store.pool().clone());
    let output_store = OutputStore::from_pool(task_store.pool().clone());
    let poll_secs = config.dispatch_poll_interval_secs.max(1);

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_secs));
    interval.tick().await; // skip the first immediate tick

    loop {
        interval.tick().await;

        let chat = state.chat().await;
        let ctx = DispatchContext {
            task_store: Arc::clone(&task_store),
            intent_store: intent_store.clone(),
            output_store: output_store.clone(),
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

#[derive(Clone)]
struct DispatchContext {
    task_store: Arc<TaskStore>,
    intent_store: IntentStore,
    output_store: OutputStore,
    chat: Arc<dyn crate::services::ChatService>,
    config: TasksConfig,
}

// ── Cycle ─────────────────────────────────────────────────────────────────────

/// Run one full scan-and-dispatch cycle. Returns the number of shifts started.
async fn run_cycle(ctx: &DispatchContext) -> Result<usize, anyhow::Error> {
    // Hard cap on concurrent shift execution before querying intents.
    let active = ctx.task_store.count_active_shifts().await?;
    if active >= ctx.config.max_concurrent_shifts {
        debug!(
            active = active,
            max = ctx.config.max_concurrent_shifts,
            "max_concurrent_shifts reached — skipping cycle"
        );
        return Ok(0);
    }

    let capacity = ctx.config.max_concurrent_shifts.saturating_sub(active);
    if capacity == 0 {
        return Ok(0);
    }

    let intents = ctx
        .task_store
        .list_actionable_intents()
        .await?
        .into_iter()
        .take(capacity);

    let jobs = intents.map(|intent| {
        let local_ctx = ctx.clone();
        let intent_id = intent.id.0.clone();
        tokio::spawn(async move { (intent_id, process_intent(&local_ctx, intent).await) })
    });

    let mut dispatched = 0usize;
    for joined in join_all(jobs).await {
        match joined {
            Ok((_, Ok(true))) => dispatched += 1,
            Ok((_, Ok(false))) => {},
            Ok((intent_id, Err(e))) => {
                warn!(intent_id = %intent_id, error = %e, "intent processing error — skipping");
            },
            Err(e) => warn!(error = %e, "intent processing task join error"),
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

    // ── 2. Load (or initialize) mutable intent state. ─────────────────────────
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

    // ── 3. Classify child shifts and decide whether to recover or create. ────
    let (shift, slot_state, shift_num, shift_was_new) =
        match classify_shifts(&ctx.task_store, &intent_id).await? {
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
                let shift = *pending_shift;
                let shift_num = intent_state.shift_count.saturating_add(1);
                let slot = match ctx
                    .intent_store
                    .set_active_shift(&intent_id, &shift.id.0, intent_state.version)
                    .await
                {
                    Ok(s) => s,
                    Err(TransitionError::VersionConflict { .. }) => return Ok(false),
                    Err(e) => return Err(e.into()),
                };
                (shift, slot, shift_num, false)
            },
            ShiftClassification::NeedsNew => {
                if intent_state.is_over_budget() {
                    if let Err(err) =
                        escalate(ctx, &list_id, &intent_id, "Token budget exhausted").await
                    {
                        warn!(
                            intent_id = %intent_id,
                            error = %err,
                            "failed to escalate over-budget intent"
                        );
                    }
                    return Ok(false);
                }

                // Cooldown: don't start a new shift too soon after the last one.
                if ctx.config.shift_cooldown_secs > 0 && intent_state.shift_count > 0 {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let elapsed = (now - intent_state.updated_at.unix_timestamp()).max(0) as u64;
                    if elapsed < ctx.config.shift_cooldown_secs {
                        debug!(
                            intent_id = %intent_id,
                            elapsed_secs = elapsed,
                            cooldown_secs = ctx.config.shift_cooldown_secs,
                            "shift cooldown active — skipping"
                        );
                        return Ok(false);
                    }
                }

                let shift_num = intent_state.shift_count.saturating_add(1);
                let shift_id = TaskId::new();
                let slot = match ctx
                    .intent_store
                    .set_active_shift(&intent_id, &shift_id.0, intent_state.version)
                    .await
                {
                    Ok(s) => s,
                    Err(TransitionError::VersionConflict { .. }) => return Ok(false),
                    Err(e) => return Err(e.into()),
                };

                let mut shift_spec = TaskSpec::new(
                    format!("{}: shift {}", intent.spec.subject, shift_num),
                    "Dispatch-managed execution shift.",
                );
                shift_spec.parent_task = Some(intent.id.clone());
                shift_spec.principal = intent.spec.principal.clone();
                let created = match ctx
                    .task_store
                    .create_with_id(&list_id, shift_id.clone(), shift_spec, vec![])
                    .await
                {
                    Ok(task) => task,
                    Err(e) => {
                        let _ = ctx
                            .intent_store
                            .clear_active_shift(&intent_id, &shift_id.0, slot.version)
                            .await;
                        return Err(e.into());
                    },
                };
                (created, slot, shift_num, true)
            },
        };

    // ── 4. Claim the shift task (Pending → Active). ───────────────────────────
    if let Err(err) = ctx
        .task_store
        .apply_transition(
            &list_id,
            &shift.id.0,
            None,
            &TransitionEvent::Claim {
                owner: "dispatch".into(),
                lease_duration_secs: Some(ctx.config.lease_duration_secs),
            },
        )
        .await
    {
        let _ = ctx
            .intent_store
            .clear_active_shift(&intent_id, &shift.id.0, slot_state.version)
            .await;
        if shift_was_new {
            cancel_shift_best_effort(&ctx.task_store, &list_id, &shift.id.0).await;
        }
        if matches!(err, TransitionError::VersionConflict { .. }) {
            return Ok(false);
        }
        return Err(err.into());
    }

    info!(
        intent_id = %intent_id,
        shift_id = %shift.id.0,
        shift_num = shift_num,
        "dispatching shift"
    );

    // ── 7. Load recent outputs and build shift session key + prompt. ──────────
    let recent_outputs = ctx
        .output_store
        .list_recent(&intent_id, 2)
        .await
        .unwrap_or_default();

    let session_key = format!("dispatch:intent:{intent_id}:shift:{shift_num}");
    let prompt = build_shift_prompt(
        &intent_id,
        &intent.spec.subject,
        &intent.spec.description,
        &intent.runtime.handoff,
        &recent_outputs,
    );

    // ── 8. Run the shift — blocks until agent completes. ─────────────────────
    let deny_list = denied_tools_for_tier(intent.spec.autonomy_tier);
    let params = serde_json::json!({
        "text":                prompt,
        "_session_key":        session_key,
        "_source":             "dispatch",
        "_dispatch_tool_deny": deny_list,
    });

    let heartbeat = spawn_shift_heartbeat(
        Arc::clone(&ctx.task_store),
        list_id.clone(),
        shift.id.0.clone(),
        ctx.config.lease_duration_secs,
        ctx.config.lease_heartbeat_interval_secs,
    );

    let shift_result = ctx.chat.send_sync(params).await;
    heartbeat.abort();

    let finalized = async {
        // ── 9. Extract token usage (best-effort; 0 on parse failure). ─────────
        let tokens_used = match &shift_result {
            Ok(v) => extract_tokens(v),
            Err(_) => 0,
        };

        // ── 10. Build structural snapshot for spin detection. ─────────────────
        let new_snapshot = build_snapshot(ctx, &intent_id).await?;

        // ── 11. Atomic finalization. ──────────────────────────────────────────
        let mut tx = ctx.task_store.begin_tx().await?;

        // Re-read shift state: agent may have escalated mid-turn.
        let shift_now = TaskStore::get_tx(&mut tx, &list_id, &shift.id.0)
            .await?
            .ok_or_else(|| TransitionError::NotFound(shift.id.0.clone()))?;

        let mut failure_class: Option<FailureClass> = None;
        if matches!(shift_now.runtime.state, RuntimeState::Active { .. }) {
            match &shift_result {
                Ok(_) => {
                    TaskStore::apply_transition_tx(
                        &mut tx,
                        &list_id,
                        &shift.id.0,
                        None,
                        &TransitionEvent::Complete,
                    )
                    .await?;
                },
                Err(err) => {
                    let class = classify_shift_error(&err.to_string());
                    failure_class = Some(class.clone());
                    TaskStore::apply_transition_tx(
                        &mut tx,
                        &list_id,
                        &shift.id.0,
                        None,
                        &TransitionEvent::Fail {
                            class,
                            handoff: HandoffContext {
                                observed_error: err.to_string(),
                                ..HandoffContext::default()
                            },
                            retry_after: None,
                        },
                    )
                    .await?;
                },
            }
        }
        // If shift is already terminal or AwaitingHuman (mid-turn escalation), skip transition.

        // Persist shift output for future context injection.
        let output_text = match &shift_result {
            Ok(v) => v
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            Err(e) => format!("[shift error: {e}]"),
        };
        let (input_toks, output_toks) = match &shift_result {
            Ok(v) => extract_token_pair(v),
            Err(_) => (0, 0),
        };
        OutputStore::insert_tx(
            &mut tx,
            &intent_id,
            &shift.id.0,
            &list_id,
            shift_num,
            &output_text,
            input_toks,
            output_toks,
        )
        .await?;

        let (final_state, is_spinning) = IntentStore::finalize_shift_tx(
            &mut tx,
            &intent_id,
            &shift.id.0,
            new_snapshot,
            tokens_used,
            slot_state.version,
        )
        .await?;

        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("finalize commit: {e}"))?;

        Ok::<_, anyhow::Error>((tokens_used, failure_class, final_state, is_spinning))
    }
    .await;

    let (tokens_used, failure_class, final_state, is_spinning) = match finalized {
        Ok(result) => result,
        Err(err) => {
            clear_slot_best_effort(ctx, &intent_id, &shift.id.0, slot_state.version).await;
            return Err(err);
        },
    };

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
    let escalation_reason = if let Some(class) = &failure_class {
        if class.requires_human() {
            Some("Shift execution requires human intervention")
        } else {
            None
        }
    } else if is_spinning {
        Some("No measurable progress after consecutive shifts")
    } else if final_state.is_over_budget() {
        Some("Token budget exhausted after shift")
    } else {
        None
    };

    if let Some(reason) = escalation_reason
        && let Err(err) = escalate(ctx, &list_id, &intent_id, reason).await
    {
        warn!(
            intent_id = %intent_id,
            reason = reason,
            error = %err,
            "failed to escalate intent after shift finalization"
        );
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
    RecoverPending(Box<Task>),
    /// No non-terminal children — dispatch loop should create a new shift.
    NeedsNew,
}

/// Inspect all child shifts of an intent and decide what the dispatch loop
/// should do.
///
/// Priority:
/// 1. Any Active / AwaitingHuman / Blocked → `InProgress` (wait, don't double-dispatch)
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
            RuntimeState::Active { .. }
            | RuntimeState::AwaitingHuman { .. }
            | RuntimeState::Blocked { .. } => {
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
            return Ok(ShiftClassification::RecoverPending(Box::new(child)));
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
    match ctx
        .task_store
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
    {
        Ok(_) | Err(TransitionError::InvalidTransition { .. }) => Ok(()),
        Err(TransitionError::VersionConflict { .. }) => Ok(()),
        Err(err) => Err(err),
    }
}

fn spawn_shift_heartbeat(
    task_store: Arc<TaskStore>,
    list_id: String,
    shift_id: String,
    lease_duration_secs: u64,
    heartbeat_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let heartbeat = heartbeat_secs.max(1);
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(heartbeat));
        interval.tick().await; // skip immediate tick
        loop {
            interval.tick().await;
            let renewed = task_store
                .apply_transition(
                    &list_id,
                    &shift_id,
                    None,
                    &TransitionEvent::renew_lease(lease_duration_secs),
                )
                .await;
            if renewed.is_err() {
                break;
            }
        }
    })
}

async fn cancel_shift_best_effort(task_store: &TaskStore, list_id: &str, shift_id: &str) {
    match task_store.get(list_id, shift_id).await {
        Ok(Some(task)) if !task.runtime.state.is_terminal() => {
            let _ = task_store
                .apply_transition(
                    list_id,
                    shift_id,
                    None,
                    &TransitionEvent::Cancel {
                        reason: "dispatch slot claim failed".to_string(),
                    },
                )
                .await;
        },
        _ => {},
    }
}

async fn clear_slot_best_effort(
    ctx: &DispatchContext,
    intent_id: &str,
    shift_id: &str,
    expected_version: u64,
) {
    if let Err(err) = ctx
        .intent_store
        .clear_active_shift(intent_id, shift_id, expected_version)
        .await
    {
        warn!(
            intent_id = %intent_id,
            shift_id = %shift_id,
            error = %err,
            "failed to clear active shift slot during dispatch cleanup"
        );
    }
}

/// Build the system prompt injected into the shift session.
fn build_shift_prompt(
    intent_id: &str,
    subject: &str,
    description: &str,
    handoff: &Option<HandoffContext>,
    recent_outputs: &[ShiftOutput],
) -> String {
    let mut parts = Vec::with_capacity(5);

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

    if !recent_outputs.is_empty() {
        let mut history = String::from("The following outputs were produced by previous shifts:\n");
        for out in recent_outputs {
            let preview = if out.output.len() > 2_000 {
                format!("{}…", &out.output[..2_000])
            } else {
                out.output.clone()
            };
            history.push_str(&format!(
                "\n### Shift {} output\n{preview}\n",
                out.shift_num
            ));
        }
        parts.push(format!("## Prior Shift History\n{history}"));
    }

    parts.push(format!(
        "When the objective is fully complete, call `task_list` with \
             `action: complete` and `id: \"{intent_id}\"` to close the dispatch loop."
    ));

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

/// Extract `(input_tokens, output_tokens)` from a `send_sync` response.
/// Returns `(0, 0)` if the fields are absent or not numeric.
fn extract_token_pair(result: &Value) -> (u64, u64) {
    let input = result
        .get("inputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = result
        .get("outputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (input, output)
}

/// Extract total tokens used from a `send_sync` response.
/// Returns 0 if the fields are absent or not numeric.
fn extract_tokens(result: &Value) -> u64 {
    let (i, o) = extract_token_pair(result);
    i.saturating_add(o)
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
    async fn classify_shifts_in_progress_when_blocked_child() {
        let store = make_store().await;

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

        // Create dependency so we can move shift -> Blocked.
        let dep = store
            .create("default", TaskSpec::new("dep", ""), vec![])
            .await
            .expect("create dep");

        let mut shift_spec = TaskSpec::new("shift 1", "");
        shift_spec.parent_task = Some(intent.id.clone());
        let shift = store
            .create("default", shift_spec, vec![dep.id.clone()])
            .await
            .expect("create shift");
        store
            .apply_transition(
                "default",
                &shift.id.0,
                None,
                &TransitionEvent::Block {
                    waiting_on: vec![dep.id.clone()],
                },
            )
            .await
            .expect("block shift");

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
        let p = build_shift_prompt("intent-123", "Find restaurants", "Near London", &None, &[]);
        assert!(p.contains("Find restaurants"));
        assert!(p.contains("Near London"));
        assert!(p.contains("task_list"));
        assert!(p.contains("intent-123"));
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
        let p = build_shift_prompt("intent-123", "Find restaurants", "", &Some(h), &[]);
        assert!(p.contains("searched Google"));
        assert!(p.contains("403 forbidden"));
        assert!(p.contains("try Bing"));
    }

    #[test]
    fn build_shift_prompt_empty_description_omitted() {
        let p = build_shift_prompt("intent-123", "subject only", "", &None, &[]);
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
        let output_store = OutputStore::from_pool(store.pool().clone());
        let ctx = DispatchContext {
            task_store: Arc::clone(&store),
            intent_store,
            output_store,
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
        let output_store = OutputStore::from_pool(store.pool().clone());
        let ctx = DispatchContext {
            task_store: Arc::clone(&store),
            intent_store,
            output_store,
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
        let output_store = OutputStore::from_pool(store.pool().clone());
        let ctx = DispatchContext {
            task_store: Arc::clone(&store),
            intent_store,
            output_store,
            chat: Arc::new(OkChatService),
            config: TasksConfig::default(),
        };
        let dispatched = run_cycle(&ctx).await.expect("run_cycle");
        assert_eq!(dispatched, 0);
    }
}
