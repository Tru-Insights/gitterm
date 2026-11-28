use super::{LayoutMetrics, TerminalConfig, TerminalView, should_defer_text_input_to_ime};
use crate::Backend;
use crate::{
    BackendSettings,
    test_support::{self, python, query_script, wait},
};
use ::gpui::TestAppContext;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line, Point as Cell},
    tty,
};
use gpui_kit::{
    AppContext, EntityInputHandler, KeyDownEvent, Keystroke, MouseDownEvent, MouseUpEvent,
    ScrollDelta, ScrollEvent, point, px,
};
use std::time::Instant;
use std::{sync::Arc, time::Duration};

#[::gpui::test]
fn multiline_clipboard_paste_waits_for_enter_in_real_zsh(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    let mut settings = BackendSettings {
        program: "/bin/zsh".into(),
        args: vec!["-d".into(), "-f".into(), "-i".into()],
        ..Default::default()
    };
    settings.env.insert("PROMPT".into(), "GATE> ".into());
    let backend = Backend::new(settings).unwrap();
    let entity = cx.new(|cx| TerminalView::new(backend, TerminalConfig::default(), cx));
    wait(|| {
        entity.update(cx, |view, _| {
            view.backend.sync() && view.backend.mode().contains(TermMode::BRACKETED_PASTE)
        })
    });
    entity.update(cx, |view, _| {
        crate::clipboard::Clipboard::new()
            .unwrap()
            .set_text("printf 'EXEC_%s\\n' ONE\nprintf 'EXEC_%s\\n' TWO\n".into())
            .unwrap();
        assert!(view.paste_from_clipboard());
    });
    std::thread::sleep(Duration::from_millis(700));
    entity.update(cx, |view, _| {
        let text = view.backend.get_all_text().expect("zsh idle lock");
        assert!(
            !text.contains("EXEC_ONE") && !text.contains("EXEC_TWO"),
            "paste executed before Enter: {text:?}"
        );
        assert!(view.write_input_bytes(b"\r"));
    });
    wait(|| {
        entity.read_with(cx, |view, _| {
            view.backend
                .get_all_text()
                .is_some_and(|text| text.contains("EXEC_ONE") && text.contains("EXEC_TWO"))
        })
    });
    println!("PASS real zsh: multiline clipboard paste executes only after Enter");
}

#[::gpui::test]
fn actual_view_sgr_press_release_wheel_reach_child(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    let backend = python(query_script(
        b"\x1b[?1049h\x1b[?1000h\x1b[?1006h",
        b"\x1b[<0;2;2M\x1b[<0;2;2m\x1b[<64;2;2M",
    ));
    let entity = cx.new(|cx| TerminalView::new(backend, TerminalConfig::default(), cx));
    wait(|| {
        entity.update(cx, |view, _| {
            view.backend.sync()
                && view.backend.mode().contains(
                    TermMode::ALT_SCREEN | TermMode::SGR_MOUSE | TermMode::MOUSE_REPORT_CLICK,
                )
        })
    });
    entity.update(cx, |view, cx| {
        *view.layout_metrics.lock() = LayoutMetrics {
            origin: point(px(0.), px(0.)),
            cell_width: px(10.),
            cell_height: px(20.),
            cols: 80,
            rows: 24,
        };
        let position = point(px(15.), px(25.));
        view.mouse_down(
            &MouseDownEvent {
                position,
                click_count: 1,
                ..Default::default()
            },
            cx,
        );
        view.mouse_up(&MouseUpEvent {
            position,
            ..Default::default()
        });
        view.scroll(&ScrollEvent {
            position,
            delta: ScrollDelta::Lines(point(0., 1.)),
            ..Default::default()
        });
        assert!(view.backend.term.lock_unfair().selection.is_none());
    });
    let mut result = None;
    wait(|| {
        entity.read_with(cx, |view, _| {
            let title = view.backend.take_events().title;
            result = title
                .filter(|title| title == "OK" || title.starts_with("FAIL:"))
                .or(result.take());
            result.is_some()
        })
    });
    assert_eq!(result.as_deref(), Some("OK"));
    println!("PASS actual view mouse handlers: exact SGR press/release/wheel");
}

