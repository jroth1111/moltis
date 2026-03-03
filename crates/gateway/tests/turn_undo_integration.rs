#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{fs, path::Path, sync::Arc};

use {
    async_trait::async_trait,
    serde_json::{Value, json},
};

use {
    moltis_gateway::{
        auth,
        methods::{MethodContext, MethodRegistry},
        services::{ChatService, GatewayServices, ServiceResult},
        state::GatewayState,
    },
    moltis_sessions::{store::SessionStore, undo::UndoManager},
};

struct QueuedOnlyChatService;

#[async_trait]
impl ChatService for QueuedOnlyChatService {
    async fn send(&self, _params: Value) -> ServiceResult {
        Ok(json!({
            "ok": true,
            "state": "queued"
        }))
    }

    async fn abort(&self, _params: Value) -> ServiceResult {
        Ok(json!({}))
    }

    async fn cancel_queued(&self, _params: Value) -> ServiceResult {
        Ok(json!({ "cleared": 0 }))
    }

    async fn history(&self, _params: Value) -> ServiceResult {
        Ok(json!([]))
    }

    async fn inject(&self, _params: Value) -> ServiceResult {
        Ok(json!({}))
    }

    async fn clear(&self, _params: Value) -> ServiceResult {
        Ok(json!({ "ok": true }))
    }

    async fn compact(&self, _params: Value) -> ServiceResult {
        Ok(json!({}))
    }

    async fn context(&self, _params: Value) -> ServiceResult {
        Ok(json!({}))
    }

    async fn raw_prompt(&self, _params: Value) -> ServiceResult {
        Ok(json!({}))
    }

    async fn full_context(&self, _params: Value) -> ServiceResult {
        Ok(json!({}))
    }
}

fn make_state(services: GatewayServices) -> Arc<GatewayState> {
    GatewayState::new(auth::resolve_auth(None, None), services)
}

fn mark_read_only(path: &Path) {
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(path, perms).unwrap();
}

async fn dispatch_method(
    methods: &MethodRegistry,
    state: &Arc<GatewayState>,
    method: &str,
    params: Value,
) -> moltis_protocol::ResponseFrame {
    methods
        .dispatch(MethodContext {
            request_id: format!("req-{method}"),
            method: method.to_string(),
            params,
            client_conn_id: "conn-1".to_string(),
            client_role: "operator".to_string(),
            client_scopes: vec!["operator.admin".to_string()],
            state: Arc::clone(state),
            channel: None,
            trace_id: uuid::Uuid::new_v4().to_string(),
        })
        .await
}

fn sample_messages(content: &str) -> Vec<Value> {
    vec![json!({ "role": "user", "content": content })]
}

#[tokio::test]
async fn queued_chat_send_does_not_push_undo_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(SessionStore::new(temp.path().join("sessions")));
    store
        .append("main", &json!({"role": "user", "content": "seed"}))
        .await
        .unwrap();

    let services = GatewayServices::noop()
        .with_chat(Arc::new(QueuedOnlyChatService))
        .with_session_store(Arc::clone(&store));
    let state = make_state(services);
    {
        let mut inner = state.inner.write().await;
        inner
            .active_sessions
            .insert("conn-1".to_string(), "main".to_string());
    }

    let methods = MethodRegistry::new();
    let response =
        dispatch_method(&methods, &state, "chat.send", json!({ "text": "queued" })).await;

    assert!(response.ok, "chat.send should succeed: {response:?}");
    let payload = response.payload.expect("payload should be present");
    assert_eq!(payload["state"], "queued");

    let inner = state.inner.read().await;
    assert!(
        !inner.undo_managers.contains_key("main"),
        "queued-only send must not create undo checkpoints"
    );
}

