//! Application launch contract, separate from the low-level backend settings.
use crate::{Backend, BackendSettings};
use std::{collections::HashMap, io, path::PathBuf};

#[derive(Debug, Clone)]
pub struct TerminalLaunch {
    pub cwd: Option<PathBuf>,
    /// Inherit the parent environment, then set (`Some`) or remove (`None`).
    pub env: HashMap<String, Option<String>>,
    pub program: String,
    pub args: Vec<String>,
    pub cols: usize,
    pub rows: usize,
    /// Trusted shell command, entered once when shell integration reports readiness.
    pub startup_command: Option<String>,
}
impl Default for TerminalLaunch {
    fn default() -> Self {
        let program = std::env::var("SHELL")
            .ok()
            .or_else(|| {
                let user = std::env::var("USER").ok()?;
                let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
                passwd.lines().find_map(|line| {
                    let fields: Vec<_> = line.split(':').collect();
                    (fields.first() == Some(&user.as_str()))
                        .then(|| fields.get(6).map(|s| s.to_string()))
                        .flatten()
                })
            })
            .unwrap_or_else(|| "/bin/zsh".into());
        Self {
            cwd: None,
            env: HashMap::new(),
            program,
            args: vec!["-l".into()],
            cols: 80,
            rows: 24,
            startup_command: None,
        }
    }
}
impl TerminalLaunch {
    /// Failures remain renderable terminal states, including invalid cwd/program.
    pub fn start(self) -> Backend {
        let size = (self.cols, self.rows);
        let command = self.startup_command.clone();
        match self.prepare().and_then(|(settings, integration)| {
            let mut backend = Backend::new_sized(settings, size.0, size.1, 1, 1)?;
            backend.integration = integration;
            if let Some(command) = command {
                backend.events.set_startup(command);
            }
            Ok(backend)
        }) {
            Ok(backend) => backend,
            Err(error) => {
                let mut backend = Backend::new_deferred(BackendSettings::default());
                backend.fail_launch(format!("launch terminal: {error}"));
                backend
            }
        }
    }
    pub(crate) fn prepare(self) -> io::Result<(BackendSettings, Option<tempfile::TempDir>)> {
        let cwd = self
            .cwd
            .map(|path| {
                std::fs::canonicalize(&path).map_err(|error| {
                    io::Error::new(error.kind(), format!("cwd {}: {error}", path.display()))
                })
            })
            .transpose()?;
        if let Some(cwd) = &cwd {
            std::fs::read_dir(cwd).map_err(|error| {
                io::Error::new(error.kind(), format!("cwd {}: {error}", cwd.display()))
            })?;
        }
        let base_dir = cwd.clone().unwrap_or(std::env::current_dir()?);
        if self.cols < 2
            || self.rows == 0
            || self.cols > u16::MAX as usize
            || self.rows > u16::MAX as usize
        {
            return Err(invalid("invalid terminal dimensions"));
        }
        for (key, value) in &self.env {
            if key.is_empty()
                || key.contains(['=', '\0'])
                || value.as_ref().is_some_and(|s| s.contains('\0'))
            {
                return Err(invalid("invalid environment overlay"));
            }
        }
        let mut settings = BackendSettings::default();
        settings.env.insert(
            "LANG".into(),
            std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".into()),
        );
        if let Ok(home) = std::env::var("HOME") {
            settings.env.insert("PATH".into(), format!("{home}/.local/bin:{home}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"));
        }
        if let Ok(user) = std::env::var("USER") {
            settings.env.insert("USER".into(), user.clone());
            settings.env.insert("LOGNAME".into(), user);
        }
        settings.env.insert("SHELL".into(), self.program.clone());
        for (key, value) in &self.env {
            if let Some(value) = value {
                settings.env.insert(key.clone(), value.clone());
            } else {
                settings.env.remove(key);
            }
        }
        let path = settings.env.get("PATH").cloned().unwrap_or_default();
        let program = if self.program.contains('/') {
            let path = PathBuf::from(&self.program);
            if path.is_absolute() {
                path
            } else {
                base_dir.join(path)
            }
        } else {
            std::env::split_paths(&path)
                .map(|dir| {
                    let dir = if dir.is_absolute() {
                        dir
                    } else {
                        base_dir.join(dir)
                    };
                    dir.join(&self.program)
                })
                .find(|p| executable(p))
                .ok_or_else(|| invalid(format!("program not found: {}", self.program)))?
        };
        if !executable(&program) {
            return Err(invalid(format!(
                "program is not executable: {}",
                program.display()
            )));
        }
        let mut integration = None;
        if let Some(command) = &self.startup_command {
            if command.len() >= crate::session::INPUT_LIMIT || command.contains(['\0', '\x1b']) {
                return Err(invalid("startup command is too large or contains NUL/ESC"));
            }
            match program.file_name().and_then(|s| s.to_str()) {
                Some("zsh") => {
                    let dir = tempfile::Builder::new()
                        .prefix("gitterm-gpui-shell-")
                        .tempdir()?;
                    let effective = |key: &str| match self.env.get(key) {
                        Some(value) => value.clone(),
                        None => settings
                            .env
                            .get(key)
                            .cloned()
                            .or_else(|| std::env::var(key).ok()),
                    };
                    let original = effective("ZDOTDIR");
                    if original.is_none() && effective("HOME").is_none() {
                        return Err(invalid("cannot resolve shell startup directory"));
                    }
                    if self.env.keys().any(|key| key.starts_with("GITTERM_GPUI_")) {
                        return Err(invalid(
                            "GITTERM_GPUI_ variables are reserved for shell integration",
                        ));
                    }
                    settings.env.insert(
                        "GITTERM_GPUI_HAS_ZDOTDIR".into(),
                        if original.is_some() { "1" } else { "0" }.into(),
                    );
                    settings.env.insert(
                        "GITTERM_GPUI_USER_ZDOTDIR".into(),
                        original.unwrap_or_default(),
                    );
                    settings.env.insert(
                        "GITTERM_GPUI_INTEGRATION".into(),
                        dir.path().to_string_lossy().into(),
                    );
                    for file in [".zshenv", ".zprofile", ".zshrc", ".zlogin"] {
                        // User files see their own ZDOTDIR, including changes from
                        // earlier rc files. Only the next loader sees our directory.
                        let mut script = format!(
                            r#"if [[ $GITTERM_GPUI_HAS_ZDOTDIR == 1 ]]; then
    ZDOTDIR=$GITTERM_GPUI_USER_ZDOTDIR
else
    unset ZDOTDIR
fi
[[ -f "${{ZDOTDIR-$HOME}}/{file}" ]] && source "${{ZDOTDIR-$HOME}}/{file}"
"#
                        );
                        if file == ".zshrc" {
                            script.push_str(
                                r#"zmodload zsh/zle
autoload -Uz add-zle-hook-widget
_gitterm_gpui_ready() {
    [[ $CONTEXT == start ]] || return 0
    printf '\033]133;B\007' >&2
    add-zle-hook-widget -d line-init _gitterm_gpui_ready
}
add-zle-hook-widget line-init _gitterm_gpui_ready
"#,
                            );
                        }
                        let retain_loader = r#"GITTERM_GPUI_HAS_ZDOTDIR=${+ZDOTDIR}
GITTERM_GPUI_USER_ZDOTDIR=${ZDOTDIR-}
ZDOTDIR=$GITTERM_GPUI_INTEGRATION
"#;
                        // zsh reads these files only at startup, and cleanup runs in the last
                        // loader for each shell kind (.zshrc non-login, .zlogin login), so the
                        // directory is not needed afterwards. Remove it here: the Rust
                        // TempDir is only dropped when the session is, which may never happen.
                        let cleanup = r#"[[ ${GITTERM_GPUI_INTEGRATION:t} == gitterm-gpui-shell-* ]] && command rm -rf -- "$GITTERM_GPUI_INTEGRATION"
unset GITTERM_GPUI_HAS_ZDOTDIR GITTERM_GPUI_USER_ZDOTDIR GITTERM_GPUI_INTEGRATION
"#;
                        match file {
                            ".zlogin" => script.push_str(cleanup),
                            ".zshrc" => script.push_str(&format!(
                                "if [[ -o login ]]; then\n{retain_loader}else\n{cleanup}fi\n"
                            )),
                            _ => script.push_str(retain_loader),
                        }
                        std::fs::write(dir.path().join(file), script)?;
                    }
                    settings
                        .env
                        .insert("ZDOTDIR".into(), dir.path().to_string_lossy().into());
                    integration = Some(dir);
                }
                Some("bash") => {
                    // Same PROMPT_COMMAND mechanism as Iced; PS1's final marker
                    // ensures input admission waits until the first prompt is printed.
                    settings.env.insert(
                        "PROMPT_COMMAND".into(),
                        "PS1=\"${PS1}\\[\\e]133;B\\a\\]\"; unset PROMPT_COMMAND".into(),
                    );
                }
                _ => {
                    return Err(invalid(
                        "startup command requires zsh or bash prompt integration",
                    ));
                }
            }
        }
        // The platform env utility handles removals and chdir in the child,
        // without shell initialization or putting environment values in argv.
        // Alacritty itself silently ignores chdir failures.
        let mut args = vec![
            "-C".into(),
            base_dir
                .to_str()
                .ok_or_else(|| invalid("cwd must be UTF-8 for the PTY launcher"))?
                .into(),
        ];
        for (key, value) in &self.env {
            if value.is_none() && !(integration.is_some() && key == "ZDOTDIR") {
                args.extend(["-u".into(), key.clone()]);
            }
        }
        // shortcut: env cannot launch '=' program paths; replace this launcher
        // if support for those executable names is needed.
        // env treats any utility containing '=' as an assignment, even after
        // '--'. Refuse it rather than accidentally printing the environment.
        if program.as_os_str().as_encoded_bytes().contains(&b'=') {
            return Err(invalid(
                "program path cannot contain '=' with the env launcher",
            ));
        }
        args.push("--".into());
        args.push(
            program
                .to_str()
                .ok_or_else(|| invalid("program path must be UTF-8 for the PTY launcher"))?
                .into(),
        );
        args.extend(self.args);
        settings.program = "/usr/bin/env".into();
        settings.args = args;
        settings.working_directory = cwd;
        Ok((settings, integration))
    }
}
fn executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// SIGKILL to the process group `pid` leads: a PTY child from
/// [`crate::Backend::child_pid`] and everything it started that kept its
/// group. While any member of the group lives its id cannot be reused.
// shortcut: once the child exited by itself, was reaped and left no group
// member, a new session leader could take the same pid and be signalled;
// track the child's exit before killing if this is used outside tests.
pub fn kill_process_group(pid: u32) -> io::Result<()> {
    let pid = i32::try_from(pid).map_err(|_| invalid("pid out of range"))?;
    // SAFETY: killpg only sends a signal.
    if unsafe { libc::killpg(pid, libc::SIGKILL) } == 0 {
        return Ok(());
    }
    match io::Error::last_os_error() {
        // The group is already gone.
        error if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        error => Err(error),
    }
}
