/// Where a page takes the bar-hint script (`bar_hint_script`).
pub const BAR_HINT_SLOT: &str = "/*__BAR_HINT__*/";
const BAR_HINT_JS: &str = include_str!("../../assets/bar_hint.js");

/// The script that defines `window.__gittermHint(text, x)` on a page
/// (TRU-192): a bubble at the page's bottom edge, drawn in `background`
/// with `color` text. Both are CSS colour values.
pub fn bar_hint_script(background: &str, color: &str) -> String {
    BAR_HINT_JS
        .replace("__BG__", background)
        .replace("__FG__", color)
}
