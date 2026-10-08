//! Runs Codex headlessly for a delegation (TRU-142 slice S3a; contract pinned
//! in `.plans/agent-handoff-ux.md` §8 and the `consult` addendum).
//!
//! Two kinds of run:
//! - **Review**: `codex exec review <target flag> --json -o <last.md>`. The
//!   findings come from the session rollout (`rollout-*-<thread_id>.jsonl`
//!   under the sessions root), then the rendered final message, then the
//!   whole final message as an unstructured summary.
//! - **Consult**: `codex exec --json --sandbox read-only -o <last.md> -` with
//!   the brief on stdin. The final agent message becomes the handoff summary.
//!
//! Every stdout line and every stderr line (wrapped as a JSON object) is
//! appended to the run's log. The child is killed when the `run` future is
//! dropped. Concurrency limits belong to the caller.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Utc};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::codex_review::{review_findings_from_rollout, review_findings_from_text};
use crate::tasks::{
    DelegationResult, ReviewFindings, ReviewTarget, ReviewTargetMode, ReviewedState, Reviewer,
    TaskHandoff,
};

const CODEX_HARNESS: &str = "codex";
const STDERR_TAIL_BYTES: usize = 2048;
const ACTIVITY_MAX_CHARS: usize = 120;
const TERMINAL_EVENT_MAX_CHARS: usize = 500;
/// Today plus the two days before it (local and UTC calendars).
const ROLLOUT_SEARCH_DAYS: i64 = 3;
/// Repository-location variables git exports to hooks. A runner started
/// from a hook (or a test run under `cargo test` in a pre-commit hook) would
/// otherwise point git and Codex at the hook's repository and index instead
/// of `CodexRun::cwd`.
const GIT_LOCATION_VARS: [&str; 7] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
];
const CONSULT_SECTIONS: [ConsultSection; 3] = [
    ConsultSection::Decisions,
    ConsultSection::NextSteps,
    ConsultSection::Blockers,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexRunKind {
    Review(ReviewTarget),
    Consult { brief: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRun {
    pub kind: CodexRunKind,
    /// The checkout Codex runs in. Must be a git work tree with a commit.
    pub cwd: PathBuf,
    /// `-m <model>`; `None` keeps Codex's configured default.
    pub model: Option<String>,
    /// JSONL log: stdout lines verbatim, stderr lines as
    /// `{"type":"gitterm.stderr","line":...}`. Codex writes the final message
    /// next to it with the extension `last.md`.
    pub log_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexRunEvent {
    Started { thread_id: String },
    Activity { description: String },
    Completed,
    Failed { message: String },
}

/// `turn.completed.usage`. All zeros for reviews in codex-cli 0.161.0, so
/// do not report cost from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub struct CodexUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_output_tokens: u64,
}

/// Where a review's findings were read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingsSource {
    Rollout(PathBuf),
    /// The rendered final message (structured or not, see
    /// `ReviewFindings::structured`).
    FinalMessage,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodexRunOutcome {
    pub thread_id: String,
    pub final_text: String,
    pub result: DelegationResult,
    /// Set for reviews.
    pub findings_source: Option<FindingsSource>,
    /// Why the rollout was not used, when a review fell back to the text.
    pub rollout_note: Option<String>,
    pub usage: Option<CodexUsage>,
    /// Non-empty stdout lines that were not JSON events.
    pub skipped_lines: usize,
}

#[derive(Debug)]
pub enum CodexRunError {
    CodexNotFound {
        search_path: String,
    },
    Git {
        cwd: PathBuf,
        message: String,
    },
    Io {
        context: String,
        source: std::io::Error,
    },
    NonZeroExit {
        status: String,
        stderr_tail: String,
    },
    IncompleteStream {
        reason: String,
        stderr_tail: String,
    },
}

impl fmt::Display for CodexRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CodexNotFound { search_path } => write!(
                formatter,
                "could not find the `codex` executable on PATH ({search_path})"
            ),
            Self::Git { cwd, message } => {
                write!(formatter, "git failed in {}: {message}", cwd.display())
            }
            Self::Io { context, source } => write!(formatter, "{context}: {source}"),
            Self::NonZeroExit {
                status,
                stderr_tail,
            } => write!(
                formatter,
                "codex {status}; stderr: {}",
                or_none(stderr_tail)
            ),
            Self::IncompleteStream {
                reason,
                stderr_tail,
            } => write!(formatter, "{reason}; stderr: {}", or_none(stderr_tail)),
        }
    }
}

impl std::error::Error for CodexRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn or_none(text: &str) -> &str {
    if text.trim().is_empty() {
        "(empty)"
    } else {
        text
    }
}

/// Where to look for `codex` and its session rollouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexEnvironment {
    /// PATH used both to find `codex` and as the child's PATH.
    pub search_path: OsString,
    /// `$CODEX_HOME/sessions`, else `~/.codex/sessions`.
    pub sessions_root: PathBuf,
}

impl CodexEnvironment {
    pub fn detect() -> Result<Self, CodexRunError> {
        let codex_home = match std::env::var_os("CODEX_HOME") {
            Some(home) if !home.is_empty() => PathBuf::from(home),
            _ => dirs::home_dir()
                .ok_or_else(|| CodexRunError::Io {
                    context: "locate the Codex sessions directory".to_string(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "neither CODEX_HOME nor a home directory is set",
                    ),
                })?
                .join(".codex"),
        };
        Ok(Self {
            search_path: child_path(),
            sessions_root: codex_home.join("sessions"),
        })
    }
}

/// Mirrors `harness::claude::extra_bin_dirs` (private there).
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

/// Mirrors `harness::claude::child_path`: the inherited PATH plus the usual
/// install dirs.
fn child_path() -> OsString {
    let mut entries: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    for dir in extra_bin_dirs() {
        if !entries.contains(&dir) {
            entries.push(dir);
        }
    }
    std::env::join_paths(entries).unwrap_or_default()
}

fn codex_program_names() -> &'static [&'static str] {
    if cfg!(windows) {
        &["codex.exe", "codex.cmd", "codex"]
    } else {
        &["codex"]
    }
}

fn resolve_codex(search_path: &OsString) -> Result<PathBuf, CodexRunError> {
    std::env::split_paths(search_path)
        .flat_map(|dir| codex_program_names().iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| CodexRunError::CodexNotFound {
            search_path: search_path.to_string_lossy().into_owned(),
        })
}

