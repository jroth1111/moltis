//! Patchright subprocess probe for challenge-page fallback.

use std::process::Stdio;

use serde::Deserialize;
use tokio::{process::Command, time::Duration};

use crate::{error::Error, types::PatchrightFallbackConfig};

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
pub struct PatchrightProbe {
    pub final_url: String,
    pub title_len: usize,
    pub body_text_len: usize,
    pub cookies: Vec<PatchrightCookie>,
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
}

/// Run a one-shot Patchright navigation probe and return cookies/metrics.
pub async fn run_patchright_probe(
    url: &str,
    config: &PatchrightFallbackConfig,
    headless: bool,
    display: Option<&str>,
) -> Result<PatchrightProbe, Error> {
    let mut cmd = Command::new(config.python_binary.trim());
    cmd.kill_on_drop(true);
    if let Some(display) = display {
        cmd.env("DISPLAY", display);
    }
    cmd.arg("-c")
        .arg(PATCHRIGHT_PROBE_PY)
        .arg(url)
        .arg(if headless { "1" } else { "0" })
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
    })
}

const PATCHRIGHT_PROBE_PY: &str = r#"
import json
import sys
import time

url = sys.argv[1]
headless = sys.argv[2] == "1"

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
        browser = p.chromium.launch(headless=headless)
        context = browser.new_context()
        page = context.new_page()
        page.goto(url, wait_until="domcontentloaded", timeout=45000)

        stable_reads = 0
        prev_len = None
        for _ in range(30):
            html = page.content()
            if not _is_challenge(html):
                break
            cur_len = len(html)
            if prev_len == cur_len:
                stable_reads += 1
            else:
                stable_reads = 0
            prev_len = cur_len
            if stable_reads >= 20:
                break
            time.sleep(1)

        text_len = page.evaluate(\"\"\"(() => {
            const text = (document.body?.innerText || '').replace(/\\s+/g, ' ').trim();
            return text.length;
        })()\"\"\") or 0
        title = (page.title() or '').strip()
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
