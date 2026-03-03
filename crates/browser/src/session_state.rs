//! Browser session state capture and persistence.
//!
//! Captures cookies and localStorage/sessionStorage from a live browser page
//! and saves them to disk (optionally encrypted). States can be restored to a
//! new page to resume a previous session.

use std::{collections::HashMap, path::PathBuf};

use {
    chromiumoxide::{
        Page,
        cdp::browser_protocol::network::{
            Cookie, CookieParam, GetCookiesParams, SetCookiesParams, TimeSinceEpoch,
        },
    },
    serde::{Deserialize, Serialize},
    tracing::{debug, warn},
};

use crate::error::Error;

/// A cookie captured from the browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CookieEntry {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: Option<String>,
    pub expires: f64,
}

impl From<&Cookie> for CookieEntry {
    fn from(c: &Cookie) -> Self {
        Self {
            name: c.name.clone(),
            value: c.value.clone(),
            domain: c.domain.clone(),
            path: c.path.clone(),
            secure: c.secure,
            http_only: c.http_only,
            same_site: c.same_site.as_ref().map(|s| s.as_ref().to_string()),
            expires: c.expires,
        }
    }
}

impl From<&CookieEntry> for CookieParam {
    fn from(e: &CookieEntry) -> Self {
        Self {
            name: e.name.clone(),
            value: e.value.clone(),
            url: None,
            domain: Some(e.domain.clone()),
            path: Some(e.path.clone()),
            secure: Some(e.secure),
            http_only: Some(e.http_only),
            same_site: None,
            expires: if e.expires > 0.0 {
                Some(TimeSinceEpoch::new(e.expires))
            } else {
                None
            },
            priority: None,
            same_party: None,
            source_scheme: None,
            source_port: None,
            partition_key: None,
        }
    }
}

/// localStorage and sessionStorage for a single origin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageEntry {
    /// Origin (scheme + host + port).
    pub origin: String,
    /// localStorage key→value map.
    pub local: HashMap<String, String>,
    /// sessionStorage key→value map.
    pub session: HashMap<String, String>,
}

/// Complete browser session state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    /// Schema version for forward-compatibility.
    pub version: u8,
    /// ISO-8601 timestamp when the snapshot was taken.
    pub captured_at: String,
    /// Page URL at capture time.
    pub url: String,
    /// Captured cookies.
    pub cookies: Vec<CookieEntry>,
    /// Captured storage entries.
    pub storage: Vec<StorageEntry>,
}

// ── Capture & restore ────────────────────────────────────────────────────────

/// Capture the current session state from `page`.
///
/// Retrieves all cookies visible to the page and dumps localStorage /
/// sessionStorage for the current origin.
pub async fn capture_state(page: &Page) -> Result<SessionState, Error> {
    // Get current URL.
    let url = page
        .url()
        .await
        .map_err(|e| Error::Cdp(format!("get_url failed: {e}")))?
        .unwrap_or_default();

    // Fetch all cookies (no URL filter = all cookies for all frames).
    let get_cookies = GetCookiesParams::default();
    let cookies_ret = page
        .execute(get_cookies)
        .await
        .map_err(|e| Error::Cdp(format!("Network.getCookies failed: {e}")))?;

    let cookies: Vec<CookieEntry> = cookies_ret
        .result
        .cookies
        .iter()
        .map(CookieEntry::from)
        .collect();

    // Dump localStorage and sessionStorage for the current origin.
    let storage_js = r#"
        (function() {
            function dumpStorage(s) {
                var out = {};
                for (var i = 0; i < s.length; i++) {
                    var k = s.key(i);
                    out[k] = s.getItem(k);
                }
                return out;
            }
            return JSON.stringify({
                origin: window.location.origin,
                local: dumpStorage(window.localStorage),
                session: dumpStorage(window.sessionStorage)
            });
        })()
    "#;

    let js_result = page
        .evaluate(storage_js)
        .await
        .map_err(|e| Error::Cdp(format!("storage dump eval failed: {e}")))?;

    let storage_entry = js_result
        .value()
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .map(|obj| {
            let origin = obj["origin"].as_str().unwrap_or("").to_string();
            let local = obj["local"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let session = obj["session"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                        .collect()
                })
                .unwrap_or_default();
            StorageEntry {
                origin,
                local,
                session,
            }
        });

    let now = time::OffsetDateTime::now_utc();
    let captured_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());

    Ok(SessionState {
        version: 1,
        captured_at,
        url,
        cookies,
        storage: storage_entry.into_iter().collect(),
    })
}