/// The child's PATH: `search_path` plus the directory holding `codex`, so a
/// Node shim installed by nvm finds the `node` next to it.
fn child_search_path(search_path: &OsString, program: &Path) -> OsString {
    let mut entries: Vec<PathBuf> = std::env::split_paths(search_path).collect();
    if let Some(dir) = program.parent() {
        if !entries.iter().any(|entry| entry == dir) {
            entries.push(dir.to_path_buf());
        }
    }
    std::env::join_paths(entries).unwrap_or_else(|_| search_path.clone())
}

/// Runs Codex with the detected environment. Sends `Started`, `Activity`
/// and finally `Completed` or `Failed` on `progress`.
pub async fn run(
    run: CodexRun,
    progress: mpsc::UnboundedSender<CodexRunEvent>,
) -> Result<CodexRunOutcome, CodexRunError> {
    match CodexEnvironment::detect() {
        Ok(environment) => run_with(run, progress, &environment).await,
        Err(error) => {
            send(
                &progress,
                CodexRunEvent::Failed {
                    message: error.to_string(),
                },
            );
            Err(error)
        }
    }
}

/// [`run`] with an explicit environment (tests use a fake `codex` and a
/// temporary sessions root).
pub async fn run_with(
    run: CodexRun,
    progress: mpsc::UnboundedSender<CodexRunEvent>,
    environment: &CodexEnvironment,
) -> Result<CodexRunOutcome, CodexRunError> {
    let result = run_inner(&run, &progress, environment).await;
    match &result {
        Ok(_) => send(&progress, CodexRunEvent::Completed),
        Err(error) => send(
            &progress,
            CodexRunEvent::Failed {
                message: error.to_string(),
            },
        ),
    }
    result
}

/// Progress is advisory: a caller that dropped its receiver no longer wants
/// it, and the outcome is still returned.
fn send(progress: &mpsc::UnboundedSender<CodexRunEvent>, event: CodexRunEvent) {
    let _ = progress.send(event);
}

