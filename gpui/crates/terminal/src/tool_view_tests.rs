//! Callback dispatch is headless; all terminal reports come from real PTYs.
use super::{LayoutMetrics, TerminalConfig, TerminalView};
use crate::{
    SessionState,
    tool_tests::{poll, ready_shell, screen, send, text},
};
use alacritty_terminal::index::{Column, Line, Point};
use gpui::TestAppContext;
use gpui_kit::{AppContext, Modifiers, MouseDownEvent, point, px};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

#[gpui::test]
fn theme_change_recreates_presentation_and_recolors_scrollback_without_restarting_pty(
    cx: &mut TestAppContext,
) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    let dir = tempfile::tempdir().unwrap();
    let mut backend = ready_shell(dir.path());
    send(
        &backend,
        "printf '\\033[2J\\033[H\\033[31mSCROLLBACK_COLOR\\033[0m\\n'; for i in {1..30}; do printf 'line\\n'; done; printf 'SCROLL_READY\\n'\r",
    );
    text(&mut backend, "SCROLL_READY");
    backend.handle(crate::Command::Scroll(1000));
    poll(&mut backend, "scrollback visible", |b| {
        screen(b).contains("SCROLLBACK_COLOR")
    });
    let pid = backend.child_pid().expect("owned PTY");
    let view = cx.new(|cx| {
        TerminalView::new(
            backend,
            TerminalConfig {
                colors: crate::ColorPalette::for_theme(true),
                ..Default::default()
            },
            cx,
        )
    });
    view.update(cx, |view, cx| {
        let snapshot = view.backend.renderable_content().clone();
        assert!(snapshot.display_offset > 0);
        let cells: Vec<_> = snapshot
            .cells
            .iter()
            .filter(|c| c.cell.c == 'S')
            .map(|c| (c.point.column.0, c.cell.clone()))
            .collect();
        assert_eq!(cells.len(), 1);
        let (_, before) = view
            .renderer
            .layout_row(0, cells.clone().into_iter(), &snapshot.colors);
        view.renderer.cell_width = px(999.);
        let mut config = view.config.clone();
        config.colors = crate::ColorPalette::for_theme(false);
        view.update_config(config, cx);
        assert_ne!(
            view.renderer.cell_width,
            px(999.),
            "rebuild renderer metrics"
        );
        let (_, after) = view
            .renderer
            .layout_row(0, cells.into_iter(), &snapshot.colors);
        assert_ne!(before[0].fg_color, after[0].fg_color);
        let color = after[0].fg_color.to_rgb();
        assert_eq!(
            [color.r, color.g, color.b].map(|channel| (channel * 255.).round() as u8),
            [0xd2, 0x0f, 0x39]
        );
        assert_eq!(view.backend.child_pid(), Some(pid));
        assert_eq!(view.backend.state(), SessionState::Running);
        assert_eq!(
            view.backend.renderable_content().display_offset,
            snapshot.display_offset
        );
        assert_eq!(
            view.backend.renderable_content().cells.len(),
            snapshot.cells.len()
        );
    });
}

