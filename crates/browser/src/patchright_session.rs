use std::{collections::HashMap, process::Stdio};

use {
    serde::{Deserialize, Serialize},
    serde_json::Value,
    tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
        process::{Child, ChildStdin, ChildStdout, Command},
        time::Duration,
    },
};

use crate::{
    error::Error, protection::PatchrightLaunchProfile, session_state::SessionState,
    types::ProtectionConfig,
};

#[derive(Debug, Clone)]
pub struct PatchrightPageCapture {
    pub final_url: String,
    pub title_len: usize,
    pub body_text_len: usize,
    pub interactive_element_count: usize,
    pub html: String,
}

#[derive(Debug)]
pub struct PatchrightSession {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    timeout_ms: u64,
    next_id: u64,
}

#[derive(Debug, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum PatchrightRequest {
    Goto {
        id: u64,
        url: String,
    },
    CapturePage {
        id: u64,
    },
    Evaluate {
        id: u64,
        code: String,
    },
    Screenshot {
        id: u64,
        full_page: bool,
    },
    RestoreState {
        id: u64,
        state: SessionState,
    },
    WaitSelector {
        id: u64,
        selector: String,
        timeout_ms: u64,
    },
    MouseMove {
        id: u64,
        x: f64,
        y: f64,
    },
    MouseClick {
        id: u64,
        x: f64,
        y: f64,
        click_count: u8,
    },
    KeyboardType {
        id: u64,
        text: String,
    },
    KeyboardPress {
        id: u64,
        key: String,
    },
    SelectOption {
        id: u64,
        selector: String,
        value: String,
    },
    Check {
        id: u64,
        selector: String,
    },
    Uncheck {
        id: u64,
        selector: String,
    },
    Clear {
        id: u64,
        selector: String,
    },
    SetInputFiles {
        id: u64,
        selector: String,
        path: String,
    },
    GetUrl {
        id: u64,
    },
    GetTitle {
        id: u64,
    },
    Back {
        id: u64,
    },
    Forward {
        id: u64,
    },
    Refresh {
        id: u64,
    },
    EnableInterception {
        id: u64,
        patterns: Vec<String>,
        extra_headers: HashMap<String, String>,
    },
    DisableInterception {
        id: u64,
    },
    SetExtraHeaders {
        id: u64,
        headers: HashMap<String, String>,
    },
    StartApiCapture {
        id: u64,
        allowed_hosts: Vec<String>,
        url_patterns: Vec<String>,
        include_document_requests: bool,
        max_examples_per_endpoint: usize,
    },
    StopApiCapture {
        id: u64,
    },
    NewTab {
        id: u64,
        name: String,
    },
    ListTabs {
        id: u64,
    },
    SwitchTab {
        id: u64,
        name: String,
    },
    CloseTab {
        id: u64,
        name: String,
    },
    Close {
        id: u64,
    },
}

#[derive(Debug, Deserialize)]
struct WorkerResponse {
    id: u64,
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    result: Value,
}

