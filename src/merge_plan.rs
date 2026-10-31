//! PRs of a piece of work and the order to merge them in (TRU-173).
//!
//! A coordinator chat (a root chat, TRU-169) produces PRs across repos and
//! tells the user which order to merge them in. GitTerm holds the list and
//! the order:
//!
//! - **Collection.** A chat tab records a PR in the chat's facts
//!   ([`crate::chat_facts::ChatFacts::prs`]) when a Bash tool result of a
//!   `gh pr create` command carries a `https://github.com/<owner>/<repo>/pull/<n>`
//!   URL ([`is_gh_pr_create`], [`pr_urls_in`]). Children's PRs count towards
//!   their root through `chat_facts::resolve_tree`.
//! - **The plan.** `<config>/merge-plans/<root chat id>.json`, written by the
//!   `merge_plan_set` MCP tool for the calling chat's root. [`order_plan`]
//!   orders it: an explicit `step` or `after` wins; a same-repo stack (a PR
//!   whose base is another plan PR's head) is ordered automatically;
//!   everything else keeps plan order. The next to merge is the first open
//!   PR whose `after` entries are all merged and which has no hold. A hold
//!   stays until the user clears it from the row: a replacing plan that
//!   omits it keeps it ([`MergePlan::replace_entries`]).
//!
//! Live state comes from `pr_watch`; nothing here talks to GitHub.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Directory under the GitTerm config dir that holds the plans.
pub const PLANS_DIR_NAME: &str = "merge-plans";
/// The only host PR URLs are collected for.
const GITHUB_PULL_PREFIX: &str = "https://github.com/";
/// Hold and note text kept per entry.
const FREE_TEXT_MAX_CHARS: usize = 300;

// ---------------------------------------------------------------------------
// PR identity.

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

fn repo_name_byte(byte: u8) -> bool {
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

// ---------------------------------------------------------------------------
// Collection from `gh pr create` results.

/// Whether a Bash command runs `gh pr create` (anywhere in a pipeline or
/// `&&` chain, with or without env assignments or a path to `gh`).
pub fn is_gh_pr_create(command: &str) -> bool {
    let words: Vec<&str> = command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        .filter(|word| !word.is_empty())
        .collect();
    words.windows(3).any(|window| {
        let program = window[0].trim_matches(|c| c == '"' || c == '\'');
        (program == "gh" || program.ends_with("/gh")) && window[1] == "pr" && window[2] == "create"
    })
}

/// Every `https://github.com/<owner>/<repo>/pull/<n>` URL in `text`, in
/// order, each once. The number must end the URL path (`/pull/12/files`
/// still names #12; `/pull/12abc` names nothing).
pub fn pr_urls_in(text: &str) -> Vec<PrRef> {
    let mut found: Vec<PrRef> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(GITHUB_PULL_PREFIX) {
        let after = &rest[at + GITHUB_PULL_PREFIX.len()..];
        if let Some(pr) = parse_pull_path(after) {
            if !found.iter().any(|known| known.key().id() == pr.key().id()) {
                found.push(pr);
            }
        }
        rest = after;
    }
    found
}

/// `<owner>/<repo>/pull/<n>…` at the start of `path`.
fn parse_pull_path(path: &str) -> Option<PrRef> {
    let bytes = path.as_bytes();
    let take_name = |from: usize| -> usize {
        bytes[from..]
            .iter()
            .take_while(|byte| repo_name_byte(**byte))
            .count()
    };
    let owner_len = take_name(0);
    if owner_len == 0 || bytes.get(owner_len) != Some(&b'/') {
        return None;
    }
    let name_start = owner_len + 1;
    let name_len = take_name(name_start);
    let name_end = name_start + name_len;
    if name_len == 0 || !path[name_end..].starts_with("/pull/") {
        return None;
    }
    let digits_start = name_end + "/pull/".len();
    let digits = bytes[digits_start..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let digits_end = digits_start + digits;
    if digits == 0
        || bytes
            .get(digits_end)
            .is_some_and(|b| b.is_ascii_alphanumeric())
    {
        return None;
    }
    let number: u64 = path[digits_start..digits_end].parse().ok()?;
    let repo = &path[..name_end];
    if number == 0 || !valid_repo(repo) {
        return None;
    }
    let repo = repo.to_string();
    Some(PrRef {
        url: format!("{GITHUB_PULL_PREFIX}{repo}/pull/{number}"),
        repo,
        number,
    })
}

// ---------------------------------------------------------------------------
// The plan.

/// How a PR merges (`gh pr merge --merge|--squash|--rebase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    // A merge commit: the default when the plan names none. (A plain
    // comment: a doc comment here would split the tool schema's enum.)
    #[default]
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    pub const ALL: [MergeMethod; 3] =
        [MergeMethod::Merge, MergeMethod::Squash, MergeMethod::Rebase];

    pub fn label(self) -> &'static str {
        match self {
            MergeMethod::Merge => "merge",
            MergeMethod::Squash => "squash",
            MergeMethod::Rebase => "rebase",
        }
    }

    /// The `gh pr merge` flag.
    pub fn flag(self) -> &'static str {
        match self {
            MergeMethod::Merge => "--merge",
            MergeMethod::Squash => "--squash",
            MergeMethod::Rebase => "--rebase",
        }
    }
}