#[::gpui::test]
fn ime_preedit_utf16_commit_selection_and_unicode_snapshots(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    let backend = python(query_script(b"\x1b]2;READY\x07", "界😀e\u{301}".as_bytes()));
    let handle = cx.add_window(|_, cx| TerminalView::new(backend, TerminalConfig::default(), cx));
    let mut ready = false;
    wait(|| {
        handle
            .update(cx, |view, _, _| {
                ready |= view.backend.take_events().title.as_deref() == Some("READY");
                ready
            })
            .unwrap()
    });
    handle
        .update(cx, |view, window, cx| {
            let event = KeyDownEvent {
                is_held: false,
                prefer_character_input: false,
                keystroke: Keystroke {
                    key: "a".into(),
                    key_char: Some("a".into()),
                    ..Default::default()
                },
            };
            assert!(should_defer_text_input_to_ime(&event));
            view.replace_and_mark_text_in_range(None, "界😀", Some(1..3), window, cx);
            assert_eq!(view.marked_text_range(window, cx), Some(1..3));
            let mut adjusted = None;
            assert_eq!(
                view.text_for_range(0..3, &mut adjusted, window, cx)
                    .as_deref(),
                Some("界😀")
            );
            assert_eq!(adjusted, Some(1..3));
            view.unmark_text(window, cx);
            assert!(view.ime_text.is_empty());
            view.replace_text_in_range(None, "界😀e\u{301}", window, cx);
            assert!(view.marked_text_range(window, cx).is_none());
        })
        .unwrap();
    let mut result = None;
    wait(|| {
        handle
            .update(cx, |view, _, _| {
                result = view.backend.take_events().title.or(result.take());
                result.is_some()
            })
            .unwrap()
    });
    assert_eq!(result.as_deref(), Some("OK"));
}

#[::gpui::test]
fn configured_menlo_cell_width_matches_measured_monospace_advance(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    use gpui_kit::{Font, FontFeatures, FontStyle, FontWeight, TextRun, black};

    let backend = Backend::new(BackendSettings::default()).unwrap();
    let handle = cx.add_window(|_, cx| TerminalView::new(backend, TerminalConfig::default(), cx));
    handle
        .update(cx, |view, window, _| {
            assert_eq!(view.config.font_family, "Menlo");
            assert_eq!(view.config.font_size, px(14.0));
            view.renderer.measure_cell(window);
            let run = TextRun {
                len: 1,
                font: Font {
                    family: "Menlo".into(),
                    features: FontFeatures::default(),
                    fallbacks: None,
                    weight: FontWeight::NORMAL,
                    style: FontStyle::Normal,
                },
                color: black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped = window
                .text_system()
                .shape_line("m".into(), px(14.0), &[run], None);
            let difference: f32 = (view.renderer.cell_width - shaped.width).into();
            assert!(difference.abs() <= 0.5, "advance difference: {difference}");
            let measured_height = shaped.ascent + shaped.descent;
            let height_difference: f32 = (view.renderer.cell_height - measured_height).into();
            assert!(
                height_difference.abs() <= 0.5,
                "line height difference: {height_difference}"
            );
        })
        .unwrap();
}

#[::gpui::test]
fn deferred_shell_starts_with_first_laid_out_grid(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    use gpui_kit::{AvailableSpace, IntoElement, ParentElement, Styled, div, size};

    cx.background_executor.allow_parking();
    cx.update(gpui_kit::init);
    let backend_settings = BackendSettings {
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "set -- $(stty size); printf '\\033]2;INITIAL:%s:%s\\007' \"$2\" \"$1\"; exec sleep 3"
                .into(),
        ],
        ..Default::default()
    };
    let first_layout = Arc::new(parking_lot::Mutex::new(None));
    let initial_prompt = Arc::new(parking_lot::Mutex::new(None));
    let (entity, cx) = cx.add_window_view(|_, cx| {
        let first_layout = first_layout.clone();
        let initial_prompt = initial_prompt.clone();
        TerminalView::new_deferred(backend_settings, TerminalConfig::default(), cx)
            .with_resize_callback(move |cols, rows| {
                let mut first_layout = first_layout.lock();
                if first_layout.is_none() {
                    *first_layout = Some((cols, rows));
                }
            })
            .with_title_callback(move |_, _, title| {
                if title.starts_with("INITIAL:") {
                    *initial_prompt.lock() = Some(title.to_string());
                }
            })
    });

    let view = entity.clone();
    cx.draw(
        point(px(0.), px(0.)),
        size(
            AvailableSpace::Definite(px(800.)),
            AvailableSpace::Definite(px(480.)),
        ),
        move |_, _| div().size_full().child(view).into_any_element(),
    );

    wait(|| {
        // Title callbacks run on render; an OS child may respond after the first frame.
        let view = entity.clone();
        cx.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(px(800.)),
                AvailableSpace::Definite(px(480.)),
            ),
            move |_, _| div().size_full().child(view).into_any_element(),
        );
        initial_prompt.lock().is_some()
    });
    entity.read_with(cx, |view, _| {
        let (cols, rows) = first_layout.lock().expect("first view layout");
        assert_eq!(
            initial_prompt.lock().as_deref(),
            Some(format!("INITIAL:{cols}:{rows}").as_str())
        );
        assert!(!view.backend.is_deferred());
    });
}

