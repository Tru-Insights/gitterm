//! Per-chat facts (TRU-166, step 1 of the work-first workspaces plan,
//! `.plans/work-first-ux/design.md` "Files").
//!
//! One small JSON file per conversation,
//! `<config>/chat-meta/<backend>-<session_id>.json`, says what the chat
//! touched: issue keys, repos and checkouts it edited or worked in, and the
//! child sessions it started (subagents, workers, reviews, consults). The
//! Chats index merges these files (a directory listing and a small read
//! each) so search covers keys, repos and children, and children fold
//! under their root chat, without ever parsing a transcript on the UI path.
//!
//! Writers:
//! - the chat tab hosting the conversation, from the tool-call frames it
//!   already renders ([`LiveFacts`]), debounced and off the UI thread; one
//!   writer per file;
//! - the explicit "Index older chats" action ([`backfill_claude`]), once
//!   per transcript, streaming existing Claude transcripts (subagent
//!   transcripts included). It skips chats a tab is hosting;
//! - the hosting tab once more when it picks a chat up (TRU-201): a chat
//!   that moved on elsewhere (another instance, the CLI) has its transcript
//!   scanned in the tab's own write slot ([`LiveFacts::request_catch_up`],
//!   [`flush_live`]), so the stretch no live writer saw is recorded too.
//!
//! Paths resolve to repo / checkout / branch with the TRU-146 rules in
//! `worktree_follow` (candidate extraction, `list_worktrees`,
//! `containing_worktree`). Reads (Read, Glob, Grep) never count; a Bash
//! `cd` / `git -C` records the checkout without counting an edit.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::chats::{ChatBackend, ChatIndexEntry};
use crate::harness::HarnessEvent;
use crate::merge_plan::{self, PrRef};
use crate::tasks::{Delegation, DelegationChild, DelegationKind, TaskCreatorKind, TaskRecord};
use crate::worktree_follow::{self, ToolInputTracker, WorktreeInfo};

/// Directory under the GitTerm config dir that holds the facts files.
pub const FACTS_DIR_NAME: &str = "chat-meta";
/// A live tab writes its facts at most this often while a turn runs; a
/// finished turn writes at once.
pub const LIVE_FLUSH_DEBOUNCE: Duration = Duration::from_secs(20);
/// Transcripts one "Index older chats" run scans, newest first. Pressing
/// it again continues with the next batch.
pub const BACKFILL_BATCH: usize = 200;
/// Child titles kept per child (subagent descriptions, review briefs).
const CHILD_TITLE_MAX_CHARS: usize = 90;

/// What one conversation touched. Every field defaults for older facts files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatFacts {
    /// Issue keys the chat mentioned or created through a `linear issue
    /// create` Bash result containing a Linear issue URL.
    #[serde(default)]
    pub issue_keys: Vec<String>,
    #[serde(default)]
    pub repos_touched: Vec<RepoTouch>,
    #[serde(default)]
    pub checkouts: Vec<CheckoutTouch>,
    #[serde(default)]
    pub children: Vec<ChatChild>,
    /// PRs the chat or its delegated workers created: a `gh pr create`
    /// result naming a GitHub PR URL (TRU-173, TRU-257).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prs: Vec<PrRef>,
    /// Repositories where the chat ran a recognized git/GitHub write
    /// command (TRU-222). Paths are resolved to the main checkout.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_work: Vec<PathBuf>,
    /// Markdown files the chat wrote or edited (TRU-191: Write, Edit,
    /// MultiEdit on a `.md` / `.markdown` path), absolute, oldest first.
    /// At most [`DOCUMENTS_CAP`], the newest kept. Inside git or not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documents: Vec<PathBuf>,
    /// "Index older chats" has scanned this chat's transcript; it never
    /// scans it again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub backfilled: bool,
    /// The backfill scan that set `backfilled` also recorded `documents`.
    /// A chat backfilled before TRU-191 lacks it and is scanned once more.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub documents_backfilled: bool,
}

/// Markdown paths kept per chat; older ones drop off first.
pub const DOCUMENTS_CAP: usize = 200;

/// Edit tools whose target counts as a document the chat wrote.
const DOCUMENT_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit"];

/// Whether `path` names a markdown file (`.md` / `.markdown`, any case).
pub fn is_markdown_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

/// A repository (main checkout dir) the chat worked in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoTouch {
    pub dir: PathBuf,
    /// Edit / Write / MultiEdit / NotebookEdit calls inside the repo, any
    /// checkout. Zero when the chat only ran commands there.
    pub edits: u64,
}

/// One checkout (main or linked worktree) the chat worked in; the branch
/// ownership source for step 2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutTouch {
    /// The repository's main checkout.
    pub repo: PathBuf,
    pub dir: PathBuf,
    /// Short branch name when last seen; `None` for a detached HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub edits: u64,
    /// RFC 3339 UTC time of the latest touch.
    pub last: String,
}

/// A session this chat started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatChild {
    /// Harness conversation id of the child; for a Claude subagent, the
    /// Agent tool-use id that launched it (the transcript's `toolUseId`).
    pub session_id: String,
    pub kind: ChildKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// What the child was asked to do (subagent description, review
    /// brief). Children with no index entry of their own are searched by
    /// this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildKind {
    Subagent,
    Worker,
    Review,
    Consult,
}

impl ChildKind {
    const ORDER: [ChildKind; 4] = [
        ChildKind::Subagent,
        ChildKind::Worker,
        ChildKind::Review,
        ChildKind::Consult,
    ];

    fn noun(self, count: usize) -> String {
        let word = match self {
            ChildKind::Subagent => "subagent",
            ChildKind::Worker => "worker",
            ChildKind::Review => "review",
            ChildKind::Consult => "consult",
        };
        if count == 1 {
            format!("1 {word}")
        } else {
            format!("{count} {word}s")
        }
    }
}

/// How a tool call touched a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchKind {
    /// A file edit (Edit, Write, MultiEdit, NotebookEdit).
    Edit,
    /// A Bash command run in the directory (leading `cd` / `git -C`).
    Visit,
}

/// A path resolved to its repository, checkout and branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub repo: PathBuf,
    pub checkout: PathBuf,
    pub branch: Option<String>,
}

impl ChatFacts {
    pub fn is_empty(&self) -> bool {
        self.issue_keys.is_empty()
            && self.repos_touched.is_empty()
            && self.checkouts.is_empty()
            && self.children.is_empty()
            && self.prs.is_empty()
            && self.repo_work.is_empty()
            && self.documents.is_empty()
    }

    /// Records a markdown file the chat wrote now: it moves to the newest
    /// end, and the oldest drop off past [`DOCUMENTS_CAP`].
    pub fn add_document(&mut self, path: &Path) {
        self.documents.retain(|known| known != path);
        self.documents.push(path.to_path_buf());
        let over = self.documents.len().saturating_sub(DOCUMENTS_CAP);
        self.documents.drain(..over);
    }

    /// Records a PR the chat created, once.
    pub fn add_pr(&mut self, pr: PrRef) {
        let id = pr.key().id();
        if !self.prs.iter().any(|known| known.key().id() == id) {
            self.prs.push(pr);
        }
    }

    pub fn add_issue_key(&mut self, key: &str) {
        if !self.issue_keys.iter().any(|known| known == key) {
            self.issue_keys.push(key.to_string());
        }
    }

    /// Counts one touch of `resolved` at `at` (RFC 3339). The branch and
    /// checkout folder name contribute issue keys for an edit, or for any
    /// touch of a linked worktree.
    pub fn record_touch(&mut self, resolved: &Resolved, kind: TouchKind, at: &str) {
        let edit = u64::from(kind == TouchKind::Edit);
        match self
            .repos_touched
            .iter_mut()
            .find(|repo| repo.dir == resolved.repo)
        {
            Some(repo) => repo.edits += edit,
            None => self.repos_touched.push(RepoTouch {
                dir: resolved.repo.clone(),
                edits: edit,
            }),
        }
        match self
            .checkouts
            .iter_mut()
            .find(|checkout| checkout.repo == resolved.repo && checkout.dir == resolved.checkout)
        {
            Some(checkout) => {
                checkout.edits += edit;
                if resolved.branch.is_some() {
                    checkout.branch.clone_from(&resolved.branch);
                }
                if later(at, &checkout.last) {
                    checkout.last = at.to_string();
                }
            }
            None => self.checkouts.push(CheckoutTouch {
                repo: resolved.repo.clone(),
                dir: resolved.checkout.clone(),
                branch: resolved.branch.clone(),
                edits: edit,
                last: at.to_string(),
            }),
        }
        // A main checkout's branch is whatever it has checked out now; it
        // names this chat's work only when the chat edits there. A linked
        // worktree is made for one piece of work, so it always counts.
        if edit == 0 && resolved.checkout == resolved.repo {
            return;
        }
        let names = resolved
            .branch
            .iter()
            .map(String::as_str)
            .chain(resolved.checkout.file_name().and_then(|name| name.to_str()));
        for name in names {
            for key in issue_keys_in_name(name) {
                self.add_issue_key(&key);
            }
        }
    }

    /// Adds a child, or fills the fields a known one lacks.
    pub fn add_child(&mut self, child: ChatChild) {
        match self
            .children
            .iter_mut()
            .find(|known| known.session_id == child.session_id)
        {
            Some(known) => {
                if known.delegation_id.is_none() {
                    known.delegation_id = child.delegation_id;
                }
                if known.task_id.is_none() {
                    known.task_id = child.task_id;
                }
                if known.title.is_none() {
                    known.title = child.title;
                }
            }
            None => self.children.push(child),
        }
    }

    /// Merges a full-transcript scan into facts a tab may already have
    /// written. The scan saw everything the tab saw, so counts take the
    /// larger of the two rather than the sum.
    pub fn absorb_scan(&mut self, scan: ChatFacts) {
        for key in &scan.issue_keys {
            self.add_issue_key(key);
        }
        for touch in scan.repos_touched {
            match self.repos_touched.iter_mut().find(|r| r.dir == touch.dir) {
                Some(known) => known.edits = known.edits.max(touch.edits),
                None => self.repos_touched.push(touch),
            }
        }
        for touch in scan.checkouts {
            match self
                .checkouts
                .iter_mut()
                .find(|c| c.repo == touch.repo && c.dir == touch.dir)
            {
                Some(known) => {
                    known.edits = known.edits.max(touch.edits);
                    if later(&touch.last, &known.last) {
                        known.last = touch.last;
                        if touch.branch.is_some() {
                            known.branch = touch.branch;
                        }
                    } else if known.branch.is_none() {
                        known.branch = touch.branch;
                    }
                }
                None => self.checkouts.push(touch),
            }
        }
        for child in scan.children {
            self.add_child(child);
        }
        for pr in scan.prs {
            self.add_pr(pr);
        }
        for dir in scan.repo_work {
            if !self.repo_work.contains(&dir) {
                self.repo_work.push(dir);
            }
        }
        for key in scan.issue_keys {
            self.add_issue_key(&key);
        }
        // The scan saw the whole transcript in order: its documents are the
        // newest; any only a tab recorded keep their place before them.
        if !scan.documents.is_empty() {
            let mut merged: Vec<PathBuf> = self
                .documents
                .drain(..)
                .filter(|known| !scan.documents.contains(known))
                .collect();
            merged.extend(scan.documents);
            let over = merged.len().saturating_sub(DOCUMENTS_CAP);
            merged.drain(..over);
            self.documents = merged;
        }
    }

    /// Children per kind, in display order, kinds with none left out.
    pub fn child_counts(&self) -> Vec<(ChildKind, usize)> {
        ChildKind::ORDER
            .iter()
            .map(|kind| {
                (
                    *kind,
                    self.children.iter().filter(|c| c.kind == *kind).count(),
                )
            })
            .filter(|(_, count)| *count > 0)
            .collect()
    }

    /// "38 subagents · 2 workers · 1 review"; `None` without children.
    pub fn fan_out_line(&self) -> Option<String> {
        let parts: Vec<String> = self
            .child_counts()
            .into_iter()
            .map(|(kind, count)| kind.noun(count))
            .collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// Folder names of the repos touched (visited or edited), most edited
    /// first.
    pub fn repo_names(&self) -> Vec<String> {
        self.names_of_repos(false)
    }

    /// Folder names of the repos the chat edited, most edited first. A repo
    /// it only `cd`'d into is not listed (the "+N repos" fact).
    pub fn edited_repo_names(&self) -> Vec<String> {
        self.names_of_repos(true)
    }

    fn names_of_repos(&self, edited_only: bool) -> Vec<String> {
        let mut repos: Vec<&RepoTouch> = self
            .repos_touched
            .iter()
            .filter(|repo| !edited_only || repo.edits > 0)
            .collect();
        repos.sort_by_key(|repo| std::cmp::Reverse(repo.edits));
        repos
            .into_iter()
            .filter_map(|repo| folder_name(&repo.dir))
            .collect()
    }

    /// Folder names a repo search matches: every repo touched and every
    /// checkout (worktree folders often name the work, e.g.
    /// `blobfs-gateway-prj-102`).
    pub fn searchable_folder_names(&self) -> impl Iterator<Item = String> + '_ {
        self.repos_touched
            .iter()
            .map(|repo| repo.dir.as_path())
            .chain(self.checkouts.iter().map(|checkout| checkout.dir.as_path()))
            .filter_map(folder_name)
    }
}

fn folder_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// Whether RFC 3339 time `a` is after `b`. Unparseable values compare as
/// text, so a broken stamp never panics; an empty `b` is always earlier.
fn later(a: &str, b: &str) -> bool {
    if b.is_empty() {
        return true;
    }
    match (
        chrono::DateTime::parse_from_rfc3339(a),
        chrono::DateTime::parse_from_rfc3339(b),
    ) {
        (Ok(a), Ok(b)) => a > b,
        _ => a > b,
    }
}

/// Now, in the stamp format facts use.
pub fn now_stamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A transcript line's `timestamp`, normalised to the facts format.
fn normalize_stamp(raw: &str) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
    Some(
        parsed
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

// ---------------------------------------------------------------------------
// Issue keys: the generic Linear/Jira key shape `[A-Z][A-Z0-9]{1,9}-[0-9]{1,7}`,
// on word boundaries.

/// Prefixes that match the key shape but name encodings, hashes and
/// standards, never issues (`UTF-8`, `SHA-256`).
const NON_ISSUE_PREFIXES: &[&str] = &["UTF", "ISO", "SHA", "RFC", "HTTP"];

fn is_non_issue_key(key: &str) -> bool {
    key.split_once('-')
        .is_some_and(|(prefix, _)| NON_ISSUE_PREFIXES.contains(&prefix))
}

/// Issue keys in human text, in order of first appearance. Case-sensitive:
/// prose like "utf-8" is not a key. A key must be a whole token: not a
/// piece of a longer hyphenated name (`WS-ABC-10`, `HOST-PROD-DEV-2`,
/// `DEV-01-x`) or a path (`/a/ABC-12`), and not in `NON_ISSUE_PREFIXES`.
pub fn issue_keys_in_text(text: &str) -> Vec<String> {
    scan_issue_keys(text, true)
}

fn scan_issue_keys(text: &str, whole_token: bool) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let at_boundary = i == 0
            || !(bytes[i - 1].is_ascii_alphanumeric()
                || (whole_token && matches!(bytes[i - 1], b'-' | b'_' | b'.' | b'/')));
        if at_boundary && bytes[i].is_ascii_uppercase() {
            if let Some(len) = key_len_at(&bytes[i..]) {
                let continues = whole_token
                    && matches!(bytes.get(i + len), Some(b'-' | b'_'))
                    && bytes
                        .get(i + len + 1)
                        .is_some_and(|b| b.is_ascii_alphanumeric());
                let key = &text[i..i + len];
                if !continues && !is_non_issue_key(key) && !keys.iter().any(|known| known == key) {
                    keys.push(key.to_string());
                }
                i += len;
                continue;
            }
        }
        i += 1;
    }
    keys
}

/// Issue keys in a branch or folder name, which are usually lowercase
/// (`tracey/tru-166-chat-facts`, `blobfs-prj-102`); returned uppercase.
/// Names are hyphen-joined words, so a key may sit between separators here.
pub fn issue_keys_in_name(name: &str) -> Vec<String> {
    scan_issue_keys(&name.to_ascii_uppercase(), false)
}

/// Length of a key starting at `bytes[0]`, if one does, with nothing
/// alphanumeric right after its digits.
fn key_len_at(bytes: &[u8]) -> Option<usize> {
    let prefix = bytes
        .iter()
        .skip(1)
        .take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        .count();
    if !(1..=9).contains(&prefix) || bytes.get(1 + prefix) != Some(&b'-') {
        return None;
    }
    let start = 2 + prefix;
    let digits = bytes[start..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if !(1..=7).contains(&digits) {
        return None;
    }
    let end = start + digits;
    if bytes.get(end).is_some_and(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    Some(end)
}

// ---------------------------------------------------------------------------
// Tool calls -> raw observations.

const EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];
/// Claude's subagent tool ("Task" in older CLIs).
const SUBAGENT_TOOLS: &[&str] = &["Agent", "Task"];

/// Tools whose calls carry facts.
pub fn is_facts_tool(name: &str) -> bool {
    name == "Bash" || EDIT_TOOLS.contains(&name) || SUBAGENT_TOOLS.contains(&name)
}

/// The paths a tool call touches and how; `None` for reads and every
/// other tool. Candidate paths come from `worktree_follow`.
pub fn tool_call_touches(
    name: &str,
    input: &Value,
    home: Option<&Path>,
) -> Option<(TouchKind, Vec<PathBuf>)> {
    let kind = if EDIT_TOOLS.contains(&name) {
        TouchKind::Edit
    } else if name == "Bash" {
        TouchKind::Visit
    } else {
        return None;
    };
    let paths = worktree_follow::tool_call_candidates(name, input, home);
    (!paths.is_empty()).then_some((kind, paths))
}

/// The child a subagent tool call starts, keyed by the call's id.
pub fn subagent_child(id: &str, name: &str, input: &Value) -> Option<ChatChild> {
    if !SUBAGENT_TOOLS.contains(&name) || id.is_empty() {
        return None;
    }
    Some(ChatChild {
        session_id: id.to_string(),
        kind: ChildKind::Subagent,
        delegation_id: None,
        task_id: None,
        title: input
            .get("description")
            .and_then(Value::as_str)
            .and_then(child_title),
    })
}

fn child_title(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut title: String = line.chars().take(CHILD_TITLE_MAX_CHARS).collect();
    if line.chars().count() > CHILD_TITLE_MAX_CHARS {
        title.push('…');
    }
    Some(title)
}

/// One path a tool call touched, before git resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTouch {
    pub kind: TouchKind,
    pub path: PathBuf,
    pub at: String,
}

/// Facts as observed, before paths are resolved against git. Live tabs
/// and transcript scans both produce these.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawFacts {
    pub touches: Vec<RawTouch>,
    /// Issue keys mentioned by the user, or returned by `linear issue
    /// create` with a Linear URL in live and transcript observations.
    pub issue_keys: Vec<String>,
    pub children: Vec<ChatChild>,
    /// PRs from `gh pr create` results: live tabs, and transcript scans
    /// for a tab catching up ("Index older chats" leaves them out).
    pub prs: Vec<PrRef>,
    /// Invocation-specific write targets, resolved off the UI thread.
    repo_work: Vec<RepoWorkTarget>,
    /// Markdown files written or edited, in call order (TRU-191).
    pub documents: Vec<PathBuf>,
}

