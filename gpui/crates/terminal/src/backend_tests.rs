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