#[::gpui::test]
fn real_pty_resize_reaches_child_and_grid(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    let backend = python(
        r#"import os,tty,time
tty.setraw(0)
os.write(1,b'\x1b]2;READY\x07')
os.read(0,1)
s=os.get_terminal_size(0)
os.write(1,b'\x1b]2;' + ('SIZE:%d:%d' % (s.columns,s.lines)).encode() + b'\x07')
"#
        .into(),
    );
    let entity = cx.new(|cx| TerminalView::new(backend, TerminalConfig::default(), cx));
    let mut ready = false;
    wait(|| {
        entity.read_with(cx, |view, _| {
            ready |= view.backend.take_events().title.as_deref() == Some("READY");
            ready
        })
    });
    entity.update(cx, |view, _| {
        view.resize(103, 31);
        assert!(view.backend.sync());
        view.write_input_bytes(b"!");
    });
    let mut result = None;
    wait(|| {
        entity.read_with(cx, |view, _| {
            let title = view.backend.take_events().title;
            result = title
                .filter(|title| title.starts_with("SIZE:"))
                .or(result.take());
            result.is_some()
        })
    });
    assert_eq!(result.as_deref(), Some("SIZE:103:31"));
    entity.read_with(cx, |view, _| {
        let term = view.backend.term.lock_unfair();
        assert_eq!((term.columns(), term.screen_lines()), (103, 31));
    });
}

#[::gpui::test]
fn hidden_view_200mb_no_render_no_foreground_drain_bounds_memory_and_verifies_all_bytes(
    cx: &mut TestAppContext,
) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    use std::sync::atomic::Ordering;
    use test_support::{Counts, ObservedPty, PATTERN, TAIL, TOTAL, hex, rss_kib};
    cx.background_executor.allow_parking();
    let script = format!(
        r#"import os,tty
tty.setraw(0)
pattern=bytes.fromhex('{pattern}')
block=pattern*1024
left={total}
while left:
    chunk=block[:min(left,len(block))]
    offset=0
    while offset<len(chunk): offset+=os.write(1,chunk[offset:])
    left-=len(chunk)
os.write(1,bytes.fromhex('{tail}'))
"#,
        pattern = hex(PATTERN),
        total = TOTAL,
        tail = hex(TAIL)
    );
    let options = tty::Options {
        shell: Some(tty::Shell::new(
            "/usr/bin/python3".into(),
            vec!["-u".into(), "-c".into(), script],
        )),
        ..Default::default()
    };
    let counts = Arc::new(Counts::default());
    let pty = tty::new(
        &options,
        crate::backend::TerminalSize::default().into(),
        278,
    )
    .unwrap();
    let backend = Backend::with_pty(
        BackendSettings::default(),
        ObservedPty::new(pty, counts.clone()),
    )
    .unwrap();
    let entity = cx.new(|cx| {
        TerminalView::new(
            backend,
            TerminalConfig {
                rows: 50,
                ..Default::default()
            },
            cx,
        )
    });
    let started = Instant::now();
    let start_rss = rss_kib();
    let mut warm_rss = None;
    let mut max_after_warm = 0;
    // Deliberately never pump GPUI or render. Byte observation happens directly
    // inside Alacritty's real PTY reader; it cannot hide a foreground byte queue.
    while counts.bytes.load(Ordering::Acquire) < TOTAL + TAIL.len() {
        let bytes = counts.bytes.load(Ordering::Acquire);
        let rss = rss_kib();
        if bytes >= 50_000_000 {
            let warm = *warm_rss.get_or_insert(rss);
            max_after_warm = max_after_warm.max(rss);
            assert!(
                rss < warm + 64 * 1024,
                "memory grew after history cap: {warm} -> {rss} KiB"
            );
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "hidden parser deadline: {bytes}/{} bytes after {:?}",
            TOTAL + TAIL.len(),
            started.elapsed()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(counts.bytes.load(Ordering::Acquire), TOTAL + TAIL.len());
    assert_eq!(counts.bad.load(Ordering::Relaxed), 0);
    entity.update(cx, |view, _| {
        // First snapshot is still the initial viewport: no foreground updates.
        assert_eq!(view.backend.renderable_content().cells.len(), 80 * 50);
        assert_eq!(view.backend.renderable_content().cells[0].cell.c, ' ');
        let term = view.backend.term.clone();
        {
            let guard = term.lock_unfair();
            assert_eq!(guard.grid().history_size(), 100_000);
            assert_eq!(guard.grid()[Cell::new(Line(0), Column(0))].c, 'F');
        }
        wait(|| view.backend.sync());
        assert_eq!(view.backend.renderable_content().cells[0].cell.c, 'F');
        assert_eq!(view.backend.renderable_content().cells.len(), 80 * 50);
        assert_eq!(view.backend.take_events().title.as_deref(), Some("DONE"));
    });
    println!(
        "PASS hidden TerminalView: bytes={} mismatches=0 history=100000 snapshot=4000 wall_s={:.3} rss_start_kib={start_rss} warm_rss_kib={warm_rss:?} max_after_warm_kib={max_after_warm}",
        TOTAL + TAIL.len(),
        started.elapsed().as_secs_f64()
    );
}

#[::gpui::test]
fn headless_paint_selection_preedit_and_resize_use_bounded_snapshot(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    use gpui_kit::{AvailableSpace, IntoElement, ParentElement, Styled, div, size};
    cx.background_executor.allow_parking();
    cx.update(gpui_kit::init);
    let backend = Backend::new(BackendSettings {
        program: "/bin/cat".into(),
        ..Default::default()
    })
    .unwrap();
    {
        let mut term = backend.term.lock_unfair();
        let mut parser: alacritty_terminal::vte::ansi::Processor = Default::default();
        for byte in "A界e\u{301} █╬\r\nsecond".as_bytes() {
            parser.advance(&mut *term, &[*byte]);
        }
    }
    let (entity, cx) =
        cx.add_window_view(|_, cx| TerminalView::new(backend, Default::default(), cx));
    entity.update(cx, |view, _| {
        view.backend.select(
            Cell::new(Line(0), Column(0)),
            alacritty_terminal::selection::SelectionType::Simple,
            false,
        );
        view.backend.extend_selection(Cell::new(Line(0), Column(4)));
        assert!(view.backend.sync());
        let snapshot = view.backend.renderable_content();
        assert_eq!(snapshot.cells[1].cell.c, '界');
        assert!(
            snapshot.cells[2]
                .cell
                .flags
                .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER)
        );
        assert_eq!(
            snapshot.cells[3].cell.zerowidth(),
            Some(['\u{301}'].as_slice())
        );
        assert_eq!(
            view.backend.selectable_content().as_deref(),
            Some("A界e\u{301} ")
        );
        view.ime_text = "日本😀".into();
        view.ime_marked_range = Some(0..2);
    });
    for (width, height) in [(800., 480.), (640., 360.)] {
        let view = entity.clone();
        cx.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(px(width)),
                AvailableSpace::Definite(px(height)),
            ),
            move |_, _| div().size_full().child(view).into_any_element(),
        );
        entity.update(cx, |view, _| {
            assert!(view.backend.sync());
            let (cols, rows) = view.dimensions();
            assert!(cols > 2 && rows > 1);
            assert_eq!(view.backend.renderable_content().cells.len(), cols * rows);
            assert!(view.layout_metrics.lock().cell_width > px(0.));
        });
    }
}