impl RawFacts {
    pub fn is_empty(&self) -> bool {
        self.touches.is_empty()
            && self.issue_keys.is_empty()
            && self.children.is_empty()
            && self.prs.is_empty()
            && self.repo_work.is_empty()
            && self.documents.is_empty()
    }

    pub fn observe_tool_call(
        &mut self,
        id: &str,
        name: &str,
        input: &Value,
        home: Option<&Path>,
        at: &str,
    ) {
        self.observe_tool_call_with_cwd(id, name, input, home, None, at);
    }

    fn observe_tool_call_with_cwd(
        &mut self,
        id: &str,
        name: &str,
        input: &Value,
        home: Option<&Path>,
        cwd: Option<&Path>,
        at: &str,
    ) {
        if name == "Bash" {
            if let Some(command) = input.get("command").and_then(Value::as_str) {
                let bash_cwd = match input.get("cwd").and_then(Value::as_str) {
                    Some(path) => command_path(path, cwd, home),
                    None => cwd.map(Path::to_path_buf),
                };
                self.repo_work
                    .extend(repo_write_targets(command, bash_cwd.as_deref(), home));
            }
        }
        if let Some((kind, paths)) = tool_call_touches(name, input, home) {
            if DOCUMENT_TOOLS.contains(&name) {
                self.documents
                    .extend(paths.iter().filter(|path| is_markdown_path(path)).cloned());
            }
            for path in paths {
                self.touches.push(RawTouch {
                    kind,
                    path,
                    at: at.to_string(),
                });
            }
        }
        if let Some(child) = subagent_child(id, name, input) {
            self.add_child(child);
        }
    }

    pub fn observe_user_text(&mut self, text: &str) {
        for key in issue_keys_in_text(text) {
            if !self.issue_keys.contains(&key) {
                self.issue_keys.push(key);
            }
        }
    }

    fn add_child(&mut self, child: ChatChild) {
        match self
            .children
            .iter_mut()
            .find(|known| known.session_id == child.session_id)
        {
            Some(known) => {
                if known.title.is_none() {
                    known.title = child.title;
                }
            }
            None => self.children.push(child),
        }
    }

    fn append(&mut self, other: RawFacts) {
        self.touches.extend(other.touches);
        self.prs.extend(other.prs);
        self.repo_work.extend(other.repo_work);
        self.documents.extend(other.documents);
        for key in other.issue_keys {
            if !self.issue_keys.contains(&key) {
                self.issue_keys.push(key);
            }
        }
        for child in other.children {
            self.add_child(child);
        }
    }

    /// Resolves every touch against git and folds the result into `facts`.
    /// Touches outside any git checkout are dropped.
    pub fn resolve_into(self, facts: &mut ChatFacts, resolver: &mut GitResolver) {
        for key in &self.issue_keys {
            facts.add_issue_key(key);
        }
        for touch in &self.touches {
            if let Some(resolved) = resolver.resolve(&touch.path) {
                facts.record_touch(&resolved, touch.kind, &touch.at);
            }
        }
        for target in &self.repo_work {
            let repo = match target {
                RepoWorkTarget::Directory(path) if path.is_dir() => {
                    resolver.resolve(path).map(|r| r.repo)
                }
                RepoWorkTarget::Directory(_) => None,
                RepoWorkTarget::Github(selector) => resolver.resolve_selector(selector),
            };
            if let Some(repo) = repo {
                if !facts.repo_work.contains(&repo) {
                    facts.repo_work.push(repo);
                }
            }
        }
        for child in self.children {
            facts.add_child(child);
        }
        for pr in self.prs {
            facts.add_pr(pr);
        }
        for key in &self.issue_keys {
            facts.add_issue_key(key);
        }
        for path in &self.documents {
            facts.add_document(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Path resolution (TRU-146 rules).

/// Resolves `path` (already `resolve_for_match`ed) against the worktree
/// list of its repository: the innermost containing worktree is the
/// checkout, the list's first entry (git's main worktree) the repo.
/// A path under `<checkout>/.claude/worktrees/` that is not itself a listed
/// worktree belonged to a removed nested worktree and is dropped rather
/// than credited to the checkout holding it.
pub fn resolve_against(path: &Path, worktrees: &[WorktreeInfo]) -> Option<Resolved> {
    let checkout = worktree_follow::containing_worktree(path, worktrees)?;
    let rel = path.strip_prefix(&checkout.path).ok()?;
    if rel.starts_with(".claude/worktrees") || rel.starts_with(".git") {
        return None;
    }
    let repo = worktrees.first()?;
    Some(Resolved {
        repo: repo.path.clone(),
        checkout: checkout.path.clone(),
        branch: checkout.branch.clone(),
    })
}

/// Git lookups for a batch of touches, cached for the batch: the nearest
/// enclosing checkout per directory, one `git worktree list` per checkout.
/// Blocking; never on the UI thread.
#[derive(Debug, Default)]
pub struct GitResolver {
    listed_repos: Vec<PathBuf>,
    nearest_checkout: HashMap<PathBuf, Option<PathBuf>>,
    worktrees: HashMap<PathBuf, Option<Vec<WorktreeInfo>>>,
}

impl GitResolver {
    pub fn with_repos(repos: &[PathBuf]) -> Self {
        Self {
            listed_repos: repos.to_vec(),
            ..Self::default()
        }
    }

    fn resolve_selector(&mut self, selector: &str) -> Option<PathBuf> {
        let parts: Vec<_> = selector.split('/').collect();
        let (host, owner, name) = match parts.as_slice() {
            [owner, name] => ("github.com", *owner, *name),
            [host, owner, name] => (*host, *owner, *name),
            _ => return None,
        };
        if [host, owner, name].iter().any(|part| part.is_empty()) {
            return None;
        }
        let mut matches = Vec::new();
        for dir in self.listed_repos.clone() {
            let Some(resolved) = self.resolve(&dir) else {
                continue;
            };
            let output = crate::agentd::git::git_command()
                .args(["config", "--get-regexp", r"^remote\..*\.url$"])
                .current_dir(&dir)
                .output();
            let output = match output {
                Ok(output) if output.status.success() => output,
                Ok(output) if output.status.code() == Some(1) => continue, // No remotes.
                Ok(output) => {
                    eprintln!(
                        "[chat-facts] cannot read remotes in {}: {}",
                        dir.display(),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    continue;
                }
                Err(error) => {
                    eprintln!(
                        "[chat-facts] cannot read remotes in {}: {error}",
                        dir.display()
                    );
                    continue;
                }
            };
            let matches_remote = String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                let Some((_, url)) = line.split_once(' ') else {
                    return false;
                };
                crate::gh_identity::parse_repository_slug(url).is_some_and(|slug| {
                    slug.host.eq_ignore_ascii_case(host)
                        && slug.owner.eq_ignore_ascii_case(owner)
                        && slug.name.eq_ignore_ascii_case(name)
                })
            });
            if matches_remote && !matches.contains(&resolved.repo) {
                matches.push(resolved.repo);
            }
        }
        // Multiple independent clones are ambiguous; worktrees of one clone fold above.
        (matches.len() == 1).then(|| matches.remove(0))
    }

    pub fn resolve(&mut self, raw: &Path) -> Option<Resolved> {
        let path = worktree_follow::resolve_for_match(raw);
        let checkout = self.nearest_checkout_of(&path)?;
        let worktrees = self
            .worktrees
            .entry(checkout.clone())
            .or_insert_with(|| match worktree_follow::list_worktrees(&checkout) {
                Ok(list) if !list.is_empty() => Some(list),
                Ok(_) => None,
                Err(error) => {
                    eprintln!("[chat-facts] {error}; touches there are not recorded");
                    None
                }
            })
            .as_deref()?;
        resolve_against(&path, worktrees)
    }

    /// The deepest ancestor of `path` holding a `.git` entry (a directory
    /// for a main checkout, a file for a linked worktree).
    fn nearest_checkout_of(&mut self, path: &Path) -> Option<PathBuf> {
        let dir = if path.is_dir() { path } else { path.parent()? };
        if let Some(known) = self.nearest_checkout.get(dir) {
            return known.clone();
        }
        let found = dir
            .ancestors()
            .find(|ancestor| std::fs::symlink_metadata(ancestor.join(".git")).is_ok())
            .map(Path::to_path_buf);
        self.nearest_checkout
            .insert(dir.to_path_buf(), found.clone());
        found
    }
}

// ---------------------------------------------------------------------------
// Files.

pub fn facts_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(FACTS_DIR_NAME)
}

fn facts_file_name(backend: ChatBackend, session_id: &str) -> Result<String, String> {
    let safe = !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !safe {
        return Err(format!(
            "session id {session_id:?} is not a safe facts file name"
        ));
    }
    Ok(format!("{}-{session_id}.json", backend.label()))
}

pub fn facts_path(dir: &Path, backend: ChatBackend, session_id: &str) -> Result<PathBuf, String> {
    Ok(dir.join(facts_file_name(backend, session_id)?))
}

/// The facts file at `path`; `Ok(None)` when there is none yet.
pub fn load_facts(path: &Path) -> Result<Option<ChatFacts>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("{} is not a facts file: {error}", path.display()))
}

/// Writes `facts` to `path` atomically, creating the directory.
pub fn write_facts(path: &Path, facts: &ChatFacts) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let encoded = serde_json::to_vec_pretty(facts)
        .map_err(|error| format!("cannot encode facts for {}: {error}", path.display()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{} has no file name", path.display()))?;
    let temporary = parent.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&temporary, &encoded)
        .and_then(|()| crate::tasks::replace_file_atomically(&temporary, path))
        .map_err(|error| {
            let _ = std::fs::remove_file(&temporary);
            format!("cannot write {}: {error}", path.display())
        })
}

/// The lock every read-merge-write of the facts file at `path` holds, so
/// the live writer, the PR catch-up and the backfill (each on its own
/// blocking thread) never drop each other's additions. One per path for the
/// process; there is one per chat, so the map stays small.
fn facts_write_lock(path: &Path) -> std::sync::Arc<std::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<HashMap<PathBuf, std::sync::Arc<std::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.entry(path.to_path_buf()).or_default().clone()
}

// ---------------------------------------------------------------------------
// Live writer: the chat tab.

/// A chat tab's facts collector. The tab feeds it the harness events it
/// renders and the prompts it sends; [`LiveFacts::take_due`] hands back
/// what to write, at most every [`LIVE_FLUSH_DEBOUNCE`] (or at once when
/// forced at a turn's end), one write at a time.
#[derive(Debug, Default)]
pub struct LiveFacts {
    tracker: ToolInputTracker,
    pending: RawFacts,
    /// Pending Bash inputs, matched with their own results for PR and
    /// Linear creates (TRU-219, TRU-257).
    bash_commands: HashMap<String, String>,
    last_flush: Option<Instant>,
    in_flight: bool,
    /// The tab picked up a chat whose transcript may have moved on while
    /// it ran elsewhere (TRU-201); the next write scans it first.
    catch_up: bool,
}

impl LiveFacts {
    pub fn observe(&mut self, event: &HarnessEvent, home: Option<&Path>) {
        self.observe_with_cwd(event, home, None);
    }

    pub fn observe_with_cwd(
        &mut self,
        event: &HarnessEvent,
        home: Option<&Path>,
        cwd: Option<&Path>,
    ) {
        if let Some(call) = self.tracker.observe_calls(event, is_facts_tool) {
            self.pending.observe_tool_call_with_cwd(
                &call.id,
                &call.name,
                &call.input,
                home,
                cwd,
                &now_stamp(),
            );
            if call.name == "Bash" {
                if let Some(command) = call.input.get("command").and_then(Value::as_str) {
                    self.bash_commands.insert(call.id, command.to_string());
                }
            }
        }
        self.observe_results(event);
    }

    /// A `gh pr create` result (the call's input may complete with the
    /// same event, so this runs after the tracker).
    fn observe_results(&mut self, event: &HarnessEvent) {
        match event {
            HarnessEvent::SubagentEvent { event, .. } => self.observe_results(event),
            HarnessEvent::ItemCompleted { id, output, .. } => {
                if let Some(command) = self.bash_commands.remove(id) {
                    self.pending.prs.extend(created_prs(&command, output));
                    self.pending
                        .issue_keys
                        .extend(created_linear_issue_keys(&command, output));
                }
            }
            HarnessEvent::ProcessExited { .. } => {
                self.bash_commands.clear();
            }
            _ => {}
        }
    }

    pub fn observe_user_text(&mut self, text: &str) {
        self.pending.observe_user_text(text);
    }

    /// The observations to write now, if any are due and no write is in
    /// flight. The caller must call [`LiveFacts::flush_done`] when the write
    /// lands.
    pub fn take_due(&mut self, now: Instant, force: bool) -> Option<RawFacts> {
        if self.in_flight || self.pending.is_empty() {
            return None;
        }
        let due = force
            || self
                .last_flush
                .is_none_or(|at| now.saturating_duration_since(at) >= LIVE_FLUSH_DEBOUNCE);
        if !due {
            return None;
        }
        self.in_flight = true;
        self.last_flush = Some(now);
        Some(std::mem::take(&mut self.pending))
    }

    pub fn flush_done(&mut self) {
        self.in_flight = false;
    }

    /// The tab picked a chat up (TRU-201): its next write first scans the
    /// transcript if it is newer than the facts file. The scan runs in the
    /// same write slot as the live observations, so the two never race.
    pub fn request_catch_up(&mut self) {
        self.catch_up = true;
    }

    /// A requested catch-up, with whatever observations wait, when no write
    /// is in flight; not debounced. The caller passes the transcript to
    /// [`flush_live`] and calls [`LiveFacts::flush_done`] when it lands.
    pub fn take_catch_up(&mut self, now: Instant) -> Option<RawFacts> {
        if self.in_flight || !self.catch_up {
            return None;
        }
        self.catch_up = false;
        self.in_flight = true;
        self.last_flush = Some(now);
        Some(std::mem::take(&mut self.pending))
    }

    /// Whether observations, or a catch-up, wait for a write.
    pub fn has_pending(&self) -> bool {
        self.catch_up || !self.pending.is_empty()
    }
}

/// Local paths available to live flushing and transcript backfill.
#[derive(Default)]
pub struct FactPaths<'a> {
    pub home: Option<&'a Path>,
    pub repos: &'a [PathBuf],
}

/// Folds a live tab's observations and its delegation children into the
/// chat's facts file. Blocking (git, file I/O); run on a blocking thread.
/// A file that exists but does not parse is left alone and reported.
///
/// `catch_up` is the chat's transcript when the tab has just picked the
/// chat up (TRU-201). If there is no facts file yet, or the transcript is
/// newer than it (the rule "Index older chats" uses), the transcript is
/// scanned and merged as a backfill would ([`ChatFacts::absorb_scan`]);
/// otherwise only PRs are recomputed from the transcript (TRU-252).
/// Then the observations are added, all in this one read, merge
/// and write. Returns the PRs the scan added, which the tab did not see live.
pub fn flush_live(
    dir: &Path,
    backend: ChatBackend,
    session_id: &str,
    pending: RawFacts,
    children: Vec<ChatChild>,
    catch_up: Option<&Path>,
    paths: FactPaths<'_>,
) -> Result<Vec<PrRef>, String> {
    let path = facts_path(dir, backend, session_id)?;
    let lock = facts_write_lock(&path);
    let _held = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let existing = load_facts(&path)?;
    let before = existing.clone().unwrap_or_default();
    let mut resolver = GitResolver::with_repos(paths.repos);
    let (mut facts, caught_up) = match catch_up {
        Some(transcript) => {
            catch_up_from_transcript(existing, &path, transcript, paths.home, &mut resolver)
        }
        None => (existing.unwrap_or_default(), Vec::new()),
    };
    pending.resolve_into(&mut facts, &mut resolver);
    for child in children {
        facts.add_child(child);
    }
    if facts == before {
        return Ok(caught_up);
    }
    write_facts(&path, &facts).map(|()| caught_up)
}

/// Whether a transcript modified at `transcript_mtime` moved on after the
/// facts file at `path` was written. False when there is no file.
fn transcript_is_newer(path: &Path, transcript_mtime: SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .is_ok_and(|written| transcript_mtime > written)
}

/// The catch-up for [`flush_live`]: `existing` (the facts file at `path`,
/// if any) with the transcript absorbed when the file is missing or older
/// than the transcript; PRs are always recomputed. Returns the facts and
/// the PRs the scan added. An unreadable transcript is logged and changes nothing.
fn catch_up_from_transcript(
    existing: Option<ChatFacts>,
    path: &Path,
    transcript: &Path,
    home: Option<&Path>,
    resolver: &mut GitResolver,
) -> (ChatFacts, Vec<PrRef>) {
    let mtime = match std::fs::metadata(transcript).and_then(|meta| meta.modified()) {
        Ok(mtime) => mtime,
        Err(error) => {
            eprintln!(
                "[chat-facts] catch-up cannot read {}: {error}",
                transcript.display()
            );
            return (existing.unwrap_or_default(), Vec::new());
        }
    };
    // Recompute PRs even for a current facts file: this repairs missed creates
    // and false positives from older matchers. Pending live facts merge afterward.
    // Everything else is absorbed only when stale.
    let stale = existing.is_none() || transcript_is_newer(path, mtime);
    let scan = match facts_from_claude_transcript(transcript, home, resolver) {
        Ok(scan) => scan,
        Err(error) => {
            eprintln!(
                "[chat-facts] catch-up cannot read {}: {error}",
                transcript.display()
            );
            return (existing.unwrap_or_default(), Vec::new());
        }
    };
    let mut facts = existing.unwrap_or_default();
    let added: Vec<PrRef> = scan
        .prs
        .iter()
        .filter(|pr| {
            let id = pr.key().id();
            !facts.prs.iter().any(|known| known.key().id() == id)
        })
        .cloned()
        .collect();
    facts.prs = scan.prs.clone();
    if !stale {
        if added.is_empty() {
            return (facts, added);
        }
        eprintln!(
            "[chat-facts] {} PRs caught up from {} ({} new PRs)",
            path.display(),
            transcript.display(),
            added.len()
        );
        return (facts, added);
    }
    facts.absorb_scan(scan);
    // A whole-transcript scan, as "Index older chats" records one.
    facts.backfilled = true;
    facts.documents_backfilled = true;
    eprintln!(
        "[chat-facts] {} caught up from {} ({} new PRs)",
        path.display(),
        transcript.display(),
        added.len()
    );
    (facts, added)
}

/// The newest modification time of a Claude transcript and its subagent
/// transcripts (`<session>/subagents/*.jsonl`), the files
/// [`scan_claude_transcript`] reads. `None` (logged) when the transcript
/// cannot be read.
fn transcript_inputs_modified(transcript: &Path) -> Option<SystemTime> {
    let mut newest = match std::fs::metadata(transcript).and_then(|meta| meta.modified()) {
        Ok(modified) => modified,
        Err(error) => {
            eprintln!(
                "[chat-facts] PR catch-up cannot read {}: {error}",
                transcript.display()
            );
            return None;
        }
    };
    let subagents = transcript.with_extension("").join("subagents");
    if let Ok(read) = std::fs::read_dir(&subagents) {
        for modified in read
            .flatten()
            .filter_map(|entry| entry.metadata().and_then(|meta| meta.modified()).ok())
        {
            newest = newest.max(modified);
        }
    }
    Some(newest)
}

/// Transcripts whose PRs [`catch_up_workspace_prs`] already checked, by the
/// newest modification time of the transcript and its subagent files. shortcut: per process, so each GitTerm launch rescans the
/// owned chats once; persist it in the facts file if launches get slow.
fn prs_checked() -> &'static std::sync::Mutex<HashMap<PathBuf, SystemTime>> {
    static CHECKED: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, SystemTime>>> =
        std::sync::OnceLock::new();
    CHECKED.get_or_init(Default::default)
}

