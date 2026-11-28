//! Iced's remote terminal launch contract. Saved commands are replayed verbatim;
//! attaching is a local PTY bridge, never a request to start another agent session.
use super::{
    config::{RemoteAgentAuthConfig, RemoteAgentConfig},
    ssh_sessions::RemoteHostConfig,
};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct RemoteSessionDefinition {
    pub host: RemoteHostConfig,
    pub label: String,
    pub session_name: String,
    pub remote_dir: String,
    pub shell_command: String,
    pub codex_command: Option<String>,
    pub claude_command: Option<String>,
}

impl From<&crate::workspace_config::RemoteSessionConfig> for RemoteSessionDefinition {
    fn from(config: &crate::workspace_config::RemoteSessionConfig) -> Self {
        Self {
            host: RemoteHostConfig::from_session(config),
            label: config.label.clone(),
            session_name: config.session_name.clone(),
            remote_dir: config.remote_dir.clone(),
            shell_command: config.shell_command.clone(),
            codex_command: config.codex_command.clone(),
            claude_command: config.claude_command.clone(),
        }
    }
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn remote_attach_command(
    session: &RemoteSessionDefinition,
    tmux_session_name: &str,
    startup_command: &str,
) -> String {
    let remote_script = format!(
        "SESSION={}; DIR={}; {} has-session -t \"$SESSION\" 2>/dev/null || {} new-session -d -s \"$SESSION\" -c \"$DIR\" {}; {} set-option -t \"$SESSION\" mouse on >/dev/null 2>&1 || true; exec {} attach -t \"$SESSION\"",
        shell_quote(tmux_session_name),
        shell_quote(&session.remote_dir),
        shell_quote(&session.host.tmux_path),
        shell_quote(&session.host.tmux_path),
        shell_quote(startup_command),
        shell_quote(&session.host.tmux_path),
        shell_quote(&session.host.tmux_path),
    );

    format!(
        "exec ssh -t -i {} -o IdentitiesOnly=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 {} {}",
        shell_quote(session.host.identity_file.to_string_lossy().as_ref()),
        shell_quote(&session.host.ssh_target),
        shell_quote(&remote_script),
    )
}

#[derive(Debug, Clone, Copy)]
pub enum SessionKind {
    Shell,
    Codex,
    Claude,
}

/// Selection from Iced's three open_remote_*_session_tab routes.
pub fn session_command(
    session: &RemoteSessionDefinition,
    kind: SessionKind,
) -> Option<(String, String, &'static str)> {
    let (name, command, title) = match kind {
        SessionKind::Shell => (
            session.session_name.clone(),
            &session.shell_command,
            "Shell",
        ),
        SessionKind::Codex => (
            super::ssh_sessions::codex_session_name(&session.session_name),
            session.codex_command.as_ref()?,
            "Codex",
        ),
        SessionKind::Claude => (
            super::ssh_sessions::claude_session_name(&session.session_name),
            session.claude_command.as_ref()?,
            "Claude",
        ),
    };
    Some((name, command.clone(), title))
}

pub fn attach_cwd(remote_identity: bool, workspace_dir: &Path, home: Option<PathBuf>) -> PathBuf {
    if remote_identity {
        home.unwrap_or_else(|| PathBuf::from("."))
    } else {
        workspace_dir.to_path_buf()
    }
}

/// Resolve next to the running executable first, exactly as Iced does.
pub fn agent_binary() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("gitterm-v6-agent")))
        .filter(|path| path.exists())
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "gitterm-v6-agent".to_string())
}

