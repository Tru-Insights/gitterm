//! Follow the git worktree a chat tab's agent works in (TRU-146, slice 1).
//!
//! A workspace opens one folder; agents then often move into another
//! worktree of the same repository (Claude Code worktree isolation under
//! `<repo>/.claude/worktrees/`, a sibling `git worktree add`, ...) and keep
//! working there. This module holds the pure rules that decide, from a chat
//! tab's tool calls, which worktree the agent is in:
//!
//! - candidate paths come from path-bearing tool inputs (Read, Write, Edit,
//!   MultiEdit, NotebookEdit, Glob, Grep) and from a Bash command's leading
//!   `cd <path>` or `git -C <path>`;
//! - a candidate inside another worktree of the tab's repository makes that
//!   worktree the tab's active one; a candidate inside the tab's own
//!   worktree clears it; a candidate outside every worktree is ignored.
//!
//! The UI layer (main.rs) owns the git calls' scheduling and the panels;
//! nothing here touches iced.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::harness::{HarnessEvent, ItemKind};

/// The worktree a tab's agent was last seen working in, when that is not
/// the tab's own worktree. Derived state: never persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveWorktree {
    pub path: PathBuf,
    /// Short branch name; `None` for a detached HEAD.
    pub branch: Option<String>,
}

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    /// Short branch name; `None` for a detached HEAD.
    pub branch: Option<String>,
}

/// Parses `git worktree list --porcelain`. Bare entries have no working
/// tree and are skipped.
pub fn parse_worktree_porcelain(text: &str) -> Vec<WorktreeInfo> {
    let mut out = Vec::new();
    let mut current: Option<(WorktreeInfo, bool)> = None;
    for line in text.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some((entry, bare)) = current.take() {
                if !bare {
                    out.push(entry);
                }
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some((entry, bare)) = current.take() {
                if !bare {
                    out.push(entry);
                }
            }
            current = Some((
                WorktreeInfo {
                    path: PathBuf::from(path),
                    branch: None,
                },
                false,
            ));
            continue;
        }
        let Some((entry, bare)) = current.as_mut() else {
            continue;
        };
        if let Some(reference) = line.strip_prefix("branch ") {
            let short = reference.strip_prefix("refs/heads/").unwrap_or(reference);
            entry.branch = Some(short.to_string());
        } else if line == "bare" {
            *bare = true;
        }
    }
    out
}