/// PRs belong to the workspace, not the chat (TRU-238): a chat a workspace
/// owns keeps its PRs after a `/clear` moves the work to a new chat. For each
/// Claude chat in `owned` that no tab hosts (`hosted` tabs catch up their own
/// chat when they pick it up), recomputes PRs from the transcript's actual
/// `gh pr create` results, repairing missed creates and old false positives.
/// Blocking; run off the UI thread. Returns how many PRs it added.
pub fn catch_up_workspace_prs(
    entries: &[ChatIndexEntry],
    owned: &HashSet<String>,
    hosted: &HashSet<String>,
    dir: &Path,
    home: Option<&Path>,
) -> usize {
    let mut total = 0;
    for entry in entries {
        if entry.backend != ChatBackend::Claude
            || !owned.contains(&entry.id)
            || hosted.contains(&entry.id)
        {
            continue;
        }
        // The transcript and its subagents' files: either can gain a PR.
        let Some(inputs) = transcript_inputs_modified(&entry.path) else {
            continue;
        };
        let seen = prs_checked()
            .lock()
            .ok()
            .and_then(|checked| checked.get(&entry.path).copied());
        if seen == Some(inputs) {
            continue;
        }
        let path = match facts_path(dir, ChatBackend::Claude, &entry.id) {
            Ok(path) => path,
            Err(error) => {
                eprintln!("[chat-facts] PR catch-up skips {}: {error}", entry.id);
                continue;
            }
        };
        let scan = match scan_claude_transcript(&entry.path, home) {
            Ok(scan) => scan,
            Err(error) => {
                eprintln!(
                    "[chat-facts] PR catch-up cannot read {}: {error}",
                    entry.path.display()
                );
                continue;
            }
        };
        let lock = facts_write_lock(&path);
        let _held = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut facts = match load_facts(&path) {
            Ok(facts) => facts.unwrap_or_default(),
            Err(error) => {
                eprintln!("[chat-facts] PR catch-up leaves {error}");
                continue;
            }
        };
        let previous = facts.prs.clone();
        facts.prs.clear();
        for pr in scan.prs {
            facts.add_pr(pr);
        }
        let added = facts
            .prs
            .iter()
            .filter(|pr| {
                !previous
                    .iter()
                    .any(|known| known.key().id() == pr.key().id())
            })
            .count();
        if facts.prs != previous {
            // Only PRs were recorded, so the file must not look as current
            // as a full scan: keep its old time (a new file gets the epoch),
            // and a later catch-up or backfill still absorbs the rest.
            let written = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if let Err(error) = write_facts(&path, &facts) {
                eprintln!("[chat-facts] PR catch-up {error}");
                continue;
            }
            if let Err(error) = std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(written))
            {
                eprintln!(
                    "[chat-facts] PR catch-up cannot keep {}'s time; a later catch-up may skip its other facts: {error}",
                    path.display()
                );
            }
            eprintln!(
                "[chat-facts] {} PRs caught up from {} ({} new PRs)",
                path.display(),
                entry.path.display(),
                added
            );
            total += added;
        }
        if let Ok(mut checked) = prs_checked().lock() {
            checked.insert(entry.path.clone(), inputs);
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Transcript scan (backfill).

/// Folds one Claude transcript line into `raw`. `main` is false for a
/// subagent transcript, whose user lines are the coordinator's brief, not
/// the human's words.
pub fn observe_claude_line(raw: &mut RawFacts, line: &str, home: Option<&Path>, main: bool) {
    observe_claude_line_with(raw, &mut HashMap::new(), line, home, main, &mut None);
}

/// Shared PR recognition for live facts and worker/Claude transcript adapters.
/// Keep the command paired with its own output; URL mentions alone are not facts.
pub(crate) fn created_prs(command: &str, output: &str) -> Vec<PrRef> {
    if is_gh_pr_create(command) {
        merge_plan::pr_urls_in(output)
    } else {
        Vec::new()
    }
}

/// Add a worker's PRs only to an existing parent's facts, under the same
/// lock as live/backfill writers. Preserve the full-scan time: adding PRs
/// must not make a stale transcript's other facts appear current.
pub(crate) fn record_worker_prs(
    dir: &Path,
    parent: &str,
    prs: &[PrRef],
) -> Result<Option<Vec<PrRef>>, String> {
    for backend in [ChatBackend::Claude, ChatBackend::Codex, ChatBackend::Pi] {
        let path = facts_path(dir, backend, parent)?;
        let lock = facts_write_lock(&path);
        let _held = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(mut facts) = load_facts(&path)? else {
            continue;
        };
        let before = facts.prs.clone();
        for pr in prs {
            facts.add_pr(pr.clone());
        }
        let added: Vec<_> = facts
            .prs
            .iter()
            .filter(|pr| !before.iter().any(|known| known.key().id() == pr.key().id()))
            .cloned()
            .collect();
        if !added.is_empty() {
            let written = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .map_err(|error| {
                    format!("cannot read {} modification time: {error}", path.display())
                })?;
            write_facts(&path, &facts)?;
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(written))
                .map_err(|error| {
                    format!(
                        "cannot preserve {} modification time: {error}",
                        path.display()
                    )
                })?;
        }
        return Ok(Some(added));
    }
    Ok(None)
}

/// Whether the shell actually invokes `gh pr create`, including chains,
/// environment assignments, executable paths and command substitutions.
/// Quoted arguments, comments and heredoc bodies are not command words.
pub fn is_gh_pr_create(command: &str) -> bool {
    ShellScanner(command.chars().peekable(), 0)
        .commands(false)
        .is_some_and(|commands| {
            commands.iter().any(|command| {
                command
                    .words
                    .first()
                    .and_then(|word| word.rsplit('/').next())
                    == Some("gh")
                    && command.words.get(1).map(String::as_str) == Some("pr")
                    && command.words.get(2).map(String::as_str) == Some("create")
            })
        })
}

/// Match a create invocation and its own Bash result together. A combined
/// result cannot disambiguate multiple Linear calls or distinct issue URLs.
fn created_linear_issue_keys(command: &str, output: &str) -> Vec<String> {
    let keys = linear_issue_keys_in(output);
    if keys.len() != 1 {
        return Vec::new();
    }
    let Some(commands) = ShellScanner(command.chars().peekable(), 0).commands(false) else {
        return Vec::new();
    };
    let mut linear = commands.iter().filter_map(|command| {
        (command.words.first()?.rsplit('/').next() == Some("linear"))
            .then_some((command, &command.words[1..]))
    });
    let Some((command, mut args)) = linear.next() else {
        return Vec::new();
    };
    if linear.next().is_some() || command.stdout_redirected {
        return Vec::new();
    }
    // These are the global options of the installed Linear CLI (`linear --help`).
    while let Some(arg) = args.first() {
        if arg == "--workspace" && args.len() >= 2 {
            args = &args[2..];
        } else if arg.starts_with("--workspace=") {
            args = &args[1..];
        } else if arg == "--" {
            args = &args[1..];
            break;
        } else {
            break;
        }
    }
    if args.first().map(String::as_str) != Some("issue")
        || args.get(1).map(String::as_str) != Some("create")
    {
        return Vec::new();
    }
    keys
}

fn shell_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.bytes().enumerate().all(|(i, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (i > 0 && byte.is_ascii_digit())
            })
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RepoWorkTarget {
    Directory(PathBuf),
    Github(String),
}

fn command_path(raw: &str, cwd: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if raw.contains(['$', '\0', '*', '?', '[', '{']) || raw.is_empty() {
        return None;
    }
    let path = Path::new(raw);
    let path = if raw == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home?.join(rest)
    } else if raw.starts_with('~') {
        return None;
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd?.join(path)
    };
    // Resolve the actual transition before folding '..': a missing component
    // must not disappear through lexical normalization.
    if !path.is_dir() {
        return None;
    }
    std::fs::canonicalize(path).ok()
}

/// Shared by live observations and transcript scans. Each literal invocation
/// owns its target; unknown directory state never falls back to the original cwd.
fn repo_write_targets(
    command: &str,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Vec<RepoWorkTarget> {
    let Some(commands) = ShellScanner(command.chars().peekable(), 0).commands(false) else {
        return Vec::new();
    };
    let changes_directory = commands
        .iter()
        .any(|cmd| !cmd.nested && cmd.words.first().is_some_and(|word| word == "cd"));
    // shortcut: shell control flow needs a full parser; skip it until supported.
    // Only an uninterrupted cd && cd && ... chain establishes a conditional cwd.
    let guarded_cd = commands.windows(2).any(|pair| {
        pair[0].separator == "&&"
            && pair[0].words.first().is_none_or(|word| word != "cd")
            && !pair[1].nested
            && pair[1].words.first().is_some_and(|word| word == "cd")
    });
    if guarded_cd {
        return Vec::new();
    }
    if commands.iter().any(|cmd| {
        matches!(cmd.separator.as_str(), "|" | "&")
            || (changes_directory && cmd.separator == "||")
            || cmd.words.iter().any(|word| {
                matches!(
                    word.as_str(),
                    "if" | "then"
                        | "else"
                        | "elif"
                        | "fi"
                        | "for"
                        | "while"
                        | "until"
                        | "do"
                        | "done"
                        | "case"
                        | "esac"
                        | "{"
                        | "}"
                        | "function"
                )
            })
    }) {
        return Vec::new();
    }
    let mut cwd = cwd.map(Path::to_path_buf);
    let mut conditional_dir = false;
    let mut targets = Vec::new();
    for command in commands {
        // Nested groups and substitutions have independent shell directories.
        // Do not flatten their cd operations into the caller's directory state.
        if command.nested {
            // A group's success can select a different outer &&/|| path.
            if conditional_dir {
                cwd = None;
            }
            continue;
        }
        if command.environment {
            // Assignments may override GIT_DIR, GIT_WORK_TREE, GH_REPO, or PATH.
            return targets;
        }
        let Some(executable) = command.words.first().map(String::as_str) else {
            continue;
        };
        let args = &command.words[1..];
        if executable == "cd" {
            cwd = if command.separator == "&&" && args.len() == 1 {
                let Some(dir) = command_path(&args[0], cwd.as_deref(), home) else {
                    return targets;
                };
                Some(dir)
            } else {
                None
            };
            conditional_dir = true;
            continue;
        }
        if matches!(
            executable,
            "eval"
                | "source"
                | "."
                | "pushd"
                | "popd"
                | "exec"
                | "export"
                | "unset"
                | "declare"
                | "typeset"
                | "readonly"
        ) {
            return targets;
        }
        let target = match executable.rsplit('/').next() {
            Some("git") => {
                git_write_target(args, cwd.as_deref(), home).map(RepoWorkTarget::Directory)
            }
            Some("gh") => gh_write_target(args, cwd.as_deref()),
            _ => None,
        };
        if let Some(target) = target {
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        if conditional_dir && command.separator != "&&" {
            cwd = None;
        }
    }
    targets
}

fn git_write_target(args: &[String], cwd: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    // shortcut: skip expansions that could supply directory or read-only flags;
    // add expansion boundaries to the scanner before accepting dynamic arguments.
    if args.iter().any(|arg| arg.contains(['$', '\0'])) {
        return None;
    }
    let mut args = args;
    let mut dir = cwd.map(Path::to_path_buf);
    let mut git_dir = None;
    let mut work_tree = None;
    while let Some(arg) = args.first().filter(|arg| arg.starts_with('-')) {
        let (option, attached) =
            if let Some(value) = arg.strip_prefix("-C").filter(|v| !v.is_empty()) {
                ("-C", Some(value))
            } else if let Some(value) = arg.strip_prefix("-c").filter(|v| !v.is_empty()) {
                ("-c", Some(value))
            } else if let Some((key, value)) = arg.split_once('=') {
                (key, Some(value))
            } else {
                (arg.as_str(), None)
            };
        match option {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env" => {
                let value = match attached {
                    Some(value) => {
                        args = &args[1..];
                        value
                    }
                    None => {
                        let value = args.get(1)?.as_str();
                        args = &args[2..];
                        value
                    }
                };
                match option {
                    "-C" if !value.is_empty() => {
                        dir = Some(command_path(value, dir.as_deref(), home)?);
                    }
                    "--git-dir" => git_dir = Some(value.to_string()),
                    "--work-tree" => work_tree = Some(value.to_string()),
                    _ => {}
                }
            }
            "--no-pager"
            | "--paginate"
            | "-p"
            | "-P"
            | "--no-optional-locks"
            | "--no-replace-objects"
            | "--no-lazy-fetch"
            | "--no-advice"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs" => args = &args[1..],
            _ => return None,
        }
    }
    let (verb, args) = args.split_first()?;
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "--dry-run"))
    {
        return None;
    }
    let writes = match verb.as_str() {
        "commit" => !args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "--short" | "--porcelain" | "--long" | "-z" | "--null"
            )
        }),
        "push" => !args
            .iter()
            .any(|arg| arg.starts_with('-') && !arg.starts_with("--") && arg.contains('n')),
        "switch" => creates_branch(args, &["-c", "-C", "--create", "--force-create"]),
        "checkout" => creates_branch(args, &["-b", "-B"]),
        "branch" => {
            let mut names = 0;
            for arg in args {
                match arg.as_str() {
                    "-f"
                    | "--force"
                    | "--no-force"
                    | "-t"
                    | "--track"
                    | "--track=direct"
                    | "--track=inherit"
                    | "--no-track"
                    | "--create-reflog"
                    | "--no-create-reflog"
                    | "--recurse-submodules"
                    | "--no-recurse-submodules"
                    | "-q"
                    | "--quiet" => {}
                    _ if !arg.starts_with('-') => names += 1,
                    _ => return None,
                }
            }
            (1..=2).contains(&names)
        }
        _ => false,
    };
    if !writes {
        return None;
    }
    let work_tree = match work_tree {
        Some(tree) => Some(command_path(&tree, dir.as_deref(), home)?),
        None => None,
    };
    if let Some(git_dir) = git_dir {
        let git_dir = command_path(&git_dir, dir.as_deref(), home)?;
        // Only a normal checkout's .git has an unambiguous filesystem owner.
        if git_dir.file_name()? != ".git" || !git_dir.is_dir() {
            return None;
        }
        let root = git_dir.parent()?.to_path_buf();
        if work_tree.as_ref().is_some_and(|tree| tree != &root) {
            return None;
        }
        return Some(root);
    }
    if let Some(tree) = work_tree {
        // --work-tree alone does not select another repository's git directory.
        if crate::workspace_repos::checkout_root(&tree)
            != dir
                .as_deref()
                .and_then(crate::workspace_repos::checkout_root)
        {
            return None;
        }
    }
    dir
}

fn creates_branch(args: &[String], options: &[&str]) -> bool {
    let mut args = args.iter();
    let mut creates = false;
    while let Some(arg) = args.next() {
        let key = arg.split('=').next().unwrap_or(arg);
        if options.contains(&key) {
            let name = arg
                .split_once('=')
                .map(|(_, name)| name)
                .or_else(|| args.next().map(String::as_str));
            if creates || !name.is_some_and(|name| !name.is_empty() && !name.starts_with('-')) {
                return false;
            }
            creates = true;
        } else if options
            .iter()
            .any(|option| option.len() == 2 && arg.starts_with(option) && arg.len() > 2)
        {
            if creates {
                return false;
            }
            creates = true;
        } else if key == "--conflict" {
            if !arg.contains('=') && args.next().is_none() {
                return false;
            }
        } else if arg.starts_with('-')
            && !matches!(
                arg.as_str(),
                "-q" | "--quiet"
                    | "-f"
                    | "--force"
                    | "-m"
                    | "--merge"
                    | "-t"
                    | "--track"
                    | "--track=direct"
                    | "--track=inherit"
                    | "--no-track"
                    | "--guess"
                    | "--no-guess"
                    | "--discard-changes"
                    | "--ignore-other-worktrees"
                    | "--recurse-submodules"
                    | "--no-recurse-submodules"
                    | "--progress"
                    | "--no-progress"
            )
        {
            return false;
        }
    }
    creates
}

fn gh_write_target(args: &[String], cwd: Option<&Path>) -> Option<RepoWorkTarget> {
    if args.iter().any(|arg| arg.contains(['$', '\0'])) {
        return None;
    }
    let mut args = args.iter();
    let mut selector = None;
    let mut verbs = Vec::new();
    while let Some(arg) = args.next() {
        if matches!(arg.as_str(), "--help" | "-h" | "--dry-run" | "--web" | "-w") {
            return None;
        }
        if arg == "--repo" || arg == "-R" {
            if selector.is_some() {
                return None;
            }
            selector = Some(args.next()?.clone());
        } else if let Some(value) = arg
            .strip_prefix("--repo=")
            .or_else(|| arg.strip_prefix("-R").filter(|v| !v.is_empty()))
        {
            if selector.is_some() || value.is_empty() {
                return None;
            }
            selector = Some(value.to_string());
        } else if !arg.starts_with('-') && verbs.len() < 2 {
            verbs.push(arg.as_str());
        } else {
            // Consume known create-option values, so text is never parsed as flags/verbs.
            if verbs != ["pr", "create"] {
                return None;
            }
            let key = arg.split('=').next()?;
            if [
                "--assignee",
                "-a",
                "--base",
                "-B",
                "--body",
                "-b",
                "--body-file",
                "-F",
                "--head",
                "-H",
                "--label",
                "-l",
                "--milestone",
                "-m",
                "--project",
                "-p",
                "--recover",
                "--reviewer",
                "-r",
                "--template",
                "-T",
                "--title",
                "-t",
            ]
            .contains(&key)
            {
                if !arg.contains('=') {
                    args.next()?;
                }
            } else if ![
                "--draft",
                "-d",
                "--editor",
                "-e",
                "--fill",
                "-f",
                "--fill-first",
                "--fill-verbose",
                "--no-maintainer-edit",
            ]
            .contains(&key)
            {
                return None;
            }
        }
    }
    if verbs != ["pr", "create"] {
        return None;
    }
    match selector {
        Some(selector) => Some(RepoWorkTarget::Github(selector)),
        None => cwd.map(|dir| RepoWorkTarget::Directory(dir.to_path_buf())),
    }
}

fn absolute_tool_path(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    let path = if raw == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home?.join(rest)
    } else {
        PathBuf::from(raw)
    };
    path.is_absolute()
        .then(|| worktree_follow::resolve_for_match(&path))
}

#[derive(Default)]
struct ShellCommand {
    nested: bool,
    environment: bool,
    separator: String,
    words: Vec<String>,
    stdout_redirected: bool,
}

#[derive(Default)]
struct ShellWord {
    text: String,
    quoted: bool,
    assignment: bool,
    commands: Vec<ShellCommand>,
}

/// A single lexical pass: the same quote/comment/continuation state governs
/// both words and heredocs. Nested substitutions get their own shell context.
/// shortcut: this identifies literal invocations, not full Bash execution;
/// dynamic executables, backticks and arithmetic require a full shell parser.
struct ShellScanner<'a>(std::iter::Peekable<std::str::Chars<'a>>, usize);