async fn run_inner(
    run: &CodexRun,
    progress: &mpsc::UnboundedSender<CodexRunEvent>,
    environment: &CodexEnvironment,
) -> Result<CodexRunOutcome, CodexRunError> {
    let program = resolve_codex(&environment.search_path)?;
    let git = git_state(&run.cwd).await?;
    let last_message_path = run.log_path.with_extension("last.md");
    let (args, stdin_text) = codex_args(run, &last_message_path);

    if let Some(parent) = run.log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| CodexRunError::Io {
            context: format!("create the log directory {}", parent.display()),
            source,
        })?;
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&run.log_path)
        .map_err(|source| CodexRunError::Io {
            context: format!("open the Codex log {}", run.log_path.display()),
            source,
        })?;
    let log = LogWriter {
        path: run.log_path.clone(),
        file: Mutex::new(log_file),
    };

    let mut command = tokio::process::Command::new(&program);
    command
        .args(&args)
        .current_dir(&run.cwd)
        .env(
            "PATH",
            child_search_path(&environment.search_path, &program),
        )
        .stdin(if stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for var in GIT_LOCATION_VARS {
        command.env_remove(var);
    }
    let mut child = command.spawn().map_err(|source| CodexRunError::Io {
        context: format!("start {} in {}", program.display(), run.cwd.display()),
        source,
    })?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().ok_or_else(|| missing_pipe("stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| missing_pipe("stderr"))?;

    let write_stdin = async {
        match (stdin, stdin_text) {
            (Some(mut stdin), Some(text)) => {
                stdin.write_all(text.as_bytes()).await?;
                stdin.shutdown().await
            }
            _ => Ok(()),
        }
    };
    let read_stdout = async {
        let mut stream = StreamState::default();
        for_each_line(stdout, |line| {
            log.append(line)?;
            for event in stream.handle_line(line) {
                send(progress, event);
            }
            Ok(())
        })
        .await?;
        Ok::<_, CodexRunError>(stream)
    };
    let read_stderr = async {
        let mut tail = StderrTail::default();
        for_each_line(stderr, |line| {
            log.append(&serde_json::json!({"type": "gitterm.stderr", "line": line}).to_string())?;
            tail.push(line);
            Ok(())
        })
        .await?;
        Ok::<_, CodexRunError>(tail)
    };
    let (stdin_result, stream, stderr_tail) = tokio::join!(write_stdin, read_stdout, read_stderr);
    let status = child.wait().await.map_err(|source| CodexRunError::Io {
        context: "wait for codex to exit".to_string(),
        source,
    })?;
    let stream = stream?;
    let stderr_tail = stderr_tail?.into_string();

    if !status.success() {
        return Err(CodexRunError::NonZeroExit {
            status: describe_status(&status),
            stderr_tail,
        });
    }
    stdin_result.map_err(|source| CodexRunError::Io {
        context: "write the brief to codex's stdin".to_string(),
        source,
    })?;
    if !stream.turn_completed {
        let mut reason = "codex exited without turn.completed".to_string();
        if let Some(event) = &stream.terminal_error {
            reason.push_str(&format!(" (last error event: {event})"));
        }
        return Err(CodexRunError::IncompleteStream {
            reason,
            stderr_tail,
        });
    }
    let Some(thread_id) = stream.thread_id.clone() else {
        return Err(CodexRunError::IncompleteStream {
            reason: "codex completed without thread.started".to_string(),
            stderr_tail,
        });
    };
    let final_text = match stream.final_text.clone() {
        Some(text) => text,
        None => std::fs::read_to_string(&last_message_path).map_err(|source| {
            CodexRunError::IncompleteStream {
                reason: format!(
                    "codex completed without a final agent message, and {} is unreadable: {source}",
                    last_message_path.display()
                ),
                stderr_tail: stderr_tail.clone(),
            }
        })?,
    };

    let reviewer = Reviewer {
        harness: CODEX_HARNESS.to_string(),
        model: run.model.clone(),
        conversation_id: Some(thread_id.clone()),
    };
    match &run.kind {
        CodexRunKind::Review(target) => {
            let review = review_from_sources(
                &thread_id,
                &final_text,
                &environment.sessions_root,
                &run.cwd,
                reviewer,
                Utc::now(),
            );
            Ok(CodexRunOutcome {
                thread_id,
                final_text,
                result: DelegationResult {
                    handoff: None,
                    findings: Some(review.findings),
                    reviewed: Some(ReviewedState {
                        head: git.head,
                        dirty: git.dirty,
                        target_description: describe_target(target),
                    }),
                },
                findings_source: Some(review.source),
                rollout_note: review.rollout_note,
                usage: stream.usage,
                skipped_lines: stream.skipped_lines,
            })
        }
        CodexRunKind::Consult { brief } => {
            if final_text.trim().is_empty() {
                return Err(CodexRunError::IncompleteStream {
                    reason: "codex completed the consult with an empty final message".to_string(),
                    stderr_tail,
                });
            }
            Ok(CodexRunOutcome {
                thread_id,
                result: DelegationResult {
                    handoff: Some(consult_handoff(brief, &final_text, Utc::now())),
                    findings: None,
                    reviewed: Some(ReviewedState {
                        head: git.head,
                        dirty: git.dirty,
                        target_description: "consult".to_string(),
                    }),
                },
                final_text,
                findings_source: None,
                rollout_note: None,
                usage: stream.usage,
                skipped_lines: stream.skipped_lines,
            })
        }
    }
}

fn missing_pipe(name: &str) -> CodexRunError {
    CodexRunError::Io {
        context: format!("capture codex {name}"),
        source: std::io::Error::other("the pipe was not created"),
    }
}

fn describe_status(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exited with code {code}"),
        None => format!("terminated without an exit code ({status})"),
    }
}

/// The command line after the program, and the text to pipe to stdin.
fn codex_args(run: &CodexRun, last_message_path: &Path) -> (Vec<OsString>, Option<String>) {
    let mut args: Vec<OsString> = vec!["exec".into()];
    let mut stdin_text = None;
    match &run.kind {
        CodexRunKind::Review(target) => {
            args.push("review".into());
            match &target.focus {
                // `[PROMPT]` and a target flag are mutually exclusive (clap
                // error, rc 2), so a focused review states the target in its
                // custom instructions, read from stdin via `-`.
                Some(focus) => stdin_text = Some(focused_review_prompt(&target.mode, focus)),
                None => args.extend(target_flags(&target.mode)),
            }
        }
        CodexRunKind::Consult { brief } => {
            // A consult advises; it must not edit the checkout.
            args.extend(["--sandbox".into(), "read-only".into()]);
            stdin_text = Some(brief.clone());
        }
    }
    args.push("--json".into());
    args.push("-o".into());
    args.push(last_message_path.as_os_str().to_os_string());
    if let Some(model) = &run.model {
        args.push("-m".into());
        args.push(model.into());
    }
    if stdin_text.is_some() {
        args.push("-".into());
    }
    (args, stdin_text)
}

fn target_flags(mode: &ReviewTargetMode) -> Vec<OsString> {
    match mode {
        ReviewTargetMode::Uncommitted => vec!["--uncommitted".into()],
        ReviewTargetMode::Base { reference } => vec!["--base".into(), reference.into()],
        ReviewTargetMode::Commit { sha } => vec!["--commit".into(), sha.into()],
    }
}

fn focused_review_prompt(mode: &ReviewTargetMode, focus: &str) -> String {
    let scope = match mode {
        ReviewTargetMode::Uncommitted => {
            "the staged, unstaged and untracked changes against HEAD".to_string()
        }
        ReviewTargetMode::Base { reference } => format!(
            "the changes between the merge base with `{reference}` and the working tree, \
             including uncommitted changes"
        ),
        ReviewTargetMode::Commit { sha } => {
            format!("the changes introduced by commit `{sha}`")
        }
    };
    format!("Review {scope}.\n\nFocus: {}", focus.trim())
}

fn describe_target(target: &ReviewTarget) -> String {
    let scope = match &target.mode {
        ReviewTargetMode::Uncommitted => "uncommitted changes".to_string(),
        ReviewTargetMode::Base { reference } => {
            format!("changes against {reference} (including uncommitted changes)")
        }
        ReviewTargetMode::Commit { sha } => format!("commit {sha}"),
    };
    match &target.focus {
        Some(focus) => format!("{scope}; focus: {}", focus.trim()),
        None => scope,
    }
}

struct LogWriter {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl LogWriter {
    fn append(&self, line: &str) -> Result<(), CodexRunError> {
        let mut file = self.file.lock().map_err(|_| CodexRunError::Io {
            context: format!("write the Codex log {}", self.path.display()),
            source: std::io::Error::other("log lock poisoned"),
        })?;
        writeln!(file, "{line}").map_err(|source| CodexRunError::Io {
            context: format!("write the Codex log {}", self.path.display()),
            source,
        })
    }
}

/// Reads lines (lossy UTF-8, without the newline) until EOF.
async fn for_each_line<R, F>(reader: R, mut on_line: F) -> Result<(), CodexRunError>
where
    R: AsyncRead + Unpin,
    F: FnMut(&str) -> Result<(), CodexRunError>,
{
    let mut reader = BufReader::new(reader);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let read = reader
            .read_until(b'\n', &mut buffer)
            .await
            .map_err(|source| CodexRunError::Io {
                context: "read codex output".to_string(),
                source,
            })?;
        if read == 0 {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&buffer);
        on_line(text.trim_end_matches(['\n', '\r']))?;
    }
}

/// Keeps the last [`STDERR_TAIL_BYTES`] of stderr.
#[derive(Debug, Default)]
struct StderrTail {
    text: String,
}

impl StderrTail {
    fn push(&mut self, line: &str) {
        self.text.push_str(line);
        self.text.push('\n');
        if self.text.len() > STDERR_TAIL_BYTES * 2 {
            self.trim();
        }
    }

    fn trim(&mut self) {
        if self.text.len() <= STDERR_TAIL_BYTES {
            return;
        }
        let mut start = self.text.len() - STDERR_TAIL_BYTES;
        while !self.text.is_char_boundary(start) {
            start += 1;
        }
        self.text.drain(..start);
    }

    fn into_string(mut self) -> String {
        self.trim();
        self.text.trim_end().to_string()
    }
}

/// The parts of `codex exec --json` stdout the runner relies on (§8):
/// `thread.started.thread_id`, the last `agent_message` text,
/// `turn.completed` and its usage. Item ids and command output are ignored.
#[derive(Debug, Default)]
struct StreamState {
    thread_id: Option<String>,
    final_text: Option<String>,
    usage: Option<CodexUsage>,
    turn_completed: bool,
    /// A `turn.failed` or `error` event, verbatim (truncated).
    terminal_error: Option<String>,
    skipped_lines: usize,
    announced_items: HashSet<String>,
}

#[derive(Debug, Deserialize)]
struct StreamLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    item: Option<StreamItem>,
    #[serde(default)]
    usage: Option<CodexUsage>,
}

#[derive(Debug, Deserialize)]
struct StreamItem {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    command: Option<String>,
}