/// One PR in a merge plan, as `merge_plan_set` takes it and the plan file
/// stores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlanEntry {
    /// The PR's repository, `owner/name`.
    pub repo: String,
    /// The PR number.
    pub number: u64,
    /// Explicit merge step (1 = first). Entries without one keep plan order
    /// after the step before them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<u32>,
    /// PRs of this plan that must be merged first, as `owner/name#n`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// Why this PR must wait for something GitTerm cannot see (for example
    /// "until acme/api#12 is in production"). Only the user clears a hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<String>,
    /// How to merge it: merge (the default), squash or rebase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<MergeMethod>,
    /// A short note for the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl PlanEntry {
    pub fn key(&self) -> PrKey {
        PrKey::new(&self.repo, self.number)
    }

    /// The explicit ordering this entry carries; it then gets no automatic
    /// stack ordering.
    fn explicit(&self) -> bool {
        self.step.is_some() || !self.after.is_empty()
    }
}

/// A root chat's merge plan, `<config>/merge-plans/<root chat id>.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePlan {
    pub root_chat_id: String,
    /// RFC 3339 UTC of the last change.
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub entries: Vec<PlanEntry>,
}

impl MergePlan {
    pub fn entry(&self, key: &PrKey) -> Option<&PlanEntry> {
        let id = key.id();
        self.entries.iter().find(|entry| entry.key().id() == id)
    }

    /// Replaces the entries with `entries` (already [`validate_entries`]d).
    /// A hold the old plan had on a PR that is still in the plan stays when
    /// the new entry names none: only the user clears a hold. Returns the
    /// PRs whose hold was kept.
    pub fn replace_entries(&mut self, mut entries: Vec<PlanEntry>, at: &str) -> Vec<PrKey> {
        let mut kept = Vec::new();
        for entry in &mut entries {
            if entry.hold.is_some() {
                continue;
            }
            if let Some(hold) = self.entry(&entry.key()).and_then(|old| old.hold.clone()) {
                entry.hold = Some(hold);
                kept.push(entry.key());
            }
        }
        self.entries = entries;
        self.updated_at = at.to_string();
        kept
    }

    /// The user cleared `key`'s hold from its row. Returns whether one was
    /// set.
    pub fn clear_hold(&mut self, key: &PrKey, at: &str) -> bool {
        let id = key.id();
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.key().id() == id && entry.hold.is_some())
        else {
            return false;
        };
        entry.hold = None;
        self.updated_at = at.to_string();
        true
    }
}

fn trimmed_text(text: Option<String>) -> Option<String> {
    let text = text?;
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return None;
    }
    let mut kept: String = text.chars().take(FREE_TEXT_MAX_CHARS).collect();
    if text.chars().count() > FREE_TEXT_MAX_CHARS {
        kept.push('\u{2026}');
    }
    Some(kept)
}

