//! `codex exec` runs a chat started outside GitTerm's tools (TRU-212).
//!
//! A chat that runs `codex exec …` in a (background) Bash starts Codex as a
//! descendant of its own `claude` process: `claude` → `zsh` → `timeout` →
//! `node …/bin/codex exec` → the native `codex` binary. GitTerm cannot
//! stop or adopt such a run, but it can show it: the [`Watcher`] walks
//! each local Claude chat's process tree on GitTerm's 5-second tick (off
//! the UI thread), keeps the native `codex` processes whose subcommand is
//! `exec`, and maps each to the rollout it holds open
//! (`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl`).
//!
//! Discovery is macOS-only and uses `libproc` directly, no `ps` or
//! `lsof`: `proc_listchildpids` for the tree, `PROC_PIDTBSDINFO` for each
//! process's name and start, `KERN_PROCARGS2` for the arguments of a
//! `codex` process (once per process), and `PROC_PIDLISTFDS` +
//! `PROC_PIDFDVNODEPATHINFO` for its open files.
//!
//! A rollout is never read whole (some are hundreds of megabytes): its
//! first line (`session_meta`) once, its last 64 KiB (1 MiB when that
//! holds no activity) when it has grown. Codex subagents a run spawns
//! (`source.subagent.thread_spawn`) are rollouts of their own in the same
//! day folder, written after the run started; their first lines are read
//! once each.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

/// The most of a rollout's first line read for its `session_meta`.
const HEAD_LIMIT: u64 = 1024 * 1024;
/// The tail read for the latest activity, then the larger retry.
const TAIL_BYTES: u64 = 64 * 1024;
const TAIL_RETRY_BYTES: u64 = 1024 * 1024;
/// Longest activity text sent to the page.
const ACTIVITY_CHARS: usize = 240;

fn log(msg: &str) {
    eprintln!("[codex-exec] {msg}");
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// ---------------------------------------------------------------------------
// Command lines
// ---------------------------------------------------------------------------

/// `codex` options (global and `exec`) that take the next argument as
/// their value, so it is not mistaken for the subcommand.
const VALUE_OPTIONS: &[&str] = &[
    "-c",
    "--config",
    "-C",
    "--cd",
    "-m",
    "--model",
    "-p",
    "--profile",
    "-s",
    "--sandbox",
    "-a",
    "--ask-for-approval",
    "-i",
    "--image",
    "--add-dir",
    "--enable",
    "--disable",
    "--local-provider",
    "--output-schema",
    "-o",
    "--output-last-message",
    "--color",
];

/// The index of the subcommand: the first positional argument after
/// `argv[0]`, skipping options and their values.
fn subcommand_index(argv: &[String]) -> Option<usize> {
    let mut i = 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        if arg == "--" {
            return None;
        }
        if arg.len() > 1 && arg.starts_with('-') {
            i += if VALUE_OPTIONS.contains(&arg) { 2 } else { 1 };
            continue;
        }
        return Some(i);
    }
    None
}

/// Whether `argv` is a `codex exec` run: the `codex` executable itself
/// (not the `node` wrapper that starts it) with `exec` (or its alias `e`)
/// as the subcommand. `codex app-server`, `codex mcp-server` and an
/// interactive `codex "prompt"` are not.
pub fn is_exec_run(argv: &[String]) -> bool {
    argv.first().is_some_and(|exe| basename(exe) == "codex")
        && subcommand_index(argv).is_some_and(|i| matches!(argv[i].as_str(), "exec" | "e"))
}

/// What an exec run's command line says about where it works.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ExecArgs {
    /// `-C` / `--cd`.
    pub cd: Option<String>,
    /// Every `--add-dir`.
    pub add_dirs: Vec<String>,
    /// `-s` / `--sandbox`.
    pub sandbox: Option<String>,
    /// `-m` / `--model`.
    pub model: Option<String>,
}

pub fn parse_exec_args(argv: &[String]) -> ExecArgs {
    let mut args = ExecArgs::default();
    let mut i = 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
            _ => (arg, None),
        };
        let takes_value = VALUE_OPTIONS.contains(&name);
        let value = if !takes_value {
            None
        } else if inline.is_some() {
            inline
        } else {
            i += 1;
            argv.get(i).cloned()
        };
        if let Some(value) = value {
            match name {
                "-C" | "--cd" => args.cd = Some(value),
                "--add-dir" => args.add_dirs.push(value),
                "-s" | "--sandbox" => args.sandbox = Some(value),
                "-m" | "--model" => args.model = Some(value),
                _ => {}
            }
        }
        i += 1;
    }
    args
}

// ---------------------------------------------------------------------------
// Rollouts
// ---------------------------------------------------------------------------

/// Where a rollout's thread came from (`session_meta.payload.source`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RolloutSource {
    /// `"exec"`: a `codex exec` run's own thread.
    Exec,
    /// `{"subagent": {"thread_spawn": {…}}}`: a subagent a thread spawned.
    Subagent {
        parent_thread_id: String,
        depth: Option<u64>,
        agent_path: Option<String>,
        agent_nickname: Option<String>,
    },
    /// Anything else, as its JSON.
    Other(String),
}

/// A rollout's first line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    /// The thread id (`payload.id`).
    pub id: String,
    pub cwd: Option<String>,
    pub originator: Option<String>,
    pub source: RolloutSource,
    pub cli_version: Option<String>,
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Parse a rollout's `session_meta` line.
pub fn parse_session_meta(line: &str) -> Result<SessionMeta, String> {
    let value: Value =
        serde_json::from_str(line).map_err(|e| format!("first line is not JSON: {e}"))?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Err(format!(
            "first line is {:?}, not session_meta",
            value.get("type")
        ));
    }
    let payload = value.get("payload").ok_or("session_meta has no payload")?;
    let id = str_field(payload, "id").ok_or("session_meta has no payload.id")?;
    let source = match payload.get("source") {
        Some(Value::String(s)) if s == "exec" => RolloutSource::Exec,
        Some(Value::String(s)) => RolloutSource::Other(s.clone()),
        Some(source) => match source.pointer("/subagent/thread_spawn") {
            Some(spawn) => RolloutSource::Subagent {
                parent_thread_id: str_field(spawn, "parent_thread_id")
                    .ok_or("thread_spawn source has no parent_thread_id")?,
                depth: spawn.get("depth").and_then(Value::as_u64),
                agent_path: str_field(spawn, "agent_path"),
                agent_nickname: str_field(spawn, "agent_nickname"),
            },
            None => RolloutSource::Other(source.to_string()),
        },
        None => RolloutSource::Other("none".to_string()),
    };
    Ok(SessionMeta {
        id,
        cwd: str_field(payload, "cwd"),
        originator: str_field(payload, "originator"),
        source,
        cli_version: str_field(payload, "cli_version"),
    })
}