impl ShellScanner<'_> {
    fn commands(&mut self, nested: bool) -> Option<Vec<ShellCommand>> {
        // Transcript input must not exhaust the stack through shell nesting.
        self.1 += 1;
        if self.1 > 64 {
            return None;
        }
        let mut commands = Vec::new();
        let mut current = ShellCommand::default();
        let mut children = Vec::new();
        let mut heredocs: Vec<(String, bool, bool)> = Vec::new();
        while let Some(&ch) = self.0.peek() {
            match ch {
                ch if ch.is_whitespace() && ch != '\n' => {
                    self.0.next();
                }
                '#' => {
                    while self.0.peek().is_some_and(|ch| *ch != '\n') {
                        self.0.next();
                    }
                }
                '\n' => {
                    self.0.next();
                    current.separator = ch.to_string();
                    finish_shell_command(&mut commands, &mut current, &mut children);
                    for (delimiter, strip_tabs, quoted) in heredocs.drain(..) {
                        self.heredoc(&delimiter, strip_tabs, quoted)?;
                    }
                }
                ';' | '&' | '|' => {
                    self.0.next();
                    if ch == '&' && self.0.peek() == Some(&'>') {
                        self.redirect(&mut current, &mut heredocs, Some(1))?;
                    } else {
                        current.separator = ch.to_string();
                        if self.0.peek() == Some(&ch) {
                            self.0.next();
                            current.separator.push(ch);
                        }
                        finish_shell_command(&mut commands, &mut current, &mut children);
                    }
                }
                '(' => {
                    self.0.next();
                    children.extend(self.commands(true)?);
                }
                ')' => {
                    self.0.next();
                    if !nested || !heredocs.is_empty() {
                        return None;
                    }
                    finish_shell_command(&mut commands, &mut current, &mut children);
                    self.1 -= 1;
                    return Some(commands);
                }
                '<' | '>' => self.redirect(&mut current, &mut heredocs, None)?,
                _ => {
                    let word = self.word(true)?;
                    if !word.quoted
                        && !word.text.is_empty()
                        && word.text.bytes().all(|byte| byte.is_ascii_digit())
                        && matches!(self.0.peek(), Some('<' | '>'))
                    {
                        self.redirect(&mut current, &mut heredocs, Some(word.text.parse().ok()?))?;
                    } else {
                        children.extend(word.commands);
                        current.nested = nested;
                        current.environment |= word.assignment && current.words.is_empty();
                        if (!word.text.is_empty() || word.quoted)
                            && !(word.assignment && current.words.is_empty())
                        {
                            current.words.push(word.text);
                        }
                    }
                }
            }
        }
        if nested || !heredocs.is_empty() {
            return None;
        }
        finish_shell_command(&mut commands, &mut current, &mut children);
        self.1 -= 1;
        Some(commands)
    }

    fn word(&mut self, expand: bool) -> Option<ShellWord> {
        let mut word = ShellWord::default();
        let mut quote = None;
        while let Some(&ch) = self.0.peek() {
            if quote.is_none() && (ch.is_whitespace() || ";&|()<>".contains(ch)) {
                break;
            }
            self.0.next();
            if quote == Some('\'') {
                if ch == '\'' {
                    quote = None;
                } else {
                    word.text.push(ch);
                }
            } else if ch == '\\' {
                let next = self.0.next()?;
                if next != '\n' {
                    if quote == Some('"') && !"$`\"\\".contains(next) {
                        word.text.push('\\');
                    }
                    word.text.push(next);
                    word.quoted = true;
                }
            } else if Some(ch) == quote {
                quote = None;
            } else if quote.is_none() && matches!(ch, '\'' | '"') {
                quote = Some(ch);
                word.quoted = true;
            } else if expand && ch == '$' && self.0.peek() == Some(&'(') {
                self.0.next();
                if self.0.peek() == Some(&'(') {
                    return None;
                }
                word.commands.extend(self.commands(true)?);
                // Expansion cannot manufacture a literal executable or subcommand.
                word.text.push('\0');
            } else if expand && ch == '`' {
                return None;
            } else {
                word.text.push(ch);
                if ch == '=' && !word.quoted && shell_assignment(&word.text) {
                    word.assignment = true;
                }
            }
        }
        quote.is_none().then_some(word)
    }

    fn redirect(
        &mut self,
        current: &mut ShellCommand,
        heredocs: &mut Vec<(String, bool, bool)>,
        fd: Option<u32>,
    ) -> Option<()> {
        let operator = self.0.next()?;
        let mut heredoc = false;
        let mut strip_tabs = false;
        if self.0.peek() == Some(&operator) {
            self.0.next();
            if operator == '<' {
                if self.0.peek() == Some(&'<') {
                    self.0.next(); // Consume the entire here-string operator.
                } else {
                    heredoc = true;
                    if self.0.peek() == Some(&'-') {
                        self.0.next();
                        strip_tabs = true;
                    }
                }
            }
        }
        if matches!(self.0.peek(), Some('&' | '|')) {
            self.0.next();
        }
        while matches!(self.0.peek(), Some(' ' | '\t')) {
            self.0.next();
        }
        let target = self.word(!heredoc)?;
        if target.text.is_empty() && !target.quoted {
            return None;
        }
        if operator == '>' && fd.unwrap_or(1) == 1 {
            current.stdout_redirected = true;
        }
        if heredoc {
            heredocs.push((target.text, strip_tabs, target.quoted));
        }
        Some(())
    }

    fn heredoc(&mut self, delimiter: &str, strip_tabs: bool, quoted: bool) -> Option<()> {
        loop {
            let mut line = String::new();
            loop {
                let ch = self.0.next();
                if ch.is_none() || ch == Some('\n') {
                    // Only unquoted heredocs join escaped physical lines.
                    if ch.is_some()
                        && !quoted
                        && line.chars().rev().take_while(|ch| *ch == '\\').count() % 2 == 1
                    {
                        line.pop();
                        continue;
                    }
                    break;
                }
                line.push(ch?);
            }
            if (if strip_tabs {
                line.trim_start_matches('\t')
            } else {
                &line
            }) == delimiter
            {
                return Some(());
            }
            self.0.peek()?;
        }
    }
}

fn finish_shell_command(
    commands: &mut Vec<ShellCommand>,
    current: &mut ShellCommand,
    children: &mut Vec<ShellCommand>,
) {
    for child in children.iter_mut() {
        child.stdout_redirected |= current.stdout_redirected;
    }
    if !current.words.is_empty() || current.environment {
        commands.push(std::mem::take(current));
    } else {
        *current = ShellCommand::default();
    }
    commands.append(children);
}

/// Linear issue keys named by `https://linear.app/<workspace>/issue/<KEY>-<n>`
/// URLs. Only a complete, well-formed key is returned.
fn linear_issue_keys_in(text: &str) -> Vec<String> {
    const PREFIX: &str = "https://linear.app/";
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(PREFIX) {
        let after = &rest[at + PREFIX.len()..];
        let Some((workspace, path)) = after.split_once("/issue/") else {
            rest = after;
            continue;
        };
        if workspace.is_empty() || workspace.contains('/') {
            rest = after;
            continue;
        }
        let key_len = path
            .bytes()
            .take_while(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
            .count();
        let valid_key = (2..=10).contains(&key_len)
            && path.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
            && path.as_bytes()[..key_len]
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit());
        if !valid_key || path.as_bytes().get(key_len) != Some(&b'-') {
            rest = after;
            continue;
        }
        let digits_start = key_len + 1;
        let digits = path.as_bytes()[digits_start..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let end = digits_start + digits;
        let valid_end = path
            .as_bytes()
            .get(end)
            .is_none_or(|byte| *byte == b'/' || !byte.is_ascii_alphanumeric());
        if digits > 0 && valid_end {
            let key = path[..end].to_string();
            if !found.contains(&key) {
                found.push(key);
            }
        }
        rest = after;
    }
    found
}

/// [`observe_claude_line`] across a whole transcript: the pending sets hold
/// create Bash calls whose result line has not come yet; URLs are recorded
/// as the live tab would (TRU-201, TRU-219).
fn observe_claude_line_with(
    raw: &mut RawFacts,
    bash_commands: &mut HashMap<String, String>,
    line: &str,
    home: Option<&Path>,
    main: bool,
    cwd: &mut Option<PathBuf>,
) {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let record_type = value.get("type").and_then(Value::as_str);
    if matches!(record_type, Some("user" | "assistant"))
        || (record_type == Some("system")
            && value.get("subtype").and_then(Value::as_str) == Some("init"))
    {
        if let Some(path) = value.get("cwd").and_then(Value::as_str) {
            *cwd = absolute_tool_path(path, home);
        }
    }
    let user_line = main && record_type == Some("user");
    let create_result = !bash_commands.is_empty() && line.contains("\"tool_result\"");
    if !user_line && !create_result && !line.contains("\"tool_use\"") {
        return;
    }
    let kind = value.get("type").and_then(Value::as_str);
    let Some(content) = value.get("message").and_then(|m| m.get("content")) else {
        return;
    };
    if create_result && kind == Some("user") {
        for block in content.as_array().into_iter().flatten() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let id = block.get("tool_use_id").and_then(Value::as_str);
            if let Some(command) = id.and_then(|id| bash_commands.remove(id)) {
                let output = block.get("content").map(tool_result_text);
                raw.prs
                    .extend(created_prs(&command, output.as_deref().unwrap_or("")));
                raw.issue_keys.extend(created_linear_issue_keys(
                    &command,
                    output.as_deref().unwrap_or(""),
                ));
            }
        }
    }
    match kind {
        Some("user") if main => {
            let flag = |key: &str| value.get(key).and_then(Value::as_bool) == Some(true);
            if flag("isMeta") || flag("isSidechain") {
                return;
            }
            let text = match content {
                Value::String(text) => text.clone(),
                Value::Array(blocks) => blocks
                    .iter()
                    .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => return,
            };
            // Harness-injected wrappers (command caveats, reminders).
            if !text.trim_start().starts_with('<') {
                raw.observe_user_text(&text);
            }
        }
        Some("assistant") => {
            let Some(blocks) = content.as_array() else {
                return;
            };
            let at = value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(normalize_stamp)
                .unwrap_or_default();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                let (Some(id), Some(name), Some(input)) = (
                    block.get("id").and_then(Value::as_str),
                    block.get("name").and_then(Value::as_str),
                    block.get("input"),
                ) else {
                    continue;
                };
                raw.observe_tool_call_with_cwd(id, name, input, home, cwd.as_deref(), &at);
                if name == "Bash" {
                    if let Some(command) = input.get("command").and_then(Value::as_str) {
                        bash_commands.insert(id.to_string(), command.to_string());
                    }
                }
            }
        }
        _ => {}
    }
}

/// A tool result's text: a string, or the text blocks of a list.
pub(crate) fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn scan_lines(
    path: &Path,
    raw: &mut RawFacts,
    home: Option<&Path>,
    main: bool,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut line = String::new();
    let mut bash_commands = HashMap::new();
    let mut cwd = None;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => observe_claude_line_with(raw, &mut bash_commands, &line, home, main, &mut cwd),
            // A line that is not UTF-8 is skipped, not fatal.
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => continue,
            Err(error) => return Err(error),
        }
    }
}