/// Checks and normalises a plan from `merge_plan_set`: `repo` is
/// `owner/name`, numbers are positive, no PR twice, every `after` names
/// another PR of the plan as `owner/name#n`, and the explicit `after`
/// links have no cycle. Hold and note text is trimmed; empty text is none.
pub fn validate_entries(entries: Vec<PlanEntry>) -> Result<Vec<PlanEntry>, String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<PlanEntry> = Vec::with_capacity(entries.len());
    for mut entry in entries {
        entry.repo = entry.repo.trim().to_string();
        if !valid_repo(&entry.repo) {
            return Err(format!(
                "repo {:?} is not owner/name (for example acme/api)",
                entry.repo
            ));
        }
        if entry.number == 0 {
            return Err(format!("{}: the PR number must be positive", entry.repo));
        }
        if !seen.insert(entry.key().id()) {
            return Err(format!("{} is in the plan twice", entry.key().label()));
        }
        entry.hold = trimmed_text(entry.hold);
        entry.note = trimmed_text(entry.note);
        out.push(entry);
    }
    for entry in &mut out {
        let mut after = Vec::new();
        for text in &entry.after {
            let Some(key) = PrKey::parse(text) else {
                return Err(format!(
                    "{}: after entry {text:?} is not owner/name#n",
                    entry.key().label()
                ));
            };
            if key.id() == entry.key().id() {
                return Err(format!("{} cannot wait for itself", entry.key().label()));
            }
            if !seen.contains(&key.id()) {
                return Err(format!(
                    "{}: after names {}, which is not in the plan; add it as an entry",
                    entry.key().label(),
                    key.label()
                ));
            }
            let id = key.id();
            if !after
                .iter()
                .any(|known: &String| PrKey::parse(known).is_some_and(|k| k.id() == id))
            {
                after.push(key.label());
            }
        }
        entry.after = after;
    }
    let ids: Vec<String> = out.iter().map(|entry| entry.key().id()).collect();
    let deps: Vec<Vec<usize>> = out
        .iter()
        .map(|entry| {
            entry
                .after
                .iter()
                .filter_map(|text| PrKey::parse(text))
                .filter_map(|key| ids.iter().position(|id| *id == key.id()))
                .collect()
        })
        .collect();
    if let Some(cycle) = find_cycle(&deps) {
        let names: Vec<String> = cycle.iter().map(|&i| out[i].key().label()).collect();
        return Err(format!(
            "the after links form a cycle: {}",
            names.join(" -> ")
        ));
    }
    Ok(out)
}

/// A cycle in `deps` (entry -> entries it waits for), as entry indices.
fn find_cycle(deps: &[Vec<usize>]) -> Option<Vec<usize>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Open,
        Done,
    }
    fn visit(
        node: usize,
        deps: &[Vec<usize>],
        marks: &mut [Mark],
        path: &mut Vec<usize>,
    ) -> Option<Vec<usize>> {
        marks[node] = Mark::Open;
        path.push(node);
        for &next in &deps[node] {
            match marks[next] {
                Mark::Open => {
                    let start = path.iter().position(|&n| n == next).unwrap_or(0);
                    let mut cycle = path[start..].to_vec();
                    cycle.push(next);
                    return Some(cycle);
                }
                Mark::New => {
                    if let Some(cycle) = visit(next, deps, marks, path) {
                        return Some(cycle);
                    }
                }
                Mark::Done => {}
            }
        }
        path.pop();
        marks[node] = Mark::Done;
        None
    }
    let mut marks = vec![Mark::New; deps.len()];
    for node in 0..deps.len() {
        if marks[node] == Mark::New {
            if let Some(cycle) = visit(node, deps, &mut marks, &mut Vec::new()) {
                return Some(cycle);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Files.

pub fn plans_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(PLANS_DIR_NAME)
}

/// The plan file of root chat `root`; the id must be a safe file name.
pub fn plan_path(dir: &Path, root: &str) -> Result<PathBuf, String> {
    let safe = !root.is_empty()
        && root
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !safe {
        return Err(format!("chat id {root:?} is not a safe plan file name"));
    }
    Ok(dir.join(format!("{root}.json")))
}

/// Every plan in `dir`. A missing directory is no plans; a file that does
/// not parse is logged and left out (and left on disk).
pub fn load_plans(dir: &Path) -> HashMap<String, MergePlan> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(error) => {
            eprintln!(
                "[merge-plan] cannot list {}: {error}; no plans loaded",
                dir.display()
            );
            return HashMap::new();
        }
    };
    let mut plans = HashMap::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let parsed = std::fs::read(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))
            .and_then(|bytes| {
                serde_json::from_slice::<MergePlan>(&bytes)
                    .map_err(|error| format!("{} is not a merge plan: {error}", path.display()))
            });
        match parsed {
            Ok(plan) if !plan.root_chat_id.is_empty() => {
                plans.insert(plan.root_chat_id.clone(), plan);
            }
            Ok(_) => eprintln!(
                "[merge-plan] {} names no root chat; skipped",
                path.display()
            ),
            Err(error) => eprintln!("[merge-plan] {error}"),
        }
    }
    plans
}

