//! Change guidelines GitTerm adds to every agent it starts (TRU-174).
//!
//! The text lives in `<shared config dir>/agent-guidelines.md`, seeded from
//! `assets/agent-guidelines.md` the first time it is missing; the user edits
//! that file. `<shared config dir>/agent-guidelines.json` holds the global
//! switch and the repos they are off in; a workspace can switch them off in
//! `workspaces.json` (`guidelines_off`).
//!
//! Each harness gets them through its own system-prompt option:
//! - Claude: `--append-system-prompt-file <path>` (claude 2.1.312 names it in
//!   `--help` and reports a missing file as "Append system prompt file not
//!   found").
//! - pi: `--append-system-prompt <path>` (pi 1.0.4: "Append text or file
//!   contents to the system prompt").
//! - Codex: `-c developer_instructions=<TOML string>` (codex-cli 0.164.0
//!   accepts the key under `--strict-config`, which rejects unknown keys).
//!
//! The file is read before every launch: an unreadable one launches the
//! agent without it and is logged. A launch is never blocked.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const GUIDELINES_FILE: &str = "agent-guidelines.md";
pub const SETTINGS_FILE: &str = "agent-guidelines.json";
/// The approved text, written to the shared config dir when it is missing.
pub const DEFAULT_GUIDELINES: &str = include_str!("../assets/agent-guidelines.md");

fn log(msg: &str) {
    eprintln!("[guidelines] {msg}");
}

fn default_true() -> bool {
    true
}

/// `agent-guidelines.json`. A missing file means on everywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuidelinesSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Absolute directories where agents start without the guidelines.
    #[serde(default)]
    pub off_repos: Vec<String>,
}

impl Default for GuidelinesSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            off_repos: Vec::new(),
        }
    }
}

impl GuidelinesSettings {
    /// Whether `off_repos` holds the repo `dir` belongs to.
    pub fn repo_is_off(&self, dir: &Path) -> bool {
        let dir = repo_key(dir);
        self.off_repos
            .iter()
            .any(|off| dir.starts_with(repo_key(Path::new(off))))
    }

    /// Switch the guidelines off (or back on) for the repo `dir` belongs to.
    /// Switching back on drops every entry that covers it.
    pub fn set_repo_off(&mut self, dir: &Path, off: bool) {
        let key = repo_key(dir);
        if off {
            if !self.repo_is_off(dir) {
                self.off_repos.push(key.to_string_lossy().into_owned());
            }
        } else {
            self.off_repos
                .retain(|entry| !key.starts_with(repo_key(Path::new(entry))));
        }
    }
}

pub fn guidelines_path(config_dir: &Path) -> PathBuf {
    config_dir.join(GUIDELINES_FILE)
}

/// Reads `agent-guidelines.json`. Missing means defaults; an unreadable or
/// invalid file is logged and also means defaults, so a bad edit never
/// switches the guidelines off unnoticed.
pub fn load_settings(config_dir: &Path) -> GuidelinesSettings {
    let path = config_dir.join(SETTINGS_FILE);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return GuidelinesSettings::default()
        }
        Err(error) => {
            log(&format!(
                "cannot read {}: {error}; using defaults (on, no exclusions)",
                path.display()
            ));
            return GuidelinesSettings::default();
        }
    };
    serde_json::from_str(&contents).unwrap_or_else(|error| {
        log(&format!(
            "ignoring {}: {error}; using defaults (on, no exclusions)",
            path.display()
        ));
        GuidelinesSettings::default()
    })
}

pub fn save_settings(config_dir: &Path, settings: &GuidelinesSettings) -> Result<(), String> {
    let path = config_dir.join(SETTINGS_FILE);
    let json = serde_json::to_string_pretty(settings)
        .map_err(|error| format!("cannot encode {}: {error}", path.display()))?;
    std::fs::create_dir_all(config_dir)
        .map_err(|error| format!("cannot create {}: {error}", config_dir.display()))?;
    std::fs::write(&path, json).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// The guidelines file, written from the shipped default when missing. An
/// existing file is never overwritten.
pub fn ensure_guidelines_file(config_dir: &Path) -> Result<PathBuf, String> {
    let path = guidelines_path(config_dir);
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(config_dir)
        .map_err(|error| format!("cannot create {}: {error}", config_dir.display()))?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(DEFAULT_GUIDELINES.as_bytes())
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            log(&format!(
                "wrote the default guidelines to {}",
                path.display()
            ));
            Ok(path)
        }
        // Another instance wrote it first.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(path),
        Err(error) => Err(format!("cannot create {}: {error}", path.display())),
    }
}

