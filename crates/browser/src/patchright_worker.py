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
    interception_enabled = False
    interception_patterns = []
    interception_extra_headers = {}
    pending_local_storage = {}
    pending_session_storage = {}

    def current_page():
        return tabs[active_tab]

    def _canonical_origin(value):
        try:
            parsed = urlparse(value or "")
        except Exception:
            return None
        if parsed.scheme not in ("http", "https") or not parsed.hostname:
            return None
        port = f":{parsed.port}" if parsed.port else ""
        return f"{parsed.scheme}://{parsed.hostname}{port}"

    def _normalize_storage_map(raw):
        if not isinstance(raw, dict):
            return {}
        return {
            str(key): "" if value is None else str(value)
            for key, value in raw.items()
            if key is not None
        }

    def _queue_state_restore(state):
        pending_local_storage.clear()
        pending_session_storage.clear()
        storage_entries = (state or {}).get("storage") or []
        for entry in storage_entries:
            origin = _canonical_origin((entry or {}).get("origin"))
            if not origin:
                continue
            local_items = _normalize_storage_map((entry or {}).get("local"))
            session_items = _normalize_storage_map((entry or {}).get("session"))
            if local_items:
                pending_local_storage.setdefault(origin, {}).update(local_items)
            if session_items:
                pending_session_storage.setdefault(origin, {}).update(session_items)

    def _apply_pending_storage(page):
        origin = _canonical_origin(getattr(page, "url", ""))
        if not origin:
            return False
        local_items = pending_local_storage.pop(origin, None) or {}
        session_items = pending_session_storage.pop(origin, None) or {}
        if not local_items and not session_items:
            return False
        page.evaluate(
            """([localItems, sessionItems]) => {
                for (const [key, value] of Object.entries(localItems || {})) {
                    try { window.localStorage.setItem(key, value ?? ""); } catch (_) {}
                }
                for (const [key, value] of Object.entries(sessionItems || {})) {
                    try { window.sessionStorage.setItem(key, value ?? ""); } catch (_) {}
                }
            }""",
            [local_items, session_items],
        )
        return True

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
            headers = dict(request.headers or {})
        except Exception:
            headers = {}
        if interception_enabled and _matches_patterns(str(request.url), interception_patterns):
            headers.update(interception_extra_headers)
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

    def _intercept_route(route, request):
        if not interception_enabled or not _matches_patterns(str(request.url), interception_patterns):
            route.continue_()
            return
        headers = dict(request.headers or {})
        headers.update(interception_extra_headers)
        route.continue_(headers=headers)

    context.route("**/*", _intercept_route)
    _result(0, {"ready": True})

    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        try:
            req = json.loads(raw)
            cmd = req.get("cmd")
            request_id = req.get("id", 0)

            if cmd == "goto":
                current_page().goto(req["url"], wait_until="domcontentloaded", timeout=45000)
                if _apply_pending_storage(current_page()):
                    current_page().reload(wait_until="domcontentloaded", timeout=45000)
                _result(request_id)
            elif cmd == "capture_page":
                page = current_page()
                title = (page.evaluate("document.title || ''") or "").strip()
                diagnostics = page.evaluate("""(() => {
                    const text = (document.body?.innerText || '').replace(/\\s+/g, ' ').trim();
                    const interactiveSelector = [
                        'a[href]',
                        'button',
                        'input:not([type="hidden"])',
                        'select',
                        'textarea',
                        '[role="button"]',
                        '[contenteditable="true"]',
                        'summary'
                    ].join(',');
                    return {
                        body_text_len: text.length,
                        interactive_element_count: document.querySelectorAll(interactiveSelector).length,
                    };
                })()""") or {}
                _result(request_id, {
                    "final_url": page.url,
                    "title": title,
                    "title_len": len(title),
                    "body_text_len": int(diagnostics.get("body_text_len") or 0),
                    "interactive_element_count": int(diagnostics.get("interactive_element_count") or 0),
                    "html": page.content(),
                })
            elif cmd == "evaluate":
                _result(request_id, current_page().evaluate(req["code"]))
            elif cmd == "screenshot":
                data = current_page().screenshot(full_page=bool(req.get("full_page")))
                _result(request_id, {"data_base64": base64.b64encode(data).decode("ascii")})
            elif cmd == "restore_state":
                state = req.get("state") or {}
                cookies = []
                for cookie in state.get("cookies") or []:
                    item = {
                        "name": str(cookie.get("name", "")),
                        "value": str(cookie.get("value", "")),
                        "path": str(cookie.get("path", "/") or "/"),
                        "secure": bool(cookie.get("secure", False)),
                        "httpOnly": bool(cookie.get("http_only", False)),
                    }
                    domain = str(cookie.get("domain", "") or "")
                    if domain:
                        item["domain"] = domain
                    else:
                        url = str(state.get("url", "") or "")
                        if url:
                            item["url"] = url
                    expires = cookie.get("expires")
                    if isinstance(expires, (int, float)) and expires > 0:
                        item["expires"] = float(expires)
                    if item.get("name") and item.get("value") and (item.get("domain") or item.get("url")):
                        cookies.append(item)
                if cookies:
                    context.add_cookies(cookies)
                _queue_state_restore(state)
                _result(request_id, {
                    "cookies": len(cookies),
                    "origins": len(pending_local_storage) + len(pending_session_storage),
                })
            elif cmd == "wait_selector":
                try:
                    current_page().locator(req["selector"]).wait_for(
                        state="attached",
                        timeout=int(req.get("timeout_ms") or 30000),
                    )
                    _result(request_id, {"found": True})
                except PlaywrightTimeoutError:
                    _result(request_id, {"found": False})
            elif cmd == "mouse_move":
                current_page().mouse.move(float(req["x"]), float(req["y"]))
                _result(request_id)
            elif cmd == "mouse_click":
                current_page().mouse.click(
                    float(req["x"]),
                    float(req["y"]),
                    click_count=int(req.get("click_count") or 1),
                )
                _result(request_id)
            elif cmd == "keyboard_type":
                current_page().keyboard.type(req["text"])
                _result(request_id)
            elif cmd == "keyboard_press":
                current_page().keyboard.press(req["key"])
                _result(request_id)
            elif cmd == "select_option":
                current_page().locator(req["selector"]).select_option(req["value"])
                _result(request_id)
            elif cmd == "check":
                current_page().locator(req["selector"]).check()
                _result(request_id)
            elif cmd == "uncheck":
                current_page().locator(req["selector"]).uncheck()
                _result(request_id)
            elif cmd == "clear":
                current_page().locator(req["selector"]).fill("")
                _result(request_id)
            elif cmd == "set_input_files":
                current_page().locator(req["selector"]).set_input_files(req["path"])
                _result(request_id)
            elif cmd == "get_url":
                _result(request_id, {"url": current_page().url})
            elif cmd == "get_title":
                _result(request_id, {"title": current_page().evaluate("document.title || ''") or ""})
            elif cmd == "back":
                current_page().go_back(wait_until="domcontentloaded", timeout=45000)
                _result(request_id)
            elif cmd == "forward":
                current_page().go_forward(wait_until="domcontentloaded", timeout=45000)
                _result(request_id)
            elif cmd == "refresh":
                current_page().reload(wait_until="domcontentloaded", timeout=45000)
                _result(request_id)
            elif cmd == "enable_interception":
                interception_enabled = True
                interception_patterns = [str(pattern) for pattern in (req.get("patterns") or [])]
                interception_extra_headers = {
                    str(name): str(value)
                    for name, value in (req.get("extra_headers") or {}).items()
                }
                _result(request_id)
            elif cmd == "disable_interception":
                interception_enabled = False
                interception_patterns = []
                interception_extra_headers = {}
                _result(request_id)
            elif cmd == "set_extra_headers":
                interception_extra_headers = {
                    str(name): str(value)
                    for name, value in (req.get("headers") or {}).items()
                }
                _result(request_id)
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
                _result(request_id)
            elif cmd == "stop_api_capture":
                capture_completed.extend(capture_pending.values())
                capture_pending = {}
                capture_config = None
                _result(request_id, {"records": capture_completed})
            elif cmd == "new_tab":
                name = req["name"]
                if name in tabs:
                    raise RuntimeError(f"tab '{name}' already exists")
                tabs[name] = context.new_page()
                _attach_capture_page(tabs[name])
                active_tab = name
                _result(request_id)
            elif cmd == "list_tabs":
                _result(request_id, {"tabs": list(tabs.keys()), "active": active_tab})
            elif cmd == "switch_tab":
                name = req["name"]
                if name not in tabs:
                    raise RuntimeError(f"tab '{name}' not found")
                active_tab = name
                tabs[name].bring_to_front()
                _result(request_id)
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
                _result(request_id)
            elif cmd == "close":
                _result(request_id)
                break
            else:
                raise RuntimeError(f"unsupported command: {cmd}")
        except Exception as e:
            _error(request_id if 'request_id' in locals() else 0, e)

    try:
        context.close()
    finally:
        browser.close()
