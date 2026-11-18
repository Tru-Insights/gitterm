use gpui::*;
use gpui_component::Root;

pub fn run() {
    if !std::env::args().any(|arg| arg == "--terminal") {
        gitterm_gpui_shell::run();
        return;
    }
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1440.), px(900.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("GitTerm · GPUI terminal".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                let view = cx.new(|cx| {
                    gitterm_terminal::TerminalView::new_deferred(
                        Default::default(),
                        Default::default(),
                        cx,
                    )
                    .with_url_opener(|url| {
                        let url = url.to_owned();
                        std::thread::spawn(move || {
                            match std::process::Command::new("/usr/bin/open")
                                .arg("--")
                                .arg(&url)
                                .status()
                            {
                                Ok(status) if status.success() => {}
                                result => {
                                    eprintln!("[gitterm-terminal] URL opener failed: {result:?}")
                                }
                            }
                        });
                    })
                });
                let focus = view.read(cx).focus_handle().clone();
                window.focus(&focus, cx);
                cx.new(|cx| Root::new(view, window, cx))
            },
        )
        .expect("open GitTerm GPUI terminal window");
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });
}
