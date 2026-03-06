//! Patchright subprocess probe for challenge-page fallback.

use std::process::Stdio;

use {
    serde::Deserialize,
    tokio::{process::Command, time::Duration},
};

use crate::{error::Error, protection::PatchrightLaunchProfile, types::PatchrightFallbackConfig};

#[derive(Debug, Clone, Deserialize)]
pub struct PatchrightCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default = "default_cookie_path")]
    pub path: String,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default)]
    pub expires: Option<f64>,
}

fn default_cookie_path() -> String {
    "/".to_string()
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PatchrightProbe {
    pub final_url: String,
    pub title_len: usize,
    pub body_text_len: usize,
    pub cookies: Vec<PatchrightCookie>,
    /// Full HTML content (for direct navigation mode)
    pub html: Option<String>,
    /// Page title text
    pub title: Option<String>,
    /// Body text content
    pub body_text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PatchrightProbeOutput {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    final_url: String,
    #[serde(default)]
    title_len: usize,
    #[serde(default)]
    body_text_len: usize,
    #[serde(default)]
    cookies: Vec<PatchrightCookie>,
    /// Full HTML content for direct navigation mode
    #[serde(default)]
    html: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body_text: Option<String>,
}

/// Run a one-shot Patchright navigation probe and return cookies/metrics.
pub async fn run_patchright_probe(
    url: &str,
    config: &PatchrightFallbackConfig,
    headless: bool,
    launch_profile: &PatchrightLaunchProfile,
) -> Result<PatchrightProbe, Error> {
    let mut cmd = Command::new(config.python_binary.trim());
    cmd.kill_on_drop(true);
    let launch_options = serde_json::to_string(launch_profile).map_err(|e| {
        Error::NavigationFailed(format!("failed to encode patchright options: {e}"))
    })?;
    cmd.arg("-c")
        .arg(PATCHRIGHT_PROBE_PY)
        .arg(url)
        .arg(if headless {
            "1"
        } else {
            "0"
        })
        .arg(launch_options)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = tokio::time::timeout(
        Duration::from_millis(config.timeout_ms.max(1000)),
        cmd.output(),
    )
    .await
    .map_err(|_| {
        Error::NavigationFailed(format!(
            "patchright fallback timed out after {} ms",
            config.timeout_ms
        ))
    })?
    .map_err(|e| Error::NavigationFailed(format!("failed to run patchright fallback: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if stdout.is_empty() {
        let status = output.status;
        return Err(Error::NavigationFailed(format!(
            "patchright fallback produced no output (status: {status}; stderr: {stderr})"
        )));
    }

    let parsed: PatchrightProbeOutput = serde_json::from_str(&stdout).map_err(|e| {
        Error::NavigationFailed(format!(
            "invalid patchright fallback output: {e}; stdout: {stdout}; stderr: {stderr}"
        ))
    })?;

    if !parsed.ok {
        return Err(Error::NavigationFailed(format!(
            "patchright fallback failed: {}",
            parsed
                .error
                .unwrap_or_else(|| "unknown patchright error".to_string())
        )));
    }

    Ok(PatchrightProbe {
        final_url: parsed.final_url,
        title_len: parsed.title_len,
        body_text_len: parsed.body_text_len,
        cookies: parsed.cookies,
        html: parsed.html,
        title: parsed.title,
        body_text: parsed.body_text,
    })
}

/// Run Patchright probe with retry logic and exponential backoff.
pub async fn run_patchright_probe_with_retry(
    url: &str,
    config: &PatchrightFallbackConfig,
    headless: bool,
    launch_profile: &PatchrightLaunchProfile,
    max_retries: u32,
) -> Result<PatchrightProbe, Error> {
    let mut last_error = None;

    for attempt in 0..=max_retries {
        if attempt > 0 {
            let backoff_ms = 500 * (2_u64.pow(attempt));
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        }

        match run_patchright_probe(url, config, headless, launch_profile).await {
            Ok(probe) => return Ok(probe),
            Err(e) => {
                last_error = Some(e);
            },
        }
    }

    Err(last_error.unwrap_or_else(|| Error::NavigationFailed("patchright retry exhausted".into())))
}

const PATCHRIGHT_PROBE_PY: &str = r#"
import json
import platform
import sys
import time

url = sys.argv[1]
headless = sys.argv[2] == "1"
launch_options = json.loads(sys.argv[3]) if len(sys.argv) > 3 and sys.argv[3] else {}
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

def _is_challenge(html):
    l = (html or "").lower()
    markers = [
        "kpsdk",
        "ips.js?",
        "x-kpsdk",
        "_incapsula_resource",
        "incapsula incident id",
        "/_incap",
        "__cf_chl",
        "checking your browser",
        "verify you are human",
        "captcha challenge",
    ]
    return any(m in l for m in markers)

def _has_contentful_page(title, body_text):
    return len(body_text) >= 1000 or (len(title) >= 20 and len(body_text) >= 400)

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
    sys.stdout.write(json.dumps(payload))
    sys.stdout.flush()

try:
    from patchright.sync_api import sync_playwright
except Exception as e:
    _emit({"ok": False, "error": f"import patchright failed: {e}"})
    sys.exit(0)

try:
    with sync_playwright() as p:
        launch_kwargs = {"headless": headless, "args": STEALTH_ARGS}
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
            extra_http_headers={
                "Accept-Language": _accept_language(locale),
            },
        )
        page = context.new_page()
        page.goto(url, wait_until="domcontentloaded", timeout=45000)

        stable_reads = 0
        prev_len = None
        for _ in range(45):
            html = page.content()
            body_text = page.evaluate("""(() => {
                const text = (document.body?.innerText || '').replace(/\\s+/g, ' ').trim();
                return text;
            })()""") or ""
            title = (page.title() or '').strip()
            if _has_contentful_page(title, body_text) or not _is_challenge(html):
                break
            cur_len = len(html)
            if prev_len == cur_len:
                stable_reads += 1
            else:
                stable_reads = 0
            prev_len = cur_len
            if stable_reads >= 8:
                break
            time.sleep(1)

        body_text = page.evaluate("""(() => {
            const text = (document.body?.innerText || '').replace(/\\s+/g, ' ').trim();
            return text;
        })()""") or ""
        text_len = len(body_text)
        title = (page.title() or '').strip()
        html_content = page.content()
        cookies = []
        for c in context.cookies():
            cookies.append({
                "name": c.get("name", ""),
                "value": c.get("value", ""),
                "domain": c.get("domain", ""),
                "path": c.get("path", "/"),
                "secure": bool(c.get("secure", False)),
                "http_only": bool(c.get("httpOnly", False)),
                "expires": c.get("expires", None),
            })

        _emit({
            "ok": True,
            "final_url": page.url,
            "title_len": len(title),
            "body_text_len": int(text_len),
            "cookies": cookies,
            "html": html_content,
            "title": title,
            "body_text": body_text,
        })

        context.close()
        browser.close()
except Exception as e:
    _emit({"ok": False, "error": str(e)})
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_path_defaults_to_root() {
        let cookie: PatchrightCookie = serde_json::from_value(serde_json::json!({
            "name": "a",
            "value": "b",
            "domain": ".example.com"
        }))
        .expect("cookie json");
        assert_eq!(cookie.path, "/");
    }
}
