//! The unchanged workspaces.json wire format shared by hosts. No persistence authority.
use crate::remote::config::{RemoteAgentConfig, WorkspaceLocationConfig};
use crate::settings::WorkspaceColor;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
// Workspace persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename = "WorkspacesFile")]
pub struct WorkspacesFile<C = WorkspaceColor> {
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

/// A repo as `workspaces.json` stores it (TRU-180). Identified by `dir` on
/// its machine (`location`; `None` is this Mac), see `RepoKey`. It is
/// open while an open workspace lists it; this entry persists while any
/// workspace does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
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
            dir: self.home_dir().to_string(),
            location: self.location.clone(),
            run_command: self.run_command.clone(),
            env: self.env.clone(),
            bottom_terminals: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceTabConfig {
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