/// Writes `plan` to its file in `dir` atomically, creating the directory.
/// Blocking: run it off the UI thread.
pub fn write_plan(dir: &Path, plan: &MergePlan) -> Result<(), String> {
    let path = plan_path(dir, &plan.root_chat_id)?;
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let encoded = serde_json::to_vec_pretty(plan)
        .map_err(|error| format!("cannot encode {}: {error}", path.display()))?;
    write_atomically(&path, &encoded)
}

/// Writes `bytes` to `path` through a temporary sibling and an atomic
/// replace.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{} has no file name", path.display()))?;
    let temporary = parent.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&temporary, bytes)
        .and_then(|()| crate::tasks::replace_file_atomically(&temporary, path))
        .map_err(|error| {
            let _ = std::fs::remove_file(&temporary);
            format!("cannot write {}: {error}", path.display())
        })
}

// ---------------------------------------------------------------------------
// Ordering and the next to merge.

/// A PR's GitHub state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

impl PrState {
    pub fn label(self) -> &'static str {
        match self {
            PrState::Open => "open",
            PrState::Draft => "draft",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
        }
    }

    /// Merged or closed: nothing more will happen.
    pub fn is_final(self) -> bool {
        matches!(self, PrState::Merged | PrState::Closed)
    }
}

/// What ordering needs to know of a PR from GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrBrief {
    pub state: PrState,
    pub head_ref: String,
    pub base_ref: String,
}

/// Where a plan row stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowStatus {
    /// The one to merge next.
    Next,
    /// Open, nothing in the way, after the next one.
    Ready,
    /// Waits for these PRs (`owner/name#n`) to merge.
    Blocked(Vec<String>),
    /// The plan holds it, with this reason.
    Held(String),
    Draft,
    Merged,
    Closed,
    /// No live state yet.
    Unknown,
}

impl RowStatus {
    pub fn label(&self) -> &'static str {
        match self {
            RowStatus::Next => "next",
            RowStatus::Ready => "ready",
            RowStatus::Blocked(_) => "blocked",
            RowStatus::Held(_) => "held",
            RowStatus::Draft => "draft",
            RowStatus::Merged => "merged",
            RowStatus::Closed => "closed",
            RowStatus::Unknown => "unknown",
        }
    }
}

/// One plan entry in merge order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedRow {
    /// 1-based merge position.
    pub position: usize,
    pub entry: PlanEntry,
    /// Every PR it waits for: explicit `after` entries, or the PR whose head
    /// its base is (a same-repo stack), as `owner/name#n`.
    pub waits: Vec<String>,
    /// Its waits came from a same-repo stack, not from the plan.
    pub stacked: bool,
    pub status: RowStatus,
}

