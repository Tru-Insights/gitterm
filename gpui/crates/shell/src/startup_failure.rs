//! Finder/Dock startup errors must be visible before the process exits.
use gpui::{
    App, AppContext as _, Context, IntoElement, Render, Window, WindowOptions, div, prelude::*,
};

fn detail(error: &str, label: &str, root: &std::path::Path) -> String {
    format!("{error}\n\n{label}: {}", root.display())
}

fn installed_executable() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
        .and_then(|macos| {
            let contents = macos.parent()?;
            Some(
                macos.file_name().is_some_and(|name| name == "MacOS")
                    && contents.file_name().is_some_and(|name| name == "Contents")
                    && contents.parent().is_some_and(|bundle| {
                        bundle
                            .extension()
                            .is_some_and(|extension| extension == "app")
                    }),
            )
        })
        .unwrap_or(false)
}

fn root_for_message() -> (&'static str, std::path::PathBuf) {
    let args = std::env::args_os().collect::<Vec<_>>();
    let preview = args
        .iter()
        .any(|arg| arg == "--preview" || arg == "--sample");
    let live = !preview && (args.iter().any(|arg| arg == "--live") || installed_executable());
    if live {
        let root = gitterm_core::config_paths::desktop_config_dir()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|_| {
                std::env::var_os("GITTERM_V6_CONFIG_DIR")
                    .map(std::path::PathBuf::from)
                    .or_else(|| dirs::home_dir().map(|home| home.join(".config/gitterm-v6")))
                    .unwrap_or_else(|| "<home directory unavailable>".into())
            });
        return ("V6 state root", root);
    }
    let root = std::env::var_os("GITTERM_GPUI_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".config/gitterm-gpui")))
        .unwrap_or_else(|| "<home directory unavailable>".into());
    if std::env::args_os().any(|arg| arg == "--sample") {
        ("Preview root", root.join("sample"))
    } else {
        ("Preview root", root)
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
    let (label, root) = root_for_message();
    let message = detail(error, label, &root);
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
            "Preview root",
            std::path::Path::new("/tmp/preview"),
        );
        assert!(text.contains("PID 123 is still alive"));
        assert!(text.contains("Preview root: /tmp/preview"));
    }

    #[test]
    fn startup_failure_uses_the_v6_state_root_label_in_live_mode() {
        let text = super::detail(
            "could not open state",
            "V6 state root",
            std::path::Path::new("/tmp/v6"),
        );
        assert!(text.contains("V6 state root: /tmp/v6"));
        assert!(!text.contains("Preview root"));
    }

    #[gpui::test]
    fn startup_failure_presents_a_native_prompt_headlessly(cx: &mut gpui::TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        use gpui::AppContext as _;
        let view = cx.update(|cx| {
            cx.open_window(gpui::WindowOptions::default(), |window, cx| {
                let failure = super::StartupFailure(super::detail(
                    "startup refused",
                    "Preview root",
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