#[tokio::test]
async fn destructive_session_methods_prune_undo_managers() {
    let state = make_state(GatewayServices::noop());

    {
        let mut inner = state.inner.write().await;

        let mut reset_mgr = UndoManager::new();
        reset_mgr.push(sample_messages("reset"), 0);
        inner
            .undo_managers
            .insert("reset-me".to_string(), reset_mgr);

        let mut delete_mgr = UndoManager::new();
        delete_mgr.push(sample_messages("delete"), 0);
        inner
            .undo_managers
            .insert("delete-me".to_string(), delete_mgr);

        let mut keep_mgr = UndoManager::new();
        keep_mgr.push(sample_messages("keep"), 0);
        inner.undo_managers.insert("keep-me".to_string(), keep_mgr);
    }

    let methods = MethodRegistry::new();

    let reset_response = dispatch_method(
        &methods,
        &state,
        "sessions.reset",
        json!({ "key": "reset-me" }),
    )
    .await;
    assert!(
        reset_response.ok,
        "sessions.reset should succeed: {reset_response:?}"
    );
    {
        let inner = state.inner.read().await;
        assert!(!inner.undo_managers.contains_key("reset-me"));
        assert!(inner.undo_managers.contains_key("delete-me"));
        assert!(inner.undo_managers.contains_key("keep-me"));
    }

    let delete_response = dispatch_method(
        &methods,
        &state,
        "sessions.delete",
        json!({ "key": "delete-me" }),
    )
    .await;
    assert!(
        delete_response.ok,
        "sessions.delete should succeed: {delete_response:?}"
    );
    {
        let inner = state.inner.read().await;
        assert!(!inner.undo_managers.contains_key("delete-me"));
        assert!(inner.undo_managers.contains_key("keep-me"));
    }

    let clear_response = dispatch_method(&methods, &state, "sessions.clear_all", json!({})).await;
    assert!(
        clear_response.ok,
        "sessions.clear_all should succeed: {clear_response:?}"
    );
    let inner = state.inner.read().await;
    assert!(
        inner.undo_managers.is_empty(),
        "sessions.clear_all must clear all undo managers"
    );
}

#[tokio::test]
async fn undo_rolls_back_manager_state_when_replace_history_fails() {
    let temp = tempfile::tempdir().unwrap();
    let sessions_dir = temp.path().join("sessions");
    let store = Arc::new(SessionStore::new(sessions_dir.clone()));

    store
        .append("main", &json!({ "role": "user", "content": "current" }))
        .await
        .unwrap();
    mark_read_only(&sessions_dir.join("main.jsonl"));

    let services = GatewayServices::noop().with_session_store(Arc::clone(&store));
    let state = make_state(services);
    {
        let mut inner = state.inner.write().await;
        inner
            .active_sessions
            .insert("conn-1".to_string(), "main".to_string());

        let mut mgr = UndoManager::new();
        mgr.push(sample_messages("before-undo"), 0);
        inner.undo_managers.insert("main".to_string(), mgr);
    }

    let methods = MethodRegistry::new();
    let response = dispatch_method(&methods, &state, "chat.undo", json!({})).await;

    assert!(
        !response.ok,
        "chat.undo should fail on read-only history file"
    );
    assert_eq!(
        response.error.as_ref().map(|e| e.code.as_str()),
        Some("UNAVAILABLE")
    );

    let inner = state.inner.read().await;
    let mgr = inner
        .undo_managers
        .get("main")
        .expect("undo manager should still exist");
    assert_eq!(mgr.undo_depth(), 1, "undo depth should be rolled back");
    assert_eq!(mgr.redo_depth(), 0, "redo depth should be rolled back");
}

#[tokio::test]
async fn redo_rolls_back_manager_state_when_replace_history_fails() {
    let temp = tempfile::tempdir().unwrap();
    let sessions_dir = temp.path().join("sessions");
    let store = Arc::new(SessionStore::new(sessions_dir.clone()));

    store
        .append(
            "main",
            &json!({ "role": "assistant", "content": "current" }),
        )
        .await
        .unwrap();
    mark_read_only(&sessions_dir.join("main.jsonl"));

    let services = GatewayServices::noop().with_session_store(Arc::clone(&store));
    let state = make_state(services);
    {
        let mut inner = state.inner.write().await;
        inner
            .active_sessions
            .insert("conn-1".to_string(), "main".to_string());

        let mut mgr = UndoManager::new();
        mgr.push(sample_messages("before-redo"), 0);
        let _ = mgr.undo(sample_messages("current"));
        inner.undo_managers.insert("main".to_string(), mgr);
    }

    let methods = MethodRegistry::new();
    let response = dispatch_method(&methods, &state, "chat.redo", json!({})).await;

    assert!(
        !response.ok,
        "chat.redo should fail on read-only history file"
    );
    assert_eq!(
        response.error.as_ref().map(|e| e.code.as_str()),
        Some("UNAVAILABLE")
    );

    let inner = state.inner.read().await;
    let mgr = inner
        .undo_managers
        .get("main")
        .expect("undo manager should still exist");
    assert_eq!(mgr.undo_depth(), 0, "undo depth should be rolled back");
    assert_eq!(mgr.redo_depth(), 1, "redo depth should be rolled back");
}
