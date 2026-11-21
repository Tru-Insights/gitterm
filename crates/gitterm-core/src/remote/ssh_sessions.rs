//! Read-only facts for legacy SSH remote sessions: the tmux sessions a host
//! is running, listed over `ssh` with the host's own identity file.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RemoteHostConfig {
    pub name: String,
    pub ssh_target: String,
    pub identity_file: PathBuf,
    pub tmux_path: String,
}

impl RemoteHostConfig {
    /// The host a saved remote session runs on (identity file `~` expanded).
    pub fn from_session(config: &crate::workspace_config::RemoteSessionConfig) -> Self {
        Self {
            name: config.host_name.clone(),
            ssh_target: config.ssh_target.clone(),
            identity_file: super::token::expand_home_path(&config.identity_file),
            tmux_path: config.tmux_path.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSessionEntry {
    pub name: String,
    pub windows: usize,
    pub attached: usize,
}

#[derive(Debug, Clone)]
pub struct RemoteSessionsSnapshot {
    pub host_name: String,
    pub sessions: Vec<RemoteSessionEntry>,
    pub error: Option<String>,
}

/// List `host`'s tmux sessions over ssh (blocking; batch mode, so it never
/// prompts). Failures come back in `error`, never as an empty success.
pub fn collect_remote_sessions(host: &RemoteHostConfig) -> RemoteSessionsSnapshot {
    let mut snapshot = RemoteSessionsSnapshot {
        host_name: host.name.clone(),
        sessions: Vec::new(),
        error: None,
    };

    let remote_command = format!(
        "{} list-sessions -F '#{{session_name}}\t#{{session_windows}}\t#{{session_attached}}' 2>/dev/null || true",
        host.tmux_path
    );
    let output = match std::process::Command::new("ssh")
        .arg("-i")
        .arg(&host.identity_file)
        .args([
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
        ])
        .arg(&host.ssh_target)
        .arg(remote_command)
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            snapshot.error = Some(format!("Failed to run ssh: {}", e));
            return snapshot;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        snapshot.error = if stderr.is_empty() {
            Some(format!("ssh exited with status {}", output.status))
        } else {
            Some(stderr)
        };
        return snapshot;
    }

    snapshot.sessions = parse_tmux_sessions(&String::from_utf8_lossy(&output.stdout));
    snapshot
}

/// `name\twindows\tattached` lines from `tmux list-sessions`, sorted by name.
pub fn parse_tmux_sessions(stdout: &str) -> Vec<RemoteSessionEntry> {
    let mut sessions = Vec::new();
    for line in stdout.lines() {
        let mut parts = line.split('\t');
        let name = parts.next().unwrap_or("").trim().to_string();
        if name.is_empty() {
            continue;
        }
        let windows = parts
            .next()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let attached = parts
            .next()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        sessions.push(RemoteSessionEntry {
            name,
            windows,
            attached,
        });
    }
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    sessions
}

/// The tmux session that runs a remote session's Codex / Claude.
pub fn codex_session_name(session_name: &str) -> String {
    format!("{session_name}-codex")
}

pub fn claude_session_name(session_name: &str) -> String {
    format!("{session_name}-claude")
}

/// "attached N", "running" or "not running" for a session's tmux entry.
pub fn session_status(runtime: Option<&RemoteSessionEntry>) -> String {
    match runtime {
        Some(entry) if entry.attached > 0 => format!("attached {}", entry.attached),
        Some(_) => "running".to_string(),
        None => "not running".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_status_names_attached_running_and_stopped() {
        let entry = |attached| RemoteSessionEntry {
            name: "s".into(),
            windows: 1,
            attached,
        };
        assert_eq!(session_status(Some(&entry(2))), "attached 2");
        assert_eq!(session_status(Some(&entry(0))), "running");
        assert_eq!(session_status(None), "not running");
        assert_eq!(codex_session_name("s"), "s-codex");
    }

    #[test]
    fn tmux_lines_parse_sorted_skipping_blank_names_and_bad_counts() {
        let sessions = parse_tmux_sessions("web\t2\t1\n\n\t3\t0\napi\tx\n");
        assert_eq!(
            sessions,
            [
                RemoteSessionEntry {
                    name: "api".into(),
                    windows: 0,
                    attached: 0
                },
                RemoteSessionEntry {
                    name: "web".into(),
                    windows: 2,
                    attached: 1
                },
            ]
        );
    }
}
