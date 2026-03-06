//! DOM snapshot extraction with element references.
//!
//! This module extracts interactive elements from a page and assigns them
//! numeric reference IDs. This approach (inspired by openclaw) provides:
//! - Stable references that don't break with page updates
//! - Security: no CSS selectors exposed to the model
//! - Reliability: elements identified by role/content, not fragile paths

use std::borrow::Cow;

use {chromiumoxide::Page, serde_json::Value, tracing::debug};

use crate::{
    error::Error,
    types::{DomSnapshot, ElementBounds, ElementRef, ScrollDimensions, ViewportSize},
};

/// JavaScript to extract interactive elements from the DOM.
pub(crate) const EXTRACT_ELEMENTS_JS: &str = r#"
(() => {
    const interactive = [
        'a', 'button', 'input', 'select', 'textarea',
        '[role="button"]', '[role="link"]', '[role="checkbox"]',
        '[role="radio"]', '[role="textbox"]', '[role="combobox"]',
        '[role="listbox"]', '[role="menu"]', '[role="menuitem"]',
        '[role="tab"]', '[role="switch"]', '[onclick]', '[tabindex]'
    ];

    const selector = interactive.join(', ');
    const elements = document.querySelectorAll(selector);
    const results = [];

    function isVisible(el) {
        const rect = el.getBoundingClientRect();
        const style = getComputedStyle(el);
        return (
            rect.width > 0 &&
            rect.height > 0 &&
            style.visibility !== 'hidden' &&
            style.display !== 'none' &&
            parseFloat(style.opacity) > 0
        );
    }

    function isInViewport(rect) {
        return (
            rect.bottom >= 0 &&
            rect.right >= 0 &&
            rect.top <= window.innerHeight &&
            rect.left <= window.innerWidth
        );
    }

    function getTextContent(el, maxLen = 100) {
        let text = el.innerText || el.textContent || '';
        text = text.trim().replace(/\s+/g, ' ');
        if (text.length > maxLen) {
            text = text.substring(0, maxLen) + '...';
        }
        return text || null;
    }

    function getRole(el) {
        if (el.getAttribute('role')) return el.getAttribute('role');
        const tag = el.tagName.toLowerCase();
        const roleMap = {
            'a': 'link',
            'button': 'button',
            'input': el.type === 'checkbox' ? 'checkbox'
                   : el.type === 'radio' ? 'radio'
                   : el.type === 'submit' || el.type === 'button' ? 'button'
                   : 'textbox',
            'select': 'combobox',
            'textarea': 'textbox',
            'h1': 'heading',
            'h2': 'heading',
            'h3': 'heading',
            'h4': 'heading',
            'h5': 'heading',
            'h6': 'heading',
            'nav': 'navigation',
            'main': 'main',
            'img': 'img',
            'table': 'table',
            'tr': 'row',
            'td': 'cell',
            'th': 'columnheader'
        };
        return roleMap[tag] || null;
    }

    function isCursorInteractive(el) {
        if (el.closest('a, button, input, select, textarea')) return false;
        const style = window.getComputedStyle(el);
        return style.cursor === 'pointer';
    }

    function isInteractive(el) {
        const tag = el.tagName.toLowerCase();
        if (['a', 'button', 'select'].includes(tag)) return true;
        if (tag === 'input' && el.type !== 'hidden') return true;
        if (tag === 'textarea') return true;
        if (el.getAttribute('onclick')) return true;
        if (el.getAttribute('role')) return true;
        const tabindex = el.getAttribute('tabindex');
        if (tabindex && parseInt(tabindex, 10) >= 0) return true;
        if (isCursorInteractive(el)) return true;
        return false;
    }

    let refNum = 1;

    for (const el of elements) {
        if (!isVisible(el)) continue;

        const rect = el.getBoundingClientRect();
        const visible = isInViewport(rect);
        const tag = el.tagName.toLowerCase();

        const isInput = tag === 'input';
        results.push({
            ref_: refNum++,
            tag: tag,
            role: getRole(el),
            text: getTextContent(el),
            href: el.href || null,
            placeholder: el.placeholder || null,
            value: el.value || null,
            aria_label: el.getAttribute('aria-label'),
            visible: visible,
            interactive: isInteractive(el),
            checked: (isInput && (el.type === 'checkbox' || el.type === 'radio'))
                ? el.checked : null,
            disabled: el.disabled || el.getAttribute('aria-disabled') === 'true' || false,
            input_type: isInput ? (el.type || 'text') : null,
            bounds: {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height
            }
        });

        // Store ref on element for later retrieval
        el.dataset.moltisRef = (refNum - 1).toString();
    }

    // Extract page content (truncated to avoid huge responses)
    let content = document.body?.innerText || '';
    content = content.replace(/\s+/g, ' ').trim();
    if (content.length > 8000) {
        content = content.substring(0, 8000) + '... [truncated]';
    }

    return {
        elements: results,
        content: content || null,
        viewport: {
            width: window.innerWidth,
            height: window.innerHeight
        },
        scroll: {
            x: window.scrollX,
            y: window.scrollY,
            width: document.documentElement.scrollWidth,
            height: document.documentElement.scrollHeight
        }
    };
})()
"#;

