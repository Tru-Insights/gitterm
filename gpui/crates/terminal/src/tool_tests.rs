//! Real programs on the production launch/PTY/parser path. No OS input or GUI.
use crate::{Backend, Command, MouseButton, SessionState, TerminalLaunch};
use alacritty_terminal::{
    index::{Column, Line, Point},
    term::{TermMode, cell::Flags},
    vte::ansi::{Color, Rgb},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command as Process,
    time::{Duration, Instant},
};

fn tool(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let found = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .chain([PathBuf::from("/usr/bin"), PathBuf::from("/bin")])
        .map(|dir| dir.join(name))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        });
    if found.is_none() {
        eprintln!("SKIP {name}: executable is not installed");
    }
    found
}
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}
fn shell(dir: &Path) -> Backend {
    TerminalLaunch {
        program: "/bin/bash".into(),
        args: vec!["--noprofile".into(), "--norc".into(), "-i".into()],
        cwd: Some(dir.into()),
        env: HashMap::from([
            ("HOME".into(), Some(dir.to_str().unwrap().into())),
            ("PS1".into(), Some("TRU274_PROMPT> ".into())),
            ("PROMPT_COMMAND".into(), None),
            ("BASH_SILENCE_DEPRECATION_WARNING".into(), Some("1".into())),
            ("INPUTRC".into(), Some("/dev/null".into())),
            ("IGNOREEOF".into(), None),
            ("HISTFILE".into(), Some("/dev/null".into())),
            ("TERM".into(), Some("xterm-256color".into())),
            ("LC_ALL".into(), Some("en_US.UTF-8".into())),
            ("GIT_CONFIG_NOSYSTEM".into(), Some("1".into())),
            ("GIT_CONFIG_GLOBAL".into(), Some("/dev/null".into())),
            ("LESS".into(), None),
            ("LESSOPEN".into(), None),
            ("LESSCLOSE".into(), None),
        ]),
        cols: 100,
        rows: 24,
        ..Default::default()
    }
    .start()
}
pub(crate) fn screen(backend: &Backend) -> String {
    let content = backend.renderable_content();
    let cols = backend.dimensions().0;
    content
        .cells
        .chunks(cols)
        .map(|row| row.iter().map(|c| c.cell.c).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
pub(crate) fn poll(backend: &mut Backend, label: &str, mut ready: impl FnMut(&Backend) -> bool) {
    let start = Instant::now();
    loop {
        if backend.sync() && ready(backend) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "{label}: {:?} mode={:?}\n{}",
            backend.state(),
            backend.mode(),
            screen(backend)
        );
        assert!(
            !matches!(backend.state(), SessionState::Error(_)),
            "{label}: {:?}",
            backend.state()
        );
        std::thread::park_timeout(Duration::from_millis(5));
    }
}
pub(crate) fn text(backend: &mut Backend, needle: &str) {
    poll(backend, needle, |b| screen(b).contains(needle));
}
pub(crate) fn send(backend: &Backend, bytes: impl AsRef<[u8]>) {
    assert!(backend.input_bytes(bytes.as_ref()));
}
pub(crate) fn ready_shell(dir: &Path) -> Backend {
    let mut backend = shell(dir);
    text(&mut backend, "TRU274_PROMPT>");
    send(&backend, "stty -echo; printf 'READY_%s\\n' OK\r");
    text(&mut backend, "READY_OK");
    backend
}
fn returned(backend: &mut Backend) {
    poll(backend, "tool returned to primary prompt", |b| {
        !b.mode().contains(TermMode::ALT_SCREEN)
            && screen(b).contains("TOOL_RETURNED")
            && screen(b).contains("TRU274_PROMPT>")
    });
}

#[test]
fn real_tools_vim_edit_paste_mouse_and_alternate_screen() {
    let Some(vim) = tool("vim") else { return };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("edit.txt");
    std::fs::write(&file, "first line\nsecond line\nthird line\n").unwrap();
    let mut backend = ready_shell(dir.path());
    send(
        &backend,
        format!(
            "{} -N -u NONE -i NONE -n --cmd 'set mouse=a ttymouse=sgr ttimeoutlen=100' {}; printf 'TOOL_%s\\n' RETURNED\r",
            quote(&vim),
            quote(&file)
        ),
    );
    poll(&mut backend, "vim screen and SGR mouse", |b| {
        b.mode()
            .contains(TermMode::ALT_SCREEN | TermMode::SGR_MOUSE)
            && b.mode().intersects(TermMode::MOUSE_MODE)
            && screen(b).contains("second line")
    });
    backend.handle(Command::MouseReport(
        MouseButton::LeftButton,
        Default::default(),
        Point::new(Line(1), Column(4)),
        true,
    ));
    backend.handle(Command::MouseReport(
        MouseButton::LeftButton,
        Default::default(),
        Point::new(Line(1), Column(4)),
        false,
    ));
    poll(&mut backend, "vim click moved cursor", |b| {
        b.renderable_content().cursor_point == Point::new(Line(1), Column(4))
    });
    send(&backend, "ggi");
    poll(&mut backend, "vim insert and bracketed paste", |b| {
        b.mode().contains(TermMode::BRACKETED_PASTE) && screen(b).contains("-- INSERT --")
    });
    let pasted = "literal\ttext\n:wq\n$(not-a-command)\n";
    assert!(backend.paste(pasted));
    text(&mut backend, "$(not-a-command)");
    assert!(backend.mode().contains(TermMode::ALT_SCREEN));
    send(&backend, b"\x1b:wq\r");
    returned(&mut backend);
    assert_eq!(
        std::fs::read_to_string(file).unwrap(),
        format!("{pasted}first line\nsecond line\nthird line\n")
    );
    assert!(
        screen(&backend).contains("READY_OK"),
        "primary screen was not restored"
    );
}

#[test]
fn real_tools_less_scroll_and_quit() {
    let Some(less) = tool("less") else { return };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pages.txt");
    std::fs::write(
        &file,
        (0..200)
            .map(|i| format!("PAGER_ROW_{i:03}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let mut backend = ready_shell(dir.path());
    send(
        &backend,
        format!(
            "{} -+F -+X {}; printf 'TOOL_%s\\n' RETURNED\r",
            quote(&less),
            quote(&file)
        ),
    );
    poll(&mut backend, "less first page", |b| {
        b.mode().contains(TermMode::ALT_SCREEN) && screen(b).contains("PAGER_ROW_000")
    });
    send(&backend, b"G");
    text(&mut backend, "PAGER_ROW_199");
    send(&backend, b"g");
    text(&mut backend, "PAGER_ROW_000");
    send(&backend, b"q");
    returned(&mut backend);
}
fn git(git: &Path, dir: &Path, args: &[&str]) {
    let output = Process::new(git)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn real_tools_git_log_and_diff_pagers_scroll_and_quit() {
    let (Some(git_bin), Some(less)) = (tool("git"), tool("less")) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    git(&git_bin, dir.path(), &["init", "-q"]);
    let file = dir.path().join("diff.txt");
    std::fs::write(
        &file,
        (0..180)
            .map(|i| format!("BASE_ROW_{i:03}\n"))
            .collect::<String>(),
    )
    .unwrap();
    git(&git_bin, dir.path(), &["add", "."]);
    git(
        &git_bin,
        dir.path(),
        &[
            "-c",
            "user.name=Terminal Test",
            "-c",
            "user.email=terminal@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "fixture",
            "-m",
            &(0..180)
                .map(|i| format!("LOG_ROW_{i:03}\n"))
                .collect::<String>(),
        ],
    );
    std::fs::write(
        &file,
        (0..180)
            .map(|i| format!("EDIT_ROW_{i:03}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let pager = format!("{} -+F -+X", quote(&less));
    let mut backend = ready_shell(dir.path());
    for (args, first, last) in [
        ("log --no-color", "LOG_ROW_000", "LOG_ROW_179"),
        ("diff --no-color", "diff --git", "EDIT_ROW_179"),
    ] {
        send(
            &backend,
            format!(
                "GIT_PAGER={} {} --paginate {args}; printf 'TOOL_%s\\n' RETURNED\r",
                quote(Path::new(&pager)),
                quote(&git_bin)
            ),
        );
        poll(&mut backend, first, |b| {
            b.mode().contains(TermMode::ALT_SCREEN) && screen(b).contains(first)
        });
        send(&backend, b"G");
        text(&mut backend, last);
        send(&backend, b"g");
        text(&mut backend, first);
        send(&backend, b"q");
        returned(&mut backend);
    }
}

#[test]
fn real_tools_monitor_resize_and_quit() {
    let monitor = tool("htop").or_else(|| tool("top"));
    let Some(monitor) = monitor else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut backend = ready_shell(dir.path());
    let htop = monitor.file_name().unwrap() == "htop";
    let args = if htop {
        "--delay=1"
    } else if cfg!(target_os = "macos") {
        "-s 1 -o pid"
    } else {
        "-d 0.2"
    };
    send(
        &backend,
        format!("{} {args}; printf 'TOOL_%s\\n' RETURNED\r", quote(&monitor)),
    );
    poll(&mut backend, "monitor started", |b| {
        screen(b).contains(if htop {
            "Tasks"
        } else if cfg!(target_os = "macos") {
            "Processes:"
        } else {
            "Tasks:"
        }) || screen(b).contains("Operation not permitted")
    });
    if screen(&backend).contains("Operation not permitted") {
        eprintln!(
            "SKIP {}: OS sandbox denies process inspection",
            monitor.display()
        );
        return;
    }
    backend.resize_cells(72, 18);
    poll(&mut backend, "monitor resize applied", |b| {
        b.dimensions() == (72, 18)
    });
    let revision = backend.revision();
    poll(&mut backend, "monitor redraw after resize", |b| {
        b.revision() > revision
            && b.dimensions() == (72, 18)
            && b.renderable_content().cells.len() == 72 * 18
            && screen(b).contains(if htop { "Tasks" } else { "PID" })
    });
    send(&backend, b"q");
    returned(&mut backend);
}

#[test]
fn real_tools_resize_during_heavy_output_restores_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let mut backend = ready_shell(dir.path());
    send(
        &backend,
        "i=0; while ((i<200000)) || [[ ! -f resize-done ]]; do printf 'FLOOD_%06d\\n' \"$i\"; ((i++)); done; printf 'FLOOD_DONE_%s\\n' \"$i\"\r",
    );
    text(&mut backend, "FLOOD_");
    for (cols, rows) in [(40, 12), (132, 40), (80, 24), (91, 31)] {
        backend.resize_cells(cols, rows);
        poll(&mut backend, "resize applied during output", |b| {
            b.dimensions() == (cols, rows) && b.renderable_content().cells.len() == cols * rows
        });
    }
    std::fs::write(dir.path().join("resize-done"), "done").unwrap();
    text(&mut backend, "FLOOD_DONE");
    text(&mut backend, "TRU274_PROMPT>");
    send(&backend, "printf 'AFTER_%s\\n' RESIZE\r");
    text(&mut backend, "AFTER_RESIZE");
    let final_screen = screen(&backend);
    let count: usize = final_screen
        .split("FLOOD_DONE_")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(count >= 200_000, "only {count} flood rows");
    assert_eq!(backend.state(), SessionState::Running);
}

#[test]
fn real_tools_unicode_and_sgr_reach_renderable_cells() {
    let dir = tempfile::tempdir().unwrap();
    let mut backend = ready_shell(dir.path());
    send(
        &backend,
        "printf '\\033[2J\\033[H界😀é\\033[38;5;196;48;5;22mI\\033[38;2;12;34;56;48;2;65;43;21mT\\033[0m'\r",
    );
    poll(&mut backend, "complete Unicode and SGR sequence", |b| {
        b.renderable_content()
            .cells
            .get(6)
            .is_some_and(|cell| cell.cell.c == 'T')
    });
    let cells = &backend.renderable_content().cells;
    assert_eq!(cells[0].cell.c, '界');
    assert!(cells[1].cell.flags.contains(Flags::WIDE_CHAR_SPACER));
    assert_eq!(cells[2].cell.c, '😀');
    assert!(cells[3].cell.flags.contains(Flags::WIDE_CHAR_SPACER));
    assert_eq!(cells[4].cell.c, 'e');
    assert_eq!(cells[4].cell.zerowidth(), Some(['\u{301}'].as_slice()));
    assert_eq!(
        (cells[5].cell.fg, cells[5].cell.bg),
        (Color::Indexed(196), Color::Indexed(22))
    );
    assert_eq!(
        (cells[6].cell.fg, cells[6].cell.bg),
        (
            Color::Spec(Rgb {
                r: 12,
                g: 34,
                b: 56
            }),
            Color::Spec(Rgb {
                r: 65,
                g: 43,
                b: 21
            })
        )
    );
}

#[test]
fn real_tools_osc8_wraps_wide_cells_and_prefers_explicit_target() {
    let dir = tempfile::tempdir().unwrap();
    let mut backend = ready_shell(dir.path());
    send(
        &backend,
        "printf '\\033[2J\\033[H\\033[99G\\033]8;id=wide;https://example.invalid/explicit\\007界😀https://label.invalid\\033]8;;\\007'\r",
    );
    text(&mut backend, "https://label.invalid");
    let start = Point::new(Line(0), Column(98));
    let end = Point::new(Line(1), Column(22));
    for point in [
        start,
        Point::new(Line(0), Column(99)),
        Point::new(Line(1), Column(1)),
        end,
    ] {
        poll(&mut backend, "explicit hyperlink ready", |_| true);
        crate::test_support::wait(|| {
            matches!(
                backend.handle(Command::ProcessLink(crate::LinkAction::Open, point)),
                crate::Action::OpenUrl(url) if url == "https://example.invalid/explicit"
            )
        });
        assert_eq!(
            backend.renderable_content().hovered_hyperlink,
            Some(start..=end)
        );
    }
    for point in [
        Point::new(Line(1), Column(100)),
        Point::new(Line(100), Column(0)),
        Point::new(Line(-1), Column(0)),
    ] {
        assert!(matches!(
            backend.handle(Command::ProcessLink(crate::LinkAction::Open, point)),
            crate::Action::Ignore
        ));
        assert!(backend.renderable_content().hovered_hyperlink.is_none());
    }
}
