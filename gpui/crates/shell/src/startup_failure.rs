//! Finder/Dock startup errors must be visible before the process exits.
use gpui::{
    App, AppContext as _, Context, IntoElement, Render, Window, WindowOptions, div, prelude::*,
};

fn detail(error: &str, root: &std::path::Path) -> String {
    format!("{error}\n\nPreview root: {}", root.display())
}

fn root_for_message() -> std::path::PathBuf {
    let root = std::env::var_os("GITTERM_GPUI_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".config/gitterm-gpui")))
        .unwrap_or_else(|| "<home directory unavailable>".into());
    if std::env::args_os().any(|arg| arg == "--sample") {
        root.join("sample")
    } else {
        root
    }
}

struct StartupFailure(String);
impl StartupFailure {
    fn prompt(&self, window: &mut Window, cx: &mut App) -> impl Future<Output = ()> + use<> {
        let answer = window.prompt(
            gpui::PromptLevel::Critical,
            "GitTerm GPUI could not start",
            Some(&self.0),
            &["Quit"],
            cx,
        );
        async move {
            let _ = answer.await;
        }
    }
}
impl Render for StartupFailure {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().p_4().child(self.0.clone())
    }
}

pub(crate) fn show(cx: &mut App, error: &str) {
    let message = detail(error, &root_for_message());
    let opened = cx.open_window(WindowOptions::default(), move |window, cx| {
        let failure = StartupFailure(message);
        let answer = failure.prompt(window, cx);
        cx.spawn(async move |_| {
            answer.await;
            std::process::exit(1);
        })
        .detach();
        cx.new(|_| failure)
    });
    if let Err(error) = opened {
        eprintln!("show GPUI startup failure window: {error:#}");
        std::process::exit(1);
    }
    cx.on_window_closed(|_, _| std::process::exit(1)).detach();
    cx.activate(true);
}

/// Used when startup failed before there was a GPUI event loop or state writer.
pub fn show_startup_failure(error: &str) -> ! {
    let error = error.to_owned();
    gpui_kit::application().run(move |cx| show(cx, &error));
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    #[test]
    fn startup_failure_presented_to_user_contains_error_and_preview_root() {
        let text = super::detail(
            "recorded launch PID 123 is still alive",
            std::path::Path::new("/tmp/preview"),
        );
        assert!(text.contains("PID 123 is still alive"));
        assert!(text.contains("Preview root: /tmp/preview"));
    }

    #[gpui::test]
    fn startup_failure_presents_a_native_prompt_headlessly(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        let view = cx.update(|cx| {
            cx.open_window(gpui::WindowOptions::default(), |window, cx| {
                let failure = super::StartupFailure(super::detail(
                    "startup refused",
                    std::path::Path::new("/tmp/preview"),
                ));
                // Exercise the production presentation path without its exit task.
                let _answer = failure.prompt(window, cx);
                cx.new(|_| failure)
            })
            .unwrap()
        });
        assert!(cx.has_pending_prompt());
        view.update(cx, |failure, _, _| {
            assert!(failure.0.contains("startup refused"));
            assert!(failure.0.contains("Preview root: /tmp/preview"));
        })
        .unwrap();
    }
}