/// Restore a previously captured session state into `page`.
///
/// Sets cookies and restores localStorage/sessionStorage entries.
pub async fn restore_state(page: &Page, state: &SessionState) -> Result<(), Error> {
    // Restore cookies.
    if !state.cookies.is_empty() {
        let cookie_params: Vec<CookieParam> = state.cookies.iter().map(CookieParam::from).collect();
        let set_cookies = SetCookiesParams::new(cookie_params);
        page.execute(set_cookies)
            .await
            .map_err(|e| Error::Cdp(format!("Network.setCookies failed: {e}")))?;
        debug!(count = state.cookies.len(), "restored cookies");
    }

    // Restore storage entries.
    for entry in &state.storage {
        // Restore localStorage.
        if !entry.local.is_empty() {
            let pairs: Vec<String> = entry
                .local
                .iter()
                .map(|(k, v)| {
                    format!(
                        "localStorage.setItem({}, {})",
                        serde_json::to_string(k).unwrap_or_default(),
                        serde_json::to_string(v).unwrap_or_default(),
                    )
                })
                .collect();
            let js = pairs.join(";");
            if let Err(e) = page.evaluate(js).await {
                warn!(origin = %entry.origin, error = %e, "failed to restore localStorage");
            }
        }
        // Restore sessionStorage.
        if !entry.session.is_empty() {
            let pairs: Vec<String> = entry
                .session
                .iter()
                .map(|(k, v)| {
                    format!(
                        "sessionStorage.setItem({}, {})",
                        serde_json::to_string(k).unwrap_or_default(),
                        serde_json::to_string(v).unwrap_or_default(),
                    )
                })
                .collect();
            let js = pairs.join(";");
            if let Err(e) = page.evaluate(js).await {
                warn!(origin = %entry.origin, error = %e, "failed to restore sessionStorage");
            }
        }
    }

    Ok(())
}

// ── Disk persistence ─────────────────────────────────────────────────────────

/// Return the directory where session files are stored.
pub fn sessions_dir() -> PathBuf {
    moltis_config::data_dir().join("browser").join("sessions")
}

/// Persist `state` to disk as `<name>.json` in the sessions directory.
///
/// When `encrypt` is `true` and the `session-encrypt` Cargo feature is enabled,
/// the JSON is XChaCha20-Poly1305 encrypted before writing.
/// When the feature is not enabled and `encrypt=true`, returns an error.
pub fn save_to_disk(state: &SessionState, name: &str, encrypt: bool) -> Result<PathBuf, Error> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::InvalidAction(format!("cannot create sessions dir: {e}")))?;

    let json = serde_json::to_string_pretty(state)
        .map_err(|e| Error::InvalidAction(format!("serialise session state: {e}")))?;

    let content = if encrypt {
        encrypt_state(&json, name)?
    } else {
        json
    };

    let path = dir.join(format!("{}.json", sanitise_name(name)));
    std::fs::write(&path, content)
        .map_err(|e| Error::InvalidAction(format!("write session file: {e}")))?;

    #[cfg(feature = "metrics")]
    moltis_metrics::counter!(moltis_metrics::browser::STATE_SAVES_TOTAL).increment(1);

    Ok(path)
}

