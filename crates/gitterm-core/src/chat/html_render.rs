//! Visual-reply result validation used by transcript replay.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_HTML_BYTES: usize = 512 * 1024;
pub const MIN_HEIGHT: u32 = 100;
pub const MAX_HEIGHT: u32 = 4000;

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