/// JavaScript to find an element by its ref number.
pub(crate) const FIND_BY_REF_JS: &str = r#"
((ref) => {
    const el = document.querySelector(`[data-moltis-ref="${ref}"]`);
    if (!el) return null;
    const rect = el.getBoundingClientRect();
    return {
        found: true,
        tag: el.tagName.toLowerCase(),
        centerX: rect.x + rect.width / 2,
        centerY: rect.y + rect.height / 2
    };
})
"#;

fn is_filtered_dom_char(ch: char) -> bool {
    let code = ch as u32;
    matches!(code, 0x200B..=0x200D | 0x2060 | 0x00AD | 0xFEFF | 0x7F)
        || (0xE0000..=0xE007F).contains(&code)
        || (code < 0x20 && !matches!(ch, '\n' | '\r' | '\t'))
}

pub(crate) fn sanitize_dom_text(input: &str) -> Cow<'_, str> {
    if !input.chars().any(is_filtered_dom_char) {
        return Cow::Borrowed(input);
    }

    let sanitized = input.chars().filter(|ch| !is_filtered_dom_char(*ch)).collect();
    Cow::Owned(sanitized)
}

fn sanitize_optional_dom_text(value: Option<&str>) -> Option<String> {
    value
        .map(sanitize_dom_text)
        .map(Cow::into_owned)
        .filter(|value| !value.is_empty())
}

/// Extract a DOM snapshot from the page.
pub async fn extract_snapshot(page: &Page) -> Result<DomSnapshot, Error> {
    let url = page
        .url()
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?
        .unwrap_or_default();

    let title = page
        .get_title()
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?
        .unwrap_or_default();

    let result: Value = page
        .evaluate(EXTRACT_ELEMENTS_JS)
        .await
        .map_err(|e| Error::JsEvalFailed(e.to_string()))?
        .into_value()
        .map_err(|e| Error::JsEvalFailed(format!("failed to get result: {e:?}")))?;

    parse_snapshot_payload(url, title, &result)
}

/// Find an element's center coordinates by its ref number.
pub async fn find_element_by_ref(page: &Page, ref_: u32) -> Result<(f64, f64), Error> {
    let js = format!("({FIND_BY_REF_JS})({ref_})");

    let result: Value = page
        .evaluate(js.as_str())
        .await
        .map_err(|e| Error::JsEvalFailed(e.to_string()))?
        .into_value()
        .map_err(|e| Error::JsEvalFailed(format!("failed to get result: {e:?}")))?;

    parse_find_element_result(&result, ref_)
}

/// Focus an input element by its ref number.
pub async fn focus_element_by_ref(page: &Page, ref_: u32) -> Result<(), Error> {
    let js = build_focus_element_js(ref_);

    let result: Value = page
        .evaluate(js.as_str())
        .await
        .map_err(|e| Error::JsEvalFailed(e.to_string()))?
        .into_value()
        .map_err(|e| Error::JsEvalFailed(format!("failed to get result: {e:?}")))?;

    if result.as_bool() != Some(true) {
        return Err(Error::ElementNotFound(ref_));
    }

    Ok(())
}

/// Scroll an element into view by its ref number.
pub async fn scroll_element_into_view(page: &Page, ref_: u32) -> Result<(), Error> {
    let js = build_scroll_into_view_js(ref_);

    let result: Value = page
        .evaluate(js.as_str())
        .await
        .map_err(|e| Error::JsEvalFailed(e.to_string()))?
        .into_value()
        .map_err(|e| Error::JsEvalFailed(format!("failed to get result: {e:?}")))?;

    if result.as_bool() != Some(true) {
        return Err(Error::ElementNotFound(ref_));
    }

    Ok(())
}

