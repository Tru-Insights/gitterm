//! Frozen Iced terminal composition from slice 7, compared via fake children.
use super::*;

fn before(
    cwd: &Path,
    startup_command: Option<&str>,
    extra_env: &[(&str, &str)],
    config_root: &Path,
    configure_browser: impl Fn(&str, &str) -> String,
) -> TerminalCommand {
    #[cfg(target_os = "windows")]
    let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string());

    #[cfg(not(target_os = "windows"))]
    let shell = std::env::var("SHELL")
        .ok()
        .or_else(|| {
            let user = std::env::var("USER").ok()?;
            let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
            for line in passwd.lines() {
                let parts: Vec<&str> = line.split(':').collect();
                if parts.first() == Some(&user.as_str()) {
                    return parts.get(6).map(|s| s.to_string());
                }
            }
            None
        })
        .unwrap_or_else(|| "/bin/zsh".to_string());

    let mut env = std::collections::HashMap::new();

    #[cfg(not(target_os = "windows"))]
    {
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());
        env.insert(
            "LANG".to_string(),
            std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".to_string()),
        );
        if let Ok(home) = std::env::var("HOME") {
            env.insert("HOME".to_string(), home.clone());
            env.insert("PATH".to_string(), format!("{}/.local/bin:{}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin", home, home));
        }
        if let Ok(user) = std::env::var("USER") {
            env.insert("USER".to_string(), user.clone());
            env.insert("LOGNAME".to_string(), user);
        }
        env.insert("SHELL".to_string(), shell.clone());
    }

    #[cfg(target_os = "windows")]
    {
        for (key, value) in std::env::vars() {
            env.insert(key, value);
        }
    }

    env.insert("GITTERM_PRECMD".to_string(), "1".to_string());

    // A child GitTerm must never reuse its launching worker's PID file.
    env.remove(crate::worker_process::PID_FILE_ENV);
    for (key, value) in extra_env {
        env.insert(key.to_string(), value.to_string());
    }

    let runtime_startup_command = startup_command.map(|command| {
        let command = env
            .get(crate::task_commands::mcp_protocol::BROWSER_MCP_URL_ENV)
            .map(|endpoint| configure_browser(command, endpoint))
            .unwrap_or_else(|| command.to_string());
        env.get(crate::task_mcp::TASK_MCP_URL_ENV)
            .map(|endpoint| crate::task_mcp::configure_task_command(&command, endpoint))
            .unwrap_or(command)
    });
    let runtime_startup_command = runtime_startup_command.map(|command| {
        env.get(crate::worker_process::PID_FILE_ENV)
            .map(|path| {
                crate::worker_process::record_shell(&command, path).unwrap_or_else(|error| {
                    eprintln!("[tasks] refuse worker launch: {error}");
                    "exit 1".to_string()
                })
            })
            .unwrap_or(command)
    });
    if let Some(command) = &runtime_startup_command {
        env.insert("GITTERM_STARTUP_CMD".to_string(), command.clone());
    }

    env.insert("CLAUDECODE".to_string(), String::new());
    env.insert("CLAUDE_CODE_ENTRYPOINT".to_string(), String::new());

    let is_zsh = shell.contains("zsh");
    let is_bash = shell.contains("bash");
    let is_windows = cfg!(target_os = "windows");

    let args = if is_windows {
        if let Some(cmd) = runtime_startup_command.as_deref() {
            let lower = shell.to_lowercase();
            if lower.contains("powershell") || lower.contains("pwsh") {
                vec![
                    "-NoExit".to_string(),
                    "-Command".to_string(),
                    cmd.to_string(),
                ]
            } else {
                vec!["/K".to_string(), cmd.to_string()]
            }
        } else {
            vec![]
        }
    } else if is_zsh {
        let home = std::env::var("HOME").unwrap_or_default();
        let gitterm_dir = config_root
            .to_path_buf()
            .join("zsh")
            .to_string_lossy()
            .into_owned();
        let gitterm_zshrc = format!("{gitterm_dir}/.zshrc");
        let codex_zsh_integration = crate::task_commands::mcp_protocol::codex_zsh_integration();

        let _ = std::fs::create_dir_all(&gitterm_dir);
        let zshrc_content = format!(
            r#"# GitTerm shell integration - sets terminal title on directory change
_gitterm_set_title() {{ print -Pn "\e]0;%~\a" }}
autoload -Uz add-zsh-hook
add-zsh-hook precmd _gitterm_set_title
add-zsh-hook chpwd _gitterm_set_title
_gitterm_set_title
[[ -f "{home}/.zshenv" ]] && source "{home}/.zshenv"
[[ -f "{home}/.zprofile" ]] && source "{home}/.zprofile"
[[ -f "{home}/.zshrc" ]] && source "{home}/.zshrc"
{codex_zsh_integration}
if [[ -n "$GITTERM_STARTUP_CMD" ]]; then
    _gitterm_cmd="$GITTERM_STARTUP_CMD"
    unset GITTERM_STARTUP_CMD
    eval "$_gitterm_cmd"
    unset _gitterm_cmd
fi
"#
        );
        let _ = std::fs::write(&gitterm_zshrc, zshrc_content);

        env.insert("ZDOTDIR".to_string(), gitterm_dir);
        vec!["-l".to_string()]
    } else if is_bash {
        let prompt_cmd = r#"printf "\e]0;%s\a" "$PWD"; if [[ -n "$GITTERM_STARTUP_CMD" ]]; then _c="$GITTERM_STARTUP_CMD"; unset GITTERM_STARTUP_CMD; eval "$_c"; unset _c; fi"#;
        env.insert("PROMPT_COMMAND".to_string(), prompt_cmd.to_string());
        vec!["-l".to_string()]
    } else {
        vec!["-l".to_string()]
    };

    TerminalCommand {
        program: shell,
        args,
        env,
        cwd: cwd.to_path_buf(),
    }
}