/// Whether an agent started in `cwd` gets the guidelines: on globally, the
/// repo not excluded, the workspace not switched off.
pub fn applies(settings: &GuidelinesSettings, cwd: &Path, workspace_off: bool) -> bool {
    settings.enabled && !workspace_off && !settings.repo_is_off(cwd)
}

/// The guidelines a launch carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guidelines {
    pub path: PathBuf,
    pub text: String,
}

/// The guidelines for an agent starting now in `cwd`, or `None` when the
/// settings switch them off or the file cannot be read (logged with the
/// path and error; the agent starts without them).
pub fn for_launch(config_dir: &Path, cwd: &Path, workspace_off: bool) -> Option<Guidelines> {
    if !applies(&load_settings(config_dir), cwd, workspace_off) {
        return None;
    }
    let path = match ensure_guidelines_file(config_dir) {
        Ok(path) => path,
        Err(error) => {
            log(&format!("{error}; starting the agent without them"));
            return None;
        }
    };
    match std::fs::read_to_string(&path) {
        Ok(text) if text.trim().is_empty() => {
            log(&format!(
                "{} is empty; starting the agent without guidelines",
                path.display()
            ));
            None
        }
        Ok(text) => Some(Guidelines { path, text }),
        Err(error) => {
            log(&format!(
                "cannot read {}: {error}; starting the agent without them",
                path.display()
            ));
            None
        }
    }
}

/// Claude's options for the control-protocol chat process.
pub fn claude_args(guidelines: &Guidelines) -> Vec<String> {
    vec![
        "--append-system-prompt-file".to_string(),
        guidelines.path.to_string_lossy().into_owned(),
    ]
}

/// pi's options: it reads an existing file given as the value.
pub fn pi_args(guidelines: &Guidelines) -> Vec<String> {
    vec![
        "--append-system-prompt".to_string(),
        guidelines.path.to_string_lossy().into_owned(),
    ]
}

/// The `-c` value for Codex. Codex parses the value as TOML; a JSON string
/// literal is a valid TOML basic string (serde_json escapes `"`, `\` and
/// control characters with escapes TOML shares, and never emits `\/`).
pub fn codex_config_value(guidelines: &Guidelines) -> String {
    let literal =
        serde_json::to_string(&guidelines.text).expect("serializing a string to JSON cannot fail");
    format!("developer_instructions={literal}")
}

pub fn codex_args(guidelines: &Guidelines) -> Vec<String> {
    vec!["-c".to_string(), codex_config_value(guidelines)]
}

/// `(executable, its basename, the rest)` of a startup command.
fn split_command(command: &str) -> (&str, &str, &str) {
    let trimmed = command.trim();
    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let (executable, rest) = trimmed.split_at(end);
    let name = executable
        .trim_matches(['\'', '"'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(executable);
    (executable, name, rest)
}

/// Whether a terminal startup command starts `claude`, `pi` or `codex`.
pub fn starts_a_harness(command: &str) -> bool {
    matches!(split_command(command).1, "claude" | "pi" | "codex")
}

/// A terminal startup command with the guidelines added right after the
/// executable, for `claude`, `pi` and `codex` launches; any other command,
/// or one that already passes the option, is returned unchanged.
pub fn configure_terminal_command(command: &str, guidelines: &Guidelines) -> String {
    let (executable, name, rest) = split_command(command);
    let path = guidelines.path.to_string_lossy();
    let added = match name {
        "claude" if !rest.contains("--append-system-prompt") => {
            format!("--append-system-prompt-file {}", shell_word(&path))
        }
        "pi" if !rest.contains("--append-system-prompt") => {
            format!("--append-system-prompt {}", shell_word(&path))
        }
        "codex" if !rest.contains("developer_instructions") => {
            format!("-c {}", shell_word(&codex_config_value(guidelines)))
        }
        _ => return command.to_string(),
    };
    format!("{executable} {added}{rest}")
}

/// `value` as one POSIX shell word: bare when it holds only safe characters,
/// otherwise single-quoted.
fn shell_word(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '='));
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// The directory `off_repos` compares: the canonical path, with a linked
/// worktree's root replaced by its main repo's root (subdirectories kept).
/// A path that does not exist is compared as given.
pub fn repo_key(path: &Path) -> PathBuf {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    for dir in path.ancestors() {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return path;
        }
        if dot_git.is_file() {
            let Some(main) = linked_worktree_main(&dot_git) else {
                return path;
            };
            // `join("")` would add a trailing separator to the stored key.
            return match path.strip_prefix(dir) {
                Ok(below) if !below.as_os_str().is_empty() => main.join(below),
                _ => main,
            };
        }
    }
    path
}