/// Load a session state from disk by name.
///
/// Automatically detects whether the file is encrypted (starts with `"ENC:"`)
/// and decrypts it if needed.
pub fn load_from_disk(name: &str) -> Result<SessionState, Error> {
    let path = sessions_dir().join(format!("{}.json", sanitise_name(name)));

    let content = std::fs::read_to_string(&path).map_err(|e| {
        Error::InvalidAction(format!("read session file '{}': {e}", path.display()))
    })?;

    let json = if content.starts_with("ENC:") {
        decrypt_state(&content, name)?
    } else {
        content
    };

    let state = serde_json::from_str::<SessionState>(&json)
        .map_err(|e| Error::InvalidAction(format!("deserialise session state: {e}")))?;

    #[cfg(feature = "metrics")]
    moltis_metrics::counter!(moltis_metrics::browser::STATE_LOADS_TOTAL).increment(1);

    Ok(state)
}

/// List saved session names.
pub fn list_saved() -> Result<Vec<String>, Error> {
    let dir = sessions_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut names = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| Error::InvalidAction(format!("list sessions dir: {e}")))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            names.push(stem.to_string());
        }
    }

    names.sort();
    Ok(names)
}

/// Delete a saved session by name.
pub fn delete_saved(name: &str) -> Result<(), Error> {
    let path = sessions_dir().join(format!("{}.json", sanitise_name(name)));
    std::fs::remove_file(&path)
        .map_err(|e| Error::InvalidAction(format!("delete session '{}': {e}", name)))?;
    Ok(())
}

// ── Encryption helpers ────────────────────────────────────────────────────────

/// Replace filesystem-unsafe characters in a session name.
fn sanitise_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

/// Encrypt `json` using XChaCha20-Poly1305, returning an `"ENC:<base64>"` envelope.
///
/// Requires the `session-encrypt` feature; returns an error otherwise.
fn encrypt_state(json: &str, name: &str) -> Result<String, Error> {
    #[cfg(feature = "session-encrypt")]
    {
        use {
            base64::{Engine, engine::general_purpose::STANDARD as BASE64},
            moltis_vault::{Cipher, XChaCha20Poly1305Cipher},
        };

        let key = derive_key(name);
        let cipher = XChaCha20Poly1305Cipher;
        let ciphertext = cipher
            .encrypt(&key, json.as_bytes(), name.as_bytes())
            .map_err(|e| Error::InvalidAction(format!("encrypt session: {e}")))?;
        Ok(format!("ENC:{}", BASE64.encode(&ciphertext)))
    }

    #[cfg(not(feature = "session-encrypt"))]
    {
        let _ = (json, name);
        Err(Error::InvalidAction(
            "session-encrypt feature not enabled; recompile with `--features session-encrypt`"
                .to_string(),
        ))
    }
}

/// Decrypt an `"ENC:<base64>"` envelope.
fn decrypt_state(content: &str, name: &str) -> Result<String, Error> {
    #[cfg(feature = "session-encrypt")]
    {
        use {
            base64::{Engine, engine::general_purpose::STANDARD as BASE64},
            moltis_vault::{Cipher, XChaCha20Poly1305Cipher},
        };

        let b64 = content
            .strip_prefix("ENC:")
            .ok_or_else(|| Error::InvalidAction("invalid encrypted envelope".to_string()))?;
        let ciphertext = BASE64
            .decode(b64)
            .map_err(|e| Error::InvalidAction(format!("base64 decode session: {e}")))?;
        let key = derive_key(name);
        let cipher = XChaCha20Poly1305Cipher;
        let plaintext = cipher
            .decrypt(&key, &ciphertext, name.as_bytes())
            .map_err(|e| Error::InvalidAction(format!("decrypt session: {e}")))?;
        String::from_utf8(plaintext)
            .map_err(|e| Error::InvalidAction(format!("session not valid UTF-8: {e}")))
    }

    #[cfg(not(feature = "session-encrypt"))]
    {
        let _ = (content, name);
        Err(Error::InvalidAction(
            "session-encrypt feature not enabled; cannot decrypt session file".to_string(),
        ))
    }
}

