//! Shared desktop policy for a standard local Claude launch.
//! Call from the session thread, after the host discovers and cancellation-binds
//! its snapshot recorder. The transport also accepts explicit `Launch` inputs
//! for callers with a separate environment policy (such as fenced previews).

use super::claude::{ClaudeMcpServer, ClaudeSessionConfig, Launch, SnapshotRecorder};
use std::ffi::OsString;
use std::path::PathBuf;

/// Directories where `claude` is commonly installed but that a Finder-
/// launched app's PATH lacks. Mirrors the PATH GitTerm gives terminal tabs.
fn extra_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs
}

/// The PATH for the child: the inherited PATH plus the usual install dirs.
pub fn child_path() -> OsString {
    let mut entries: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for dir in extra_bin_dirs() {
        if !entries.contains(&dir) {
            entries.push(dir);
        }
    }
    std::env::join_paths(entries).unwrap_or_default()
}

/// Locate the `claude` executable on the child PATH.
pub fn resolve_claude_program() -> Result<PathBuf, String> {
    let path = child_path();
    std::env::split_paths(&path)
        .map(|dir| dir.join("claude"))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            format!(
                "could not find the `claude` executable on PATH ({})",
                path.to_string_lossy()
            )
        })
}

/// Compose the standard local launch with the host's existing recorder.
/// Executable resolution is separate so hosts retain their discovery order;
/// MCP overrides affect the child's env, never the executable selected earlier.
pub fn local_launch(
    config: ClaudeSessionConfig,
    program: PathBuf,
    snapshots: Box<dyn SnapshotRecorder>,
) -> Launch {
    let environment = child_environment(std::env::vars_os(), child_path(), &config.mcp_servers);
    Launch {
        flags: Default::default(),
        config,
        program,
        environment,
        temporary_dir: std::env::temp_dir(),
        snapshots,
    }
}

/// Preserve inherited OS values, then apply SDK policy and MCP overrides.
pub fn child_environment(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    path: OsString,
    servers: &[ClaudeMcpServer],
) -> Vec<(OsString, OsString)> {
    // Command preserves platform environment-key semantics (case-insensitive
    // on Windows). It is only used to build the explicit map, never executed.
    let inherited: Vec<_> = inherited.into_iter().collect();
    let mut command = std::process::Command::new("claude");
    command
        .env_clear()
        .envs(inherited.iter().cloned())
        .env("PATH", path);
    for (key, _) in inherited {
        let key = key.to_string_lossy();
        if key == "CLAUDECODE" || key.starts_with("CLAUDE_CODE_") || key == "CLAUDE_PID" {
            command.env_remove(key.as_ref());
        }
    }
    command
        .env_remove("NODE_OPTIONS")
        .env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts")
        .env("CLAUDE_CODE_SDK_READS_SESSION_STATE", "1");
    for server in servers {
        command.envs(server.env.iter().map(|(key, value)| (key, value)));
    }
    command
        .get_envs()
        .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
        .collect()
}