/// The child a subagent transcript stands for, from its `.meta.json`
/// sibling (`toolUseId`, `description`); keyed by the file stem when the
/// meta is missing.
fn subagent_from_meta(transcript: &Path) -> Option<ChatChild> {
    let stem = transcript.file_stem()?.to_str()?;
    let meta: Option<Value> = std::fs::read(transcript.with_extension("meta.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let field = |key: &str| {
        meta.as_ref()
            .and_then(|meta| meta.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    Some(ChatChild {
        session_id: field("toolUseId").unwrap_or_else(|| stem.to_string()),
        kind: ChildKind::Subagent,
        delegation_id: None,
        task_id: None,
        title: field("description").as_deref().and_then(child_title),
    })
}

/// Streams a Claude transcript and its subagent transcripts
/// (`<session>/subagents/agent-*.jsonl`) into raw facts. Bounded memory:
/// one line at a time.
pub fn scan_claude_transcript(path: &Path, home: Option<&Path>) -> std::io::Result<RawFacts> {
    let mut raw = RawFacts::default();
    scan_lines(path, &mut raw, home, true)?;
    let subagents = path.with_extension("").join("subagents");
    let read = match std::fs::read_dir(&subagents) {
        Ok(read) => Some(read),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(read) = read {
        let mut files: Vec<PathBuf> = read
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();
        files.sort();
        for file in files {
            let mut sub = RawFacts::default();
            if let Some(child) = subagent_from_meta(&file) {
                sub.add_child(child);
            }
            scan_lines(&file, &mut sub, home, false).map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("cannot read {}: {error}", file.display()),
                )
            })?;
            raw.append(sub);
        }
    }
    Ok(raw)
}

/// Facts derived from a whole Claude transcript.
pub fn facts_from_claude_transcript(
    path: &Path,
    home: Option<&Path>,
    resolver: &mut GitResolver,
) -> std::io::Result<ChatFacts> {
    let raw = scan_claude_transcript(path, home)?;
    let mut facts = ChatFacts::default();
    raw.resolve_into(&mut facts, resolver);
    Ok(facts)
}

/// A transcript "Index older chats" may scan.
#[derive(Debug, Clone)]
pub struct BackfillTarget {
    pub id: String,
    pub path: PathBuf,
    pub mtime: SystemTime,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillReport {
    pub scanned: usize,
    pub already: usize,
    /// Chats a tab is hosting; that tab is their writer.
    pub hosted: usize,
    pub remaining: usize,
    pub failed: usize,
}

impl BackfillReport {
    pub fn summary(&self) -> String {
        let mut line = format!("Indexed {} older chats", self.scanned);
        if self.remaining > 0 {
            line.push_str(&format!(", {} left (run again)", self.remaining));
        }
        if self.failed > 0 {
            line.push_str(&format!(", {} failed (see log)", self.failed));
        }
        line
    }
}

/// "Index older chats": scans up to `max` Claude transcripts, newest
/// first, that have no backfilled facts yet and no hosting tab (`hosted`),
/// and writes their facts (merged with whatever a tab wrote earlier) into
/// `dir`. Blocking; run on a blocking thread.
pub fn backfill_claude(
    mut targets: Vec<BackfillTarget>,
    dir: &Path,
    hosted: &HashSet<String>,
    links: &[ChildLink],
    max: usize,
    paths: FactPaths<'_>,
) -> BackfillReport {
    targets.sort_by_key(|target| std::cmp::Reverse(target.mtime));
    let mut report = BackfillReport::default();
    let mut resolver = GitResolver::with_repos(paths.repos);
    for target in targets {
        if hosted.contains(&target.id) {
            report.hosted += 1;
            continue;
        }
        let path = match facts_path(dir, ChatBackend::Claude, &target.id) {
            Ok(path) => path,
            Err(error) => {
                eprintln!(
                    "[chat-facts] backfill skips {}: {error}",
                    target.path.display()
                );
                report.failed += 1;
                continue;
            }
        };
        let lock = facts_write_lock(&path);
        let _held = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let existing = match load_facts(&path) {
            Ok(existing) => existing,
            Err(error) => {
                eprintln!("[chat-facts] backfill leaves {error}");
                report.failed += 1;
                continue;
            }
        };
        // A backfilled chat is scanned again when its transcript moved on
        // after the facts file was written (a CLI session has no live
        // writer, so this is how it catches up).
        let stale = transcript_is_newer(&path, target.mtime);
        if existing
            .as_ref()
            .is_some_and(|facts| facts.backfilled && facts.documents_backfilled && !stale)
        {
            report.already += 1;
            continue;
        }
        if report.scanned >= max {
            report.remaining += 1;
            continue;
        }
        let mut scan = match facts_from_claude_transcript(&target.path, paths.home, &mut resolver) {
            Ok(scan) => scan,
            Err(error) => {
                eprintln!(
                    "[chat-facts] backfill cannot read {}: {error}",
                    target.path.display()
                );
                report.failed += 1;
                continue;
            }
        };
        // PRs stay a hosting tab's record (TRU-173): indexing old chats
        // does not fill the PRs lists with their history.
        let mut facts = existing.unwrap_or_default();
        facts.prs.retain(|pr| {
            scan.prs
                .iter()
                .any(|known| known.key().id() == pr.key().id())
        });
        scan.prs.clear();
        facts.absorb_scan(scan);
        for link in links.iter().filter(|link| link.parent == target.id) {
            facts.add_child(link.child.clone());
        }
        facts.backfilled = true;
        facts.documents_backfilled = true;
        match write_facts(&path, &facts) {
            Ok(()) => report.scanned += 1,
            Err(error) => {
                eprintln!("[chat-facts] backfill {error}");
                report.failed += 1;
            }
        }
    }
    report
}

// ---------------------------------------------------------------------------
// Parent / child links and the index.

/// A child session named by GitTerm's own records (delegations, tasks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildLink {
    /// Session id of the parent chat.
    pub parent: String,
    pub child: ChatChild,
}

fn delegation_child_kind(kind: DelegationKind) -> ChildKind {
    match kind {
        DelegationKind::Review => ChildKind::Review,
        DelegationKind::Implement => ChildKind::Worker,
        DelegationKind::Consult => ChildKind::Consult,
    }
}

/// Children named by delegation records (`DelegationParent.chat_session_id`,
/// else the parent tab's `session_uid` through `uid_to_chat`) and by tasks
/// a coordinator created (`TaskCreator.session_id` is the creating tab's
/// `session_uid`). A child with no conversation yet is left out until it
/// has one.
pub fn store_links(
    delegations: &[Delegation],
    tasks: &[TaskRecord],
    uid_to_chat: &HashMap<String, String>,
) -> Vec<ChildLink> {
    let task_conversations = |task_id: &str| -> Vec<String> {
        tasks
            .iter()
            .find(|task| task.task_id == task_id)
            .map(|task| {
                task.sessions
                    .iter()
                    .filter_map(|session| session.conversation.as_ref())
                    .map(|conversation| conversation.session_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut links = Vec::new();
    for delegation in delegations {
        let parent = delegation
            .parent
            .chat_session_id
            .clone()
            .or_else(|| uid_to_chat.get(&delegation.parent.session_uid).cloned());
        let Some(parent) = parent else {
            continue;
        };
        let kind = delegation_child_kind(delegation.kind);
        let title = child_title(&delegation.brief);
        let child = |session_id: String, kind: ChildKind, task_id: Option<String>| ChatChild {
            session_id,
            kind,
            delegation_id: Some(delegation.delegation_id.clone()),
            task_id,
            title: title.clone(),
        };
        let children: Vec<ChatChild> = match &delegation.child {
            DelegationChild::CodexReview { thread_id, .. }
            | DelegationChild::CodexConsult { thread_id, .. } => thread_id
                .iter()
                .map(|thread| child(thread.clone(), kind, None))
                .collect(),
            DelegationChild::TaskSession {
                task_id,
                conversation,
                ..
            } => match conversation {
                Some(conversation) => vec![child(
                    conversation.session_id.clone(),
                    kind,
                    Some(task_id.clone()),
                )],
                None => task_conversations(task_id)
                    .into_iter()
                    .map(|session| child(session, kind, Some(task_id.clone())))
                    .collect(),
            },
            DelegationChild::ClaudeSubagent { parent_tool_use_id } => parent_tool_use_id
                .iter()
                .map(|id| child(id.clone(), ChildKind::Subagent, None))
                .collect(),
        };
        links.extend(children.into_iter().map(|child| ChildLink {
            parent: parent.clone(),
            child,
        }));
    }
    for task in tasks {
        let Some(creator) = &task.created_by else {
            continue;
        };
        if creator.kind != TaskCreatorKind::Coordinator {
            continue;
        }
        let Some(parent) = creator
            .session_id
            .as_ref()
            .and_then(|uid| uid_to_chat.get(uid))
        else {
            continue;
        };
        for session in &task.sessions {
            let Some(conversation) = &session.conversation else {
                continue;
            };
            links.push(ChildLink {
                parent: parent.clone(),
                child: ChatChild {
                    session_id: conversation.session_id.clone(),
                    kind: ChildKind::Worker,
                    delegation_id: None,
                    task_id: Some(task.task_id.clone()),
                    title: None,
                },
            });
        }
    }
    links
}

/// The children `links` name for one parent chat.
pub fn links_for_parent(links: &[ChildLink], parent: &str) -> Vec<ChatChild> {
    links
        .iter()
        .filter(|link| link.parent == parent)
        .map(|link| link.child.clone())
        .collect()
}

/// Reads the facts files of `entries` from `dir`: one directory listing,
/// then a small read per chat that has a file. Unreadable files are
/// logged and left out. Blocking; run with the index build.
pub fn attach_facts(entries: &mut [ChatIndexEntry], dir: &Path) {
    let names: HashSet<String> = match std::fs::read_dir(dir) {
        Ok(read) => read
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            eprintln!(
                "[chat-facts] cannot list {}: {error}; chats show without facts",
                dir.display()
            );
            return;
        }
    };
    for entry in entries.iter_mut() {
        let Ok(name) = facts_file_name(entry.backend, &entry.id) else {
            continue;
        };
        if !names.contains(&name) {
            continue;
        }
        match load_facts(&dir.join(&name)) {
            Ok(facts) => entry.facts = facts,
            Err(error) => eprintln!("[chat-facts] {error}"),
        }
    }
}

/// Merges `links` into their parents' facts, fills child titles from the
/// children's own index entries, and sets each child entry's `root_id` to
/// its topmost indexed ancestor. A root is a chat no facts file or link
/// names as a child.
pub fn resolve_tree(entries: &mut [ChatIndexEntry], links: &[ChildLink]) {
    let index: HashMap<String, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| (entry.id.clone(), i))
        .collect();
    for link in links {
        if link.child.session_id == link.parent {
            continue;
        }
        if let Some(&i) = index.get(&link.parent) {
            entries[i]
                .facts
                .get_or_insert_with(ChatFacts::default)
                .add_child(link.child.clone());
        }
    }
    let titles: HashMap<String, String> = entries
        .iter()
        .map(|entry| (entry.id.clone(), entry.title.clone()))
        .collect();
    let mut parent_of: HashMap<String, String> = HashMap::new();
    for entry in entries.iter_mut() {
        let Some(facts) = entry.facts.as_mut() else {
            continue;
        };
        for child in &mut facts.children {
            if child.session_id == entry.id {
                continue;
            }
            if child.title.is_none() {
                child.title = titles.get(&child.session_id).cloned();
            }
            parent_of
                .entry(child.session_id.clone())
                .or_insert_with(|| entry.id.clone());
        }
    }
    for entry in entries.iter_mut() {
        entry.root_id = root_of(&entry.id, &parent_of, &index);
    }
}

/// The topmost indexed ancestor of `id`, if it has one.
fn root_of(
    id: &str,
    parent_of: &HashMap<String, String>,
    index: &HashMap<String, usize>,
) -> Option<String> {
    let mut seen: HashSet<&str> = HashSet::from([id]);
    let mut current = id;
    let mut root = None;
    while let Some(parent) = parent_of.get(current) {
        if !seen.insert(parent.as_str()) {
            break;
        }
        if index.contains_key(parent) {
            root = Some(parent.clone());
        }
        current = parent;
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::ItemKind;
    use serde_json::json;

    const PORCELAIN: &str = "\
worktree /repos/app
branch refs/heads/main

worktree /repos/app/.claude/worktrees/fix-login
branch refs/heads/claude/fix-login

worktree /wt/app-tru-12
branch refs/heads/tracey/tru-12-thing

";

    fn worktrees() -> Vec<WorktreeInfo> {
        worktree_follow::parse_worktree_porcelain(PORCELAIN)
    }

    fn resolved(repo: &str, checkout: &str, branch: Option<&str>) -> Resolved {
        Resolved {
            repo: PathBuf::from(repo),
            checkout: PathBuf::from(checkout),
            branch: branch.map(str::to_string),
        }
    }

    #[test]
    fn issue_keys_follow_the_repo_pattern_on_word_boundaries() {
        assert_eq!(
            issue_keys_in_text("Do PRJ-101 then PRJ-102, not PRJ-101 again (TRU-166)."),
            vec!["PRJ-101", "PRJ-102", "TRU-166"]
        );
        // Lowercase prose, a single-letter prefix, too many digits, glued
        // to letters on either side: none are keys.
        assert!(issue_keys_in_text("utf-8 A-12 AB-12345678 xPRJ-1 PRJ-12x").is_empty());
        assert_eq!(
            issue_keys_in_text("A1-2 and ABCDEFGHIJ-1"),
            vec!["A1-2", "ABCDEFGHIJ-1"]
        );
        assert!(issue_keys_in_text("ABCDEFGHIJK-1").is_empty());
        assert_eq!(
            issue_keys_in_name("tracey/tru-166-chat-facts-search-fold"),
            vec!["TRU-166"]
        );
        assert_eq!(
            issue_keys_in_name("ws-service-prj-102-attest"),
            vec!["PRJ-102"]
        );
        assert!(issue_keys_in_name("live-pipeline-standby").is_empty());
        assert!(issue_keys_in_name("api-server-pr587").is_empty());
    }

    #[test]
    fn non_issue_prefixes_are_not_keys_but_year_like_numbers_are() {
        assert!(issue_keys_in_text("UTF-8 ISO-8601 SHA-1 RFC-2616 HTTP-2").is_empty());
        // Pieces of longer hyphenated names and paths are not keys.
        assert!(
            issue_keys_in_text("WS-ABC-10 HOST-PROD-DEV-2 DEV-01-x /a/ABC-12 a_ABC-1").is_empty()
        );
        assert_eq!(
            issue_keys_in_text("fix ABC-2024 and ABC-123, see ABC-12. (ABC-13)"),
            vec!["ABC-2024", "ABC-123", "ABC-12", "ABC-13"]
        );
    }

    #[test]
    fn repo_names_list_only_repos_with_edits() {
        let facts = ChatFacts {
            repos_touched: vec![
                RepoTouch {
                    dir: "/w/visited".into(),
                    edits: 0,
                },
                RepoTouch {
                    dir: "/w/edited".into(),
                    edits: 2,
                },
            ],
            ..ChatFacts::default()
        };
        assert_eq!(facts.edited_repo_names(), vec!["edited"]);
        assert_eq!(facts.repo_names(), vec!["edited", "visited"]);
        // Search still matches a repo the chat only visited.
        assert!(facts.searchable_folder_names().any(|n| n == "visited"));
    }

    #[test]
    fn edits_and_bash_dirs_touch_reads_do_not() {
        let home = Some(Path::new("/Users/me"));
        assert_eq!(
            tool_call_touches("Edit", &json!({"file_path": "/repos/app/src/x.rs"}), home),
            Some((TouchKind::Edit, vec![PathBuf::from("/repos/app/src/x.rs")]))
        );
        assert_eq!(
            tool_call_touches("NotebookEdit", &json!({"notebook_path": "~/n.ipynb"}), home),
            Some((TouchKind::Edit, vec![PathBuf::from("/Users/me/n.ipynb")]))
        );
        assert_eq!(
            tool_call_touches("Bash", &json!({"command": "git -C /repos/app push"}), home),
            Some((TouchKind::Visit, vec![PathBuf::from("/repos/app")]))
        );
        assert_eq!(
            tool_call_touches("Read", &json!({"file_path": "/repos/app/x"}), home),
            None
        );
        assert_eq!(
            tool_call_touches("Grep", &json!({"path": "/repos/app"}), home),
            None
        );
        assert_eq!(
            tool_call_touches("Bash", &json!({"command": "ls /repos/app"}), home),
            None
        );
    }

    #[test]
    fn write_command_facts_rebuild_from_transcript_like_live_facts() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        let command = "git commit -m update";

        let mut live = LiveFacts::default();
        live.observe_with_cwd(
            &HarnessEvent::ItemStarted {
                id: "call".into(),
                kind: ItemKind::ToolCall {
                    name: "Bash".into(),
                    input: json!({"command": command}),
                },
            },
            None,
            Some(&repo),
        );
        let mut live_facts = ChatFacts::default();
        live.take_due(Instant::now(), true)
            .unwrap()
            .resolve_into(&mut live_facts, &mut GitResolver::default());

        let transcript = temp.path().join("session.jsonl");
        std::fs::write(
            &transcript,
            [
                json!({"type":"system", "subtype":"init", "cwd":repo}),
                json!({"type":"assistant", "message":{"content":[
                    {"type":"tool_use", "id":"call", "name":"Bash", "input":{"command":command}}
                ]}}),
            ]
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();
        let mut rebuilt = ChatFacts::default();
        scan_claude_transcript(&transcript, None)
            .unwrap()
            .resolve_into(&mut rebuilt, &mut GitResolver::default());
        let canonical_repo = std::fs::canonicalize(&repo).unwrap();
        assert_eq!(live_facts.repo_work, vec![canonical_repo]);
        assert_eq!(rebuilt.repo_work, live_facts.repo_work);
        assert!(
            !repo_write_targets("gh pr create --title x", Some(Path::new("/repo")), None)
                .is_empty()
        );
        assert!(
            !repo_write_targets(&format!("git -C '{}' push", repo.display()), None, None)
                .is_empty()
        );
        assert!(repo_write_targets("git status", Some(Path::new("/repo")), None).is_empty());
        assert!(repo_write_targets("git branch -D old", Some(Path::new("/repo")), None).is_empty());
    }

    fn work_repo(root: &Path, name: &str) -> PathBuf {
        let repo = root.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::canonicalize(repo).unwrap()
    }

    fn command_repos(command: &str, cwd: &Path) -> Vec<PathBuf> {
        let mut raw = RawFacts::default();
        raw.observe_tool_call_with_cwd(
            "call",
            "Bash",
            &json!({"command":command}),
            None,
            Some(cwd),
            "now",
        );
        let mut facts = ChatFacts::default();
        raw.resolve_into(&mut facts, &mut GitResolver::default());
        facts.repo_work
    }

    #[test]
    fn repo_write_finding_1_each_invocation_has_its_own_target() {
        let temp = tempfile::tempdir().unwrap();
        let read = work_repo(temp.path(), "read");
        let a = work_repo(temp.path(), "a");
        let b = work_repo(temp.path(), "b");
        let command = format!(
            "git -C '{}' status && git -C '{}' push && git -C '{}' commit -m update",
            read.display(),
            a.display(),
            b.display()
        );
        assert_eq!(command_repos(&command, &read), vec![a, b]);
    }

    #[test]
    fn repo_write_finding_2_saved_message_cwd_matches_live() {
        let temp = tempfile::tempdir().unwrap();
        let repo = work_repo(temp.path(), "repo");
        for kind in ["user", "assistant"] {
            let transcript = temp.path().join("saved.jsonl");
            std::fs::write(&transcript, [
                json!({"type":kind,"cwd":repo,"message":{"content":"context"}}),
                json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"call","name":"Bash","input":{"command":"git push"}}]}}),
            ].iter().map(Value::to_string).collect::<Vec<_>>().join("\n")).unwrap();
            let mut rebuilt = ChatFacts::default();
            scan_claude_transcript(&transcript, None)
                .unwrap()
                .resolve_into(&mut rebuilt, &mut GitResolver::default());
            assert_eq!(
                rebuilt.repo_work,
                command_repos("git push", &repo),
                "{kind}"
            );
        }
    }

    #[test]
    fn repo_write_finding_3_read_only_variants_never_join() {
        let temp = tempfile::tempdir().unwrap();
        let repo = work_repo(temp.path(), "repo");
        for command in [
            "git branch feature --list",
            "git branch --list",
            "git commit --dry-run",
            "git push --dry-run",
            "git push -n",
            "git push -nv",
            "git commit --porcelain",
            "git commit --short",
            "git commit -z",
            "git commit --help",
            "gh pr create --help",
            "gh pr view",
            "gh pr view --json pr,create",
        ] {
            assert!(command_repos(command, &repo).is_empty(), "{command}");
        }
    }

    #[test]
    fn repo_write_finding_4_unmatched_gh_selector_never_uses_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let repo = work_repo(temp.path(), "unrelated");
        for command in [
            "gh pr create --repo acme/api",
            "gh --repo acme/api pr create",
            "gh pr create -Racme/api",
            "gh pr create --repo=acme/api",
        ] {
            assert!(command_repos(command, &repo).is_empty(), "{command}");
        }
    }

    #[test]
    fn gh_selectors_match_only_unique_listed_local_remotes_in_live_and_scan() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = work_repo(temp.path(), "cwd");
        let api = work_repo(temp.path(), "api");
        git(
            &api,
            &["remote", "add", "upstream", "git@github.com:acme/api.git"],
        );
        let repos = [cwd.clone(), api.clone()];
        let dir = temp.path().join("facts");
        for (i, command) in [
            "gh pr create --repo acme/api",
            "gh -R acme/api pr create",
            "gh pr create -Racme/api",
            "gh pr create --repo=github.com/acme/api",
        ]
        .iter()
        .enumerate()
        {
            let mut live = LiveFacts::default();
            live.observe_with_cwd(
                &HarnessEvent::ItemStarted {
                    id: "call".into(),
                    kind: ItemKind::ToolCall {
                        name: "Bash".into(),
                        input: json!({"command":command}),
                    },
                },
                None,
                Some(&cwd),
            );
            let transcript = temp.path().join("saved.jsonl");
            std::fs::write(&transcript,json!({"type":"assistant","cwd":cwd,"message":{"content":[{"type":"tool_use","id":"call","name":"Bash","input":{"command":command}}]}}).to_string()).unwrap();
            for (id, pending, scan) in [
                (
                    format!("live-{i}"),
                    live.take_due(Instant::now(), true).unwrap(),
                    None,
                ),
                (
                    format!("scan-{i}"),
                    RawFacts::default(),
                    Some(transcript.as_path()),
                ),
            ] {
                flush_live(
                    &dir,
                    ChatBackend::Claude,
                    &id,
                    pending,
                    vec![],
                    scan,
                    FactPaths {
                        home: None,
                        repos: &repos,
                    },
                )
                .unwrap();
                assert_eq!(
                    load_facts(&facts_path(&dir, ChatBackend::Claude, &id).unwrap())
                        .unwrap()
                        .unwrap()
                        .repo_work,
                    vec![api.clone()],
                    "{id}"
                );
            }
        }
        let duplicate = work_repo(temp.path(), "duplicate");
        git(
            &duplicate,
            &["remote", "add", "origin", "https://github.com/acme/api.git"],
        );
        let mut resolver = GitResolver::with_repos(&[api.clone(), duplicate]);
        assert_eq!(resolver.resolve_selector("acme/api"), None);
        let mut resolver = GitResolver::with_repos(&[api.clone()]);
        assert_eq!(resolver.resolve_selector("other.example/acme/api"), None);
        assert_eq!(resolver.resolve_selector("acme/unknown"), None);
        git(
            &api,
            &[
                "remote",
                "set-url",
                "upstream",
                "https://other.example/acme/api.git",
            ],
        );
        assert_eq!(
            resolver.resolve_selector("other.example/acme/api"),
            Some(api)
        );
    }

    #[test]
    fn ambiguous_shell_directories_never_fall_back_to_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let repo = work_repo(temp.path(), "repo");
        for command in [
            "git -C $TARGET push",
            "git -C missing push",
            "cd $TARGET && git push",
            "cd missing; git push",
            "cd ../elsewhere || git push",
            "cd missing && (false) || git push",
            "cd ../elsewhere | cat; git push",
            "GIT_DIR=/elsewhere/.git git push",
            "GIT_DIR=/elsewhere/.git; git push",
            "if true; then cd ../elsewhere; fi; git push",
            "{ cd ../elsewhere; }; git push",
            "eval 'cd ../elsewhere'; git push",
            "git --unknown value push",
            "git checkout --pathspec-from-file -b feature",
            "git switch --conflict -c feature",
            "git switch -c feature --no-create",
            "git push $FLAGS",
            "git branch $FLAGS feature",
            "gh pr create --title $TITLE",
            "export GIT_DIR=/elsewhere/.git; git push",
            "GIT_DIR=/elsewhere/.git; git -C . push",
        ] {
            assert!(command_repos(command, &repo).is_empty(), "{command}");
        }
        assert_eq!(
            command_repos("(cd /elsewhere); git push", &repo),
            vec![repo.clone()]
        );
        assert!(command_repos("cat <<'EOF'\ngit push\nEOF", &repo).is_empty());
        assert!(
            command_repos("git commit -m \"$(cat <<'EOF'\nmessage\nEOF\n)\"", &repo).is_empty()
        );
    }

    #[test]
    fn repo_write_finding_5_relative_overrides_use_bash_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = work_repo(temp.path(), "cwd");
        let api = work_repo(temp.path(), "api");
        for command in [
            "git -C ../api push",
            "cd ../api && git push",
            "git -C .. -C api push",
            "git --git-dir .git -C ../api push",
        ] {
            assert_eq!(command_repos(command, &cwd), vec![api.clone()], "{command}");
        }
        let mut raw = RawFacts::default();
        raw.observe_tool_call_with_cwd(
            "call",
            "Bash",
            &json!({"command":"git push","cwd":"../api"}),
            None,
            Some(&cwd),
            "now",
        );
        let mut facts = ChatFacts::default();
        raw.resolve_into(&mut facts, &mut GitResolver::default());
        assert_eq!(facts.repo_work, vec![api]);
    }

    #[test]
    fn repo_write_conditionally_skipped_cd_never_joins() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = work_repo(temp.path(), "cwd");
        let a = work_repo(temp.path(), "a");
        let b = work_repo(temp.path(), "b");
        let ambiguous = [
            format!(
                "cd '{}' && true || cd '{}' && git push",
                a.display(),
                b.display()
            ),
            format!("cd '{}' || true; git commit -m update", a.display()),
            format!("[ -d '{}' ] && cd '{}'; git push", a.display(), a.display()),
            format!(
                "[ -d '{}' ] && cd '{}' && git push",
                a.display(),
                a.display()
            ),
            format!("false || cd '{}' && git push", a.display()),
        ];
        let joined: Vec<_> = ambiguous
            .iter()
            .map(|command| (command, command_repos(command, &cwd)))
            .filter(|(_, repos)| !repos.is_empty())
            .collect();
        assert!(joined.is_empty(), "ambiguous commands joined: {joined:?}");
        for (command, repo) in [
            ("cd ../a && git push", a),
            ("cd ../a && cd ../b && git push", b.clone()),
            ("true; cd ../b && git push", b),
        ] {
            assert_eq!(command_repos(command, &cwd), vec![repo], "{command}");
        }
    }

    #[test]
    fn repo_write_validates_every_directory_transition() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = work_repo(temp.path(), "cwd");
        let repo = work_repo(temp.path(), "repo");
        let file = temp.path().join("file");
        std::fs::write(&file, "not a directory").unwrap();
        let mut commands = Vec::new();
        for invalid in [
            temp.path().join("missing"),
            temp.path().join("missing/../cwd"),
            file.join("../cwd"),
            file,
        ] {
            for options in [
                format!("-C '{}' -C '{}'", invalid.display(), repo.display()),
                format!("-C'{}' -C'{}'", invalid.display(), repo.display()),
            ] {
                commands.push(format!("git {options} push"));
            }
            commands.push(format!(
                "cd '{}' && cd '{}' && git push",
                invalid.display(),
                repo.display()
            ));
        }
        commands.push(format!("git -C ../missing -C '{}' push", repo.display()));
        commands.push(format!("git -C '~other' -C '{}' push", repo.display()));
        let joined: Vec<_> = commands
            .iter()
            .map(|command| (command, command_repos(command, &cwd)))
            .filter(|(_, repos)| !repos.is_empty())
            .collect();
        assert!(joined.is_empty(), "invalid transitions joined: {joined:?}");
        assert_eq!(
            command_repos("git -C '' -C .. -C repo push", &cwd),
            vec![repo]
        );
    }

    #[test]
    fn repo_write_finding_7_git_global_option_values_are_consumed() {
        let temp = tempfile::tempdir().unwrap();
        let repo = work_repo(temp.path(), "repo");
        for options in [
            "-c user.name=bot",
            "--config-env user.name=BOT_NAME",
            "--namespace test",
            "--git-dir .git",
            "--work-tree .",
            "-cuser.name=bot",
        ] {
            assert_eq!(
                command_repos(&format!("git {options} commit -m update"), &repo),
                vec![repo.clone()],
                "{options}"
            );
        }
    }

    #[test]
    fn repo_write_finding_8_branch_creation_with_options() {
        let temp = tempfile::tempdir().unwrap();
        let repo = work_repo(temp.path(), "repo");
        for command in [
            "git switch -C feature",
            "git switch -c feature",
            "git switch --create=feature",
            "git checkout -b feature",
            "git checkout -B feature",
            "git branch -f feature",
            "git branch --track feature origin/main",
            "git branch feature --force",
        ] {
            assert_eq!(
                command_repos(command, &repo),
                vec![repo.clone()],
                "{command}"
            );
        }
    }

    #[test]
    fn older_chat_facts_without_repo_work_still_load() {
        let facts: ChatFacts = serde_json::from_str(
            r#"{"issue_keys":[],"repos_touched":[],"checkouts":[],"children":[]}"#,
        )
        .unwrap();
        assert!(facts.repo_work.is_empty());
    }

    #[test]
    fn resolution_uses_the_innermost_worktree_and_drops_removed_nested_ones() {
        let list = worktrees();
        assert_eq!(
            resolve_against(Path::new("/repos/app/src/main.rs"), &list),
            Some(resolved("/repos/app", "/repos/app", Some("main")))
        );
        assert_eq!(
            resolve_against(
                Path::new("/repos/app/.claude/worktrees/fix-login/a.rs"),
                &list
            ),
            Some(resolved(
                "/repos/app",
                "/repos/app/.claude/worktrees/fix-login",
                Some("claude/fix-login")
            ))
        );
        assert_eq!(
            resolve_against(Path::new("/wt/app-tru-12/b.rs"), &list),
            Some(resolved(
                "/repos/app",
                "/wt/app-tru-12",
                Some("tracey/tru-12-thing")
            ))
        );
        // A nested worktree that is no longer listed, git internals, and
        // paths outside the repository resolve to nothing.
        assert_eq!(
            resolve_against(Path::new("/repos/app/.claude/worktrees/gone/c.rs"), &list),
            None
        );
        assert_eq!(
            resolve_against(Path::new("/repos/app/.git/HEAD"), &list),
            None
        );
        assert_eq!(resolve_against(Path::new("/elsewhere/x"), &list), None);
    }

    #[test]
    fn touches_count_edits_per_repo_and_checkout_and_collect_keys() {
        let mut facts = ChatFacts::default();
        let main = resolved("/repos/app", "/repos/app", Some("main"));
        let wt = resolved("/repos/app", "/wt/app-tru-12", Some("tracey/tru-12-thing"));
        facts.record_touch(&main, TouchKind::Edit, "2026-10-28T10:00:00Z");
        facts.record_touch(&wt, TouchKind::Visit, "2026-10-28T11:00:00Z");
        facts.record_touch(&wt, TouchKind::Edit, "2026-10-28T10:30:00Z");
        facts.record_touch(&wt, TouchKind::Edit, "2026-10-28T12:00:00Z");
        assert_eq!(
            facts.repos_touched,
            vec![RepoTouch {
                dir: PathBuf::from("/repos/app"),
                edits: 3
            }]
        );
        assert_eq!(facts.checkouts.len(), 2);
        assert_eq!(facts.checkouts[1].edits, 2);
        assert_eq!(facts.checkouts[1].last, "2026-10-28T12:00:00Z");
        assert_eq!(facts.issue_keys, vec!["TRU-12"]);
        // A command-only repo is touched with zero edits, and its current
        // branch says nothing about this chat.
        facts.record_touch(
            &resolved("/repos/lib", "/repos/lib", Some("fix/ops-361-roster")),
            TouchKind::Visit,
            "2026-10-28T13:00:00Z",
        );
        assert_eq!(facts.issue_keys, vec!["TRU-12"]);
        assert_eq!(facts.repo_names(), vec!["app", "lib"]);
        assert_eq!(facts.repos_touched[1].edits, 0);
    }

    #[test]
    fn a_full_scan_merges_with_live_facts_without_double_counting() {
        let mut live = ChatFacts::default();
        let main = resolved("/repos/app", "/repos/app", Some("main"));
        live.record_touch(&main, TouchKind::Edit, "2026-10-29T09:00:00Z");
        live.add_issue_key("TRU-1");
        let mut scan = ChatFacts::default();
        for _ in 0..3 {
            scan.record_touch(&main, TouchKind::Edit, "2026-10-28T09:00:00Z");
        }
        scan.add_issue_key("TRU-2");
        scan.add_child(ChatChild {
            session_id: "toolu_1".into(),
            kind: ChildKind::Subagent,
            delegation_id: None,
            task_id: None,
            title: Some("Map the repo".into()),
        });
        live.absorb_scan(scan);
        assert_eq!(live.repos_touched[0].edits, 3);
        assert_eq!(live.checkouts[0].edits, 3);
        assert_eq!(live.checkouts[0].last, "2026-10-29T09:00:00Z");
        assert_eq!(live.issue_keys, vec!["TRU-1", "TRU-2"]);
        assert_eq!(live.fan_out_line().as_deref(), Some("1 subagent"));
    }

    #[test]
    fn facts_record_markdown_paths_written_or_edited_only() {
        let home = Some(Path::new("/Users/me"));
        let mut raw = RawFacts::default();
        let at = "2026-10-30T10:00:00Z";
        raw.observe_tool_call(
            "w1",
            "Write",
            &json!({"file_path": "/work/example/docs/status.md"}),
            home,
            at,
        );
        raw.observe_tool_call(
            "e1",
            "Edit",
            &json!({"file_path": "~/code/api/docs/DECISIONS.MD"}),
            home,
            at,
        );
        raw.observe_tool_call(
            "m1",
            "MultiEdit",
            &json!({"file_path": "/code/api/notes.markdown"}),
            home,
            at,
        );
        // Not markdown, or not a write: ignored.
        raw.observe_tool_call(
            "e2",
            "Edit",
            &json!({"file_path": "/code/api/src/main.rs"}),
            home,
            at,
        );
        raw.observe_tool_call(
            "r1",
            "Read",
            &json!({"file_path": "/code/api/README.md"}),
            home,
            at,
        );
        raw.observe_tool_call(
            "n1",
            "NotebookEdit",
            &json!({"notebook_path": "/code/api/x.md"}),
            home,
            at,
        );
        // Written again later: it becomes the newest.
        raw.observe_tool_call(
            "w2",
            "Write",
            &json!({"file_path": "/work/example/docs/status.md"}),
            home,
            at,
        );
        let mut facts = ChatFacts::default();
        raw.resolve_into(&mut facts, &mut GitResolver::default());
        assert_eq!(
            facts.documents,
            vec![
                PathBuf::from("/Users/me/code/api/docs/DECISIONS.MD"),
                PathBuf::from("/code/api/notes.markdown"),
                PathBuf::from("/work/example/docs/status.md"),
            ]
        );
        assert!(!facts.is_empty());

        // The backfill reads the same calls from transcript lines.
        let mut scanned = RawFacts::default();
        let line = json!({"type": "assistant", "timestamp": at,
            "message": {"content": [{"type": "tool_use", "id": "w3", "name": "Write",
                "input": {"file_path": "/work/example/docs/plan.md"}}]}})
        .to_string();
        observe_claude_line(&mut scanned, &line, home, true);
        assert_eq!(
            scanned.documents,
            vec![PathBuf::from("/work/example/docs/plan.md")]
        );
    }

    #[test]
    fn documents_keep_the_newest_within_the_cap_and_merge_with_a_scan() {
        let mut facts = ChatFacts::default();
        for n in 0..DOCUMENTS_CAP + 5 {
            facts.add_document(Path::new(&format!("/notes/{n}.md")));
        }
        assert_eq!(facts.documents.len(), DOCUMENTS_CAP);
        assert_eq!(facts.documents[0], PathBuf::from("/notes/5.md"));
        assert_eq!(
            facts.documents.last(),
            Some(&PathBuf::from(format!("/notes/{}.md", DOCUMENTS_CAP + 4)))
        );

        let mut live = ChatFacts::default();
        live.add_document(Path::new("/a.md"));
        live.add_document(Path::new("/b.md"));
        let mut scan = ChatFacts::default();
        scan.add_document(Path::new("/b.md"));
        scan.add_document(Path::new("/c.md"));
        live.absorb_scan(scan);
        assert_eq!(
            live.documents,
            vec![
                PathBuf::from("/a.md"),
                PathBuf::from("/b.md"),
                PathBuf::from("/c.md")
            ]
        );

        // Facts files from before TRU-191 load without documents and are
        // due one more backfill scan.
        let old: ChatFacts = serde_json::from_str(r#"{"backfilled": true}"#).unwrap();
        assert!(old.documents.is_empty());
        assert!(old.backfilled && !old.documents_backfilled);
        let written = serde_json::to_value(ChatFacts::default()).unwrap();
        assert!(written.get("documents").is_none());
    }

    #[test]
    fn transcript_lines_yield_touches_children_and_user_keys() {
        let home = Some(Path::new("/Users/me"));
        let mut raw = RawFacts::default();
        let user = json!({"type": "user", "message": {"role": "user",
            "content": "Start PRJ-102 in infra"}})
        .to_string();
        let wrapper = json!({"type": "user", "message": {"role": "user",
            "content": "<system-reminder>TRU-999</system-reminder>"}})
        .to_string();
        let meta = json!({"type": "user", "isMeta": true, "message": {"role": "user",
            "content": "TRU-998"}})
        .to_string();
        let result = json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "TRU-997"}]}})
        .to_string();
        let assistant = json!({"type": "assistant", "timestamp": "2026-10-28T12:34:56.789Z",
        "message": {"role": "assistant", "content": [
            {"type": "text", "text": "on it"},
            {"type": "tool_use", "id": "t1", "name": "Edit",
             "input": {"file_path": "/repos/infra/main.tf"}},
            {"type": "tool_use", "id": "t2", "name": "Read",
             "input": {"file_path": "/repos/other/x"}},
            {"type": "tool_use", "id": "t3", "name": "Agent",
             "input": {"description": "Fable: BlobFS on new cloud design", "prompt": "…"}},
            {"type": "tool_use", "id": "t4", "name": "Bash",
             "input": {"command": "cd ~/GitRepo/x && git status"}}
        ]}})
        .to_string();
        for line in [&user, &wrapper, &meta, &result, &assistant] {
            observe_claude_line(&mut raw, line, home, true);
        }
        assert_eq!(raw.issue_keys, vec!["PRJ-102"]);
        assert_eq!(
            raw.touches,
            vec![
                RawTouch {
                    kind: TouchKind::Edit,
                    path: PathBuf::from("/repos/infra/main.tf"),
                    at: "2026-10-28T12:34:56Z".into()
                },
                RawTouch {
                    kind: TouchKind::Visit,
                    path: PathBuf::from("/Users/me/GitRepo/x"),
                    at: "2026-10-28T12:34:56Z".into()
                },
            ]
        );
        assert_eq!(raw.children.len(), 1);
        assert_eq!(raw.children[0].session_id, "t3");
        assert_eq!(
            raw.children[0].title.as_deref(),
            Some("Fable: BlobFS on new cloud design")
        );
        // A subagent transcript's user line is a brief, not the human.
        let mut sub = RawFacts::default();
        observe_claude_line(&mut sub, &user, home, false);
        assert!(sub.is_empty());
    }

    #[test]
    fn live_frames_complete_streamed_inputs_and_debounce_writes() {
        let mut live = LiveFacts::default();
        let started = HarnessEvent::ItemStarted {
            id: "a1".into(),
            kind: ItemKind::ToolCall {
                name: "Agent".into(),
                input: json!({}),
            },
        };
        live.observe(&started, None);
        live.observe(
            &HarnessEvent::ItemInputDelta {
                id: "a1".into(),
                partial_json: r#"{"description":"Review PR 587"}"#.into(),
            },
            None,
        );
        assert!(!live.has_pending());
        live.observe(
            &HarnessEvent::ItemCompleted {
                id: "a1".into(),
                output: String::new(),
                is_error: false,
            },
            None,
        );
        // Subagent tool calls arrive whole inside the subagent wrapper.
        live.observe(
            &HarnessEvent::SubagentEvent {
                parent_tool_use_id: "a1".into(),
                event: Box::new(HarnessEvent::ItemStarted {
                    id: "s1".into(),
                    kind: ItemKind::ToolCall {
                        name: "Write".into(),
                        input: json!({"file_path": "/repos/app/new.rs"}),
                    },
                }),
            },
            None,
        );
        live.observe_user_text("look at OPS-847");
        let t0 = Instant::now();
        let first = live.take_due(t0, false).expect("first write is due");
        assert_eq!(first.children[0].title.as_deref(), Some("Review PR 587"));
        assert_eq!(first.touches[0].path, PathBuf::from("/repos/app/new.rs"));
        assert_eq!(first.issue_keys, vec!["OPS-847"]);
        // One write at a time, then the debounce, unless forced.
        live.observe_user_text("and OPS-848");
        assert_eq!(live.take_due(t0, true), None);
        live.flush_done();
        assert_eq!(live.take_due(t0 + Duration::from_secs(1), false), None);
        assert!(live.take_due(t0 + Duration::from_secs(1), true).is_some());
        live.flush_done();
        assert_eq!(live.take_due(t0 + LIVE_FLUSH_DEBOUNCE * 2, true), None);
    }

    #[test]
    fn pr_creates_use_shell_command_words_live_and_in_transcripts() {
        let output = "https://github.com/acme/api/pull/42";
        let cases = [
            (r#"grep -n "gh pr create" x.rs"#, false),
            (r#"rg 'gh pr create' x.rs"#, false),
            (r#"echo "gh pr create""#, false),
            ("echo gh pr create", false),
            ("printf '%s' gh pr create", false),
            ("# gh pr create\necho result", false),
            ("cat <<'EOF'\ngh pr create\nEOF", false),
            ("cat <<EOF\ngh pr create\nEOF", false),
            (r#"bash -c 'echo "gh pr create"'"#, false),
            ("git push && gh pr create --fill", true),
            ("gh pr create --title 'A title' --body 'Details'", true),
            ("cd /tmp; /usr/local/bin/gh pr create --fill", true),
            ("GH_REPO=acme/api gh pr create --fill", true),
            ("pr=$(gh pr create --fill); echo \"$pr\"", true),
            ("git push; gh \\\npr create --fill", true),
            ("cat <<'EOF'\ngh pr create\nEOF\ngh pr create --fill", true),
        ];
        let temp = tempfile::tempdir().unwrap();
        let transcript = temp.path().join("session.jsonl");
        for (command, expected) in cases {
            assert_eq!(is_gh_pr_create(command), expected, "{command}");
            let mut live = LiveFacts::default();
            live.observe(
                &HarnessEvent::ItemStarted {
                    id: "call".into(),
                    kind: ItemKind::ToolCall {
                        name: "Bash".into(),
                        input: json!({"command": command}),
                    },
                },
                None,
            );
            live.observe(
                &HarnessEvent::ItemCompleted {
                    id: "call".into(),
                    output: output.into(),
                    is_error: false,
                },
                None,
            );
            let pending = live.take_due(Instant::now(), true).unwrap_or_default();
            assert_eq!(pending.prs.len(), usize::from(expected), "live: {command}");
            let lines = [
                json!({"type": "assistant", "message": {"content": [
                    {"type": "tool_use", "id": "call", "name": "Bash", "input": {"command": command}}
                ]}}),
                json!({"type": "user", "message": {"content": [
                    {"type": "tool_result", "tool_use_id": "call", "content": output}
                ]}}),
            ];
            std::fs::write(
                &transcript,
                lines
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .unwrap();
            let scan = scan_claude_transcript(&transcript, None).unwrap();
            assert_eq!(
                scan.prs.len(),
                usize::from(expected),
                "transcript: {command}"
            );
        }
    }

    #[test]
    fn live_frames_record_prs_from_gh_pr_create_results_only() {
        let mut live = LiveFacts::default();
        let bash = |id: &str, command: &str| HarnessEvent::ItemStarted {
            id: id.into(),
            kind: ItemKind::ToolCall {
                name: "Bash".into(),
                input: json!({ "command": command }),
            },
        };
        let done = |id: &str, output: &str| HarnessEvent::ItemCompleted {
            id: id.into(),
            output: output.into(),
            is_error: false,
        };
        // A PR URL in another command's output is not a PR this chat made.
        live.observe(&bash("b1", "gh pr view 12 --json url"), None);
        live.observe(&done("b1", "https://github.com/acme/api/pull/12"), None);
        live.observe(
            &bash("b2", "git push -u origin HEAD && gh pr create --fill"),
            None,
        );
        live.observe(
            &done(
                "b2",
                "Creating pull request for prj-101 into main in acme/api\n\nhttps://github.com/acme/api/pull/13\n",
            ),
            None,
        );
        // A subagent's PR counts for this chat; its input arrives streamed.
        live.observe(
            &HarnessEvent::SubagentEvent {
                parent_tool_use_id: "a1".into(),
                event: Box::new(HarnessEvent::ItemStarted {
                    id: "s1".into(),
                    kind: ItemKind::ToolCall {
                        name: "Bash".into(),
                        input: json!({}),
                    },
                }),
            },
            None,
        );
        live.observe(
            &HarnessEvent::SubagentEvent {
                parent_tool_use_id: "a1".into(),
                event: Box::new(HarnessEvent::ItemInputDelta {
                    id: "s1".into(),
                    partial_json: r#"{"command":"gh pr create --draft --repo acme/web"}"#.into(),
                }),
            },
            None,
        );
        live.observe(
            &HarnessEvent::SubagentEvent {
                parent_tool_use_id: "a1".into(),
                event: Box::new(done("s1", "https://github.com/acme/web/pull/7")),
            },
            None,
        );
        let raw = live.take_due(Instant::now(), true).expect("PRs are due");
        assert_eq!(
            raw.prs
                .iter()
                .map(|pr| pr.key().label())
                .collect::<Vec<_>>(),
            vec!["acme/api#13", "acme/web#7"]
        );
        let mut facts = ChatFacts::default();
        raw.resolve_into(&mut facts, &mut GitResolver::default());
        facts.add_pr(PrRef {
            repo: "ACME/api".into(),
            number: 13,
            url: "https://github.com/ACME/api/pull/13".into(),
        });
        assert_eq!(facts.prs.len(), 2);
        assert!(!facts.is_empty());
    }

    #[test]
    fn live_frames_record_linear_issues_from_create_results_only() {
        let mut live = LiveFacts::default();
        let bash = |id: &str, command: &str| HarnessEvent::ItemStarted {
            id: id.into(),
            kind: ItemKind::ToolCall {
                name: "Bash".into(),
                input: json!({ "command": command }),
            },
        };
        let done = |id: &str, output: &str| HarnessEvent::ItemCompleted {
            id: id.into(),
            output: output.into(),
            is_error: false,
        };
        live.observe(&bash("view", "linear issue view DEMO-20"), None);
        live.observe(
            &done(
                "view",
                "https://linear.app/fake-space/issue/DEMO-20/example",
            ),
            None,
        );
        live.observe(
            &bash(
                "update",
                "linear --workspace fake-space issue update DEMO-21",
            ),
            None,
        );
        live.observe(
            &done(
                "update",
                "https://linear.app/fake-space/issue/DEMO-21/example",
            ),
            None,
        );
        live.observe(
            &bash(
                "create",
                "linear --workspace fake-space issue create --title example",
            ),
            None,
        );
        live.observe(
            &done(
                "create",
                "Created https://linear.app/fake-space/issue/DEMO-22/example",
            ),
            None,
        );
        live.observe(&bash("failed", "linear issue create --title failed"), None);
        live.observe(&done("failed", "creation failed"), None);
        live.observe(
            &HarnessEvent::TextDelta(
                "https://linear.app/fake-space/issue/DEMO-23/assistant".into(),
            ),
            None,
        );
        let raw = live.take_due(Instant::now(), true).expect("issue is due");
        assert_eq!(raw.issue_keys, vec!["DEMO-22"]);
        let mut facts = ChatFacts::default();
        raw.resolve_into(&mut facts, &mut GitResolver::default());
        assert_eq!(facts.issue_keys, vec!["DEMO-22"]);
    }

    #[test]
    fn transcript_scan_records_linear_issue_create_result_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let transcript = temp.path().join("session.jsonl");
        let lines = [
            json!({"type":"assistant", "message":{"content":[
                {"type":"text", "text":"https://linear.app/fake-space/issue/DEMO-30/assistant"},
                {"type":"tool_use", "id":"view", "name":"Bash",
                    "input":{"command":"linear issue view DEMO-31"}},
                {"type":"tool_use", "id":"update", "name":"Bash",
                    "input":{"command":"linear --workspace fake-space issue update DEMO-32"}},
                {"type":"tool_use", "id":"create", "name":"Bash",
                    "input":{"command":"linear --workspace fake-space issue create --title example"}},
                {"type":"tool_use", "id":"failed", "name":"Bash",
                    "input":{"command":"linear issue create --title failed"}}
            ]}}),
            json!({"type":"user", "message":{"content":[
                {"type":"tool_result", "tool_use_id":"view",
                    "content":"https://linear.app/fake-space/issue/DEMO-31/view"},
                {"type":"tool_result", "tool_use_id":"update",
                    "content":"https://linear.app/fake-space/issue/DEMO-32/update"},
                {"type":"tool_result", "tool_use_id":"create",
                    "content":[{"type":"text", "text":"https://linear.app/fake-space/issue/DEMO-33/example"}]},
                {"type":"tool_result", "tool_use_id":"failed", "content":"no URL"}
            ]}}),
        ];
        std::fs::write(
            &transcript,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let raw = scan_claude_transcript(&transcript, None).unwrap();
        assert_eq!(raw.issue_keys, vec!["DEMO-33"]);
    }

    #[test]
    fn linear_issue_urls_require_a_complete_issue_path() {
        assert_eq!(
            linear_issue_keys_in(
                "https://linear.app/fake-space/issue/DEMO-42/path, https://linear.app/fake-space/issue/DEMO-42abc, https://linear.app/fake-space/issue/demo-43, https://linear.app/fake-space/extra/issue/DEMO-45, https://example.com/fake-space/issue/DEMO-44"
            ),
            vec!["DEMO-42"]
        );
        let url = "https://linear.app/fake-space/issue/DEMO-42/path";
        assert_eq!(
            created_linear_issue_keys("linear issue create --title x", url),
            vec!["DEMO-42"]
        );
        assert_eq!(
            created_linear_issue_keys("linear --workspace fake-space issue create --title x", url),
            vec!["DEMO-42"]
        );
        assert!(created_linear_issue_keys("linear issue view DEMO-42", url).is_empty());
        assert!(created_linear_issue_keys("linear issue update DEMO-42", url).is_empty());
    }

    fn scanned_issue_keys(command: &str, output: &str) -> Vec<String> {
        let temp = tempfile::tempdir().expect("tempdir");
        let transcript = temp.path().join("session.jsonl");
        let lines = [
            json!({"type":"assistant", "message":{"content":[
                {"type":"tool_use", "id":"call", "name":"Bash",
                    "input":{"command":command}}
            ]}}),
            json!({"type":"user", "message":{"content":[
                {"type":"tool_result", "tool_use_id":"call", "content":output}
            ]}}),
        ];
        std::fs::write(
            &transcript,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        scan_claude_transcript(&transcript, None)
            .unwrap()
            .issue_keys
    }

    fn live_issue_keys(command: &str, output: &str) -> Vec<String> {
        let mut live = LiveFacts::default();
        live.observe(
            &HarnessEvent::ItemStarted {
                id: "call".into(),
                kind: ItemKind::ToolCall {
                    name: "Bash".into(),
                    input: json!({"command":command}),
                },
            },
            None,
        );
        live.observe(
            &HarnessEvent::ItemCompleted {
                id: "call".into(),
                output: output.into(),
                is_error: false,
            },
            None,
        );
        live.take_due(Instant::now(), true)
            .unwrap_or_default()
            .issue_keys
    }

    #[test]
    fn linear_matcher_rejects_excessive_shell_nesting() {
        let command = format!("{}linear issue create{}", "(".repeat(256), ")".repeat(256));
        assert!(created_linear_issue_keys(
            &command,
            "https://linear.app/fake-space/issue/DEMO-80/new"
        )
        .is_empty());
    }

    #[test]
    fn linear_review_reproductions_live() {
        linear_review_reproductions(live_issue_keys);
    }

    #[test]
    fn linear_review_reproductions_transcript() {
        linear_review_reproductions(scanned_issue_keys);
    }

    fn linear_review_reproductions(collect: fn(&str, &str) -> Vec<String>) {
        let url = "https://linear.app/fake-space/issue/DEMO-80/new";
        // Both review rounds, including the additional probes in their full records.
        let cases = [
            ("update argument", "linear issue update DEMO-21 --description 'issue create'", url, false),
            ("view comment", "linear issue view DEMO-21 # issue create", url, false),
            ("view output", "linear issue create --title new >/dev/null && linear issue view DEMO-20 --json", url, false),
            ("both create kinds", "linear issue create --title new && gh pr create --title new --body new", "https://linear.app/fake-space/issue/DEMO-80/new\nhttps://github.com/fake/repo/pull/42", true),
            ("continuation", "linear \\\n issue create --title new", url, true),
            ("heredoc data", "cat <<'EOF'\nlinear issue create --title new\nhttps://linear.app/fake-space/issue/DEMO-80/new\nEOF", url, false),
            ("normal heredoc", "cd /tmp && linear issue create --title new --description \"$(cat <<'EOF'\nsome text\nEOF\n)\"", url, true),
            ("duplicate URL", "linear issue create --title new", "https://linear.app/fake-space/issue/DEMO-80/new\nhttps://linear.app/fake-space/issue/DEMO-80/new", true),
            ("quoted parentheses", "printf '%s' '(linear issue create --title new)'", url, false),
            ("quoted parentheses argument", "linear issue create --title '(new)'", url, true),
            ("quoted substitution", "url=\"$(linear issue create --title new)\"; printf '%s' \"$url\"", url, true),
            ("quoted executable", "'linear' issue create --title new", url, true),
            ("assignment", "NAME=\"value\" linear issue create --title new", url, true),
            ("quoted assignment is executable", "'NAME=value' linear issue create --title new", url, false),
            ("empty command argument", "linear '' issue create", url, false),
            ("workspace value named issue", "linear --workspace issue issue create --title new", url, true),
            ("workspace equals", "linear --workspace=fake-space issue create --title new", url, true),
            ("stderr redirection", "linear issue create --title new 2>/tmp/fake-errors", url, true),
            ("stdout group redirection", "(linear issue create --title new) >/dev/null; cat old-issue.json", url, false),
            ("multiple results", "linear issue create --title new", "https://linear.app/fake-space/issue/DEMO-80/new https://linear.app/fake-space/issue/DEMO-81/other", false),
            ("heredoc at EOF", "linear issue create --title new <<EOF\nbody\nEOF", url, true),
            ("tabbed mixed delimiter", "cat <<-E'ND'-DESC\n\tbody\n\tEND-DESC\nlinear issue create --title new", url, true),
            ("subshell", "(linear issue create --title new)", url, true),
            ("substitution", "url=$(linear issue create --title new); printf '%s' \"$url\"", url, true),
            ("comment heredoc marker", "# description uses <<EOF\nlinear issue create --title new", url, true),
            ("here string", "cat <<<foo\nlinear issue create --title new", url, true),
            ("quoted heredoc backslash", "cat <<'EOF'\nexample \\\nEOF\nlinear issue create --title new", url, true),
            ("hyphen delimiter", "cat <<END-DESC\nbody\nEND-DESC\nlinear issue create --title new", url, true),
            ("printf output", "linear issue create --title new >/dev/null && printf '%s\\n' 'https://linear.app/fake-space/issue/DEMO-80/new'", url, false),
            ("cat output", "linear issue create --title new >/dev/null && cat old-issue.json", url, false),
            ("quoted marker across lines", "printf '%s\\n' 'first line\n<<EOF\ntext'\nlinear issue create --title new", url, true),
            ("continued executable", "lin\\\near issue create --title new", url, true),
            ("continued subcommand", "linear iss\\\nue create --title new", url, true),
            ("heredoc nested quotes", "linear issue create --description \"$(cat <<'EOF'\nexample \"\nlinear issue create --title decoy\nEOF\n)\"", url, true),
        ];
        let mut failures = Vec::new();
        for (name, command, output, created) in cases {
            let expected = if created { vec!["DEMO-80"] } else { vec![] };
            let actual = collect(command, output);
            if actual != expected {
                failures.push(format!("{name}: expected {expected:?}, got {actual:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn issue_create_matching_checks_subcommand_not_argument_text() {
        let decoy = "linear issue update DEMO-70 --description 'issue create'";
        let output = "https://linear.app/fake-space/issue/DEMO-70/example";
        assert_eq!(live_issue_keys(decoy, output), Vec::<String>::new());
        assert_eq!(scanned_issue_keys(decoy, output), Vec::<String>::new());

        let create = "linear --workspace fake-space issue create --title example";
        let created = "https://linear.app/fake-space/issue/DEMO-71/example";
        assert_eq!(live_issue_keys(create, created), vec!["DEMO-71"]);
        assert_eq!(scanned_issue_keys(create, created), vec!["DEMO-71"]);
    }

    #[test]
    fn issue_create_matching_skips_heredocs_and_joins_continuations() {
        let command = "cat <<'EOF'\nlinear issue create --title fake\nEOF\nlinear \\\nissue create --title real";
        let output = "https://linear.app/fake-space/issue/DEMO-72/example";
        assert_eq!(live_issue_keys(command, output), vec!["DEMO-72"]);
        assert_eq!(scanned_issue_keys(command, output), vec!["DEMO-72"]);

        let heredoc_only = "cat <<EOF\nlinear issue create --title fake\nEOF";
        assert_eq!(live_issue_keys(heredoc_only, output), Vec::<String>::new());
        assert_eq!(
            scanned_issue_keys(heredoc_only, output),
            Vec::<String>::new()
        );
    }

    #[test]
    fn ambiguous_linear_create_output_is_not_attributed() {
        let command =
            "linear issue create --title new >/dev/null && linear issue view DEMO-73 --json";
        let output = "https://linear.app/fake-space/issue/DEMO-73/existing";
        assert_eq!(live_issue_keys(command, output), Vec::<String>::new());
        assert_eq!(scanned_issue_keys(command, output), Vec::<String>::new());
    }

    #[test]
    fn issue_and_pr_create_results_are_both_recorded_live_and_in_transcripts() {
        let command =
            "linear --workspace fake-space issue create --title new && gh pr create --title new";
        let output = "https://linear.app/fake-space/issue/DEMO-74/new https://github.com/example/project/pull/75";
        assert_eq!(live_issue_keys(command, output), vec!["DEMO-74"]);
        assert_eq!(scanned_issue_keys(command, output), vec!["DEMO-74"]);

        let mut live = LiveFacts::default();
        live.observe(
            &HarnessEvent::ItemStarted {
                id: "call".into(),
                kind: ItemKind::ToolCall {
                    name: "Bash".into(),
                    input: json!({"command":command}),
                },
            },
            None,
        );
        live.observe(
            &HarnessEvent::ItemCompleted {
                id: "call".into(),
                output: output.into(),
                is_error: false,
            },
            None,
        );
        let raw = live.take_due(Instant::now(), true).unwrap();
        assert_eq!(raw.issue_keys, vec!["DEMO-74"]);
        assert_eq!(raw.prs.len(), 1);

        let temp = tempfile::tempdir().expect("tempdir");
        let transcript = temp.path().join("session.jsonl");
        let lines = [
            json!({"type":"assistant", "message":{"content":[
                {"type":"tool_use", "id":"call", "name":"Bash", "input":{"command":command}}
            ]}}),
            json!({"type":"user", "message":{"content":[
                {"type":"tool_result", "tool_use_id":"call", "content":output}
            ]}}),
        ];
        std::fs::write(
            &transcript,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let raw = scan_claude_transcript(&transcript, None).unwrap();
        assert_eq!(raw.issue_keys, vec!["DEMO-74"]);
        assert_eq!(raw.prs.len(), 1);
    }

    #[test]
    fn facts_files_without_prs_load_and_write_unchanged() {
        let old = r#"{"issue_keys":["PRJ-101"],"repos_touched":[],"checkouts":[],"children":[]}"#;
        let facts: ChatFacts = serde_json::from_str(old).unwrap();
        assert!(facts.prs.is_empty());
        let written = serde_json::to_string(&facts).unwrap();
        assert_eq!(written, old);
        let mut with_pr = facts;
        with_pr.add_pr(PrRef {
            repo: "acme/api".into(),
            number: 12,
            url: "https://github.com/acme/api/pull/12".into(),
        });
        let round: ChatFacts =
            serde_json::from_str(&serde_json::to_string(&with_pr).unwrap()).unwrap();
        assert_eq!(round, with_pr);
    }

    fn entry(id: &str, title: &str) -> ChatIndexEntry {
        ChatIndexEntry {
            id: id.into(),
            backend: ChatBackend::Claude,
            path: PathBuf::from(format!("/t/{id}.jsonl")),
            cwd: PathBuf::from("/repos/app"),
            repo_root: None,
            is_worktree: false,
            branch: None,
            title: title.into(),
            latest: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 1,
            dead_cwd: false,
            facts: None,
            root_id: None,
        }
    }

    #[test]
    fn workspace_chats_get_missed_prs_from_their_transcripts() {
        // TRU-238: a chat the workspace owns, not open in a tab, whose facts
        // lack a PR its transcript created, gets it; a hosted chat and a chat
        // no workspace owns are left alone.
        let tmp = tempfile::tempdir().unwrap();
        let facts_dir = tmp.path().join("facts");
        std::fs::create_dir_all(&facts_dir).unwrap();
        let pr_lines = |number: u32| {
            let lines = [
                json!({"type": "assistant", "message": {"content": [
                    {"type": "tool_use", "id": "b1", "name": "Bash",
                        "input": {"command": "CO=$(gh pr create --fill); echo $CO"}}]}}),
                json!({"type": "user", "message": {"content": [
                    {"type": "tool_result", "tool_use_id": "b1",
                        "content": format!("https://github.com/acme/api/pull/{number}\n")}]}}),
            ];
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let transcript = |id: &str| {
            let path = tmp.path().join(format!("{id}.jsonl"));
            std::fs::write(&path, pr_lines(81)).unwrap();
            path
        };
        let set_time = |path: &Path, at: SystemTime| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(at)
                .unwrap();
        };
        let modified = |path: &Path| std::fs::metadata(path).unwrap().modified().unwrap();
        let long_ago = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let mut entries = Vec::new();
        for id in ["owned", "hosted", "stranger"] {
            let mut e = entry(id, id);
            e.path = transcript(id);
            e.mtime = SystemTime::now();
            let mut known = ChatFacts::default();
            known.add_issue_key("LIVE-1");
            if id == "owned" {
                known.add_pr(
                    merge_plan::pr_urls_in("https://github.com/fake/repo/pull/42").remove(0),
                );
            }
            let path = facts_dir.join(format!("claude-{id}.json"));
            write_facts(&path, &known).unwrap();
            set_time(&path, long_ago);
            entries.push(e);
        }
        // A chat with no facts file yet.
        let mut fresh = entry("fresh", "fresh");
        fresh.path = transcript("fresh");
        fresh.mtime = SystemTime::now();
        entries.push(fresh);
        let owned: HashSet<String> = ["owned", "hosted", "fresh"].map(String::from).into();
        let hosted: HashSet<String> = ["hosted"].map(String::from).into();
        let added = catch_up_workspace_prs(&entries, &owned, &hosted, &facts_dir, None);
        assert_eq!(added, 2);
        // Only PRs were recorded: the files keep looking older than their
        // transcripts, so a later catch-up or backfill still absorbs the
        // rest (review F2).
        let owned_path = facts_dir.join("claude-owned.json");
        assert_eq!(modified(&owned_path), long_ago);
        assert!(transcript_is_newer(&owned_path, modified(&entries[0].path)));
        let fresh_path = facts_dir.join("claude-fresh.json");
        assert_eq!(modified(&fresh_path), SystemTime::UNIX_EPOCH);
        assert!(transcript_is_newer(&fresh_path, modified(&entries[3].path)));
        let read = |id: &str| {
            load_facts(&facts_dir.join(format!("claude-{id}.json")))
                .unwrap()
                .unwrap()
        };
        let caught = read("owned");
        assert_eq!(caught.prs.len(), 1);
        assert_eq!(caught.prs[0].number, 81);
        assert_eq!(caught.issue_keys, vec!["LIVE-1"]);
        assert!(read("hosted").prs.is_empty());
        assert!(read("stranger").prs.is_empty());
        // The same transcript is not scanned again in this process.
        assert_eq!(
            catch_up_workspace_prs(&entries, &owned, &hosted, &facts_dir, None),
            0
        );
        // A subagent that creates a PR changes no parent timestamp; its
        // file still makes the chat due again (review F3).
        let subagents = tmp.path().join("owned/subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let sub = subagents.join("agent-x.jsonl");
        std::fs::write(&sub, pr_lines(82)).unwrap();
        set_time(&sub, SystemTime::now() + std::time::Duration::from_secs(60));
        assert_eq!(
            catch_up_workspace_prs(&entries, &owned, &hosted, &facts_dir, None),
            1
        );
        let numbers: Vec<u64> = read("owned").prs.iter().map(|pr| pr.number).collect();
        assert_eq!(numbers, vec![81, 82]);
        // Removal-only reconciliation must also write, even with no new PRs.
        std::fs::write(&entries[0].path, "").unwrap();
        std::fs::remove_file(&sub).unwrap();
        set_time(
            &entries[0].path,
            SystemTime::now() + Duration::from_secs(120),
        );
        assert_eq!(
            catch_up_workspace_prs(&entries, &owned, &hosted, &facts_dir, None),
            0
        );
        assert!(read("owned").prs.is_empty());
    }

    #[test]
    fn current_chat_catch_up_removes_a_search_result_pr_without_new_prs() {
        let temp = tempfile::tempdir().unwrap();
        let transcript = temp.path().join("session.jsonl");
        let output = "https://github.com/fake/repo/pull/42";
        let lines = [
            json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "id": "search", "name": "Bash",
                    "input": {"command": "grep -n \"gh pr create\" x.rs"}}
            ]}}),
            json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "search", "content": output}
            ]}}),
        ];
        std::fs::write(
            &transcript,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let mut known = ChatFacts::default();
        known.add_issue_key("DEMO-42");
        known.prs = merge_plan::pr_urls_in(output);
        let path = facts_path(temp.path(), ChatBackend::Claude, "session").unwrap();
        write_facts(&path, &known).unwrap();
        set_mtime(&transcript, SystemTime::UNIX_EPOCH);
        let added = flush_live(
            temp.path(),
            ChatBackend::Claude,
            "session",
            RawFacts::default(),
            vec![],
            Some(&transcript),
            FactPaths::default(),
        )
        .unwrap();
        assert!(added.is_empty());
        let caught = load_facts(&path).unwrap().unwrap();
        assert!(caught.prs.is_empty());
        assert_eq!(caught.issue_keys, known.issue_keys);
    }

    #[test]
    fn failed_transcript_scan_preserves_recorded_prs() {
        let temp = tempfile::tempdir().unwrap();
        let transcript = temp.path().join("session.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let subagents = temp.path().join("session/subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        // A directory in place of a child transcript cannot be read as lines.
        std::fs::create_dir(subagents.join("agent-broken.jsonl")).unwrap();
        let mut known = ChatFacts::default();
        known.prs = merge_plan::pr_urls_in("https://github.com/acme/api/pull/42");
        let (caught, added) = catch_up_from_transcript(
            Some(known.clone()),
            &temp.path().join("facts.json"),
            &transcript,
            None,
            &mut GitResolver::default(),
        );
        assert_eq!(caught, known);
        assert!(added.is_empty());
    }

    fn child(id: &str, kind: ChildKind) -> ChatChild {
        ChatChild {
            session_id: id.into(),
            kind,
            delegation_id: None,
            task_id: None,
            title: None,
        }
    }

    #[test]
    fn roots_are_chats_nothing_names_as_a_child() {
        let mut entries = vec![
            entry("coord", "Cloud migration"),
            entry("worker", "Implement PRJ-101 cluster control plane"),
            entry("grandchild", "Review the worker's diff"),
            entry("solo", "Fix a typo"),
        ];
        let mut facts = ChatFacts::default();
        facts.add_child(child("toolu_sub", ChildKind::Subagent));
        entries[0].facts = Some(facts);
        let links = vec![
            ChildLink {
                parent: "coord".into(),
                child: child("worker", ChildKind::Worker),
            },
            ChildLink {
                parent: "worker".into(),
                child: child("grandchild", ChildKind::Review),
            },
            // A link to a parent outside the index changes nothing.
            ChildLink {
                parent: "elsewhere".into(),
                child: child("solo", ChildKind::Worker),
            },
        ];
        resolve_tree(&mut entries, &links);
        assert_eq!(entries[0].root_id, None);
        assert_eq!(entries[1].root_id.as_deref(), Some("coord"));
        assert_eq!(entries[2].root_id.as_deref(), Some("coord"));
        assert_eq!(entries[3].root_id, None);
        let coord = entries[0].facts.as_ref().unwrap();
        assert_eq!(
            coord.fan_out_line().as_deref(),
            Some("1 subagent · 1 worker")
        );
        // The worker child takes its title from its own index entry.
        assert_eq!(
            coord.children[1].title.as_deref(),
            Some("Implement PRJ-101 cluster control plane")
        );
        // Links created facts for a parent that had none.
        assert!(entries[1].facts.is_some());
    }

    #[test]
    fn cycles_never_loop() {
        let mut entries = vec![entry("a", "A"), entry("b", "B")];
        let links = vec![
            ChildLink {
                parent: "a".into(),
                child: child("b", ChildKind::Worker),
            },
            ChildLink {
                parent: "b".into(),
                child: child("a", ChildKind::Worker),
            },
        ];
        resolve_tree(&mut entries, &links);
        assert_eq!(entries[0].root_id.as_deref(), Some("b"));
        assert_eq!(entries[1].root_id.as_deref(), Some("a"));
    }

    #[test]
    fn facts_files_round_trip_and_reject_unsafe_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = facts_path(dir.path(), ChatBackend::Claude, "11111111-aaaa").unwrap();
        assert_eq!(path.file_name().unwrap(), "claude-11111111-aaaa.json");
        assert_eq!(load_facts(&path).unwrap(), None);
        let mut facts = ChatFacts::default();
        facts.add_issue_key("PRJ-102");
        write_facts(&path, &facts).unwrap();
        assert_eq!(load_facts(&path).unwrap(), Some(facts.clone()));
        // Unknown fields from later steps (workspace_id) are ignored.
        std::fs::write(
            &path,
            r#"{"workspace_id":"w1","issue_keys":["PRJ-1"],"backfilled":true}"#,
        )
        .unwrap();
        let loaded = load_facts(&path).unwrap().unwrap();
        assert!(loaded.backfilled);
        assert!(facts_path(dir.path(), ChatBackend::Claude, "../etc").is_err());
        std::fs::write(&path, "not json").unwrap();
        assert!(load_facts(&path).is_err());

        let mut entries = vec![entry("11111111-aaaa", "t"), entry("other", "u")];
        std::fs::write(&path, r#"{"issue_keys":["PRJ-102"]}"#).unwrap();
        attach_facts(&mut entries, dir.path());
        assert_eq!(
            entries[0].facts.as_ref().map(|f| f.issue_keys.clone()),
            Some(vec!["PRJ-102".to_string()])
        );
        assert_eq!(entries[1].facts, None);
    }

    /// `git` with the repo-discovery variables scrubbed, as in
    /// `worktree_follow`'s tests.
    fn git(dir: &Path, args: &[&str]) {
        let status = crate::agentd::git::git_command()
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    #[test]
    fn backfill_scans_once_with_subagents_and_real_git() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("infra");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let linked = temp.path().join("infra-prj-7");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "prj-7-thing",
                linked.to_str().unwrap(),
            ],
        );
        let projects = temp.path().join("projects");
        std::fs::create_dir_all(projects.join("s1/subagents")).unwrap();
        let edit = |path: &Path| {
            json!({"type": "assistant", "timestamp": "2026-10-28T10:00:00Z",
                "message": {"content": [{"type": "tool_use", "id": "e1", "name": "Write",
                    "input": {"file_path": path.to_str().unwrap()}}]}})
            .to_string()
        };
        let main_lines = [
            json!({"type": "user", "message": {"content": "migrate, see PRJ-6"}}).to_string(),
            edit(&repo.join("main.tf")),
        ];
        let transcript = projects.join("s1.jsonl");
        std::fs::write(&transcript, main_lines.join("\n")).unwrap();
        std::fs::write(
            projects.join("s1/subagents/agent-abc.jsonl"),
            edit(&linked.join("new.rs")),
        )
        .unwrap();
        std::fs::write(
            projects.join("s1/subagents/agent-abc.meta.json"),
            r#"{"toolUseId":"toolu_9","description":"Build the thing"}"#,
        )
        .unwrap();

        let facts_dir = temp.path().join("chat-meta");
        let targets = vec![BackfillTarget {
            id: "s1".into(),
            path: transcript.clone(),
            mtime: SystemTime::now(),
        }];
        let links = vec![ChildLink {
            parent: "s1".into(),
            child: child("codex-thread", ChildKind::Review),
        }];
        let report = backfill_claude(
            targets.clone(),
            &facts_dir,
            &HashSet::new(),
            &links,
            10,
            FactPaths::default(),
        );
        assert_eq!(report.scanned, 1);
        let facts = load_facts(&facts_dir.join("claude-s1.json"))
            .unwrap()
            .unwrap();
        assert!(facts.backfilled);
        let canonical_repo = std::fs::canonicalize(&repo).unwrap();
        let canonical_linked = std::fs::canonicalize(&linked).unwrap();
        assert_eq!(
            facts.repos_touched,
            vec![RepoTouch {
                dir: canonical_repo.clone(),
                edits: 2
            }]
        );
        assert_eq!(facts.checkouts.len(), 2);
        assert_eq!(facts.checkouts[1].dir, canonical_linked);
        assert_eq!(facts.checkouts[1].branch.as_deref(), Some("prj-7-thing"));
        assert_eq!(facts.issue_keys, vec!["PRJ-6", "PRJ-7"]);
        assert_eq!(
            facts.fan_out_line().as_deref(),
            Some("1 subagent · 1 review")
        );
        assert_eq!(facts.children[0].title.as_deref(), Some("Build the thing"));

        // Once per transcript; hosted chats and the batch bound are honoured.
        let again = backfill_claude(
            targets.clone(),
            &facts_dir,
            &HashSet::new(),
            &[],
            10,
            FactPaths::default(),
        );
        assert_eq!((again.scanned, again.already), (0, 1));
        // A transcript that moved on after the facts were written is
        // scanned again; an untouched one is not.
        let moved_on = vec![BackfillTarget {
            id: "s1".into(),
            path: transcript.clone(),
            mtime: SystemTime::now() + std::time::Duration::from_secs(3600),
        }];
        let rescan = backfill_claude(
            moved_on,
            &facts_dir,
            &HashSet::new(),
            &[],
            10,
            FactPaths::default(),
        );
        assert_eq!((rescan.scanned, rescan.already), (1, 0));
        let hosted: HashSet<String> = ["s1".to_string()].into();
        let skipped = backfill_claude(targets, &facts_dir, &hosted, &[], 10, FactPaths::default());
        assert_eq!((skipped.scanned, skipped.hosted), (0, 1));
        let bounded = backfill_claude(
            vec![BackfillTarget {
                id: "s2".into(),
                path: transcript,
                mtime: SystemTime::now(),
            }],
            &facts_dir,
            &HashSet::new(),
            &[],
            0,
            FactPaths::default(),
        );
        assert_eq!((bounded.scanned, bounded.remaining), (0, 1));

        // The live writer adds to the same file without losing the scan.
        let mut pending = RawFacts::default();
        pending.observe_user_text("now TRU-5");
        flush_live(
            &facts_dir,
            ChatBackend::Claude,
            "s1",
            pending,
            vec![],
            None,
            FactPaths::default(),
        )
        .unwrap();
        let facts = load_facts(&facts_dir.join("claude-s1.json"))
            .unwrap()
            .unwrap();
        assert_eq!(facts.issue_keys, vec!["PRJ-6", "PRJ-7", "TRU-5"]);
        assert!(facts.backfilled);
    }

    fn set_mtime(path: &Path, at: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(at))
            .expect("set mtime");
    }

    #[test]
    fn a_catch_up_waits_for_the_write_in_flight_and_takes_what_waits() {
        let mut live = LiveFacts::default();
        live.observe_user_text("see ACME-1");
        let t0 = Instant::now();
        let first = live.take_due(t0, true).expect("due");
        assert_eq!(first.issue_keys, vec!["ACME-1"]);
        live.request_catch_up();
        assert!(live.has_pending());
        // One write per tab at a time: the catch-up waits for this one.
        assert_eq!(live.take_catch_up(t0), None);
        live.observe_user_text("then ACME-2");
        live.flush_done();
        let caught = live.take_catch_up(t0).expect("catch-up after the write");
        assert_eq!(caught.issue_keys, vec!["ACME-2"]);
        // While it runs, nothing else writes; once done, nothing is left.
        live.observe_user_text("and ACME-3");
        assert_eq!(live.take_due(t0, true), None);
        live.flush_done();
        assert_eq!(live.take_catch_up(t0), None);
        assert!(live.take_due(t0, true).is_some());
    }

    #[test]
    fn a_tab_picking_up_a_stale_chat_scans_it_once_and_keeps_live_facts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let facts_dir = temp.path().join("chat-meta");
        let transcript = temp.path().join("s9.jsonl");
        let plan = temp.path().join("notes/plan.md");
        let lines = [
            json!({"type": "user", "message": {"content": "ship ACME-12"}}),
            json!({"type": "assistant", "timestamp": "2026-11-01T10:00:00Z",
                "message": {"content": [
                    {"type": "tool_use", "id": "b1", "name": "Bash",
                        "input": {"command": "gh pr create --fill"}},
                    {"type": "tool_use", "id": "b2", "name": "Bash",
                        "input": {"command": "echo https://github.com/acme/launch/pull/1"}},
                    {"type": "tool_use", "id": "a1", "name": "Agent",
                        "input": {"description": "Draft the brief", "prompt": "go"}},
                    {"type": "tool_use", "id": "w1", "name": "Write",
                        "input": {"file_path": plan.to_str().unwrap()}}]}}),
            json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "b2",
                    "content": "https://github.com/acme/launch/pull/1"},
                {"type": "tool_result", "tool_use_id": "b1",
                    "content": [{"type": "text",
                        "text": "https://github.com/acme/launch/pull/42\n"}]}]}}),
        ];
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(&transcript, body.join("\n")).unwrap();

        // What a tab hosting the chat earlier wrote, before the chat ran on
        // elsewhere; the transcript is newer.
        let path = facts_dir.join("claude-s9.json");
        let mut known = ChatFacts::default();
        known.add_issue_key("LIVE-1");
        known.add_pr(merge_plan::pr_urls_in("https://github.com/fake/repo/pull/42").remove(0));
        write_facts(&path, &known).unwrap();
        let now = SystemTime::now();
        set_mtime(&path, now - Duration::from_secs(7200));
        set_mtime(&transcript, now - Duration::from_secs(3600));

        let mut pending = RawFacts::default();
        pending.observe_user_text("then LIVE-2");
        pending
            .prs
            .extend(merge_plan::pr_urls_in("https://github.com/acme/web/pull/7"));
        let added = flush_live(
            &facts_dir,
            ChatBackend::Claude,
            "s9",
            pending,
            vec![],
            Some(&transcript),
            FactPaths::default(),
        )
        .unwrap();
        let ids = |prs: &[PrRef]| prs.iter().map(|pr| pr.key().id()).collect::<Vec<_>>();
        assert_eq!(
            ids(&added),
            ids(&merge_plan::pr_urls_in(
                "https://github.com/acme/launch/pull/42"
            ))
        );
        let facts = load_facts(&path).unwrap().unwrap();
        assert_eq!(facts.issue_keys, vec!["LIVE-1", "ACME-12", "LIVE-2"]);
        assert_eq!(ids(&facts.prs), vec!["acme/launch#42", "acme/web#7"]);
        assert_eq!(facts.documents, vec![plan.clone()]);
        assert_eq!(facts.children.len(), 1);
        assert_eq!(facts.children[0].title.as_deref(), Some("Draft the brief"));
        assert!(facts.backfilled && facts.documents_backfilled);

        // The file is newer: only PRs are recomputed, and live facts still land.
        let mut pending = RawFacts::default();
        pending.observe_user_text("LIVE-3");
        let added = flush_live(
            &facts_dir,
            ChatBackend::Claude,
            "s9",
            pending,
            vec![],
            Some(&transcript),
            FactPaths::default(),
        )
        .unwrap();
        assert!(added.is_empty());
        let after = load_facts(&path).unwrap().unwrap();
        assert_eq!(
            after.issue_keys,
            vec!["LIVE-1", "ACME-12", "LIVE-2", "LIVE-3"]
        );

        // A fresh chat (facts written after its transcript) whose live
        // watcher missed a `gh pr create` (TRU-238): the PR is added from
        // the transcript; nothing else is absorbed.
        let fresh = facts_dir.join("claude-s10.json");
        let mut live_only = ChatFacts::default();
        live_only.add_issue_key("LIVE-9");
        live_only.add_pr(merge_plan::pr_urls_in("https://github.com/fake/repo/pull/42").remove(0));
        write_facts(&fresh, &live_only).unwrap();
        set_mtime(&transcript, now - Duration::from_secs(9000));
        let added = flush_live(
            &facts_dir,
            ChatBackend::Claude,
            "s10",
            RawFacts::default(),
            vec![],
            Some(&transcript),
            FactPaths::default(),
        )
        .unwrap();
        assert_eq!(
            ids(&added),
            ids(&merge_plan::pr_urls_in(
                "https://github.com/acme/launch/pull/42"
            ))
        );
        let caught = load_facts(&fresh).unwrap().unwrap();
        assert_eq!(caught.issue_keys, vec!["LIVE-9"]);
        assert_eq!(ids(&caught.prs), ids(&added));
        assert!(!caught.backfilled);

        // Picked up again with nothing missing: unchanged.
        let added = flush_live(
            &facts_dir,
            ChatBackend::Claude,
            "s10",
            RawFacts::default(),
            vec![],
            Some(&transcript),
            FactPaths::default(),
        )
        .unwrap();
        assert!(added.is_empty());
        assert_eq!(load_facts(&fresh).unwrap().unwrap(), caught);
    }

    /// Real-data check (TRU-166 acceptance), read-only over a transcript you
    /// choose; facts go to a temp dir. Run with
    /// `GITTERM_REAL_TRANSCRIPT=/path/to/session.jsonl [GITTERM_REAL_QUERIES=a,b] \
    ///  cargo test --lib chat_facts::tests::real_transcript_backfill -- --ignored --nocapture`.
    /// Each comma-separated query must match the backfilled facts.
    #[test]
    #[ignore = "reads a transcript path from GITTERM_REAL_TRANSCRIPT"]
    fn real_transcript_backfill() {
        let Some(path) = std::env::var_os("GITTERM_REAL_TRANSCRIPT").map(PathBuf::from) else {
            eprintln!("GITTERM_REAL_TRANSCRIPT not set; skipping");
            return;
        };
        let home = dirs::home_dir().unwrap();
        let id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("transcript path has a file stem")
            .to_string();
        let started = Instant::now();
        let dir = tempfile::tempdir().unwrap();
        let report = backfill_claude(
            vec![BackfillTarget {
                id: id.clone(),
                path: path.clone(),
                mtime: SystemTime::now(),
            }],
            dir.path(),
            &HashSet::new(),
            &[],
            1,
            FactPaths {
                home: Some(&home),
                repos: &[],
            },
        );
        eprintln!("{report:?} in {:?}", started.elapsed());
        let facts = load_facts(&facts_path(dir.path(), ChatBackend::Claude, &id).unwrap())
            .unwrap()
            .unwrap();
        eprintln!("issue_keys: {:?}", facts.issue_keys);
        for repo in &facts.repos_touched {
            eprintln!("repo {} edits={}", repo.dir.display(), repo.edits);
        }
        for checkout in &facts.checkouts {
            eprintln!(
                "checkout {} [{}] edits={} last={}",
                checkout.dir.display(),
                checkout.branch.as_deref().unwrap_or("-"),
                checkout.edits,
                checkout.last
            );
        }
        eprintln!("fan-out: {:?}", facts.fan_out_line());
        let mut entry = entry(&id, "real transcript");
        entry.facts = Some(facts.clone());
        let queries = std::env::var("GITTERM_REAL_QUERIES").unwrap_or_default();
        for query in queries.split(',').map(str::trim).filter(|q| !q.is_empty()) {
            eprintln!("query {query:?}: {:?}", entry.query_match(query));
            assert!(entry.matches_query(query), "query {query:?} did not match");
        }
    }
}
