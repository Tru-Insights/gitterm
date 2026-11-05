//! Visual-reply contracts shared by the MCP tools and chat page.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_HTML_BYTES: usize = 512 * 1024;
pub const MIN_HEIGHT: u32 = 100;
pub const MAX_HEIGHT: u32 = 4000;
pub const DEFAULT_WIDTH: u32 = 760;
// Wry 0.48's Windows navigation callback omits subframe navigation. Keep
// rendering inert until that platform has an equivalent native boundary.
pub const NATIVE_HTML_SUPPORTED: bool = cfg!(target_os = "macos");
pub const RENDER_JS: &str = include_str!("../assets/html_render.js");

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HtmlRenderRequest {
    /// Complete self-contained HTML document, at most 512 KiB in UTF-8.
    #[schemars(length(min = 1, max = 524288))]
    pub html: String,
    /// Fitted frame height in CSS pixels, 100–4000. Use html_preview's contentHeight.
    #[schemars(range(min = 100, max = 4000))]
    pub height: u32,
}

/// Additive tool-result payload: persisted by each harness with its conversation.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HtmlRenderResult {
    pub html_render: HtmlRenderRequest,
}

/// Ordinary tool output stays capped. A validated visual reply must retain
/// its complete document for replay, including adapters without raw tool input.
pub fn is_render_output(output: &str) -> bool {
    output.len() <= MAX_HTML_BYTES * 6 + 128
        && serde_json::from_str::<HtmlRenderResult>(output)
            .is_ok_and(|result| result.html_render.validate().is_ok())
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum HtmlTheme {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HtmlPreviewRequest {
    /// Complete self-contained HTML document, at most 512 KiB in UTF-8.
    #[schemars(length(min = 1, max = 524288))]
    pub html: String,
    /// Viewport width in CSS pixels, 240–1600; default 760. Try 390 for phones.
    #[schemars(range(min = 240, max = 1600))]
    pub width: Option<u32>,
    /// Theme to check: dark (default) or light.
    pub theme: Option<HtmlTheme>,
}

pub fn validate_html(html: &str) -> Result<(), String> {
    if html.trim().is_empty() || html.len() > MAX_HTML_BYTES {
        return Err("HTML must be nonempty and at most 512 KiB in UTF-8".into());
    }
    Ok(())
}

impl HtmlRenderRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_html(&self.html)?;
        if !(MIN_HEIGHT..=MAX_HEIGHT).contains(&self.height) {
            return Err("HTML height must be 100–4000 CSS pixels".into());
        }
        Ok(())
    }
}

impl HtmlPreviewRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_html(&self.html)?;
        if !(240..=1600).contains(&self.width.unwrap_or(DEFAULT_WIDTH)) {
            return Err("HTML preview width must be 240–1600 CSS pixels".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_byte_size_height_and_width_at_boundaries() {
        assert!(validate_html("").is_err());
        assert!(validate_html(" \n ").is_err());
        assert!(validate_html(&"x".repeat(MAX_HTML_BYTES)).is_ok());
        assert!(validate_html(&"x".repeat(MAX_HTML_BYTES + 1)).is_err());
        assert!(validate_html(&"é".repeat(MAX_HTML_BYTES / 2 + 1)).is_err());
        for height in [0, 99, 4001, u32::MAX] {
            assert!(HtmlRenderRequest {
                html: "ok".into(),
                height
            }
            .validate()
            .is_err());
        }
        for height in [100, 4000] {
            assert!(HtmlRenderRequest {
                html: "ok".into(),
                height
            }
            .validate()
            .is_ok());
        }
        for width in [0, 239, 1601, u32::MAX] {
            assert!(HtmlPreviewRequest {
                html: "ok".into(),
                width: Some(width),
                theme: None
            }
            .validate()
            .is_err());
        }
        for width in [None, Some(240), Some(1600)] {
            assert!(HtmlPreviewRequest {
                html: "ok".into(),
                width,
                theme: None
            }
            .validate()
            .is_ok());
        }
    }

    #[test]
    fn native_ipc_requires_the_owning_pages_capability() {
        let message = r#"{"type":"submit","tabId":1}"#;
        assert!(ipc_message(message, "owner").is_none());
        assert!(ipc_message(
            &serde_json::json!({"token":"other page", "message":message}).to_string(),
            "owner"
        )
        .is_none());
        assert!(ipc_message(
            &serde_json::json!({"token":"owner", "message":{}}).to_string(),
            "owner"
        )
        .is_none());
        assert_eq!(
            ipc_message(
                &serde_json::json!({"token":"owner", "message":message}).to_string(),
                "owner"
            )
            .as_deref(),
            Some(message)
        );
        for url in [
            "https://example.com",
            "http://127.0.0.1",
            "file:///tmp/test",
            "data:text/html,test",
            "javascript:alert(1)",
        ] {
            assert!(!agent_navigation_allowed(url));
        }
        assert!(agent_navigation_allowed("about:srcdoc"));
        assert!(agent_navigation_allowed("about:blank"));
    }

    #[test]
    fn only_bounded_valid_render_results_bypass_output_truncation() {
        assert!(!is_render_output(r#"{"html_render":true}"#));
        assert!(!is_render_output(
            r#"{"html_render":{"html":"ok","height":99999}}"#
        ));
        assert!(!is_render_output(
            &serde_json::json!({"html_render":{"html":"x".repeat(MAX_HTML_BYTES+1),"height":100}})
                .to_string()
        ));
        assert!(is_render_output(
            r#"{"html_render":{"html":"ok","height":100}}"#
        ));
        let legacy = serde_json::json!({"type":"text_delta","data":"old conversation"});
        assert!(serde_json::from_value::<crate::harness::HarnessEvent>(legacy).is_ok());
    }
}

/// Runs at document start in *every* frame. CSP does not govern WebRTC,
/// and nested srcdoc documents can execute despite frame-src 'none'.
pub const FRAME_GUARD_JS: &str = include_str!("../assets/html_frame_guard.js");

/// Only the native main frame gets this capability. WebKit's raw message
/// handler is reachable from subframes even though window.ipc is main-only.
pub fn ipc_bootstrap(token: &str) -> String {
    let token = serde_json::to_string(token).expect("string serializes");
    let supported = NATIVE_HTML_SUPPORTED;
    format!(
        r#"(() => {{
      const send = window.ipc.postMessage.bind(window.ipc);
      Object.defineProperty(window, '__gittermNativePost', {{
        value: (message) => send(JSON.stringify({{ token: {token}, message }})),
        writable: false, configurable: false
      }});
      Object.defineProperty(window, '__gittermCanRenderHtml', {{ value: {supported}, writable: false, configurable: false }});
    }})();"#
    )
}

pub fn ipc_message(raw: &str, token: &str) -> Option<String> {
    let envelope: serde_json::Value = serde_json::from_str(raw).ok()?;
    if envelope.get("token")?.as_str()? != token {
        return None;
    }
    envelope.get("message")?.as_str().map(str::to_owned)
}

/// Agent pages never navigate to external content. On macOS Wry routes
/// subframe navigations through its new-window policy as well.
pub fn agent_navigation_allowed(url: &str) -> bool {
    matches!(url, "about:blank" | "about:srcdoc")
}