#[::gpui::test]
fn drag_shift_copy_and_native_copy_survive_parser_contention(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    use gpui_kit::Modifiers;
    cx.background_executor.allow_parking();
    let backend = Backend::new(BackendSettings {
        program: "/bin/cat".into(),
        ..Default::default()
    })
    .unwrap();
    {
        let mut term = backend.term.lock_unfair();
        let mut parser: alacritty_terminal::vte::ansi::Processor = Default::default();
        parser.advance(&mut *term, b"abcdefghij");
    }
    let handle = cx.add_window(|_, cx| TerminalView::new(backend, Default::default(), cx));
    handle
        .update(cx, |view, window, cx| {
            assert!(view.backend.sync());
            *view.layout_metrics.lock() = LayoutMetrics {
                origin: point(px(0.), px(0.)),
                cell_width: px(10.),
                cell_height: px(20.),
                cols: 80,
                rows: 24,
            };
            view.mouse_down(
                &MouseDownEvent {
                    position: point(px(15.), px(5.)),
                    click_count: 1,
                    ..Default::default()
                },
                cx,
            );
            let term = view.backend.term.clone();
            let guard = term.lock_unfair();
            view.mouse_up(&MouseUpEvent {
                position: point(px(35.), px(5.)),
                ..Default::default()
            });
            assert!(view.pending_copy);
            drop(guard);
            assert!(view.copy_selection());
            assert_eq!(
                crate::clipboard::Clipboard::new()
                    .unwrap()
                    .get_text()
                    .unwrap(),
                "bcd"
            );
            view.mouse_down(
                &MouseDownEvent {
                    position: point(px(65.), px(5.)),
                    click_count: 1,
                    modifiers: Modifiers {
                        shift: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                cx,
            );
            view.mouse_up(&MouseUpEvent {
                position: point(px(65.), px(5.)),
                ..Default::default()
            });
            assert_eq!(
                crate::clipboard::Clipboard::new()
                    .unwrap()
                    .get_text()
                    .unwrap(),
                "bcdefg"
            );
            let guard = term.lock_unfair();
            view.on_key_down(
                &KeyDownEvent {
                    keystroke: Keystroke {
                        key: "c".into(),
                        modifiers: Modifiers {
                            platform: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    is_held: false,
                    prefer_character_input: false,
                },
                window,
                cx,
            );
            assert!(view.pending_copy);
            drop(guard);
            assert!(view.copy_selection());
            assert_eq!(
                crate::clipboard::Clipboard::new()
                    .unwrap()
                    .get_text()
                    .unwrap(),
                "bcdefg"
            );
        })
        .unwrap();
}

#[::gpui::test]
fn dropping_view_joins_reader_and_reaps_child(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    let backend = python("import os, signal, time\nsignal.signal(signal.SIGHUP, signal.SIG_IGN)\nos.write(1,b'\x1b]2;' + str(os.getpid()).encode() + b'\x07')\ntime.sleep(60)".into());
    let weak = Arc::downgrade(&backend.term);
    let entity = cx.new(|cx| TerminalView::new(backend, TerminalConfig::default(), cx));
    let mut pid = None;
    wait(|| {
        pid = entity
            .read_with(cx, |view, _| view.backend.take_events().title)
            .and_then(|s| s.parse::<i32>().ok())
            .or(pid);
        pid.is_some()
    });
    drop(entity);
    cx.update(|_| {}); // Flush GPUI's deferred entity release.
    cx.run_until_parked();
    assert!(
        weak.upgrade().is_none(),
        "reader must release the terminal when view closes"
    );
    assert_eq!(unsafe { libc::kill(pid.unwrap(), 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid.unwrap(), &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[::gpui::test]
fn exited_and_failed_views_paint_persistent_status_and_ignore_input(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    use gpui_kit::{AvailableSpace, IntoElement, ParentElement, Styled, div, size};
    cx.background_executor.allow_parking();
    cx.update(gpui_kit::init);
    for failure in [false, true] {
        let backend = if failure {
            Backend::new_deferred(BackendSettings {
                program: "/no-such-terminal-test-shell".into(),
                ..Default::default()
            })
        } else {
            let backend = python("import sys\nsys.exit(17)".into());
            wait(|| backend.state() != crate::SessionState::Running);
            backend
        };
        let (entity, cx) =
            cx.add_window_view(|_, cx| TerminalView::new(backend, TerminalConfig::default(), cx));
        let view = entity.clone();
        cx.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(px(500.)),
                AvailableSpace::Definite(px(300.)),
            ),
            move |_, _| div().size_full().child(view).into_any_element(),
        );
        wait(|| {
            entity.read_with(cx, |view, _| {
                view.backend.state() != crate::SessionState::Running
            })
        });
        entity.update(cx, |view, _| {
            if failure {
                assert!(view.status_message().unwrap().contains("Terminal error:"));
            } else {
                assert_eq!(
                    view.status_message().as_deref(),
                    Some("Shell exited (status 17)")
                );
            }
            view.backend.take_events();
            assert!(view.status_message().is_some());
            assert!(!view.write_input_bytes(b"x"));
            assert!(!view.backend.paste("ignored"));
        });
    }
}

#[::gpui::test]
fn legacy_mouse_press_release_and_wheel_use_non_sgr_bytes(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    let backend = python(query_script(
        b"\x1b[?1049h\x1b[?1000h",
        b"\x1b[M\x20\x22\x22\x1b[M\x23\x22\x22\x1b[M\x60\x22\x22",
    ));
    let entity = cx.new(|cx| TerminalView::new(backend, TerminalConfig::default(), cx));
    wait(|| {
        entity.update(cx, |view, _| {
            view.backend.sync() && view.backend.mode().contains(TermMode::MOUSE_REPORT_CLICK)
        })
    });
    entity.update(cx, |view, cx| {
        *view.layout_metrics.lock() = LayoutMetrics {
            origin: point(px(0.), px(0.)),
            cell_width: px(10.),
            cell_height: px(20.),
            cols: 80,
            rows: 24,
        };
        let position = point(px(15.), px(25.));
        // Consumed Cmd-click must suppress its release even if Cmd is released first.
        view.mouse_down(
            &MouseDownEvent {
                position,
                modifiers: gpui_kit::Modifiers {
                    platform: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            cx,
        );
        view.mouse_up(&MouseUpEvent {
            position,
            ..Default::default()
        });
        view.mouse_down(
            &MouseDownEvent {
                position,
                click_count: 1,
                ..Default::default()
            },
            cx,
        );
        // A regular press still needs a release if Cmd is pressed in between.
        view.mouse_up(&MouseUpEvent {
            position,
            modifiers: gpui_kit::Modifiers {
                platform: true,
                ..Default::default()
            },
            ..Default::default()
        });
        view.scroll(&ScrollEvent {
            position,
            delta: ScrollDelta::Lines(point(0., 1.)),
            ..Default::default()
        });
        assert!(view.backend.term.lock_unfair().selection.is_none());
    });
    let mut title = None;
    wait(|| {
        title = entity
            .read_with(cx, |view, _| view.backend.take_events().title)
            .filter(|title| title == "OK" || title.starts_with("FAIL:"))
            .or(title.take());
        title.is_some()
    });
    assert_eq!(title.as_deref(), Some("OK"));
}

#[::gpui::test]
fn cmd_hover_and_click_resolve_current_url_and_call_only_app_opener(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    use alacritty_terminal::vte::ansi::Processor;
    use gpui_kit::{Modifiers, ModifiersChangedEvent, MouseMoveEvent};
    use std::{cell::RefCell, rc::Rc};
    cx.background_executor.allow_parking();
    let opened = Rc::new(RefCell::new(Vec::new()));
    let callback = opened.clone();
    let backend = Backend::new(BackendSettings {
        program: "/bin/cat".into(),
        ..Default::default()
    })
    .unwrap();
    let handle = cx.add_window(|_, cx| {
        TerminalView::new(backend, TerminalConfig::default(), cx)
            .with_url_opener(move |url| callback.borrow_mut().push(url.to_owned()))
    });
    handle
        .update(cx, |view, window, cx| {
            let mut parser: Processor = Processor::new();
            parser.advance(
                &mut *view.backend.term.lock_unfair(),
                b"https://example.com/path other\r\nhttps://example.org/second\x1b[?1000h",
            );
            assert!(view.backend.sync());
            *view.layout_metrics.lock() = LayoutMetrics {
                origin: point(px(0.), px(0.)),
                cell_width: px(10.),
                cell_height: px(20.),
                cols: 80,
                rows: 24,
            };
            let position = point(px(35.), px(10.));
            view.on_mouse_move(
                &MouseMoveEvent {
                    position,
                    ..Default::default()
                },
                window,
                cx,
            );
            assert!(
                view.backend
                    .renderable_content()
                    .hovered_hyperlink
                    .is_none()
            );
            view.on_modifiers_changed(
                &ModifiersChangedEvent {
                    modifiers: Modifiers {
                        platform: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                window,
                cx,
            );
            assert!(
                view.backend
                    .renderable_content()
                    .hovered_hyperlink
                    .is_some()
            );
            let click = |position| MouseDownEvent {
                position,
                click_count: 1,
                modifiers: Modifiers {
                    platform: true,
                    ..Default::default()
                },
                ..Default::default()
            };
            view.mouse_down(&click(position), cx);
            // Clicking another URL without a prior hover must never use the old URL.
            view.mouse_down(&click(point(px(35.), px(30.))), cx);
            view.mouse_down(&click(point(px(600.), px(30.))), cx);
            assert_eq!(
                &*opened.borrow(),
                &["https://example.com/path", "https://example.org/second"]
            );
            assert!(view.backend.term.lock_unfair().selection.is_none());
            // Lock contention clears hover and refuses an old cached destination.
            let term = view.backend.term.clone();
            let guard = term.lock_unfair();
            view.mouse_down(&click(position), cx);
            assert_eq!(opened.borrow().len(), 2);
            drop(guard);
            view.on_modifiers_changed(&ModifiersChangedEvent::default(), window, cx);
            assert!(
                view.backend
                    .renderable_content()
                    .hovered_hyperlink
                    .is_none()
            );
        })
        .unwrap();
}

#[::gpui::test]
fn find_busy_polls_notify_only_when_visible_state_changes(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    use std::{cell::Cell, rc::Rc, sync::atomic::Ordering};
    cx.background_executor.allow_parking();
    let entity = cx.new(|cx| {
        let mut view =
            TerminalView::new_deferred(BackendSettings::default(), TerminalConfig::default(), cx);
        view.backend.service_busy.store(true, Ordering::Release);
        view.open_find(cx);
        view.set_find_query("needle".into(), cx);
        view.execute_find(cx);
        view
    });
    let notifications = Rc::new(Cell::new(0));
    let observed = notifications.clone();
    let _subscription =
        cx.update(|cx| cx.observe(&entity, move |_, _| observed.set(observed.get() + 1)));
    entity.update(cx, |view, cx| view.poll_services(cx));
    assert_eq!(notifications.get(), 0);
    entity.update(cx, |view, cx| view.poll_services(cx));
    assert_eq!(notifications.get(), 0);
}

#[::gpui::test]
fn iced_find_input_submit_count_buttons_shortcuts_and_close_without_process(
    cx: &mut TestAppContext,
) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    use alacritty_terminal::vte::ansi::Processor;
    use gpui_kit::{Entity, VisualTestContext, component::Root, size};
    cx.background_executor.allow_parking();
    cx.update(gpui_kit::init);
    let mut backend = Backend::new_deferred(BackendSettings {
        program: "/must-not-spawn-find-fixture".into(),
        ..Default::default()
    });
    backend.cancel(); // In-memory terminal only: rendering cannot start a PTY.
    {
        let mut term = backend.term.lock_unfair();
        let mut parser: Processor = Processor::new();
        parser.advance(&mut *term, b"first[.]\r\nsecond[.]\r\n");
        backend.events.invalidate();
    }
    let received = Arc::new(parking_lot::Mutex::new(None));
    let callback = received.clone();
    let mut entity: Option<Entity<TerminalView>> = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| {
            TerminalView::new(backend, TerminalConfig::default(), cx)
                .with_export_callback(move |result, _| *callback.lock() = Some(result))
        });
        entity = Some(view.clone());
        Root::new(view, window, cx)
    });
    let entity = entity.unwrap();
    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        cx.run_until_parked();
    }
    fn click(id: &'static str, cx: &mut VisualTestContext) {
        draw(cx);
        let bounds = cx
            .debug_bounds(id)
            .unwrap_or_else(|| panic!("missing {id}"));
        cx.simulate_click(bounds.center(), gpui_kit::Modifiers::none());
        draw(cx);
    }
    cx.simulate_resize(size(px(1000.), px(400.)));
    cx.update(|window, cx| entity.read(cx).focus_handle.clone().focus(window, cx));
    draw(cx);
    cx.simulate_keystrokes("cmd-f");
    draw(cx);
    cx.update(|window, cx| {
        use gpui::Focusable as _;
        let view = entity.read(cx);
        assert!(
            !view.focus_handle.is_focused(window),
            "focus is in the input"
        );
        assert!(
            view.find_input
                .as_ref()
                .unwrap()
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        );
    });
    cx.simulate_input("[x]");
    cx.simulate_keystrokes("left backspace");
    cx.simulate_input("."); // Edit in the middle: this must be a real editable field.
    draw(cx);
    cx.read(|cx| {
        let view = entity.read(cx);
        assert_eq!(view.find_query, "[.]");
        assert_eq!(view.find_count_text(), "No matches");
        assert!(
            view.find_job.is_none(),
            "typing must not execute the search"
        );
    });
    click("terminal-find-next", cx);
    click("terminal-find-prev", cx);
    cx.read(|cx| assert_eq!(entity.read(cx).find_count, 0));
    cx.simulate_keystrokes("enter");
    wait(|| {
        entity.update(cx, |view, cx| {
            view.poll_services(cx);
            view.find_count_text() == "1/2"
        })
    });
    draw(cx);
    let first = cx.read(|cx| {
        entity
            .read(cx)
            .backend
            .renderable_content()
            .find_match
            .as_ref()
            .unwrap()
            .start
    });
    click("terminal-find-next", cx);
    cx.read(|cx| {
        let view = entity.read(cx);
        assert_eq!(view.find_count_text(), "2/2");
        assert_ne!(
            view.backend
                .renderable_content()
                .find_match
                .as_ref()
                .unwrap()
                .start,
            first
        );
    });
    click("terminal-find-prev", cx);
    cx.read(|cx| assert_eq!(entity.read(cx).find_count_text(), "1/2"));
    click("terminal-find-prev", cx);
    cx.read(|cx| assert_eq!(entity.read(cx).find_count_text(), "2/2"));
    click("terminal-find-next", cx);
    cx.read(|cx| assert_eq!(entity.read(cx).find_count_text(), "1/2"));
    cx.simulate_keystrokes("cmd-g");
    draw(cx);
    cx.read(|cx| assert_eq!(entity.read(cx).find_count_text(), "2/2"));
    // Iced's key handler catches Cmd+Shift+G for its debug agent tab before the
    // terminal SearchPrev branch, so the terminal adds no previous-match chord.
    cx.simulate_keystrokes("cmd-shift-g");
    cx.read(|cx| assert_eq!(entity.read(cx).find_count_text(), "2/2"));
    cx.simulate_keystrokes("enter");
    wait(|| {
        entity.update(cx, |view, cx| {
            view.poll_services(cx);
            view.find_count_text() == "1/2"
        })
    });
    cx.simulate_keystrokes("cmd-a");
    cx.simulate_input("draft");
    cx.read(|cx| {
        let view = entity.read(cx);
        assert_eq!(view.find_query, "draft");
        assert_eq!(
            view.find_count_text(),
            "1/2",
            "editing retains executed results"
        );
        assert!(view.find_job.is_none());
    });
    click("terminal-find-close", cx);
    cx.update(|window, cx| {
        let view = entity.read(cx);
        assert!(!view.find_open);
        assert!(view.find_query.is_empty());
        assert!(view.backend.renderable_content().find_match.is_none());
        assert!(view.focus_handle.is_focused(window));
    });
    cx.simulate_keystrokes("cmd-f");
    draw(cx);
    cx.simulate_input("missing");
    cx.simulate_keystrokes("enter");
    wait(|| {
        entity.update(cx, |view, cx| {
            view.poll_services(cx);
            view.find_revision.is_some()
        })
    });
    cx.read(|cx| assert_eq!(entity.read(cx).find_count_text(), "No matches"));
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert!(cx.debug_bounds("terminal-find").is_none());
    cx.read(|cx| {
        let view = entity.read(cx);
        assert!(!view.find_open);
        assert!(!view.backend.is_deferred());
        assert_eq!(view.backend.state(), crate::SessionState::Cancelled);
    });
    wait(|| entity.update(cx, |view, _| view.request_export().is_ok()));
    wait(|| {
        entity.update(cx, |view, cx| view.poll_services(cx));
        received.lock().is_some()
    });
    assert!(
        received
            .lock()
            .take()
            .unwrap()
            .unwrap()
            .starts_with("first[.]\nsecond[.]\n")
    );
}

#[::gpui::test]
fn launch_cwd_and_title_callbacks_reach_app_headlessly(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    use std::{cell::RefCell, rc::Rc};
    cx.background_executor.allow_parking();
    let cwd = Rc::new(RefCell::new(None));
    let title = Rc::new(RefCell::new(None));
    let reported_cwd = cwd.clone();
    let reported_title = title.clone();
    let backend = python("import os\nos.write(1,b'\\x1b]7;file:///tmp/callback%20cwd\\x07\\x1b]2;CALLBACK_TITLE\\x07')".into());
    let handle = cx.add_window(|_, cx| {
        TerminalView::new(backend, TerminalConfig::default(), cx)
            .with_cwd_callback(move |_, _, path| {
                *reported_cwd.borrow_mut() = Some(path.to_path_buf())
            })
            .with_title_callback(move |_, _, value| {
                *reported_title.borrow_mut() = Some(value.to_string())
            })
    });
    wait(|| {
        handle
            .update(cx, |view, window, cx| view.process_events(window, cx))
            .unwrap();
        cwd.borrow().is_some() && title.borrow().is_some()
    });
    assert_eq!(
        cwd.borrow().as_deref(),
        Some(std::path::Path::new("/tmp/callback cwd"))
    );
    assert_eq!(title.borrow().as_deref(), Some("CALLBACK_TITLE"));
}

#[::gpui::test]
fn terminal_osc52_callback_stores_decoded_text_and_rejects_invalid_payloads(
    cx: &mut TestAppContext,
) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    use alacritty_terminal::vte::ansi::Processor;
    let mut backend = Backend::new_deferred(BackendSettings::default());
    backend.cancel();
    let stored = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let callback = stored.clone();
    let window = cx.add_window(|_, cx| {
        TerminalView::new(backend, Default::default(), cx)
            .with_clipboard_store_callback(move |_, _, text| callback.lock().push(text.to_owned()))
    });
    window
        .update(cx, |view, window, cx| {
            let mut parser: Processor = Processor::new();
            for payload in [
                b"\x1b]52;c;bad*base64\x07".as_slice(),
                b"\x1b]52;c;/w==\x07",
                b"\x1b]52;x;YQ==\x07",
            ] {
                parser.advance(&mut *view.backend.term.lock_unfair(), payload);
                view.process_events(window, cx);
            }
            assert!(stored.lock().is_empty());
            parser.advance(
                &mut *view.backend.term.lock_unfair(),
                b"\x1b]52;c;Zml4dHVyZSDinJM=\x07",
            );
            view.process_events(window, cx);
            assert_eq!(&*stored.lock(), &["fixture ✓"]);
            // Iced adds no app-level size cap; the shared parser accepts a larger payload too.
            let payload = format!("\x1b]52;s;{}\x07", "eHh4".repeat(1366));
            parser.advance(&mut *view.backend.term.lock_unfair(), payload.as_bytes());
            view.process_events(window, cx);
            assert_eq!(stored.lock()[1], "x".repeat(4098));
        })
        .unwrap();
}

#[::gpui::test]
fn terminal_scrollback_limit_survives_deferred_pty_start(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    let entity = cx.new(|cx| {
        TerminalView::new_deferred(
            BackendSettings {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "read fixture".into()],
                ..Default::default()
            },
            TerminalConfig {
                scrollback: 13,
                ..Default::default()
            },
            cx,
        )
    });
    entity.update(cx, |view, _| {
        assert!(view.backend.sync());
        assert!(view.backend.initialize_deferred(80, 24, 8, 16).unwrap());
        let mut parser: alacritty_terminal::vte::ansi::Processor = Default::default();
        parser.advance(
            &mut *view.backend.term.lock_unfair(),
            "line\r\n".repeat(100).as_bytes(),
        );
        assert_eq!(view.backend.term.lock_unfair().grid().history_size(), 13);
    });
}

#[::gpui::test]
fn golive10_adopted_callbacks_preserve_launch_owner(cx: &mut TestAppContext) {
    use std::{cell::RefCell, rc::Rc};
    cx.background_executor.allow_parking();
    let events = Rc::new(RefCell::new(Vec::new()));
    let backend = python(
        "import os\nos.write(1,b'\\x1b]7;file:///tmp/placeholder\\x07\\x1b]2;PLACEHOLDER\\x07')"
            .into(),
    );
    let seen = events.clone();
    let handle = cx.add_window(|_, cx| {
        let (cwd, title, exit) = (seen.clone(), seen.clone(), seen.clone());
        let mut view = TerminalView::new(backend, TerminalConfig::default(), cx)
            .with_cwd_callback(move |_, _, _| cwd.borrow_mut().push("owner cwd"))
            .with_title_callback(move |_, _, _| title.borrow_mut().push("owner title"))
            .with_exit_callback(move |_, _| exit.borrow_mut().push("owner exit"));
        let (cwd, title, exit) = (seen.clone(), seen.clone(), seen.clone());
        view.add_cwd_callback(move |_, _, _| cwd.borrow_mut().push("pane cwd"));
        view.add_title_callback(move |_, _, _| title.borrow_mut().push("pane title"));
        view.add_exit_callback(move |_, _| exit.borrow_mut().push("pane exit"));
        view
    });
    wait(|| {
        handle
            .update(cx, |view, window, cx| view.process_events(window, cx))
            .unwrap();
        ["pane cwd", "pane title", "pane exit"]
            .iter()
            .all(|event| events.borrow().contains(event))
    });
    let events = events.borrow();
    for pair in [
        ["owner cwd", "pane cwd"],
        ["owner title", "pane title"],
        ["owner exit", "pane exit"],
    ] {
        assert!(
            events.windows(2).any(|events| events == pair),
            "{pair:?}: {events:?}"
        );
    }
}