fn parse_elements(result: &Value) -> Result<Vec<ElementRef>, Error> {
    let elements = result["elements"]
        .as_array()
        .ok_or_else(|| Error::JsEvalFailed("elements not an array".into()))?;

    Ok(elements
        .iter()
        .filter_map(|e| {
            Some(ElementRef {
                ref_: e["ref_"].as_u64()? as u32,
                tag: e["tag"].as_str()?.to_string(),
                role: sanitize_optional_dom_text(e["role"].as_str()),
                text: sanitize_optional_dom_text(e["text"].as_str()),
                href: e["href"].as_str().map(String::from),
                placeholder: sanitize_optional_dom_text(e["placeholder"].as_str()),
                value: sanitize_optional_dom_text(e["value"].as_str()),
                aria_label: sanitize_optional_dom_text(e["aria_label"].as_str()),
                visible: e["visible"].as_bool().unwrap_or(false),
                interactive: e["interactive"].as_bool().unwrap_or(false),
                checked: e["checked"].as_bool(),
                disabled: e["disabled"].as_bool().unwrap_or(false),
                input_type: e["input_type"].as_str().map(String::from),
                bounds: parse_bounds(&e["bounds"]),
            })
        })
        .collect())
}

pub(crate) fn parse_snapshot_payload(
    url: String,
    title: String,
    result: &Value,
) -> Result<DomSnapshot, Error> {
    let elements = parse_elements(result)?;
    let content = sanitize_optional_dom_text(result.get("content").and_then(|v| v.as_str()));
    let viewport = parse_viewport(result)?;
    let scroll = parse_scroll(result)?;
    let title = sanitize_dom_text(&title).into_owned();

    debug!(
        url = url,
        elements = elements.len(),
        content_len = content.as_ref().map(|c| c.len()).unwrap_or(0),
        "extracted DOM snapshot"
    );

    Ok(DomSnapshot {
        url,
        title,
        content,
        elements,
        viewport,
        scroll,
    })
}

pub(crate) fn parse_find_element_result(result: &Value, ref_: u32) -> Result<(f64, f64), Error> {
    if result.is_null() {
        return Err(Error::ElementNotFound(ref_));
    }

    let center_x = result["centerX"]
        .as_f64()
        .ok_or(Error::ElementNotFound(ref_))?;
    let center_y = result["centerY"]
        .as_f64()
        .ok_or(Error::ElementNotFound(ref_))?;

    Ok((center_x, center_y))
}

pub(crate) fn build_find_element_js(ref_: u32) -> String {
    format!("({FIND_BY_REF_JS})({ref_})")
}

pub(crate) fn build_focus_element_js(ref_: u32) -> String {
    format!(
        r#"(() => {{
            const el = document.querySelector(`[data-moltis-ref="{ref_}"]`);
            if (!el) return false;
            el.focus();
            return true;
        }})()"#
    )
}

pub(crate) fn build_scroll_into_view_js(ref_: u32) -> String {
    format!(
        r#"(() => {{
            const el = document.querySelector(`[data-moltis-ref="{ref_}"]`);
            if (!el) return false;
            el.scrollIntoView({{ behavior: 'instant', block: 'center' }});
            return true;
        }})()"#
    )
}

fn parse_bounds(v: &Value) -> Option<ElementBounds> {
    Some(ElementBounds {
        x: v["x"].as_f64()?,
        y: v["y"].as_f64()?,
        width: v["width"].as_f64()?,
        height: v["height"].as_f64()?,
    })
}

fn parse_viewport(result: &Value) -> Result<ViewportSize, Error> {
    let v = &result["viewport"];
    Ok(ViewportSize {
        width: v["width"].as_u64().unwrap_or(1280) as u32,
        height: v["height"].as_u64().unwrap_or(720) as u32,
    })
}

