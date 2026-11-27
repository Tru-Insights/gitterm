//! Frozen launch observations from Iced's production path, before extraction.
//! Each case gets a separate test process: no process-global environment races,
//! inherited credentials, real provider state, or real Claude executable.
use super::*;
use gitterm::harness::claude::ClaudeSession;
use gitterm_core::chat::runtime::RuntimeEvent;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

const CHILD_TEST: &str = "tab::agent::claude_launch_tests::production_launch_child";
const GUIDELINES: &str = "Guidelines: preserve bytes.\r\nUnicode: café 🦀\n";

fn environment(root: &Path, case: &str) -> BTreeMap<OsString, OsString> {
    let mut env: BTreeMap<OsString, OsString> = [
        ("GITTERM_LAUNCH_CASE", case.into()),
        ("GITTERM_LAUNCH_ROOT", root.as_os_str().to_owned()),
        ("GITTERM_V6_CONFIG_DIR", root.join("desktop").into()),
        ("CLAUDE_CONFIG_DIR", root.join("provider").into()),
        ("HOME", root.join("home").into()),
        ("TMPDIR", root.join("tmp").into()),
        // Prevent Python's locale coercion from modifying the observed OS env.
        ("LC_CTYPE", "UTF-8".into()),
        ("PYTHONCOERCECLOCALE", "0".into()),
        // macOS Foundation otherwise inserts the current user's encoding.
        (
            "__CF_USER_TEXT_ENCODING",
            format!("0x{:X}:0x0:0x0", unsafe { libc::geteuid() }).into(),
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value))
    .collect();
    let first = root.join("first");
    let local = root.join("home/.local/bin");
    let cargo = root.join("home/.cargo/bin");
    if case != "absent-path" {
        let paths = if matches!(case, "full" | "path-collisions") {
            // Inherited duplicates survive; only appended install dirs dedup.
            vec![
                first.clone(),
                local.clone(),
                cargo.clone(),
                local,
                "/opt/homebrew/bin".into(),
                "/usr/local/bin".into(),
                first,
            ]
        } else if case == "cargo" {
            vec![cargo]
        } else {
            vec![first, root.join("second")]
        };
        env.insert("PATH".into(), std::env::join_paths(paths).unwrap());
    }
    if case == "absent-home" {
        env.remove(std::ffi::OsStr::new("HOME"));
    }
    if case == "absent-provider" {
        env.remove(std::ffi::OsStr::new("CLAUDE_CONFIG_DIR"));
    }
    if matches!(case, "full" | "sanitized") {
        for (key, value) in [
            ("CLAUDECODE", "parent"),
            ("CLAUDE_CODE_SESSION_ID", "parent"),
            ("CLAUDE_CODE_OTHER", "parent"),
            ("CLAUDE_PID", "123"),
            ("NODE_OPTIONS", "--parent-only"),
            ("CLAUDE_CODE_ENTRYPOINT", "parent"),
            ("CLAUDE_CODE_SDK_READS_SESSION_STATE", "parent"),
            ("COLLISION", "inherited"),
            ("UNRELATED", "preserved"),
        ] {
            env.insert(key.into(), value.into());
        }
    }
    if case == "full" {
        env.insert(
            "GITTERM_V6_CLAUDE_WIRE_LOG_DIR".into(),
            root.join("wire").into(),
        );
        env.insert("NON_UTF8".into(), OsString::from_vec(vec![b'x', 0xff]));
    }
    env
}

