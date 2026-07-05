//! Chats index: a read-only catalog of harness conversations on this
//! machine (TRU-78 slice 1: local claude only).
//!
//! Performance contract (see .plans/chats-panel.md): the index never
//! parses a full transcript. Per file it reads stat + a bounded head
//! (title, cwd, branch) and a bounded tail (freshness). The preview
//! tail for a selected chat is parsed on demand, also bounded. All
//! functions here are blocking and must run inside a background Task,
//! never in update()/view().

use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Max bytes of a transcript head scanned for title/cwd/branch. A single
/// pasted-content line can be huge; this bounds the worst case.
const HEAD_SCAN_BYTES: u64 = 256 * 1024;
/// Max lines of the head scanned before giving up on a title.
const HEAD_SCAN_LINES: usize = 60;
/// Bytes read from the end of a transcript for the preview tail.
const TAIL_READ_BYTES: u64 = 128 * 1024;
/// Messages shown in the preview tail.
const PREVIEW_MESSAGES: usize = 8;
/// Truncation length for titles derived from the first user message.
const TITLE_MAX_CHARS: usize = 90;
/// A transcript younger than this that no GitTerm tab owns was probably
/// started by hand in a bare terminal and may still be running.
const POSSIBLY_RUNNING_SECS: u64 = 120;

#[derive(Debug, Clone, PartialEq)]
pub struct ChatIndexEntry {
    /// Session id — the transcript file stem (claude: a uuid).
    pub id: String,
    pub backend: ChatBackend,
    pub path: PathBuf,
    pub cwd: PathBuf,
    /// Main-repo root (worktrees collapse into it). None when the cwd is
    /// gone or not a git repo; grouping falls back to the cwd.
    pub repo_root: Option<PathBuf>,
    /// True when cwd is a linked worktree of repo_root rather than the
    /// main checkout.
    pub is_worktree: bool,
    pub branch: Option<String>,
    pub title: String,
    pub mtime: SystemTime,
    pub size: u64,
    /// The recorded cwd no longer exists on disk.
    pub dead_cwd: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatBackend {
    Claude,
    Codex,
    Pi,
}

impl ChatBackend {
    pub const ALL: [ChatBackend; 3] = [ChatBackend::Claude, ChatBackend::Codex, ChatBackend::Pi];

    /// Inverse of `label()`; used to decode the wire form of remote
    /// chat entries.
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "claude" => Some(ChatBackend::Claude),
            "codex" => Some(ChatBackend::Codex),
            "pi" => Some(ChatBackend::Pi),
            _ => None,
        }
    }
}

/// The three scope rings of the Chats panel. With only the local machine
/// indexed (slice 1), Machine and Everywhere show the same set; they
/// diverge when remote machines land (slice 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChatScope {
    #[default]
    Workspace,
    Machine,
    Everywhere,
}

/// Compact relative age for list rows ("now", "5m", "3h", "2d", "4mo").
pub fn format_age(mtime: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(mtime)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        86_400..=2_591_999 => format!("{}d", secs / 86_400),
        _ => format!("{}mo", secs / 2_592_000),
    }
}

pub fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

impl ChatBackend {
    pub fn label(&self) -> &'static str {
        match self {
            ChatBackend::Claude => "claude",
            ChatBackend::Codex => "codex",
            ChatBackend::Pi => "pi",
        }
    }
}

impl ChatIndexEntry {
    /// Key the sidebar groups by: the main-repo root when known,
    /// otherwise the recorded cwd.
    pub fn group_root(&self) -> &Path {
        self.repo_root.as_deref().unwrap_or(&self.cwd)
    }

