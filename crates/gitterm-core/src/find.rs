//! Read-only Find services. All filesystem work is blocking.
//!
//! The caller supplies source roots; no user home or config is discovered here.
//! Roots preserve the historical walker scope, including directory symlinks and
//! recorded-cwd Git probes. They are not a containment boundary.

use crate::chats::{ChatBackend, ChatIndexEntry, ChatPreview, ChatPreviewMessage};
use serde::Deserialize;
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Bytes read from the end of a transcript for the preview tail.
const TAIL_READ_BYTES: u64 = 128 * 1024;
/// Messages shown in the preview tail.
const PREVIEW_MESSAGES: usize = 8;

/// Maximum transcript tail bytes read for a ranking snippet.
pub const SNIPPET_TAIL_BYTES: u64 = 64 * 1024;

/// Fields claude writes on most transcript lines. Unknown fields ignored.
// Preview-only builds retain the metadata fields so both modes parse the same line shape.
#[cfg_attr(not(feature = "runtime"), allow(dead_code))]
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
    timestamp: Option<String>,
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

/// Complete JSONL lines from the last `TAIL_READ_BYTES` of the file.
fn read_tail_lines(path: &Path) -> std::io::Result<Vec<String>> {
    read_tail_lines_bounded(path, TAIL_READ_BYTES)
}

/// Complete JSONL lines from the last `max_bytes` of the file: at most
/// `max_bytes` are read, and the first line is dropped when the read
/// starts mid-file (it is a partial line).
fn read_tail_lines_bounded(path: &Path, max_bytes: u64) -> std::io::Result<Vec<String>> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    // `take` keeps the read bounded even if the file grows meanwhile.
    let mut bytes = Vec::new();
    file.take(max_bytes).read_to_end(&mut bytes)?;
    // A seek can land mid-UTF-8; that partial first line is dropped below.
    let buf = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = buf.lines().map(str::to_string).collect();
    // The first line is truncated unless we started at 0.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    Ok(lines)
}

/// A codex `response_item` message: (is_user, text). Codex wraps turns as
/// `{"type":"response_item","payload":{"type":"message","role":...,
/// "content":[{"type":"input_text"|"output_text","text":...}]}}`.
#[doc(hidden)]
pub fn codex_message(value: &Value) -> Option<(bool, String)> {
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

/// Whether a codex `session_meta` payload marks a subagent rollout —
/// a conversation codex spawned for itself (`source: {"subagent": …}`,
/// e.g. the "guardian" approval arbiter) rather than a human chat.
/// Human sessions carry `source: "cli"`; older rollouts have no source.
#[doc(hidden)]
pub fn codex_meta_is_subagent(payload: &Value) -> bool {
    payload
        .get("source")
        .is_some_and(|source| source.get("subagent").is_some())
}

/// Parse the preview tail for one chat. Blocking; run on a background Task.
pub fn load_preview(path: &Path, backend: ChatBackend) -> ChatPreview {
    let Ok(lines) = read_tail_lines(path) else {
        return ChatPreview::default();
    };
    preview_from_lines(&lines, backend)
}

/// The preview messages in the last [`SNIPPET_TAIL_BYTES`] of a
/// transcript, parsed as [`load_preview`] parses its tail. Blocking; run
/// on a background Task. An unreadable file is an error for the caller
/// to report; a tail with no conversation text is an empty preview.
pub fn load_snippet_preview(path: &Path, backend: ChatBackend) -> std::io::Result<ChatPreview> {
    let lines = read_tail_lines_bounded(path, SNIPPET_TAIL_BYTES)?;
    Ok(preview_from_lines(&lines, backend))
}

/// The last [`PREVIEW_MESSAGES`] conversation messages of `lines` (oldest
/// first), and claude's message count when the tail carries one.
fn preview_from_lines(lines: &[String], backend: ChatBackend) -> ChatPreview {
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

/// Chats in their existing order, filtered by workspace, backend and lowercase query.
/// The host chooses the workspace root from its scope/machine selection.
pub fn filter_chat_entries<'a>(
    entries: &'a [ChatIndexEntry],
    ws_root: Option<&Path>,
    backend_filter: Option<ChatBackend>,
    query: &str,
) -> Vec<&'a ChatIndexEntry> {
    entries
        .iter()
        .filter(|entry| ws_root.is_none_or(|dir| entry.in_workspace(dir)))
        .filter(|entry| backend_filter.is_none_or(|backend| entry.backend == backend))
        .filter(|entry| entry.matches_query(query))
        .collect()
}

#[cfg(feature = "runtime")]
pub use index::{build_local_index, codex_subagent_conversations_under, resolve_repo_root};