/// A rollout's `session_meta`, from its first line only. `Ok(None)` while
/// the first line is not complete yet (a file just created).
pub fn read_session_meta(path: &Path) -> Result<Option<SessionMeta>, String> {
    let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut line = Vec::new();
    BufReader::new(file)
        .take(HEAD_LIMIT)
        .read_until(b'\n', &mut line)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if line.last() != Some(&b'\n') {
        if line.len() as u64 >= HEAD_LIMIT {
            return Err(format!(
                "{}: first line is longer than {HEAD_LIMIT} bytes",
                path.display()
            ));
        }
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&line);
    parse_session_meta(text.trim_end())
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Whether `path` names a rollout file (`rollout-*.jsonl`).
pub fn is_rollout_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
}

/// When a rollout was created, from its name
/// (`rollout-2026-11-04T08-30-41-<id>.jsonl`, local time), in Unix ms.
pub fn rollout_name_time_ms(path: &Path) -> Option<i64> {
    use chrono::TimeZone;
    let name = path.file_name()?.to_str()?;
    let stamp = name.strip_prefix("rollout-")?.get(..19)?;
    let naive = chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H-%M-%S").ok()?;
    chrono::Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|t| t.timestamp_millis())
}

/// What a thread did last, as one line for the page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Activity {
    pub text: String,
    /// When the rollout line was written (Unix ms), if it says.
    pub at_ms: Option<i64>,
}

/// What the tail of a rollout says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TailSummary {
    pub activity: Option<Activity>,
    /// The newest turn event is `task_complete` (no `task_started` after).
    pub completed: bool,
}

fn one_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else {
        let cut: String = collapsed.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn text_parts(content: Option<&Value>, kinds: &[&str]) -> Option<String> {
    let parts: Vec<&str> = content?
        .as_array()?
        .iter()
        .filter(|part| {
            part.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| kinds.contains(&t))
        })
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// The activity one rollout line records, if it is one the page shows: an
/// agent message, a tool call or command, a file change, a subagent start,
/// or the turn's end.
pub fn activity_of(line: &Value) -> Option<String> {
    let payload = line.get("payload")?;
    let kind = payload.get("type").and_then(Value::as_str);
    match (line.get("type").and_then(Value::as_str)?, kind?) {
        ("response_item", "message")
            if payload.get("role").and_then(Value::as_str) == Some("assistant") =>
        {
            text_parts(payload.get("content"), &["output_text"])
                .map(|text| one_line(&text, ACTIVITY_CHARS))
        }
        ("response_item", "function_call") => {
            let name = str_field(payload, "name")?;
            let args: Option<Value> = payload
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|a| serde_json::from_str(a).ok());
            let what = args.as_ref().and_then(|args| {
                ["task_name", "target", "cmd", "path"]
                    .iter()
                    .find_map(|key| args.get(*key).and_then(Value::as_str))
                    .map(str::to_string)
            });
            Some(match what {
                Some(what) => format!("{name} {}", one_line(&what, ACTIVITY_CHARS)),
                None => name,
            })
        }
        ("response_item", "custom_tool_call") => {
            let name = str_field(payload, "name")?;
            Some(match payload.get("input").and_then(Value::as_str) {
                Some(input) if !input.trim().is_empty() => {
                    format!("{name}: {}", one_line(input, ACTIVITY_CHARS))
                }
                _ => name,
            })
        }
        ("event_msg", "item_completed") => {
            let item = payload.get("item")?;
            match item.get("type").and_then(Value::as_str)? {
                "AgentMessage" => text_parts(item.get("content"), &["Text"])
                    .map(|text| one_line(&text, ACTIVITY_CHARS)),
                "CommandExecution" => {
                    let command: Vec<&str> = item
                        .get("command")?
                        .as_array()?
                        .iter()
                        .filter_map(Value::as_str)
                        .collect();
                    // `["/bin/zsh", "-lc", "<script>"]`: the script.
                    let shown = match command.as_slice() {
                        [_, flag, script] if flag.starts_with('-') => script.to_string(),
                        _ => command.join(" "),
                    };
                    Some(format!("Ran {}", one_line(&shown, ACTIVITY_CHARS)))
                }
                "FileChange" => {
                    let mut files: Vec<&str> = item
                        .get("changes")?
                        .as_object()?
                        .keys()
                        .map(|path| basename(path))
                        .collect();
                    files.sort_unstable();
                    Some(one_line(
                        &format!("Edited {}", files.join(", ")),
                        ACTIVITY_CHARS,
                    ))
                }
                "SubAgentActivity"
                    if item.get("kind").and_then(Value::as_str) == Some("started") =>
                {
                    Some(format!(
                        "Started subagent {}",
                        str_field(item, "agent_path").unwrap_or_else(|| "?".to_string())
                    ))
                }
                _ => None,
            }
        }
        ("event_msg", "task_complete") => Some(match str_field(payload, "last_agent_message") {
            Some(message) => format!("Finished: {}", one_line(&message, ACTIVITY_CHARS)),
            None => "Finished".to_string(),
        }),
        _ => None,
    }
}

fn line_time_ms(line: &Value) -> Option<i64> {
    let stamp = line.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// The newest activity in a rollout tail, and whether its turn completed.
/// `partial_first` drops the first line, cut by where the tail starts.
pub fn summarize_tail(tail: &[u8], partial_first: bool) -> TailSummary {
    let text = String::from_utf8_lossy(tail);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if partial_first && !lines.is_empty() {
        lines.remove(0);
    }
    let mut summary = TailSummary::default();
    let mut turn_seen = false;
    for line in lines.iter().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            // The line being written while we read.
            continue;
        };
        if !turn_seen && value.get("type").and_then(Value::as_str) == Some("event_msg") {
            match value.pointer("/payload/type").and_then(Value::as_str) {
                Some("task_complete") => {
                    summary.completed = true;
                    turn_seen = true;
                }
                Some("task_started") => turn_seen = true,
                _ => {}
            }
        }
        if summary.activity.is_none() {
            if let Some(text) = activity_of(&value) {
                summary.activity = Some(Activity {
                    text,
                    at_ms: line_time_ms(&value),
                });
            }
        }
        if summary.activity.is_some() && turn_seen {
            break;
        }
    }
    summary
}

fn read_tail(path: &Path, bytes: u64) -> std::io::Result<(Vec<u8>, bool)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    file.take(bytes).read_to_end(&mut buf)?;
    Ok((buf, start > 0))
}

/// The summary of a rollout's tail: its last 64 KiB, or 1 MiB when those
/// hold no activity (a long command output or diff).
pub fn read_tail_summary(path: &Path) -> Result<TailSummary, String> {
    let read =
        |bytes| read_tail(path, bytes).map_err(|e| format!("cannot read {}: {e}", path.display()));
    let (tail, partial) = read(TAIL_BYTES)?;
    let summary = summarize_tail(&tail, partial);
    if summary.activity.is_some() || !partial {
        return Ok(summary);
    }
    let (tail, partial) = read(TAIL_RETRY_BYTES)?;
    Ok(summarize_tail(&tail, partial))
}

