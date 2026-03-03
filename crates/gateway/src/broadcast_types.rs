use std::borrow::Cow;

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct BroadcastMessage {
    pub event: Cow<'static, str>,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub enum BroadcastEvent {
    Tick(TickPayload),
    Session(SessionEventPayload),
    Presence(PresencePayload),
    ExecApprovalRequested(ExecApprovalRequestedPayload),
    NodePairRequested(PairRequestedPayload),
    NodePairResolved(PairResolvedPayload),
    DevicePairResolved(PairResolvedPayload),
    PushSubscriptions(PushSubscriptionsPayload),
    Raw(RawBroadcastEvent),
}

impl BroadcastEvent {
    pub fn raw(event: impl Into<String>, payload: Value) -> Self {
        Self::Raw(RawBroadcastEvent {
            event: Cow::Owned(event.into()),
            payload,
        })
    }

    pub fn raw_static(event: &'static str, payload: Value) -> Self {
        Self::Raw(RawBroadcastEvent {
            event: Cow::Borrowed(event),
            payload,
        })
    }

    pub fn session(kind: SessionEventKind, session_key: impl Into<String>, version: Option<u64>) -> Self {
        Self::Session(SessionEventPayload {
            kind,
            session_key: session_key.into(),
            version,
        })
    }

    pub fn node_connected(node_id: impl Into<String>, platform: impl Into<String>) -> Self {
        Self::Presence(PresencePayload {
            kind: PresenceKind::NodeConnected,
            node_id: node_id.into(),
            platform: Some(platform.into()),
        })
    }

    pub fn node_disconnected(node_id: impl Into<String>) -> Self {
        Self::Presence(PresencePayload {
            kind: PresenceKind::NodeDisconnected,
            node_id: node_id.into(),
            platform: None,
        })
    }

    pub fn pair_requested(
        id: impl Into<String>,
        device_id: impl Into<String>,
        display_name: Option<String>,
        platform: impl Into<String>,
    ) -> Self {
        Self::NodePairRequested(PairRequestedPayload {
            id: id.into(),
            device_id: device_id.into(),
            display_name,
            platform: platform.into(),
        })
    }

    pub fn pair_resolved_node(id: impl Into<String>, status: PairResolvedStatus) -> Self {
        Self::NodePairResolved(PairResolvedPayload {
            id: id.into(),
            status,
        })
    }

    pub fn pair_resolved_device(id: impl Into<String>, status: PairResolvedStatus) -> Self {
        Self::DevicePairResolved(PairResolvedPayload {
            id: id.into(),
            status,
        })
    }

    pub fn exec_approval_requested(
        request_id: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        Self::ExecApprovalRequested(ExecApprovalRequestedPayload {
            request_id: request_id.into(),
            command: command.into(),
        })
    }

    pub fn push_subscriptions(action: PushSubscriptionAction) -> Self {
        Self::PushSubscriptions(PushSubscriptionsPayload { action })
    }

