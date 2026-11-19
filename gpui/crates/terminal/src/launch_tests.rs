use crate::{SessionState, TerminalLaunch, test_support::wait};
use std::{collections::HashMap, path::PathBuf};

#[test]
fn launch_real_pty_cwd_env_set_remove_program_args_and_size() {
    let dir = tempfile::tempdir().unwrap();
    let launch = TerminalLaunch {
        cwd: Some(dir.path().into()),
        program: "/usr/bin/python3".into(),
        args: vec![
            "-u".into(),
            "-c".into(),
            r#"import os,sys
assert os.getcwd() == sys.argv[1], (os.getcwd(),sys.argv[1])
assert os.environ['TRU291_SET'] == 'value with spaces; $literal'
assert 'HOME' not in os.environ
assert 'PATH' not in os.environ
assert 'PWD' not in os.environ
assert os.environ['SHLVL'] == '23'
assert os.get_terminal_size() == (91, 33)
print('LAUNCH_CONTRACT_OK', flush=True)
"#
            .into(),
            dir.path().canonicalize().unwrap().to_string_lossy().into(),
        ],
        env: HashMap::from([
            (
                "TRU291_SET".into(),
                Some("value with spaces; $literal".into()),
            ),
            ("HOME".into(), None),
            ("PATH".into(), None),
            ("PWD".into(), None),
            ("SHLVL".into(), Some("23".into())),
        ]),
        cols: 91,
        rows: 33,
        startup_command: None,
    };
    let backend = launch.start();
    wait(|| {
        backend
            .get_all_text()
            .is_some_and(|s| s.contains("LAUNCH_CONTRACT_OK"))
    });
    assert!(!matches!(backend.state(), SessionState::Error(_)));
}

#[test]
fn launch_invalid_cwd_and_program_are_visible_errors_without_a_child() {
    for launch in [
        TerminalLaunch {
            cwd: Some(PathBuf::from("/nonexistent-tru291-directory")),
            ..Default::default()
        },
        TerminalLaunch {
            program: "/nonexistent-tru291-program".into(),
            ..Default::default()
        },
        TerminalLaunch {
            program: "nonexistent-tru291-program".into(),
            ..Default::default()
        },
    ] {
        let backend = launch.start();
        assert!(matches!(backend.state(), SessionState::Error(_)));
        assert!(
            backend
                .state()
                .message()
                .unwrap()
                .contains("launch terminal")
        );
    }
}

#[test]
fn launch_real_zsh_types_startup_once_after_prompt_readiness() {
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("startup-count");
    std::fs::write(
        home.path().join(".zshrc"),
        "precmd() { (( ++_prompt_count )); print -r -- PROMPT_$_prompt_count; }\n",
    )
    .unwrap();
    let backend = TerminalLaunch {
        cwd: Some(home.path().into()),
        env: HashMap::from([
            ("HOME".into(), Some(home.path().to_string_lossy().into())),
            ("ZDOTDIR".into(), Some(home.path().to_string_lossy().into())),
        ]),
        program: "/bin/zsh".into(),
        args: vec!["-d".into(), "-l".into()],
        startup_command: Some(format!(
            "printf 'once\\n' >> '{}'; printf '\\033]2;STARTUP_READY\\007'",
            marker.display()
        )),
        ..Default::default()
    }
    .start();
    let deadline = std::time::Instant::now();
    loop {
        if backend.take_events().title.as_deref() == Some("STARTUP_READY") {
            break;
        }
        assert!(
            deadline.elapsed().as_secs() < 5,
            "startup failed: {:?} {:?}",
            backend.state(),
            backend.get_all_text()
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "once\n");
    wait(|| {
        backend
            .get_all_text()
            .is_some_and(|s| s.contains("PROMPT_2"))
    });
    assert!(backend.write(b"\r".to_vec()));
    wait(|| {
        backend
            .get_all_text()
            .is_some_and(|s| s.contains("PROMPT_3"))
    });
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "once\n");
}

#[test]
fn launch_real_pty_osc7_cwd_and_title_reports_survive_split_reads() {
    let backend = TerminalLaunch {
        program: "/usr/bin/python3".into(),
        args: vec!["-u".into(), "-c".into(), "import os,time\nfor b in b'\\x1b]7;file://localhost/tmp/a%20b\\x1b\\\\\\x1b]2;REPORTED_TITLE\\x07':\n os.write(1,bytes([b])); time.sleep(.001)".into()],
        ..Default::default()
    }.start();
    let mut cwd = None;
    let mut title = None;
    wait(|| {
        let events = backend.take_events();
        cwd = events.cwd.or(cwd.take());
        title = events.title.or(title.take());
        cwd.is_some() && title.is_some()
    });
    assert_eq!(cwd, Some(PathBuf::from("/tmp/a b")));
    assert_eq!(title.as_deref(), Some("REPORTED_TITLE"));
}