// ---------------------------------------------------------------------------
// What the page gets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Finished,
}

/// A Codex subagent of an exec run, as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubagentView {
    pub thread_id: String,
    pub nickname: Option<String>,
    pub agent_path: Option<String>,
    pub depth: Option<u64>,
    /// `running`, or `done` once its turn completed or the run ended.
    pub state: &'static str,
    pub activity: Option<Activity>,
    pub rollout_path: String,
}

/// An exec run, as the page shows it (`{"kind":"codex_exec","run":…}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunView {
    /// `<pid>-<start ms>`: a pid reused later is another run.
    pub key: String,
    pub pid: u32,
    pub state: RunState,
    pub started_at_ms: i64,
    /// When GitTerm saw the process gone (within one poll of its exit).
    pub ended_at_ms: Option<i64>,
    /// Where it works: `-C`, else the rollout's `cwd`.
    pub cwd: Option<String>,
    /// The last component of `cwd`.
    pub checkout: Option<String>,
    pub add_dirs: Vec<String>,
    pub sandbox: Option<String>,
    pub model: Option<String>,
    pub rollout_path: Option<String>,
    pub thread_id: Option<String>,
    pub cli_version: Option<String>,
    pub activity: Option<Activity>,
    pub subagents: Vec<SubagentView>,
}

// ---------------------------------------------------------------------------
// The watcher
// ---------------------------------------------------------------------------

/// A process as discovery sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    /// The kernel's short name (`p_comm`, 16 bytes).
    pub comm: String,
    pub start_ms: i64,
}

/// A local Claude chat: its tab and its `claude` process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Root {
    pub tab_id: usize,
    pub pid: u32,
}

/// What discovery needs from the OS; the real one is [`Libproc`].
pub trait Processes {
    /// `pid` and every process below it, `pid` excluded.
    fn descendants(&self, pid: u32) -> Result<Vec<ProcInfo>, String>;
    fn argv(&self, pid: u32) -> Result<Vec<String>, String>;
    fn open_files(&self, pid: u32) -> Result<Vec<PathBuf>, String>;
}

type RunKey = (u32, i64);

#[derive(Debug, Default)]
struct TailCache {
    len: Option<u64>,
    summary: TailSummary,
}

impl TailCache {
    /// Re-read the tail when the file has grown or shrunk since last time.
    fn refresh(&mut self, path: &Path) -> Result<(), String> {
        let len = std::fs::metadata(path)
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?
            .len();
        if self.len == Some(len) {
            return Ok(());
        }
        self.summary = read_tail_summary(path)?;
        self.len = Some(len);
        Ok(())
    }
}

#[derive(Debug)]
struct Sub {
    path: PathBuf,
    meta: SessionMeta,
    tail: TailCache,
}

#[derive(Debug)]
struct Run {
    tab_id: usize,
    pid: u32,
    started_at_ms: i64,
    args: ExecArgs,
    rollout: Option<(PathBuf, SessionMeta, TailCache)>,
    subs: Vec<Sub>,
    /// Rollout heads read for this run: `None` for one not to look at
    /// again (unreadable, or not this run's). Heads never change.
    heads: HashMap<PathBuf, Option<SessionMeta>>,
}

/// The `codex exec` runs under each local Claude chat, between polls.
#[derive(Debug, Default)]
pub struct Watcher {
    runs: HashMap<RunKey, Run>,
    /// `codex` processes that are not exec runs (`app-server`, …).
    not_runs: HashSet<RunKey>,
    /// Failures already logged, by what failed, so each is logged once.
    logged: HashSet<String>,
}

impl Watcher {
    /// Whether any run is being followed (a poll is due to see it end
    /// even when no chat is left).
    pub fn has_runs(&self) -> bool {
        !self.runs.is_empty()
    }

    fn log_once(&mut self, what: &str, error: &str) {
        if self.logged.insert(what.to_string()) {
            log(&format!("{what}: {error} (logged once)"));
        }
    }

    /// One poll: the runs under `roots` now, as page payloads by tab. A
    /// running run is sent every poll (a rebuilt page gets it back); one
    /// whose process is gone is sent once, finished, and forgotten.
    pub fn poll(
        &mut self,
        procs: &dyn Processes,
        roots: &[Root],
        now_ms: i64,
    ) -> Vec<(usize, RunView)> {
        let mut seen: HashSet<RunKey> = HashSet::new();
        let mut codex_seen: HashSet<RunKey> = HashSet::new();
        for root in roots {
            let descendants = match procs.descendants(root.pid) {
                Ok(list) => list,
                Err(e) => {
                    self.log_once("listing a chat's processes", &e);
                    continue;
                }
            };
            for proc in descendants.into_iter().filter(|p| p.comm == "codex") {
                let key = (proc.pid, proc.start_ms);
                codex_seen.insert(key);
                if self.not_runs.contains(&key) {
                    continue;
                }
                if !self.runs.contains_key(&key) {
                    let argv = match procs.argv(proc.pid) {
                        Ok(argv) => argv,
                        Err(e) => {
                            self.log_once("reading a codex process's arguments", &e);
                            continue;
                        }
                    };
                    if !is_exec_run(&argv) {
                        self.not_runs.insert(key);
                        continue;
                    }
                    self.runs.insert(
                        key,
                        Run {
                            tab_id: root.tab_id,
                            pid: proc.pid,
                            started_at_ms: proc.start_ms,
                            args: parse_exec_args(&argv),
                            rollout: None,
                            subs: Vec::new(),
                            heads: HashMap::new(),
                        },
                    );
                }
                if let Some(run) = self.runs.get_mut(&key) {
                    run.tab_id = root.tab_id;
                }
                seen.insert(key);
            }
        }
        self.not_runs.retain(|key| codex_seen.contains(key));

        let mut out = Vec::new();
        let keys: Vec<RunKey> = self.runs.keys().copied().collect();
        for key in keys {
            let Some(mut run) = self.runs.remove(&key) else {
                continue;
            };
            let live = seen.contains(&key);
            let mut errors = Vec::new();
            if live {
                refresh_run(&mut run, procs, &mut errors);
            } else {
                // One last look at what it did before it ended.
                if let Some((path, _, tail)) = run.rollout.as_mut() {
                    if let Err(e) = tail.refresh(path) {
                        errors.push(("reading a rollout's tail", e));
                    }
                }
            }
            for (what, error) in errors {
                self.log_once(what, &error);
            }
            out.push((run.tab_id, view(&run, live, now_ms)));
            if live {
                self.runs.insert(key, run);
            }
        }
        out
    }
}

