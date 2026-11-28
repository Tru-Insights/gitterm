//! The complete workspaces.json wire format shared by hosts.
use crate::remote::config::{RemoteAgentConfig, WorkspaceLocationConfig};
use crate::settings::WorkspaceColor;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
// Workspace persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename = "WorkspacesFile")]
pub struct WorkspacesFile<C = WorkspaceColor> {
    /// Fields written by a newer desktop survive full-document Live saves.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    pub workspaces: Vec<WorkspaceConfig<C>>,
    pub active_workspace: usize,
    #[serde(default = "default_remote_sessions")]
    pub remote_sessions: Vec<RemoteSessionConfig>,
    #[serde(
        default = "default_remote_agents",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub remote_agents: Vec<RemoteAgentConfig>,
    /// Machine group keys ("local", "agent:<id>", ...) whose `Repos` zone
    /// on the bottom bar is collapsed (TRU-175). Shared across instances
    /// and restarts; empty (every zone expanded) by default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collapsed_repo_groups: Vec<String>,
    /// The repos workspaces list (TRU-180): a checkout's run command and
    /// env, shared by every workspace listing it. Files written before
    /// TRU-180 have none; `normalize_repos` derives them from each
    /// workspace's own fields.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoConfig>,
}

impl<C> Default for WorkspacesFile<C> {
    fn default() -> Self {
        Self {
            extra: Default::default(),
            workspaces: Vec::new(),
            active_workspace: 0,
            remote_sessions: Vec::new(),
            remote_agents: Vec::new(),
            collapsed_repo_groups: Vec::new(),
            repos: Vec::new(),
        }
    }
}

/// A repo as `workspaces.json` stores it (TRU-180). Identified by `dir` on
/// its machine (`location`; `None` is this Mac), see `RepoKey`. It is
/// open while an open workspace lists it; this entry persists while any
/// workspace does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    pub dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<WorkspaceLocationConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_command: Option<String>,
    /// Environment for terminals and tabs running in this repo; a
    /// workspace's `env_overrides` apply on top.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// Read only: the build that kept bottom terminals on repos wrote
    /// them here. `WorkspacesFile::normalize_repos` hands them to the
    /// workspaces listing the repo; terminals are each workspace's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bottom_terminals: Vec<BottomTerminalConfig>,
}

impl RepoConfig {
    pub fn key(&self) -> RepoKey {
        RepoKey::of_config(&self.dir, self.location.as_ref())
    }
}

/// Which checkout a repo is (TRU-180): its canonical dir on this Mac, or
/// its root on a remote machine. Two workspaces whose repos have the same
/// key share one repo.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RepoKey {
    Local(PathBuf),
    RemoteAgent {
        remote_id: String,
        root: String,
    },
    /// A tmux-session workspace from before remote agents. Runtime only:
    /// its dir is a local stand-in, so it is never stored as a repo.
    LegacyRemote {
        session_name: String,
        root: String,
    },
}

impl RepoKey {
    /// A local checkout, by canonical path. A folder that cannot be
    /// resolved (gone, unreadable) keys by the path as given.
    pub fn local(dir: &Path) -> Self {
        RepoKey::Local(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
    }

    pub fn of_config(dir: &str, location: Option<&WorkspaceLocationConfig>) -> Self {
        match location {
            Some(WorkspaceLocationConfig::RemoteAgent { remote_id, .. }) => RepoKey::RemoteAgent {
                remote_id: remote_id.clone(),
                root: dir.to_string(),
            },
            Some(WorkspaceLocationConfig::Local { .. }) | None => RepoKey::local(Path::new(dir)),
        }
    }

    /// A checkout on this Mac.
    pub fn is_local(&self) -> bool {
        matches!(self, RepoKey::Local(_))
    }
}

/// `env` with `overrides` on top: what a tab, terminal or console in a
/// repo of a workspace runs with (TRU-180).
pub fn layered_env(
    env: &HashMap<String, String>,
    overrides: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut layered = env.clone();
    layered.extend(overrides.iter().map(|(k, v)| (k.clone(), v.clone())));
    layered
}

/// The entries of `env` that `base` lacks or holds differently: what a
/// workspace joining a shared repo keeps as its overrides.
pub fn env_difference(
    env: &HashMap<String, String>,
    base: &HashMap<String, String>,
) -> HashMap<String, String> {
    env.iter()
        .filter(|(key, value)| base.get(*key) != Some(*value))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSessionConfig {
    pub label: String,
    pub host_name: String,
    pub ssh_target: String,
    pub identity_file: String,
    #[serde(default = "default_remote_session_tmux_path")]
    pub tmux_path: String,
    pub session_name: String,
    pub remote_dir: String,
    #[serde(default = "default_remote_session_shell")]
    pub shell_command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename = "WorkspaceConfig")]
pub struct WorkspaceConfig<C = WorkspaceColor> {
    /// Fields written by a newer desktop survive full-document Live saves.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    /// Collapsed Issues groups (TRU-243), independently saved for each workspace.
    #[serde(default = "crate::issues::IssueGroup::default_collapsed")]
    pub collapsed_issue_groups: Vec<crate::issues::IssueGroup>,
    pub name: String,
    pub abbrev: String,
    pub dir: String,
    pub color: C,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<WorkspaceLocationConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_session: Option<RemoteSessionConfig>,
    /// Active tab index for this workspace. Older config files omit this; restore
    /// code falls back to the workspace-root tab when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tab: Option<usize>,
    pub tabs: Vec<WorkspaceTabConfig>,
    #[serde(default)]
    pub run_command: Option<String>,
    #[serde(default)]
    pub bottom_terminals: Vec<BottomTerminalConfig>,
    /// Environment variables to inject into all terminal sessions in this workspace.
    /// Edit workspaces.json to add any vars without recompiling, e.g.:
    /// "env": { "LINEAR_WORKSPACE": "truinsights", "LINEAR_TEAM": "TRU", "GH_TOKEN": "..." }
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// Iced keeps a legacy remote's home env in its runtime repo, separate
    /// from the effective compatibility copy written to `env` on each save.
    #[serde(skip)]
    pub legacy_env_base: Option<HashMap<String, String>>,
    /// Whether this workspace is currently open. Closed workspaces are kept in
    /// workspaces.json so their env vars and settings are preserved for reopening.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Root chat session ids this workspace owns (TRU-169). A workspace
    /// that owns chats is a piece of work: its overview reads these chats
    /// and their children wherever they ran, and resuming one opens it
    /// here. Empty keeps the folder-scoped behaviour. A chat belongs to
    /// at most one workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chats: Vec<String>,
    /// Set when × parked this workspace (TRU-171): the note to come back
    /// to and when. A closed entry without it (closed before parking
    /// existed) lists as parked with an empty note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked: Option<ParkedInfo>,
    /// When the work was marked done (RFC 3339). A done workspace leaves
    /// the Parked list but stays in this file; its chats stay findable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_at: Option<String>,
    /// Agents started in this workspace get no change guidelines
    /// (TRU-174). See `gitterm::guidelines`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub guidelines_off: bool,
    /// Claude sessions started in this workspace get Claude in Chrome
    /// (`--chrome`); off (`--no-chrome`) by default, so agents use GitTerm's
    /// own browser tools (TRU-181). Read when a Claude process starts; a
    /// running one keeps what it started with.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub claude_in_chrome: bool,
    /// The listed repo team-facing decisions go to (TRU-191), by dir; the
    /// work's agents are told it beside the work's docs folder. None by
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decisions_repo: Option<String>,
    /// The repos this workspace lists, by dir on its machine, home first
    /// (TRU-180). Their settings live in `WorkspacesFile::repos`. Files
    /// written before TRU-180 have none: `normalize_repos` makes `[dir]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    /// The home repo's dir, where new chats start; `dir` until set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// Env applied on top of a repo's env for this workspace's tabs and
    /// terminals (TRU-180). `env` and `run_command` above are still
    /// written, from the home repo (env with these applied), so a build
    /// before TRU-180 reads them; `bottom_terminals` is the workspace's.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env_overrides: HashMap<String, String>,
    /// Repos added by hand ("+ add repo"); listed whatever the chats touch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos_added: Vec<String>,
    /// Repos removed by hand (×, or undoing an auto-add); never listed
    /// again by the chats touching them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos_removed: Vec<String>,
}

/// Why and when a workspace was parked (TRU-171).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedInfo {
    #[serde(default)]
    pub note: String,
    /// RFC 3339.
    pub parked_at: String,
}

