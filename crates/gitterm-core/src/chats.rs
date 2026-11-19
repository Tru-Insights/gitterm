//! Neutral chat index, scope, search and preview models.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
    /// The newest real user message in the transcript tail, when it says
    /// something different from `title`. A long-lived session keeps its
    /// opening title for recognition; this line says what the session has
    /// become since (TRU-158). Claude backend only.
    pub latest: Option<String>,
    /// When the chat was last worked in: its newest real message's
    /// timestamp (Claude), else the file's modification time.
    pub mtime: SystemTime,
    pub size: u64,
    /// The recorded cwd no longer exists on disk.
    pub dead_cwd: bool,
    /// What the chat touched and started, from its facts file (TRU-166).
    /// Merged in after the index build (`chat_facts::attach_facts`); None
    /// for chats without one and for remote machines' entries.
    pub facts: Option<crate::chat_facts::ChatFacts>,
    /// The root chat this one folds under, when a facts file or a
    /// delegation names it as a child (`chat_facts::resolve_tree`). None
    /// for roots.
    pub root_id: Option<String>,
}

/// Which field of a chat a search matched (TRU-166). The row names the
/// field when it is not one already on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryMatch {
    Title,
    Folder,
    Branch,
    IssueKey(String),
    Repo(String),
    Child(String),
}

impl QueryMatch {
    /// "matched repo blobfs"; None for fields the row already shows.
    pub fn label(&self) -> Option<String> {
        match self {
            QueryMatch::Title | QueryMatch::Folder | QueryMatch::Branch => None,
            QueryMatch::IssueKey(key) => Some(format!("matched key {key}")),
            QueryMatch::Repo(name) => Some(format!("matched repo {name}")),
            QueryMatch::Child(title) => Some(format!("matched child · {title}")),
        }
    }
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

/// One conversation on one machine (`remote_id` None = this Mac). A
/// conversation moved to another machine keeps its session id, so the id
/// alone can name two transcripts; every lookup goes through both.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChatRef {
    pub remote_id: Option<String>,
    pub id: String,
}

impl ChatRef {
    pub fn new(remote_id: Option<&str>, id: &str) -> Self {
        Self {
            remote_id: remote_id.map(str::to_string),
            id: id.to_string(),
        }
    }

    pub fn local(id: &str) -> Self {
        Self::new(None, id)
    }
}

/// The three scope rings of the Chats panel. With only the local machine
/// indexed (slice 1), Machine and Everywhere show the same set; they
/// diverge when remote machines land (slice 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChatScope {
    Workspace,
    /// The default since TRU-192: the Find mode looks across this Mac;
    /// Work lists the workspace's own chats.
    #[default]
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
        query_lower.is_empty() || self.query_match(query_lower).is_some()
    }

    /// The first field `query_lower` matches: title, folder, branch, then
    /// the facts' issue keys, repos touched (folder names) and children's
    /// titles.
    pub fn query_match(&self, query_lower: &str) -> Option<QueryMatch> {
        let hit = |text: &str| text.to_lowercase().contains(query_lower);
        if hit(&self.title) {
            return Some(QueryMatch::Title);
        }
        if hit(&self.group_name()) {
            return Some(QueryMatch::Folder);
        }
        if self.branch.as_deref().is_some_and(hit) {
            return Some(QueryMatch::Branch);
        }
        let facts = self.facts.as_ref()?;
        if let Some(key) = facts.issue_keys.iter().find(|key| hit(key)) {
            return Some(QueryMatch::IssueKey(key.clone()));
        }
        if let Some(name) = facts.searchable_folder_names().find(|name| hit(name)) {
            return Some(QueryMatch::Repo(name));
        }
        facts
            .children
            .iter()
            .filter_map(|child| child.title.as_deref())
            .find(|title| hit(title))
            .map(|title| QueryMatch::Child(title.to_string()))
    }
}

/// One top-level row of the Chats panel: a root chat and the visible
/// chats folded under it (TRU-166).
#[derive(Debug, Clone, PartialEq)]
pub struct FoldedChat<'a> {
    pub root: &'a ChatIndexEntry,
    /// Visible children, in `visible` order.
    pub children: Vec<&'a ChatIndexEntry>,
    /// The root itself passed the filters; false when only a child did.
    pub root_visible: bool,
}

