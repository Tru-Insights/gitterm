//! Change guidelines GitTerm adds to every agent it starts (TRU-174).
//!
//! The text lives in `<shared config dir>/agent-guidelines.md`, seeded from
//! `assets/agent-guidelines.md` the first time it is missing; the user edits
//! that file. `<shared config dir>/agent-guidelines.json` holds the global
//! switch and the repos they are off in; a workspace can switch them off in
//! `workspaces.json` (`guidelines_off`).
//!
//! Each harness gets them through its own instruction channel:
//! - Claude: `--append-system-prompt-file <path>` (claude 2.1.312 names it in
//!   `--help` and reports a missing file as "Append system prompt file not
//!   found").
//! - pi: `--append-system-prompt <path>` (pi 1.0.4: "Append text or file
//!   contents to the system prompt").
//! - Codex: `-c developer_instructions=<TOML string>` (codex-cli 0.164.0
//!   accepts the key under `--strict-config`, which rejects unknown keys).
//! - Gemini CLI over ACP: a delimited preamble on the first prompt (ACP
//!   `session/new` has no system-instruction field).
//! - OpenCode over ACP: its `instructions` config paths, supplied through
//!   `OPENCODE_CONFIG_CONTENT`.
//! - A remote Claude chat: the desktop sends the text in the remote start;
//!   the agent writes a temporary prompt file for that Claude process.
//!
//! The file is read before every launch: an unreadable one launches the
//! agent without it and is logged. A launch is never blocked.
//!
//! An agent started in a piece of work also gets a short documents note
//! (TRU-191, [`launch_instructions`]) naming the work's docs folder and its
//! decisions repo. It goes in through the same options; for the harnesses
//! that read a file, the guidelines and the note are written together to a
//! per-launch file.
//!
//! Every agent given GitTerm's task tools also gets a short note on
//! starting Codex through them (TRU-212, [`CODEX_TOOLS_NOTE`]), with or
//! without the guidelines and the documents note.

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

/// Where a work workspace's agents keep their documents (TRU-191): its
/// private docs folder in the work's home, and the repo the team's
/// decisions go to, when one is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentPlaces {
    pub workspace_name: String,
    pub repos: Vec<PathBuf>,
    pub docs_dir: PathBuf,
    pub decisions_repo: Option<PathBuf>,
}

/// The document places of an agent starting in a workspace: only a piece
/// of work on this Mac has them. A folder workspace's agents follow the
/// repo's own conventions and are told nothing.
pub fn document_places(
    is_work: bool,
    local: bool,
    home: &Path,
    decisions_repo: Option<&Path>,
    workspace_name: &str,
    repos: &[PathBuf],
) -> Option<DocumentPlaces> {
    (is_work && local).then(|| DocumentPlaces {
        workspace_name: workspace_name.to_string(),
        repos: repos.to_vec(),
        docs_dir: home.join("docs"),
        decisions_repo: decisions_repo.map(Path::to_path_buf),
    })
}