impl<C> WorkspaceConfig<C> {
    /// On the Parked list: closed and not done.
    pub fn is_parked(&self) -> bool {
        crate::work::catalog::is_parked(self.active, self.done_at.as_deref())
    }

    /// The home repo's dir: `home`, else `dir` (files before TRU-180).
    pub fn home_dir(&self) -> &str {
        crate::work::catalog::home_dir(self.home.as_deref(), &self.dir)
    }

    /// A tmux-session workspace from before remote agents: its repo is
    /// never stored in the `repos` section (its dir is a local stand-in),
    /// so its own fields stay the source of its settings.
    pub fn is_legacy_remote(&self) -> bool {
        self.location.is_none() && self.remote_session.is_some()
    }

    /// The key of repo `dir` this workspace lists; `None` for a legacy
    /// remote workspace (see `is_legacy_remote`).
    pub fn repo_key(&self, dir: &str) -> Option<RepoKey> {
        (!self.is_legacy_remote()).then(|| RepoKey::of_config(dir, self.location.as_ref()))
    }

    /// The home repo's settings as this entry's own (pre-TRU-180) fields
    /// hold them. Its terminals stay the workspace's.
    pub fn own_home_repo(&self) -> RepoConfig {
        RepoConfig {
            extra: Default::default(),
            dir: self.home_dir().to_string(),
            location: self.location.clone(),
            run_command: self.run_command.clone(),
            env: self.env.clone(),
            bottom_terminals: Vec::new(),
        }
    }
}

/// The terminal branch of Iced's workspace_config projection. Both frontends
/// supply runtime identity, never a GPUI layout id, to the shared inventory.
impl WorkspaceTabConfig {
    pub fn from_terminal(
        dir: &Path,
        repo_dir: &Path,
        startup_command: Option<&str>,
        target: &crate::session::Target,
        chat_session_id: Option<String>,
        worker_shutdown_refused: bool,
    ) -> Self {
        Self {
            extra: Default::default(),
            dir: dir.to_string_lossy().to_string(),
            repo_dir: Some(repo_dir.to_string_lossy().to_string()),
            startup_command: startup_command.map(String::from),
            tab_kind: None,
            agent_config: None,
            chat_session_id,
            task_id: target.task_id.clone(),
            task_session_id: target.task_session_id.clone(),
            worker_shutdown_refused,
            session_uid: Some(target.session_uid.clone()),
        }
    }
}