#[test]
fn launch_real_bash_startup_after_first_prompt() {
    let home = tempfile::tempdir().unwrap();
    let backend = TerminalLaunch {
        program: "/bin/bash".into(),
        args: vec!["--noprofile".into(), "--norc".into(), "-i".into()],
        env: HashMap::from([("HOME".into(), Some(home.path().to_string_lossy().into()))]),
        startup_command: Some("printf '\\033]2;BASH_STARTUP_READY\\007'".into()),
        ..Default::default()
    }
    .start();
    wait(|| backend.take_events().title.as_deref() == Some("BASH_STARTUP_READY"));
}

#[test]
fn launch_zsh_home_overlay_rc_redirection_and_nonlogin_restoration() {
    for login in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let redirected = home.path().join("rc");
        std::fs::create_dir(&redirected).unwrap();
        std::fs::write(
            home.path().join(".zshenv"),
            "[[ ! -v ZDOTDIR ]] || exit 41\nZDOTDIR=$HOME/rc\n",
        )
        .unwrap();
        std::fs::write(
            redirected.join(".zshrc"),
            r#"[[ $ZDOTDIR == $HOME/rc ]] || exit 42
PROMPT='INITIAL_PROMPT> '
"#,
        )
        .unwrap();
        let marker = home.path().join("restored");
        let backend = TerminalLaunch {
            cwd: Some(home.path().into()),
            env: HashMap::from([
                ("HOME".into(), Some(home.path().to_string_lossy().into())),
                ("ZDOTDIR".into(), None),
            ]),
            program: "/bin/zsh".into(),
            args: if login {
                vec!["-d".into(), "-l".into()]
            } else {
                vec!["-d".into(), "-i".into()]
            },
            startup_command: Some(format!(
                "print -r -- \"$ZDOTDIR\" > '{}'; printf '\\033]2;RESTORED\\007'",
                marker.display()
            )),
            ..Default::default()
        }
        .start();
        wait(|| backend.take_events().title.as_deref() == Some("RESTORED"));
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().trim(),
            redirected.to_str().unwrap()
        );
        let text = backend.get_all_text().unwrap();
        assert!(
            text.find("INITIAL_PROMPT>").unwrap() < text.find("print -r").unwrap(),
            "startup preceded prompt: {text}"
        );
    }
}

#[test]
fn launch_zsh_unset_zdotdir_is_restored_after_rc_loading() {
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("unset");
    let backend = TerminalLaunch {
        program: "/bin/zsh".into(),
        args: vec!["-d".into(), "-i".into()],
        env: HashMap::from([
            ("HOME".into(), Some(home.path().to_string_lossy().into())),
            ("ZDOTDIR".into(), None),
        ]),
        startup_command: Some(format!(
            "print -r -- ${{+ZDOTDIR}} > '{}'; printf '\\033]2;UNSET_READY\\007'",
            marker.display()
        )),
        ..Default::default()
    }
    .start();
    wait(|| backend.take_events().title.as_deref() == Some("UNSET_READY"));
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "0\n");
}

#[test]
fn launch_zsh_startup_waits_past_a_login_file_vared_editor() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".zlogin"),
        "answer=''; vared -p 'LOGIN_QUESTION> ' answer\n[[ $answer == yes ]] || exit 43\n",
    )
    .unwrap();
    let marker = home.path().join("started");
    let backend = TerminalLaunch {
        program: "/bin/zsh".into(),
        args: vec!["-d".into(), "-l".into()],
        env: HashMap::from([
            ("HOME".into(), Some(home.path().to_string_lossy().into())),
            ("ZDOTDIR".into(), None),
        ]),
        startup_command: Some(format!(
            "print -r -- once > '{}'; printf '\\033]2;AFTER_LOGIN_EDITOR\\007'",
            marker.display()
        )),
        ..Default::default()
    }
    .start();
    wait(|| {
        backend
            .get_all_text()
            .is_some_and(|s| s.contains("LOGIN_QUESTION>"))
    });
    assert!(!marker.exists(), "startup entered login editor");
    assert!(backend.write(b"yes\r".to_vec()));
    wait(|| backend.take_events().title.as_deref() == Some("AFTER_LOGIN_EDITOR"));
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "once\n");
}
