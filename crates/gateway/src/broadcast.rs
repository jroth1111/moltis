use std::{collections::HashMap, sync::Arc};

use {
    moltis_protocol::{EventFrame, StateVersion, scopes},
    tracing::{debug, warn},
};

pub use crate::broadcast_types::BroadcastEvent;
use crate::{
    broadcast_types::{TickMemoryPayload, TickPayload},
    state::GatewayState,
};

// ── Scope guards ─────────────────────────────────────────────────────────────

/// Events that require specific scopes to receive.
///
/// **Maintenance note:** when adding a new `BroadcastEvent` variant that maps
/// to a scope-restricted event name, you must add a corresponding entry here.
/// The `scope_guard_table_covers_sensitive_variants` test verifies coverage.
fn event_scope_guards() -> HashMap<&'static str, &'static [&'static str]> {
    let mut m = HashMap::new();
    m.insert("exec.approval.requested", [scopes::APPROVALS].as_slice());
    m.insert("exec.approval.resolved", [scopes::APPROVALS].as_slice());
    m.insert("device.pair.requested", [scopes::PAIRING].as_slice());
    m.insert("device.pair.resolved", [scopes::PAIRING].as_slice());
    m.insert("node.pair.requested", [scopes::PAIRING].as_slice());
    m.insert("node.pair.resolved", [scopes::PAIRING].as_slice());
    m
}

// ── Broadcast options ────────────────────────────────────────────────────────

#[derive(Default)]
pub struct BroadcastOpts {
    pub drop_if_slow: bool,
    pub state_version: Option<StateVersion>,
    /// Stream group ID for chunked delivery (v4).
    pub stream: Option<String>,
    /// End-of-stream marker (v4).
    pub done: bool,
    /// Logical channel for multiplexing (v4).
    pub channel: Option<String>,
}

// ── Broadcaster ──────────────────────────────────────────────────────────────

/// Broadcast events to all connected WebSocket clients, respecting scope
/// guards and dropping/closing slow consumers.
pub async fn broadcast(state: &Arc<GatewayState>, event: BroadcastEvent, opts: BroadcastOpts) {
    let message = match event.into_message() {
        Ok(message) => message,
        Err(e) => {
            warn!("failed to serialize broadcast payload: {e}");
            return;
        },
    };
    let event_name = message.event;

    let seq = state.next_seq();
    let stream = opts.stream.clone();
    let done = opts.done.then_some(true);
    let channel = opts.channel.clone();
    let frame = EventFrame {
        r#type: "event".into(),
        event: event_name.to_string(),
        payload: Some(message.payload),
        seq: Some(seq),
        state_version: opts.state_version,
        stream,
        done,
        channel,
    };
    let json = match serde_json::to_string(&frame) {
        Ok(j) => j,
        Err(e) => {
            warn!("failed to serialize broadcast event: {e}");
            return;
        },
    };

    // Forward to GraphQL subscription broadcast channel.
    #[cfg(feature = "graphql")]
    if let Some(ref payload) = frame.payload {
        let _ = state
            .graphql_broadcast
            .send((event_name.to_string(), payload.clone()));
    }

    let guards = event_scope_guards();
    let required_scopes = guards.get(event_name.as_ref());

    let inner = state.inner.read().await;
    debug!(
        event = event_name.as_ref(),
        seq,
        clients = inner.clients.len(),
        "broadcasting event"
    );
    for client in inner.clients.values() {
        // Check scope guard: if the event requires a scope, verify the client has it.
        if let Some(required) = required_scopes {
            let client_scopes = client.scopes();
            let has = client_scopes.contains(&scopes::ADMIN)
                || required.iter().any(|s| client_scopes.contains(s));
            if !has {
                continue;
            }
        }

        // Subscription filter (v4): skip clients not subscribed to this event.
        if !client.is_subscribed_to(event_name.as_ref()) {
            continue;
        }

        // Channel filter (v4): if event is scoped to a channel, skip clients
        // that haven't joined it.
        if let Some(ref ch) = opts.channel
            && !client.is_in_channel(ch)
        {
            continue;
        }

        if !client.send(&json) && opts.drop_if_slow {
            // Channel full or closed — skip silently when drop_if_slow.
            continue;
        }
    }
}