/// Narrow desktop mutations: unmounted sessions and unrelated fields stay owned
/// by the document. Bottom shells have no persisted identity or main selection.
#[derive(Clone)]
pub enum DesktopSessionEdit {
    OpenTabs(Vec<WorkspaceTabConfig>),
    /// Current projections of mounted records, in their saved slots.
    RefreshTabs(Vec<(usize, WorkspaceTabConfig)>),
    CloseBottom(usize),
    OpenBottom {
        terminal: BottomTerminalConfig,
        initial_tabs: Vec<WorkspaceTabConfig>,
    },
    Select(Option<usize>),
}
impl WorkspacesFile {
    pub fn edit_desktop_sessions(
        &mut self,
        target: &WorkspaceTarget,
        edit: DesktopSessionEdit,
    ) -> Result<(), String> {
        let index = target.index(self)?;
        if !matches!(edit, DesktopSessionEdit::Select(_))
            && self
                .repos
                .iter()
                .any(|repo| !repo.bottom_terminals.is_empty())
        {
            // Iced migrates repo-owned terminals before building runtime inventory.
            self.normalize_repos();
        }
        let workspace = &mut self.workspaces[index];
        if !matches!(edit, DesktopSessionEdit::Select(_))
            && (workspace.is_legacy_remote()
                || matches!(
                    workspace.location,
                    Some(WorkspaceLocationConfig::RemoteAgent { .. })
                ))
        {
            return Err("Local terminal workspace changed location".into());
        }
        match edit {
            DesktopSessionEdit::RefreshTabs(tabs) => {
                for (index, mut tab) in tabs {
                    let saved = workspace
                        .tabs
                        .get_mut(index)
                        .ok_or("Mounted terminal is no longer in the saved inventory")?;
                    let mut extra = saved.extra.clone();
                    extra.extend(tab.extra);
                    tab.extra = extra;
                    *saved = tab;
                }
            }
            DesktopSessionEdit::CloseBottom(index) => {
                if index >= workspace.bottom_terminals.len() {
                    return Err("Bottom terminal is no longer in the saved inventory".into());
                }
                workspace.bottom_terminals.remove(index);
            }
            DesktopSessionEdit::OpenTabs(tabs) => {
                for tab in tabs {
                    let uid = tab
                        .session_uid
                        .as_deref()
                        .ok_or("New terminal has no session identity")?;
                    let at = workspace
                        .tabs
                        .iter()
                        .position(|saved| saved.session_uid.as_deref() == Some(uid));
                    // An adopted worker may already be in the inventory. Do not
                    // discard its saved metadata or append it a second time.
                    let at = at.unwrap_or_else(|| {
                        workspace.tabs.push(tab);
                        workspace.tabs.len() - 1
                    });
                    workspace.active_tab = Some(at);
                }
            }
            DesktopSessionEdit::OpenBottom {
                terminal,
                initial_tabs,
            } => {
                if workspace.tabs.is_empty() && !initial_tabs.is_empty() {
                    workspace.tabs = initial_tabs;
                    workspace.active_tab = Some(0);
                }
                workspace.bottom_terminals.push(terminal);
            }
            DesktopSessionEdit::Select(tab) => {
                if let Some(tab) = tab {
                    if tab >= workspace.tabs.len() {
                        return Err("Selected main tab is no longer open".into());
                    }
                    workspace.active_tab = Some(tab);
                }
                // V6 stores an ordinal among open workspaces, not the file
                // index (closed workspaces can be interleaved in older files).
                self.active_workspace =
                    self.workspaces[..index].iter().filter(|w| w.active).count();
            }
        }
        Ok(())
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceTabConfig {
    /// Fields written by a newer desktop survive full-document Live saves.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    pub dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_command: Option<String>,
    /// Tab kind discriminator. `None` or `Some("terminal")` means a terminal tab
    /// (the implicit default — older `workspaces.json` files predate this field).
    /// `Some("agent")` means a Claude Code / pi agent tab; see `agent_config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_kind: Option<String>,
    /// Backend config for `tab_kind: "agent"`. Required when `tab_kind == Some("agent")`;
    /// ignored otherwise. Schema: `{"backend": "pi"|"claude", ...backend-specific fields}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<crate::chat::types::AgentBackendConfig>,
    /// Harness conversation owned by this tab (claude session uuid) — set
    /// when the tab was spawned by the Chats panel resume flow or by a
    /// picker launch with a pre-assigned id. Keys the one-live-tab-per-
    /// conversation registry rule (TRU-78) and survives restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_session_id: Option<String>,
    /// Durable task owned by this view. Closing the tab does not remove the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Durable identity for this child view inside a task. Several tabs may
    /// share one task id; this id distinguishes their sessions across restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_session_id: Option<String>,
    /// Automatic shutdown was refused; keep this final worker tab usable
    /// and restore it without retrying the shutdown.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub worker_shutdown_refused: bool,
    /// Durable caller identity of this tab (uuid v4). Every tab carries one
    /// on its task MCP URL (`?caller=<uid>`) so the task server can tell
    /// which tab made a call. Older files predate it; a missing value is
    /// generated when the tab is restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_uid: Option<String>,
}

impl WorkspaceTabConfig {
    /// Project identity without changing the persisted DTO or reconstructing
    /// a tab/workspace and losing unrelated settings.
    pub fn session_identity(&self) -> crate::session::SavedIdentity {
        crate::session::SavedIdentity {
            session_uid: self.session_uid.clone(),
            task_id: self.task_id.clone(),
            task_session_id: self.task_session_id.clone(),
            chat_session_id: self.chat_session_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BottomTerminalConfig {
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    pub dir: String,
}

impl<C> WorkspacesFile<C> {
    /// Fills in the repo model (TRU-180) for entries written without it:
    /// each workspace lists its `dir` as home and only repo, and a home
    /// with no `repos` entry gets one from the workspace's own fields.
    /// Open workspaces go first, in file order, so the first open one
    /// sharing a dir wins its run command and env; a later one's
    /// differing env becomes its overrides. Entries already in `repos`
    /// are left as stored.
    ///
    /// Bottom terminals are each workspace's own `bottom_terminals`. A
    /// file from the build that kept them on repos (`repos[].bottom_terminals`)
    /// gives each workspace the terminals of every repo it lists, home
    /// first: a repo's terminals shared by several workspaces go to each.
    pub fn normalize_repos(&mut self) {
        let repo_terminals: Vec<(RepoKey, Vec<BottomTerminalConfig>)> = self
            .repos
            .iter_mut()
            .map(|repo| (repo.key(), std::mem::take(&mut repo.bottom_terminals)))
            .collect();
        let terminals_on_repos = repo_terminals.iter().any(|(_, t)| !t.is_empty());
        let mut created: Vec<RepoKey> = Vec::new();
        let order: Vec<usize> = (0..self.workspaces.len())
            .filter(|&i| self.workspaces[i].active)
            .chain((0..self.workspaces.len()).filter(|&i| !self.workspaces[i].active))
            .collect();
        for idx in order {
            let ws = &mut self.workspaces[idx];
            let home = ws.home_dir().to_string();
            ws.home = Some(home.clone());
            ws.repos.retain(|dir| *dir != home);
            ws.repos.insert(0, home.clone());
            let Some(key) = ws.repo_key(&home) else {
                continue;
            };
            match self.repos.iter_mut().find(|repo| repo.key() == key) {
                None => {
                    self.repos.push(ws.own_home_repo());
                    created.push(key);
                }
                Some(repo) if created.contains(&key) => {
                    for (k, v) in env_difference(&ws.env, &repo.env) {
                        ws.env_overrides.entry(k).or_insert(v);
                    }
                }
                Some(_) => {}
            }
        }
        if !terminals_on_repos {
            return;
        }
        for ws in &mut self.workspaces {
            let stored = |dir: &String| {
                let key = ws.repo_key(dir)?;
                repo_terminals.iter().find(|(k, _)| *k == key)
            };
            // A home the repo section never held (a legacy remote, an entry
            // an older build added) keeps the workspace's own terminals.
            if ws.repos.first().and_then(stored).is_none() {
                continue;
            }
            let terminals: Vec<BottomTerminalConfig> = ws
                .repos
                .iter()
                .filter_map(stored)
                .flat_map(|(_, terminals)| terminals.iter().cloned())
                .collect();
            ws.bottom_terminals = terminals;
        }
    }

    /// The stored repo with `key`.
    pub fn repo(&self, key: &RepoKey) -> Option<&RepoConfig> {
        self.repos.iter().find(|repo| repo.key() == *key)
    }

    /// The env a tab or terminal of workspace `name` running in `cwd`
    /// gets (TRU-180): the env of the deepest repo it lists containing
    /// `cwd`, else of its home repo (every tab got the workspace's env
    /// before repos), with the workspace's overrides on top. A legacy
    /// remote workspace, or a home with no stored repo, uses the
    /// workspace's own `env`. An unknown workspace gets none, as before.
    pub fn workspace_env(&self, name: &str, cwd: Option<&Path>) -> HashMap<String, String> {
        let Some(ws) = self.workspaces.iter().find(|ws| ws.name == name) else {
            return HashMap::new();
        };
        let home = ws.home_dir();
        let dir = cwd
            .filter(|_| !ws.is_legacy_remote())
            .and_then(|cwd| {
                let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
                ws.repos
                    .iter()
                    .filter(|dir| {
                        let dir = Path::new(dir.as_str());
                        let canonical =
                            std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
                        cwd.starts_with(&canonical) || cwd.starts_with(dir)
                    })
                    .max_by_key(|dir| Path::new(dir.as_str()).components().count())
            })
            .map(String::as_str)
            .unwrap_or(home);
        let stored = |dir: &str| ws.repo_key(dir).and_then(|key| self.repo(&key));
        let base = match stored(dir) {
            Some(repo) => &repo.env,
            // A listed repo with nothing stored yet has no env of its own.
            None if dir != home => return ws.env_overrides.clone(),
            None => &ws.env,
        };
        layered_env(base, &ws.env_overrides)
    }

    /// Entries a TRU-180 build wrote whose `env` copy no longer matches
    /// what the repo model gives them (the copy was edited by hand), with
    /// the keys that differ. That copy is only for older builds: tabs get
    /// the repo's env and `env_overrides`, so edits belong there.
    pub fn stale_env_copies(&self) -> Vec<(String, Vec<String>)> {
        self.workspaces
            .iter()
            .filter(|ws| ws.home.is_some())
            .filter_map(|ws| {
                let repo = self.repo(&ws.repo_key(ws.home_dir())?)?;
                let layered = layered_env(&repo.env, &ws.env_overrides);
                let mut keys: Vec<String> = ws
                    .env
                    .keys()
                    .chain(layered.keys())
                    .filter(|key| ws.env.get(*key) != layered.get(*key))
                    .cloned()
                    .collect();
                keys.sort();
                keys.dedup();
                (!keys.is_empty()).then(|| (ws.name.clone(), keys))
            })
            .collect()
    }
}

fn default_remote_session_shell() -> String {
    "/bin/zsh".to_string()
}
fn default_remote_session_tmux_path() -> String {
    "/opt/homebrew/bin/tmux".to_string()
}
pub fn default_remote_sessions() -> Vec<RemoteSessionConfig> {
    Vec::new()
}
pub fn default_remote_agents() -> Vec<RemoteAgentConfig> {
    Vec::new()
}

/// Iced's inline rename: whitespace is trimmed and an empty draft is ignored.
pub fn rename_workspace(name: &mut String, draft: &str) -> bool {
    let draft = draft.trim();
    if draft.is_empty() || name == draft {
        return false;
    }
    *name = draft.to_string();
    true
}

/// Iced's run command editor: empty means no command.
pub fn run_command(draft: &str) -> Option<String> {
    let draft = draft.trim();
    (!draft.is_empty()).then(|| draft.to_string())
}

pub fn check_remove_repo(home: &Path, dir: &Path) -> Result<(), String> {
    if RepoKey::local(home) == RepoKey::local(dir) {
        Err("the home repo stays listed".into())
    } else {
        Ok(())
    }
}

pub fn add_manually(dir: &Path, added: &mut Vec<PathBuf>, removed: &mut Vec<PathBuf>) {
    removed.retain(|gone| gone != dir);
    if !added.iter().any(|known| known == dir) {
        added.push(dir.to_path_buf());
    }
}

/// Records a manual remove of `dir`: it stays out even when its chats
/// touch it again.
pub fn remove_manually(dir: &Path, added: &mut Vec<PathBuf>, removed: &mut Vec<PathBuf>) {
    added.retain(|known| known != dir);
    if !removed.iter().any(|gone| gone == dir) {
        removed.push(dir.to_path_buf());
    }
}

/// Identity captured by a control, checked against the owner's current document.
/// Catalog indexes are sorted and exclude closed workspaces; never use them as
/// file indexes. Ambiguous or stale controls fail without changing the document.
#[derive(Clone, Debug)]
pub struct WorkspaceTarget {
    name: String,
    dir: String,
    chats: Vec<String>,
    location: Option<WorkspaceLocationConfig>,
    legacy_session: Option<String>,
}
impl WorkspaceTarget {
    pub fn of<C>(workspace: &WorkspaceConfig<C>) -> Self {
        Self {
            name: workspace.name.clone(),
            dir: workspace.dir.clone(),
            chats: workspace.chats.clone(),
            location: workspace.location.clone(),
            legacy_session: workspace
                .remote_session
                .as_ref()
                .map(|s| s.session_name.clone()),
        }
    }
    pub fn matches<C>(&self, workspace: &WorkspaceConfig<C>) -> bool {
        workspace.name == self.name
            && workspace.dir == self.dir
            && workspace.chats == self.chats
            && workspace.location == self.location
            && workspace
                .remote_session
                .as_ref()
                .map(|s| s.session_name.as_str())
                == self.legacy_session.as_deref()
    }
    pub fn index<C>(&self, file: &WorkspacesFile<C>) -> Result<usize, String> {
        let mut matches = file
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, w)| w.active && self.matches(w));
        let index = matches
            .next()
            .map(|(i, _)| i)
            .ok_or("Workspace is no longer open")?;
        if matches.next().is_some() {
            return Err("Workspace identity is ambiguous".into());
        }
        Ok(index)
    }
}

#[derive(Clone, Debug)]
pub enum RepoEdit {
    Rename(String),
    Add {
        dir: PathBuf,
        detected_command: Option<String>,
    },
    Remove(Vec<PathBuf>),
    RunCommand {
        dir: PathBuf,
        draft: String,
    },
}
impl<C> WorkspacesFile<C> {
    /// Apply to a private candidate on DesktopStore's serialized owner thread.
    pub fn edit_repo(&mut self, target: &WorkspaceTarget, edit: RepoEdit) -> Result<(), String> {
        let index = target.index(self)?;
        if let RepoEdit::Rename(draft) = edit {
            rename_workspace(&mut self.workspaces[index].name, &draft);
            return Ok(());
        }
        let ws = &self.workspaces[index];
        if ws.is_legacy_remote()
            || matches!(
                ws.location,
                Some(WorkspaceLocationConfig::RemoteAgent { .. })
            )
        {
            return Err("a remote workspace lists only its home".into());
        }
        self.normalize_repos();
        match edit {
            RepoEdit::Add {
                dir,
                detected_command,
            } => {
                let dir = dir.to_string_lossy().into_owned();
                let ws = &mut self.workspaces[index];
                let key = RepoKey::local(Path::new(&dir));
                if !ws.repos.iter().any(|d| RepoKey::local(Path::new(d)) == key) {
                    ws.repos.push(dir.clone());
                }
                ws.repos_removed
                    .retain(|d| RepoKey::local(Path::new(d)) != key);
                if !ws.repos_added.contains(&dir) {
                    ws.repos_added.push(dir.clone());
                }
                if self.repo(&key).is_none() {
                    self.repos.push(RepoConfig {
                        extra: Default::default(),
                        dir,
                        location: None,
                        run_command: detected_command,
                        env: HashMap::new(),
                        bottom_terminals: Vec::new(),
                    });
                }
            }
            RepoEdit::Remove(dirs) => {
                let ws = &mut self.workspaces[index];
                for dir in &dirs {
                    check_remove_repo(Path::new(ws.home_dir()), dir)?;
                }
                for dir in dirs {
                    let key = RepoKey::local(&dir);
                    ws.repos.retain(|d| RepoKey::local(Path::new(d)) != key);
                    ws.repos_added
                        .retain(|d| RepoKey::local(Path::new(d)) != key);
                    let dir = dir.to_string_lossy().into_owned();
                    if !ws.repos_removed.contains(&dir) {
                        ws.repos_removed.push(dir);
                    }
                }
                // Iced releases repo settings only after the last workspace unlists it.
                let retained: Vec<_> = self
                    .workspaces
                    .iter()
                    .flat_map(|w| w.repos.iter().filter_map(|d| w.repo_key(d)))
                    .collect();
                self.repos.retain(|r| retained.contains(&r.key()));
            }
            RepoEdit::RunCommand { dir, draft } => {
                let key = RepoKey::local(&dir);
                if !self.workspaces[index]
                    .repos
                    .iter()
                    .any(|d| RepoKey::local(Path::new(d)) == key)
                {
                    return Err("Repo is no longer listed".into());
                }
                if self.repo(&key).is_none() {
                    // Legacy lists can name an extra repo without a settings
                    // record. Iced's list_repo gives it an empty environment.
                    self.repos.push(RepoConfig {
                        extra: Default::default(),
                        dir: dir.to_string_lossy().into_owned(),
                        location: None,
                        run_command: None,
                        env: HashMap::new(),
                        bottom_terminals: Vec::new(),
                    });
                }
                let repo = self
                    .repos
                    .iter_mut()
                    .find(|r| r.key() == key)
                    .expect("listed repo settings");
                repo.run_command = run_command(&draft);
                // Iced writes the shared command into the legacy home copy too.
                let command = repo.run_command.clone();
                for ws in &mut self.workspaces {
                    if ws.active && ws.repo_key(ws.home_dir()).as_ref() == Some(&key) {
                        ws.run_command = command.clone();
                    }
                }
            }
            RepoEdit::Rename(_) => unreachable!(),
        }
        // Iced's repos_section writes open registry entries first, then settings
        // kept only by closed workspaces. A newly attached repo precedes those.
        let open: Vec<_> = self
            .workspaces
            .iter()
            .filter(|ws| ws.active)
            .flat_map(|ws| ws.repos.iter().filter_map(|dir| ws.repo_key(dir)))
            .collect();
        self.repos.sort_by_key(|repo| !open.contains(&repo.key()));
        Ok(())
    }

    /// Chat facts join repos automatically; unlike an explicit Add these do
    /// not become manual pins and never override a manual removal.
    pub fn join_discovered_repos(
        &mut self,
        target: &WorkspaceTarget,
        dirs: Vec<PathBuf>,
    ) -> Result<(), String> {
        let index = target.index(self)?;
        let manual = self.workspaces[index].repos_added.clone();
        for dir in dirs {
            let key = RepoKey::local(&dir);
            let workspace = &self.workspaces[index];
            if workspace
                .repos_removed
                .iter()
                .chain(&workspace.repos)
                .any(|known| RepoKey::local(Path::new(known)) == key)
            {
                continue;
            }
            let detected_command = detect_run_command(&dir);
            self.edit_repo(
                target,
                RepoEdit::Add {
                    dir,
                    detected_command,
                },
            )?;
            self.workspaces[index].repos_added = manual.clone();
        }
        Ok(())
    }

    /// Iced OpenWorktree focuses an existing tab by repo OR current directory.
    pub fn open_worktree_tab(
        &mut self,
        target: &WorkspaceTarget,
        path: &Path,
    ) -> Result<usize, String> {
        let index = target.index(self)?;
        let ws = &mut self.workspaces[index];
        let key = RepoKey::local(path);
        let index = ws
            .tabs
            .iter()
            .position(|t| {
                RepoKey::local(Path::new(&t.dir)) == key
                    || t.repo_dir
                        .as_deref()
                        .is_some_and(|d| RepoKey::local(Path::new(d)) == key)
            })
            .unwrap_or_else(|| {
                ws.tabs.push(WorkspaceTabConfig {
                    extra: Default::default(),
                    dir: path.to_string_lossy().into_owned(),
                    repo_dir: Some(path.to_string_lossy().into_owned()),
                    startup_command: None,
                    tab_kind: None,
                    agent_config: None,
                    chat_session_id: None,
                    task_id: None,
                    task_session_id: None,
                    worker_shutdown_refused: false,
                    session_uid: Some(uuid::Uuid::new_v4().to_string()),
                });
                ws.tabs.len() - 1
            });
        ws.tabs[index]
            .session_uid
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
        ws.active_tab = Some(index);
        Ok(index)
    }
}

pub fn detect_run_command(dir: &Path) -> Option<String> {
    // Detect package manager (used by multiple checks)
    let detect_pm = |dir: &Path| -> &str {
        if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
            "bun"
        } else if dir.join("yarn.lock").exists() {
            "yarn"
        } else if dir.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else {
            "npm"
        }
    };

    // 1. Tauri app (has src-tauri/ directory with Cargo.toml)
    if dir.join("src-tauri").join("Cargo.toml").exists() {
        // Check if package.json has a "tauri" script (frontend-driven setup)
        if dir.join("package.json").exists() {
            let pm = detect_pm(dir);
            if let Ok(contents) = std::fs::read_to_string(dir.join("package.json")) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) {
                    if let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) {
                        if scripts.contains_key("tauri") {
                            return Some(format!("{} run tauri", pm));
                        }
                        if scripts.contains_key("tauri:dev") {
                            return Some(format!("{} run tauri:dev", pm));
                        }
                    }
                }
            }
        }
        // Fallback: run cargo tauri dev from inside src-tauri
        return Some("cd src-tauri && cargo tauri dev".to_string());
    }

    // 2. package.json (check before bare Cargo.toml for hybrid projects)
    if dir.join("package.json").exists() {
        let pm = detect_pm(dir);

        if let Ok(contents) = std::fs::read_to_string(dir.join("package.json")) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) {
                if let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) {
                    // Priority: tauri dev > dev > start
                    for script_name in &["tauri dev", "dev", "start"] {
                        if scripts.contains_key(*script_name) {
                            if *script_name == "start" && pm == "npm" {
                                return Some("npm start".to_string());
                            }
                            return Some(format!("{} run {}", pm, script_name));
                        }
                    }
                }
            }
        }
    }

    // 3. Cargo.toml (plain Rust project)
    if dir.join("Cargo.toml").exists() {
        return Some("cargo run".to_string());
    }

    // 4. docker-compose
    if dir.join("docker-compose.yml").exists() || dir.join("docker-compose.yaml").exists() {
        return Some("docker compose up".to_string());
    }

    // 5. Go project
    if dir.join("go.mod").exists() && (dir.join("main.go").exists() || dir.join("cmd").is_dir()) {
        return Some("go run .".to_string());
    }

    None
}