fn refresh_run(run: &mut Run, procs: &dyn Processes, errors: &mut Vec<(&'static str, String)>) {
    // The run's rollouts: the ones its process holds open. Codex opens
    // the run's own when the thread starts; until then, try again.
    match procs.open_files(run.pid) {
        Ok(files) => {
            for path in files.into_iter().filter(|p| is_rollout_path(p)) {
                if let Some(meta) = head(run, &path, errors) {
                    adopt(run, path, meta);
                }
            }
        }
        Err(e) => errors.push(("listing a codex process's open files", e)),
    }
    let Some(run_path) = run.rollout.as_ref().map(|(path, _, _)| path.clone()) else {
        return;
    };
    // Subagents written since the run started, in its day folder and
    // today's (a run past midnight).
    let mut dirs: Vec<PathBuf> = run_path
        .parent()
        .map(Path::to_path_buf)
        .into_iter()
        .collect();
    if let Some(root) = run_path.ancestors().nth(4) {
        let today = root.join(chrono::Local::now().format("%Y/%m/%d").to_string());
        if !dirs.contains(&today) {
            dirs.push(today);
        }
    }
    let mut candidates: Vec<(i64, PathBuf)> = Vec::new();
    for dir in dirs {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                errors.push((
                    "listing a Codex sessions folder",
                    format!("{}: {e}", dir.display()),
                ));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if run.heads.contains_key(&path) || !is_rollout_path(&path) {
                continue;
            }
            // Names carry seconds; allow for the second the run began in.
            match rollout_name_time_ms(&path) {
                Some(at) if at + 1000 >= run.started_at_ms => candidates.push((at, path)),
                _ => {}
            }
        }
    }
    // Oldest first, so a parent is known before the subagents it spawned.
    candidates.sort();
    for (_, path) in candidates {
        if let Some(meta) = head(run, &path, errors) {
            adopt(run, path, meta);
        }
    }
    if let Some((path, _, tail)) = run.rollout.as_mut() {
        if let Err(e) = tail.refresh(path) {
            errors.push(("reading a rollout's tail", e));
        }
    }
    for sub in &mut run.subs {
        if let Err(e) = sub.tail.refresh(&sub.path) {
            errors.push(("reading a subagent rollout's tail", e));
        }
    }
}

