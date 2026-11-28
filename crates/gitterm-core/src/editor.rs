use std::path::{Path, PathBuf};
#[cfg(feature = "runtime")]
use std::time::Duration;

use crate::content::source::SourcePath;

#[cfg(feature = "runtime")]
const EDITOR_EXIT_REPORT_WINDOW: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCommand {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    pub name: String,
    pub command: OpenCommand,
}

pub fn build_open_command(
    setting: &str,
    path: &Path,
    line: Option<usize>,
) -> Result<Editor, String> {
    let path = absolute_path(path)?;
    let path_str = path.to_string_lossy();
    let (name, bundle, scheme) = match setting {
        "vscode" => ("VS Code", Some("com.microsoft.VSCode"), Some("vscode")),
        "cursor" => (
            "Cursor",
            Some("com.todesktop.230313mzl4w4u92"),
            Some("cursor"),
        ),
        "zed" => ("Zed", Some("dev.zed.Zed"), None),
        custom => {
            let args = split_command(custom)?
                .into_iter()
                .map(|arg| substitute_template(&arg, &path_str, line))
                .collect::<Vec<_>>();
            let Some(program) = args.first().cloned() else {
                return Err("Custom editor command is empty".into());
            };
            return Ok(Editor {
                name: PathBuf::from(&program)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(&program)
                    .to_string(),
                command: OpenCommand {
                    program,
                    args: args[1..].to_vec(),
                },
            });
        }
    };
    let args = match (scheme, line) {
        (Some(scheme), Some(line)) => {
            vec![format!("{scheme}://file/{}:{line}", encode_path(&path_str))]
        }
        _ => {
            let mut args = Vec::new();
            if let Some(bundle) = bundle {
                args.extend(["-b".to_string(), bundle.to_string()]);
            }
            args.push(path_str.to_string());
            args
        }
    };
    Ok(Editor {
        name: name.to_string(),
        command: OpenCommand {
            program: "open".into(),
            args,
        },
    })
}

/// Launch the editor without tying application shutdown to its lifetime.
#[cfg(feature = "runtime")]
pub async fn launch_command(command: OpenCommand) -> Result<(), String> {
    let mut child = tokio::process::Command::new(&command.program)
        .args(&command.args)
        .spawn()
        .map_err(|error| format!("{}: {error}", command.program))?;
    let program = command.program;
    let monitor_program = program.clone();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let program = monitor_program;
        match tokio::time::timeout(EDITOR_EXIT_REPORT_WINDOW, child.wait()).await {
            Ok(Ok(status)) if !status.success() => {
                let _ = result_tx.send(Err(format!("{program:?} exited with {status}")));
                return;
            }
            Ok(Err(error)) => {
                let _ = result_tx.send(Err(format!("failed to wait for {program:?}: {error}")));
                return;
            }
            Ok(Ok(_)) => {
                let _ = result_tx.send(Ok(()));
                return;
            }
            Err(_) => {
                // Once the reporting window expires, leave the app-facing
                // operation successful and keep reaping independently.
                let _ = result_tx.send(Ok(()));
            }
        }
        if let Err(error) = child.wait().await {
            eprintln!("[editor] failed to reap command {program:?}: {error}");
        }
    });
    result_rx
        .await
        .map_err(|_| format!("editor launch monitor for {program:?} stopped"))?
}