    /// Display name for the group header.
    pub fn group_name(&self) -> String {
        self.group_root()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.group_root().to_string_lossy().to_string())
    }

    /// Whether this chat belongs to a workspace rooted at `ws_dir`:
    /// either the recorded cwd is under it, or the chat ran in a
    /// worktree whose main repo lives under it.
    pub fn in_workspace(&self, ws_dir: &Path) -> bool {
        self.cwd.starts_with(ws_dir)
            || self
                .repo_root
                .as_deref()
                .is_some_and(|root| root.starts_with(ws_dir))
    }

    /// Shell command that resumes this conversation. Must run in the
    /// conversation's recorded cwd (or the rescue dir when cwd is dead).
    pub fn resume_command(&self) -> String {
        match self.backend {
            ChatBackend::Claude => format!("claude --resume {}", self.id),
            ChatBackend::Codex => format!("codex resume {}", self.id),
            // pi resumes a specific session via --session (`--resume` is
            // the interactive picker).
            ChatBackend::Pi => format!("pi --session {}", self.id),
        }
    }

    /// The transcript was modified moments ago yet no GitTerm tab owns
    /// it — likely a session running in a plain terminal GitTerm didn't
    /// start. Callers overlay this with the live-tab registry.
    pub fn possibly_running(&self) -> bool {
        SystemTime::now()
            .duration_since(self.mtime)
            .map(|age| age.as_secs() < POSSIBLY_RUNNING_SECS)
            .unwrap_or(true)
    }

    pub fn matches_query(&self, query_lower: &str) -> bool {
        if query_lower.is_empty() {
            return true;
        }
        self.title.to_lowercase().contains(query_lower)
            || self.group_name().to_lowercase().contains(query_lower)
            || self
                .branch
                .as_deref()
                .is_some_and(|b| b.to_lowercase().contains(query_lower))
    }
}

/// One message of a preview tail.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatPreviewMessage {
    pub is_user: bool,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChatPreview {
    pub messages: Vec<ChatPreviewMessage>,
    /// From the transcript's own bookkeeping when present (system
    /// turn_duration lines carry messageCount); None otherwise.
    pub message_count: Option<u64>,
}

/// Fields claude writes on most transcript lines. Unknown fields ignored.
#[derive(Debug, Deserialize)]
struct LineMeta {
    #[serde(rename = "type")]
    kind: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "gitBranch")]
    git_branch: Option<String>,
    summary: Option<String>,
    #[serde(rename = "isMeta")]
    is_meta: Option<bool>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    #[serde(rename = "messageCount")]
    message_count: Option<u64>,
    entrypoint: Option<String>,
    message: Option<Value>,
}

impl LineMeta {
    fn is_real_message(&self) -> bool {
        !self.is_meta.unwrap_or(false) && !self.is_sidechain.unwrap_or(false)
    }

    /// Plain text of a user/assistant message, if this line is one.
    fn message_text(&self) -> Option<String> {
        let kind = self.kind.as_deref()?;
        if kind != "user" && kind != "assistant" {
            return None;
        }
        let content = self.message.as_ref()?.get("content")?;
        let text = match content {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| {
                    (b.get("type")?.as_str()? == "text")
                        .then(|| b.get("text")?.as_str().map(str::to_string))?
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => return None,
        };
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_string())
    }
}

/// Harness-injected user content (command wrappers, caveats, reminders)
/// that must not become a chat title.
fn is_synthetic_user_text(text: &str) -> bool {
    text.starts_with('<')
}

fn title_from_text(text: &str) -> String {
    let first_line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or(text);
    let mut title: String = first_line.trim().chars().take(TITLE_MAX_CHARS).collect();
    if first_line.trim().chars().count() > TITLE_MAX_CHARS {
        title.push('…');
    }
    title
}

/// Head-of-file metadata scan: bounded read, stops as soon as it has a
/// title (cwd/branch usually arrive on the same early lines).
struct HeadScan {
    title: Option<String>,
    cwd: Option<PathBuf>,
    branch: Option<String>,
    /// True when the session was started headlessly (`claude -p` / SDK —
    /// entrypoint "sdk-cli" etc.), e.g. hook-spawned review runs. These
    /// are one-shot automation, not conversations to go back to, so the
    /// index skips them. A missing entrypoint counts as interactive.
    headless: bool,
}