/// Lists the worktrees of the repository containing `repo`, with each
/// existing path canonicalized so matches survive symlinked roots.
pub fn list_worktrees(repo: &Path) -> Result<Vec<WorktreeInfo>, String> {
    let output = crate::agentd::git::git_command()
        .args(["--no-optional-locks", "worktree", "list", "--porcelain"])
        .current_dir(repo)
        .output()
        .map_err(|error| {
            format!(
                "failed to run git worktree list in {}: {error}",
                repo.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "git worktree list in {} failed: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(
        parse_worktree_porcelain(&String::from_utf8_lossy(&output.stdout))
            .into_iter()
            .map(|entry| WorktreeInfo {
                path: resolve_for_match(&entry.path),
                branch: entry.branch,
            })
            .collect(),
    )
}

/// `path` with its deepest existing ancestor canonicalized and `.` / `..`
/// folded lexically, so a file that does not exist yet (a Write target)
/// still compares against canonical worktree roots.
pub fn resolve_for_match(path: &Path) -> PathBuf {
    let path = normalize_lexically(path);
    let mut existing = path.as_path();
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(existing) {
            let mut resolved = canonical;
            for part in rest.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return path,
        }
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Tools whose input names a file or directory.
const PATH_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "Glob",
    "Grep",
];
const PATH_KEYS: &[&str] = &["file_path", "path", "notebook_path"];

fn is_watched_tool(name: &str) -> bool {
    name == "Bash" || PATH_TOOLS.contains(&name)
}

/// Absolute paths a tool call says the agent is working at. Relative paths
/// are ignored: the agent's cwd is not known here.
pub fn tool_call_candidates(name: &str, input: &Value, home: Option<&Path>) -> Vec<PathBuf> {
    if PATH_TOOLS.contains(&name) {
        return PATH_KEYS
            .iter()
            .filter_map(|key| input.get(*key).and_then(Value::as_str))
            .filter_map(|raw| absolute_path(raw, home))
            .collect();
    }
    if name == "Bash" {
        return input
            .get("command")
            .and_then(Value::as_str)
            .and_then(|command| bash_leading_dir(command, home))
            .into_iter()
            .collect();
    }
    Vec::new()
}

fn absolute_path(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    let expanded = if raw == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home?.join(rest)
    } else {
        PathBuf::from(raw)
    };
    expanded
        .is_absolute()
        .then(|| normalize_lexically(&expanded))
}

/// The directory of a Bash command's leading `cd <path>` or
/// `git -C <path>`, when it is absolute (or `~`-relative).
pub fn bash_leading_dir(command: &str, home: Option<&Path>) -> Option<PathBuf> {
    let words = first_simple_command_words(command);
    let raw = match words.as_slice() {
        [cd, dir, ..] if cd == "cd" => dir,
        [git, flag, dir, ..] if git == "git" && flag == "-C" => dir,
        _ => return None,
    };
    absolute_path(raw, home)
}

/// Words of the first simple command, honouring single and double quotes
/// and stopping at the first unquoted `;`, `&`, `|` or newline.
fn first_simple_command_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = command.trim_start().chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        word.push(next);
                        in_word = true;
                    }
                }
                ';' | '&' | '|' | '\n' => break,
                c if c.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut word));
                        in_word = false;
                    }
                }
                c => {
                    word.push(c);
                    in_word = true;
                }
            },
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// The innermost worktree containing `path` (worktrees can nest: Claude
/// Code puts its worktrees under `<repo>/.claude/worktrees/`).
pub fn containing_worktree<'a>(
    path: &Path,
    worktrees: &'a [WorktreeInfo],
) -> Option<&'a WorktreeInfo> {
    worktrees
        .iter()
        .filter(|worktree| path.starts_with(&worktree.path))
        .max_by_key(|worktree| worktree.path.components().count())
}

/// What one candidate path means for a tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowDecision {
    /// The agent works in another worktree of the tab's repository.
    Follow(ActiveWorktree),
    /// The agent works in the tab's own worktree again.
    ReturnToRoot,
    /// The path is outside every worktree of the repository.
    Ignore,
}

/// Decides what `candidate` means for a tab whose own checkout is
/// `own_root`. All paths are expected already resolved
/// (`resolve_for_match` / `list_worktrees`).
pub fn decide(candidate: &Path, own_root: &Path, worktrees: &[WorktreeInfo]) -> FollowDecision {
    let Some(hit) = containing_worktree(candidate, worktrees) else {
        return FollowDecision::Ignore;
    };
    let own = containing_worktree(own_root, worktrees);
    if own.is_some_and(|own| own.path == hit.path) {
        return FollowDecision::ReturnToRoot;
    }
    FollowDecision::Follow(ActiveWorktree {
        path: hit.path.clone(),
        branch: hit.branch.clone(),
    })
}

/// The first decisive candidate of one tool call, in input order.
pub fn decide_all(
    candidates: &[PathBuf],
    own_root: &Path,
    worktrees: &[WorktreeInfo],
) -> FollowDecision {
    candidates
        .iter()
        .map(|candidate| decide(candidate, own_root, worktrees))
        .find(|decision| *decision != FollowDecision::Ignore)
        .unwrap_or(FollowDecision::Ignore)
}

/// How the header names a followed worktree: relative to the directory
/// holding the main checkout when it lives there (siblings and nested
/// `.claude/worktrees`), else `~`-relative, else the full path.
pub fn short_worktree_label(
    path: &Path,
    worktrees: &[WorktreeInfo],
    home: Option<&Path>,
) -> String {
    let repo_parent = worktrees.first().and_then(|main| main.path.parent());
    if let Some(rel) = repo_parent.and_then(|parent| path.strip_prefix(parent).ok()) {
        if !rel.as_os_str().is_empty() {
            return rel.display().to_string();
        }
    }
    if let Some(rel) = home.and_then(|home| path.strip_prefix(home).ok()) {
        return format!("~/{}", rel.display());
    }
    path.display().to_string()
}