pub fn remote_session_attach_command(
    agent_bin: &str,
    config: &RemoteAgentConfig,
    session_id: &str,
) -> String {
    let RemoteAgentAuthConfig::Token { token_ref } = &config.auth;
    format!(
        "{} attach --endpoint {} --token-ref {} --session {}",
        shell_quote(agent_bin),
        shell_quote(&config.endpoint),
        shell_quote(token_ref),
        shell_quote(session_id)
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionLaunch {
    pub kind: String,
    pub cwd: String,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

/// A browser path is supplied only when it belongs to the remote source.
/// An attach tab's browser is local and must fall back to the remote root.
pub fn agent_session_launch(
    preset_name: Option<&str>,
    config: Option<&RemoteAgentConfig>,
    root: &str,
    remote_browser_dir: Option<&str>,
    chrome: bool,
) -> AgentSessionLaunch {
    let normalized = preset_name.map(|name| {
        let full = name.to_lowercase();
        let first_word = full
            .split_whitespace()
            .next()
            .unwrap_or("shell")
            .to_string();
        (full, first_word)
    });
    let kind = normalized
        .as_ref()
        .map(|(_, first_word)| first_word.clone())
        .unwrap_or_else(|| "shell".to_string());
    let remote_command = match (&normalized, &config) {
        (Some((full, first_word)), Some(config)) => config
            .session_commands
            .get(full)
            .or_else(|| config.session_commands.get(first_word))
            .cloned()
            .unwrap_or_else(|| first_word.clone()),
        (Some((_, first_word)), None) => first_word.clone(),
        (None, Some(config)) => config
            .shell_command
            .clone()
            .unwrap_or_else(|| "/bin/zsh -il".to_string()),
        (None, None) => "/bin/zsh -il".to_string(),
    };
    let remote_command = crate::harness::claude::configure_terminal_chrome(&remote_command, chrome);
    AgentSessionLaunch {
        kind,
        cwd: remote_browser_dir.unwrap_or(root).to_string(),
        command: vec!["/bin/sh".to_string(), "-lc".to_string(), remote_command],
        env: Vec::new(),
        cols: 120,
        rows: 32,
    }
}

/// Iced separates the local PTY launch directory from its tracked display cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalPaths {
    pub cwd: PathBuf,
    pub current_dir: PathBuf,
}

pub fn restored_terminal_paths(
    remote_agent: bool,
    tab: &crate::workspace_config::WorkspaceTabConfig,
    home: Option<PathBuf>,
) -> TerminalPaths {
    let tab_dir = PathBuf::from(&tab.dir);
    if remote_agent {
        return TerminalPaths {
            cwd: home.unwrap_or_else(|| PathBuf::from(".")),
            current_dir: tab_dir,
        };
    }
    let cwd = tab
        .repo_dir
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| {
            git2::Repository::discover(&tab_dir)
                .ok()
                .and_then(|repo| repo.workdir().map(PathBuf::from))
        })
        .unwrap_or_else(|| tab_dir.clone());
    let current_dir = if tab_dir.is_dir() {
        tab_dir
    } else {
        cwd.clone()
    };
    TerminalPaths { cwd, current_dir }
}

pub fn tab_title(session: &RemoteSessionDefinition, startup_command: Option<&str>) -> String {
    let Some(command) = startup_command else {
        return "Shell".into();
    };
    if command.contains(&super::ssh_sessions::codex_session_name(
        &session.session_name,
    )) {
        "Codex"
    } else if command.contains(&super::ssh_sessions::claude_session_name(
        &session.session_name,
    )) {
        "Claude"
    } else if command.contains(&session.session_name) {
        "Shell"
    } else {
        "Remote"
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> RemoteSessionDefinition {
        RemoteSessionDefinition {
            host: RemoteHostConfig {
                name: "Placeholder".into(),
                ssh_target: "user@remote.example".into(),
                identity_file: "/tmp/key with 'quote'".into(),
                tmux_path: "/opt/tmux path/it's tmux".into(),
            },
            label: "Placeholder".into(),
            session_name: "placeholder's session".into(),
            remote_dir: "/srv/project's folder".into(),
            shell_command: "/bin/zsh -il".into(),
            codex_command: Some("/opt/agent bin/codex --model 'placeholder'".into()),
            claude_command: Some("claude --model placeholder".into()),
        }
    }

    #[test]
    fn golive19_quoting_paths_and_session_routes() {
        assert_eq!(shell_quote("a b'c"), r#"'a b'\''c'"#);
        let session = session();
        for (kind, suffix, title, command) in [
            (SessionKind::Shell, "", "Shell", "/bin/zsh -il"),
            (
                SessionKind::Codex,
                "-codex",
                "Codex",
                "/opt/agent bin/codex --model 'placeholder'",
            ),
            (
                SessionKind::Claude,
                "-claude",
                "Claude",
                "claude --model placeholder",
            ),
        ] {
            let (name, selected, label) = session_command(&session, kind).unwrap();
            assert_eq!(name, format!("placeholder's session{suffix}"));
            assert_eq!(selected, command);
            assert_eq!(label, title);
        }
        let words = shell_words::split(&remote_attach_command(
            &session,
            &session.session_name,
            &session.shell_command,
        ))
        .unwrap();
        assert_eq!(
            &words[..12],
            [
                "exec",
                "ssh",
                "-t",
                "-i",
                "/tmp/key with 'quote'",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "user@remote.example"
            ]
        );
        assert_eq!(
            words[12],
            r#"SESSION='placeholder'\''s session'; DIR='/srv/project'\''s folder'; '/opt/tmux path/it'\''s tmux' has-session -t "$SESSION" 2>/dev/null || '/opt/tmux path/it'\''s tmux' new-session -d -s "$SESSION" -c "$DIR" '/bin/zsh -il'; '/opt/tmux path/it'\''s tmux' set-option -t "$SESSION" mouse on >/dev/null 2>&1 || true; exec '/opt/tmux path/it'\''s tmux' attach -t "$SESSION""#
        );
        let mut missing = session;
        missing.codex_command = None;
        assert!(session_command(&missing, SessionKind::Codex).is_none());
    }

    fn agent() -> RemoteAgentConfig {
        RemoteAgentConfig {
            id: "placeholder".into(),
            name: "Placeholder".into(),
            endpoint: "http://remote.example:7777".into(),
            auth: RemoteAgentAuthConfig::Token {
                token_ref: "env:PLACEHOLDER_TOKEN".into(),
            },
            shell_command: Some("/bin/bash -il".into()),
            session_commands: [
                (
                    "claude code".into(),
                    "'/opt/agent bin/claude' --model 'placeholder'".into(),
                ),
                ("codex".into(), "/opt/bin/codex --model placeholder".into()),
            ]
            .into(),
        }
    }

    #[test]
    fn golive19_agent_preset_cwd_and_attach_exact_commands() {
        let config = agent();
        let shell = agent_session_launch(None, Some(&config), "/srv/root", None, false);
        assert_eq!(
            shell,
            AgentSessionLaunch {
                kind: "shell".into(),
                cwd: "/srv/root".into(),
                command: vec!["/bin/sh".into(), "-lc".into(), "/bin/bash -il".into()],
                env: vec![],
                cols: 120,
                rows: 32
            }
        );
        let claude = agent_session_launch(
            Some("Claude Code"),
            Some(&config),
            "/srv/root",
            Some("/srv/project's folder"),
            true,
        );
        assert_eq!(claude.kind, "claude");
        assert_eq!(claude.cwd, "/srv/project's folder");
        assert_eq!(
            claude.command,
            [
                "/bin/sh",
                "-lc",
                "'/opt/agent bin/claude' --model 'placeholder'"
            ]
        );
        let codex =
            agent_session_launch(Some("Codex CLI"), Some(&config), "/srv/root", None, false);
        assert_eq!(
            codex.command,
            ["/bin/sh", "-lc", "/opt/bin/codex --model placeholder"]
        );
        assert_eq!(
            agent_session_launch(Some("Other Agent"), None, "/srv/root", None, false).command,
            ["/bin/sh", "-lc", "other"]
        );
        assert_eq!(
            agent_session_launch(None, None, "/srv/root", None, false).command,
            ["/bin/sh", "-lc", "/bin/zsh -il"]
        );
        assert_eq!(
            remote_session_attach_command(
                "/opt/agent's bin/gitterm-v6-agent",
                &config,
                "session's id"
            ),
            r#"'/opt/agent'\''s bin/gitterm-v6-agent' attach --endpoint 'http://remote.example:7777' --token-ref 'env:PLACEHOLDER_TOKEN' --session 'session'\''s id'"#
        );
        assert_eq!(
            attach_cwd(true, Path::new("/remote/root"), Some("/local/home".into())),
            PathBuf::from("/local/home")
        );
        assert_eq!(
            attach_cwd(false, Path::new("/local/project"), None),
            PathBuf::from("/local/project")
        );
    }

    #[test]
    fn golive19_restore_paths_keep_iced_launch_and_display_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let repo = root.join("placeholder repo");
        git2::Repository::init(&repo).unwrap();
        let nested = repo.join("nested folder");
        std::fs::create_dir(&nested).unwrap();
        let mut tab: crate::workspace_config::WorkspaceTabConfig =
            serde_json::from_value(serde_json::json!({"dir":nested})).unwrap();
        assert_eq!(
            restored_terminal_paths(false, &tab, None),
            TerminalPaths {
                cwd: repo.clone(),
                current_dir: nested.clone()
            }
        );
        let explicit = root.join("explicit cwd");
        std::fs::create_dir(&explicit).unwrap();
        tab.repo_dir = Some(explicit.to_string_lossy().into_owned());
        assert_eq!(
            restored_terminal_paths(false, &tab, None),
            TerminalPaths {
                cwd: explicit.clone(),
                current_dir: nested
            }
        );
        tab.dir = "/srv/remote-only path".into();
        assert_eq!(
            restored_terminal_paths(false, &tab, None),
            TerminalPaths {
                cwd: explicit.clone(),
                current_dir: explicit
            }
        );
        assert_eq!(
            restored_terminal_paths(true, &tab, Some(root.clone())),
            TerminalPaths {
                cwd: root,
                current_dir: "/srv/remote-only path".into()
            }
        );
        assert_eq!(
            restored_terminal_paths(true, &tab, None).cwd,
            PathBuf::from(".")
        );
    }

    #[cfg(unix)]
    #[test]
    fn golive19_fake_tmux_attach_never_duplicates_after_restart() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let tmux = temp.path().join("placeholder's tmux");
        let state = temp.path().join("session-exists");
        let trace = temp.path().join("trace");
        std::fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> {trace}
case "$1" in
  has-session) test -f {state} ;;
  new-session) touch {state} ;;
esac
"#,
                trace = shell_quote(trace.to_str().unwrap()),
                state = shell_quote(state.to_str().unwrap())
            ),
        )
        .unwrap();
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut session = session();
        session.host.tmux_path = tmux.to_str().unwrap().into();
        let command =
            remote_attach_command(&session, &session.session_name, &session.shell_command);
        // Simulate disk/restart, then execute only the remote script with fake
        // tmux. Never execute ssh, nor rely on machine-wide PATH changes.
        let saved = temp.path().join("saved-command");
        std::fs::write(&saved, &command).unwrap();
        for command in [command, std::fs::read_to_string(saved).unwrap()] {
            let words = shell_words::split(&command).unwrap();
            assert!(std::process::Command::new("/bin/sh")
                .args(["-c", words.last().unwrap()])
                .status()
                .unwrap()
                .success());
        }
        let log = std::fs::read_to_string(trace).unwrap();
        assert_eq!(
            log.lines()
                .filter(|line| line.starts_with("new-session "))
                .count(),
            1
        );
        assert_eq!(
            log.lines()
                .filter(|line| line.starts_with("attach "))
                .count(),
            2
        );
        assert!(log.contains("-c /srv/project's folder /bin/zsh -il"));
    }
}
