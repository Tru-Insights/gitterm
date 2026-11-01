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
//!   transcripts included). It skips chats a tab is hosting.
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

/// What one conversation touched. Additive: every field defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatFacts {
    #[serde(default)]
    pub issue_keys: Vec<String>,
    #[serde(default)]
    pub repos_touched: Vec<RepoTouch>,
    #[serde(default)]
    pub checkouts: Vec<CheckoutTouch>,
    #[serde(default)]
    pub children: Vec<ChatChild>,
    /// PRs the chat created (TRU-173): a `gh pr create` Bash result naming
    /// a GitHub PR URL. Written by the hosting tab only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prs: Vec<PrRef>,
    /// "Index older chats" has scanned this chat's transcript; it never
    /// scans it again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub backfilled: bool,
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
// Issue keys: the repo's key pattern `[A-Z][A-Z0-9]{1,9}-[0-9]{1,7}`
// (agent-workflow.config.json), on word boundaries.

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
    pub issue_keys: Vec<String>,
    pub children: Vec<ChatChild>,
    /// PRs from `gh pr create` results (live tabs only).
    pub prs: Vec<PrRef>,
}

impl RawFacts {
    pub fn is_empty(&self) -> bool {
        self.touches.is_empty()
            && self.issue_keys.is_empty()
            && self.children.is_empty()
            && self.prs.is_empty()
    }

    pub fn observe_tool_call(
        &mut self,
        id: &str,
        name: &str,
        input: &Value,
        home: Option<&Path>,
        at: &str,
    ) {
        if let Some((kind, paths)) = tool_call_touches(name, input, home) {
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
        for child in self.children {
            facts.add_child(child);
        }
        for pr in self.prs {
            facts.add_pr(pr);
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
    nearest_checkout: HashMap<PathBuf, Option<PathBuf>>,
    worktrees: HashMap<PathBuf, Option<Vec<WorktreeInfo>>>,
}

impl GitResolver {
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
    /// Bash calls running `gh pr create` whose result has not arrived
    /// (TRU-173); their result names the PR.
    pr_create_calls: HashSet<String>,
    last_flush: Option<Instant>,
    in_flight: bool,
}

impl LiveFacts {
    pub fn observe(&mut self, event: &HarnessEvent, home: Option<&Path>) {
        if let Some(call) = self.tracker.observe_calls(event, is_facts_tool) {
            self.pending
                .observe_tool_call(&call.id, &call.name, &call.input, home, &now_stamp());
            let creates_pr = call.name == "Bash"
                && call
                    .input
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(merge_plan::is_gh_pr_create);
            if creates_pr {
                self.pr_create_calls.insert(call.id);
            }
        }
        self.observe_results(event);
    }

    /// A `gh pr create` result (the call's input may complete with the
    /// same event, so this runs after the tracker).
    fn observe_results(&mut self, event: &HarnessEvent) {
        match event {
            HarnessEvent::SubagentEvent { event, .. } => self.observe_results(event),
            HarnessEvent::ItemCompleted { id, output, .. } if self.pr_create_calls.remove(id) => {
                self.pending.prs.extend(merge_plan::pr_urls_in(output));
            }
            HarnessEvent::ProcessExited { .. } => self.pr_create_calls.clear(),
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

    /// Whether observations wait for a write.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// Folds a live tab's observations and its delegation children into the
/// chat's facts file. Blocking (git, file I/O); run on a blocking thread.
/// A file that exists but does not parse is left alone and reported.
pub fn flush_live(
    dir: &Path,
    backend: ChatBackend,
    session_id: &str,
    pending: RawFacts,
    children: Vec<ChatChild>,
) -> Result<(), String> {
    let path = facts_path(dir, backend, session_id)?;
    let mut facts = load_facts(&path)?.unwrap_or_default();
    let before = facts.clone();
    pending.resolve_into(&mut facts, &mut GitResolver::default());
    for child in children {
        facts.add_child(child);
    }
    if facts == before {
        return Ok(());
    }
    write_facts(&path, &facts)
}

// ---------------------------------------------------------------------------
// Transcript scan (backfill).

/// Folds one Claude transcript line into `raw`. `main` is false for a
/// subagent transcript, whose user lines are the coordinator's brief, not
/// the human's words.
pub fn observe_claude_line(raw: &mut RawFacts, line: &str, home: Option<&Path>, main: bool) {
    // Most lines are neither tool calls nor user text; skip them unparsed.
    let user_line = main && line.contains("\"user\"");
    if !user_line && !line.contains("\"tool_use\"") {
        return;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let kind = value.get("type").and_then(Value::as_str);
    let Some(content) = value.get("message").and_then(|m| m.get("content")) else {
        return;
    };
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
                raw.observe_tool_call(id, name, input, home, &at);
            }
        }
        _ => {}
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
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => observe_claude_line(raw, &line, home, main),
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
    if let Ok(read) = std::fs::read_dir(&subagents) {
        let mut files: Vec<PathBuf> = read
            .flatten()
            .map(|entry| entry.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();
        files.sort();
        for file in files {
            let mut sub = RawFacts::default();
            if let Some(child) = subagent_from_meta(&file) {
                sub.add_child(child);
            }
            if let Err(error) = scan_lines(&file, &mut sub, home, false) {
                eprintln!("[chat-facts] cannot read {}: {error}", file.display());
            }
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
    home: Option<&Path>,
) -> BackfillReport {
    targets.sort_by_key(|target| std::cmp::Reverse(target.mtime));
    let mut report = BackfillReport::default();
    let mut resolver = GitResolver::default();
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
        let stale = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .is_ok_and(|written| target.mtime > written);
        if existing
            .as_ref()
            .is_some_and(|facts| facts.backfilled && !stale)
        {
            report.already += 1;
            continue;
        }
        if report.scanned >= max {
            report.remaining += 1;
            continue;
        }
        let scan = match facts_from_claude_transcript(&target.path, home, &mut resolver) {
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
        let mut facts = existing.unwrap_or_default();
        facts.absorb_scan(scan);
        for link in links.iter().filter(|link| link.parent == target.id) {
            facts.add_child(link.child.clone());
        }
        facts.backfilled = true;
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
            None,
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
        let again = backfill_claude(targets.clone(), &facts_dir, &HashSet::new(), &[], 10, None);
        assert_eq!((again.scanned, again.already), (0, 1));
        // A transcript that moved on after the facts were written is
        // scanned again; an untouched one is not.
        let moved_on = vec![BackfillTarget {
            id: "s1".into(),
            path: transcript.clone(),
            mtime: SystemTime::now() + std::time::Duration::from_secs(3600),
        }];
        let rescan = backfill_claude(moved_on, &facts_dir, &HashSet::new(), &[], 10, None);
        assert_eq!((rescan.scanned, rescan.already), (1, 0));
        let hosted: HashSet<String> = ["s1".to_string()].into();
        let skipped = backfill_claude(targets, &facts_dir, &hosted, &[], 10, None);
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
            None,
        );
        assert_eq!((bounded.scanned, bounded.remaining), (0, 1));

        // The live writer adds to the same file without losing the scan.
        let mut pending = RawFacts::default();
        pending.observe_user_text("now TRU-5");
        flush_live(&facts_dir, ChatBackend::Claude, "s1", pending, vec![]).unwrap();
        let facts = load_facts(&facts_dir.join("claude-s1.json"))
            .unwrap()
            .unwrap();
        assert_eq!(facts.issue_keys, vec!["PRJ-6", "PRJ-7", "TRU-5"]);
        assert!(facts.backfilled);
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
            Some(&home),
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