#[cfg(all(test, feature = "runtime"))]
#[path = "workspace_config/action_tests.rs"]
mod repo_action_tests;

/// Markdown roots follow the workspace home folder, including named work;
/// starting repos and the decisions repo never replace this root.
pub fn document_roots(workspace: &Path) -> (PathBuf, PathBuf) {
    (workspace.join(".plans"), workspace.join("docs"))
}

/// Creates Iced's numbered ~/Work/<slug> folder. Nothing is created on cancel.
pub fn create_work_folder(base: &Path, name: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(base)?;
    let slug = crate::work::summary::work_folder_slug(name);
    for n in 1u32.. {
        let dir = if n == 1 {
            base.join(&slug)
        } else {
            base.join(format!("{slug}-{n}"))
        };
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("create {}: {error}", dir.display()),
                ))
            }
        }
    }
    unreachable!("u32 suffixes exhausted under {}", base.display())
}

/// Iced's startup directory, including its fallback if the cwd was removed.
pub fn initial_directory() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Iced's empty-start workspace: the current directory, Lavender, one terminal.
/// This only builds a value; Preview may use it without writing Iced's catalog.
pub fn initial_workspace(cwd: &Path) -> WorkspaceConfig {
    let name = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "Home".to_string());
    let mut workspace = new_work_config(
        &name,
        cwd,
        WorkspaceColor::Lavender,
        crate::work::catalog::WorkStart::default(),
    );
    workspace.run_command = detect_run_command(cwd);
    workspace.tabs.push(WorkspaceTabConfig {
        extra: Default::default(),
        dir: workspace.dir.clone(),
        repo_dir: Some(workspace.dir.clone()),
        startup_command: None,
        tab_kind: None,
        agent_config: None,
        chat_session_id: None,
        task_id: None,
        task_session_id: None,
        worker_shutdown_refused: false,
        session_uid: None,
    });
    workspace
}