#[test]
fn production_launch_characterization() {
    // /usr/bin/python3 is an xcrun shim on macOS: it rewrites the env and
    // drops non-UTF8 values. Use the interpreter itself, in isolated mode.
    let python = std::process::Command::new("python3")
        .args(["-I", "-S", "-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(python.status.success());
    let python = String::from_utf8(python.stdout).unwrap();
    let python = python.trim();
    assert!(Path::new(python).is_absolute());
    for case in [
        "full",
        "sanitized",
        "path-collisions",
        "default",
        "empty-model",
        "absent-path",
        "absent-home",
        "absent-provider",
        "cargo",
        "content",
        "spawn-failure",
    ] {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        for dir in [
            "first",
            "second",
            "home/.local/bin",
            "home/.cargo/bin",
            "cwd",
            "tmp",
            "provider",
            "desktop",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for dir in ["first", "second", "home/.local/bin", "home/.cargo/bin"] {
            let program = root.join(dir).join("claude");
            std::fs::write(
                &program,
                include_str!("../../tests/fixtures/claude/capture-launch.py").replacen(
                    "#!/usr/bin/python3",
                    &format!("#!{python} -I"),
                    1,
                ),
            )
            .unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        if case == "spawn-failure" {
            // Resolution accepts the first file, even if a later one is executable.
            std::fs::set_permissions(
                root.join("first/claude"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        std::fs::write(root.join("guidelines.md"), GUIDELINES).unwrap();
        let output = std::fs::File::create(root.join("test-output")).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD_TEST, "--nocapture"])
            .env_clear()
            .envs(environment(&root, case))
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("production launch case {case} timed out");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(
            status.success(),
            "case {case}: {}",
            std::fs::read_to_string(root.join("test-output")).unwrap()
        );
    }
}

// The parent invokes only this test in a fresh process with a synthetic env.
#[tokio::test]
async fn production_launch_child() {
    let Ok(case) = std::env::var("GITTERM_LAUNCH_CASE") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("GITTERM_LAUNCH_ROOT").unwrap());
    let full = case == "full";
    let mut tab = AgentSession::new(AgentBackendConfig::Claude {
        model: if full {
            "  test-model  "
        } else if case == "empty-model" {
            "  "
        } else {
            "default"
        }
        .into(),
        permission_mode: full.then(|| "acceptEdits".into()),
        effort: full.then(|| "high".into()),
    });
    tab.bind_session(full.then(|| "resume-session".into()));
    let servers = if full {
        ["task", "browser", "last"]
            .into_iter()
            .map(|name| ClaudeMcpServer {
                config: json!({"mcpServers": {name: {"url": format!("http://127.0.0.1/{name}")}}}),
                allowed_tools: if name == "last" {
                    vec![]
                } else {
                    vec![format!("mcp__{name}__one"), format!("mcp__{name}__two")]
                },
                env: {
                    let mut env = vec![
                        ("COLLISION".into(), name.into()),
                        (format!("ONLY_{}", name.to_uppercase()), name.into()),
                    ];
                    if name == "last" {
                        // MCP overrides run after PATH, sanitization and SDK vars.
                        env.extend([
                            ("PATH".into(), "/mcp/final/path".into()),
                            ("NODE_OPTIONS".into(), "mcp-options".into()),
                            ("CLAUDECODE".into(), "mcp-parent".into()),
                            ("CLAUDE_CODE_ENTRYPOINT".into(), "mcp-entrypoint".into()),
                            (
                                "CLAUDE_CODE_SDK_READS_SESSION_STATE".into(),
                                "mcp-state".into(),
                            ),
                        ]);
                    }
                    env
                },
                public_env: vec![],
            })
            .collect()
    } else {
        vec![]
    };
    let guidelines = (full || case == "content").then(|| root.join("guidelines.md"));
    let mut config = tab
        .claude_session_config(root.join("cwd"), servers, full, guidelines)
        .unwrap();
    assert_eq!(
        config.snapshot_facts_dir,
        Some(gitterm::chat_facts::facts_dir(&root.join("desktop")))
    );
    assert_eq!(config.wire_log_dir, full.then(|| root.join("wire")));
    if case == "content" {
        // Content wins over the supplied file; core creates and removes its file.
        config.guidelines_content = Some("Content guidelines.\r\n雪\n".into());
    }
    let (session, mut events) = ClaudeSession::spawn_runtime(config);
    let mut errors = vec![];
    let exit = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = events.recv().await {
            match event {
                RuntimeEvent::Harness(HarnessEvent::Error(error)) => errors.push(error),
                RuntimeEvent::Harness(HarnessEvent::ProcessExited { code }) => return code,
                _ => {}
            }
        }
        panic!("session closed without ProcessExited");
    })
    .await
    .expect("production session timed out");
    if case == "spawn-failure" {
        assert_eq!(exit, None);
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].starts_with(&format!(
                "failed to start {} in {}:",
                root.join("first/claude").display(),
                root.join("cwd").display()
            )),
            "{}",
            errors[0]
        );
        assert_eq!(session.local_pid(), None);
        assert!(!root.join("capture.json").exists());
        return;
    }
    assert_eq!(exit, Some(0));
    assert!(errors.is_empty(), "{errors:?}");
    let capture: Value =
        serde_json::from_slice(&std::fs::read(root.join("capture.json")).unwrap()).unwrap();
    let program_dir = match case.as_str() {
        "absent-path" => "home/.local/bin",
        "cargo" => "home/.cargo/bin",
        _ => "first",
    };
    let mut argv = vec![root.join(program_dir).join("claude").into_os_string()];
    argv.extend(
        [
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--permission-prompt-tool",
            "stdio",
            "--include-partial-messages",
        ]
        .map(OsString::from),
    );
    if full {
        argv.extend(
            [
                "--chrome",
                "--model",
                "test-model",
                "--permission-mode",
                "acceptEdits",
                "--effort",
                "high",
                "--append-system-prompt-file",
            ]
            .map(OsString::from),
        );
        argv.push(root.join("guidelines.md").into());
        for (name, tools) in [("task", true), ("browser", true), ("last", false)] {
            argv.push(format!("--mcp-config={{\"mcpServers\":{{\"{name}\":{{\"url\":\"http://127.0.0.1/{name}\"}}}}}}").into());
            if tools {
                argv.push(format!("--allowedTools=mcp__{name}__one,mcp__{name}__two").into());
            }
        }
        argv.push("--resume=resume-session".into());
    } else {
        argv.extend(["--no-chrome", "--permission-mode", "default"].map(OsString::from));
        if case == "content" {
            let path = PathBuf::from(capture["guidelines_path"].as_str().unwrap());
            assert_eq!(path.parent(), Some(root.join("tmp").as_path()));
            let filename = path.file_name().unwrap().to_str().unwrap();
            let id = filename
                .strip_prefix("gitterm-v6-remote-guidelines-")
                .unwrap()
                .strip_suffix(".md")
                .unwrap();
            uuid::Uuid::parse_str(id).unwrap();
            argv.push("--append-system-prompt-file".into());
            argv.push(path.into());
        }
    }
    assert_eq!(
        capture["argv"],
        json!(argv.iter().map(|arg| arg.as_bytes()).collect::<Vec<_>>())
    );
    assert_eq!(
        capture["cwd"],
        json!(root.join("cwd").as_os_str().as_bytes())
    );
    let expected_guidelines = if full {
        GUIDELINES
    } else if case == "content" {
        "Content guidelines.\r\n雪\n"
    } else {
        ""
    };
    assert_eq!(capture["guidelines"], json!(expected_guidelines.as_bytes()));
    let mut expected = environment(&root, &case);
    for key in [
        "CLAUDECODE",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_OTHER",
        "CLAUDE_PID",
        "NODE_OPTIONS",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SDK_READS_SESSION_STATE",
    ] {
        expected.remove(std::ffi::OsStr::new(key));
    }
    let first = root.join("first");
    let local = root.join("home/.local/bin");
    let cargo = root.join("home/.cargo/bin");
    let paths: Vec<PathBuf> = match case.as_str() {
        "full" | "path-collisions" => vec![
            first.clone(),
            local.clone(),
            cargo.clone(),
            local,
            "/opt/homebrew/bin".into(),
            "/usr/local/bin".into(),
            first,
        ],
        "absent-path" => vec![
            local,
            cargo,
            "/opt/homebrew/bin".into(),
            "/usr/local/bin".into(),
        ],
        "absent-home" => vec![
            first,
            root.join("second"),
            "/opt/homebrew/bin".into(),
            "/usr/local/bin".into(),
        ],
        "cargo" => vec![
            cargo,
            local,
            "/opt/homebrew/bin".into(),
            "/usr/local/bin".into(),
        ],
        _ => vec![
            first,
            root.join("second"),
            local,
            cargo,
            "/opt/homebrew/bin".into(),
            "/usr/local/bin".into(),
        ],
    };
    expected.insert("PATH".into(), std::env::join_paths(paths).unwrap());
    expected.insert("CLAUDE_CODE_ENTRYPOINT".into(), "sdk-ts".into());
    expected.insert("CLAUDE_CODE_SDK_READS_SESSION_STATE".into(), "1".into());
    if full {
        for (key, value) in [
            ("COLLISION", "last"),
            ("ONLY_TASK", "task"),
            ("ONLY_BROWSER", "browser"),
            ("ONLY_LAST", "last"),
            ("PATH", "/mcp/final/path"),
            ("NODE_OPTIONS", "mcp-options"),
            ("CLAUDECODE", "mcp-parent"),
            ("CLAUDE_CODE_ENTRYPOINT", "mcp-entrypoint"),
            ("CLAUDE_CODE_SDK_READS_SESSION_STATE", "mcp-state"),
        ] {
            expected.insert(key.into(), value.into());
        }
    }
    let bytes: BTreeMap<_, _> = expected
        .iter()
        .map(|(key, value)| (key.to_str().unwrap(), value.as_bytes()))
        .collect();
    assert_eq!(capture["environment"], json!(bytes));
    if full {
        let pid = capture["pid"].as_u64().unwrap();
        for stream in ["stdin", "stdout"] {
            assert!(
                !std::fs::read(root.join(format!("wire/claude-{pid}-{stream}.jsonl")))
                    .unwrap()
                    .is_empty()
            );
        }
    } else {
        assert!(!root.join("wire").exists());
    }
    // Channel closure follows guideline RAII cleanup and session runtime teardown.
    assert!(tokio::time::timeout(Duration::from_secs(5), async {
        while events.recv().await.is_some() {}
    })
    .await
    .is_ok());
    if case == "content" {
        assert_eq!(std::fs::read_dir(root.join("tmp")).unwrap().count(), 0);
    }
}