/// The text appended for `places`, naming both locations as paths.
pub fn documents_note(places: &DocumentPlaces) -> String {
    let mut note = format!(
        "# Documents for this work (added by GitTerm)\n\n\
         - Working notes for this work (status, plans, scratch) go in \
         `{}`. It is outside the project repos. When the folder above it is a git repo with a remote, \
         GitTerm commits and pushes it there, so keep secrets out of it.\n",
        places.docs_dir.display()
    );
    note.push_str(&format!(
        "- Workspace: `{}`.\n- Workspace repos: {}.\n",
        places.workspace_name,
        if places.repos.is_empty() {
            "none".to_string()
        } else {
            places
                .repos
                .iter()
                .map(|repo| format!("`{}`", repo.display()))
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    match &places.decisions_repo {
        Some(repo) => note.push_str(&format!(
            "- Decisions the team should see go in the decisions repo, `{}`, \
             following that repo's conventions, and land through a merged PR.\n",
            repo.display()
        )),
        None => note.push_str(
            "- No decisions repo is set for this work: put decisions the team \
             should see where the repository you change keeps them, following \
             its conventions, through a PR.\n",
        ),
    }
    // Last, so the SessionStart hook can re-read it (`workspace_context`).
    note.push_str(&checkpoint_note(&places.docs_dir.join("checkpoint.md")));
    note
}

const CHECKPOINT_LINE: &str = "- Workspace checkpoint: `";

/// The workspace's own checkpoint (TRU-237): repos are shared between
/// workspaces, so a repo's `.agents/checkpoint.md` cannot say whose work it
/// is. Code names the file; the agent only reads and writes it. Its status
/// comes from disk each time, so `/clear` cannot leave it stale.
fn checkpoint_note(resume: &Path) -> String {
    let note = format!(
        "{CHECKPOINT_LINE}{}` ({}). `/checkpoint` writes here and `/resume` \
         reads here, never a repo's `.agents/checkpoint.md`.\n\
         Read it at the start of the chat and after a clear or compaction; resume only this \
         workspace's work; if unsure which work to continue, ask.\n",
        resume.display(),
        if resume.is_file() {
            "exists"
        } else {
            "not written yet"
        }
    );
    note
}

/// The workspace section of appended instructions, also supplied by Claude's
/// SessionStart callback after a clear or compaction. The checkpoint status
/// is read from disk on each call, not taken from the text.
pub fn workspace_context(instructions: &str) -> Option<String> {
    let start = instructions.find("# Documents for this work (added by GitTerm)\n")?;
    let section = &instructions[start..];
    // The checkpoint line ends the note's fixed part; parse its path and
    // rebuild the status and re-read instruction from disk.
    let at = section.find(CHECKPOINT_LINE)?;
    let path_start = at + CHECKPOINT_LINE.len();
    let path_end = path_start + section[path_start..].find('`')?;
    let resume = Path::new(&section[path_start..path_end]);
    let mut context = format!("{}{}", &section[..at], checkpoint_note(resume));
    if resume.is_file() {
        context.push_str(&format!(
            "Re-read the workspace checkpoint `{}` now, before continuing: it may have changed since this chat started.\n",
            resume.display()
        ));
    }
    Some(context.trim_end().to_string())
}

/// How agents in GitTerm start Codex (TRU-212): through GitTerm's task
/// tools, which GitTerm shows, never `codex exec` in a background shell,
/// which it cannot see.
pub const CODEX_TOOLS_NOTE: &str = "# Starting Codex in GitTerm (added by GitTerm)\n\n\
In GitTerm, start Codex workers with GitTerm's task tools: `delegate_task` for implementation, \
`consult_request` for advice and `review_request` for code review. To work in another repository \
or an existing worktree of this workspace, pass `checkout` (or, for `delegate_task`, `repo` to \
make a new worktree from that repository); pass `time_limit_minutes` and `effort` when the job \
needs them. Do not run `codex exec` in a background shell: GitTerm cannot see those runs, so the \
user cannot follow or stop them.\n\
Check `plan_limits` before starting workers or reviewers, follow `routing`, and relay warnings to the user.\n";

/// The instructions a launch carries: the guidelines (when they apply),
/// the documents note (when the workspace has places, TRU-191) and, for an
/// agent given GitTerm's task tools (`task_tools`), [`CODEX_TOOLS_NOTE`]
/// (TRU-212), whatever else is or is not there. With a note, the combined
/// text is written once to a file in `launch_dir` (named by its content,
/// so equal launches share one) for the harnesses that read a file. A file
/// that cannot be written is logged and the launch carries the guidelines
/// alone; a launch is never blocked.
pub fn launch_instructions(
    guidelines: Option<Guidelines>,
    places: Option<&DocumentPlaces>,
    task_tools: bool,
    launch_dir: &Path,
) -> Option<Guidelines> {
    let mut notes = Vec::new();
    if let Some(places) = places {
        notes.push(documents_note(places));
    }
    if task_tools {
        notes.push(CODEX_TOOLS_NOTE.to_string());
    }
    if notes.is_empty() {
        return guidelines;
    }
    let mut text = guidelines
        .as_ref()
        .map(|guidelines| guidelines.text.trim_end().to_string())
        .unwrap_or_default();
    for note in notes {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&note);
    }
    match write_launch_file(launch_dir, &text) {
        Ok(path) => Some(Guidelines { path, text }),
        Err(error) => {
            log(&format!(
                "{error}; starting the agent without GitTerm's notes (documents, Codex tools)"
            ));
            guidelines
        }
    }
}

fn write_launch_file(launch_dir: &Path, text: &str) -> Result<PathBuf, String> {
    let digest = ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    let name: String = digest.as_ref()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let path = launch_dir.join(format!("agent-instructions-{name}.md"));
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == text) {
        return Ok(path);
    }
    std::fs::create_dir_all(launch_dir)
        .map_err(|error| format!("cannot create {}: {error}", launch_dir.display()))?;
    let temporary = launch_dir.join(format!(
        ".agent-instructions-{name}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&temporary, text)
        .and_then(|()| std::fs::rename(&temporary, &path))
        .map_err(|error| {
            let _ = std::fs::remove_file(&temporary);
            format!("cannot write {}: {error}", path.display())
        })?;
    Ok(path)
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
/// `value` as one POSIX shell word: as is when it is plainly safe, else
/// single-quoted.
pub(crate) fn shell_word(value: &str) -> String {
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

    #[test]
    fn workspace_identity_names_only_its_existing_resume_file() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("example-work");
        let repos = vec![
            PathBuf::from("/code/example-api"),
            PathBuf::from("/code/example-web"),
        ];
        let places = document_places(true, true, &home, None, "Example workspace", &repos).unwrap();
        let missing = documents_note(&places);
        assert!(missing.contains("Workspace: `Example workspace`"));
        assert!(missing.contains("`/code/example-api`, `/code/example-web`"));
        assert!(missing
            .contains("resume only this workspace's work; if unsure which work to continue, ask."));
        let checkpoint = places.docs_dir.join("checkpoint.md");
        assert!(missing.contains(&format!(
            "Workspace checkpoint: `{}` (not written yet)",
            checkpoint.display()
        )));
        assert!(
            missing.contains("Read it at the start of the chat and after a clear or compaction")
        );
        assert!(missing.contains("never a repo's `.agents/checkpoint.md`"));
        assert!(missing.contains("It is outside the project repos. When the folder above it is a git repo with a remote, GitTerm commits and pushes it there, so keep secrets out of it."));
        std::fs::create_dir_all(&places.docs_dir).unwrap();
        let marker = "UNIQUE_CHECKPOINT_MARKER_TRU259";
        std::fs::write(&checkpoint, marker).unwrap();
        let note = documents_note(&places);
        assert!(note.contains(&format!(
            "Workspace checkpoint: `{}` (exists)",
            checkpoint.display()
        )));
        assert!(!note.contains(marker));
        let launch =
            launch_instructions(Some(sample()), Some(&places), true, fixture.path()).unwrap();
        assert!(launch.text.contains(&checkpoint.display().to_string()));
        assert!(!launch.text.contains(marker));
        let context = workspace_context(&launch.text).unwrap();
        assert!(context.contains(&checkpoint.display().to_string()));
        assert!(!context.contains(marker));
        assert!(context.contains(&format!("Re-read the workspace checkpoint `{}` now, before continuing: it may have changed since this chat started.", checkpoint.display())));
        assert!(!context.contains("Starting Codex"));
        assert_eq!(workspace_context("Other instructions"), None);

        // The checkpoint status and hook instruction follow disk changes.
        std::fs::remove_file(&checkpoint).unwrap();
        let context = workspace_context(&launch.text).unwrap();
        assert!(context.contains("(not written yet)"), "{context}");
        assert!(!context.contains("Re-read the workspace checkpoint"));
        let launch =
            launch_instructions(Some(sample()), Some(&places), true, fixture.path()).unwrap();
        assert!(launch.text.contains("(not written yet)"));
        std::fs::write(&checkpoint, marker).unwrap();
        let context = workspace_context(&launch.text).unwrap();
        assert!(context.contains("(exists)"), "{context}");
        assert!(context.contains("Re-read the workspace checkpoint"));
        assert!(!context.contains(marker));
    }

    #[test]
    fn work_launches_name_the_docs_folder_and_the_decisions_repo() {
        let home = Path::new("/work/example-work");
        let repo = Path::new("/code/team-decisions");
        let places = document_places(
            true,
            true,
            home,
            Some(repo),
            "Example work",
            &[repo.to_path_buf()],
        )
        .unwrap();
        let launch = tempfile::tempdir().unwrap();
        let combined =
            launch_instructions(Some(sample()), Some(&places), false, launch.path()).unwrap();
        assert!(combined.text.starts_with("Say \"why\"."));
        assert!(combined.text.contains("`/work/example-work/docs`"));
        assert!(combined.text.contains("`/code/team-decisions`"));
        assert!(combined.text.contains("merged PR"));
        // Claude and pi read the file; it holds the same text, and Codex
        // gets the text itself.
        assert!(combined.path.starts_with(launch.path()));
        assert_eq!(
            std::fs::read_to_string(&combined.path).unwrap(),
            combined.text
        );
        assert!(codex_config_value(&combined).contains("example-work/docs"));
        // Equal launches share the file.
        let again =
            launch_instructions(Some(sample()), Some(&places), false, launch.path()).unwrap();
        assert_eq!(again.path, combined.path);

        // Without a decisions repo it says so; with the guidelines off the
        // note still goes in, alone.
        let no_repo = document_places(true, true, home, None, "Example work", &[]).unwrap();
        let alone = launch_instructions(None, Some(&no_repo), false, launch.path()).unwrap();
        assert!(alone.text.starts_with("# Documents for this work"));
        assert!(alone.text.contains("`/work/example-work/docs`"));
        assert!(alone.text.contains("No decisions repo is set"));
    }

    #[test]
    fn agents_with_task_tools_get_the_codex_tools_note_whatever_else_they_get() {
        for word in [
            "delegate_task",
            "consult_request",
            "review_request",
            "`checkout`",
            "`repo`",
            "time_limit_minutes",
            "effort",
            "codex exec",
            "plan_limits",
            "follow `routing`",
            "relay warnings",
        ] {
            assert!(CODEX_TOOLS_NOTE.contains(word), "{word}");
        }
        let launch = tempfile::tempdir().unwrap();
        let places =
            document_places(true, true, Path::new("/work/w"), None, "Example work", &[]).unwrap();

        // Guidelines only, nothing else appended (the coordinator chat of
        // 2026-11-04 had only these): the note follows them.
        let guidelines_only =
            launch_instructions(Some(sample()), None, true, launch.path()).unwrap();
        assert!(guidelines_only.text.starts_with("Say \"why\"."));
        assert!(guidelines_only.text.ends_with(CODEX_TOOLS_NOTE));
        assert!(!guidelines_only.text.contains("# Documents for this work"));
        assert!(guidelines_only.path.starts_with(launch.path()));
        assert_eq!(
            std::fs::read_to_string(&guidelines_only.path).unwrap(),
            guidelines_only.text
        );
        assert!(codex_config_value(&guidelines_only).contains("delegate_task"));

        // Nothing else at all (guidelines off, no documents): the note alone.
        let alone = launch_instructions(None, None, true, launch.path()).unwrap();
        assert_eq!(alone.text, CODEX_TOOLS_NOTE);
        assert_eq!(
            std::fs::read_to_string(&alone.path).unwrap(),
            CODEX_TOOLS_NOTE
        );

        // Everything: guidelines, documents, then the note.
        let all = launch_instructions(Some(sample()), Some(&places), true, launch.path()).unwrap();
        let docs = all.text.find("# Documents for this work").unwrap();
        let codex = all.text.find("# Starting Codex in GitTerm").unwrap();
        assert!(all.text.starts_with("Say \"why\".") && docs < codex);
        assert!(all.text.ends_with(CODEX_TOOLS_NOTE));

        // One file per launch: no half-composed file is left beside it.
        let names: Vec<String> = std::fs::read_dir(launch.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 3, "{names:?}");

        // Without the task tools there is no note.
        let without =
            launch_instructions(Some(sample()), Some(&places), false, launch.path()).unwrap();
        assert!(!without.text.contains("Starting Codex"));
        assert_eq!(launch_instructions(None, None, false, launch.path()), None);
    }

    #[test]
    fn folder_and_remote_workspaces_get_no_documents_note() {
        let home = Path::new("/code/api");
        assert_eq!(
            document_places(false, true, home, None, "Example work", &[]),
            None
        );
        assert_eq!(
            document_places(true, false, home, None, "Example work", &[]),
            None
        );
        let launch = tempfile::tempdir().unwrap();
        assert_eq!(
            launch_instructions(Some(sample()), None, false, launch.path()),
            Some(sample())
        );
        assert_eq!(launch_instructions(None, None, false, launch.path()), None);
        assert_eq!(std::fs::read_dir(launch.path()).unwrap().count(), 0);
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