pub fn new_work_config(
    name: &str,
    dir: &Path,
    color: crate::settings::WorkspaceColor,
    start: crate::work::catalog::WorkStart,
) -> crate::workspace_config::WorkspaceConfig {
    use crate::workspace_config::WorkspaceConfig;
    let dir = dir.to_string_lossy().into_owned();
    let added: Vec<String> = start
        .repos
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    WorkspaceConfig {
        extra: Default::default(),
        collapsed_issue_groups: crate::issues::IssueGroup::default_collapsed(),
        name: name.into(),
        abbrev: name.chars().take(2).collect::<String>().to_uppercase(),
        dir: dir.clone(),
        color,
        location: Some(crate::remote::config::WorkspaceLocationConfig::Local { root: dir.clone() }),
        remote_session: None,
        active_tab: Some(0),
        tabs: vec![],
        run_command: None,
        bottom_terminals: vec![],
        env: Default::default(),
        legacy_env_base: None,
        active: true,
        chats: vec![],
        parked: None,
        done_at: None,
        guidelines_off: false,
        claude_in_chrome: false,
        decisions_repo: start
            .decisions_repo
            .map(|p| p.to_string_lossy().into_owned()),
        repos: std::iter::once(dir.clone())
            .chain(added.iter().filter(|r| **r != dir).cloned())
            .collect(),
        home: Some(dir),
        env_overrides: Default::default(),
        repos_added: added,
        repos_removed: vec![],
    }
}