fn scan_head(path: &Path) -> std::io::Result<HeadScan> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file.take(HEAD_SCAN_BYTES));
    let mut out = HeadScan {
        title: None,
        cwd: None,
        branch: None,
        headless: false,
    };
    let mut line = String::new();
    for _ in 0..HEAD_SCAN_LINES {
        line.clear();
        // A line truncated by the byte cap fails to parse as JSON and is
        // skipped; that only loses metadata, never corrupts it.
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let Ok(meta) = serde_json::from_str::<LineMeta>(&line) else {
            continue;
        };
        if let Some(entrypoint) = &meta.entrypoint {
            if entrypoint.starts_with("sdk") {
                out.headless = true;
                break;
            }
        }
        if out.cwd.is_none() {
            if let Some(cwd) = &meta.cwd {
                out.cwd = Some(PathBuf::from(cwd));
            }
        }
        if out.branch.is_none() {
            if let Some(branch) = &meta.git_branch {
                if !branch.is_empty() {
                    out.branch = Some(branch.clone());
                }
            }
        }
        if out.title.is_none() {
            if let Some(summary) = &meta.summary {
                if meta.kind.as_deref() == Some("summary") && !summary.is_empty() {
                    out.title = Some(summary.clone());
                }
            }
        }
        if out.title.is_none() && meta.kind.as_deref() == Some("user") && meta.is_real_message() {
            if let Some(text) = meta.message_text() {
                if !is_synthetic_user_text(&text) {
                    out.title = Some(title_from_text(&text));
                }
            }
        }
        if out.title.is_some() && out.cwd.is_some() && out.branch.is_some() {
            break;
        }
    }
    Ok(out)
}

/// Complete JSONL lines from the last `TAIL_READ_BYTES` of the file.
fn read_tail_lines(path: &Path) -> std::io::Result<Vec<String>> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_READ_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).or_else(|_| {
        // A seek can land mid-UTF-8; retry lossily.
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        buf = String::from_utf8_lossy(&bytes).into_owned();
        Ok::<usize, std::io::Error>(buf.len())
    })?;
    let mut lines: Vec<String> = buf.lines().map(str::to_string).collect();
    // The first line is truncated unless we started at 0.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    Ok(lines)
}

/// Scan one transcript into an index entry. Returns None for sessions
/// with no real user message (empty shells from aborted launches).
fn index_transcript(path: &Path, backend: ChatBackend) -> Option<ChatIndexEntry> {
    let meta = std::fs::metadata(path).ok()?;
    let head = scan_head(path).ok()?;
    if head.headless {
        return None;
    }
    let title = head.title?;
    let cwd = head.cwd?;
    let id = path.file_stem()?.to_string_lossy().to_string();
    let dead_cwd = !cwd.exists();
    Some(ChatIndexEntry {
        id,
        backend,
        path: path.to_path_buf(),
        cwd,
        repo_root: None,
        is_worktree: false,
        branch: head.branch,
        title,
        mtime: meta.modified().ok()?,
        size: meta.len(),
        dead_cwd,
    })
}

/// A codex `response_item` message: (is_user, text). Codex wraps turns as
/// `{"type":"response_item","payload":{"type":"message","role":...,
/// "content":[{"type":"input_text"|"output_text","text":...}]}}`.
fn codex_message(value: &Value) -> Option<(bool, String)> {
    if value.get("type")?.as_str()? != "response_item" {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type")?.as_str()? != "message" {
        return None;
    }
    let is_user = match payload.get("role")?.as_str()? {
        "user" => true,
        "assistant" => false,
        // developer/system carry instructions, not conversation.
        _ => return None,
    };
    let text = payload
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|block| {
            let kind = block.get("type")?.as_str()?;
            (kind == "input_text" || kind == "output_text")
                .then(|| block.get("text")?.as_str().map(str::to_string))?
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| (is_user, text.to_string()))
}

