use std::path::{Path, PathBuf};

use crate::source::SourcePath;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenCommand {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Editor {
    pub(crate) name: String,
    pub(crate) command: OpenCommand,
}

pub(crate) fn build_open_command(
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

/// Run an editor command to completion so a launcher that exits unsuccessfully
/// is reported and its child process is reaped.
pub(crate) fn run_command(command: OpenCommand) -> Result<(), String> {
    let result = std::process::Command::new(&command.program)
        .args(&command.args)
        .output()
        .map_err(|error| format!("{}: {error}", command.program))?;
    if result.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&result.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("{} exited with {}", command.program, result.status)
    } else {
        format!(
            "{} exited with {}: {stderr}",
            command.program, result.status
        )
    })
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

pub(crate) fn not_installed_error(name: &str) -> String {
    format!("{name} isn't installed. Choose an editor in Settings")
}

pub(crate) fn display_name(setting: &str) -> String {
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

pub(crate) fn known_checkout_path(
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

pub(crate) fn local_editor_path(source: &SourcePath) -> Option<&Path> {
    source.as_local()
}

pub(crate) fn viewer_header_title(path: &str, error: Option<&str>) -> String {
    error.unwrap_or(path).to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn reports_custom_command_failure_after_waiting_for_child() {
        let error = run_command(OpenCommand {
            program: "/usr/bin/false".into(),
            args: Vec::new(),
        })
        .unwrap_err();
        assert!(error.contains("exited with"), "{error}");
    }
}
