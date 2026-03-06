use std::process::Stdio;

use {
    serde::{Deserialize, Serialize},
    serde_json::Value,
    tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
        process::{Child, ChildStdin, ChildStdout, Command},
        time::Duration,
    },
};

use crate::{error::Error, protection::PatchrightLaunchProfile, types::ProtectionConfig};

#[derive(Debug, Clone)]
pub struct PatchrightPageCapture {
    pub final_url: String,
    pub title_len: usize,
    pub body_text_len: usize,
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

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "moltis_browser::patchright", line, "patchright stderr");
                }
            });
        }

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
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
        let html = value
            .get("html")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(PatchrightPageCapture {
            final_url,
            title_len,
            body_text_len,
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

const PATCHRIGHT_SESSION_PY: &str = r#"
import base64
import fnmatch
import json
import platform
import sys
from urllib.parse import urlparse

launch_options = json.loads(sys.argv[1]) if len(sys.argv) > 1 else {}
channel = launch_options.get("channel")
browser_path = launch_options.get("executable_path")
viewport_width = int(launch_options.get("viewport_width") or 2560)
viewport_height = int(launch_options.get("viewport_height") or 1440)
device_scale_factor = float(launch_options.get("device_scale_factor") or 1.0)
locale = launch_options.get("locale") or "en-US"
user_agent_override = launch_options.get("user_agent")

STEALTH_ARGS = [
    "--disable-blink-features=AutomationControlled",
    "--no-sandbox",
    "--disable-setuid-sandbox",
]

def _accept_language(locale):
    normalized = (locale or "en-US").replace("_", "-")
    base = normalized.split("-")[0]
    return f"{normalized},{base};q=0.9"

def _default_user_agent(version):
    major = "120"
    if version:
        major = version.split(".", 1)[0] or major
    chrome_version = f"{major}.0.0.0"
    system = platform.system().lower()
    if system == "darwin":
        platform_token = "Macintosh; Intel Mac OS X 10_15_7"
    elif system == "windows":
        platform_token = "Windows NT 10.0; Win64; x64"
    else:
        platform_token = "X11; Linux x86_64"
    return (
        f"Mozilla/5.0 ({platform_token}) AppleWebKit/537.36 "
        f"(KHTML, like Gecko) Chrome/{chrome_version} Safari/537.36"
    )

def _emit(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

def _result(id, result=None):
    _emit({"id": id, "ok": True, "result": result if result is not None else {}})

def _error(id, error):
    _emit({"id": id, "ok": False, "error": str(error), "result": {}})

try:
    from patchright.sync_api import TimeoutError as PlaywrightTimeoutError, sync_playwright
except Exception as e:
    _emit({"id": 0, "ok": False, "error": f"import patchright failed: {e}", "result": {}})
    sys.exit(0)

with sync_playwright() as p:
    launch_kwargs = {"headless": True, "args": STEALTH_ARGS}
    if channel:
        launch_kwargs["channel"] = channel
    if browser_path:
        launch_kwargs["executable_path"] = browser_path
    browser = p.chromium.launch(**launch_kwargs)
    user_agent = user_agent_override or _default_user_agent(getattr(browser, "version", ""))
    context = browser.new_context(
        user_agent=user_agent,
        locale=locale,
        viewport={"width": viewport_width, "height": viewport_height},
        screen={"width": viewport_width, "height": viewport_height},
        device_scale_factor=device_scale_factor,
        extra_http_headers={"Accept-Language": _accept_language(locale)},
    )

    tabs = {}
    tabs["main"] = context.new_page()
    active_tab = "main"
    capture_config = None
    capture_pending = {}
    capture_completed = []
    capture_attached_pages = set()

    def current_page():
        return tabs[active_tab]

    def _normalize_allowed_host(host):
        return (host or "").strip().strip(".").lower()

    def _host_allowed(url, allowed_hosts):
        if not allowed_hosts:
            return True
        try:
            parsed = urlparse(url)
        except Exception:
            return False
        host = (parsed.hostname or "").lower()
        if not host:
            return False
        for candidate in (_normalize_allowed_host(value) for value in allowed_hosts):
            if not candidate:
                continue
            if host == candidate:
                return True
            if ":" not in candidate and host.endswith("." + candidate):
                return True
        return False

    def _matches_patterns(url, patterns):
        if not patterns:
            return True
        return any(fnmatch.fnmatch(url, pattern) for pattern in patterns)

    def _should_capture_request(request):
        if capture_config is None:
            return False
        resource_type = (getattr(request, "resource_type", "") or "").lower()
        if resource_type == "document":
            if not capture_config.get("include_document_requests"):
                return False
        elif resource_type not in ("fetch", "xhr", "eventsource", "other", ""):
            return False
        return _host_allowed(request.url, capture_config.get("allowed_hosts") or []) and _matches_patterns(
            request.url,
            capture_config.get("url_patterns") or [],
        )

    def _request_headers(request):
        try:
            headers = request.headers or {}
        except Exception:
            headers = {}
        return [[str(name), str(value)] for name, value in headers.items()]

    def _request_content_type(headers):
        for name, value in headers:
            if str(name).lower() == "content-type":
                return str(value)
        return None

    def _record_from_request(request):
        headers = _request_headers(request)
        try:
            body = request.post_data
        except Exception:
            body = None
        return {
            "request_id": f"pw-{id(request)}",
            "method": str(request.method),
            "url": str(request.url),
            "request_headers": headers,
            "request_body": body,
            "request_content_type": _request_content_type(headers),
            "resource_type": getattr(request, "resource_type", None),
            "status": None,
            "response_content_type": None,
        }

    def _capture_key(request):
        return str(id(request))

    def _on_request(request):
        if not _should_capture_request(request):
            return
        capture_pending[_capture_key(request)] = _record_from_request(request)

    def _on_response(response):
        request = response.request
        record = capture_pending.get(_capture_key(request))
        if record is None:
            return
        try:
            headers = response.headers or {}
        except Exception:
            headers = {}
        record["status"] = int(getattr(response, "status", 0) or 0) or None
        record["response_content_type"] = headers.get("content-type")

    def _finalize_request(request):
        record = capture_pending.pop(_capture_key(request), None)
        if record is not None:
            capture_completed.append(record)

    def _attach_capture_page(page):
        page_key = str(id(page))
        if page_key in capture_attached_pages:
            return
        capture_attached_pages.add(page_key)
        page.on("request", _on_request)
        page.on("response", _on_response)
        page.on("requestfinished", _finalize_request)
        page.on("requestfailed", _finalize_request)

    context.on("page", _attach_capture_page)
    _attach_capture_page(tabs["main"])

    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        try:
            req = json.loads(raw)
            cmd = req.get("cmd")
            id = req.get("id", 0)

            if cmd == "goto":
                current_page().goto(req["url"], wait_until="domcontentloaded", timeout=45000)
                _result(id)
            elif cmd == "capture_page":
                page = current_page()
                title = (page.evaluate("document.title || ''") or "").strip()
                body_text = page.evaluate("""(() => {
                    const text = (document.body?.innerText || '').replace(/\\s+/g, ' ').trim();
                    return text.length;
                })()""") or 0
                _result(id, {
                    "final_url": page.url,
                    "title": title,
                    "title_len": len(title),
                    "body_text_len": int(body_text),
                    "html": page.content(),
                })
            elif cmd == "evaluate":
                _result(id, current_page().evaluate(req["code"]))
            elif cmd == "screenshot":
                data = current_page().screenshot(full_page=bool(req.get("full_page")))
                _result(id, {"data_base64": base64.b64encode(data).decode("ascii")})
            elif cmd == "wait_selector":
                try:
                    current_page().locator(req["selector"]).wait_for(
                        state="attached",
                        timeout=int(req.get("timeout_ms") or 30000),
                    )
                    _result(id, {"found": True})
                except PlaywrightTimeoutError:
                    _result(id, {"found": False})
            elif cmd == "mouse_move":
                current_page().mouse.move(float(req["x"]), float(req["y"]))
                _result(id)
            elif cmd == "mouse_click":
                current_page().mouse.click(
                    float(req["x"]),
                    float(req["y"]),
                    click_count=int(req.get("click_count") or 1),
                )
                _result(id)
            elif cmd == "keyboard_type":
                current_page().keyboard.type(req["text"])
                _result(id)
            elif cmd == "keyboard_press":
                current_page().keyboard.press(req["key"])
                _result(id)
            elif cmd == "select_option":
                current_page().locator(req["selector"]).select_option(req["value"])
                _result(id)
            elif cmd == "check":
                current_page().locator(req["selector"]).check()
                _result(id)
            elif cmd == "uncheck":
                current_page().locator(req["selector"]).uncheck()
                _result(id)
            elif cmd == "clear":
                current_page().locator(req["selector"]).fill("")
                _result(id)
            elif cmd == "set_input_files":
                current_page().locator(req["selector"]).set_input_files(req["path"])
                _result(id)
            elif cmd == "get_url":
                _result(id, {"url": current_page().url})
            elif cmd == "get_title":
                _result(id, {"title": current_page().evaluate("document.title || ''") or ""})
            elif cmd == "back":
                current_page().go_back(wait_until="domcontentloaded", timeout=45000)
                _result(id)
            elif cmd == "forward":
                current_page().go_forward(wait_until="domcontentloaded", timeout=45000)
                _result(id)
            elif cmd == "refresh":
                current_page().reload(wait_until="domcontentloaded", timeout=45000)
                _result(id)
            elif cmd == "start_api_capture":
                capture_config = {
                    "allowed_hosts": req.get("allowed_hosts") or [],
                    "url_patterns": req.get("url_patterns") or [],
                    "include_document_requests": bool(req.get("include_document_requests")),
                    "max_examples_per_endpoint": int(req.get("max_examples_per_endpoint") or 3),
                }
                capture_pending = {}
                capture_completed = []
                for page in tabs.values():
                    _attach_capture_page(page)
                _result(id)
            elif cmd == "stop_api_capture":
                capture_completed.extend(capture_pending.values())
                capture_pending = {}
                capture_config = None
                _result(id, {"records": capture_completed})
            elif cmd == "new_tab":
                name = req["name"]
                if name in tabs:
                    raise RuntimeError(f"tab '{name}' already exists")
                tabs[name] = context.new_page()
                _attach_capture_page(tabs[name])
                active_tab = name
                _result(id)
            elif cmd == "list_tabs":
                _result(id, {"tabs": list(tabs.keys()), "active": active_tab})
            elif cmd == "switch_tab":
                name = req["name"]
                if name not in tabs:
                    raise RuntimeError(f"tab '{name}' not found")
                active_tab = name
                tabs[name].bring_to_front()
                _result(id)
            elif cmd == "close_tab":
                name = req["name"]
                if name == "main":
                    raise RuntimeError("cannot close the main tab")
                if name not in tabs:
                    raise RuntimeError(f"tab '{name}' not found")
                tabs[name].close()
                del tabs[name]
                if active_tab == name:
                    active_tab = "main"
                    tabs["main"].bring_to_front()
                _result(id)
            elif cmd == "close":
                _result(id)
                break
            else:
                raise RuntimeError(f"unsupported command: {cmd}")
        except Exception as e:
            _error(id if 'id' in locals() else 0, e)

    try:
        context.close()
    finally:
        browser.close()
"#;
