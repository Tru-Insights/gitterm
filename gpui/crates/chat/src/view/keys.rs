//! Keyboard scrolling of the transcript, as a browser scrolls Iced's `#messages`.
use super::*;
use scroll::ScrollKey;

impl ChatView {
    /// An input keeps its own cursor keys; otherwise the key scrolls.
    pub(super) fn scroll_key(
        &mut self,
        key: ScrollKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_focused_input(cx) {
            cx.propagate();
            return;
        }
        self.scroller
            .update(cx, |scroller, cx| scroller.scroll_key(key, cx));
    }
}