/// For a linked worktree's `.git` file (`gitdir: <main>/.git/worktrees/<n>`
/// whose `commondir` names `<main>/.git`), the main checkout's root. `None`
/// for anything else (a submodule's `.git` file has no `commondir`).
fn linked_worktree_main(dot_git_file: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(dot_git_file).ok()?;
    let gitdir = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?;
    let gitdir = PathBuf::from(gitdir.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        dot_git_file.parent()?.join(gitdir)
    };
    let commondir = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common = gitdir.join(commondir.trim());
    let common = std::fs::canonicalize(&common).ok()?;
    common.parent().map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `<root>/main` with a `.git` dir and a linked worktree at
    /// `<root>/wt`, laid out the way `git worktree add` writes them.
    fn repo_with_worktree(root: &Path) -> (PathBuf, PathBuf) {
        let main = root.join("main");
        let admin = main.join(".git").join("worktrees").join("wt");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        let wt = root.join("wt");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        (
            std::fs::canonicalize(main).unwrap(),
            std::fs::canonicalize(wt).unwrap(),
        )
    }

    #[test]
    fn missing_settings_file_means_on_everywhere() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load_settings(dir.path());
        assert_eq!(settings, GuidelinesSettings::default());
        assert!(applies(&settings, dir.path(), false));
    }

    #[test]
    fn invalid_settings_file_falls_back_to_on() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SETTINGS_FILE), "not json").unwrap();
        assert_eq!(load_settings(dir.path()), GuidelinesSettings::default());
        // A partial file keeps the defaults for what it leaves out.
        std::fs::write(dir.path().join(SETTINGS_FILE), r#"{"off_repos":[]}"#).unwrap();
        assert!(load_settings(dir.path()).enabled);
    }

    #[test]
    fn global_off_and_workspace_off_switch_them_off() {
        let dir = tempfile::tempdir().unwrap();
        let off = GuidelinesSettings {
            enabled: false,
            off_repos: Vec::new(),
        };
        assert!(!applies(&off, dir.path(), false));
        assert!(!applies(&GuidelinesSettings::default(), dir.path(), true));
    }

    #[test]
    fn excluded_repo_covers_its_worktrees_and_subdirectories() {
        let root = tempfile::tempdir().unwrap();
        let (main, wt) = repo_with_worktree(root.path());
        let other = root.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let settings = GuidelinesSettings {
            enabled: true,
            off_repos: vec![main.to_string_lossy().into_owned()],
        };
        assert!(!applies(&settings, &main, false));
        assert!(!applies(&settings, &main.join("src"), false));
        assert!(!applies(&settings, &wt, false));
        assert!(!applies(&settings, &wt.join("src"), false));
        assert!(applies(&settings, &other, false));
        // A worktree entry means its main repo.
        let by_worktree = GuidelinesSettings {
            enabled: true,
            off_repos: vec![wt.to_string_lossy().into_owned()],
        };
        assert!(!applies(&by_worktree, &main, false));
    }

    #[test]
    fn excluded_repo_matches_a_non_canonical_spelling() {
        let root = tempfile::tempdir().unwrap();
        let (main, _) = repo_with_worktree(root.path());
        let spelled = main.join("src").join("..");
        let settings = GuidelinesSettings {
            enabled: true,
            off_repos: vec![spelled.to_string_lossy().into_owned()],
        };
        assert!(settings.repo_is_off(&main.join("src")));
    }

    #[test]
    fn set_repo_off_stores_the_main_repo_and_toggles_back() {
        let root = tempfile::tempdir().unwrap();
        let (main, wt) = repo_with_worktree(root.path());
        let mut settings = GuidelinesSettings::default();
        settings.set_repo_off(&wt, true);
        assert_eq!(
            settings.off_repos,
            vec![main.to_string_lossy().into_owned()]
        );
        settings.set_repo_off(&main, true);
        assert_eq!(settings.off_repos.len(), 1);
        settings.set_repo_off(&wt, false);
        assert!(settings.off_repos.is_empty());
    }

    #[test]
    fn default_file_is_created_once_and_never_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("config");
        let path = ensure_guidelines_file(&dir).unwrap();
        assert_eq!(path, dir.join(GUIDELINES_FILE));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DEFAULT_GUIDELINES);
        std::fs::write(&path, "edited").unwrap();
        ensure_guidelines_file(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");
    }

    #[test]
    fn for_launch_reads_the_file_and_honours_the_settings() {
        let dir = tempfile::tempdir().unwrap();
        let launch = for_launch(dir.path(), dir.path(), false).unwrap();
        assert_eq!(launch.text, DEFAULT_GUIDELINES);
        assert!(for_launch(dir.path(), dir.path(), true).is_none());
        save_settings(
            dir.path(),
            &GuidelinesSettings {
                enabled: false,
                off_repos: Vec::new(),
            },
        )
        .unwrap();
        assert!(for_launch(dir.path(), dir.path(), false).is_none());
    }

    #[test]
    fn unreadable_or_empty_file_launches_without_them() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be cannot be read as text.
        std::fs::create_dir_all(dir.path().join(GUIDELINES_FILE)).unwrap();
        assert!(for_launch(dir.path(), dir.path(), false).is_none());
        let empty = tempfile::tempdir().unwrap();
        std::fs::write(empty.path().join(GUIDELINES_FILE), "  \n").unwrap();
        assert!(for_launch(empty.path(), empty.path(), false).is_none());
    }

    fn sample() -> Guidelines {
        Guidelines {
            path: PathBuf::from("/cfg/agent-guidelines.md"),
            text: "Say \"why\".\nIt's C:\\path.\n".to_string(),
        }
    }

    #[test]
    fn harness_args() {
        let g = sample();
        assert_eq!(
            claude_args(&g),
            ["--append-system-prompt-file", "/cfg/agent-guidelines.md"]
        );
        assert_eq!(
            pi_args(&g),
            ["--append-system-prompt", "/cfg/agent-guidelines.md"]
        );
        assert_eq!(
            codex_args(&g),
            [
                "-c",
                r#"developer_instructions="Say \"why\".\nIt's C:\\path.\n""#
            ]
        );
    }

    #[test]
    fn terminal_commands_get_the_option_after_the_executable() {
        let g = sample();
        assert_eq!(
            configure_terminal_command("claude --resume abc", &g),
            "claude --append-system-prompt-file /cfg/agent-guidelines.md --resume abc"
        );
        assert_eq!(
            configure_terminal_command("/usr/local/bin/pi", &g),
            "/usr/local/bin/pi --append-system-prompt /cfg/agent-guidelines.md"
        );
        assert_eq!(
            configure_terminal_command("codex -m gpt 'brief'", &g),
            r#"codex -c 'developer_instructions="Say \"why\".\nIt'\''s C:\\path.\n"' -m gpt 'brief'"#
        );
        let spaced = Guidelines {
            path: PathBuf::from("/My Config/agent-guidelines.md"),
            ..sample()
        };
        assert_eq!(
            configure_terminal_command("claude", &spaced),
            "claude --append-system-prompt-file '/My Config/agent-guidelines.md'"
        );
        assert!(starts_a_harness("/opt/bin/codex resume"));
        assert!(!starts_a_harness("zsh"));
        for unchanged in [
            "zsh",
            "gemini",
            "claude --append-system-prompt-file /x.md",
            "codex -c developer_instructions=\"x\"",
        ] {
            assert_eq!(configure_terminal_command(unchanged, &g), unchanged);
        }
    }
}