/// Collects streamed tool-call inputs until they are complete.
///
/// The main agent's `ItemStarted` carries an empty input (`{}`); the real
/// input streams as `ItemInputDelta` fragments and is complete once the
/// tool's result (`ItemCompleted`) arrives. Subagent tool calls arrive
/// whole in `ItemStarted`.
#[derive(Debug, Default)]
pub struct ToolInputTracker {
    pending: HashMap<String, PendingCall>,
}

#[derive(Debug)]
struct PendingCall {
    name: String,
    json: String,
}

impl ToolInputTracker {
    /// Feeds one harness event; returns candidate paths of a tool call whose
    /// input became complete with it.
    pub fn observe(&mut self, event: &HarnessEvent, home: Option<&Path>) -> Vec<PathBuf> {
        match event {
            HarnessEvent::SubagentEvent { event, .. } => self.observe(event, home),
            HarnessEvent::ItemStarted {
                id,
                kind: ItemKind::ToolCall { name, input },
            } => {
                if !is_watched_tool(name) {
                    return Vec::new();
                }
                if input.as_object().is_some_and(|object| !object.is_empty()) {
                    return tool_call_candidates(name, input, home);
                }
                self.pending.insert(
                    id.clone(),
                    PendingCall {
                        name: name.clone(),
                        json: String::new(),
                    },
                );
                Vec::new()
            }
            HarnessEvent::ItemInputDelta { id, partial_json } => {
                if let Some(call) = self.pending.get_mut(id) {
                    call.json.push_str(partial_json);
                }
                Vec::new()
            }
            HarnessEvent::ItemCompleted { id, .. } => {
                let Some(call) = self.pending.remove(id) else {
                    return Vec::new();
                };
                if call.json.is_empty() {
                    return Vec::new();
                }
                match serde_json::from_str::<Value>(&call.json) {
                    Ok(input) => tool_call_candidates(&call.name, &input, home),
                    Err(error) => {
                        eprintln!(
                            "[worktree-follow] {} call {id}: streamed input is not JSON ({error}); skipped",
                            call.name
                        );
                        Vec::new()
                    }
                }
            }
            HarnessEvent::TurnCompleted { .. } | HarnessEvent::ProcessExited { .. } => {
                self.pending.clear();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

/// A chat tab's worktree-following state.
#[derive(Debug)]
pub struct WorktreeFollow {
    /// Where the agent was last seen working, when not the tab's own tree.
    pub active: Option<ActiveWorktree>,
    /// How the header names `active`.
    pub label: Option<String>,
    /// The user's choice: panels follow `active` (true) or stay on the
    /// workspace root (false). Detection keeps updating `active` either way.
    pub follow: bool,
    /// One-line note for the Git panel after the followed worktree vanished;
    /// cleared by the next detection.
    pub note: Option<String>,
    /// Every worktree this tab has followed, so a late git status snapshot
    /// from one never re-roots the tab.
    followed: Vec<PathBuf>,
    pub tracker: ToolInputTracker,
}

impl Default for WorktreeFollow {
    fn default() -> Self {
        Self {
            active: None,
            label: None,
            follow: true,
            note: None,
            followed: Vec::new(),
            tracker: ToolInputTracker::default(),
        }
    }
}

impl WorktreeFollow {
    /// The directory the tab's Git and Files panels read, when not the
    /// tab's own root.
    pub fn panel_root(&self) -> Option<&Path> {
        if self.follow {
            self.active.as_ref().map(|active| active.path.as_path())
        } else {
            None
        }
    }

    /// Applies one decision; true when the panel root changed.
    pub fn apply(&mut self, decision: FollowDecision, label: Option<String>) -> bool {
        let before = self.panel_root().map(Path::to_path_buf);
        match decision {
            FollowDecision::Ignore => return false,
            FollowDecision::Follow(active) => {
                if !self.followed.contains(&active.path) {
                    self.followed.push(active.path.clone());
                }
                self.active = Some(active);
                self.label = label;
            }
            FollowDecision::ReturnToRoot => {
                self.active = None;
                self.label = None;
            }
        }
        self.note = None;
        before.as_deref() != self.panel_root()
    }

    /// Flips between following the worktree and the workspace root; true
    /// when the panel root changed.
    pub fn toggle(&mut self) -> bool {
        self.follow = !self.follow;
        self.active.is_some()
    }

    /// Clears a followed worktree that no longer exists and leaves a note;
    /// true when it did (the panel root may then have changed).
    pub fn check_removed(&mut self, exists: impl Fn(&Path) -> bool) -> bool {
        let Some(active) = &self.active else {
            return false;
        };
        if exists(&active.path) {
            return false;
        }
        let name = active
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| active.path.display().to_string());
        self.note = Some(format!(
            "worktree {name} is gone; back to the workspace root"
        ));
        self.active = None;
        self.label = None;
        true
    }

    /// Worktrees this tab has followed at some point.
    pub fn followed_paths(&self) -> impl Iterator<Item = &Path> {
        self.followed.iter().map(PathBuf::as_path)
    }
}

/// How long a worktree listing is trusted.
pub const WORKTREE_LIST_TTL: Duration = Duration::from_secs(30);
/// A candidate that matches no known worktree re-lists at most this often,
/// so an agent reading files outside the repo does not spawn git per call.
pub const WORKTREE_MISS_REFRESH: Duration = Duration::from_secs(3);

/// Worktree listings per repository root, with the candidates waiting on an
/// in-flight listing.
#[derive(Debug, Default)]
pub struct WorktreeListCache {
    entries: HashMap<PathBuf, CacheEntry>,
}

#[derive(Debug, Default)]
struct CacheEntry {
    worktrees: Vec<WorktreeInfo>,
    fetched_at: Option<Instant>,
    in_flight: bool,
    waiting: Vec<(usize, Vec<PathBuf>)>,
}

/// What to do with a tab's candidates.
#[derive(Debug, PartialEq, Eq)]
pub enum CacheLookup {
    /// Decide now against this listing.
    Ready(Vec<WorktreeInfo>),
    /// Start listing the repository; the candidates wait for it.
    Fetch,
    /// A listing is already running; the candidates wait for it.
    Queued,
}

impl WorktreeListCache {
    pub fn lookup(
        &mut self,
        repo: &Path,
        tab_id: usize,
        candidates: Vec<PathBuf>,
        now: Instant,
    ) -> CacheLookup {
        let entry = self.entries.entry(repo.to_path_buf()).or_default();
        if entry.in_flight {
            entry.waiting.push((tab_id, candidates));
            return CacheLookup::Queued;
        }
        let age = entry.fetched_at.map(|at| now.saturating_duration_since(at));
        let stale = age.is_none_or(|age| age >= WORKTREE_LIST_TTL);
        let miss = candidates
            .iter()
            .any(|candidate| containing_worktree(candidate, &entry.worktrees).is_none());
        let miss_refresh = miss && age.is_none_or(|age| age >= WORKTREE_MISS_REFRESH);
        if stale || miss_refresh {
            entry.in_flight = true;
            entry.waiting.push((tab_id, candidates));
            return CacheLookup::Fetch;
        }
        CacheLookup::Ready(entry.worktrees.clone())
    }

    /// Records a finished listing (a failed one keeps the previous list but
    /// still counts as fetched, so a broken repo is not re-listed per call)
    /// and hands back the candidates that waited for it.
    pub fn complete(
        &mut self,
        repo: &Path,
        result: Result<Vec<WorktreeInfo>, String>,
        now: Instant,
    ) -> (Vec<WorktreeInfo>, Vec<(usize, Vec<PathBuf>)>) {
        let entry = self.entries.entry(repo.to_path_buf()).or_default();
        if let Ok(worktrees) = result {
            entry.worktrees = worktrees;
        }
        entry.fetched_at = Some(now);
        entry.in_flight = false;
        (entry.worktrees.clone(), std::mem::take(&mut entry.waiting))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PORCELAIN: &str = "\
worktree /repos/app
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repos/app/.claude/worktrees/fix-login
HEAD 2222222222222222222222222222222222222222
branch refs/heads/claude/fix-login

worktree /repos/app-worktrees/feature
HEAD 3333333333333333333333333333333333333333
detached

";

    fn worktrees() -> Vec<WorktreeInfo> {
        parse_worktree_porcelain(PORCELAIN)
    }

    fn follow(path: &str, branch: Option<&str>) -> FollowDecision {
        FollowDecision::Follow(ActiveWorktree {
            path: PathBuf::from(path),
            branch: branch.map(str::to_string),
        })
    }

    #[test]
    fn parses_porcelain_branches_detached_and_bare() {
        let parsed = worktrees();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].branch.as_deref(), Some("main"));
        assert_eq!(
            parsed[1].path,
            PathBuf::from("/repos/app/.claude/worktrees/fix-login")
        );
        assert_eq!(parsed[1].branch.as_deref(), Some("claude/fix-login"));
        assert_eq!(parsed[2].branch, None);
        let bare = parse_worktree_porcelain(
            "worktree /srv/app.git\nbare\n\nworktree /srv/wt\nbranch refs/heads/x\n",
        );
        assert_eq!(
            bare,
            vec![WorktreeInfo {
                path: PathBuf::from("/srv/wt"),
                branch: Some("x".to_string())
            }]
        );
    }

    #[test]
    fn path_inside_other_worktree_follows_it() {
        let decision = decide(
            Path::new("/repos/app-worktrees/feature/src/lib.rs"),
            Path::new("/repos/app"),
            &worktrees(),
        );
        assert_eq!(decision, follow("/repos/app-worktrees/feature", None));
    }

    #[test]
    fn nested_claude_worktree_wins_over_the_checkout_containing_it() {
        let decision = decide(
            Path::new("/repos/app/.claude/worktrees/fix-login/src/main.rs"),
            Path::new("/repos/app"),
            &worktrees(),
        );
        assert_eq!(
            decision,
            follow(
                "/repos/app/.claude/worktrees/fix-login",
                Some("claude/fix-login")
            )
        );
    }

    #[test]
    fn path_inside_own_worktree_returns_to_root() {
        let decision = decide(
            Path::new("/repos/app/src/main.rs"),
            Path::new("/repos/app"),
            &worktrees(),
        );
        assert_eq!(decision, FollowDecision::ReturnToRoot);
        // A tab rooted in a linked worktree: the main checkout is "other".
        let decision = decide(
            Path::new("/repos/app/src/main.rs"),
            Path::new("/repos/app-worktrees/feature"),
            &worktrees(),
        );
        assert_eq!(decision, follow("/repos/app", Some("main")));
    }

    #[test]
    fn path_outside_the_repo_is_ignored() {
        let decision = decide(
            Path::new("/Users/me/.claude/settings.json"),
            Path::new("/repos/app"),
            &worktrees(),
        );
        assert_eq!(decision, FollowDecision::Ignore);
        // Prefix of a name is not containment.
        let decision = decide(
            Path::new("/repos/app-other/x"),
            Path::new("/repos/app"),
            &worktrees(),
        );
        assert_eq!(decision, FollowDecision::Ignore);
    }

    #[test]
    fn first_decisive_candidate_wins() {
        let candidates = vec![
            PathBuf::from("/elsewhere/a"),
            PathBuf::from("/repos/app-worktrees/feature/b"),
            PathBuf::from("/repos/app/c"),
        ];
        assert_eq!(
            decide_all(&candidates, Path::new("/repos/app"), &worktrees()),
            follow("/repos/app-worktrees/feature", None)
        );
        assert_eq!(
            decide_all(&[], Path::new("/repos/app"), &worktrees()),
            FollowDecision::Ignore
        );
    }

    #[test]
    fn path_tool_inputs_yield_absolute_candidates() {
        let home = Path::new("/Users/me");
        assert_eq!(
            tool_call_candidates(
                "Edit",
                &json!({"file_path": "/repos/app/./src/../x.rs"}),
                Some(home)
            ),
            vec![PathBuf::from("/repos/app/x.rs")]
        );
        assert_eq!(
            tool_call_candidates(
                "Grep",
                &json!({"pattern": "fn", "path": "~/GitRepo/x"}),
                Some(home)
            ),
            vec![PathBuf::from("/Users/me/GitRepo/x")]
        );
        assert_eq!(
            tool_call_candidates(
                "NotebookEdit",
                &json!({"notebook_path": "/n/a.ipynb"}),
                None
            ),
            vec![PathBuf::from("/n/a.ipynb")]
        );
        assert!(
            tool_call_candidates("Glob", &json!({"pattern": "**/*.rs", "path": "src"}), None)
                .is_empty()
        );
        assert!(tool_call_candidates("WebFetch", &json!({"url": "/x"}), None).is_empty());
    }

    #[test]
    fn bash_leading_cd_and_git_dash_c() {
        let home = Some(Path::new("/Users/me"));
        assert_eq!(
            bash_leading_dir("cd /repos/app-worktrees/feature && cargo test", home),
            Some(PathBuf::from("/repos/app-worktrees/feature"))
        );
        assert_eq!(
            bash_leading_dir("  cd \"/repos/my app\"; ls", home),
            Some(PathBuf::from("/repos/my app"))
        );
        assert_eq!(
            bash_leading_dir("cd ~/GitRepo/cree8-worktrees/x|cat", home),
            Some(PathBuf::from("/Users/me/GitRepo/cree8-worktrees/x"))
        );
        assert_eq!(
            bash_leading_dir(
                "git -C '/repos/app/.claude/worktrees/fix-login' status",
                home
            ),
            Some(PathBuf::from("/repos/app/.claude/worktrees/fix-login"))
        );
        assert_eq!(
            bash_leading_dir("cd /a/my\\ dir && make", home),
            Some(PathBuf::from("/a/my dir"))
        );
        // Not leading, relative, or not a cd / git -C.
        assert_eq!(bash_leading_dir("cargo test && cd /repos/x", home), None);
        assert_eq!(bash_leading_dir("cd src", home), None);
        assert_eq!(bash_leading_dir("git status", home), None);
        assert_eq!(bash_leading_dir("ls /repos/app", home), None);
        assert_eq!(bash_leading_dir("cd ~/x", None), None);
    }

    #[test]
    fn tracker_completes_streamed_main_agent_inputs() {
        let mut tracker = ToolInputTracker::default();
        let started = HarnessEvent::ItemStarted {
            id: "t1".to_string(),
            kind: ItemKind::ToolCall {
                name: "Bash".to_string(),
                input: json!({}),
            },
        };
        assert!(tracker.observe(&started, None).is_empty());
        for part in [
            r#"{"command": "cd /repos/"#,
            r#"app-worktrees/feature && ls"}"#,
        ] {
            let delta = HarnessEvent::ItemInputDelta {
                id: "t1".to_string(),
                partial_json: part.to_string(),
            };
            assert!(tracker.observe(&delta, None).is_empty());
        }
        let done = HarnessEvent::ItemCompleted {
            id: "t1".to_string(),
            output: String::new(),
            is_error: false,
        };
        assert_eq!(
            tracker.observe(&done, None),
            vec![PathBuf::from("/repos/app-worktrees/feature")]
        );
        // Completed once; a duplicate completion yields nothing.
        assert!(tracker.observe(&done, None).is_empty());
    }

    #[test]
    fn tracker_reads_whole_subagent_inputs_and_skips_unwatched_tools() {
        let mut tracker = ToolInputTracker::default();
        let sub = HarnessEvent::SubagentEvent {
            parent_tool_use_id: "agent-1".to_string(),
            event: Box::new(HarnessEvent::ItemStarted {
                id: "s1".to_string(),
                kind: ItemKind::ToolCall {
                    name: "Read".to_string(),
                    input: json!({"file_path": "/repos/app/.claude/worktrees/fix-login/a.rs"}),
                },
            }),
        };
        assert_eq!(
            tracker.observe(&sub, None),
            vec![PathBuf::from("/repos/app/.claude/worktrees/fix-login/a.rs")]
        );
        let other = HarnessEvent::ItemStarted {
            id: "w1".to_string(),
            kind: ItemKind::ToolCall {
                name: "WebSearch".to_string(),
                input: json!({}),
            },
        };
        assert!(tracker.observe(&other, None).is_empty());
        let delta = HarnessEvent::ItemInputDelta {
            id: "w1".to_string(),
            partial_json: "{\"query\":\"x\"}".to_string(),
        };
        assert!(tracker.observe(&delta, None).is_empty());
        assert!(tracker.pending.is_empty());
    }

    #[test]
    fn tracker_drops_pending_calls_when_the_turn_ends() {
        let mut tracker = ToolInputTracker::default();
        tracker.observe(
            &HarnessEvent::ItemStarted {
                id: "t1".to_string(),
                kind: ItemKind::ToolCall {
                    name: "Edit".to_string(),
                    input: json!({}),
                },
            },
            None,
        );
        assert_eq!(tracker.pending.len(), 1);
        tracker.observe(
            &HarnessEvent::TurnCompleted {
                status: crate::harness::TurnStatus::Interrupted,
                usage: Value::Null,
                cost_usd: None,
            },
            None,
        );
        assert!(tracker.pending.is_empty());
    }

    #[test]
    fn toggle_keeps_detection_but_moves_the_panels() {
        let mut state = WorktreeFollow::default();
        assert!(state.follow);
        assert_eq!(state.panel_root(), None);
        assert!(state.apply(follow("/w/a", Some("a")), Some("w/a".to_string())));
        assert_eq!(state.panel_root(), Some(Path::new("/w/a")));
        // The user picks the workspace root.
        assert!(state.toggle());
        assert_eq!(state.panel_root(), None);
        // Detection still updates the active tree, panels stay on the root.
        assert!(!state.apply(follow("/w/b", Some("b")), Some("w/b".to_string())));
        assert_eq!(
            state.active.as_ref().map(|a| a.path.as_path()),
            Some(Path::new("/w/b"))
        );
        assert_eq!(state.panel_root(), None);
        // Flipping back follows the latest tree.
        assert!(state.toggle());
        assert_eq!(state.panel_root(), Some(Path::new("/w/b")));
        // Returning to the own tree clears it; toggling then moves nothing.
        assert!(state.apply(FollowDecision::ReturnToRoot, None));
        assert_eq!(state.active, None);
        assert!(!state.toggle());
        assert!(!state.apply(FollowDecision::Ignore, None));
        let followed: Vec<&Path> = state.followed_paths().collect();
        assert_eq!(followed, vec![Path::new("/w/a"), Path::new("/w/b")]);
    }

    #[test]
    fn removed_worktree_falls_back_with_a_note_until_next_detection() {
        let mut state = WorktreeFollow::default();
        state.apply(
            follow("/w/feature-x", Some("feature-x")),
            Some("w/feature-x".to_string()),
        );
        assert!(!state.check_removed(|_| true));
        assert!(state.check_removed(|_| false));
        assert_eq!(state.active, None);
        assert_eq!(state.panel_root(), None);
        assert_eq!(
            state.note.as_deref(),
            Some("worktree feature-x is gone; back to the workspace root")
        );
        // Nothing followed: nothing to clear, the note stays.
        assert!(!state.check_removed(|_| false));
        assert!(state.note.is_some());
        // An ignored candidate is not a detection.
        state.apply(FollowDecision::Ignore, None);
        assert!(state.note.is_some());
        state.apply(FollowDecision::ReturnToRoot, None);
        assert_eq!(state.note, None);
    }

    #[test]
    fn labels_are_relative_to_repo_parent_then_home() {
        let list = worktrees();
        assert_eq!(
            short_worktree_label(
                Path::new("/repos/app/.claude/worktrees/fix-login"),
                &list,
                None
            ),
            "app/.claude/worktrees/fix-login"
        );
        assert_eq!(
            short_worktree_label(Path::new("/repos/app-worktrees/feature"), &list, None),
            "app-worktrees/feature"
        );
        assert_eq!(
            short_worktree_label(
                Path::new("/Users/me/wt/x"),
                &list,
                Some(Path::new("/Users/me"))
            ),
            "~/wt/x"
        );
        assert_eq!(
            short_worktree_label(Path::new("/opt/wt"), &list, None),
            "/opt/wt"
        );
    }

    #[test]
    fn cache_fetches_once_queues_and_refreshes_on_miss() {
        let mut cache = WorktreeListCache::default();
        let repo = Path::new("/repos/app");
        let t0 = Instant::now();
        let inside = vec![PathBuf::from("/repos/app-worktrees/feature/x")];
        assert_eq!(
            cache.lookup(repo, 1, inside.clone(), t0),
            CacheLookup::Fetch
        );
        assert_eq!(
            cache.lookup(repo, 2, inside.clone(), t0),
            CacheLookup::Queued
        );
        let (list, waiting) = cache.complete(repo, Ok(worktrees()), t0);
        assert_eq!(list.len(), 3);
        assert_eq!(
            waiting.iter().map(|(tab, _)| *tab).collect::<Vec<_>>(),
            vec![1, 2]
        );
        // Fresh and matching: decide from the cache.
        assert_eq!(
            cache.lookup(repo, 1, inside.clone(), t0 + Duration::from_secs(1)),
            CacheLookup::Ready(worktrees())
        );
        // A miss soon after a listing is answered from the cache...
        let outside = vec![PathBuf::from("/repos/new-wt/x")];
        assert_eq!(
            cache.lookup(repo, 1, outside.clone(), t0 + Duration::from_secs(1)),
            CacheLookup::Ready(worktrees())
        );
        // ...and re-lists once the miss window has passed.
        assert_eq!(
            cache.lookup(repo, 1, outside, t0 + WORKTREE_MISS_REFRESH),
            CacheLookup::Fetch
        );
        // A failed listing keeps the previous list.
        let (list, _) = cache.complete(repo, Err("boom".to_string()), t0 + WORKTREE_MISS_REFRESH);
        assert_eq!(list.len(), 3);
        // Past the TTL even a hit re-lists.
        assert_eq!(
            cache.lookup(
                repo,
                1,
                inside,
                t0 + WORKTREE_MISS_REFRESH + WORKTREE_LIST_TTL
            ),
            CacheLookup::Fetch
        );
    }

    /// `git` with the repo-discovery variables scrubbed, so a test run from
    /// a git hook never touches the hook's repository.
    fn git(dir: &Path, args: &[&str]) {
        let status = crate::agentd::git::git_command()
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    #[test]
    fn lists_real_worktrees_canonically() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("app");
        std::fs::create_dir(&repo).expect("mkdir");
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
        let linked = temp.path().join("app-wt");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                linked.to_str().expect("utf8"),
            ],
        );
        let list = list_worktrees(&repo).expect("list");
        let canonical_linked = std::fs::canonicalize(&linked).expect("canonical");
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].path, canonical_linked);
        assert_eq!(list[1].branch.as_deref(), Some("feature"));
        // A not-yet-written file in the linked tree resolves inside it.
        let candidate = resolve_for_match(&linked.join("src/new.rs"));
        assert_eq!(
            decide(&candidate, &resolve_for_match(&repo), &list),
            FollowDecision::Follow(ActiveWorktree {
                path: canonical_linked,
                branch: Some("feature".to_string())
            })
        );
    }
}