/// Saved effects of Iced's WorkspaceFromChat / ChatAddToWorkspace handlers.
#[derive(Clone, Debug)]
pub enum ChatWorkspaceEdit {
    Make {
        entry: Box<crate::chats::ChatIndexEntry>,
        home: PathBuf,
    },
    Add {
        root: String,
        chat: String,
        target: WorkspaceTarget,
    },
}

impl WorkspacesFile {
    pub fn chat_workspace(&mut self, edit: ChatWorkspaceEdit) -> Result<WorkspaceTarget, String> {
        let (target, root, shown, made) = match edit {
            ChatWorkspaceEdit::Make { entry, home } => {
                if entry.root_id.is_some() {
                    return Err("A child chat folds under its coordinator".into());
                }
                if let Some(owner) = self.workspaces.iter().find(|w| w.chats.contains(&entry.id)) {
                    return Ok(WorkspaceTarget::of(owner));
                }
                let name = crate::work::summary::workspace_name_from_title(&entry.title);
                let dir = create_work_folder(&home.join("Work"), &name).map_err(|e| {
                    format!(
                        "cannot create a folder in {}: {e}",
                        home.join("Work").display()
                    )
                })?;
                use crate::settings::WorkspaceColor as C;
                let colors = [
                    C::Lavender,
                    C::Blue,
                    C::Green,
                    C::Peach,
                    C::Pink,
                    C::Yellow,
                    C::Red,
                    C::Teal,
                ];
                let used: Vec<_> = self
                    .workspaces
                    .iter()
                    .filter(|w| w.active)
                    .map(|w| w.color)
                    .collect();
                let color = colors
                    .iter()
                    .copied()
                    .find(|c| !used.contains(c))
                    .unwrap_or(colors[used.len() % colors.len()]);
                let mut workspace = new_work_config(&name, &dir, color, Default::default());
                workspace.chats.push(entry.id.clone());
                let target = self
                    .workspaces
                    .iter()
                    .position(|w| !w.active)
                    .unwrap_or(self.workspaces.len());
                self.workspaces.insert(target, workspace);
                self.active_workspace = target;
                self.normalize_repos();
                (target, entry.id.clone(), entry.id, true)
            }
            ChatWorkspaceEdit::Add { root, chat, target } => {
                let index = target.index(self)?;
                let workspace = &self.workspaces[index];
                if !workspace.active || !local_workspace(workspace) {
                    return Err("Chat target workspace is not open on this Mac".into());
                }
                let changed = crate::work::summary::assign_chat(
                    self.workspaces
                        .iter_mut()
                        .enumerate()
                        .map(|(i, w)| (i == index, &mut w.chats)),
                    &root,
                );
                if !changed {
                    return Ok(WorkspaceTarget::of(&self.workspaces[index]));
                }
                (index, root, chat, false)
            }
        };
        // Iced moves the coordinator first, then the shown child, retaining
        // every tab field and its live session identity.
        let mut ids = vec![root];
        if ids[0] != shown {
            ids.push(shown);
        }
        let mut moved = false;
        for id in ids {
            let source = self
                .workspaces
                .iter()
                .enumerate()
                .filter(|(i, w)| *i != target && w.active && local_workspace(w))
                .find_map(|(i, w)| {
                    w.tabs
                        .iter()
                        .position(|t| t.chat_session_id.as_deref() == Some(&id))
                        .map(|t| (i, t))
                });
            if let Some((source, tab)) = source {
                if self.workspaces[source].tabs.len() == 1 {
                    let dir = self.workspaces[source].dir.clone();
                    self.workspaces[source].tabs.push(WorkspaceTabConfig {
                        extra: Default::default(),
                        dir: dir.clone(),
                        repo_dir: Some(dir),
                        startup_command: None,
                        tab_kind: None,
                        agent_config: None,
                        chat_session_id: None,
                        task_id: None,
                        task_session_id: None,
                        worker_shutdown_refused: false,
                        session_uid: Some(uuid::Uuid::new_v4().to_string()),
                    });
                }
                let removed = tab;
                let tab = self.workspaces[source].tabs.remove(removed);
                let remaining = self.workspaces[source].tabs.len();
                if let Some(active) = &mut self.workspaces[source].active_tab {
                    *active = active_tab_after_removal(*active, removed, remaining);
                }
                self.workspaces[target].tabs.push(tab);
                self.workspaces[target].active_tab = Some(self.workspaces[target].tabs.len() - 1);
                moved = true;
            }
        }
        if made || moved {
            self.active_workspace = self
                .workspaces
                .iter()
                .take(target)
                .filter(|w| w.active)
                .count();
        }
        Ok(WorkspaceTarget::of(&self.workspaces[target]))
    }
}