#[cfg(feature = "runtime")]
mod index {
    use super::*;
    use crate::plan_limits::jsonl_files_under;
    use std::collections::{HashMap, HashSet};
    use std::io::{BufRead, BufReader};
    use std::path::PathBuf;
    use std::time::SystemTime;

    /// Max bytes scanned for cwd/branch and the initial title scan. A single
    /// pasted-content line can be huge; this bounds the metadata scan.
    const HEAD_SCAN_BYTES: u64 = 256 * 1024;
    /// Max lines scanned for metadata and the initial title.
    const HEAD_SCAN_LINES: usize = 60;
    /// Max bytes searched for a transcript title. Metadata remains limited to
    /// `HEAD_SCAN_BYTES` and `HEAD_SCAN_LINES`.
    const TITLE_SCAN_BYTES: u64 = 2 * 1024 * 1024;
    /// Bytes read from the end of a transcript for Claude's own title. The
    /// CLI rewrites its `ai-title` line through the session, so the end of a
    /// file nearly always holds the current one.
    const TITLE_TAIL_BYTES: u64 = 64 * 1024;
    /// Truncation length for titles derived from the first user message.
    const TITLE_MAX_CHARS: usize = 90;

    /// Entrypoint used by native chat tabs.
    const SDK_TS_ENTRYPOINT: &str = "sdk-ts";

    /// Codex user-role items that are not typed by the user, so never a title:
    /// the AGENTS.md preamble and the wrapper Codex prepends when replaying a
    /// prior request. Only for Codex title selection; an assistant reply may
    /// legitimately start with the same words.
    fn is_codex_non_title(text: &str) -> bool {
        is_synthetic_user_text(text)
            || text.starts_with("# AGENTS.md instructions")
            || text.starts_with("You previously asked the user:")
    }

    fn title_from_text(text: &str) -> String {
        let first_line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or(text);
        let mut title: String = first_line.trim().chars().take(TITLE_MAX_CHARS).collect();
        if first_line.trim().chars().count() > TITLE_MAX_CHARS {
            title.push('…');
        }
        title
    }

