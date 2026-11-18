use gpui::*;
use gpui_component::{Root, dock::DockSkin};

pub fn run() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1440.), px(900.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("GitTerm · GPUI stage 0".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let (dock, _) = DockSkin::dock_area("gitterm-gpui", None, window, cx);
                cx.new(|cx| Root::new(dock, window, cx))
            },
        )
        .expect("open GitTerm GPUI stage 0 window");
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });
}