/// The head of `path`, read once per run. A file whose first line is not
/// complete yet is tried again next poll.
fn head(
    run: &mut Run,
    path: &Path,
    errors: &mut Vec<(&'static str, String)>,
) -> Option<SessionMeta> {
    if let Some(known) = run.heads.get(path) {
        return known.clone();
    }
    match read_session_meta(path) {
        Ok(Some(meta)) => {
            run.heads.insert(path.to_path_buf(), Some(meta.clone()));
            Some(meta)
        }
        Ok(None) => None,
        Err(e) => {
            errors.push(("reading a rollout's session_meta", e));
            run.heads.insert(path.to_path_buf(), None);
            None
        }
    }
}

/// Take `meta` as the run's own rollout or one of its subagents', or mark
/// it as neither.
fn adopt(run: &mut Run, path: PathBuf, meta: SessionMeta) {
    if run.rollout.as_ref().is_some_and(|(p, _, _)| *p == path)
        || run.subs.iter().any(|sub| sub.path == path)
    {
        return;
    }
    match &meta.source {
        RolloutSource::Exec if run.rollout.is_none() => {
            run.rollout = Some((path, meta, TailCache::default()));
        }
        RolloutSource::Subagent {
            parent_thread_id, ..
        } => {
            let ours = run
                .rollout
                .as_ref()
                .is_some_and(|(_, m, _)| m.id == *parent_thread_id)
                || run.subs.iter().any(|sub| sub.meta.id == *parent_thread_id);
            if ours {
                run.subs.push(Sub {
                    path,
                    meta,
                    tail: TailCache::default(),
                });
            } else if run.rollout.is_some() {
                // Another run's; its parent will not appear later.
                run.heads.insert(path, None);
            } else {
                // The run's own thread is not known yet: look again.
                run.heads.remove(&path);
            }
        }
        _ => {
            run.heads.insert(path, None);
        }
    }
}

fn view(run: &Run, live: bool, now_ms: i64) -> RunView {
    let meta = run.rollout.as_ref().map(|(_, meta, _)| meta);
    let cwd = run
        .args
        .cd
        .clone()
        .or_else(|| meta.and_then(|m| m.cwd.clone()));
    RunView {
        key: format!("{}-{}", run.pid, run.started_at_ms),
        pid: run.pid,
        state: if live {
            RunState::Running
        } else {
            RunState::Finished
        },
        started_at_ms: run.started_at_ms,
        ended_at_ms: (!live).then_some(now_ms),
        checkout: cwd
            .as_deref()
            .map(|cwd| basename(cwd.trim_end_matches('/')).to_string()),
        cwd,
        add_dirs: run.args.add_dirs.clone(),
        sandbox: run.args.sandbox.clone(),
        model: run.args.model.clone(),
        rollout_path: run
            .rollout
            .as_ref()
            .map(|(path, _, _)| path.display().to_string()),
        thread_id: meta.map(|m| m.id.clone()),
        cli_version: meta.and_then(|m| m.cli_version.clone()),
        activity: run
            .rollout
            .as_ref()
            .and_then(|(_, _, tail)| tail.summary.activity.clone()),
        subagents: run
            .subs
            .iter()
            .map(|sub| {
                let (agent_path, agent_nickname, depth) = match &sub.meta.source {
                    RolloutSource::Subagent {
                        agent_path,
                        agent_nickname,
                        depth,
                        ..
                    } => (agent_path.clone(), agent_nickname.clone(), *depth),
                    _ => (None, None, None),
                };
                SubagentView {
                    thread_id: sub.meta.id.clone(),
                    nickname: agent_nickname,
                    agent_path,
                    depth,
                    state: if live && !sub.tail.summary.completed {
                        "running"
                    } else {
                        "done"
                    },
                    activity: sub.tail.summary.activity.clone(),
                    rollout_path: sub.path.display().to_string(),
                }
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// macOS process discovery
// ---------------------------------------------------------------------------

/// Discovery through `libproc` and `sysctl`.
pub struct Libproc;

#[cfg(target_os = "macos")]
mod sys {
    use super::ProcInfo;
    use std::ffi::c_void;
    use std::path::PathBuf;

    fn errno() -> std::io::Error {
        std::io::Error::last_os_error()
    }

    pub fn child_pids(pid: u32) -> Result<Vec<u32>, String> {
        let mut cap = 64usize;
        loop {
            let mut buf = vec![0 as libc::pid_t; cap];
            let size = (cap * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
            // SAFETY: the buffer holds `size` bytes; the call writes at most
            // that many and returns how many pids it wrote.
            let n = unsafe {
                libc::proc_listchildpids(pid as libc::pid_t, buf.as_mut_ptr() as *mut c_void, size)
            };
            if n < 0 {
                return Err(format!("proc_listchildpids({pid}): {}", errno()));
            }
            let n = n as usize;
            if n < cap {
                buf.truncate(n);
                return Ok(buf
                    .into_iter()
                    .filter(|p| *p > 0)
                    .map(|p| p as u32)
                    .collect());
            }
            cap *= 4;
            if cap > 1 << 16 {
                return Err(format!("proc_listchildpids({pid}): too many children"));
            }
        }
    }

    /// `None` when the process is gone.
    pub fn info(pid: u32) -> Result<Option<ProcInfo>, String> {
        // SAFETY: proc_bsdinfo is plain data; zeroed is a valid value.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is `size` bytes and the flavor writes that struct.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut c_void,
                size,
            )
        };
        if n != size {
            let error = errno();
            return match error.raw_os_error() {
                Some(libc::ESRCH) | Some(0) | None => Ok(None),
                _ => Err(format!("proc_pidinfo({pid}, PROC_PIDTBSDINFO): {error}")),
            };
        }
        let comm: Vec<u8> = info
            .pbi_comm
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        Ok(Some(ProcInfo {
            pid,
            comm: String::from_utf8_lossy(&comm).into_owned(),
            start_ms: info.pbi_start_tvsec as i64 * 1000 + info.pbi_start_tvusec as i64 / 1000,
        }))
    }

    pub fn argv(pid: u32) -> Result<Vec<String>, String> {
        let mut argmax: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        // SAFETY: reads one c_int into `argmax`.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                &mut argmax as *mut _ as *mut c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(format!("sysctl KERN_ARGMAX: {}", errno()));
        }
        let mut buf = vec![0u8; argmax.max(4096) as usize];
        let mut size = buf.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        // SAFETY: writes at most `size` bytes into `buf` and updates `size`.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr() as *mut c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(format!("sysctl KERN_PROCARGS2 for {pid}: {}", errno()));
        }
        Ok(parse_procargs2(&buf[..size]))
    }

    /// `KERN_PROCARGS2`: argc (native int), the exec path, NUL padding,
    /// then argc NUL-terminated arguments (then the environment).
    pub fn parse_procargs2(buf: &[u8]) -> Vec<String> {
        if buf.len() < 4 {
            return Vec::new();
        }
        let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]).max(0) as usize;
        let mut rest = &buf[4..];
        // Skip the exec path and its padding.
        let path_end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
        rest = &rest[path_end..];
        let start = rest.iter().position(|b| *b != 0).unwrap_or(rest.len());
        rest = &rest[start..];
        rest.split(|b| *b == 0)
            .take(argc)
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect()
    }

    const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

    /// `struct vnode_fdinfowithpath` (sys/proc_info.h): `proc_fileinfo`
    /// (24 bytes), `vnode_info` (152), then the path (MAXPATHLEN).
    #[repr(C, align(8))]
    struct VnodeFdInfoWithPath {
        _pfi: [u8; 24],
        _vip: [u8; 152],
        path: [u8; 1024],
    }

    pub fn open_files(pid: u32) -> Result<Vec<PathBuf>, String> {
        let entry = std::mem::size_of::<libc::proc_fdinfo>();
        // SAFETY: a null buffer asks for the size needed.
        let need = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDLISTFDS,
                0,
                std::ptr::null_mut(),
                0,
            )
        };
        if need <= 0 {
            return Err(format!("proc_pidinfo({pid}, PROC_PIDLISTFDS): {}", errno()));
        }
        let cap = need as usize / entry + 16;
        // SAFETY: proc_fdinfo is plain data.
        let mut fds: Vec<libc::proc_fdinfo> = vec![unsafe { std::mem::zeroed() }; cap];
        // SAFETY: `fds` holds `cap` entries.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr() as *mut c_void,
                (cap * entry) as libc::c_int,
            )
        };
        if n <= 0 {
            return Err(format!("proc_pidinfo({pid}, PROC_PIDLISTFDS): {}", errno()));
        }
        fds.truncate(n as usize / entry);
        let mut paths = Vec::new();
        for fd in fds
            .iter()
            .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        {
            // SAFETY: plain data.
            let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<VnodeFdInfoWithPath>() as libc::c_int;
            // SAFETY: `info` is `size` bytes, the size of the flavor's struct.
            let got = unsafe {
                libc::proc_pidfdinfo(
                    pid as libc::c_int,
                    fd.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    &mut info as *mut _ as *mut c_void,
                    size,
                )
            };
            // A descriptor closed since the listing is skipped.
            if got != size {
                continue;
            }
            let end = info
                .path
                .iter()
                .position(|b| *b == 0)
                .unwrap_or(info.path.len());
            if end > 0 {
                paths.push(PathBuf::from(
                    String::from_utf8_lossy(&info.path[..end]).into_owned(),
                ));
            }
        }
        Ok(paths)
    }

    #[cfg(test)]
    pub const VNODE_INFO_SIZE: usize = std::mem::size_of::<VnodeFdInfoWithPath>();
}

#[cfg(target_os = "macos")]
impl Processes for Libproc {
    fn descendants(&self, pid: u32) -> Result<Vec<ProcInfo>, String> {
        let mut out = Vec::new();
        let mut queue = vec![pid];
        while let Some(parent) = queue.pop() {
            for child in sys::child_pids(parent)? {
                if let Some(info) = sys::info(child)? {
                    out.push(info);
                    queue.push(child);
                }
            }
            if out.len() > 4096 {
                return Err(format!("more than 4096 processes under {pid}"));
            }
        }
        Ok(out)
    }

    fn argv(&self, pid: u32) -> Result<Vec<String>, String> {
        sys::argv(pid)
    }

    fn open_files(&self, pid: u32) -> Result<Vec<PathBuf>, String> {
        sys::open_files(pid)
    }
}

#[cfg(not(target_os = "macos"))]
impl Processes for Libproc {
    fn descendants(&self, _pid: u32) -> Result<Vec<ProcInfo>, String> {
        Err("finding codex exec runs is only implemented on macOS".to_string())
    }

    fn argv(&self, _pid: u32) -> Result<Vec<String>, String> {
        Err("finding codex exec runs is only implemented on macOS".to_string())
    }