    pub fn into_message(self) -> serde_json::Result<BroadcastMessage> {
        let (event, payload) = match self {
            Self::Tick(payload) => ("tick", serde_json::to_value(payload)?),
            Self::Session(payload) => ("session", serde_json::to_value(payload)?),
            Self::Presence(payload) => ("presence", serde_json::to_value(payload)?),
            Self::ExecApprovalRequested(payload) => {
                ("exec.approval.requested", serde_json::to_value(payload)?)
            },
            Self::NodePairRequested(payload) => {
                ("node.pair.requested", serde_json::to_value(payload)?)
            },
            Self::NodePairResolved(payload) => ("node.pair.resolved", serde_json::to_value(payload)?),
            Self::DevicePairResolved(payload) => {
                ("device.pair.resolved", serde_json::to_value(payload)?)
            },
            Self::PushSubscriptions(payload) => {
                ("push.subscriptions", serde_json::to_value(payload)?)
            },
            Self::Raw(payload) => return Ok(payload.into_message()),
        };
        Ok(BroadcastMessage {
            event: Cow::Borrowed(event),
            payload,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RawBroadcastEvent {
    pub event: Cow<'static, str>,
    pub payload: Value,
}

impl RawBroadcastEvent {
    pub fn into_message(self) -> BroadcastMessage {
        BroadcastMessage {
            event: self.event,
            payload: self.payload,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TickPayload {
    pub ts: u64,
    pub mem: TickMemoryPayload,
}

#[derive(Debug, Clone, Serialize)]
pub struct TickMemoryPayload {
    pub process: u64,
    pub available: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEventPayload {
    pub kind: SessionEventKind,
    pub session_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionEventKind {
    Created,
    Deleted,
    Patched,
    Switched,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresencePayload {
    #[serde(rename = "type")]
    pub kind: PresenceKind,
    pub node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum PresenceKind {
    #[serde(rename = "node.connected")]
    NodeConnected,
    #[serde(rename = "node.disconnected")]
    NodeDisconnected,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecApprovalRequestedPayload {
    pub request_id: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRequestedPayload {
    pub id: String,
    pub device_id: String,
    pub display_name: Option<String>,
    pub platform: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PairResolvedPayload {
    pub id: String,
    pub status: PairResolvedStatus,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PairResolvedStatus {
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Serialize)]
pub struct PushSubscriptionsPayload {
    pub action: PushSubscriptionAction,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PushSubscriptionAction {
    Added,
    Removed,
}

#[cfg(test)]
mod tests {
    use super::{
        BroadcastEvent, PairResolvedStatus, PresenceKind, PushSubscriptionAction, SessionEventKind,
    };

    #[test]
    fn session_event_maps_to_legacy_wire_shape() {
        let event = BroadcastEvent::session(SessionEventKind::Patched, "session-123", Some(7));
        let message = match event.into_message() {
            Ok(message) => message,
            Err(err) => panic!("failed to map session event: {err}"),
        };
        assert_eq!(message.event.as_ref(), "session");
        assert_eq!(
            message.payload,
            serde_json::json!({
                "kind": "patched",
                "sessionKey": "session-123",
                "version": 7
            })
        );
    }

    #[test]
    fn presence_event_maps_to_legacy_wire_shape() {
        let event = BroadcastEvent::node_connected("node-a", "ios");
        let message = match event.into_message() {
            Ok(message) => message,
            Err(err) => panic!("failed to map presence event: {err}"),
        };
        assert_eq!(message.event.as_ref(), "presence");
        assert_eq!(
            message.payload,
            serde_json::json!({
                "type": "node.connected",
                "nodeId": "node-a",
                "platform": "ios",
            })
        );

        let disconnected = BroadcastEvent::node_disconnected("node-b");
        let disconnected_message = match disconnected.into_message() {
            Ok(message) => message,
            Err(err) => panic!("failed to map disconnected presence event: {err}"),
        };
        assert_eq!(disconnected_message.event.as_ref(), "presence");
        assert_eq!(
            disconnected_message.payload,
            serde_json::json!({
                "type": "node.disconnected",
                "nodeId": "node-b",
            })
        );
    }

    #[test]
    fn pairing_and_push_events_map_to_legacy_names() {
        let node_pair = BroadcastEvent::pair_resolved_node("pair-1", PairResolvedStatus::Approved);
        let node_pair_message = match node_pair.into_message() {
            Ok(message) => message,
            Err(err) => panic!("failed to map node pair event: {err}"),
        };
        assert_eq!(node_pair_message.event.as_ref(), "node.pair.resolved");
        assert_eq!(
            node_pair_message.payload,
            serde_json::json!({
                "id": "pair-1",
                "status": "approved",
            })
        );

        let push = BroadcastEvent::PushSubscriptions(super::PushSubscriptionsPayload {
            action: PushSubscriptionAction::Removed,
        });
        let push_message = match push.into_message() {
            Ok(message) => message,
            Err(err) => panic!("failed to map push event: {err}"),
        };
        assert_eq!(push_message.event.as_ref(), "push.subscriptions");
        assert_eq!(push_message.payload, serde_json::json!({ "action": "removed" }));
    }

    #[test]
    fn raw_event_preserves_name_and_payload() {
        let event = BroadcastEvent::raw("custom.event", serde_json::json!({ "k": "v" }));
        let message = match event.into_message() {
            Ok(message) => message,
            Err(err) => panic!("failed to map raw event: {err}"),
        };
        assert_eq!(message.event.as_ref(), "custom.event");
        assert_eq!(message.payload, serde_json::json!({ "k": "v" }));
    }

    #[test]
    fn constructed_payload_variants_are_serializable() {
        let event = BroadcastEvent::pair_requested(
            "pair-2",
            "device-2",
            Some("MacBook".to_string()),
            "macos",
        );
        let message = match event.into_message() {
            Ok(message) => message,
            Err(err) => panic!("failed to map pair requested event: {err}"),
        };
        assert_eq!(message.event.as_ref(), "node.pair.requested");
        assert_eq!(
            message.payload,
            serde_json::json!({
                "id": "pair-2",
                "deviceId": "device-2",
                "displayName": "MacBook",
                "platform": "macos",
            })
        );

        let presence = serde_json::to_value(PresenceKind::NodeDisconnected);
        let value = match presence {
            Ok(value) => value,
            Err(err) => panic!("failed to serialize presence kind: {err}"),
        };
        assert_eq!(value, serde_json::json!("node.disconnected"));
    }
}