#[gpui::test]
fn real_tools_osc8_label_reaches_link_callback(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    let dir = tempfile::tempdir().unwrap();
    let mut backend = ready_shell(dir.path());
    send(
        &backend,
        "printf '\\033[2J\\033[H\\033]8;id=fixture;https://example.invalid/target\\033\\\\Click here\\033]8;;\\033\\\\ plain'\r",
    );
    text(&mut backend, "Click here");
    assert_eq!(
        backend.renderable_content().cells[0]
            .cell
            .hyperlink()
            .unwrap()
            .uri(),
        "https://example.invalid/target"
    );
    let opened = Rc::new(RefCell::new(Vec::new()));
    let reported = opened.clone();
    let entity = cx.new(|cx| {
        TerminalView::new(
            backend,
            TerminalConfig {
                cols: 100,
                rows: 24,
                ..Default::default()
            },
            cx,
        )
        .with_url_opener(move |url| reported.borrow_mut().push(url.to_string()))
    });
    entity.update(cx, |view, _| {
        *view.layout_metrics.lock() = LayoutMetrics {
            origin: point(px(0.), px(0.)),
            cell_width: px(10.),
            cell_height: px(20.),
            cols: 100,
            rows: 24,
        };
    });
    crate::test_support::wait(|| {
        entity.update(cx, |view, cx| {
            if view.backend.sync() && opened.borrow().is_empty() {
                view.mouse_down(
                    &MouseDownEvent {
                        position: point(px(35.), px(10.)),
                        modifiers: Modifiers {
                            platform: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    cx,
                );
            }
            opened.borrow().len() == 1
        })
    });
    entity.update(cx, |view, cx| {
        assert_eq!(&*opened.borrow(), &["https://example.invalid/target"]);
        assert_eq!(
            view.backend.renderable_content().hovered_hyperlink,
            Some(Point::new(Line(0), Column(0))..=Point::new(Line(0), Column(9)))
        );
        view.mouse_down(
            &MouseDownEvent {
                position: point(px(125.), px(10.)),
                modifiers: Modifiers {
                    platform: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            cx,
        );
        assert_eq!(
            opened.borrow().len(),
            1,
            "plain text must not reuse destination"
        );
    });
}

#[gpui::test]
fn real_tools_osc0_osc2_and_osc7_callbacks(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    let dir = tempfile::tempdir().unwrap();
    let backend = ready_shell(dir.path());
    let titles = Rc::new(RefCell::new(Vec::new()));
    let cwd = Rc::new(RefCell::new(None));
    let reported_titles = titles.clone();
    let reported_cwd = cwd.clone();
    let handle = cx.add_window(|_, cx| {
        TerminalView::new(backend, TerminalConfig::default(), cx)
            .with_title_callback(move |_, _, title| {
                reported_titles.borrow_mut().push(title.to_string())
            })
            .with_cwd_callback(move |_, _, path| {
                *reported_cwd.borrow_mut() = Some(path.to_path_buf())
            })
    });
    handle
        .update(cx, |view, window, cx| {
            poll(&mut view.backend, "initial view configuration", |_| true);
            view.process_events(window, cx);
        })
        .unwrap();
    titles.borrow_mut().clear(); // Initial configuration may report ResetTitle.
    for (osc, title) in [(0, "OSC_ZERO"), (2, "OSC_TWO")] {
        let command = format!(
            "printf '\\033]{osc};{title}\\007\\033]7;file://localhost/tmp/terminal%%20cwd\\007CALLBACK_%s\\n' READY\r"
        );
        handle
            .update(cx, |view, _, _| send(&view.backend, &command))
            .unwrap();
        crate::test_support::wait(|| {
            handle
                .update(cx, |view, window, cx| view.process_events(window, cx))
                .unwrap();
            titles.borrow().iter().any(|t| t == title)
        });
    }
    handle
        .update(cx, |view, _, _| {
            text(&mut view.backend, "CALLBACK_READY");
            assert!(screen(&view.backend).contains("TRU274_PROMPT>"));
        })
        .unwrap();
    assert_eq!(&*titles.borrow(), &["OSC_ZERO", "OSC_TWO"]);
    assert_eq!(
        cwd.borrow().as_deref(),
        Some(std::path::Path::new("/tmp/terminal cwd"))
    );
}

#[gpui::test]
fn real_tools_ctrl_d_and_exit_callback_reap_session(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    for input in [b"\x04".as_slice(), b"exit\r".as_slice()] {
        let dir = tempfile::tempdir().unwrap();
        let mut backend = ready_shell(dir.path());
        send(
            &backend,
            "printf '%s' $$ > owned-pid; printf 'PID_%s\\n' READY\r",
        );
        text(&mut backend, "PID_READY");
        let pid: i32 = std::fs::read_to_string(dir.path().join("owned-pid"))
            .unwrap()
            .parse()
            .unwrap();
        let closed = Rc::new(Cell::new(0));
        let reported = closed.clone();
        let handle = cx.add_window(|_, cx| {
            TerminalView::new(backend, TerminalConfig::default(), cx)
                .with_exit_callback(move |_, _| reported.set(reported.get() + 1))
        });
        handle
            .update(cx, |view, _, _| send(&view.backend, input))
            .unwrap();
        crate::test_support::wait(|| {
            handle
                .update(cx, |view, window, cx| {
                    view.process_events(window, cx);
                    matches!(view.backend.state(), SessionState::Exited(Some(0)))
                        && closed.get() == 1
                })
                .unwrap()
        });
        handle
            .update(cx, |view, window, cx| {
                poll(&mut view.backend, "exit tail parsed", |b| {
                    screen(b).contains("exit")
                });
                view.backend.cancel(); // Join completed reader and release its PTY.
                assert_eq!(
                    Arc::strong_count(&view.backend.term),
                    1,
                    "worker retained grid"
                );
                assert!(!view.backend.input_bytes(b"ignored"));
                view.process_events(window, cx);
                assert_eq!(closed.get(), 1);
            })
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}