impl StreamState {
    fn handle_line(&mut self, line: &str) -> Vec<CodexRunEvent> {
        if line.trim().is_empty() {
            return Vec::new();
        }
        let Ok(parsed) = serde_json::from_str::<StreamLine>(line) else {
            self.skipped_lines += 1;
            return Vec::new();
        };
        match parsed.kind.as_str() {
            "thread.started" => match parsed.thread_id {
                Some(thread_id) => {
                    self.thread_id = Some(thread_id.clone());
                    vec![CodexRunEvent::Started { thread_id }]
                }
                None => {
                    self.skipped_lines += 1;
                    Vec::new()
                }
            },
            "item.started" | "item.completed" => {
                let Some(item) = parsed.item else {
                    self.skipped_lines += 1;
                    return Vec::new();
                };
                if item.kind == "agent_message" {
                    if parsed.kind == "item.completed" {
                        if let Some(text) = item.text {
                            self.final_text = Some(text);
                        }
                    }
                    return Vec::new();
                }
                // Announce each item once, when it is first seen.
                if let Some(id) = &item.id {
                    if !self.announced_items.insert(id.clone()) {
                        return Vec::new();
                    }
                }
                activity_label(&item)
                    .map(|description| vec![CodexRunEvent::Activity { description }])
                    .unwrap_or_default()
            }
            "turn.completed" => {
                self.turn_completed = true;
                self.usage = parsed.usage;
                Vec::new()
            }
            "turn.failed" | "error" => {
                self.terminal_error = Some(truncate_chars(line, TERMINAL_EVENT_MAX_CHARS));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

/// A short label for an item. Only `command_execution` has verified fields
/// (§8); other item types are named by their type.
fn activity_label(item: &StreamItem) -> Option<String> {
    let label = match item.kind.as_str() {
        "command_execution" => {
            let command = item.command.as_deref()?;
            let inner = unwrap_shell(command);
            let first_line = inner.lines().next().unwrap_or(inner).trim();
            if first_line.is_empty() {
                return None;
            }
            first_line.to_string()
        }
        "reasoning" => "Thinking".to_string(),
        other => other.replace('_', " "),
    };
    Some(truncate_chars(&label, ACTIVITY_MAX_CHARS))
}

/// `/bin/zsh -lc 'git diff'` -> `git diff`.
fn unwrap_shell(command: &str) -> &str {
    for flag in [" -lc ", " -c "] {
        if let Some((shell, rest)) = command.split_once(flag) {
            if shell.starts_with('/') && !shell.contains(' ') {
                let rest = rest.trim();
                for quote in ['\'', '"'] {
                    if let Some(inner) = rest
                        .strip_prefix(quote)
                        .and_then(|value| value.strip_suffix(quote))
                    {
                        return inner;
                    }
                }
                return rest;
            }
        }
    }
    command
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('\u{2026}');
    truncated
}

struct GitState {
    head: String,
    dirty: bool,
}

async fn git_state(cwd: &Path) -> Result<GitState, CodexRunError> {
    let head = git_output(cwd, &["rev-parse", "HEAD"]).await?;
    let status = git_output(cwd, &["status", "--porcelain"]).await?;
    Ok(GitState {
        head: head.trim().to_string(),
        dirty: !status.trim().is_empty(),
    })
}

async fn git_output(cwd: &Path, args: &[&str]) -> Result<String, CodexRunError> {
    let mut command = tokio::process::Command::new("git");
    command.args(args).current_dir(cwd).stdin(Stdio::null());
    for var in GIT_LOCATION_VARS {
        command.env_remove(var);
    }
    let output = command
        .output()
        .await
        .map_err(|source| CodexRunError::Git {
            cwd: cwd.to_path_buf(),
            message: format!("could not run `git {}`: {source}", args.join(" ")),
        })?;
    if !output.status.success() {
        return Err(CodexRunError::Git {
            cwd: cwd.to_path_buf(),
            message: format!(
                "`git {}` {}: {}",
                args.join(" "),
                describe_status(&output.status),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Finds `rollout-*-<thread_id>.jsonl` in the `YYYY/MM/DD` directories of
/// the last [`ROLLOUT_SEARCH_DAYS`] days (local and UTC calendars, since the
/// directory follows Codex's local start time). Missing day directories are
/// skipped.
pub fn find_rollout(
    sessions_root: &Path,
    thread_id: &str,
    now: DateTime<Utc>,
) -> std::io::Result<Option<PathBuf>> {
    let mut days: Vec<NaiveDate> = Vec::new();
    for offset in 0..ROLLOUT_SEARCH_DAYS {
        let instant = now - Duration::days(offset);
        for day in [
            instant.with_timezone(&Local).date_naive(),
            instant.date_naive(),
        ] {
            if !days.contains(&day) {
                days.push(day);
            }
        }
    }
    for day in days {
        let dir = sessions_root
            .join(format!("{:04}", day.year()))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("read {}: {error}", dir.display()),
                ))
            }
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_rollout_for(&name, thread_id) {
                return Ok(Some(entry.path()));
            }
        }
    }
    Ok(None)
}

/// `rollout-<YYYY-MM-DDTHH-MM-SS>-<thread_id>.jsonl`. The start time must
/// be digits, `-` and `T`, so `other-<thread_id>` never matches.
fn is_rollout_for(file_name: &str, thread_id: &str) -> bool {
    let Some(stem) = file_name
        .strip_prefix("rollout-")
        .and_then(|rest| rest.strip_suffix(".jsonl"))
        .and_then(|rest| rest.strip_suffix(thread_id))
        .and_then(|rest| rest.strip_suffix('-'))
    else {
        return false;
    };
    !stem.is_empty()
        && stem
            .chars()
            .all(|character| character.is_ascii_digit() || character == '-' || character == 'T')
}

struct ReviewFromSources {
    findings: ReviewFindings,
    source: FindingsSource,
    rollout_note: Option<String>,
}

/// The §8 fallback chain: rollout, then the rendered final message, which
/// `review_findings_from_text` itself falls back to as an unstructured
/// summary.
fn review_from_sources(
    thread_id: &str,
    final_text: &str,
    sessions_root: &Path,
    repo_root: &Path,
    reviewer: Reviewer,
    now: DateTime<Utc>,
) -> ReviewFromSources {
    let rollout_note = match find_rollout(sessions_root, thread_id, now) {
        Ok(Some(path)) => match std::fs::read_to_string(&path) {
            Ok(rollout) => {
                match review_findings_from_rollout(&rollout, repo_root, reviewer.clone()) {
                    Ok(findings) => {
                        return ReviewFromSources {
                            findings,
                            source: FindingsSource::Rollout(path),
                            rollout_note: None,
                        }
                    }
                    Err(error) => format!("{}: {error}", path.display()),
                }
            }
            Err(error) => format!("read {}: {error}", path.display()),
        },
        Ok(None) => format!(
            "no rollout for thread {thread_id} under {} in the last {ROLLOUT_SEARCH_DAYS} days",
            sessions_root.display()
        ),
        Err(error) => format!("search for the rollout of thread {thread_id}: {error}"),
    };
    ReviewFromSources {
        findings: review_findings_from_text(final_text, repo_root, reviewer),
        source: FindingsSource::FinalMessage,
        rollout_note: Some(rollout_note),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsultSection {
    Decisions,
    NextSteps,
    Blockers,
}

impl ConsultSection {
    fn heading(self) -> &'static str {
        match self {
            Self::Decisions => "Decisions",
            Self::NextSteps => "Next steps",
            Self::Blockers => "Blockers",
        }
    }
}

/// The consult handoff: the final message is the summary. When the brief
/// names one of the headings `Decisions`, `Next steps` or `Blockers`, the
/// matching sections of the reply are read into lists and the summary is
/// the text before the first of them.
fn consult_handoff(brief: &str, final_text: &str, now: DateTime<Utc>) -> TaskHandoff {
    let requested: Vec<ConsultSection> = CONSULT_SECTIONS
        .into_iter()
        .filter(|section| brief.contains(section.heading()))
        .collect();
    let mut handoff = TaskHandoff {
        summary: final_text.trim().to_string(),
        decisions: Vec::new(),
        next_steps: Vec::new(),
        blockers: Vec::new(),
        updated_by_session_id: None,
        updated_at: now.to_rfc3339(),
    };
    if requested.is_empty() {
        return handoff;
    }

    let mut preamble: Vec<&str> = Vec::new();
    let mut current: Option<Option<ConsultSection>> = None;
    for line in final_text.lines() {
        if let Some(heading) = heading_text(line) {
            current = Some(
                requested
                    .iter()
                    .copied()
                    .find(|section| heading == section.heading()),
            );
            continue;
        }
        match current {
            None => preamble.push(line),
            Some(Some(section)) => {
                let item = strip_list_marker(line.trim());
                if !item.is_empty() {
                    match section {
                        ConsultSection::Decisions => handoff.decisions.push(item.to_string()),
                        ConsultSection::NextSteps => handoff.next_steps.push(item.to_string()),
                        ConsultSection::Blockers => handoff.blockers.push(item.to_string()),
                    }
                }
            }
            // A section the brief did not ask for: not summary, not a list.
            Some(None) => {}
        }
    }
    let preamble = preamble.join("\n").trim().to_string();
    if !preamble.is_empty() {
        handoff.summary = preamble;
    }
    handoff
}

/// `## Decisions`, `**Decisions**`, `Decisions:` -> `Decisions`. Returns
/// `None` for lines that are not headings.
fn heading_text(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix('#') {
        let text = rest.trim_start_matches('#').trim();
        return Some(text.trim_end_matches(':').trim());
    }
    if let Some(inner) = trimmed
        .strip_prefix("**")
        .and_then(|value| value.strip_suffix("**"))
    {
        return Some(inner.trim().trim_end_matches(':').trim());
    }
    let without_colon = trimmed.strip_suffix(':')?;
    CONSULT_SECTIONS
        .iter()
        .any(|section| without_colon == section.heading())
        .then_some(without_colon)
}

fn strip_list_marker(line: &str) -> &str {
    for marker in ["- ", "* ", "\u{2022} "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return rest.trim();
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        if let Some(rest) = line[digits..]
            .strip_prefix(". ")
            .or_else(|| line[digits..].strip_prefix(") "))
        {
            return rest.trim();
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::ReviewVerdict;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex");
    const REVIEW_STREAMS: [&str; 6] = [
        "uncommitted",
        "base",
        "commit",
        "schema",
        "ephemeral",
        "clean",
    ];

    fn fixture(name: &str) -> String {
        let path = Path::new(FIXTURES).join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read fixture {}: {error}", path.display()))
    }

    fn parse_stream(text: &str) -> (StreamState, Vec<CodexRunEvent>) {
        let mut state = StreamState::default();
        let mut events = Vec::new();
        for line in text.lines() {
            events.extend(state.handle_line(line));
        }
        (state, events)
    }

    fn reviewer(thread_id: &str) -> Reviewer {
        Reviewer {
            harness: CODEX_HARNESS.to_string(),
            model: None,
            conversation_id: Some(thread_id.to_string()),
        }
    }

    fn fixture_thread_id(name: &str) -> String {
        parse_stream(&fixture(&format!("review-{name}.jsonl")))
            .0
            .thread_id
            .expect("fixture has a thread id")
    }

    fn day_dir(root: &Path, day: NaiveDate) -> PathBuf {
        root.join(format!("{:04}", day.year()))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()))
    }

    fn plant_rollout(root: &Path, day: NaiveDate, thread_id: &str, content: &str) -> PathBuf {
        let dir = day_dir(root, day);
        std::fs::create_dir_all(&dir).expect("create day dir");
        let path = dir.join(format!("rollout-2026-10-08T10-44-19-{thread_id}.jsonl"));
        std::fs::write(&path, content).expect("write rollout");
        path
    }

    #[test]
    fn stream_parser_reads_every_review_fixture() {
        for name in REVIEW_STREAMS {
            let stream = fixture(&format!("review-{name}.jsonl"));
            let (state, events) = parse_stream(&stream);
            let thread_id = state.thread_id.clone().expect("thread id");
            assert_eq!(thread_id.len(), 36, "{name}: {thread_id}");
            assert_eq!(
                events.first(),
                Some(&CodexRunEvent::Started {
                    thread_id: thread_id.clone()
                }),
                "{name}"
            );
            let activities: Vec<&str> = events
                .iter()
                .filter_map(|event| match event {
                    CodexRunEvent::Activity { description } => Some(description.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(activities.len(), 3, "{name}: {activities:?}");
            for description in &activities {
                assert!(!description.is_empty(), "{name}");
                assert!(
                    !description.starts_with("/bin/zsh"),
                    "{name}: {description}"
                );
                assert!(description.chars().count() <= ACTIVITY_MAX_CHARS, "{name}");
            }
            assert!(state.turn_completed, "{name}");
            assert_eq!(state.usage, Some(CodexUsage::default()), "{name}");
            assert_eq!(state.skipped_lines, 0, "{name}");
            let final_text = state.final_text.clone().expect("final text");
            assert!(!final_text.trim().is_empty(), "{name}");
            if !matches!(name, "schema" | "ephemeral") {
                assert_eq!(
                    final_text,
                    fixture(&format!("review-{name}.last.md")),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn stream_parser_counts_unparseable_lines_and_keeps_going() {
        let stream = fixture("review-uncommitted.jsonl");
        let mut lines: Vec<&str> = stream.lines().collect();
        lines.insert(1, "not json");
        lines.insert(3, "{\"no_type\":true}");
        lines.insert(4, "");
        let (state, _) = parse_stream(&lines.join("\n"));
        assert_eq!(state.skipped_lines, 2);
        assert!(state.turn_completed);
        assert!(state.final_text.is_some());
    }

    #[test]
    fn stream_parser_records_terminal_error_events_without_completing() {
        let (state, _) = parse_stream(
            "{\"type\":\"thread.started\",\"thread_id\":\"t\"}\n{\"type\":\"turn.failed\",\"error\":{}}",
        );
        assert!(!state.turn_completed);
        assert!(state
            .terminal_error
            .as_deref()
            .is_some_and(|event| event.contains("turn.failed")));
    }

    #[test]
    fn activity_labels_unwrap_the_shell() {
        assert_eq!(
            unwrap_shell("/bin/zsh -lc 'git diff HEAD'"),
            "git diff HEAD"
        );
        assert_eq!(
            unwrap_shell("/bin/bash -c \"python3 -B x.py\""),
            "python3 -B x.py"
        );
        assert_eq!(unwrap_shell("git status"), "git status");
    }

    #[test]
    fn find_rollout_searches_the_last_three_days_only() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = Utc::now();
        let today = now.with_timezone(&Local).date_naive();
        let recent = plant_rollout(root.path(), today - Duration::days(2), "thread-a", "{}");
        plant_rollout(root.path(), today - Duration::days(5), "thread-old", "{}");
        plant_rollout(root.path(), today, "other-thread-a", "{}");

        assert_eq!(
            find_rollout(root.path(), "thread-a", now).expect("search"),
            Some(recent)
        );
        assert_eq!(
            find_rollout(root.path(), "thread-old", now).expect("search"),
            None
        );
        assert_eq!(
            find_rollout(root.path(), "missing", now).expect("search"),
            None
        );
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            find_rollout(&empty.path().join("absent"), "thread-a", now).expect("search"),
            None
        );
    }

    #[test]
    fn review_prefers_the_rollout() {
        let root = tempfile::tempdir().expect("tempdir");
        let thread_id = fixture_thread_id("base");
        let now = Utc::now();
        let path = plant_rollout(
            root.path(),
            now.with_timezone(&Local).date_naive(),
            &thread_id,
            &fixture("review-base.rollout.jsonl"),
        );
        let review = review_from_sources(
            &thread_id,
            &fixture("review-base.last.md"),
            root.path(),
            Path::new("/scratch/repo"),
            reviewer(&thread_id),
            now,
        );
        assert_eq!(review.source, FindingsSource::Rollout(path));
        assert_eq!(review.rollout_note, None);
        assert!(review.findings.structured);
        assert_eq!(review.findings.verdict, ReviewVerdict::NeedsChanges);
        assert_eq!(review.findings.findings.len(), 3);
        assert!(review.findings.findings[0].confidence.is_some());
        assert_eq!(
            review.findings.reviewer.conversation_id.as_deref(),
            Some(thread_id.as_str())
        );
    }

    #[test]
    fn review_falls_back_to_the_rendered_text_then_the_whole_text() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = Utc::now();
        let thread_id = fixture_thread_id("base");

        let missing = review_from_sources(
            &thread_id,
            &fixture("review-base.last.md"),
            root.path(),
            Path::new("/scratch/repo"),
            reviewer(&thread_id),
            now,
        );
        assert_eq!(missing.source, FindingsSource::FinalMessage);
        assert!(missing
            .rollout_note
            .as_deref()
            .is_some_and(|note| note.contains("no rollout")));
        assert!(missing.findings.structured);
        assert_eq!(missing.findings.findings.len(), 3);
        // Text findings carry no confidence: proves the rollout was not used.
        assert!(missing.findings.findings[0].confidence.is_none());
        assert_eq!(
            missing.findings.findings[0].file.as_deref(),
            Some(Path::new("stats.py"))
        );

        plant_rollout(
            root.path(),
            now.with_timezone(&Local).date_naive(),
            &thread_id,
            "not json\n",
        );
        let corrupt = review_from_sources(
            &thread_id,
            "Looks fine overall, but see my notes in prose.",
            root.path(),
            Path::new("/scratch/repo"),
            reviewer(&thread_id),
            now,
        );
        assert_eq!(corrupt.source, FindingsSource::FinalMessage);
        assert!(corrupt
            .rollout_note
            .as_deref()
            .is_some_and(|note| note.contains("not JSON")));
        assert!(!corrupt.findings.structured);
        assert!(corrupt.findings.findings.is_empty());
        assert_eq!(
            corrupt.findings.summary,
            "Looks fine overall, but see my notes in prose."
        );
    }

    #[test]
    fn consult_without_requested_sections_keeps_the_whole_message() {
        let text = "It is a terminal.\n\n## Decisions\n- none";
        let handoff = consult_handoff(
            "In two sentences, what does this repository do?",
            text,
            Utc::now(),
        );
        assert_eq!(handoff.summary, text);
        assert!(handoff.decisions.is_empty());
        assert!(handoff.next_steps.is_empty());
        assert!(handoff.blockers.is_empty());
    }

    #[test]
    fn consult_reads_the_sections_the_brief_asked_for() {
        let brief = "Assess the plan. Answer with sections Decisions, Next steps and Blockers.";
        let text = "Use option B.\nIt is smaller.\n\n## Decisions\n- Take option B\n- Keep the store\n\n**Next steps:**\n1. Write S3\n2) Wire S4\n\nBlockers:\n* Caller identity\n\n## Appendix\nignored";
        let handoff = consult_handoff(brief, text, Utc::now());
        assert_eq!(handoff.summary, "Use option B.\nIt is smaller.");
        assert_eq!(handoff.decisions, vec!["Take option B", "Keep the store"]);
        assert_eq!(handoff.next_steps, vec!["Write S3", "Wire S4"]);
        assert_eq!(handoff.blockers, vec!["Caller identity"]);
    }

    #[test]
    fn stderr_tail_keeps_the_last_two_kilobytes() {
        let mut tail = StderrTail::default();
        for index in 0..500 {
            tail.push(&format!("line {index} \u{00e9}"));
        }
        let text = tail.into_string();
        assert!(text.len() <= STDERR_TAIL_BYTES);
        assert!(text.ends_with("line 499 \u{00e9}"));
    }

    #[test]
    fn args_map_targets_focus_and_consults() {
        let last = Path::new("/logs/d1.last.md");
        let review = |mode, focus: Option<&str>| CodexRun {
            kind: CodexRunKind::Review(ReviewTarget {
                mode,
                focus: focus.map(str::to_string),
            }),
            cwd: PathBuf::from("/repo"),
            model: None,
            log_path: PathBuf::from("/logs/d1.jsonl"),
        };
        let strings = |args: Vec<OsString>| -> Vec<String> {
            args.into_iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };

        let (args, stdin) = codex_args(&review(ReviewTargetMode::Uncommitted, None), last);
        assert_eq!(
            strings(args),
            [
                "exec",
                "review",
                "--uncommitted",
                "--json",
                "-o",
                "/logs/d1.last.md"
            ]
        );
        assert_eq!(stdin, None);

        let (args, _) = codex_args(
            &review(
                ReviewTargetMode::Base {
                    reference: "main".into(),
                },
                None,
            ),
            last,
        );
        assert_eq!(&strings(args)[..4], ["exec", "review", "--base", "main"]);

        let (args, _) = codex_args(
            &review(
                ReviewTargetMode::Commit {
                    sha: "abc123".into(),
                },
                None,
            ),
            last,
        );
        assert_eq!(
            &strings(args)[..4],
            ["exec", "review", "--commit", "abc123"]
        );

        let (args, stdin) = codex_args(
            &review(ReviewTargetMode::Uncommitted, Some("error handling")),
            last,
        );
        assert_eq!(
            strings(args),
            ["exec", "review", "--json", "-o", "/logs/d1.last.md", "-"]
        );
        let stdin = stdin.expect("focused review prompt");
        assert!(stdin.contains("untracked changes against HEAD"));
        assert!(stdin.ends_with("Focus: error handling"));

        let consult = CodexRun {
            kind: CodexRunKind::Consult {
                brief: "Why?".into(),
            },
            cwd: PathBuf::from("/repo"),
            model: Some("gpt-6-astra".into()),
            log_path: PathBuf::from("/logs/d1.jsonl"),
        };
        let (args, stdin) = codex_args(&consult, last);
        assert_eq!(
            strings(args),
            [
                "exec",
                "--sandbox",
                "read-only",
                "--json",
                "-o",
                "/logs/d1.last.md",
                "-m",
                "gpt-6-astra",
                "-"
            ]
        );
        assert_eq!(stdin.as_deref(), Some("Why?"));
    }

    #[cfg(unix)]
    mod process {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        struct FakeCodex {
            dir: tempfile::TempDir,
            repo: tempfile::TempDir,
            sessions: tempfile::TempDir,
        }

        impl FakeCodex {
            /// A `codex` script that records its argv and stdin, prints
            /// `stdout`, writes a benign stderr line and exits with `code`.
            fn new(stdout: &str, code: i32) -> Self {
                let dir = tempfile::tempdir().expect("tempdir");
                let stdout_path = dir.path().join("stdout.jsonl");
                std::fs::write(&stdout_path, stdout).expect("write stdout fixture");
                let script = format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{dir}/args'\ncat > '{dir}/stdin'\ncat '{out}'\necho 'ERROR codex_models_manager::manager: failed to refresh available models: request timed out' >&2\nexit {code}\n",
                    dir = dir.path().display(),
                    out = stdout_path.display(),
                );
                let program = dir.path().join("codex");
                std::fs::write(&program, script).expect("write fake codex");
                std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod fake codex");
                Self {
                    dir,
                    repo: scratch_repo(),
                    sessions: tempfile::tempdir().expect("tempdir"),
                }
            }

            fn environment(&self) -> CodexEnvironment {
                CodexEnvironment {
                    search_path: std::env::join_paths([
                        self.dir.path(),
                        Path::new("/bin"),
                        Path::new("/usr/bin"),
                    ])
                    .expect("join paths"),
                    sessions_root: self.sessions.path().to_path_buf(),
                }
            }

            fn run(&self, kind: CodexRunKind) -> CodexRun {
                CodexRun {
                    kind,
                    cwd: self.repo.path().to_path_buf(),
                    model: None,
                    log_path: self.dir.path().join("logs").join("d1.jsonl"),
                }
            }

            fn recorded(&self, name: &str) -> String {
                std::fs::read_to_string(self.dir.path().join(name)).expect("recorded file")
            }
        }

        fn git(repo: &Path, args: &[&str]) -> String {
            let mut command = std::process::Command::new("git");
            for var in GIT_LOCATION_VARS {
                command.env_remove(var);
            }
            for (var, value) in [
                ("GIT_AUTHOR_NAME", "Test"),
                ("GIT_AUTHOR_EMAIL", "test@example.com"),
                ("GIT_COMMITTER_NAME", "Test"),
                ("GIT_COMMITTER_EMAIL", "test@example.com"),
            ] {
                command.env(var, value);
            }
            let output = command
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo)
                .output()
                .expect("run git");
            assert!(output.status.success(), "git {args:?}: {output:?}");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        fn scratch_repo() -> tempfile::TempDir {
            let repo = tempfile::tempdir().expect("tempdir");
            git(repo.path(), &["init", "-q"]);
            std::fs::write(repo.path().join("stats.py"), "x = 1\n").expect("write");
            git(repo.path(), &["add", "."]);
            git(repo.path(), &["commit", "-q", "-m", "init"]);
            std::fs::write(repo.path().join("stats.py"), "x = 2\n").expect("write");
            repo
        }

        async fn run_collect(
            fake: &FakeCodex,
            run: CodexRun,
        ) -> (Result<CodexRunOutcome, CodexRunError>, Vec<CodexRunEvent>) {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let result = run_with(run, sender, &fake.environment()).await;
            let mut events = Vec::new();
            while let Ok(event) = receiver.try_recv() {
                events.push(event);
            }
            (result, events)
        }

        #[tokio::test]
        async fn review_run_uses_the_rollout_and_logs_both_streams() {
            let fake = FakeCodex::new(&fixture("review-uncommitted.jsonl"), 0);
            let thread_id = fixture_thread_id("uncommitted");
            plant_rollout(
                fake.sessions.path(),
                Local::now().date_naive(),
                &thread_id,
                &fixture("review-uncommitted.rollout.jsonl"),
            );
            let head = git(fake.repo.path(), &["rev-parse", "HEAD"]);
            let run = fake.run(CodexRunKind::Review(ReviewTarget {
                mode: ReviewTargetMode::Uncommitted,
                focus: None,
            }));
            let log_path = run.log_path.clone();

            let (result, events) = run_collect(&fake, run).await;
            let outcome = result.expect("review succeeds despite stderr noise");

            assert_eq!(outcome.thread_id, thread_id);
            assert!(matches!(
                outcome.findings_source,
                Some(FindingsSource::Rollout(_))
            ));
            let findings = outcome.result.findings.expect("findings");
            assert!(findings.structured);
            assert_eq!(findings.findings.len(), 1);
            assert_eq!(
                outcome.result.reviewed,
                Some(ReviewedState {
                    head,
                    dirty: true,
                    target_description: "uncommitted changes".to_string(),
                })
            );
            assert!(outcome.result.handoff.is_none());

            let args = fake.recorded("args");
            let args: Vec<&str> = args.lines().collect();
            assert_eq!(
                &args[..5],
                ["exec", "review", "--uncommitted", "--json", "-o"]
            );
            assert!(args[5].ends_with("d1.last.md"));

            assert_eq!(
                events.first(),
                Some(&CodexRunEvent::Started {
                    thread_id: thread_id.clone()
                })
            );
            assert_eq!(events.last(), Some(&CodexRunEvent::Completed));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, CodexRunEvent::Activity { .. }))
                    .count(),
                3
            );

            let log = std::fs::read_to_string(log_path).expect("log");
            let log_lines: Vec<&str> = log.lines().collect();
            assert_eq!(log_lines.len(), 11, "10 stdout lines + 1 stderr line");
            assert!(log_lines.iter().any(
                |line| line.contains("gitterm.stderr") && line.contains("codex_models_manager")
            ));
        }

        #[tokio::test]
        async fn consult_run_pipes_the_brief_and_returns_a_handoff() {
            let stream = concat!(
                "{\"type\":\"thread.started\",\"thread_id\":\"thread-consult\"}\n",
                "{\"type\":\"turn.started\"}\n",
                "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",\"type\":\"agent_message\",\"text\":\"It is a terminal. It has tabs.\"}}\n",
                "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":12,\"output_tokens\":7}}\n",
            );
            let fake = FakeCodex::new(stream, 0);
            let head = git(fake.repo.path(), &["rev-parse", "HEAD"]);
            let mut run = fake.run(CodexRunKind::Consult {
                brief: "In two sentences, what does this repository do?".to_string(),
            });
            run.model = Some("gpt-6-astra".to_string());

            let (result, events) = run_collect(&fake, run).await;
            let outcome = result.expect("consult succeeds");

            let handoff = outcome.result.handoff.expect("handoff");
            assert_eq!(handoff.summary, "It is a terminal. It has tabs.");
            assert!(handoff.decisions.is_empty());
            assert!(outcome.result.findings.is_none());
            assert_eq!(outcome.result.reviewed.map(|state| state.head), Some(head));
            assert_eq!(
                outcome.usage,
                Some(CodexUsage {
                    input_tokens: 12,
                    output_tokens: 7,
                    ..CodexUsage::default()
                })
            );
            assert_eq!(
                fake.recorded("stdin"),
                "In two sentences, what does this repository do?"
            );
            let args = fake.recorded("args");
            let args: Vec<&str> = args.lines().collect();
            assert_eq!(&args[..4], ["exec", "--sandbox", "read-only", "--json"]);
            assert_eq!(&args[args.len() - 3..], ["-m", "gpt-6-astra", "-"]);
            assert_eq!(events.last(), Some(&CodexRunEvent::Completed));
        }

        #[tokio::test]
        async fn missing_codex_fails_before_spawning() {
            let fake = FakeCodex::new("", 0);
            let empty = tempfile::tempdir().expect("tempdir");
            let environment = CodexEnvironment {
                search_path: std::env::join_paths([empty.path()]).expect("join"),
                sessions_root: fake.sessions.path().to_path_buf(),
            };
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let result = run_with(
                fake.run(CodexRunKind::Consult {
                    brief: "hi".to_string(),
                }),
                sender,
                &environment,
            )
            .await;
            assert!(matches!(result, Err(CodexRunError::CodexNotFound { .. })));
            assert!(matches!(
                receiver.try_recv(),
                Ok(CodexRunEvent::Failed { message }) if message.contains("could not find the `codex`")
            ));
        }

        #[tokio::test]
        async fn non_zero_exit_fails_with_the_stderr_tail() {
            let fake = FakeCodex::new(&fixture("review-uncommitted.jsonl"), 3);
            let (result, events) = run_collect(
                &fake,
                fake.run(CodexRunKind::Review(ReviewTarget {
                    mode: ReviewTargetMode::Uncommitted,
                    focus: None,
                })),
            )
            .await;
            match result {
                Err(CodexRunError::NonZeroExit {
                    status,
                    stderr_tail,
                }) => {
                    assert_eq!(status, "exited with code 3");
                    assert!(stderr_tail.contains("codex_models_manager"));
                }
                other => panic!("expected NonZeroExit, got {other:?}"),
            }
            assert!(matches!(events.last(), Some(CodexRunEvent::Failed { .. })));
        }

        #[tokio::test]
        async fn a_stream_without_turn_completed_fails() {
            let stream = fixture("review-uncommitted.jsonl");
            let truncated: Vec<&str> = stream
                .lines()
                .filter(|line| !line.contains("turn.completed"))
                .collect();
            let fake = FakeCodex::new(&(truncated.join("\n") + "\nnot json\n"), 0);
            let (result, events) = run_collect(
                &fake,
                fake.run(CodexRunKind::Review(ReviewTarget {
                    mode: ReviewTargetMode::Uncommitted,
                    focus: None,
                })),
            )
            .await;
            assert!(
                matches!(&result, Err(CodexRunError::IncompleteStream { reason, .. }) if reason.contains("turn.completed")),
                "{result:?}"
            );
            assert!(matches!(events.last(), Some(CodexRunEvent::Failed { .. })));
        }

        #[tokio::test]
        async fn a_non_git_cwd_fails_before_spawning() {
            let fake = FakeCodex::new("", 0);
            let plain = tempfile::tempdir().expect("tempdir");
            let mut run = fake.run(CodexRunKind::Consult {
                brief: "hi".to_string(),
            });
            run.cwd = plain.path().to_path_buf();
            let (result, _) = run_collect(&fake, run).await;
            assert!(
                matches!(result, Err(CodexRunError::Git { .. })),
                "{result:?}"
            );
            assert!(!fake.dir.path().join("args").exists());
        }
    }
}