impl PatchrightSession {
    pub async fn start(
        config: &ProtectionConfig,
        launch_profile: &PatchrightLaunchProfile,
    ) -> Result<Self, Error> {
        let mut child = Command::new(config.python_binary.trim())
            .arg("-u")
            .arg("-c")
            .arg(PATCHRIGHT_SESSION_PY)
            .arg(
                serde_json::to_string(launch_profile)
                    .map_err(|e| Error::LaunchFailed(format!("invalid patchright profile: {e}")))?,
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::LaunchFailed(format!("failed to start patchright worker: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::LaunchFailed("failed to capture patchright stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::LaunchFailed("failed to capture patchright stdout".into()))?;
        let mut stdout = BufReader::new(stdout).lines();

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "moltis_browser::patchright", line, "patchright stderr");
                }
            });
        }

        let handshake_line = tokio::time::timeout(
            Duration::from_millis(config.timeout_ms.max(1_000)),
            stdout.next_line(),
        )
        .await
        .map_err(|_| Error::LaunchFailed("patchright worker handshake timed out".into()))?
        .map_err(|e| Error::LaunchFailed(format!("patchright worker handshake failed: {e}")))?
        .ok_or_else(|| Error::LaunchFailed("patchright worker exited before handshake".into()))?;
        let handshake: WorkerResponse = serde_json::from_str(&handshake_line).map_err(|e| {
            Error::LaunchFailed(format!(
                "invalid patchright worker handshake: {e}; line: {handshake_line}"
            ))
        })?;
        if handshake.id != 0 {
            return Err(Error::LaunchFailed(format!(
                "invalid patchright worker handshake id: expected 0, got {}",
                handshake.id
            )));
        }
        if !handshake.ok {
            return Err(Error::LaunchFailed(handshake.error.unwrap_or_else(|| {
                "patchright worker reported an unknown startup error".to_string()
            })));
        }
        if handshake.result.get("ready").and_then(Value::as_bool) != Some(true) {
            return Err(Error::LaunchFailed(
                "patchright worker handshake missing ready signal".into(),
            ));
        }

        Ok(Self {
            child,
            stdin,
            stdout,
            timeout_ms: config.timeout_ms.max(1_000),
            next_id: 1,
        })
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn send(&mut self, request: PatchrightRequest) -> Result<Value, Error> {
        let request_id = match &request {
            PatchrightRequest::Goto { id, .. }
            | PatchrightRequest::CapturePage { id }
            | PatchrightRequest::Evaluate { id, .. }
            | PatchrightRequest::Screenshot { id, .. }
            | PatchrightRequest::RestoreState { id, .. }
            | PatchrightRequest::WaitSelector { id, .. }
            | PatchrightRequest::MouseMove { id, .. }
            | PatchrightRequest::MouseClick { id, .. }
            | PatchrightRequest::KeyboardType { id, .. }
            | PatchrightRequest::KeyboardPress { id, .. }
            | PatchrightRequest::SelectOption { id, .. }
            | PatchrightRequest::Check { id, .. }
            | PatchrightRequest::Uncheck { id, .. }
            | PatchrightRequest::Clear { id, .. }
            | PatchrightRequest::SetInputFiles { id, .. }
            | PatchrightRequest::GetUrl { id }
            | PatchrightRequest::GetTitle { id }
            | PatchrightRequest::Back { id }
            | PatchrightRequest::Forward { id }
            | PatchrightRequest::Refresh { id }
            | PatchrightRequest::EnableInterception { id, .. }
            | PatchrightRequest::DisableInterception { id }
            | PatchrightRequest::SetExtraHeaders { id, .. }
            | PatchrightRequest::StartApiCapture { id, .. }
            | PatchrightRequest::StopApiCapture { id }
            | PatchrightRequest::NewTab { id, .. }
            | PatchrightRequest::ListTabs { id }
            | PatchrightRequest::SwitchTab { id, .. }
            | PatchrightRequest::CloseTab { id, .. }
            | PatchrightRequest::Close { id } => *id,
        };
        let line = serde_json::to_vec(&request).map_err(|e| {
            Error::NavigationFailed(format!("failed to encode patchright rpc: {e}"))
        })?;
        self.stdin
            .write_all(&line)
            .await
            .map_err(|e| Error::ConnectionClosed(format!("patchright stdin write failed: {e}")))?;
        self.stdin.write_all(b"\n").await.map_err(|e| {
            Error::ConnectionClosed(format!("patchright stdin newline failed: {e}"))
        })?;
        self.stdin
            .flush()
            .await
            .map_err(|e| Error::ConnectionClosed(format!("patchright stdin flush failed: {e}")))?;

        let line = tokio::time::timeout(
            Duration::from_millis(self.timeout_ms),
            self.stdout.next_line(),
        )
        .await
        .map_err(|_| {
            Error::Timeout(format!(
                "patchright rpc timed out after {}ms",
                self.timeout_ms
            ))
        })?
        .map_err(|e| Error::ConnectionClosed(format!("patchright stdout read failed: {e}")))?
        .ok_or_else(|| Error::ConnectionClosed("patchright worker exited".into()))?;

        let response: WorkerResponse = serde_json::from_str(&line).map_err(|e| {
            Error::NavigationFailed(format!(
                "invalid patchright rpc response: {e}; line: {line}"
            ))
        })?;
        if response.id != request_id {
            return Err(Error::ConnectionClosed(format!(
                "patchright response id mismatch: expected {request_id}, got {}",
                response.id
            )));
        }
        if !response.ok {
            return Err(Error::NavigationFailed(response.error.unwrap_or_else(
                || "patchright worker returned an unknown error".to_string(),
            )));
        }
        Ok(response.result)
    }

    pub async fn goto(&mut self, url: &str) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::Goto {
            id,
            url: url.to_string(),
        })
        .await
        .map(|_| ())
    }

    pub async fn capture_page(&mut self) -> Result<PatchrightPageCapture, Error> {
        let id = self.next_id();
        let value = self.send(PatchrightRequest::CapturePage { id }).await?;
        let final_url = value
            .get("final_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let title_len = value
            .get("title_len")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(0);
        let body_text_len = value
            .get("body_text_len")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(0);
        let interactive_element_count = value
            .get("interactive_element_count")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(0);
        let html = value
            .get("html")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(PatchrightPageCapture {
            final_url,
            title_len,
            body_text_len,
            interactive_element_count,
            html,
        })
    }

    pub async fn evaluate(&mut self, code: &str) -> Result<Value, Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::Evaluate {
            id,
            code: code.to_string(),
        })
        .await
    }

    pub async fn screenshot(&mut self, full_page: bool) -> Result<String, Error> {
        let id = self.next_id();
        let value = self
            .send(PatchrightRequest::Screenshot { id, full_page })
            .await?;
        value
            .get("data_base64")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| Error::ScreenshotFailed("patchright screenshot missing data".into()))
    }

    pub async fn restore_state(&mut self, state: &SessionState) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::RestoreState {
            id,
            state: state.clone(),
        })
        .await
        .map(|_| ())
    }

    pub async fn wait_selector(&mut self, selector: &str, timeout_ms: u64) -> Result<bool, Error> {
        let id = self.next_id();
        let value = self
            .send(PatchrightRequest::WaitSelector {
                id,
                selector: selector.to_string(),
                timeout_ms,
            })
            .await?;
        Ok(value.get("found").and_then(Value::as_bool).unwrap_or(false))
    }

    pub async fn mouse_move(&mut self, x: f64, y: f64) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::MouseMove { id, x, y })
            .await
            .map(|_| ())
    }

    pub async fn mouse_click(&mut self, x: f64, y: f64, click_count: u8) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::MouseClick {
            id,
            x,
            y,
            click_count,
        })
        .await
        .map(|_| ())
    }

    pub async fn keyboard_type(&mut self, text: &str) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::KeyboardType {
            id,
            text: text.to_string(),
        })
        .await
        .map(|_| ())
    }

    pub async fn keyboard_press(&mut self, key: &str) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::KeyboardPress {
            id,
            key: key.to_string(),
        })
        .await
        .map(|_| ())
    }

    pub async fn select_option(&mut self, selector: &str, value: &str) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::SelectOption {
            id,
            selector: selector.to_string(),
            value: value.to_string(),
        })
        .await
        .map(|_| ())
    }

    pub async fn check(&mut self, selector: &str) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::Check {
            id,
            selector: selector.to_string(),
        })
        .await
        .map(|_| ())
    }

    pub async fn uncheck(&mut self, selector: &str) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::Uncheck {
            id,
            selector: selector.to_string(),
        })
        .await
        .map(|_| ())
    }

    pub async fn clear(&mut self, selector: &str) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::Clear {
            id,
            selector: selector.to_string(),
        })
        .await
        .map(|_| ())
    }

    pub async fn set_input_files(&mut self, selector: &str, path: &str) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::SetInputFiles {
            id,
            selector: selector.to_string(),
            path: path.to_string(),
        })
        .await
        .map(|_| ())
    }

    pub async fn get_url(&mut self) -> Result<String, Error> {
        let id = self.next_id();
        let value = self.send(PatchrightRequest::GetUrl { id }).await?;
        Ok(value
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    pub async fn get_title(&mut self) -> Result<String, Error> {
        let id = self.next_id();
        let value = self.send(PatchrightRequest::GetTitle { id }).await?;
        Ok(value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    pub async fn back(&mut self) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::Back { id }).await.map(|_| ())
    }

    pub async fn forward(&mut self) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::Forward { id })
            .await
            .map(|_| ())
    }

    pub async fn refresh(&mut self) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::Refresh { id })
            .await
            .map(|_| ())
    }

    pub async fn enable_interception(
        &mut self,
        patterns: Vec<String>,
        extra_headers: HashMap<String, String>,
    ) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::EnableInterception {
            id,
            patterns,
            extra_headers,
        })
        .await
        .map(|_| ())
    }

    pub async fn disable_interception(&mut self) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::DisableInterception { id })
            .await
            .map(|_| ())
    }

    pub async fn set_extra_headers(
        &mut self,
        headers: HashMap<String, String>,
    ) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::SetExtraHeaders { id, headers })
            .await
            .map(|_| ())
    }

    pub async fn start_api_capture(
        &mut self,
        config: &crate::api_capture::ApiCaptureConfig,
    ) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::StartApiCapture {
            id,
            allowed_hosts: config.allowed_hosts.clone(),
            url_patterns: config.url_patterns.clone(),
            include_document_requests: config.include_document_requests,
            max_examples_per_endpoint: config.max_examples_per_endpoint,
        })
        .await
        .map(|_| ())
    }

    pub async fn stop_api_capture(
        &mut self,
    ) -> Result<Vec<crate::api_capture::CapturedRequestRecord>, Error> {
        let id = self.next_id();
        let value = self.send(PatchrightRequest::StopApiCapture { id }).await?;
        serde_json::from_value(
            value
                .get("records")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
        )
        .map_err(|error| {
            Error::NavigationFailed(format!(
                "invalid patchright api capture records payload: {error}"
            ))
        })
    }

    pub async fn new_tab(&mut self, name: &str) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::NewTab {
            id,
            name: name.to_string(),
        })
        .await
        .map(|_| ())
    }

    pub async fn list_tabs(&mut self) -> Result<Vec<String>, Error> {
        let id = self.next_id();
        let value = self.send(PatchrightRequest::ListTabs { id }).await?;
        Ok(value
            .get("tabs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect())
    }

    pub async fn switch_tab(&mut self, name: &str) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::SwitchTab {
            id,
            name: name.to_string(),
        })
        .await
        .map(|_| ())
    }

    pub async fn close_tab(&mut self, name: &str) -> Result<(), Error> {
        let id = self.next_id();
        self.send(PatchrightRequest::CloseTab {
            id,
            name: name.to_string(),
        })
        .await
        .map(|_| ())
    }

    pub async fn close(&mut self) -> Result<(), Error> {
        let id = self.next_id();
        let _ = self.send(PatchrightRequest::Close { id }).await;
        let _ = self.child.wait().await;
        Ok(())
    }
}

