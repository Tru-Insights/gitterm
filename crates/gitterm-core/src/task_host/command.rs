//! UI-free production terminal launch, shared by Iced and GPUI.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: PathBuf,
}

/// Iced's terminal settings, including its shell integration. The browser
/// adapter lives in gitterm-browser; both frontends pass the same function.
pub fn terminal_command(
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

pub fn check_delegation_run(
    new: &mut crate::tasks::NewDelegation,
    caller: &crate::delegations::CallerTab,
) -> Result<crate::delegation_state::Authority, String> {
    crate::delegations::check_run(new, caller)?;
    let path = new.options.checkout.as_deref().unwrap_or(&new.parent.cwd);
    let checkout = std::fs::canonicalize(path)
        .map_err(|error| format!("resolve delegation checkout {}: {error}", path.display()))?;
    // Review/consult authority tracks Git's actual common directory, including
    // bare-backed worktrees and submodules; worker main-checkout trust is separate.
    let repository = crate::delegations::repository_identity(&checkout)?;
    let live = git2::Repository::discover(&checkout)
        .map_err(|error| format!("open delegation checkout {}: {error}", checkout.display()))?;
    let workdir = live.workdir().ok_or_else(|| {
        format!(
            "delegation checkout {} has no working directory",
            checkout.display()
        )
    })?;
    let workdir = std::fs::canonicalize(workdir).map_err(|error| {
        format!(
            "resolve delegation working directory {}: {error}",
            workdir.display()
        )
    })?;
    // Matches check_run's known_roots: a missing unrelated repository does
    // not invalidate a target that another available root authorizes.
    let roots = std::iter::once(&caller.home)
        .chain(caller.repos.iter())
        .map(|root| std::fs::canonicalize(root).ok())
        .collect();
    Ok(crate::delegation_state::Authority {
        checkout,
        repository,
        workdir,
        roots,
    })
}

/// Persist the brief once; startup commands embed only this quote-checked path.
pub fn write_task_brief(
    root: &Path,
    session_id: &str,
    task: &crate::tasks::TaskRecord,
    report_back: Option<&str>,
) -> Result<PathBuf, String> {
    let dir = root.join("task-briefs");
    let path = dir.join(format!("{session_id}.md"));
    if path.display().to_string().contains('\'') {
        return Err(format!(
            "brief path {} contains a single quote and cannot be shell-quoted",
            path.display()
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    std::fs::write(
        &path,
        format!(
            "{}\n\nGitTerm task session: {session_id}\n",
            crate::tasks::task_handoff_prompt(task, report_back)
        ),
    )
    .map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