fn parse_scroll(result: &Value) -> Result<ScrollDimensions, Error> {
    let s = &result["scroll"];
    Ok(ScrollDimensions {
        x: s["x"].as_i64().unwrap_or(0) as i32,
        y: s["y"].as_i64().unwrap_or(0) as i32,
        width: s["width"].as_u64().unwrap_or(0) as u32,
        height: s["height"].as_u64().unwrap_or(0) as u32,
    })
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_elements_empty() {
        let result = serde_json::json!({
            "elements": [],
            "viewport": { "width": 1280, "height": 720 },
            "scroll": { "x": 0, "y": 0, "width": 1280, "height": 720 }
        });
        let elements = parse_elements(&result).unwrap();
        assert!(elements.is_empty());
    }

    #[test]
    fn test_parse_elements_with_data() {
        let result = serde_json::json!({
            "elements": [{
                "ref_": 1,
                "tag": "button",
                "role": "button",
                "text": "Click me",
                "href": null,
                "placeholder": null,
                "value": null,
                "aria_label": null,
                "visible": true,
                "interactive": true,
                "checked": null,
                "disabled": false,
                "input_type": null,
                "bounds": { "x": 10, "y": 20, "width": 100, "height": 40 }
            }],
            "viewport": { "width": 1280, "height": 720 },
            "scroll": { "x": 0, "y": 0, "width": 1280, "height": 720 }
        });

        let elements = parse_elements(&result).unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].ref_, 1);
        assert_eq!(elements[0].tag, "button");
        assert_eq!(elements[0].text.as_deref(), Some("Click me"));
        assert!(elements[0].visible);
    }

    fn element_json(tag: &str, extras: Value) -> Value {
        let mut base = serde_json::json!({
            "ref_": 1,
            "tag": tag,
            "role": null,
            "text": null,
            "href": null,
            "placeholder": null,
            "value": null,
            "aria_label": null,
            "visible": true,
            "interactive": true,
            "checked": null,
            "disabled": false,
            "input_type": null,
            "bounds": { "x": 0, "y": 0, "width": 50, "height": 20 }
        });
        if let (Some(obj), Some(ext)) = (base.as_object_mut(), extras.as_object()) {
            for (k, v) in ext {
                obj.insert(k.clone(), v.clone());
            }
        }
        serde_json::json!({
            "elements": [base],
            "viewport": { "width": 800, "height": 600 },
            "scroll": { "x": 0, "y": 0, "width": 800, "height": 600 }
        })
    }

    #[test]
    fn test_parse_elements_checked_field() {
        let result = element_json(
            "input",
            serde_json::json!({"checked": true, "input_type": "checkbox"}),
        );
        let elements = parse_elements(&result).unwrap();
        assert_eq!(elements[0].checked, Some(true));
        assert_eq!(elements[0].input_type.as_deref(), Some("checkbox"));
    }

    #[test]
    fn test_parse_elements_disabled_field() {
        let result = element_json(
            "input",
            serde_json::json!({"disabled": true, "input_type": "text"}),
        );
        let elements = parse_elements(&result).unwrap();
        assert!(elements[0].disabled);
    }

    #[test]
    fn test_parse_elements_input_type_field() {
        let result = element_json("input", serde_json::json!({"input_type": "email"}));
        let elements = parse_elements(&result).unwrap();
        assert_eq!(elements[0].input_type.as_deref(), Some("email"));
    }

    #[test]
    fn test_parse_elements_no_checked_for_non_checkbox() {
        // Non-checkbox inputs should have checked = null (None)
        let result = element_json(
            "input",
            serde_json::json!({"input_type": "text", "checked": null}),
        );
        let elements = parse_elements(&result).unwrap();
        assert_eq!(elements[0].checked, None);
    }

    #[test]
    fn sanitize_dom_text_strips_invisible_unicode() {
        let dirty = "he\u{200b}ll\u{2060}o\u{00ad}\u{E0001}\u{0007}";
        assert_eq!(sanitize_dom_text(dirty), "hello");
    }

    #[test]
    fn parse_snapshot_payload_sanitizes_content_and_title() {
        let result = serde_json::json!({
            "elements": [{
                "ref_": 1,
                "tag": "button",
                "role": "bu\u{200b}tton",
                "text": "Cl\u{2060}ick",
                "href": null,
                "placeholder": "pla\u{00ad}ceholder",
                "value": null,
                "aria_label": "la\u{E0002}bel",
                "visible": true,
                "interactive": true,
                "checked": null,
                "disabled": false,
                "input_type": null,
                "bounds": { "x": 1, "y": 2, "width": 3, "height": 4 }
            }],
            "content": "vi\u{200b}sible\u{2060} text",
            "viewport": { "width": 1280, "height": 720 },
            "scroll": { "x": 0, "y": 0, "width": 1280, "height": 720 }
        });

        let snapshot = parse_snapshot_payload(
            "https://example.com".to_string(),
            "Ti\u{200b}tle".to_string(),
            &result,
        )
        .unwrap();

        assert_eq!(snapshot.title, "Title");
        assert_eq!(snapshot.content.as_deref(), Some("visible text"));
        assert_eq!(snapshot.elements[0].role.as_deref(), Some("button"));
        assert_eq!(snapshot.elements[0].text.as_deref(), Some("Click"));
        assert_eq!(snapshot.elements[0].placeholder.as_deref(), Some("placeholder"));
        assert_eq!(snapshot.elements[0].aria_label.as_deref(), Some("label"));
    }
}