const PATCHRIGHT_SESSION_PY: &str = include_str!("patchright_worker.py");

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, OnceLock},
    };

    use {
        axum::{
            Json, Router,
            extract::State,
            response::Html,
            routing::{get, post},
        },
        serde_json::json,
        tokio::{
            net::TcpListener,
            sync::{Mutex, MutexGuard},
            task::JoinHandle,
            time::{Duration, sleep},
        },
    };

    use {
        super::*,
        crate::{
            api_capture::ApiCaptureConfig,
            protection::build_patchright_launch_profile_for_browser,
            session_state::{CookieEntry, SessionState, StorageEntry},
            types::BrowserConfig,
        },
    };

    #[derive(Clone, Default)]
    struct WorkerTestState {
        auth_headers: Arc<std::sync::Mutex<Vec<Option<String>>>>,
    }

    fn live_browser_test_mutex() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    async fn acquire_live_browser_test_guard() -> MutexGuard<'static, ()> {
        live_browser_test_mutex().lock().await
    }

    fn test_browser_config() -> BrowserConfig {
        let mut config = BrowserConfig::default();
        config.persist_profile = false;
        config.protection.timeout_ms = 90_000;
        config
    }

    async fn worker_page() -> Html<&'static str> {
        Html(
            r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>Worker Probe</title>
  </head>
  <body data-ready="false">
    <main>
      <h1>Worker Probe</h1>
      <p>This page is intentionally contentful so navigation diagnostics classify it as real content.</p>
      <button id="go" type="button">Go</button>
      <input id="name" value="worker">
    </main>
    <script>
      (async () => {
        await fetch('/api/search?term=worker');
        document.body.dataset.ready = 'true';
      })();
    </script>
  </body>
</html>"#,
        )
    }

    async fn api_search(
        State(state): State<WorkerTestState>,
        headers: axum::http::HeaderMap,
    ) -> Json<Value> {
        state.auth_headers.lock().unwrap().push(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
        );
        Json(json!({ "ok": true }))
    }

    async fn start_worker_server()
    -> Result<(String, WorkerTestState, JoinHandle<()>), Box<dyn std::error::Error>> {
        let state = WorkerTestState::default();
        let app = Router::new()
            .route("/page", get(worker_page))
            .route("/api/search", post(api_search).get(api_search))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .unwrap_or_else(|error| panic!("worker server should run: {error}"));
        });

        Ok((origin, state, server))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patchright_session_executes_core_commands() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = acquire_live_browser_test_guard().await;
        let (origin, _state, server) = start_worker_server().await?;
        let config = test_browser_config();
        let profile = build_patchright_launch_profile_for_browser(&config, None);
        let mut session = PatchrightSession::start(&config.protection, &profile).await?;

        session.goto(&format!("{origin}/page")).await?;
        assert!(session.wait_selector("#go", 10_000).await?);
        let capture = session.capture_page().await?;
        assert_eq!(capture.final_url, format!("{origin}/page"));
        assert!(capture.title_len > 0);
        assert!(capture.body_text_len >= 64);
        assert!(capture.interactive_element_count >= 2);

        assert_eq!(session.get_title().await?, "Worker Probe");
        assert_eq!(session.get_url().await?, format!("{origin}/page"));
        assert_eq!(
            session
                .evaluate("document.getElementById('go').textContent")
                .await?
                .as_str(),
            Some("Go")
        );
        let screenshot = session.screenshot(false).await?;
        assert!(!screenshot.is_empty());

        session.close().await?;
        server.abort();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patchright_session_supports_interception_and_api_capture()
    -> Result<(), Box<dyn std::error::Error>> {
        let _guard = acquire_live_browser_test_guard().await;
        let (origin, state, server) = start_worker_server().await?;
        let config = test_browser_config();
        let profile = build_patchright_launch_profile_for_browser(&config, None);
        let mut session = PatchrightSession::start(&config.protection, &profile).await?;

        session
            .start_api_capture(&ApiCaptureConfig {
                allowed_hosts: vec!["127.0.0.1".to_string()],
                url_patterns: vec!["*api/search*".to_string()],
                ..ApiCaptureConfig::default()
            })
            .await?;
        session
            .enable_interception(
                vec!["*api/search*".to_string()],
                HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer initial-token".to_string(),
                )]),
            )
            .await?;
        session
            .set_extra_headers(HashMap::from([(
                "Authorization".to_string(),
                "Bearer patched-token".to_string(),
            )]))
            .await?;

        session.goto(&format!("{origin}/page")).await?;
        assert!(
            session
                .wait_selector("body[data-ready='true']", 10_000)
                .await?
        );
        sleep(Duration::from_millis(250)).await;

        let records = session.stop_api_capture().await?;
        session.disable_interception().await?;
        session.close().await?;
        server.abort();

        assert!(records.iter().any(|record| {
            record.url.contains("/api/search")
                && record.request_headers.iter().any(|(name, value)| {
                    name.eq_ignore_ascii_case("authorization") && value == "Bearer patched-token"
                })
        }));
        assert!(
            state
                .auth_headers
                .lock()
                .unwrap()
                .iter()
                .any(|value| { value.as_deref() == Some("Bearer patched-token") })
        );

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patchright_session_restores_state_before_navigation()
    -> Result<(), Box<dyn std::error::Error>> {
        let _guard = acquire_live_browser_test_guard().await;
        let (origin, _state, server) = start_worker_server().await?;
        let config = test_browser_config();
        let profile = build_patchright_launch_profile_for_browser(&config, None);
        let mut session = PatchrightSession::start(&config.protection, &profile).await?;

        session
            .restore_state(&SessionState {
                version: 1,
                captured_at: "2026-03-06T00:00:00Z".to_string(),
                url: format!("{origin}/page"),
                cookies: vec![CookieEntry {
                    name: "handoff".to_string(),
                    value: "yes".to_string(),
                    domain: "127.0.0.1".to_string(),
                    path: "/".to_string(),
                    secure: false,
                    http_only: false,
                    same_site: None,
                    expires: 0.0,
                }],
                storage: vec![StorageEntry {
                    origin: origin.clone(),
                    local: HashMap::from([("theme".to_string(), "dark".to_string())]),
                    session: HashMap::from([("auth".to_string(), "ready".to_string())]),
                }],
            })
            .await?;

        session.goto(&format!("{origin}/page")).await?;
        assert!(
            session
                .wait_selector("body[data-ready='true']", 10_000)
                .await?
        );
        assert_eq!(
            session
                .evaluate("localStorage.getItem('theme')")
                .await?
                .as_str(),
            Some("dark")
        );
        assert_eq!(
            session
                .evaluate("sessionStorage.getItem('auth')")
                .await?
                .as_str(),
            Some("ready")
        );
        assert_eq!(
            session
                .evaluate("document.cookie.includes('handoff=yes')")
                .await?,
            Value::Bool(true)
        );

        session.close().await?;
        server.abort();
        Ok(())
    }
}