    fn open_files(&self, _pid: u32) -> Result<Vec<PathBuf>, String> {
        Err("finding codex exec runs is only implemented on macOS".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    const NATIVE: &str = "/opt/codex/vendor/aarch64-apple-darwin/bin/codex";

    #[test]
    fn exec_runs_are_told_from_other_codex_processes_by_their_args() {
        assert!(is_exec_run(&argv(&[
            NATIVE,
            "exec",
            "-s",
            "workspace-write",
            "-c",
            "approval_policy=\"never\"",
            "-C",
            "/repo",
            "do the thing"
        ])));
        // Global options (with values) before the subcommand.
        assert!(is_exec_run(&argv(&[
            "codex",
            "-c",
            "model=\"x\"",
            "-C",
            "/r",
            "exec",
            "hi"
        ])));
        assert!(is_exec_run(&argv(&["codex", "e", "hi"])));
        // Long-lived helpers and other subcommands.
        assert!(!is_exec_run(&argv(&[NATIVE, "app-server"])));
        assert!(!is_exec_run(&argv(&[NATIVE, "mcp-server"])));
        assert!(!is_exec_run(&argv(&[
            "/opt/codex/bin/codex-code-mode-host"
        ])));
        // Interactive, with "exec" inside its prompt or as an option value.
        assert!(!is_exec_run(&argv(&["codex", "please exec this"])));
        assert!(!is_exec_run(&argv(&["codex", "-c", "exec", "resume"])));
        assert!(!is_exec_run(&argv(&["codex"])));
        // The node wrapper is not the run (the native child holds the rollout).
        assert!(!is_exec_run(&argv(&[
            "node",
            "/opt/codex/bin/codex",
            "exec",
            "hi"
        ])));
    }

    #[test]
    fn exec_args_name_the_checkout_and_added_dirs() {
        let args = parse_exec_args(&argv(&[
            NATIVE,
            "exec",
            "-s",
            "workspace-write",
            "-c",
            "approval_policy=\"never\"",
            "-C",
            "/work/repo-a",
            "--add-dir",
            "/tmp/out",
            "--add-dir=/tmp/more",
            "-m",
            "gpt-x",
            "the brief -C /not/this",
        ]));
        assert_eq!(args.cd.as_deref(), Some("/work/repo-a"));
        assert_eq!(args.add_dirs, ["/tmp/out", "/tmp/more"]);
        assert_eq!(args.sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(args.model.as_deref(), Some("gpt-x"));
        assert_eq!(
            parse_exec_args(&argv(&["codex", "exec", "--cd=/r", "x"]))
                .cd
                .as_deref(),
            Some("/r")
        );
    }

    fn meta_line(source: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-01-02T10:00:00.000Z","type":"session_meta","payload":{{"id":"thread-1","timestamp":"2026-01-02T10:00:00.000Z","cwd":"/work/repo-a","originator":"codex_exec","cli_version":"9.9.9","source":{source},"base_instructions":{{"text":"long"}}}}}}"#
        )
    }

    #[test]
    fn session_meta_reads_exec_and_thread_spawn_sources() {
        let exec = parse_session_meta(&meta_line(r#""exec""#)).unwrap();
        assert_eq!(exec.id, "thread-1");
        assert_eq!(exec.cwd.as_deref(), Some("/work/repo-a"));
        assert_eq!(exec.originator.as_deref(), Some("codex_exec"));
        assert_eq!(exec.cli_version.as_deref(), Some("9.9.9"));
        assert_eq!(exec.source, RolloutSource::Exec);

        let sub = parse_session_meta(&meta_line(
            r#"{"subagent":{"thread_spawn":{"parent_thread_id":"thread-0","depth":1,"agent_path":"/root/review","agent_nickname":"Ada","agent_role":null}}}"#,
        ))
        .unwrap();
        assert_eq!(
            sub.source,
            RolloutSource::Subagent {
                parent_thread_id: "thread-0".into(),
                depth: Some(1),
                agent_path: Some("/root/review".into()),
                agent_nickname: Some("Ada".into()),
            }
        );
        assert_eq!(
            parse_session_meta(&meta_line(r#""cli""#)).unwrap().source,
            RolloutSource::Other("cli".into())
        );
        assert!(parse_session_meta(r#"{"type":"event_msg","payload":{}}"#).is_err());
        assert!(parse_session_meta(r#"{"type":"session_meta","payload":{"cwd":"/x"}}"#).is_err());
        assert!(parse_session_meta(
            r#"{"type":"session_meta","payload":{"id":"t","source":{"subagent":{"thread_spawn":{}}}}}"#
        )
        .is_err());
    }

    #[test]
    fn session_meta_is_read_from_the_first_line_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("rollout-2026-01-02T10-00-00-thread-1.jsonl");
        std::fs::write(&path, format!("{}\n{{not json\n", meta_line(r#""exec""#))).unwrap();
        assert_eq!(read_session_meta(&path).unwrap().unwrap().id, "thread-1");
        // A first line still being written is not an error yet.
        std::fs::write(&path, &meta_line(r#""exec""#)[..40]).unwrap();
        assert_eq!(read_session_meta(&path).unwrap(), None);
        assert!(read_session_meta(&dir.path().join("missing.jsonl")).is_err());
    }

    fn line(kind: &str, payload: &str) -> String {
        format!(r#"{{"timestamp":"2026-01-02T10:00:05.000Z","type":"{kind}","payload":{payload}}}"#)
    }

    #[test]
    fn the_tail_gives_the_latest_message_or_tool_call() {
        let started = line("event_msg", r#"{"type":"task_started","turn_id":"t"}"#);
        let message = line(
            "response_item",
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"\nI will fix the parser,\nthen test."}]}"#,
        );
        let reasoning = line("response_item", r#"{"type":"reasoning","summary":[]}"#);
        let tokens = line("event_msg", r#"{"type":"token_count","info":{}}"#);
        let command = line(
            "event_msg",
            r#"{"type":"item_completed","item":{"type":"CommandExecution","command":["/bin/zsh","-lc","cargo test --lib"],"exit_code":0}}"#,
        );
        let tail = [
            started.as_str(),
            message.as_str(),
            reasoning.as_str(),
            tokens.as_str(),
        ]
        .join("\n");
        let summary = summarize_tail(tail.as_bytes(), false);
        assert_eq!(
            summary.activity.as_ref().unwrap().text,
            "I will fix the parser,"
        );
        assert_eq!(summary.activity.unwrap().at_ms, Some(1767348005000));
        assert!(!summary.completed);

        // A command after it is newer; a half-written last line is skipped.
        let tail = format!("{tail}\n{command}\n{{\"type\":\"resp");
        assert_eq!(
            summarize_tail(tail.as_bytes(), false)
                .activity
                .unwrap()
                .text,
            "Ran cargo test --lib"
        );

        // The tail's cut first line is dropped.
        let cut = &command[20..];
        assert_eq!(summarize_tail(cut.as_bytes(), true), TailSummary::default());

        let call = line(
            "response_item",
            r#"{"type":"function_call","name":"spawn_agent","namespace":"collaboration","arguments":"{\"task_name\":\"review\",\"message\":\"x\"}"}"#,
        );
        assert_eq!(
            summarize_tail(call.as_bytes(), false)
                .activity
                .unwrap()
                .text,
            "spawn_agent review"
        );
        let custom = line(
            "response_item",
            r#"{"type":"custom_tool_call","name":"exec","input":"text(await tools.exec_command({cmd:\"ls\"}));\nmore"}"#,
        );
        assert_eq!(
            summarize_tail(custom.as_bytes(), false)
                .activity
                .unwrap()
                .text,
            r#"exec: text(await tools.exec_command({cmd:"ls"}));"#
        );
        let edit = line(
            "event_msg",
            r#"{"type":"item_completed","item":{"type":"FileChange","changes":{"/r/src/a.rs":{},"/r/b.rs":{}}}}"#,
        );
        assert_eq!(
            summarize_tail(edit.as_bytes(), false)
                .activity
                .unwrap()
                .text,
            "Edited a.rs, b.rs"
        );
        let agent = line(
            "event_msg",
            r#"{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"Done here."}],"phase":"final_answer"}}"#,
        );
        assert_eq!(
            summarize_tail(agent.as_bytes(), false)
                .activity
                .unwrap()
                .text,
            "Done here."
        );

        // The turn's end.
        let complete = line(
            "event_msg",
            r#"{"type":"task_complete","last_agent_message":"Fixed it.\n\nDetails…"}"#,
        );
        let tail = format!("{started}\n{message}\n{complete}\n{tokens}");
        let summary = summarize_tail(tail.as_bytes(), false);
        assert!(summary.completed);
        assert_eq!(summary.activity.unwrap().text, "Finished: Fixed it.");
        // A new turn after it runs again.
        let tail = format!("{complete}\n{started}");
        let summary = summarize_tail(tail.as_bytes(), false);
        assert!(!summary.completed);
        assert_eq!(summary.activity.unwrap().text, "Finished: Fixed it.");
    }

    #[test]
    fn long_tails_are_read_from_the_end_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-x.jsonl");
        let message = line(
            "response_item",
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Working on it."}]}"#,
        );
        // A big output after the message: the first 64 KiB hold no activity.
        let big = line(
            "response_item",
            &format!(
                r#"{{"type":"custom_tool_call_output","output":"{}"}}"#,
                "x".repeat(200_000)
            ),
        );
        let mut text = String::new();
        for _ in 0..50 {
            text.push_str(&line("event_msg", r#"{"type":"token_count"}"#));
            text.push('\n');
        }
        text.push_str(&format!("{message}\n{big}\n"));
        std::fs::write(&path, &text).unwrap();
        let summary = read_tail_summary(&path).unwrap();
        assert_eq!(summary.activity.unwrap().text, "Working on it.");
    }

    #[test]
    fn rollout_names_and_times() {
        assert!(is_rollout_path(Path::new(
            "/s/2026/01/02/rollout-2026-01-02T10-00-00-abc.jsonl"
        )));
        assert!(!is_rollout_path(Path::new("/s/2026/01/02/history.jsonl")));
        assert!(!is_rollout_path(Path::new("/s/rollout-x.json")));
        use chrono::TimeZone;
        let expected = chrono::Local
            .with_ymd_and_hms(2026, 1, 2, 10, 0, 7)
            .earliest()
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            rollout_name_time_ms(Path::new("rollout-2026-01-02T10-00-07-0a1b.jsonl")),
            Some(expected)
        );
        assert_eq!(
            rollout_name_time_ms(Path::new("rollout-garbage.jsonl")),
            None
        );
    }

    /// A process table and open files the test controls.
    #[derive(Default)]
    struct FakeProcs {
        tree: HashMap<u32, Vec<ProcInfo>>,
        argv: HashMap<u32, Vec<String>>,
        open: HashMap<u32, Vec<PathBuf>>,
    }

    impl Processes for FakeProcs {
        fn descendants(&self, pid: u32) -> Result<Vec<ProcInfo>, String> {
            self.tree
                .get(&pid)
                .cloned()
                .ok_or_else(|| format!("no process {pid}"))
        }
        fn argv(&self, pid: u32) -> Result<Vec<String>, String> {
            self.argv
                .get(&pid)
                .cloned()
                .ok_or_else(|| format!("no argv {pid}"))
        }
        fn open_files(&self, pid: u32) -> Result<Vec<PathBuf>, String> {
            Ok(self.open.get(&pid).cloned().unwrap_or_default())
        }
    }

    fn proc(pid: u32, comm: &str, start_ms: i64) -> ProcInfo {
        ProcInfo {
            pid,
            comm: comm.into(),
            start_ms,
        }
    }

    #[test]
    fn a_chats_exec_run_maps_to_its_rollout_and_subagents_then_finishes() {
        use chrono::TimeZone;
        let sessions = tempfile::tempdir().unwrap();
        let start = chrono::Local
            .with_ymd_and_hms(2026, 1, 2, 10, 0, 0)
            .earliest()
            .unwrap();
        let day = sessions.path().join("2026/01/02");
        std::fs::create_dir_all(&day).unwrap();
        let run_path = day.join("rollout-2026-01-02T10-00-00-thread-1.jsonl");
        let message = line(
            "response_item",
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Reading the code."}]}"#,
        );
        std::fs::write(
            &run_path,
            format!("{}\n{message}\n", meta_line(r#""exec""#)),
        )
        .unwrap();
        // A subagent of this run, one of another run, and an older file.
        let sub_meta = meta_line(
            r#"{"subagent":{"thread_spawn":{"parent_thread_id":"thread-1","depth":1,"agent_path":"/root/review","agent_nickname":"Ada"}}}"#,
        )
        .replace("\"id\":\"thread-1\"", "\"id\":\"thread-2\"");
        let sub_path = day.join("rollout-2026-01-02T10-01-00-thread-2.jsonl");
        let done = line(
            "event_msg",
            r#"{"type":"task_complete","last_agent_message":"Reviewed."}"#,
        );
        std::fs::write(&sub_path, format!("{sub_meta}\n{done}\n")).unwrap();
        let other = meta_line(r#"{"subagent":{"thread_spawn":{"parent_thread_id":"elsewhere"}}}"#)
            .replace("\"id\":\"thread-1\"", "\"id\":\"thread-9\"");
        std::fs::write(
            day.join("rollout-2026-01-02T10-02-00-thread-9.jsonl"),
            format!("{other}\n"),
        )
        .unwrap();
        std::fs::write(
            day.join("rollout-2026-01-02T09-00-00-thread-0.jsonl"),
            format!(
                "{}\n",
                meta_line(r#""exec""#).replace("thread-1", "thread-0")
            ),
        )
        .unwrap();

        let start_ms = start.timestamp_millis();
        let mut procs = FakeProcs::default();
        procs.tree.insert(
            100,
            vec![
                proc(101, "zsh", start_ms - 5),
                proc(102, "node", start_ms - 2),
                proc(103, "codex", start_ms),
                proc(104, "codex", start_ms - 60_000),
            ],
        );
        procs.argv.insert(
            103,
            argv(&[
                NATIVE,
                "exec",
                "-C",
                "/work/repo-a",
                "--add-dir",
                "/tmp/out",
                "brief",
            ]),
        );
        procs.argv.insert(104, argv(&[NATIVE, "app-server"]));
        procs
            .open
            .insert(103, vec![PathBuf::from("/dev/null"), run_path.clone()]);

        let mut watcher = Watcher::default();
        let roots = [Root {
            tab_id: 7,
            pid: 100,
        }];
        let out = watcher.poll(&procs, &roots, start_ms + 60_000);
        assert_eq!(out.len(), 1);
        let (tab, run) = &out[0];
        assert_eq!(*tab, 7);
        assert_eq!(run.key, format!("103-{start_ms}"));
        assert_eq!(run.state, RunState::Running);
        assert_eq!(run.ended_at_ms, None);
        assert_eq!(run.cwd.as_deref(), Some("/work/repo-a"));
        assert_eq!(run.checkout.as_deref(), Some("repo-a"));
        assert_eq!(run.add_dirs, ["/tmp/out"]);
        assert_eq!(
            run.rollout_path.as_deref(),
            Some(run_path.to_str().unwrap())
        );
        assert_eq!(run.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(run.activity.as_ref().unwrap().text, "Reading the code.");
        assert_eq!(run.subagents.len(), 1);
        let sub = &run.subagents[0];
        assert_eq!(sub.thread_id, "thread-2");
        assert_eq!(sub.nickname.as_deref(), Some("Ada"));
        assert_eq!(sub.agent_path.as_deref(), Some("/root/review"));
        assert_eq!(sub.state, "done");
        assert_eq!(sub.activity.as_ref().unwrap().text, "Finished: Reviewed.");
        // The app-server's args were read once and it is remembered as no run.
        assert!(watcher.not_runs.contains(&(104, start_ms - 60_000)));
        procs.argv.remove(&104);

        // The rollout grows: the next poll sees the new activity.
        let command = line(
            "event_msg",
            r#"{"type":"item_completed","item":{"type":"CommandExecution","command":["/bin/zsh","-lc","cargo test"]}}"#,
        );
        let mut text = std::fs::read_to_string(&run_path).unwrap();
        text.push_str(&format!("{command}\n"));
        std::fs::write(&run_path, text).unwrap();
        let out = watcher.poll(&procs, &roots, start_ms + 65_000);
        assert_eq!(out[0].1.activity.as_ref().unwrap().text, "Ran cargo test");
        assert!(watcher.logged.is_empty(), "{:?}", watcher.logged);

        // The process exits: one finished payload, then nothing.
        procs.tree.insert(100, vec![proc(101, "zsh", start_ms - 5)]);
        let out = watcher.poll(&procs, &roots, start_ms + 70_000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.state, RunState::Finished);
        assert_eq!(out[0].1.ended_at_ms, Some(start_ms + 70_000));
        assert_eq!(out[0].1.subagents[0].state, "done");
        assert!(!watcher.has_runs());
        assert!(watcher.poll(&procs, &roots, start_ms + 75_000).is_empty());
        assert!(watcher.not_runs.is_empty());
    }

    #[test]
    fn discovery_failures_are_logged_once() {
        let procs = FakeProcs::default();
        let mut watcher = Watcher::default();
        let roots = [Root { tab_id: 1, pid: 5 }, Root { tab_id: 2, pid: 6 }];
        assert!(watcher.poll(&procs, &roots, 0).is_empty());
        assert!(watcher.poll(&procs, &roots, 0).is_empty());
        assert_eq!(watcher.logged.len(), 1);
    }

    #[test]
    fn the_run_payload_carries_what_the_row_shows() {
        let view = RunView {
            key: "103-1000".into(),
            pid: 103,
            state: RunState::Finished,
            started_at_ms: 1000,
            ended_at_ms: Some(721_000),
            cwd: Some("/work/repo-a".into()),
            checkout: Some("repo-a".into()),
            add_dirs: vec!["/tmp/out".into()],
            sandbox: Some("workspace-write".into()),
            model: None,
            rollout_path: Some("/s/rollout-a.jsonl".into()),
            thread_id: Some("thread-1".into()),
            cli_version: Some("9.9.9".into()),
            activity: Some(Activity {
                text: "Ran cargo test".into(),
                at_ms: Some(700_000),
            }),
            subagents: vec![SubagentView {
                thread_id: "thread-2".into(),
                nickname: Some("Ada".into()),
                agent_path: Some("/root/review".into()),
                depth: Some(1),
                state: "done",
                activity: None,
                rollout_path: "/s/rollout-b.jsonl".into(),
            }],
        };
        let payload = crate::chat_page::codex_exec_payload(&view);
        assert_eq!(payload["kind"], "codex_exec");
        let run = &payload["run"];
        assert_eq!(run["key"], "103-1000");
        assert_eq!(run["state"], "finished");
        assert_eq!(run["ended_at_ms"], 721_000);
        assert_eq!(run["checkout"], "repo-a");
        assert_eq!(run["add_dirs"][0], "/tmp/out");
        assert_eq!(run["activity"]["text"], "Ran cargo test");
        assert_eq!(run["subagents"][0]["nickname"], "Ada");
        assert_eq!(run["subagents"][0]["state"], "done");
        assert!(run["model"].is_null());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn libproc_finds_children_their_args_and_open_files() {
        assert_eq!(sys::VNODE_INFO_SIZE, 1200);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-2026-01-02T10-00-00-open.jsonl");
        std::fs::write(&path, "x\n").unwrap();
        let _held = File::open(&path).unwrap();
        let me = std::process::id();
        let open = Libproc.open_files(me).unwrap();
        let canonical = path.canonicalize().unwrap();
        assert!(open.contains(&canonical), "{canonical:?} not in {open:?}");

        let mut child = std::process::Command::new("/bin/sleep")
            .args(["30", "exec"])
            .spawn()
            .unwrap();
        let found = Libproc.descendants(me);
        let argv = Libproc.argv(child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        let found = found.unwrap();
        let sleep = found
            .iter()
            .find(|p| p.pid == child.id())
            .expect("the child");
        assert_eq!(sleep.comm, "sleep");
        let now_ms = chrono::Utc::now().timestamp_millis();
        assert!(
            (now_ms - sleep.start_ms).abs() < 60_000,
            "{}",
            sleep.start_ms
        );
        assert_eq!(argv.unwrap(), ["/bin/sleep", "30", "exec"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn procargs2_layout() {
        let mut buf = 3i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/codex\0\0\0\0codex\0exec\0brief text\0HOME=/x\0");
        assert_eq!(sys::parse_procargs2(&buf), ["codex", "exec", "brief text"]);
        assert!(sys::parse_procargs2(&[1, 0]).is_empty());
    }
}
