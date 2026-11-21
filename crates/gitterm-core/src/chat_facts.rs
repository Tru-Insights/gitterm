//! Neutral chat facts DTOs, pure accumulation and read-only facts/index services.

use crate::chats::{ChatBackend, ChatIndexEntry};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Directory under the explicit config root containing facts files.
pub const FACTS_DIR_NAME: &str = "chat-meta";
#[doc(hidden)]
pub const GITHUB_PULL_PREFIX: &str = "https://github.com/";

/// A pull request: `owner/name` and its number. GitHub names are
/// case-insensitive, so identity is [`PrKey::id`], lowercased; `repo` keeps
/// the case it was given in for display.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PrKey {
    pub repo: String,
    pub number: u64,
}

impl PrKey {
    pub fn new(repo: &str, number: u64) -> Self {
        Self {
            repo: repo.to_string(),
            number,
        }
    }

    /// `owner/name#n`, lowercased: the key maps and comparisons use.
    pub fn id(&self) -> String {
        format!("{}#{}", self.repo.to_ascii_lowercase(), self.number)
    }

    /// `owner/name#n` as given.
    pub fn label(&self) -> String {
        format!("{}#{}", self.repo, self.number)
    }

    pub fn owner(&self) -> &str {
        self.repo.split_once('/').map_or("", |(owner, _)| owner)
    }

    pub fn url(&self) -> String {
        format!("{GITHUB_PULL_PREFIX}{}/pull/{}", self.repo, self.number)
    }

    /// Parses `owner/name#n`.
    pub fn parse(text: &str) -> Option<Self> {
        let (repo, number) = text.trim().rsplit_once('#')?;
        let number: u64 = number.parse().ok().filter(|n| *n > 0)?;
        valid_repo(repo).then(|| Self::new(repo, number))
    }
}

/// `owner/name` with GitHub's allowed characters on both sides.
pub fn valid_repo(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    let part_ok = |part: &str| !part.is_empty() && part.bytes().all(repo_name_byte);
    part_ok(owner) && part_ok(name) && name != "." && name != ".."
}

#[doc(hidden)]
pub fn repo_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

/// A PR a chat created, as recorded in its facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRef {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
    pub url: String,
}

impl PrRef {
    pub fn key(&self) -> PrKey {
        PrKey::new(&self.repo, self.number)
    }
}

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
    /// Confirmed create PR keys from live tabs, transcripts or workers.
    /// The historical field name is retained for saved facts. Reconciliation never
    /// removes these, even when a transcript lags or has been compacted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub live_created_prs: Vec<String>,
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
        for id in scan.live_created_prs {
            if !self.live_created_prs.contains(&id) {
                self.live_created_prs.push(id);
            }
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

/// The issue key a branch or folder name carries, which is usually
/// lowercase (`tracey/tru-166-chat-facts`, `blobfs-prj-102`); returned
/// uppercase. Names are hyphen-joined words, so a key may sit between
/// separators here. Only the first key counts: words after it read like
/// keys too (`tru-274-stage-5-pr-17`). A UUID (a task branch's id) is
/// skipped, since its hex groups can (`e5f6-4504`).
pub fn issue_keys_in_name(name: &str) -> Vec<String> {
    let mut upper = name.to_ascii_uppercase().into_bytes();
    blank_uuids(&mut upper);
    let upper = String::from_utf8(upper).expect("blanking ASCII keeps UTF-8");
    scan_issue_keys(&upper, false).into_iter().take(1).collect()
}

/// Overwrites each standalone UUID (`8-4-4-4-12` hex digits) with `/`.
fn blank_uuids(bytes: &mut [u8]) {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let uuid_len = |at: &[u8]| -> Option<usize> {
        let mut len = 0;
        for (index, group) in GROUPS.iter().enumerate() {
            if index > 0 {
                if at.get(len) != Some(&b'-') {
                    return None;
                }
                len += 1;
            }
            let hex = at.get(len..len + group)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            len += group;
        }
        (!at.get(len).is_some_and(u8::is_ascii_alphanumeric)).then_some(len)
    };
    let mut i = 0;
    while i < bytes.len() {
        if i == 0 || !bytes[i - 1].is_ascii_alphanumeric() {
            if let Some(len) = uuid_len(&bytes[i..]) {
                bytes[i..i + len].fill(b'/');
                i += len;
                continue;
            }
        }
        i += 1;
    }
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

/// A child session named by GitTerm's own records (delegations, tasks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildLink {
    /// Session id of the parent chat.
    pub parent: String,
    pub child: ChatChild,
}

/// Follow direct parents so flattened indexed ancestry cannot skip an owned root.
fn saved_chat_root<'a>(
    chat: &'a str,
    index: &'a [ChatIndexEntry],
    parents: &'a HashMap<String, String>,
    root: &str,
) -> Option<&'a str> {
    let mut current = chat;
    let mut seen = HashSet::new();
    while seen.insert(current) {
        if current == root {
            return Some(current);
        }
        let parent = parents.get(current).or_else(|| {
            index
                .iter()
                .find(|entry| entry.id == current)
                .and_then(|entry| entry.root_id.as_ref())
        });
        let Some(parent) = parent else {
            return Some(current);
        };
        current = parent;
    }
    None
}