/// Orders `plan` and marks where each row stands, with `live` keyed by
/// [`PrKey::id`]:
///
/// - each entry gets a rank: its `step`, else the rank of the entry before
///   it in plan order (0 at the start);
/// - waits are its explicit `after` entries; an entry with no explicit
///   `step` or `after` waits for the plan PR in its repo whose head branch
///   is its base branch (a stack), when live state names both;
/// - rows go out lowest (rank, plan index) first among those whose waits
///   are placed; a cycle (only possible through stacks) is broken in rank
///   order.
///
/// The next to merge is the first open row whose waits are all merged and
/// which has no hold.
pub fn order_plan(plan: &MergePlan, live: &HashMap<String, PrBrief>) -> Vec<OrderedRow> {
    let entries = &plan.entries;
    let ids: Vec<String> = entries.iter().map(|entry| entry.key().id()).collect();
    let mut rank: Vec<u32> = Vec::with_capacity(entries.len());
    let mut previous = 0;
    for entry in entries {
        let own = entry.step.unwrap_or(previous);
        rank.push(own);
        previous = own;
    }
    let mut waits: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    let mut stacked = vec![false; entries.len()];
    for (i, entry) in entries.iter().enumerate() {
        if entry.explicit() {
            waits[i] = entry
                .after
                .iter()
                .filter_map(|text| PrKey::parse(text))
                .filter_map(|key| ids.iter().position(|id| *id == key.id()))
                .filter(|&j| j != i)
                .collect();
            continue;
        }
        let Some(own) = live.get(&ids[i]) else {
            continue;
        };
        let repo = entry.repo.to_ascii_lowercase();
        let below = entries.iter().enumerate().position(|(j, other)| {
            j != i
                && other.repo.to_ascii_lowercase() == repo
                && live.get(&ids[j]).is_some_and(|other| {
                    !other.head_ref.is_empty() && other.head_ref == own.base_ref
                })
        });
        if let Some(j) = below {
            waits[i].push(j);
            stacked[i] = true;
        }
    }

    let mut placed = vec![false; entries.len()];
    let mut order: Vec<usize> = Vec::with_capacity(entries.len());
    while order.len() < entries.len() {
        let pick = |need_ready: bool| {
            (0..entries.len())
                .filter(|&i| !placed[i])
                .filter(|&i| !need_ready || waits[i].iter().all(|&j| placed[j]))
                .min_by_key(|&i| (rank[i], i))
        };
        let Some(next) = pick(true).or_else(|| pick(false)) else {
            break;
        };
        placed[next] = true;
        order.push(next);
    }

    let state_of = |i: usize| live.get(&ids[i]).map(|brief| brief.state);
    let mut next_found = false;
    order
        .iter()
        .enumerate()
        .map(|(n, &i)| {
            let entry = entries[i].clone();
            let wait_labels: Vec<String> =
                waits[i].iter().map(|&j| entries[j].key().label()).collect();
            let unmerged: Vec<String> = waits[i]
                .iter()
                .filter(|&&j| state_of(j) != Some(PrState::Merged))
                .map(|&j| entries[j].key().label())
                .collect();
            let status = match state_of(i) {
                None => RowStatus::Unknown,
                Some(PrState::Merged) => RowStatus::Merged,
                Some(PrState::Closed) => RowStatus::Closed,
                Some(PrState::Draft) => RowStatus::Draft,
                Some(PrState::Open) => match (&entry.hold, unmerged.is_empty()) {
                    (Some(hold), _) => RowStatus::Held(hold.clone()),
                    (None, false) => RowStatus::Blocked(unmerged),
                    (None, true) if !next_found => {
                        next_found = true;
                        RowStatus::Next
                    }
                    (None, true) => RowStatus::Ready,
                },
            };
            OrderedRow {
                position: n + 1,
                entry,
                waits: wait_labels,
                stacked: stacked[i],
                status,
            }
        })
        .collect()
}