/// Legacy/raw bridge for dynamic event names and ad-hoc payloads.
// TODO: migrate remaining broadcast_raw call sites to typed BroadcastEvent
pub async fn broadcast_raw(
    state: &Arc<GatewayState>,
    event: impl Into<String>,
    payload: serde_json::Value,
    opts: BroadcastOpts,
) {
    broadcast(state, BroadcastEvent::raw(event, payload), opts).await;
}

/// Broadcast a tick event with the current timestamp and memory stats.
pub async fn broadcast_tick(
    state: &Arc<GatewayState>,
    process_memory_bytes: u64,
    system_available_bytes: u64,
    system_total_bytes: u64,
) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    broadcast(
        state,
        BroadcastEvent::Tick(TickPayload {
            ts,
            mem: TickMemoryPayload {
                process: process_memory_bytes,
                available: system_available_bytes,
                total: system_total_bytes,
            },
        }),
        BroadcastOpts {
            drop_if_slow: true,
            ..Default::default()
        },
    )
    .await;
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::broadcast_types::{
        ExecApprovalRequestedPayload, PairRequestedPayload, PairResolvedPayload,
        PairResolvedStatus, PresenceKind, PresencePayload, PushSubscriptionAction,
        PushSubscriptionsPayload, SessionEventKind, SessionEventPayload, TickMemoryPayload,
    };

    /// Verify that every `BroadcastEvent` variant whose event name appears in
    /// `event_scope_guards()` is accounted for. Adding a new variant forces
    /// the developer to consider whether it needs a scope guard entry.
    #[test]
    fn scope_guard_table_covers_sensitive_variants() {
        let guards = event_scope_guards();

        // Build a representative instance of every variant and check its event name
        // against the scope guard table. Variants that produce scope-guarded event
        // names must have an entry; others must not.
        let variants: Vec<BroadcastEvent> = vec![
            BroadcastEvent::Tick(TickPayload {
                ts: 0,
                mem: TickMemoryPayload {
                    process: 0,
                    available: 0,
                    total: 0,
                },
            }),
            BroadcastEvent::Session(SessionEventPayload {
                kind: SessionEventKind::Created,
                session_key: String::new(),
                version: None,
            }),
            BroadcastEvent::Presence(PresencePayload {
                kind: PresenceKind::NodeConnected,
                node_id: String::new(),
                platform: None,
            }),
            BroadcastEvent::ExecApprovalRequested(ExecApprovalRequestedPayload {
                request_id: String::new(),
                command: String::new(),
            }),
            BroadcastEvent::NodePairRequested(PairRequestedPayload {
                id: String::new(),
                device_id: String::new(),
                display_name: None,
                platform: String::new(),
            }),
            BroadcastEvent::NodePairResolved(PairResolvedPayload {
                id: String::new(),
                status: PairResolvedStatus::Approved,
            }),
            BroadcastEvent::DevicePairResolved(PairResolvedPayload {
                id: String::new(),
                status: PairResolvedStatus::Approved,
            }),
            BroadcastEvent::PushSubscriptions(PushSubscriptionsPayload {
                action: PushSubscriptionAction::Added,
            }),
            BroadcastEvent::raw("test.raw", serde_json::json!({})),
        ];

        // Events that are scope-guarded. Some are still emitted via broadcast_raw
        // (exec.approval.resolved, device.pair.requested) and don't have typed
        // variants yet.
        let expected_guarded = [
            "exec.approval.requested",
            "exec.approval.resolved",
            "device.pair.requested",
            "device.pair.resolved",
            "node.pair.requested",
            "node.pair.resolved",
        ];

        for variant in variants {
            let message = variant
                .into_message()
                .expect("serialization should not fail");
            let event_name = message.event;
            if expected_guarded.contains(&event_name.as_ref()) {
                assert!(
                    guards.contains_key(event_name.as_ref()),
                    "scope-sensitive event {event_name:?} is missing from event_scope_guards()"
                );
            }
        }

        // Also verify the guard table only references known event names.
        for key in guards.keys() {
            assert!(
                expected_guarded.contains(key),
                "event_scope_guards() contains unknown event {key:?}"
            );
        }
    }
}
