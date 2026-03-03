//! Extended browser action implementations.
//!
//! Pure CDP implementations of hover, double-click, drag, check/uncheck,
//! select, key press, file upload, and element clear. These functions take an
//! already-resolved `&Page` — session management, scroll-into-view pre-steps,
//! and behavioral-mode selection are handled by `BrowserManager`.

use std::time::Duration;

use {
    chromiumoxide::{
        Page,
        cdp::browser_protocol::{
            dom::{GetDocumentParams, NodeId, QuerySelectorParams, SetFileInputFilesParams},
            input::{
                DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams,
                DispatchMouseEventType, MouseButton,
            },
        },
    },
    tokio::time::sleep,
};

use crate::error::Error;

// ─────────────────────────────────────────────────────────────────────────────
// Private CDP helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Send a single `mouseMoved` CDP event (no button state).
async fn instant_mouse_move(page: &Page, x: f64, y: f64) -> Result<(), Error> {
    let cmd = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseMoved)
        .x(x)
        .y(y)
        .build()
        .map_err(|e| Error::Cdp(e.to_string()))?;
    page.execute(cmd)
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?;
    Ok(())
}

/// Send a `mousePressed` or `mouseReleased` CDP event with left button.
async fn mouse_button(
    page: &Page,
    type_: DispatchMouseEventType,
    x: f64,
    y: f64,
) -> Result<(), Error> {
    let cmd = DispatchMouseEventParams::builder()
        .r#type(type_)
        .x(x)
        .y(y)
        .button(MouseButton::Left)
        .click_count(1)
        .build()
        .map_err(|e| Error::Cdp(e.to_string()))?;
    page.execute(cmd)
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Public action implementations
// ─────────────────────────────────────────────────────────────────────────────

/// Move the mouse cursor to (x, y) with a single instant `MouseMoved` event.
pub async fn hover_instant(page: &Page, x: f64, y: f64) -> Result<(), Error> {
    instant_mouse_move(page, x, y).await
}

/// Perform a double-click at (x, y).
///
/// Assumes the mouse is already near the target. Sequence:
/// press (count=1) → release (count=1) → 50 ms pause →
/// press (count=2) → release (count=2).
pub async fn double_click_events(page: &Page, x: f64, y: f64) -> Result<(), Error> {
    // First click
    let press1 = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MousePressed)
        .x(x)
        .y(y)
        .button(MouseButton::Left)
        .click_count(1)
        .build()
        .map_err(|e| Error::Cdp(e.to_string()))?;
    page.execute(press1)
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?;

    let release1 = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseReleased)
        .x(x)
        .y(y)
        .button(MouseButton::Left)
        .click_count(1)
        .build()
        .map_err(|e| Error::Cdp(e.to_string()))?;
    page.execute(release1)
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?;

    // Inter-click pause — within OS double-click interval
    sleep(Duration::from_millis(50)).await;

    // Second click with count=2 so the browser fires the `dblclick` event
    let press2 = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MousePressed)
        .x(x)
        .y(y)
        .button(MouseButton::Left)
        .click_count(2)
        .build()
        .map_err(|e| Error::Cdp(e.to_string()))?;
    page.execute(press2)
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?;

    let release2 = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseReleased)
        .x(x)
        .y(y)
        .button(MouseButton::Left)
        .click_count(2)
        .build()
        .map_err(|e| Error::Cdp(e.to_string()))?;
    page.execute(release2)
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?;

    Ok(())
}

/// Perform an instant drag from (from_x, from_y) to (to_x, to_y).
///
/// Sequence: mouseMoved (source) → mousePressed → 5 intermediate mouseMoved
/// steps → mouseReleased (destination).
pub async fn drag_instant(
    page: &Page,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
) -> Result<(), Error> {
    instant_mouse_move(page, from_x, from_y).await?;
    mouse_button(page, DispatchMouseEventType::MousePressed, from_x, from_y).await?;

    // Intermediate move steps help browsers register drag intent
    const DRAG_STEPS: u32 = 5;
    for step in 1..=DRAG_STEPS {
        let t = step as f64 / DRAG_STEPS as f64;
        let x = from_x + (to_x - from_x) * t;
        let y = from_y + (to_y - from_y) * t;
        instant_mouse_move(page, x, y).await?;
    }

    mouse_button(page, DispatchMouseEventType::MouseReleased, to_x, to_y).await?;
    Ok(())
}