fn substitute_template(template: &str, path: &str, line: Option<usize>) -> String {
    let mut result = String::new();
    let mut rest = template;
    while !rest.is_empty() {
        if let Some(next) = rest.strip_prefix("{path}") {
            result.push_str(path);
            rest = next;
        } else if let Some(next) = rest.strip_prefix("{line}") {
            if let Some(line) = line {
                result.push_str(&line.to_string());
            }
            rest = next;
        } else {
            let ch = rest.chars().next().expect("rest is not empty");
            result.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    result
}

pub fn not_installed_error(name: &str) -> String {
    format!("{name} isn't installed. Choose an editor in Settings")
}

pub fn display_name(setting: &str) -> String {
    match setting {
        "vscode" => "VS Code".into(),
        "cursor" => "Cursor".into(),
        "zed" => "Zed".into(),
        custom => custom
            .split_whitespace()
            .next()
            .and_then(|word| Path::new(word).file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("editor")
            .to_string(),
    }
}

pub fn known_checkout_path(
    known: &[PathBuf],
    current_checkout: &Path,
    requested: &Path,
    local_source: bool,
) -> Option<PathBuf> {
    if !local_source {
        return None;
    }
    if current_checkout == requested {
        return Some(current_checkout.to_path_buf());
    }
    known
        .iter()
        .find(|path| path.as_path() == requested)
        .cloned()
}

pub fn local_editor_path(source: &SourcePath) -> Option<&Path> {
    source.as_local()
}

pub fn viewer_header_title(path: &str, error: Option<&str>) -> String {
    const MAX_ERROR_CHARS: usize = 32;
    let title = error.unwrap_or(path);
    if error.is_some() && title.chars().count() > MAX_ERROR_CHARS {
        let truncated = title.chars().take(MAX_ERROR_CHARS - 1).collect::<String>();
        format!("{truncated}…")
    } else {
        title.to_string()
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| format!("cannot make {} absolute: {error}", path.display()))
    }
}

fn encode_path(path: &str) -> String {
    path.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Split a command line into argv without invoking a shell. Supports single
/// and double quotes and backslash escapes, but no expansion or shell syntax.
fn split_command(command: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut arg = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for ch in command.chars() {
        if escaped {
            arg.push(ch);
            escaped = false;
            started = true;
        } else if ch == '\\' && quote != Some('\'') {
            escaped = true;
            started = true;
        } else if Some(ch) == quote {
            quote = None;
            started = true;
        } else if quote.is_none() && (ch == '\'' || ch == '"') {
            quote = Some(ch);
            started = true;
        } else if quote.is_none() && ch.is_whitespace() {
            if started {
                args.push(std::mem::take(&mut arg));
                started = false;
            }
        } else {
            arg.push(ch);
            started = true;
        }
    }
    if escaped {
        return Err("Custom editor command ends with an escape".into());
    }
    if quote.is_some() {
        return Err("Custom editor command has an unmatched quote".into());
    }
    if started {
        args.push(arg);
    }
    Ok(args)
}

/// Iced's terminal-pencil route is separate from the configured GUI editor.
/// The editor setting is a trusted shell command; the path is not (it can come
/// from any cloned repo), so it is single-quoted and no shell expansion runs on
/// it (decision D14).
pub fn terminal_command(editor: Option<&str>, path: &Path) -> String {
    let path = path.display().to_string().replace('\'', "'\\''");
    format!("{} '{}'", editor.unwrap_or("vim"), path)
}

pub fn open_error(setting: &str, error: &str) -> String {
    let name = display_name(setting);
    if matches!(setting, "vscode" | "cursor" | "zed") {
        not_installed_error(&name)
    } else {
        format!("Could not open in {name}: {error}")
    }
}

#[cfg(test)]
#[path = "editor_before_tests.rs"]
mod before;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iced_before_and_core_commands_are_byte_identical() {
        for path in [
            "/tmp/a b.rs",
            "/tmp/a'\"$`\\{line}.rs",
            "relative a.rs",
            "/tmp/folder",
        ] {
            for setting in [
                "vscode",
                "cursor",
                "zed",
                "fake-editor --goto '{path}:{line}' --flag",
                "'fake editor' \"{path}\" '' {line}",
                "",
                "editor 'broken",
                "editor \\",
            ] {
                for line in [None, Some(17)] {
                    let old = before::build_open_command(setting, Path::new(path), line);
                    let new = build_open_command(setting, Path::new(path), line);
                    match (old, new) {
                        (Ok(old), Ok(new)) => {
                            assert_eq!(old.name.as_bytes(), new.name.as_bytes());
                            assert_eq!(
                                old.command.program.as_bytes(),
                                new.command.program.as_bytes()
                            );
                            assert_eq!(old.command.args, new.command.args);
                        }
                        (Err(old), Err(new)) => assert_eq!(old, new),
                        other => panic!("changed command: {other:?}"),
                    }
                }
            }
        }
        assert_eq!(
            terminal_command(None, Path::new("/tmp/a b")),
            "vim '/tmp/a b'"
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminal_command_never_expands_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        for name in [
            format!("a$(touch {}).rs", marker.display()),
            format!("a`touch {}`.rs", marker.display()),
            format!("a\"; touch {}; \".rs", marker.display()),
            format!("a'; touch {}; '.rs", marker.display()),
        ] {
            // `printf %s` prints its argument verbatim, so the output must be the
            // literal filename and the marker must never be created.
            let command = terminal_command(Some("printf %s"), Path::new(&name));
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", &command])
                .output()
                .unwrap();
            assert!(output.status.success(), "{command}");
            assert_eq!(String::from_utf8(output.stdout).unwrap(), name);
            assert!(!marker.exists(), "path was expanded: {command}");
        }
    }

    #[cfg(all(unix, feature = "runtime"))]
    #[test]
    fn fake_editor_on_path_records_argv_for_both_routes() {
        use std::os::unix::fs::PermissionsExt;
        const PROBE: &str = "GITTERM_EDITOR_ARGV_FIXTURE";
        let Ok(root) = std::env::var(PROBE) else {
            let root = tempfile::tempdir().unwrap();
            for program in ["fake-editor", "open"] {
                let script = root.path().join(program);
                std::fs::write(
                    &script,
                    "#!/bin/sh\nprintf '%s\\0' \"$@\" > \"$GITTERM_EDITOR_ARGV_FIXTURE/argv.tmp\"\n/bin/mv \"$GITTERM_EDITOR_ARGV_FIXTURE/argv.tmp\" \"$GITTERM_EDITOR_ARGV_FIXTURE/argv\"\n",
                )
                .unwrap();
                std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "editor::tests::fake_editor_on_path_records_argv_for_both_routes",
                    "--test-threads=1",
                ])
                .env(PROBE, root.path())
                .env("PATH", root.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        };
        let root = Path::new(&root);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for path in [
            "/tmp/space and 'single' and \"double\".rs",
            "/tmp/{line} $`\\.rs",
        ] {
            for setting in [
                "fake-editor --goto '{path}:{line}' ''",
                "vscode",
                "cursor",
                "zed",
            ] {
                for line in [None, Some(23)] {
                    let command = build_open_command(setting, Path::new(path), line)
                        .unwrap()
                        .command;
                    let record = root.join("argv");
                    if record.exists() {
                        std::fs::remove_file(&record).unwrap();
                    }
                    runtime.block_on(launch_command(command.clone())).unwrap();
                    // Launch success deliberately precedes a slow child exiting.
                    let deadline = std::time::Instant::now() + Duration::from_secs(30);
                    while !record.exists() {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "fake editor did not record argv"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let bytes = std::fs::read(record).unwrap();
                    let expected: Vec<u8> = command
                        .args
                        .iter()
                        .flat_map(|arg| arg.bytes().chain([0]))
                        .collect();
                    assert_eq!(bytes, expected);
                }
            }
        }
        let command = terminal_command(
            Some("fake-editor --terminal"),
            Path::new("/tmp/a space and 'quote'.rs"),
        );
        assert!(std::process::Command::new("/bin/sh")
            .args(["-c", &command])
            .status()
            .unwrap()
            .success());
        assert_eq!(
            std::fs::read(root.join("argv")).unwrap(),
            b"--terminal\0/tmp/a space and 'quote'.rs\0"
        );
        let missing = runtime
            .block_on(launch_command(OpenCommand {
                program: "missing-editor".into(),
                args: vec![],
            }))
            .unwrap_err();
        assert_eq!(
            open_error("vscode", &missing),
            "VS Code isn't installed. Choose an editor in Settings"
        );
        assert_eq!(
            open_error("fake-editor {path}", &missing),
            format!("Could not open in fake-editor: {missing}")
        );
    }

    #[test]
    fn builds_macos_commands_for_each_editor_with_and_without_line() {
        let path = Path::new("/tmp/project/file with spaces.rs");
        for (setting, bundle, scheme) in [
            ("vscode", "com.microsoft.VSCode", Some("vscode")),
            ("cursor", "com.todesktop.230313mzl4w4u92", Some("cursor")),
            ("zed", "dev.zed.Zed", None),
        ] {
            let editor = build_open_command(setting, path, None).unwrap();
            assert_eq!(editor.command.program, "open");
            assert_eq!(editor.command.args, ["-b", bundle, path.to_str().unwrap()]);
            let editor = build_open_command(setting, path, Some(17)).unwrap();
            if let Some(scheme) = scheme {
                assert_eq!(
                    editor.command.args,
                    [format!(
                        "{scheme}://file//tmp/project/file%20with%20spaces.rs:17"
                    )]
                );
            } else {
                assert_eq!(editor.command.args, ["-b", bundle, path.to_str().unwrap()]);
            }
        }
    }

    #[test]
    fn custom_editor_substitutes_and_splits_without_shell() {
        let editor = build_open_command(
            "my-editor --goto '{path}:{line}' --flag",
            Path::new("/tmp/a b.rs"),
            Some(9),
        )
        .unwrap();
        assert_eq!(editor.command.program, "my-editor");
        assert_eq!(editor.command.args, ["--goto", "/tmp/a b.rs:9", "--flag"]);
        let no_line =
            build_open_command("editor {path}:{line}", Path::new("/tmp/a"), None).unwrap();
        assert_eq!(no_line.command.args, ["/tmp/a:"]);
        assert!(build_open_command("editor 'broken", Path::new("/tmp/a"), None).is_err());
        let braces =
            build_open_command("editor {path} {line}", Path::new("/tmp/{line}"), Some(4)).unwrap();
        assert_eq!(braces.command.args, ["/tmp/{line}", "4"]);
    }

    #[test]
    fn reports_not_installed_text() {
        assert_eq!(
            not_installed_error("VS Code"),
            "VS Code isn't installed. Choose an editor in Settings"
        );
    }

    #[test]
    fn refuses_unknown_checkout_paths() {
        let known = vec![PathBuf::from("/tmp/work")];
        assert_eq!(
            known_checkout_path(
                &known,
                Path::new("/tmp/current"),
                Path::new("/tmp/work"),
                true,
            ),
            Some(known[0].clone())
        );
        assert_eq!(
            known_checkout_path(
                &known,
                Path::new("/tmp/current"),
                Path::new("/tmp/other"),
                true,
            ),
            None
        );
    }

    #[test]
    fn allows_existing_chat_checkout_after_workspace_move_but_rejects_remote() {
        let choices = vec![PathBuf::from("/Users/me/Work/fix-213")];
        let current = Path::new("/Users/me/src/repo");
        assert_eq!(
            known_checkout_path(&choices, current, current, true),
            Some(current.to_path_buf())
        );
        assert_eq!(known_checkout_path(&choices, current, current, false), None);
    }

    #[test]
    fn local_editor_path_rejects_remote_display_paths() {
        assert_eq!(
            local_editor_path(&SourcePath::Local(PathBuf::from("/tmp/a.rs"))),
            Some(Path::new("/tmp/a.rs"))
        );
        assert_eq!(
            local_editor_path(&SourcePath::Remote("/tmp/a.rs".into())),
            None
        );
    }

    #[test]
    fn viewer_header_shows_editor_error_in_visible_chrome() {
        assert_eq!(
            viewer_header_title("README.md", Some("VS Code isn't installed")),
            "VS Code isn't installed"
        );
        assert_eq!(viewer_header_title("README.md", None), "README.md");
    }

    #[test]
    fn viewer_header_bounds_long_editor_errors() {
        let error = format!("{}stderr", "/very/long/custom/editor/path/".repeat(20));
        let title = viewer_header_title("README.md", Some(&error));
        assert!(
            title.chars().count() <= 32,
            "{} chars: {title}",
            title.chars().count()
        );
        assert!(title.ends_with('…'));
    }

    #[cfg(all(unix, feature = "runtime"))]
    #[test]
    fn editor_task_does_not_hold_runtime_shutdown_until_process_exit() {
        use std::time::{Duration, Instant};

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        {
            let _guard = runtime.enter();
            runtime.spawn(launch_command(OpenCommand {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "sleep 1".into()],
            }));
        }
        runtime.block_on(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        let started = Instant::now();
        drop(runtime);
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "runtime shutdown waited {:?} for the child process",
            started.elapsed()
        );
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn reports_editor_spawn_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(launch_command(OpenCommand {
                program: "/no/such/editor-command".into(),
                args: Vec::new(),
            }))
            .unwrap_err();
        assert!(error.contains("/no/such/editor-command"), "{error}");
    }

    #[cfg(all(unix, feature = "runtime"))]
    #[test]
    fn reports_quick_nonzero_editor_exit() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(launch_command(OpenCommand {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "exit 7".into()],
            }))
            .unwrap_err();
        assert!(error.contains("exited with"), "{error}");
    }
}