fn local_workspace<C>(workspace: &WorkspaceConfig<C>) -> bool {
    workspace.remote_session.is_none()
        && workspace
            .location
            .as_ref()
            .is_none_or(|l| matches!(l, WorkspaceLocationConfig::Local { .. }))
}

#[cfg(all(test, feature = "runtime"))]
mod remaining_chat_workspace_tests {
    use super::*;
    use crate::desktop_store::DesktopStore;
    use serde_json::json;

    fn file(dir: &Path) -> WorkspacesFile {
        let file: WorkspacesFile = serde_json::from_value(json!({"future":true,"active_workspace":0,"workspaces":[
            {"name":"Source","abbrev":"S","dir":dir,"color":"blue","chats":["root"],"active_tab":0,"future_workspace":42,"tabs":[{"dir":dir,"chat_session_id":"root","session_uid":"root-uid","future_tab":7},{"dir":dir,"chat_session_id":"child","session_uid":"child-uid"}]},
            {"name":"Target","abbrev":"T","dir":dir,"color":"green","chats":["other"],"tabs":[]},
            {"name":"Parked","abbrev":"P","dir":dir,"color":"peach","active":false,"chats":["root","parked"],"tabs":[]}
        ]})).unwrap();
        file
    }
    #[test]
    fn remaining_add_coordinator_moves_only_iced_ownership_and_hosted_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let mut initial = file(dir.path());
        let target = WorkspaceTarget::of(&initial.workspaces[1]);
        let mut iced = initial.clone();
        crate::work::summary::assign_chat(
            iced.workspaces
                .iter_mut()
                .enumerate()
                .map(|(i, w)| (i == 1, &mut w.chats)),
            "root",
        );
        for id in ["root", "child"] {
            let i = iced.workspaces[0]
                .tabs
                .iter()
                .position(|t| t.chat_session_id.as_deref() == Some(id))
                .unwrap();
            let tab = iced.workspaces[0].tabs.remove(i);
            iced.workspaces[1].tabs.push(tab);
            iced.workspaces[1].active_tab = Some(iced.workspaces[1].tabs.len() - 1);
        }
        iced.workspaces[0].active_tab = Some(0);
        iced.active_workspace = 1;
        let path = dir.path().join("workspaces.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&initial).unwrap()).unwrap();
        let store = DesktopStore::open(dir.path()).unwrap();
        let saved = store
            .try_edit_workspaces(move |f| {
                f.chat_workspace(ChatWorkspaceEdit::Add {
                    root: "root".into(),
                    chat: "child".into(),
                    target,
                })
                .map(|_| ())
            })
            .wait()
            .unwrap();
        assert_eq!(saved.workspaces[0].tabs.len(), 1);
        assert!(saved.workspaces[0].tabs[0].chat_session_id.is_none());
        assert_eq!(
            saved.workspaces[0].tabs[0].repo_dir,
            Some(dir.path().to_string_lossy().into_owned())
        );
        iced.workspaces[0].tabs = saved.workspaces[0].tabs.clone();
        assert_eq!(
            serde_json::to_value(&saved).unwrap(),
            serde_json::to_value(&iced).unwrap()
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap(),
            serde_json::to_value(iced).unwrap()
        );
        // A fresh stale click fails before touching the document.
        initial.workspaces[1].active = false;
        let before = std::fs::read(&path).unwrap();
        assert!(store
            .try_edit_workspaces(move |f| f
                .chat_workspace(ChatWorkspaceEdit::Add {
                    root: "root".into(),
                    chat: "child".into(),
                    target: WorkspaceTarget::of(&initial.workspaces[1])
                })
                .map(|_| ()))
            .wait()
            .is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn remaining_make_workspace_matches_iced_name_folder_fields_and_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let mut initial = file(dir.path());
        for workspace in &mut initial.workspaces {
            workspace.chats.retain(|c| c != "root");
        }
        let path = dir.path().join("workspaces.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&initial).unwrap()).unwrap();
        let entry = crate::chats::ChatIndexEntry {
            id: "root".into(),
            title: "  Example   work  ".into(),
            backend: crate::chats::ChatBackend::Claude,
            path: dir.path().join("fixture.jsonl"),
            cwd: dir.path().into(),
            repo_root: None,
            is_worktree: false,
            branch: None,
            latest: None,
            mtime: std::time::SystemTime::now(),
            size: 0,
            dead_cwd: false,
            facts: None,
            root_id: None,
        };
        let home = dir.path().join("home");
        let store = DesktopStore::open(dir.path()).unwrap();
        let saved = store
            .try_edit_workspaces(move |f| {
                f.chat_workspace(ChatWorkspaceEdit::Make {
                    entry: Box::new(entry),
                    home,
                })
                .map(|_| ())
            })
            .wait()
            .unwrap();
        let made = &saved.workspaces[2];
        let expected_dir = dir.path().join("home/Work/example-work");
        let mut iced = new_work_config(
            &crate::work::summary::workspace_name_from_title("  Example   work  "),
            &expected_dir,
            crate::settings::WorkspaceColor::Lavender,
            Default::default(),
        );
        iced.chats = vec!["root".into()];
        iced.tabs = vec![initial.workspaces[0].tabs[0].clone()];
        assert_eq!(
            serde_json::to_value(made).unwrap(),
            serde_json::to_value(iced).unwrap()
        );
        assert!(expected_dir.is_dir());
        assert_eq!(saved.active_workspace, 2);
        assert_eq!(saved.workspaces[0].extra["future_workspace"], 42);
        assert_eq!(
            saved.workspaces[0].tabs[0].chat_session_id.as_deref(),
            Some("child")
        );
        assert_eq!(saved.workspaces[3].chats, ["parked"]);
    }
}

/// Iced's selection after a hosted tab moves to another workspace.
pub fn active_tab_after_removal(active: usize, removed: usize, remaining: usize) -> usize {
    let active = if active > removed { active - 1 } else { active };
    active.min(remaining.saturating_sub(1))
}

mod workspace_bar;

#[cfg(all(test, feature = "runtime"))]
#[path = "workspace_config/desktop_session_tests.rs"]
mod desktop_session_tests;

#[cfg(test)]
mod initial_workspace_tests {
    use super::*;

    #[test]
    fn empty_start_workspace_matches_iced_cwd_name_color_and_terminal() {
        let fixture = tempfile::tempdir().unwrap();
        let cwd = fixture.path().join("example-project");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(cwd.join("Cargo.toml"), "[package]\nname = 'fixture'\n").unwrap();
        let initial = initial_workspace(&cwd);
        assert_eq!(initial.name, "example-project");
        assert_eq!(initial.abbrev, "EX");
        assert_eq!(initial.color, WorkspaceColor::Lavender);
        assert_eq!(initial.dir, cwd.to_string_lossy());
        assert_eq!(initial.run_command, detect_run_command(&cwd));
        assert_eq!(initial.active_tab, Some(0));
        assert!(initial.active);
        assert_eq!(initial.tabs.len(), 1);
        let tab = &initial.tabs[0];
        assert_eq!(tab.dir, initial.dir);
        assert_eq!(tab.repo_dir.as_deref(), Some(initial.dir.as_str()));
        assert!(tab.tab_kind.is_none() && tab.startup_command.is_none());
        assert!(!fixture.path().join("workspaces.json").exists());
        assert_eq!(initial_workspace(Path::new("/")).name, "Home");
    }
}