    /// Head-of-file metadata scan: cwd, branch, headless state, and any early
    /// title, all within the existing byte and line limits.
    struct HeadScan {
        title: Option<String>,
        cwd: Option<PathBuf>,
        branch: Option<String>,
        /// True when the session was started headlessly (`claude -p` / SDK —
        /// entrypoint "sdk-cli" etc.), e.g. hook-spawned review runs. These
        /// are one-shot automation, not conversations to go back to, so the
        /// index skips them. A missing entrypoint counts as interactive, and
        /// so does `sdk-ts` (GitTerm's own chat tabs).
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
                // GitTerm chat tabs spawn `claude` as `sdk-ts` and are real
                // conversations; hook review runs (`claude -p`) are `sdk-cli`.
                // GitTerm cannot claim its own value: the CLI keys real
                // behaviour (artifact tools, question preview format, client
                // type) on the exact entrypoint, so anything but `sdk-ts`
                // changes the session.
                if entrypoint.starts_with("sdk") && entrypoint != SDK_TS_ENTRYPOINT {
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
            if out.title.is_none() && meta.kind.as_deref() == Some("user") && meta.is_real_message()
            {
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

    /// Find a Claude title beyond the metadata head scan. Only lines that may
    /// contain a summary or user message are JSON-parsed; tool output can be
    /// large, and title search stops at the first title or 2 MiB.
    fn scan_claude_title(path: &Path) -> std::io::Result<Option<String>> {
        let file = std::fs::File::open(path)?;
        let mut reader = BufReader::new(file.take(TITLE_SCAN_BYTES));
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            if !(line.contains("\"user\"") || line.contains("\"summary\"")) {
                continue;
            }
            let Ok(meta) = serde_json::from_str::<LineMeta>(&line) else {
                continue;
            };
            if meta.kind.as_deref() == Some("summary") {
                if let Some(summary) = meta
                    .summary
                    .as_deref()
                    .filter(|summary| !summary.is_empty())
                {
                    return Ok(Some(summary.to_string()));
                }
            }
            if meta.kind.as_deref() == Some("user") && meta.is_real_message() {
                if let Some(text) = meta.message_text() {
                    if !is_synthetic_user_text(&text) {
                        return Ok(Some(title_from_text(&text)));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Find a Codex rollout title beyond its metadata head scan.
    fn scan_codex_title(path: &Path) -> std::io::Result<Option<String>> {
        let file = std::fs::File::open(path)?;
        let mut reader = BufReader::new(file.take(TITLE_SCAN_BYTES));
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            if !line.contains("\"user\"") {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some((true, text)) = codex_message(&value) {
                if !is_codex_non_title(&text) {
                    return Ok(Some(title_from_text(&text)));
                }
            }
        }
        Ok(None)
    }

    /// Find a pi session title beyond its metadata head scan.
    fn scan_pi_title(path: &Path) -> std::io::Result<Option<String>> {
        let file = std::fs::File::open(path)?;
        let mut reader = BufReader::new(file.take(TITLE_SCAN_BYTES));
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            if !line.contains("\"user\"") {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some((true, text)) = pi_message(&value) {
                if !is_synthetic_user_text(&text) {
                    return Ok(Some(title_from_text(&text)));
                }
            }
        }
        Ok(None)
    }

    /// The latest title Claude generated for a session (`{"type":"ai-title",
    /// "aiTitle":…}` lines), if `lines` hold one.
    fn claude_ai_title(lines: &[String]) -> Option<String> {
        lines.iter().rev().find_map(|line| {
            if !line.contains("\"ai-title\"") {
                return None;
            }
            let value: Value = serde_json::from_str(line).ok()?;
            if value.get("type")?.as_str()? != "ai-title" {
                return None;
            }
            let title = value.get("aiTitle")?.as_str()?.trim();
            (!title.is_empty()).then(|| title.to_string())
        })
    }

    /// When the newest real user or assistant message among `lines` was
    /// written. Reopening a chat appends bookkeeping lines (cost-state,
    /// bridge-session, pr-link), which move the file's modification time
    /// without anyone working in it, so recency comes from here (TRU-170).
    fn last_message_time(lines: &[String]) -> Option<SystemTime> {
        lines.iter().rev().find_map(|line| {
            let meta = serde_json::from_str::<LineMeta>(line).ok()?;
            if !matches!(meta.kind.as_deref(), Some("user" | "assistant"))
                || !meta.is_real_message()
            {
                return None;
            }
            let stamp = chrono::DateTime::parse_from_rfc3339(meta.timestamp.as_deref()?).ok()?;
            Some(SystemTime::from(stamp))
        })
    }

    /// The newest real user message among `lines`, title-formatted. Reuses
    /// the same tail read as [`claude_ai_title`], so finding it costs no
    /// extra I/O.
    fn latest_user_title(lines: &[String]) -> Option<String> {
        lines.iter().rev().find_map(|line| {
            let meta = serde_json::from_str::<LineMeta>(line).ok()?;
            if meta.kind.as_deref() != Some("user") || !meta.is_real_message() {
                return None;
            }
            let text = meta.message_text()?;
            if is_synthetic_user_text(&text) {
                return None;
            }
            Some(title_from_text(&text))
        })
    }

    /// Scan one transcript into an index entry. Returns None for sessions
    /// with no real user message (empty shells from aborted launches).
    fn index_transcript(path: &Path, backend: ChatBackend) -> Option<ChatIndexEntry> {
        let meta = std::fs::metadata(path).ok()?;
        let head = scan_head(path).ok()?;
        if head.headless {
            return None;
        }
        // A session with no real user message is an empty shell, even if
        // Claude titled it.
        let first_message_title = match head.title {
            Some(title) => title,
            None => scan_claude_title(path).ok()??,
        };
        let tail = read_tail_lines_bounded(path, TITLE_TAIL_BYTES).unwrap_or_default();
        let title = claude_ai_title(&tail).unwrap_or(first_message_title);
        let latest = latest_user_title(&tail).filter(|line| *line != title);
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
            latest,
            mtime: last_message_time(&tail).or_else(|| meta.modified().ok())?,
            size: meta.len(),
            dead_cwd,
            facts: None,
            root_id: None,
        })
    }

    /// Index one codex rollout. Returns None for headless `codex exec` runs
    /// (originator carries "exec"), subagent rollouts (approval arbiters and
    /// similar harness-internal sessions), and sessions with no real user
    /// message.
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
                if codex_meta_is_subagent(payload) {
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
                    if !is_codex_non_title(&text) {
                        title = Some(title_from_text(&text));
                    }
                }
            }
            if id.is_some() && title.is_some() {
                break;
            }
        }
        if title.is_none() {
            title = scan_codex_title(path).ok().flatten();
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
            latest: None,
            mtime: meta.modified().ok()?,
            size: meta.len(),
            dead_cwd,
            facts: None,
            root_id: None,
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
        if title.is_none() {
            title = scan_pi_title(path).ok().flatten();
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
            latest: None,
            mtime: meta.modified().ok()?,
            size: meta.len(),
            dead_cwd,
            facts: None,
            root_id: None,
        })
    }

    /// Resolved git identity of a cwd: (main-repo root, is_worktree).
    pub fn resolve_repo_root(cwd: &Path) -> Option<(PathBuf, bool)> {
        let out = crate::git_status::git_command()
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

    pub fn codex_subagent_conversations_under(
        dir: &Path,
        ids: &HashSet<String>,
    ) -> HashSet<String> {
        let mut subagents = HashSet::new();
        if ids.is_empty() {
            return subagents;
        }
        // Rollout files are named `rollout-<timestamp>-<session-id>.jsonl`.
        for path in jsonl_files_under(dir, 3) {
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Some(id) = ids.iter().find(|id| stem.ends_with(id.as_str())) else {
                continue;
            };
            if subagents.contains(id) {
                continue;
            }
            let Ok(file) = std::fs::File::open(&path) else {
                continue;
            };
            let mut reader = BufReader::new(file.take(HEAD_SCAN_BYTES));
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let is_subagent = serde_json::from_str::<Value>(&line).is_ok_and(|value| {
                value.get("type").and_then(Value::as_str) == Some("session_meta")
                    && value.get("payload").is_some_and(codex_meta_is_subagent)
            });
            if is_subagent {
                subagents.insert(id.clone());
            }
        }
        subagents
    }

    /// Build the local index under the supplied harness home, without ambient discovery.
    pub fn build_local_index(home: &Path) -> Vec<ChatIndexEntry> {
        let mut entries = Vec::new();
        for path in jsonl_files_under(&home.join(".claude").join("projects"), 1) {
            if let Some(entry) = index_transcript(&path, ChatBackend::Claude) {
                entries.push(entry);
            }
        }
        for path in jsonl_files_under(&home.join(".codex").join("sessions"), 3) {
            if let Some(entry) = index_codex_transcript(&path) {
                entries.push(entry);
            }
        }
        for path in jsonl_files_under(&home.join(".pi").join("agent").join("sessions"), 1) {
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn fixture_index_uses_only_selected_home_and_never_changes_sources() {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let absent_cwd = temp.path().join("absent-cwd");
            let cwd = absent_cwd.to_str().unwrap();
            let claude = home.join(".claude/projects/fixture/claude-id.jsonl");
            let codex = home.join(".codex/sessions/2026/01/02/rollout-codex-id.jsonl");
            let pi = home.join(".pi/agent/sessions/fixture/pi-id.jsonl");
            let sources = [
                (
                    claude.clone(),
                    user_line(cwd, "fixture-branch", "Synthetic Claude"),
                ),
                (
                    codex.clone(),
                    format!(
                        "{}\n{}",
                        codex_meta_line("codex-id", cwd, "fixture-branch", "codex_cli_rs"),
                        codex_user_line("Synthetic Codex")
                    ),
                ),
                (
                    pi.clone(),
                    format!(
                        "{}\n{}",
                        serde_json::json!({"type":"session","id":"pi-id","cwd":cwd}),
                        serde_json::json!({"type":"message","message":{"role":"user","content":"Synthetic Pi"}})
                    ),
                ),
                // Directory depth and headless exclusions are part of the source scope.
                (
                    home.join(".claude/projects/fixture/subagents/hidden.jsonl"),
                    user_line(cwd, "main", "Too deep"),
                ),
                (
                    home.join(".codex/sessions/2026/01/02/rollout-hidden.jsonl"),
                    format!(
                        "{}\n{}",
                        codex_meta_line("hidden", cwd, "main", "codex_exec"),
                        codex_user_line("Headless")
                    ),
                ),
            ];
            let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
            for (path, body) in &sources {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, body).unwrap();
                std::fs::File::options()
                    .write(true)
                    .open(path)
                    .unwrap()
                    .set_modified(at)
                    .unwrap();
                let mut permissions = std::fs::metadata(path).unwrap().permissions();
                permissions.set_readonly(true);
                std::fs::set_permissions(path, permissions).unwrap();
            }
            let mut entries = build_local_index(&home);
            entries.sort_by(|a, b| a.id.cmp(&b.id));
            assert_eq!(
                entries
                    .iter()
                    .map(|e| (e.id.as_str(), e.backend, e.title.as_str()))
                    .collect::<Vec<_>>(),
                vec![
                    ("claude-id", ChatBackend::Claude, "Synthetic Claude"),
                    ("codex-id", ChatBackend::Codex, "Synthetic Codex"),
                    ("pi-id", ChatBackend::Pi, "Synthetic Pi"),
                ]
            );
            assert!(entries
                .iter()
                .all(|e| e.dead_cwd && e.repo_root.is_none() && e.mtime == at));
            for (path, body) in &sources {
                assert_eq!(std::fs::read(path).unwrap(), body.as_bytes());
                let meta = std::fs::metadata(path).unwrap();
                assert_eq!(meta.modified().unwrap(), at);
                assert!(meta.permissions().readonly());
            }
            assert_eq!(jsonl_files_under(&home, 8).len(), sources.len());
            assert!(build_local_index(&temp.path().join("other-home")).is_empty());
            assert!(!temp.path().join("other-home").exists());
        }

        #[test]
        fn malformed_oversized_and_missing_transcripts_keep_distinct_read_contracts() {
            let temp = tempfile::tempdir().unwrap();
            let malformed =
                write_transcript(temp.path(), "broken.jsonl", &["not json", "{\"type\":"]);
            assert!(index_transcript(&malformed, ChatBackend::Claude).is_none());
            assert_eq!(
                load_preview(&malformed, ChatBackend::Claude),
                ChatPreview::default()
            );
            assert_eq!(
                load_snippet_preview(&malformed, ChatBackend::Claude).unwrap(),
                ChatPreview::default()
            );
            let oversized = temp.path().join("oversized.jsonl");
            let body = user_line(
                "/synthetic/missing",
                "main",
                &"x".repeat(TITLE_SCAN_BYTES as usize + 1),
            );
            std::fs::write(&oversized, &body).unwrap();
            assert!(index_transcript(&oversized, ChatBackend::Claude).is_none());
            assert_eq!(
                load_preview(&oversized, ChatBackend::Claude),
                ChatPreview::default()
            );
            assert_eq!(
                load_snippet_preview(&oversized, ChatBackend::Claude).unwrap(),
                ChatPreview::default()
            );
            assert_eq!(std::fs::read(&oversized).unwrap(), body.as_bytes());
            let missing = temp.path().join("missing.jsonl");
            for backend in ChatBackend::ALL {
                assert_eq!(load_preview(&missing, backend), ChatPreview::default());
                assert_eq!(
                    load_snippet_preview(&missing, backend).unwrap_err().kind(),
                    std::io::ErrorKind::NotFound
                );
            }
            assert!(index_transcript(&missing, ChatBackend::Claude).is_none());
            assert!(index_codex_transcript(&missing).is_none());
            assert!(index_pi_transcript(&missing).is_none());
        }

        #[test]
        fn snippet_tail_keeps_small_complete_messages_after_an_oversized_line() {
            let temp = tempfile::tempdir().unwrap();
            let user = user_line("/synthetic/missing", "main", "last ask");
            let reply =
                r#"{"type":"assistant","message":{"role":"assistant","content":"last reply"}}"#;
            let path = write_transcript(
                temp.path(),
                "snippet.jsonl",
                &[&"x".repeat(SNIPPET_TAIL_BYTES as usize + 1), &user, reply],
            );
            let preview = load_snippet_preview(&path, ChatBackend::Claude).unwrap();
            assert_eq!(
                preview.messages,
                vec![
                    ChatPreviewMessage {
                        is_user: true,
                        text: "last ask".into()
                    },
                    ChatPreviewMessage {
                        is_user: false,
                        text: "last reply".into()
                    }
                ]
            );
        }

        #[test]
        fn replay_wrapper_is_excluded_only_from_codex_titles() {
            let wrapper = "You previously asked the user: which branch?";
            // Previews and snippets keep an assistant reply with these words.
            assert!(!is_synthetic_user_text(wrapper));
            assert!(is_codex_non_title(wrapper));
            assert!(is_codex_non_title("# AGENTS.md instructions for /repo"));
            assert!(!is_codex_non_title("do we need to run the checks?"));
        }

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
        fn index_prefers_claudes_latest_generated_title() {
            let dir = tempfile::tempdir().unwrap();
            let real = user_line("/tmp/x", "main", "it is just coming to my attention");
            let path = write_transcript(
                dir.path(),
                "abc-456.jsonl",
                &[
                    real.as_str(),
                    r#"{"type":"ai-title","aiTitle":"First guess","sessionId":"abc-456"}"#,
                    r#"{"type":"assistant","message":{"role":"assistant","content":"ok"}}"#,
                    r#"{"type":"ai-title","aiTitle":"Org snapshot billing period regression","sessionId":"abc-456"}"#,
                ],
            );
            let entry = index_transcript(&path, ChatBackend::Claude).unwrap();
            assert_eq!(entry.title, "Org snapshot billing period regression");
        }

        #[test]
        fn index_surfaces_the_latest_user_message_when_it_differs() {
            let dir = tempfile::tempdir().unwrap();
            let opener = user_line("/tmp/x", "main", "what logging do we have for studio beta");
            let newer = user_line(
                "/tmp/x",
                "main",
                "now enforce verified org context in the api",
            );
            let path = write_transcript(
                dir.path(),
                "abc-789.jsonl",
                &[
                    opener.as_str(),
                    r#"{"type":"assistant","message":{"role":"assistant","content":"ok"}}"#,
                    newer.as_str(),
                ],
            );
            let entry = index_transcript(&path, ChatBackend::Claude).unwrap();
            assert_eq!(entry.title, "what logging do we have for studio beta");
            assert_eq!(
                entry.latest.as_deref(),
                Some("now enforce verified org context in the api")
            );
        }

        #[test]
        fn index_dates_a_chat_by_its_last_message_not_reopen_bookkeeping() {
            let dir = tempfile::tempdir().unwrap();
            let opener = r#"{"type":"user","cwd":"/tmp/x","timestamp":"2026-10-01T18:00:00.000Z","message":{"role":"user","content":"check the regions"}}"#;
            let reply = r#"{"type":"assistant","timestamp":"2026-10-01T18:27:10.077Z","message":{"role":"assistant","content":"done"}}"#;
            let worker = r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-10-02T09:00:00.000Z","message":{"role":"assistant","content":"sub"}}"#;
            // A later reopen writes bookkeeping only.
            let reopen = r#"{"type":"pr-link","timestamp":"2026-10-26T14:45:07.793Z"}"#;
            let path = write_transcript(
                dir.path(),
                "abc-901.jsonl",
                &[opener, reply, worker, reopen, r#"{"type":"cost-state"}"#],
            );
            let entry = index_transcript(&path, ChatBackend::Claude).unwrap();
            let expected =
                chrono::DateTime::parse_from_rfc3339("2026-10-01T18:27:10.077Z").unwrap();
            assert_eq!(entry.mtime, SystemTime::from(expected));
        }

        #[test]
        fn index_dates_a_chat_without_message_timestamps_by_the_file() {
            let dir = tempfile::tempdir().unwrap();
            let only = user_line("/tmp/x", "main", "no timestamps here");
            let path = write_transcript(dir.path(), "abc-902.jsonl", &[only.as_str()]);
            let entry = index_transcript(&path, ChatBackend::Claude).unwrap();
            let file_time = std::fs::metadata(&path).unwrap().modified().unwrap();
            assert_eq!(entry.mtime, file_time);
        }

        #[test]
        fn index_omits_latest_when_the_session_never_moved_on() {
            let dir = tempfile::tempdir().unwrap();
            let only = user_line("/tmp/x", "main", "fix the flaky test in ci");
            let path = write_transcript(
                dir.path(),
                "abc-790.jsonl",
                &[
                    only.as_str(),
                    r#"{"type":"assistant","message":{"role":"assistant","content":"ok"}}"#,
                ],
            );
            let entry = index_transcript(&path, ChatBackend::Claude).unwrap();
            assert_eq!(entry.latest, None);
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
        fn index_finds_first_user_title_after_large_tool_result_lines() {
            let dir = tempfile::tempdir().unwrap();
            let large_result = serde_json::json!({
                "type": "tool-result",
                "content": "x".repeat(80 * 1024)
            })
            .to_string();
            let real = user_line("/tmp/x", "main", "message after large tool output");
            let mut transcript = vec![
                r#"{"type":"system","cwd":"/tmp/x","gitBranch":"main"}"#.to_string(),
                large_result.clone(),
                large_result.clone(),
                large_result.clone(),
                large_result,
                real,
            ]
            .join("\n");
            transcript.push('\n');
            assert!(transcript.find("message after large tool output").unwrap() > 300 * 1024);
            let path = dir.path().join("large.jsonl");
            std::fs::write(&path, transcript).unwrap();

            let entry = index_transcript(&path, ChatBackend::Claude).unwrap();
            assert_eq!(entry.title, "message after large tool output");
        }

        #[test]
        fn title_scans_accept_whitespace_variants_after_large_lines_for_all_backends() {
            let dir = tempfile::tempdir().unwrap();
            let padding = serde_json::json!({
                "type": "tool-result",
                "content": "x".repeat(80 * 1024)
            })
            .to_string();

            let claude_user = user_line("/tmp/x", "main", "Claude title past the head limit")
                .replace("\"type\":\"user\"", "\"type\" : \"user\"");
            let mut claude_lines =
                vec![r#"{"type":"system","cwd":"/tmp/x","gitBranch":"main"}"#.to_string()];
            claude_lines.extend(std::iter::repeat_n(padding.clone(), 4));
            claude_lines.push(claude_user);
            let claude_path = dir.path().join("claude-whitespace.jsonl");
            let claude_transcript = claude_lines.join("\n");
            assert!(
                claude_transcript
                    .find("Claude title past the head limit")
                    .unwrap()
                    > 300 * 1024
            );
            std::fs::write(&claude_path, claude_transcript).unwrap();
            assert_eq!(
                index_transcript(&claude_path, ChatBackend::Claude)
                    .unwrap()
                    .title,
                "Claude title past the head limit"
            );

            let mut codex_lines = vec![codex_meta_line(
                "019e-white-codex",
                "/tmp/x",
                "main",
                "codex-tui",
            )];
            codex_lines.extend(std::iter::repeat_n(padding.clone(), 4));
            codex_lines.push(
                codex_user_line("Codex title past the head limit")
                    .replace("\"role\":\"user\"", "\"role\":  \"user\""),
            );
            let codex_transcript = codex_lines.join("\n");
            assert!(
                codex_transcript
                    .find("Codex title past the head limit")
                    .unwrap()
                    > 300 * 1024
            );
            let codex_path = dir.path().join("codex-whitespace.jsonl");
            std::fs::write(&codex_path, codex_transcript).unwrap();
            assert_eq!(
                index_codex_transcript(&codex_path).unwrap().title,
                "Codex title past the head limit"
            );

            let pi_session = serde_json::json!({
                "type": "session", "version": 3, "id": "019e-white-pi", "cwd": "/tmp/x"
            })
            .to_string();
            let pi_user = serde_json::json!({
                "type": "message",
                "message": {"role": "user", "content": "Pi title past the head limit"}
            })
            .to_string()
            .replace("\"role\":\"user\"", "\"role\":  \"user\"");
            let mut pi_lines = vec![pi_session];
            pi_lines.extend(std::iter::repeat_n(padding, 4));
            pi_lines.push(pi_user);
            let pi_transcript = pi_lines.join("\n");
            assert!(pi_transcript.find("Pi title past the head limit").unwrap() > 300 * 1024);
            let pi_path = dir.path().join("pi-whitespace.jsonl");
            std::fs::write(&pi_path, pi_transcript).unwrap();
            assert_eq!(
                index_pi_transcript(&pi_path).unwrap().title,
                "Pi title past the head limit"
            );
        }

        #[test]
        fn codex_index_skips_synthetic_previous_request_wrapper_for_title() {
            let dir = tempfile::tempdir().unwrap();
            let meta = codex_meta_line("019e-wrapper", "/tmp/x", "main", "codex-tui");
            let wrapper = codex_user_line(
                "You previously asked the user: please continue from this generated context",
            );
            let real = codex_user_line("Use the real user request as the title");
            let path = write_transcript(
                dir.path(),
                "rollout-wrapper.jsonl",
                &[meta.as_str(), wrapper.as_str(), real.as_str()],
            );

            let entry = index_codex_transcript(&path).unwrap();
            assert_eq!(entry.title, "Use the real user request as the title");
        }

        #[test]
        fn index_skips_empty_shell_with_large_non_message_lines() {
            let dir = tempfile::tempdir().unwrap();
            let large_result = serde_json::json!({
                "type": "tool-result",
                "content": "x".repeat(80 * 1024)
            })
            .to_string();
            let mut lines = vec![
                r#"{"type":"system","cwd":"/tmp/x","gitBranch":"main"}"#.to_string(),
                r#"{"type":"queue-operation","content":"/resume"}"#.to_string(),
            ];
            for _ in 0..28 {
                lines.push(large_result.clone());
            }
            lines.push(r#"{"type":"command","content":"/clear"}"#.to_string());
            let path = dir.path().join("empty-large.jsonl");
            std::fs::write(&path, lines.join("\n")).unwrap();

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

        #[test]
        fn index_skips_sdk_py_sessions() {
            let dir = tempfile::tempdir().unwrap();
            let sdk_user = serde_json::json!({
                "type": "user", "entrypoint": "sdk-py", "cwd": "/tmp/x", "gitBranch": "main",
                "message": {"role": "user", "content": "Summarise the build log."}
            })
            .to_string();
            let path = write_transcript(dir.path(), "py.jsonl", &[sdk_user.as_str()]);
            assert!(index_transcript(&path, ChatBackend::Claude).is_none());
        }

        #[test]
        fn index_keeps_gitterm_chat_tab_sessions() {
            let dir = tempfile::tempdir().unwrap();
            let chat_user = serde_json::json!({
                "type": "user", "entrypoint": "sdk-ts", "cwd": "/tmp/x", "gitBranch": "feat/chat",
                "message": {"role": "user", "content": "Why does the backfill skip March?"}
            })
            .to_string();
            let path = write_transcript(
                dir.path(),
                "chat.jsonl",
                &[r#"{"type":"queue-operation"}"#, chat_user.as_str()],
            );
            let entry = index_transcript(&path, ChatBackend::Claude).unwrap();
            assert_eq!(entry.title, "Why does the backfill skip March?");
            assert_eq!(entry.cwd, PathBuf::from("/tmp/x"));
            assert_eq!(entry.branch.as_deref(), Some("feat/chat"));
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
        }

        #[test]
        fn codex_and_pi_title_search_pass_the_head_limits() {
            let dir = tempfile::tempdir().unwrap();
            let large_codex_line = serde_json::json!({
                "type": "function_call_output",
                "output": "x".repeat(80 * 1024)
            })
            .to_string();
            let mut codex_lines =
                vec![codex_meta_line("019e-large", "/tmp/x", "main", "codex-tui")];
            for _ in 0..4 {
                codex_lines.push(large_codex_line.clone());
            }
            codex_lines.push(codex_user_line("Codex message after large tool output"));
            assert!(
                codex_lines
                    .join("\n")
                    .find("Codex message after large tool output")
                    .unwrap()
                    > 300 * 1024
            );
            let codex_path = dir.path().join("rollout-large.jsonl");
            std::fs::write(&codex_path, codex_lines.join("\n")).unwrap();
            let codex_entry = index_codex_transcript(&codex_path).unwrap();
            assert_eq!(codex_entry.title, "Codex message after large tool output");

            let pi_session = serde_json::json!({
                "type": "session", "version": 3, "id": "019f-large", "cwd": "/tmp/x"
            })
            .to_string();
            let large_pi_line = serde_json::json!({
                "type": "toolResult",
                "content": "x".repeat(80 * 1024)
            })
            .to_string();
            let pi_user = serde_json::json!({
                "type": "message",
                "message": {"role": "user", "content": "Pi message after large tool output"}
            })
            .to_string();
            let mut pi_lines = vec![pi_session];
            for _ in 0..4 {
                pi_lines.push(large_pi_line.clone());
            }
            pi_lines.push(pi_user);
            assert!(
                pi_lines
                    .join("\n")
                    .find("Pi message after large tool output")
                    .unwrap()
                    > 300 * 1024
            );
            let pi_path = dir.path().join("pi-large.jsonl");
            std::fs::write(&pi_path, pi_lines.join("\n")).unwrap();
            let pi_entry = index_pi_transcript(&pi_path).unwrap();
            assert_eq!(pi_entry.title, "Pi message after large tool output");
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

        fn codex_subagent_meta_line(id: &str) -> String {
            serde_json::json!({
                "type": "session_meta",
                "payload": {"id": id, "cwd": "/tmp/x", "originator": "codex-tui",
                            "source": {"subagent": {"other": "guardian"}},
                            "git": {"branch": "main"}}
            })
            .to_string()
        }

        #[test]
        fn codex_index_skips_subagent_rollouts() {
            let dir = tempfile::tempdir().unwrap();
            let meta = codex_subagent_meta_line("019e-arb");
            let user = codex_user_line(
                "The following is the Codex agent history whose request action you are assessing.",
            );
            let path = write_transcript(
                dir.path(),
                "rollout-arbiter.jsonl",
                &[meta.as_str(), user.as_str()],
            );
            assert!(index_codex_transcript(&path).is_none());
        }

        #[test]
        fn codex_subagent_lookup_flags_only_subagent_ids() {
            let dir = tempfile::tempdir().unwrap();
            let arbiter = codex_subagent_meta_line("019e-arb");
            let human = codex_meta_line("019e-hum", "/tmp/x", "main", "codex-tui");
            write_transcript(dir.path(), "rollout-t1-019e-arb.jsonl", &[arbiter.as_str()]);
            write_transcript(dir.path(), "rollout-t2-019e-hum.jsonl", &[human.as_str()]);

            let ids: HashSet<String> = ["019e-arb", "019e-hum", "019e-gone"]
                .into_iter()
                .map(str::to_string)
                .collect();
            let subagents = codex_subagent_conversations_under(dir.path(), &ids);
            assert!(subagents.contains("019e-arb"));
            // Human sessions and missing transcripts are never flagged.
            assert!(!subagents.contains("019e-hum"));
            assert!(!subagents.contains("019e-gone"));
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
        fn bounded_tail_drops_the_partial_first_line() {
            let dir = tempfile::tempdir().unwrap();
            let path = write_transcript(dir.path(), "t.jsonl", &["aaaaaaaa", "bbbb", "cccc"]);
            // The file is 18 bytes ("aaaaaaaa\nbbbb\ncccc"): 15 starts inside "a…".
            assert_eq!(
                read_tail_lines_bounded(&path, 15).unwrap(),
                ["bbbb", "cccc"]
            );
            // The whole file: nothing dropped.
            assert_eq!(
                read_tail_lines_bounded(&path, 1024).unwrap(),
                ["aaaaaaaa", "bbbb", "cccc"]
            );
            // Starting exactly on a line boundary still drops one line: the
            // reader cannot tell, and a lost line only shortens the tail.
            assert_eq!(read_tail_lines_bounded(&path, 9).unwrap(), ["cccc"]);
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
    }
}

/// Pure conversation browser orchestration.
pub mod state;