/// Read saved facts by id, including descendants absent from the index.
/// Iced calls it when PR homes rebuild, never from a view.
pub fn load_root_prs(
    dir: &Path,
    index: &[ChatIndexEntry],
    links: &[ChildLink],
    root: &str,
    mut log: impl FnMut(&str),
) -> Vec<PrRef> {
    let mut parents = HashMap::new();
    // Match resolve_tree's first indexed parent before using raw links for
    // chats whose parents are also absent from the index.
    for entry in index {
        for child in entry.facts.iter().flat_map(|facts| &facts.children) {
            if child.session_id != entry.id {
                parents
                    .entry(child.session_id.clone())
                    .or_insert_with(|| entry.id.clone());
            }
        }
    }
    for link in links {
        if link.child.session_id != link.parent {
            parents
                .entry(link.child.session_id.clone())
                .or_insert_with(|| link.parent.clone());
        }
    }
    let mut pending = vec![root.to_string()];
    pending.extend(
        crate::work::summary::chats_under(index, &[root.to_string()])
            .into_iter()
            .map(|entry| entry.id.clone()),
    );
    let mut seen = HashSet::new();
    let mut prs = Vec::new();
    while let Some(chat) = pending.pop() {
        if !seen.insert(chat.clone()) {
            continue;
        }
        if chat != root
            && saved_chat_root(&chat, index, &parents, root).is_some_and(|owner| owner != root)
        {
            continue;
        }
        pending.extend(
            links_for_parent(links, &chat)
                .into_iter()
                .map(|child| child.session_id),
        );
        for entry in index.iter().filter(|entry| entry.id == chat) {
            if let Some(facts) = &entry.facts {
                prs.extend(facts.prs.iter().cloned());
                pending.extend(facts.children.iter().map(|child| child.session_id.clone()));
            }
        }
        // Owned lists carry ids without a backend. Probe the supported facts
        // names so an unindexed chat needs no transcript or index entry.
        for backend in ChatBackend::ALL {
            let path = match facts_path(dir, backend, &chat) {
                Ok(path) => path,
                Err(error) => {
                    log(&format!(
                        "[pr-watch] facts for chat {chat:?} in {}: {error}",
                        dir.display()
                    ));
                    break;
                }
            };
            match load_facts(&path) {
                Ok(Some(facts)) => {
                    prs.extend(facts.prs);
                    for child in facts.children {
                        if child.session_id != chat {
                            parents
                                .entry(child.session_id.clone())
                                .or_insert_with(|| chat.clone());
                        }
                        pending.push(child.session_id);
                    }
                }
                Ok(None) => {}
                Err(error) => log(&format!(
                    "[pr-watch] facts for chat {chat:?} at {}: {error}",
                    path.display()
                )),
            }
        }
    }
    prs
}

/// Follows `chat`'s parents through `links` to the top, stopping at a cycle.
pub fn root_through_links(links: &[ChildLink], chat: &str) -> String {
    let mut current = chat.to_string();
    let mut seen: HashSet<String> = HashSet::from([current.clone()]);
    while let Some(link) = links.iter().find(|link| link.child.session_id == current) {
        if !seen.insert(link.parent.clone()) {
            break;
        }
        current = link.parent.clone();
    }
    current
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

    #[test]
    fn names_give_their_first_issue_key_and_never_a_uuid_piece() {
        for (name, keys) in [
            ("TRU-274-tru-274-stage-5-pr-17-delegation", vec!["TRU-274"]),
            (
                "task/0d59b300-9d11-4504-836a-6fb46ceb69d3-tru-274-task-host-h4b",
                vec!["TRU-274"],
            ),
            // UUID groups that read like keys (`E5F6-4504`, `AB12CDEF-1234`).
            (
                "task/0d59b300-e5f6-4504-836a-6fb46ceb69d3-tru-274-task-host-h4b",
                vec!["TRU-274"],
            ),
            (
                "task/ab12cdef-1234-4504-836a-6fb46ceb69d3-tru-274-task-host-h4b",
                vec!["TRU-274"],
            ),
            ("task/ab12cdef-1234-4504-836a-6fb46ceb69d3", vec![]),
            ("tru-143/chips", vec!["TRU-143"]),
            ("pr/TRU-293", vec!["TRU-293"]),
            ("pr/task-host-h4b", vec![]),
        ] {
            assert_eq!(issue_keys_in_name(name), keys, "{name}");
        }
    }
    #[test]
    fn facts_read_errors_and_legacy_shape_stay_distinct() {
        let temp = tempfile::tempdir().unwrap();
        let path = facts_path(temp.path(), ChatBackend::Claude, "fixture-id").unwrap();
        assert_eq!(load_facts(&path).unwrap(), None);
        assert!(!path.exists());
        let legacy = r#"{"issue_keys":["TEST-42"],"unknown_future_field":true}"#;
        std::fs::write(&path, legacy).unwrap();
        let facts = load_facts(&path).unwrap().unwrap();
        assert_eq!(facts.issue_keys, ["TEST-42"]);
        assert!(facts.prs.is_empty() && facts.documents.is_empty() && !facts.backfilled);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), legacy);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(
            load_facts(&path).unwrap_err(),
            format!(
                "{} is not a facts file: expected ident at line 1 column 2",
                path.display()
            )
        );
        let directory = temp.path().join("directory");
        std::fs::create_dir(&directory).unwrap();
        assert!(load_facts(&directory)
            .unwrap_err()
            .starts_with(&format!("cannot read {}: ", directory.display())));
        assert_eq!(
            facts_path(temp.path(), ChatBackend::Claude, "../escape").unwrap_err(),
            "session id \"../escape\" is not a safe facts file name"
        );
    }
}