/// Derive a deterministic 32-byte key from a session name using SHA-256.
///
/// This is intentionally simple: the same name always produces the same key on
/// any machine. It provides basic obfuscation rather than strong per-user
/// security (which would require the vault's unseal mechanism).
// TODO(security): Replace SHA-256(salt||name) with a proper KDF.
// Options: HKDF-SHA256 with vault unseal key, or argon2id with per-install secret.
// Currently `session-encrypt` is intentionally excluded from default features until hardened.
#[cfg(feature = "session-encrypt")]
fn derive_key(name: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const SALT: &[u8] = b"moltis-browser-session-v1";
    let mut hasher = Sha256::new();
    hasher.update(SALT);
    hasher.update(name.as_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state(url: &str) -> SessionState {
        SessionState {
            version: 1,
            captured_at: "2024-01-01T00:00:00Z".to_string(),
            url: url.to_string(),
            cookies: vec![CookieEntry {
                name: "session".to_string(),
                value: "abc123".to_string(),
                domain: "example.com".to_string(),
                path: "/".to_string(),
                secure: true,
                http_only: true,
                same_site: Some("Strict".to_string()),
                expires: -1.0,
            }],
            storage: vec![StorageEntry {
                origin: "https://example.com".to_string(),
                local: {
                    let mut m = HashMap::new();
                    m.insert("theme".to_string(), "dark".to_string());
                    m
                },
                session: HashMap::new(),
            }],
        }
    }

    #[test]
    fn test_session_state_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let state = make_state("https://example.com");
        let json = serde_json::to_string(&state)?;
        let decoded: SessionState = serde_json::from_str(&json)?;

        assert_eq!(decoded.url, "https://example.com");
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.cookies.len(), 1);
        assert_eq!(decoded.cookies[0].name, "session");
        assert_eq!(decoded.storage[0].local["theme"], "dark");
        Ok(())
    }

    #[test]
    fn test_save_load_disk_unencrypted() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        // Since sessions_dir() is hardcoded, we test the serde round-trip
        // by writing/reading directly to a temp path.
        let state = make_state("https://save-load.test");
        let json = serde_json::to_string_pretty(&state)?;
        let name = "test-session";
        let path = dir.path().join(format!("{name}.json"));
        std::fs::write(&path, &json)?;

        let loaded: SessionState = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert_eq!(loaded.url, "https://save-load.test");
        assert_eq!(loaded.cookies[0].value, "abc123");
        Ok(())
    }

    #[test]
    fn test_sanitise_name_replaces_unsafe_chars() {
        let safe = sanitise_name("my session/2024");
        assert_eq!(safe, "my_session_2024");
    }

    #[cfg(feature = "session-encrypt")]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn session_state_encrypt_decrypt_round_trip() {
        let state = make_state("https://encrypt-roundtrip.test");

        // Use a unique name to avoid collisions with other tests.
        let name = "test-encrypt-rt";

        // Save encrypted.
        let path = save_to_disk(&state, name, true).unwrap();

        // The file on disk should start with "ENC:".
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.starts_with("ENC:"),
            "encrypted file must start with ENC: prefix"
        );

        // Load and verify round-trip fidelity.
        let loaded = load_from_disk(name).unwrap();
        assert_eq!(loaded.version, state.version);
        assert_eq!(loaded.url, state.url);
        assert_eq!(loaded.captured_at, state.captured_at);
        assert_eq!(loaded.cookies.len(), 1);
        assert_eq!(loaded.cookies[0].name, "session");
        assert_eq!(loaded.cookies[0].value, "abc123");
        assert_eq!(loaded.storage[0].local["theme"], "dark");

        // Clean up.
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_cookie_entry_from_roundtrip_fields() {
        let entry = CookieEntry {
            name: "tok".to_string(),
            value: "xyz".to_string(),
            domain: ".example.com".to_string(),
            path: "/".to_string(),
            secure: false,
            http_only: false,
            same_site: None,
            expires: -1.0,
        };
        let param = CookieParam::from(&entry);
        assert_eq!(param.name, "tok");
        assert_eq!(param.value, "xyz");
        assert_eq!(param.expires, None); // -1 expires → None
    }
}
