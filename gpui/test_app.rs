//! Scoped teardown for GPUI's test platform (no native app or windows).
use gpui::TestAppContext;

pub(crate) struct Cleanup(TestAppContext);

impl Cleanup {
    pub(crate) fn new(cx: &TestAppContext) -> Self {
        Self(cx.clone())
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        // gpui-pre 0.3.14's test macro calls App::quit, a no-op on TestPlatform.
        // TestAppContext::quit also frees the contexts retained by
        // add_window_view, then shuts down windows and releases their entities.
        // Keep this guard outside fixture helpers so borrowed VisualTestContexts
        // remain valid until the test body (including unwinding) has finished.
        self.0.quit();
    }
}