/// The next PR to merge in `rows`, if any.
pub fn next_to_merge(rows: &[OrderedRow]) -> Option<&OrderedRow> {
    rows.iter().find(|row| row.status == RowStatus::Next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(repo: &str, number: u64) -> PlanEntry {
        PlanEntry {
            repo: repo.to_string(),
            number,
            step: None,
            after: Vec::new(),
            hold: None,
            method: None,
            note: None,
        }
    }

    fn plan(entries: Vec<PlanEntry>) -> MergePlan {
        MergePlan {
            root_chat_id: "root-1".to_string(),
            updated_at: String::new(),
            entries,
        }
    }

    fn brief(state: PrState, head: &str, base: &str) -> PrBrief {
        PrBrief {
            state,
            head_ref: head.to_string(),
            base_ref: base.to_string(),
        }
    }

    fn order_labels(rows: &[OrderedRow]) -> Vec<String> {
        rows.iter().map(|row| row.entry.key().label()).collect()
    }

    #[test]
    fn detects_gh_pr_create_in_chains_and_with_env() {
        assert!(is_gh_pr_create("gh pr create --draft --title x"));
        assert!(is_gh_pr_create(
            "git push -u origin HEAD && gh pr create --fill"
        ));
        assert!(is_gh_pr_create(
            "GH_TOKEN=\"$(gh auth token --user dev)\" gh pr create --base main"
        ));
        assert!(is_gh_pr_create("cd /tmp/x; /opt/homebrew/bin/gh pr create"));
        assert!(!is_gh_pr_create("gh pr view 12"));
        assert!(!is_gh_pr_create("gh pr list --state open"));
        assert!(!is_gh_pr_create("echo gh pr creates"));
    }

    #[test]
    fn extracts_pr_urls_from_gh_pr_create_output() {
        let output = "Warning: 1 uncommitted change\n\nCreating pull request for feature/prj-101 into main in acme/api\n\nhttps://github.com/acme/api/pull/12\n";
        assert_eq!(
            pr_urls_in(output),
            vec![PrRef {
                repo: "acme/api".to_string(),
                number: 12,
                url: "https://github.com/acme/api/pull/12".to_string(),
            }]
        );
        // An existing PR is reported the same way; each PR once.
        let output = "a pull request for branch \"x\" into branch \"main\" already exists:\nhttps://github.com/acme/web.app/pull/841\nhttps://github.com/acme/web.app/pull/841/files";
        let found = pr_urls_in(output);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key().label(), "acme/web.app#841");
        // Not PR URLs.
        assert!(pr_urls_in("https://github.com/acme/api/issues/12").is_empty());
        assert!(pr_urls_in("https://github.com/acme/api/pull/12abc").is_empty());
        assert!(pr_urls_in("https://github.com/acme/pull/12").is_empty());
        assert!(pr_urls_in("https://example.com/acme/api/pull/12").is_empty());
        // Two PRs in one result.
        let both = pr_urls_in(
            "(https://github.com/acme/api/pull/12) and https://github.com/Acme/Web/pull/7.",
        );
        assert_eq!(
            both.iter().map(|pr| pr.key().label()).collect::<Vec<_>>(),
            vec!["acme/api#12", "Acme/Web#7"]
        );
    }

    #[test]
    fn pr_keys_parse_and_compare_case_insensitively() {
        let key = PrKey::parse("Acme/API#12").unwrap();
        assert_eq!(key.id(), "acme/api#12");
        assert_eq!(key.label(), "Acme/API#12");
        assert_eq!(key.owner(), "Acme");
        assert_eq!(key.url(), "https://github.com/Acme/API/pull/12");
        assert!(PrKey::parse("acme/api#0").is_none());
        assert!(PrKey::parse("acme#12").is_none());
        assert!(PrKey::parse("acme/api/12").is_none());
    }

    #[test]
    fn validation_rejects_bad_plans_and_normalises_text() {
        assert!(validate_entries(vec![entry("acme", 1)]).is_err());
        assert!(validate_entries(vec![entry("acme/api", 0)]).is_err());
        assert!(validate_entries(vec![entry("acme/api", 1), entry("ACME/api", 1)]).is_err());
        let mut waits_outside = entry("acme/api", 2);
        waits_outside.after = vec!["acme/api#9".to_string()];
        let error = validate_entries(vec![entry("acme/api", 1), waits_outside]).unwrap_err();
        assert!(error.contains("not in the plan"), "{error}");
        let mut a = entry("acme/api", 1);
        a.after = vec!["acme/web#2".to_string()];
        let mut b = entry("acme/web", 2);
        b.after = vec!["acme/api#1".to_string()];
        let error = validate_entries(vec![a, b]).unwrap_err();
        assert!(error.contains("cycle"), "{error}");

        let mut held = entry("acme/api", 3);
        held.hold = Some("  until acme/api#1\n is in production ".to_string());
        held.note = Some("   ".to_string());
        held.after = vec![" acme/api#1".to_string(), "Acme/API#1".to_string()];
        let out = validate_entries(vec![entry("acme/api", 1), held]).unwrap();
        assert_eq!(
            out[1].hold.as_deref(),
            Some("until acme/api#1 is in production")
        );
        assert_eq!(out[1].note, None);
        assert_eq!(out[1].after, vec!["acme/api#1".to_string()]);
    }

    #[test]
    fn explicit_steps_order_and_unstepped_entries_keep_plan_order() {
        let mut a = entry("acme/api", 1);
        a.step = Some(2);
        let b = entry("acme/web", 2);
        let mut c = entry("acme/infra", 3);
        c.step = Some(1);
        let d = entry("acme/api", 4);
        let rows = order_plan(&plan(vec![a, b, c, d]), &HashMap::new());
        // a and b rank 2 (b inherits a's), c ranks 1, d inherits c's 1.
        assert_eq!(
            order_labels(&rows),
            vec!["acme/infra#3", "acme/api#4", "acme/api#1", "acme/web#2"]
        );
        assert_eq!(
            rows.iter().map(|row| row.position).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert!(rows.iter().all(|row| row.status == RowStatus::Unknown));
    }

    #[test]
    fn after_wins_over_plan_order_and_blocks_until_merged() {
        let mut first = entry("acme/web", 841);
        first.after = vec!["acme/api#400".to_string()];
        let second = entry("acme/api", 400);
        let p = plan(vec![first, second]);
        let mut live = HashMap::new();
        live.insert(
            "acme/web#841".to_string(),
            brief(PrState::Open, "web-x", "main"),
        );
        live.insert(
            "acme/api#400".to_string(),
            brief(PrState::Open, "api-x", "main"),
        );
        let rows = order_plan(&p, &live);
        assert_eq!(order_labels(&rows), vec!["acme/api#400", "acme/web#841"]);
        assert_eq!(rows[0].status, RowStatus::Next);
        assert_eq!(
            rows[1].status,
            RowStatus::Blocked(vec!["acme/api#400".to_string()])
        );
        assert_eq!(next_to_merge(&rows).unwrap().entry.number, 400);

        live.insert(
            "acme/api#400".to_string(),
            brief(PrState::Merged, "api-x", "main"),
        );
        let rows = order_plan(&p, &live);
        assert_eq!(rows[0].status, RowStatus::Merged);
        assert_eq!(rows[1].status, RowStatus::Next);
    }

    #[test]
    fn same_repo_stacks_order_automatically() {
        // Listed top of the stack first; the base PR must still go first.
        let top = entry("acme/api", 13);
        let bottom = entry("acme/api", 12);
        let other_repo = entry("acme/web", 5);
        let p = plan(vec![top, other_repo, bottom]);
        let mut live = HashMap::new();
        live.insert(
            "acme/api#13".to_string(),
            brief(PrState::Open, "prj-101-b", "prj-101-a"),
        );
        live.insert(
            "acme/api#12".to_string(),
            brief(PrState::Open, "prj-101-a", "main"),
        );
        // Same branch names in another repo are not a stack.
        live.insert(
            "acme/web#5".to_string(),
            brief(PrState::Open, "x", "prj-101-a"),
        );
        let rows = order_plan(&p, &live);
        assert_eq!(
            order_labels(&rows),
            vec!["acme/web#5", "acme/api#12", "acme/api#13"]
        );
        assert!(rows[2].stacked);
        assert_eq!(rows[2].waits, vec!["acme/api#12".to_string()]);
        assert_eq!(rows[0].status, RowStatus::Next);
        assert_eq!(rows[1].status, RowStatus::Ready);
        assert_eq!(
            rows[2].status,
            RowStatus::Blocked(vec!["acme/api#12".to_string()])
        );

        // An explicit step wins over the stack.
        let mut top = entry("acme/api", 13);
        top.step = Some(1);
        let mut bottom = entry("acme/api", 12);
        bottom.step = Some(2);
        let rows = order_plan(&plan(vec![bottom, top]), &live);
        assert_eq!(order_labels(&rows), vec!["acme/api#13", "acme/api#12"]);
        assert!(!rows[0].stacked);
    }

    #[test]
    fn holds_and_drafts_are_never_next() {
        let mut held = entry("acme/api", 852);
        held.hold = Some("until acme/api#400 is in production".to_string());
        let draft = entry("acme/api", 853);
        let open = entry("acme/web", 9);
        let p = plan(vec![held, draft, open]);
        let mut live = HashMap::new();
        live.insert(
            "acme/api#852".to_string(),
            brief(PrState::Open, "a", "main"),
        );
        live.insert(
            "acme/api#853".to_string(),
            brief(PrState::Draft, "b", "main"),
        );
        live.insert("acme/web#9".to_string(), brief(PrState::Open, "c", "main"));
        let rows = order_plan(&p, &live);
        assert_eq!(
            rows[0].status,
            RowStatus::Held("until acme/api#400 is in production".to_string())
        );
        assert_eq!(rows[1].status, RowStatus::Draft);
        assert_eq!(rows[2].status, RowStatus::Next);

        // Nothing open: no next.
        live.insert(
            "acme/web#9".to_string(),
            brief(PrState::Closed, "c", "main"),
        );
        let rows = order_plan(&p, &live);
        assert!(next_to_merge(&rows).is_none());
    }

    #[test]
    fn a_replacing_plan_keeps_holds_until_the_user_clears_them() {
        let mut held = entry("acme/api", 852);
        held.hold = Some("until #400 is in production".to_string());
        let mut p = plan(vec![held, entry("acme/api", 400)]);
        let kept = p.replace_entries(
            vec![entry("acme/api", 400), entry("acme/api", 852)],
            "2026-10-31T10:00:00Z",
        );
        assert_eq!(kept, vec![PrKey::new("acme/api", 852)]);
        assert_eq!(
            p.entry(&PrKey::new("acme/api", 852))
                .unwrap()
                .hold
                .as_deref(),
            Some("until #400 is in production")
        );
        assert_eq!(p.updated_at, "2026-10-31T10:00:00Z");
        assert!(p.clear_hold(&PrKey::new("ACME/api", 852), "2026-10-31T11:00:00Z"));
        assert!(!p.clear_hold(&PrKey::new("acme/api", 852), "2026-10-31T11:00:00Z"));
        let kept = p.replace_entries(vec![entry("acme/api", 852)], "2026-10-31T12:00:00Z");
        assert!(kept.is_empty());
        assert_eq!(p.entries[0].hold, None);
    }

    #[test]
    fn plan_files_round_trip_and_minimal_files_load() {
        let dir = std::env::temp_dir().join(format!("gitterm-merge-plan-{}", uuid::Uuid::new_v4()));
        let mut stepped = entry("acme/api", 12);
        stepped.step = Some(1);
        stepped.method = Some(MergeMethod::Squash);
        let p = MergePlan {
            root_chat_id: "root-1".to_string(),
            updated_at: "2026-10-31T10:00:00Z".to_string(),
            entries: vec![stepped],
        };
        write_plan(&dir, &p).unwrap();
        // A hand-written file with only the required fields.
        std::fs::write(
            dir.join("root-2.json"),
            r#"{"root_chat_id":"root-2","entries":[{"repo":"acme/web","number":3}]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("broken.json"), "{").unwrap();
        let plans = load_plans(&dir);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans["root-1"], p);
        assert_eq!(plans["root-2"].entries[0], entry("acme/web", 3));
        let text = std::fs::read_to_string(dir.join("root-1.json")).unwrap();
        assert!(text.contains("\"method\": \"squash\""), "{text}");
        assert!(!text.contains("hold"), "{text}");
        assert!(plan_path(&dir, "../x").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
