use super::*;
use crate::test_support::{python, query_script, wait};
use alacritty_terminal::vte::ansi::Processor;

#[test]
fn cpr_and_da_reach_waiting_child_without_foreground_pumping() {
    for (query, expected) in [
        (b"\x1b[3;5H\x1b[6n".as_slice(), b"\x1b[3;5R".as_slice()),
        (b"\x1b[c".as_slice(), b"\x1b[?6c".as_slice()),
    ] {
        let backend = python(query_script(query, expected));
        let mut result = None;
        wait(|| {
            result = backend.take_events().title.or(result.take());
            result.is_some()
        });
        assert_eq!(result.as_deref(), Some("OK"));
    }
}

#[test]
fn first_retained_cell_extraction_and_unicode_regression() {
    use alacritty_terminal::event::VoidListener;
    let mut term = Term::new(term::Config::default(), &TermSize::new(8, 2), VoidListener);
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, "A界e\u{301}\r\nsecond".as_bytes());
    assert_eq!(Backend::all_text(&term), "A界e\u{301}\nsecond");
    // Also exercise an evicted/retained history boundary.
    term.set_options(term::Config {
        scrolling_history: 2,
        ..Default::default()
    });
    parser.advance(&mut term, b"\r\nthird\r\nfourth\r\nfifth");
    assert_eq!(Backend::all_text(&term), "second\nthird\nfourth\nfifth");
}

#[test]
fn history_100k_literal_search_scroll_and_contention_are_explicit() {
    let mut backend = Backend::new(BackendSettings {
        program: "/bin/cat".into(),
        ..Default::default()
    })
    .unwrap();
    {
        let mut term = backend.term.lock_unfair();
        let mut parser: Processor = Processor::new();
        for i in 0..100_100 {
            parser.advance(&mut *term, format!("row{i:06} needle[.]\r\n").as_bytes());
        }
        assert_eq!(term.grid().history_size(), 100_000);
    }
    let matches = backend.search_all("row000500 needle[.]").unwrap();
    assert_eq!(matches.len(), 1);
    backend.scroll_to_line(matches[0].start.line.0);
    let text = backend.get_all_text().unwrap();
    assert!(text.starts_with("row000051 needle[.]\n"));
    assert!(text.contains("row000500 needle[.]"));
    assert!(text.contains("row100099 needle[.]"));
    assert!(backend.sync());
    assert!(backend.last_content.display_offset > 99_000);
    assert_eq!(backend.last_content.cells.len(), 80 * 50);
    let term = backend.term.clone();
    let guard = term.lock_unfair();
    assert!(backend.get_all_text().is_none());
    assert!(backend.search_all("needle").is_none());
    assert!(!backend.sync());
    assert!(backend.needs_sync());
    assert!(backend.input(b"x".to_vec()));
    drop(guard);
    assert!(backend.sync());
    assert_eq!(backend.renderable_content().display_offset, 0);
}

#[test]
fn deferred_selection_and_resize_survive_parser_lock_contention() {
    let mut backend = Backend::new(BackendSettings {
        program: "/bin/cat".into(),
        ..Default::default()
    })
    .unwrap();
    let term = backend.term.clone();
    let mut guard = term.lock_unfair();
    let mut parser: Processor = Processor::new();
    parser.advance(&mut *guard, b"abcdefghij");
    backend.select(Point::new(Line(0), Column(1)), SelectionType::Simple, false);
    backend.extend_selection(Point::new(Line(0), Column(3)));
    backend.resize_cells(100, 30);
    assert!(!backend.sync());
    drop(guard);
    assert!(backend.sync());
    assert_eq!(backend.selectable_content().as_deref(), Some("bcd"));
    backend.select(Point::new(Line(0), Column(6)), SelectionType::Simple, true);
    assert!(backend.sync());
    assert_eq!(backend.selectable_content().as_deref(), Some("bcdefg"));
    let guard = term.lock_unfair();
    assert_eq!((guard.columns(), guard.screen_lines()), (100, 30));
}

#[test]
fn first_layout_grid_matches_initial_pty_geometry_without_resize() {
    let cols = 103;
    let rows = 31;
    let mut backend = Backend::new_sized(BackendSettings::default(), cols, rows, 9, 19).unwrap();
    let (initial_cols, initial_rows) = backend.dimensions();
    assert_eq!((initial_cols, initial_rows), (cols, rows));

    backend.resize_cells(cols, rows);
    assert!(
        !backend.needs_sync(),
        "matching first layout must not queue resize"
    );
}

#[test]
fn startup_layout_resize_waits_until_first_pty_output() {
    let mut backend = Backend::new_deferred(BackendSettings {
        program: "/bin/cat".into(),
        ..Default::default()
    });
    assert!(backend.initialize_deferred(103, 31, 9, 19).unwrap());
    assert_eq!(backend.dimensions(), (103, 31));

    backend.resize_cells(120, 35);
    assert_eq!(backend.dimensions(), (103, 31));
    assert!(!backend.needs_sync());

    backend.events.send_event(Event::Wakeup);
    assert!(backend.sync());
    assert_eq!(backend.dimensions(), (120, 35));
}