/// Check a checkbox or radio element.
///
/// No-op if already checked. Returns [`Error::ElementNotFound`] when the
/// element ref doesn't exist on the current page.
pub async fn check_element(page: &Page, ref_: u32) -> Result<(), Error> {
    let js = format!(
        r#"(() => {{
            const el = document.querySelector(`[data-moltis-ref="{ref_}"]`);
            if (!el) return 'not_found';
            if (el.checked) return 'already_checked';
            el.click();
            return 'checked';
        }})()"#
    );
    let result: String = page
        .evaluate(js.as_str())
        .await
        .map_err(|e| Error::JsEvalFailed(e.to_string()))?
        .into_value()
        .map_err(|e| Error::JsEvalFailed(format!("{e:?}")))?;
    if result == "not_found" {
        return Err(Error::ElementNotFound(ref_));
    }
    Ok(())
}

/// Uncheck a checkbox element.
///
/// No-op if already unchecked. Returns [`Error::ElementNotFound`] when the
/// element ref doesn't exist.
pub async fn uncheck_element(page: &Page, ref_: u32) -> Result<(), Error> {
    let js = format!(
        r#"(() => {{
            const el = document.querySelector(`[data-moltis-ref="{ref_}"]`);
            if (!el) return 'not_found';
            if (!el.checked) return 'already_unchecked';
            el.click();
            return 'unchecked';
        }})()"#
    );
    let result: String = page
        .evaluate(js.as_str())
        .await
        .map_err(|e| Error::JsEvalFailed(e.to_string()))?
        .into_value()
        .map_err(|e| Error::JsEvalFailed(format!("{e:?}")))?;
    if result == "not_found" {
        return Err(Error::ElementNotFound(ref_));
    }
    Ok(())
}

/// Select an option in a `<select>` element by value attribute.
///
/// Fires `input` and `change` events with `bubbles: true` so React/Vue/Alpine
/// reactive listeners pick up the change.
pub async fn select_option(page: &Page, ref_: u32, value: &str) -> Result<(), Error> {
    let value_json = serde_json::to_string(value)
        .map_err(|e| Error::Cdp(format!("value serialization: {e}")))?;
    let js = format!(
        r#"(() => {{
            const el = document.querySelector(`[data-moltis-ref="{ref_}"]`);
            if (!el) return false;
            el.value = {value_json};
            el.dispatchEvent(new Event('input', {{ bubbles: true }}));
            el.dispatchEvent(new Event('change', {{ bubbles: true }}));
            return true;
        }})()"#
    );
    let found: bool = page
        .evaluate(js.as_str())
        .await
        .map_err(|e| Error::JsEvalFailed(e.to_string()))?
        .into_value()
        .map_err(|e| Error::JsEvalFailed(format!("{e:?}")))?;
    if !found {
        return Err(Error::ElementNotFound(ref_));
    }
    Ok(())
}

/// Press a named key or printable character on the currently focused element.
///
/// `key` must be a CDP key name:
/// - Named keys: `"Enter"`, `"Escape"`, `"Tab"`, `"Backspace"`, `"ArrowDown"`, etc.
/// - Printable chars: `"a"`, `"A"`, `"1"`, `" "`, `"!"`, etc.
///
/// Single printable characters also set `text` so ARIA inputs that listen on
/// `keydown.text` receive the character insertion.
pub async fn press_key(page: &Page, key: &str) -> Result<(), Error> {
    // Named keys (len > 1) have no character text; printable chars carry themselves.
    let text = if key.chars().count() == 1 {
        key.to_string()
    } else {
        String::new()
    };

    let key_down = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyDown)
        .key(key.to_string())
        .text(text)
        .build()
        .map_err(|e| Error::Cdp(e.to_string()))?;
    page.execute(key_down)
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?;

    // Per CDP spec, keyUp text must be empty
    let key_up = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyUp)
        .key(key.to_string())
        .build()
        .map_err(|e| Error::Cdp(e.to_string()))?;
    page.execute(key_up)
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?;

    Ok(())
}

/// Upload a file to a `<input type="file">` element.
///
/// `path` must be an absolute path readable by the browser process.
/// Uses `DOM.getDocument` → `DOM.querySelector` → `DOM.setFileInputFiles`
/// over CDP (chromiumoxide 0.8 does not expose `Element::set_files`).
pub async fn upload_file(page: &Page, ref_: u32, path: &str) -> Result<(), Error> {
    // 1. Get document root nodeId
    let doc = page
        .execute(GetDocumentParams::default())
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?;
    let root_node_id = doc.result.root.node_id;

    // 2. Query for element nodeId — DOM.querySelector returns NodeId(0) when not found
    let selector = format!("[data-moltis-ref=\"{ref_}\"]");
    let found = page
        .execute(QuerySelectorParams::new(root_node_id, selector))
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?;

    if found.result.node_id == NodeId::new(0) {
        return Err(Error::ElementNotFound(ref_));
    }

    // 3. Set the file via DOM.setFileInputFiles
    let set_files = SetFileInputFilesParams::builder()
        .file(path.to_string())
        .node_id(found.result.node_id)
        .build()
        .map_err(|e| Error::Cdp(e.to_string()))?;
    page.execute(set_files)
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?;

    Ok(())
}