/// A pi `message` line: (is_user, text). Same content-block shape as
/// claude, nested under `message`.
fn pi_message(value: &Value) -> Option<(bool, String)> {
    if value.get("type")?.as_str()? != "message" {
        return None;
    }
    let message = value.get("message")?;
    let is_user = match message.get("role")?.as_str()? {
        "user" => true,
        "assistant" => false,
        _ => return None,
    };
    let text = match message.get("content")? {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| {
                (block.get("type")?.as_str()? == "text")
                    .then(|| block.get("text")?.as_str().map(str::to_string))?
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| (is_user, text.to_string()))
}

/// Index one codex rollout. Returns None for headless `codex exec` runs
/// (originator carries "exec") and sessions with no real user message.
fn index_codex_transcript(path: &Path) -> Option<ChatIndexEntry> {
    let meta = std::fs::metadata(path).ok()?;
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file.take(HEAD_SCAN_BYTES));
    let mut line = String::new();
    let mut id = None;
    let mut cwd: Option<PathBuf> = None;
    let mut branch = None;
    let mut title = None;
    for _ in 0..HEAD_SCAN_LINES {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("session_meta") {
            let payload = value.get("payload")?;
            if payload
                .get("originator")
                .and_then(Value::as_str)
                .is_some_and(|originator| originator.contains("exec"))
            {
                return None;
            }
            id = payload
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string);
            cwd = payload
                .get("cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            branch = payload
                .get("git")
                .and_then(|git| git.get("branch"))
                .and_then(Value::as_str)
                .map(str::to_string);
        } else if title.is_none() {
            if let Some((true, text)) = codex_message(&value) {
                // Codex injects project docs as a plain user message
                // ("# AGENTS.md instructions for <path>"); never a title.
                if !is_synthetic_user_text(&text) && !text.starts_with("# AGENTS.md instructions") {
                    title = Some(title_from_text(&text));
                }
            }
        }
        if id.is_some() && title.is_some() {
            break;
        }
    }
    let cwd = cwd?;
    let dead_cwd = !cwd.exists();
    // Detached HEAD records the literal branch name "HEAD" — not useful.
    let branch = branch.filter(|branch| branch != "HEAD");
    Some(ChatIndexEntry {
        id: id?,
        backend: ChatBackend::Codex,
        path: path.to_path_buf(),
        cwd,
        repo_root: None,
        is_worktree: false,
        branch,
        title: title?,
        mtime: meta.modified().ok()?,
        size: meta.len(),
        dead_cwd,
    })
}

/// Index one pi session file. Line 1 is
/// `{"type":"session","id":...,"cwd":...}`; the title comes from the
/// first real user message.
fn index_pi_transcript(path: &Path) -> Option<ChatIndexEntry> {
    let meta = std::fs::metadata(path).ok()?;
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file.take(HEAD_SCAN_BYTES));
    let mut line = String::new();
    let mut id = None;
    let mut cwd: Option<PathBuf> = None;
    let mut title = None;
    for _ in 0..HEAD_SCAN_LINES {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("session") {
            id = value.get("id").and_then(Value::as_str).map(str::to_string);
            cwd = value.get("cwd").and_then(Value::as_str).map(PathBuf::from);
        } else if title.is_none() {
            if let Some((true, text)) = pi_message(&value) {
                if !is_synthetic_user_text(&text) {
                    title = Some(title_from_text(&text));
                }
            }
        }
        if id.is_some() && title.is_some() {
            break;
        }
    }
    let cwd = cwd?;
    let dead_cwd = !cwd.exists();
    Some(ChatIndexEntry {
        id: id?,
        backend: ChatBackend::Pi,
        path: path.to_path_buf(),
        cwd,
        repo_root: None,
        is_worktree: false,
        branch: None,
        title: title?,
        mtime: meta.modified().ok()?,
        size: meta.len(),
        dead_cwd,
    })
}