#[test]
#[cfg(unix)]
fn iced_before_after_fake_child_argv_env_cwd() {
    use std::{
        io::Read,
        os::unix::fs::PermissionsExt,
        process::{Command, Stdio},
    };
    const CHILD: &str = "GITTERM_LAUNCH_PARITY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        let config = root.join("v6");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&config).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "task_host::command::tests::iced_before_after_fake_child_argv_env_cwd",
                "--nocapture",
            ])
            .env(CHILD, &root)
            .env("HOME", home)
            .env("GITTERM_V6_CONFIG_DIR", config)
            .env_remove("CODEX_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = PathBuf::from(std::env::var_os(CHILD).unwrap());
    let config = PathBuf::from(std::env::var_os("GITTERM_V6_CONFIG_DIR").unwrap());
    assert!(config.is_absolute());
    let cwd = root.join("checkout");
    std::fs::create_dir(&cwd).unwrap();
    let shell = root.join("zsh");
    // This fake records the terminal launch; it never executes a real harness.
    std::fs::write(
        &shell,
        r##"#!/bin/sh
printf '%s\0' "$@" > "$TRACE/argv"
/usr/bin/env | /usr/bin/sort > "$TRACE/env"
pwd -P > "$TRACE/cwd"
printf ready
read _fixture_release
"##,
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("SHELL", &shell);
    for command in [
        "claude --model sample --session-id fixed",
        "codex -m sample",
        "codex exec review --uncommitted --json",
        "codex exec --json --sandbox read-only -",
    ] {
        let extra = [
            ("PLACEHOLDER", "value with spaces"),
            (
                crate::worker_process::PID_FILE_ENV,
                "/placeholder/launch.json",
            ),
            (
                crate::task_mcp::TASK_MCP_URL_ENV,
                "http://127.0.0.1:1234/mcp",
            ),
        ];
        let old = before(&cwd, Some(command), &extra, &config, |cmd, _| cmd.into());
        let new = terminal_command(&cwd, Some(command), &extra, &config, |cmd, _| cmd.into());
        assert_eq!(old, new);
        let trace = root.join("trace");
        std::fs::create_dir_all(&trace).unwrap();
        let mut records = Vec::new();
        for spec in [old, new] {
            let mut child = Command::new(spec.program)
                .args(spec.args)
                .env_clear()
                .envs(spec.env)
                .env("TRACE", &trace)
                .current_dir(spec.cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            // The pipe is the readiness event: scheduling the fake child may
            // take arbitrarily long on a busy host. It writes only after all
            // three trace files are complete, and EOF reports an early exit.
            let mut ready = [0; 5];
            child.stdout.take().unwrap().read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"ready");
            records
                .push(["argv", "env", "cwd"].map(|file| std::fs::read(trace.join(file)).unwrap()));
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert_eq!(records[0], records[1], "{command}");
    }
}