/// Folds `visible` (already scoped, filtered and ordered) under each
/// entry's root, looked up in `all` (the same machine's index). A group
/// takes the position of its first visible member, so a ranking order
/// survives. A child whose root is not indexed stays top-level.
pub fn fold_under_roots<'a>(
    visible: &[&'a ChatIndexEntry],
    all: &'a [ChatIndexEntry],
) -> Vec<FoldedChat<'a>> {
    let roots_needed: HashSet<&str> = visible
        .iter()
        .filter_map(|entry| entry.root_id.as_deref())
        .collect();
    let by_id: HashMap<&str, &ChatIndexEntry> = all
        .iter()
        .filter(|entry| roots_needed.contains(entry.id.as_str()))
        .map(|entry| (entry.id.as_str(), entry))
        .collect();
    let mut out: Vec<FoldedChat<'a>> = Vec::new();
    let mut position: HashMap<&str, usize> = HashMap::new();
    for entry in visible {
        let root = entry
            .root_id
            .as_deref()
            .and_then(|id| by_id.get(id).copied());
        let group_root = root.unwrap_or(entry);
        let index = *position.entry(group_root.id.as_str()).or_insert_with(|| {
            out.push(FoldedChat {
                root: group_root,
                children: Vec::new(),
                root_visible: false,
            });
            out.len() - 1
        });
        if root.is_some() {
            out[index].children.push(entry);
        } else {
            out[index].root_visible = true;
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

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
            latest: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 1,
            dead_cwd: false,
            facts: None,
            root_id: None,
        };
        assert!(entry.in_workspace(Path::new("/Users/me/GitRepo/repo")));
        assert!(!entry.in_workspace(Path::new("/Users/me/GitRepo/other")));
    }

    #[test]
    fn children_fold_under_their_root_at_its_first_position() {
        let chat = |id: &str, root: Option<&str>| ChatIndexEntry {
            id: id.into(),
            backend: ChatBackend::Claude,
            path: PathBuf::from(format!("/t/{id}.jsonl")),
            cwd: PathBuf::from("/r"),
            repo_root: None,
            is_worktree: false,
            branch: None,
            title: id.into(),
            latest: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 1,
            dead_cwd: false,
            facts: None,
            root_id: root.map(str::to_string),
        };
        let all = vec![
            chat("worker1", Some("coord")),
            chat("solo", None),
            chat("coord", None),
            chat("worker2", Some("coord")),
            chat("orphan", Some("not-indexed")),
        ];
        let visible: Vec<&ChatIndexEntry> = all.iter().collect();
        let folded = fold_under_roots(&visible, &all);
        let shape: Vec<(&str, Vec<&str>, bool)> = folded
            .iter()
            .map(|row| {
                (
                    row.root.id.as_str(),
                    row.children.iter().map(|c| c.id.as_str()).collect(),
                    row.root_visible,
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                ("coord", vec!["worker1", "worker2"], true),
                ("solo", vec![], true),
                ("orphan", vec![], true),
            ]
        );
        // A search that only a child passes still shows its root.
        let only_child = vec![&all[3]];
        let folded = fold_under_roots(&only_child, &all);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].root.id, "coord");
        assert!(!folded[0].root_visible);
    }

    #[test]
    fn query_matches_facts_and_says_which_field() {
        use crate::chat_facts::{ChatChild, ChatFacts, CheckoutTouch, ChildKind, RepoTouch};
        let mut entry = ChatIndexEntry {
            id: "c".into(),
            backend: ChatBackend::Claude,
            path: PathBuf::from("/t/c.jsonl"),
            cwd: PathBuf::from("/Users/me/GitRepo/acme"),
            repo_root: None,
            is_worktree: false,
            branch: Some("main".into()),
            title: "Cloud migration".into(),
            latest: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 1,
            dead_cwd: false,
            facts: None,
            root_id: None,
        };
        assert_eq!(entry.query_match("cloud"), Some(QueryMatch::Title));
        assert_eq!(entry.query_match("acme"), Some(QueryMatch::Folder));
        assert_eq!(entry.query_match("main"), Some(QueryMatch::Branch));
        assert!(!entry.matches_query("blobfs"));
        assert!(entry.matches_query(""));
        entry.facts = Some(ChatFacts {
            issue_keys: vec!["PRJ-101".into(), "PRJ-102".into()],
            repos_touched: vec![RepoTouch {
                dir: PathBuf::from("/Users/me/GitRepo/acme/infra"),
                edits: 190,
            }],
            checkouts: vec![CheckoutTouch {
                repo: PathBuf::from("/Users/me/GitRepo/acme/blobfs-gateway"),
                dir: PathBuf::from("/Users/me/GitRepo/acme/.worktrees/blobfs-gateway-prj-102"),
                branch: Some("prj-102-cloud-backend".into()),
                edits: 0,
                last: String::new(),
            }],
            children: vec![ChatChild {
                session_id: "toolu_1".into(),
                kind: ChildKind::Subagent,
                delegation_id: None,
                task_id: None,
                title: Some("Fable review of identity design".into()),
            }],
            prs: Vec::new(),
            live_created_prs: Vec::new(),
            repo_work: Vec::new(),
            documents: Vec::new(),
            backfilled: false,
            documents_backfilled: false,
        });
        assert_eq!(
            entry.query_match("prj-102"),
            Some(QueryMatch::IssueKey("PRJ-102".into()))
        );
        assert_eq!(
            entry.query_match("infra"),
            Some(QueryMatch::Repo("infra".into()))
        );
        let blobfs = entry.query_match("blobfs").unwrap();
        assert_eq!(blobfs, QueryMatch::Repo("blobfs-gateway-prj-102".into()));
        assert_eq!(
            blobfs.label().as_deref(),
            Some("matched repo blobfs-gateway-prj-102")
        );
        assert_eq!(
            entry.query_match("identity"),
            Some(QueryMatch::Child("Fable review of identity design".into()))
        );
        assert_eq!(QueryMatch::Title.label(), None);
    }
}