/// Resolved git identity of a cwd: (main-repo root, is_worktree).
fn resolve_repo_root(cwd: &Path) -> Option<(PathBuf, bool)> {
    let out = crate::agentd::git::git_command()
        .args([
            "--no-optional-locks",
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let toplevel = PathBuf::from(lines.next()?.trim());
    let common_dir = PathBuf::from(lines.next()?.trim());
    // Main checkout: common dir is <root>/.git. Linked worktree: common
    // dir still points into the main checkout.
    let main_root = common_dir.parent()?.to_path_buf();
    let is_worktree = toplevel != main_root;
    Some((main_root, is_worktree))
}

/// All .jsonl files up to `depth` directory levels below `root`
/// (claude/pi: slug/file = 1; codex: YYYY/MM/DD/file = 3).
fn jsonl_files_under(root: &Path, depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dirs = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, level)) = dirs.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in read.flatten() {
            let path = item.path();
            if path.is_dir() {
                if level < depth {
                    dirs.push((path, level + 1));
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
    out
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Claude Code's home dir. Deliberately independent of GitTerm's config
/// dir override: the index is read-only over harness files and should
/// see the real ones on whichever machine this code runs (desktop or
/// agentd on a remote host).
fn claude_home_dir() -> PathBuf {
    home_dir().join(".claude")
}

/// Build the full local index across all backends. Blocking; run on a
/// background Task.
pub fn build_local_index() -> Vec<ChatIndexEntry> {
    let mut entries = Vec::new();
    for path in jsonl_files_under(&claude_home_dir().join("projects"), 1) {
        if let Some(entry) = index_transcript(&path, ChatBackend::Claude) {
            entries.push(entry);
        }
    }
    for path in jsonl_files_under(&home_dir().join(".codex").join("sessions"), 3) {
        if let Some(entry) = index_codex_transcript(&path) {
            entries.push(entry);
        }
    }
    for path in jsonl_files_under(&home_dir().join(".pi").join("agent").join("sessions"), 1) {
        if let Some(entry) = index_pi_transcript(&path) {
            entries.push(entry);
        }
    }

    // One git resolution per unique live cwd, shared across entries.
    let mut roots: HashMap<PathBuf, Option<(PathBuf, bool)>> = HashMap::new();
    for entry in &mut entries {
        if entry.dead_cwd {
            continue;
        }
        let resolved = roots
            .entry(entry.cwd.clone())
            .or_insert_with(|| resolve_repo_root(&entry.cwd));
        if let Some((root, is_worktree)) = resolved {
            entry.repo_root = Some(root.clone());
            entry.is_worktree = *is_worktree;
        }
    }

    entries.sort_by_key(|entry| std::cmp::Reverse(entry.mtime));
    entries
}

/// Parse the preview tail for one chat. Blocking; run on a background Task.
pub fn load_preview(path: &Path, backend: ChatBackend) -> ChatPreview {
    let Ok(lines) = read_tail_lines(path) else {
        return ChatPreview::default();
    };
    let mut messages = Vec::new();
    let mut message_count = None;
    for line in lines.iter().rev() {
        match backend {
            ChatBackend::Claude => {
                let Ok(meta) = serde_json::from_str::<LineMeta>(line) else {
                    continue;
                };
                if message_count.is_none() {
                    message_count = meta.message_count;
                }
                if messages.len() < PREVIEW_MESSAGES && meta.is_real_message() {
                    if let Some(text) = meta.message_text() {
                        if !is_synthetic_user_text(&text) {
                            messages.push(ChatPreviewMessage {
                                is_user: meta.kind.as_deref() == Some("user"),
                                text,
                            });
                        }
                    }
                }
                if messages.len() >= PREVIEW_MESSAGES && message_count.is_some() {
                    break;
                }
            }
            ChatBackend::Codex | ChatBackend::Pi => {
                if messages.len() >= PREVIEW_MESSAGES {
                    break;
                }
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                let parsed = match backend {
                    ChatBackend::Codex => codex_message(&value),
                    _ => pi_message(&value),
                };
                if let Some((is_user, text)) = parsed {
                    if !is_synthetic_user_text(&text) {
                        messages.push(ChatPreviewMessage { is_user, text });
                    }
                }
            }
        }
    }
    messages.reverse();
    ChatPreview {
        messages,
        message_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_transcript(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn user_line(cwd: &str, branch: &str, text: &str) -> String {
        serde_json::json!({
            "type": "user", "cwd": cwd, "gitBranch": branch,
            "message": {"role": "user", "content": text}
        })
        .to_string()
    }

    #[test]
    fn index_takes_first_real_user_message_as_title() {
        let dir = tempfile::tempdir().unwrap();
        let caveat = user_line(
            "/tmp/x",
            "main",
            "<local-command-caveat>ignore</local-command-caveat>",
        );
        let real = user_line("/tmp/x", "main", "fix the flaky test in ci");
        let path = write_transcript(
            dir.path(),
            "abc-123.jsonl",
            &[
                r#"{"type":"mode","mode":"normal"}"#,
                caveat.as_str(),
                real.as_str(),
            ],
        );
        let entry = index_transcript(&path, ChatBackend::Claude).unwrap();
        assert_eq!(entry.title, "fix the flaky test in ci");
        assert_eq!(entry.id, "abc-123");
        assert_eq!(entry.cwd, PathBuf::from("/tmp/x"));
        assert_eq!(entry.branch.as_deref(), Some("main"));
    }

    #[test]
    fn index_prefers_summary_line_over_user_text() {
        let dir = tempfile::tempdir().unwrap();
        let user = user_line("/tmp/x", "main", "hello there");
        let path = write_transcript(
            dir.path(),
            "s.jsonl",
            &[
                r#"{"type":"summary","summary":"Fix CI flake"}"#,
                user.as_str(),
            ],
        );
        let entry = index_transcript(&path, ChatBackend::Claude).unwrap();
        assert_eq!(entry.title, "Fix CI flake");
    }

    #[test]
    fn index_skips_sessions_without_real_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            "empty.jsonl",
            &[r#"{"type":"mode","mode":"normal"}"#],
        );
        assert!(index_transcript(&path, ChatBackend::Claude).is_none());
    }

    #[test]
    fn index_skips_headless_sdk_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let sdk_user = serde_json::json!({
            "type": "user", "entrypoint": "sdk-cli", "cwd": "/tmp/x", "gitBranch": "main",
            "message": {"role": "user", "content": "You are reviewing code for a push."}
        })
        .to_string();
        let path = write_transcript(
            dir.path(),
            "hook.jsonl",
            &[r#"{"type":"queue-operation"}"#, sdk_user.as_str()],
        );
        assert!(index_transcript(&path, ChatBackend::Claude).is_none());
    }

    fn codex_meta_line(id: &str, cwd: &str, branch: &str, originator: &str) -> String {
        serde_json::json!({
            "type": "session_meta",
            "payload": {"id": id, "cwd": cwd, "originator": originator,
                        "git": {"branch": branch}}
        })
        .to_string()
    }

    fn codex_user_line(text: &str) -> String {
        serde_json::json!({
            "type": "response_item",
            "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": text}]}
        })
        .to_string()
    }

    #[test]
    fn codex_index_reads_session_meta_and_first_user_message() {
        let dir = tempfile::tempdir().unwrap();
        let meta = codex_meta_line("019e-abc", "/tmp/x", "feat/thing", "codex-tui");
        let dev = serde_json::json!({
            "type": "response_item",
            "payload": {"type": "message", "role": "developer",
                        "content": [{"type": "input_text", "text": "<permissions instructions>"}]}
        })
        .to_string();
        let user = codex_user_line("why is the backfill wrong");
        let path = write_transcript(
            dir.path(),
            "rollout-x.jsonl",
            &[meta.as_str(), dev.as_str(), user.as_str()],
        );
        let entry = index_codex_transcript(&path).unwrap();
        assert_eq!(entry.id, "019e-abc");
        assert_eq!(entry.backend, ChatBackend::Codex);
        assert_eq!(entry.branch.as_deref(), Some("feat/thing"));
        assert_eq!(entry.title, "why is the backfill wrong");
        assert_eq!(entry.resume_command(), "codex resume 019e-abc");
    }

    #[test]
    fn codex_index_skips_agents_md_instruction_message_for_title() {
        let dir = tempfile::tempdir().unwrap();
        let meta = codex_meta_line("019e-ghi", "/tmp/x", "HEAD", "codex-tui");
        let docs = codex_user_line("# AGENTS.md instructions for /tmp/x\n...");
        let real = codex_user_line("fix the login redirect");
        let path = write_transcript(
            dir.path(),
            "rollout-docs.jsonl",
            &[meta.as_str(), docs.as_str(), real.as_str()],
        );
        let entry = index_codex_transcript(&path).unwrap();
        assert_eq!(entry.title, "fix the login redirect");
        // Detached-HEAD "branch" is dropped rather than shown.
        assert_eq!(entry.branch, None);
    }

    #[test]
    fn codex_index_skips_exec_runs() {
        let dir = tempfile::tempdir().unwrap();
        let meta = codex_meta_line("019e-def", "/tmp/x", "main", "codex-exec");
        let user = codex_user_line("automated run");
        let path = write_transcript(
            dir.path(),
            "rollout-exec.jsonl",
            &[meta.as_str(), user.as_str()],
        );
        assert!(index_codex_transcript(&path).is_none());
    }

    #[test]
    fn pi_index_reads_session_line_and_first_user_message() {
        let dir = tempfile::tempdir().unwrap();
        let session = serde_json::json!({
            "type": "session", "version": 3, "id": "019f-xyz", "cwd": "/tmp/y"
        })
        .to_string();
        let user = serde_json::json!({
            "type": "message",
            "message": {"role": "user",
                        "content": [{"type": "text", "text": "sketch the reattach plan"}]}
        })
        .to_string();
        let path = write_transcript(
            dir.path(),
            "2026_019f-xyz.jsonl",
            &[session.as_str(), user.as_str()],
        );
        let entry = index_pi_transcript(&path).unwrap();
        assert_eq!(entry.id, "019f-xyz");
        assert_eq!(entry.backend, ChatBackend::Pi);
        assert_eq!(entry.title, "sketch the reattach plan");
        assert_eq!(entry.resume_command(), "pi --session 019f-xyz");
    }

    #[test]
    fn codex_preview_orders_tail_messages() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = vec![codex_meta_line("019e-p", "/tmp/x", "main", "codex-tui")];
        for i in 0..12 {
            lines.push(codex_user_line(&format!("turn {i}")));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let path = write_transcript(dir.path(), "rollout-p.jsonl", &refs);
        let preview = load_preview(&path, ChatBackend::Codex);
        assert_eq!(preview.messages.len(), PREVIEW_MESSAGES);
        assert_eq!(preview.messages.last().unwrap().text, "turn 11");
    }

    #[test]
    fn long_titles_truncate() {
        let long = "x".repeat(200);
        let title = title_from_text(&long);
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn preview_returns_last_messages_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines: Vec<String> = Vec::new();
        for i in 0..20 {
            lines.push(user_line("/tmp/x", "main", &format!("message {i}")));
        }
        lines.push(
            serde_json::json!({"type":"system","subtype":"turn_duration","messageCount":40})
                .to_string(),
        );
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let path = write_transcript(dir.path(), "p.jsonl", &refs);
        let preview = load_preview(&path, ChatBackend::Claude);
        assert_eq!(preview.message_count, Some(40));
        assert_eq!(preview.messages.len(), PREVIEW_MESSAGES);
        assert_eq!(preview.messages.last().unwrap().text, "message 19");
        assert!(preview.messages[0].is_user);
    }

    #[test]
    #[ignore = "scans the real ~/.claude of this machine; run manually with --ignored"]
    fn real_index_smoke() {
        let started = std::time::Instant::now();
        let entries = build_local_index();
        eprintln!("indexed {} chats in {:?}", entries.len(), started.elapsed());
        for entry in entries.iter().take(12) {
            eprintln!(
                "  {} | {:8} | {:24} | wt={} dead={} | {}",
                format_age(entry.mtime),
                entry.branch.as_deref().unwrap_or("-"),
                entry.group_name(),
                entry.is_worktree,
                entry.dead_cwd,
                entry.title,
            );
        }
    }

    #[test]
    fn workspace_membership_covers_worktrees() {
        let entry = ChatIndexEntry {
            id: "e".into(),
            backend: ChatBackend::Claude,
            path: PathBuf::from("/t/e.jsonl"),
            cwd: PathBuf::from("/private/tmp/repo-worktree"),
            repo_root: Some(PathBuf::from("/Users/me/GitRepo/repo")),
            is_worktree: true,
            branch: None,
            title: "t".into(),
            mtime: SystemTime::UNIX_EPOCH,
            size: 1,
            dead_cwd: false,
        };
        assert!(entry.in_workspace(Path::new("/Users/me/GitRepo/repo")));
        assert!(!entry.in_workspace(Path::new("/Users/me/GitRepo/other")));
    }
}