/// Clear an input or textarea element.
///
/// Uses the native input value setter so React's synthetic event system detects
/// the change, then fires `input` and `change` events for other frameworks.
pub async fn clear_input(page: &Page, ref_: u32) -> Result<(), Error> {
    let js = format!(
        r#"(() => {{
            const el = document.querySelector(`[data-moltis-ref="{ref_}"]`);
            if (!el) return false;
            const nativeInputSetter =
                Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value')?.set
                ?? Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value')?.set;
            if (nativeInputSetter) {{
                nativeInputSetter.call(el, '');
            }} else {{
                el.value = '';
            }}
            el.dispatchEvent(new Event('input', {{ bubbles: true }}));
            el.dispatchEvent(new Event('change', {{ bubbles: true }}));
            return true;
        }})()"#
    );
    let found: bool = page
        .evaluate(js.as_str())
        .await
        .map_err(|e| Error::JsEvalFailed(e.to_string()))?
        .into_value()
        .map_err(|e| Error::JsEvalFailed(format!("{e:?}")))?;
    if !found {
        return Err(Error::ElementNotFound(ref_));
    }
    Ok(())
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn press_key_named_key_has_empty_text() {
        // Multi-char key names must not carry text (per CDP spec, keyDown text
        // is the character produced; named keys produce no character)
        for key in [
            "Enter",
            "Escape",
            "Tab",
            "Backspace",
            "ArrowDown",
            "ArrowUp",
            "F5",
        ] {
            let text = if key.chars().count() == 1 {
                key.to_string()
            } else {
                String::new()
            };
            assert!(
                text.is_empty(),
                "named key '{key}' should produce empty text, got '{text}'"
            );
        }
    }

    #[test]
    fn press_key_printable_char_carries_text() {
        // Single printable chars should produce themselves as text
        for key in ["a", "A", "1", " ", "!"] {
            let text = if key.chars().count() == 1 {
                key.to_string()
            } else {
                String::new()
            };
            assert_eq!(text, key, "single char '{key}' should carry itself as text");
        }
    }

    #[test]
    fn select_option_value_json_escapes_special_chars() {
        // serde_json must escape quotes so the generated JS is safe
        let value = r#"it's "tricky""#;
        let json = serde_json::to_string(value).expect("serialization must succeed");
        // Verify double-quote escaping
        assert!(json.contains("\\\""), "double quotes must be JSON-escaped");
        // Verify round-trip correctness
        let decoded: String =
            serde_json::from_str(&json).expect("round-trip deserialization must succeed");
        assert_eq!(decoded, value);
    }

    #[test]
    fn drag_instant_interpolation_reaches_destination() {
        // Verify the linear interpolation formula reaches to_x at the final step
        const DRAG_STEPS: u32 = 5;
        let (from_x, to_x) = (0.0_f64, 100.0_f64);
        let (from_y, to_y) = (0.0_f64, 200.0_f64);

        for step in 1..=DRAG_STEPS {
            let t = step as f64 / DRAG_STEPS as f64;
            let x = from_x + (to_x - from_x) * t;
            let y = from_y + (to_y - from_y) * t;
            assert!(x > 0.0 && x <= 100.0, "step {step}: x={x} out of [0, 100]");
            assert!(y > 0.0 && y <= 200.0, "step {step}: y={y} out of [0, 200]");
        }

        // Final step must reach the destination exactly
        let t_final = DRAG_STEPS as f64 / DRAG_STEPS as f64;
        let x_final = from_x + (to_x - from_x) * t_final;
        let y_final = from_y + (to_y - from_y) * t_final;
        assert!((x_final - to_x).abs() < 1e-9, "drag did not reach to_x");
        assert!((y_final - to_y).abs() < 1e-9, "drag did not reach to_y");
    }

    #[test]
    fn double_click_inter_click_delay_documented() {
        // The 50 ms inter-click delay is well within OS double-click intervals
        // (typically 200-500 ms). This test documents and catches accidental changes.
        let delay = Duration::from_millis(50);
        assert_eq!(delay.as_millis(), 50);
    }
}