#[test]
fn cancel_is_idempotent_joins_reader_and_reaps_hup_ignoring_child() {
    let mut backend = python("import os, signal, time\nsignal.signal(signal.SIGHUP, signal.SIG_IGN)\nos.write(1, b'\x1b]2;' + str(os.getpid()).encode() + b'\x07')\ntime.sleep(60)".into());
    let mut pid = None;
    wait(|| {
        pid = backend
            .take_events()
            .title
            .and_then(|s| s.parse::<i32>().ok())
            .or(pid);
        pid.is_some()
    });
    let term = Arc::downgrade(&backend.term);
    let started = std::time::Instant::now();
    backend.cancel();
    backend.cancel();
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(backend.state(), SessionState::Cancelled);
    assert!(!backend.input(b"ignored".to_vec()));
    assert!(backend.session.is_none(), "reader has been joined");
    assert_reaped(pid.unwrap());
    drop(backend);
    assert!(term.upgrade().is_none(), "worker retained the terminal");
}

pub(super) fn assert_reaped(pid: i32) {
    // Only inspect the child PID reported by our own fixture.
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

#[test]
fn child_exit_status_persists_after_events_are_consumed_and_ignores_input() {
    let backend = python("import sys\nsys.exit(23)".into());
    wait(|| backend.state() != SessionState::Running);
    assert_eq!(backend.state(), SessionState::Exited(Some(23)));
    backend.take_events();
    assert_eq!(
        backend.status_message().as_deref(),
        Some("Shell exited (status 23)")
    );
    assert!(!backend.input(b"ignored".to_vec()));
    assert!(!backend.paste("ignored"));
}

#[test]
fn blocked_child_input_is_bounded_atomic_and_preserves_reply_reserve() {
    let backend = python(
        "import os, tty, time\ntty.setraw(0)\nos.write(1,b'\x1b]2;READY\x07')\ntime.sleep(60)"
            .into(),
    );
    wait(|| backend.take_events().title.as_deref() == Some("READY"));
    let huge = "x".repeat(crate::session::INPUT_LIMIT * 4);
    let started = std::time::Instant::now();
    assert!(!backend.paste(&huge));
    assert!(started.elapsed() < std::time::Duration::from_millis(100));
    assert_eq!(backend.session.as_ref().unwrap().control.queued_bytes(), 0);
    let term = backend.term.lock_unfair();
    let started = std::time::Instant::now();
    let mut accepted = 0;
    while backend.input(vec![b'x'; 4096]) {
        accepted += 1;
        assert!(accepted < 1024, "input is unbounded");
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    let control = &backend.session.as_ref().unwrap().control;
    assert!(control.queued_bytes() <= crate::session::INPUT_LIMIT);
    assert!(backend.status_message().unwrap().contains("not sent"));
    backend
        .events
        .send_event(Event::PtyWrite("\x1b[3;5R".into()));
    assert_eq!(backend.state(), SessionState::Running);
    drop(term);
}

#[test]
fn protocol_cpr_da_osc_title_and_bell_survive_without_ui_delivery() {
    let backend = python(query_script(
        b"\x1b[3;5H\x1b[6n\x1b[c\x1b]2;intermediate\x1b\\\x07",
        b"\x1b[3;5R\x1b[?6c",
    ));
    let mut title = None;
    let mut bell = false;
    wait(|| {
        let pending = backend.take_events();
        title = pending.title.or(title.take());
        bell |= pending.bell;
        title.as_deref() == Some("OK")
    });
    assert!(bell);
}

#[test]
fn flooding_session_cannot_starve_its_input_or_a_second_session() {
    // The flooder also reads a sentinel: fairness must cover duplex I/O, not
    // merely scheduling an unrelated reader thread.
    let flood = python(
        r#"import os, tty, select
 tty.setraw(0)
 os.write(1, b'\x1b]2;READY\x07')
 while True:
     os.write(1, b'flood\r\n' * 1024)
     if select.select([0], [], [], 0)[0] and b'!' in os.read(0, 4096):
         os.write(1, b'\x1b]2;FLOOD_ACK\x07')
         break
"#
        .replace("\n ", "\n"),
    );
    let peer = python(query_script(b"\x1b]2;READY\x07", b"peer-input"));
    let mut flood_ready = false;
    let mut peer_ready = false;
    wait(|| {
        flood_ready |= flood.take_events().title.as_deref() == Some("READY");
        peer_ready |= peer.take_events().title.as_deref() == Some("READY");
        flood_ready && peer_ready
    });
    let started = std::time::Instant::now();
    assert!(peer.input(b"peer-input".to_vec()));
    assert!(flood.input(b"!".to_vec()));
    let mut peer_ack = false;
    let mut flood_ack = false;
    wait(|| {
        peer_ack |= peer.take_events().title.as_deref() == Some("OK");
        flood_ack |= flood.take_events().title.as_deref() == Some("FLOOD_ACK");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "flood starved duplex I/O"
        );
        peer_ack && flood_ack
    });
}
