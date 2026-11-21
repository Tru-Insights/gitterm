//! Read-only view facts. Live data reads the application owner on the GPUI thread.
use gpui::App;

/// Read-only roster facts from the resolved Iced catalog, never the preview's
/// own config directory. Keep tasks outside the chat's home workspace too:
/// workers can run in another repository or worktree.
#[derive(Default)]
pub(crate) struct RosterFacts {
    pub tasks: Vec<gitterm_core::tasks::TaskRecord>,
    pub workers: Vec<gitterm_core::tasks::Delegation>,
    pub tabs: Vec<gitterm_core::workspace_config::WorkspaceTabConfig>,
}
impl RosterFacts {
    pub(crate) fn worker_tab(
        &self,
        task_id: &str,
        session: Option<&str>,
    ) -> gitterm_core::delegations::WorkerTab {
        use gitterm_core::{delegations::WorkerTab, tasks::TaskLifecycle};
        if !self.tabs.iter().any(|tab| {
            tab.task_id.as_deref() == Some(task_id)
                && session.is_none_or(|id| tab.task_session_id.as_deref() == Some(id))
        }) {
            WorkerTab::Closed
        } else if self
            .tasks
            .iter()
            .any(|task| task.task_id == task_id && task.lifecycle == TaskLifecycle::WaitingForInput)
        {
            WorkerTab::AwaitingInput
        } else {
            WorkerTab::Idle
        }
    }

    #[cfg(test)]
    pub(crate) fn read_from(source: &std::path::Path) -> anyhow::Result<Self> {
        Self::read(source, &TaskSource::iced(source))
    }
    /// `source` is Iced's config root, for its tabs; tasks and workers come
    /// from `tasks`.
    pub(crate) fn read(source: &std::path::Path, tasks: &TaskSource) -> anyhow::Result<Self> {
        let store = tasks.load()?;
        let catalog = crate::workspace_config::IcedCatalog::read(source)?;
        Ok(Self {
            tasks: store.tasks().to_vec(),
            workers: store.delegations().iter().rev().cloned().collect(),
            tabs: catalog
                .workspaces
                .into_iter()
                .filter(|w| {
                    !w.is_legacy_remote()
                        && !matches!(w.location,
                    Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. }))
                })
                .flat_map(|w| w.tabs)
                .collect(),
        })
    }
}
/// Where the panes read tasks and delegations (H5b B8b): the preview task
/// host's store when the host runs, else Iced's `tasks.json`, read-only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TaskSource {
    /// The host store's own file under the preview root. B4 seeds it from
    /// Iced's file (re-seeded every 2 s, replaced only by an atomic rename of
    /// a validated copy) and loads the host's store from it, so loading it
    /// reads the host's snapshot. A path, not a clone: `TaskStore` is not
    /// `Clone` and the readers run on the background executor.
    Host(std::path::PathBuf),
    /// Iced's own file: only when the host did not start.
    Iced(std::path::PathBuf),
}
impl TaskSource {
    /// Resolve on the GPUI thread, then load where the reader runs.
    /// `iced_config` is Iced's config root, used only when the host is down.
    pub(crate) fn resolve(iced_config: &std::path::Path, cx: &App) -> Self {
        static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        Self::choose(crate::task_panel::host(cx), iced_config, &LOGGED, |line| {
            eprintln!("{line}")
        })
    }
    pub(crate) fn choose(
        host: Option<&gitterm_core::task_host::TaskHost>,
        iced_config: &std::path::Path,
        logged: &std::sync::atomic::AtomicBool,
        log: impl FnOnce(&str),
    ) -> Self {
        match host.and_then(|host| host.store().as_ref()) {
            Some(store) => Self::Host(store.path().to_owned()),
            None => {
                if !logged.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    log(
                        "GPUI panes read Iced's tasks.json directly: the preview task host is not running (see its startup error)",
                    );
                }
                Self::iced(iced_config)
            }
        }
    }
    pub(crate) fn iced(iced_config: &std::path::Path) -> Self {
        Self::Iced(gitterm_core::tasks::TaskStore::path_for_config_root(
            iced_config,
        ))
    }
    pub(crate) fn load(
        &self,
    ) -> Result<gitterm_core::tasks::TaskStore, gitterm_core::tasks::TaskStoreError> {
        match self {
            Self::Host(path) | Self::Iced(path) => gitterm_core::tasks::TaskStore::load(path),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceId(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionId(pub u64);

#[derive(Clone, Debug)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub layout_key: String,
    pub name: String,
    pub status: String,
    pub repositories: Vec<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    Terminal,
    Chat,
}
#[derive(Clone, Debug)]
pub struct Session {
    pub id: SessionId,
    pub title: String,
    pub kind: SessionKind,
}
#[derive(Clone, Debug)]
pub struct ChangedFile {
    pub path: String,
    pub status: String,
    pub staged: bool,
}
#[derive(Clone, Debug)]
pub struct GitStatus {
    /// False until a first status result exists; the branch and files are placeholders.
    pub checked: bool,
    pub repository: String,
    pub branch: String,
    pub files: Vec<ChangedFile>,
    pub loading: bool,
    pub error: Option<String>,
    pub is_git_repo: bool,
}
#[derive(Clone, Debug)]
pub struct FileNode {
    pub name: String,
    pub children: Vec<FileNode>,
}

pub trait ShellData {
    fn local_owner(&self) -> Option<gpui::Entity<crate::WorkspaceOwner>> {
        None
    }
    fn workspaces(&self, cx: &App) -> Vec<Workspace>;
    fn sessions(&self, workspace: WorkspaceId, _cx: &App) -> Vec<Session>;
    fn active_session(&self, _workspace: WorkspaceId, _cx: &App) -> Option<SessionId> {
        None
    }
    fn imported_session(
        &self,
        _workspace: WorkspaceId,
        _id: SessionId,
        _cx: &App,
    ) -> Option<String> {
        None
    }
    fn imported_chat(
        &self,
        _workspace: WorkspaceId,
        _id: SessionId,
        _cx: &App,
    ) -> Option<gitterm_core::workspace_config::WorkspaceTabConfig> {
        None
    }
    fn status(&self, workspace: WorkspaceId, cx: &App) -> GitStatus;
    fn files(&self, workspace: WorkspaceId, cx: &App) -> Vec<FileNode>;
    /// Folder new terminals start in; `None` means the user's home folder.
    fn terminal_cwd(&self, _workspace: WorkspaceId, _cx: &App) -> Option<std::path::PathBuf> {
        None
    }
}

pub struct SampleData;
impl ShellData for SampleData {
    fn workspaces(&self, _cx: &App) -> Vec<Workspace> {
        [
            (1, "acme-api", "Working", vec!["acme-api", "shared-types"]),
            (2, "web-app", "Needs you", vec!["web-app"]),
            (3, "design-system", "Quiet", vec!["design-system"]),
        ]
        .into_iter()
        .map(|(id, name, status, repos)| Workspace {
            id: WorkspaceId(id),
            layout_key: id.to_string(),
            name: name.into(),
            status: status.into(),
            repositories: repos.into_iter().map(String::from).collect(),
        })
        .collect()
    }
    fn sessions(&self, workspace: WorkspaceId, _cx: &App) -> Vec<Session> {
        vec![
            Session {
                id: SessionId(workspace.0 * 10 + 1),
                title: "Codex · Retry handling".into(),
                kind: SessionKind::Chat,
            },
            Session {
                id: SessionId(workspace.0 * 10 + 2),
                title: "Shell".into(),
                kind: SessionKind::Terminal,
            },
        ]
    }
    fn status(&self, workspace: WorkspaceId, cx: &App) -> GitStatus {
        let repository = self
            .workspaces(cx)
            .into_iter()
            .find(|w| w.id == workspace)
            .expect("sample workspace exists")
            .name;
        GitStatus {
            checked: true,
            repository,
            loading: false,
            error: None,
            is_git_repo: true,
            branch: if workspace.0 == 3 {
                "main"
            } else {
                "fix/retry-handling"
            }
            .into(),
            files: if workspace.0 == 3 {
                vec![]
            } else {
                [
                    ("src/client.rs", "M", false),
                    ("src/retry.rs", "M", false),
                    ("tests/retry.rs", "A", true),
                    ("notes.md", "??", false),
                ]
                .into_iter()
                .map(|(path, status, staged)| ChangedFile {
                    path: path.into(),
                    status: status.into(),
                    staged,
                })
                .collect()
            },
        }
    }
    fn files(&self, _: WorkspaceId, _cx: &App) -> Vec<FileNode> {
        let leaf = |name: &str| FileNode {
            name: name.into(),
            children: vec![],
        };
        vec![
            FileNode {
                name: "src".into(),
                children: vec![leaf("client.rs"), leaf("retry.rs")],
            },
            FileNode {
                name: "tests".into(),
                children: vec![leaf("retry.rs")],
            },
            leaf("Cargo.toml"),
            leaf("README.md"),
            leaf("notes.md"),
        ]
    }
}

/// A view handle, never a second writable catalog or service owner.
pub struct LocalData(pub gpui::Entity<crate::WorkspaceOwner>);
impl ShellData for LocalData {
    fn local_owner(&self) -> Option<gpui::Entity<crate::WorkspaceOwner>> {
        Some(self.0.clone())
    }
    fn terminal_cwd(&self, workspace: WorkspaceId, cx: &App) -> Option<std::path::PathBuf> {
        self.0
            .read(cx)
            .model
            .workspaces
            .iter()
            .find(|w| w.id == workspace && !w.remote)
            .map(|w| w.folder.clone())
    }
    fn workspaces(&self, cx: &App) -> Vec<Workspace> {
        self.0
            .read(cx)
            .model
            .workspaces
            .iter()
            .map(|w| {
                if let Some(saved) = self.0.read(cx).iced_workspace(w.id) {
                    let home = saved.home_dir().to_owned();
                    let mut repositories = vec![home.clone()];
                    repositories.extend(saved.repos.iter().filter(|r| **r != home).cloned());
                    return Workspace {
                        id: w.id,
                        layout_key: crate::workspace_config::iced_layout_key(saved),
                        name: saved.name.clone(),
                        status: w.status.branch_name.clone(),
                        repositories,
                    };
                }
                Workspace {
                    id: w.id,
                    layout_key: crate::layout::workspace_layout_key(&w.folder),
                    name: w
                        .folder
                        .file_name()
                        .unwrap_or(w.folder.as_os_str())
                        .to_string_lossy()
                        .into_owned(),
                    status: w.status.branch_name.clone(),
                    repositories: vec![w.status.repo_name.clone()],
                }
            })
            .collect()
    }
    fn sessions(&self, workspace: WorkspaceId, cx: &App) -> Vec<Session> {
        if let Some(saved) = self.0.read(cx).iced_workspace(workspace) {
            return iced_sessions(saved);
        }
        let model = &self.0.read(cx).model;
        let folder = &model
            .workspaces
            .iter()
            .find(|candidate| candidate.id == workspace)
            .expect("owned workspace")
            .folder;
        vec![Session {
            id: SessionId(crate::layout::stable_path_hash(folder)),
            title: "Shell".into(),
            kind: SessionKind::Terminal,
        }]
    }
    fn active_session(&self, workspace: WorkspaceId, cx: &App) -> Option<SessionId> {
        let owner = self.0.read(cx);
        let saved = owner.iced_workspace(workspace)?;
        let sessions = iced_sessions(saved);
        let active = iced_active_tab(saved);
        sessions.get(active).map(|s| s.id)
    }
    fn imported_session(&self, workspace: WorkspaceId, id: SessionId, cx: &App) -> Option<String> {
        let owner = self.0.read(cx);
        let saved = owner.iced_workspace(workspace)?;
        let sessions = iced_sessions(saved);
        let index = sessions.iter().position(|s| s.id == id)?;
        let dir = saved
            .tabs
            .get(index)
            .map(|tab| tab.dir.as_str())
            .unwrap_or(&saved.dir);
        let conversation = saved
            .tabs
            .get(index)
            .and_then(|tab| tab.chat_session_id.as_deref())
            .unwrap_or("Terminal");
        Some(format!(
            "{}\nIced session (read-only)\n{dir}\n{conversation}{}",
            sessions[index].title,
            if sessions[index].kind == SessionKind::Chat {
                "\nhistory unavailable in preview"
            } else {
                ""
            }
        ))
    }
    fn imported_chat(
        &self,
        workspace: WorkspaceId,
        id: SessionId,
        cx: &App,
    ) -> Option<gitterm_core::workspace_config::WorkspaceTabConfig> {
        let owner = self.0.read(cx);
        let saved = owner.iced_workspace(workspace)?;
        if saved.is_legacy_remote()
            || matches!(
                saved.location,
                Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })
            )
        {
            return None;
        }
        let index = iced_sessions(saved).iter().position(|s| s.id == id)?;
        saved
            .tabs
            .get(index)
            .filter(|t| {
                t.tab_kind.as_deref() == Some("agent")
                    && matches!(
                        t.agent_config,
                        Some(gitterm_core::chat::types::AgentBackendConfig::Claude { .. })
                    )
            })
            .cloned()
    }
    fn status(&self, workspace: WorkspaceId, cx: &App) -> GitStatus {
        self.0.read(cx).model.status_view(workspace)
    }
    fn files(&self, _: WorkspaceId, _: &App) -> Vec<FileNode> {
        vec![]
    }
}

pub(crate) fn iced_sessions(
    saved: &gitterm_core::workspace_config::WorkspaceConfig,
) -> Vec<Session> {
    let key = crate::workspace_config::iced_layout_key(saved);
    let mut sessions: Vec<_> = saved
        .tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            let identity = tab
                .session_uid
                .as_ref()
                .map(|uid| format!("{key}:tab:{uid}"))
                .unwrap_or_else(|| format!("{key}:tab-index:{index}"));
            Session {
                id: SessionId(crate::layout::stable_path_hash(std::path::Path::new(
                    &identity,
                ))),
                title: tab.chat_session_id.clone().unwrap_or_else(|| {
                    if tab.tab_kind.as_deref() == Some("agent") {
                        "Agent".into()
                    } else {
                        "Shell".into()
                    }
                }),
                kind: if tab.tab_kind.as_deref() == Some("agent") {
                    SessionKind::Chat
                } else {
                    SessionKind::Terminal
                },
            }
        })
        .collect();
    // Owned root chats classify the work; only saved tabs are open sessions.
    // Iced gives an empty local workspace its default root shell tab.
    if sessions.is_empty()
        && !saved.is_legacy_remote()
        && !matches!(
            saved.location,
            Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })
        )
    {
        sessions.push(Session {
            id: SessionId(crate::layout::stable_path_hash(std::path::Path::new(
                &format!("{key}:default"),
            ))),
            title: "Shell".into(),
            kind: SessionKind::Terminal,
        });
    }
    sessions
}

pub(crate) fn iced_active_tab(saved: &gitterm_core::workspace_config::WorkspaceConfig) -> usize {
    saved
        .active_tab
        .filter(|&i| i < saved.tabs.len())
        .or_else(|| {
            saved.tabs.iter().position(|tab| {
                let equal =
                    |a: &str, b: &str| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
                        (Ok(a), Ok(b)) => a == b,
                        _ => a == b,
                    };
                equal(&tab.dir, &saved.dir)
                    || tab
                        .repo_dir
                        .as_deref()
                        .is_some_and(|dir| equal(dir, &saved.dir))
            })
        })
        .unwrap_or(0)
}

/// A read-only Work load. No store mutation, migration, lease, or index backfill
/// is performed. The caller runs this blocking work away from the GPUI thread.
#[derive(Clone)]
pub(crate) struct IcedWorkData {
    pub facts: crate::work_sidebar::WorkspaceFacts,
    pub tasks: Vec<gitterm_core::tasks::TaskRecord>,
    pub workers: Vec<gitterm_core::tasks::Delegation>,
    pub running_task_ids: std::collections::HashSet<String>,
    pub repo_recency: Vec<(std::path::PathBuf, std::time::SystemTime)>,
    pub pull_requests: Vec<crate::work_sidebar::PullRequestGroup>,
    pub indexed_chats: Vec<gitterm_core::chats::ChatIndexEntry>,
    pub issues: gitterm_core::issues::IssueTitlesFile,
    pub collapsed_issue_groups: Vec<gitterm_core::issues::IssueGroup>,
    inputs: gitterm_core::work::state::Inputs,
    summary: gitterm_core::work::summary::WorkSummary,
    branches: Vec<gitterm_core::work::summary::BranchStatus>,
    documents: gitterm_core::work::documents::ScanResult,
    errors: Vec<String>,
}
impl IcedWorkData {
    pub fn read(
        config: &std::path::Path,
        tasks: &TaskSource,
        id: u64,
        saved: &gitterm_core::workspace_config::WorkspaceConfig,
        focus: gitterm_core::work::summary::Focus,
        previous: Option<&Self>,
        mut publish: impl FnMut(&Self),
    ) -> anyhow::Result<Self> {
        #[cfg(test)]
        let home: Option<std::path::PathBuf> = None;
        #[cfg(not(test))]
        let home = dirs::home_dir();
        // Retain indexed chats and repo presentation while partial facts publish
        // ahead of the slow full read, which replaces the snapshot.
        if home.is_some() {
            Self::read_with_progress(
                config,
                tasks,
                None,
                id,
                saved,
                focus.clone(),
                previous,
                false,
                &mut publish,
            )?;
        }
        Self::read_with_progress(
            config,
            tasks,
            home.as_deref(),
            id,
            saved,
            focus,
            previous,
            true,
            publish,
        )
    }
    #[cfg(test)]
    pub(crate) fn read_with_home(
        config: &std::path::Path,
        home: Option<&std::path::Path>,
        id: u64,
        saved: &gitterm_core::workspace_config::WorkspaceConfig,
        focus: gitterm_core::work::summary::Focus,
    ) -> anyhow::Result<Self> {
        Self::read_with_progress(
            config,
            &TaskSource::iced(config),
            home,
            id,
            saved,
            focus,
            None,
            true,
            |_| {},
        )
    }
    // Keep the prior snapshot explicit alongside the read inputs and progress sink.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn read_with_progress(
        config: &std::path::Path,
        task_source: &TaskSource,
        home: Option<&std::path::Path>,
        id: u64,
        saved: &gitterm_core::workspace_config::WorkspaceConfig,
        focus: gitterm_core::work::summary::Focus,
        previous: Option<&Self>,
        measure_branches: bool,
        mut publish: impl FnMut(&Self),
    ) -> anyhow::Result<Self> {
        use gitterm_core::{
            chats,
            tasks::*,
            work::{documents, state::*, summary::*},
        };
        use std::{
            collections::HashMap,
            path::{Path, PathBuf},
            time::SystemTime,
        };
        let started = std::time::Instant::now();
        eprintln!(
            "[work-sidebar] reading {} (workspace {id}, transcripts: {})",
            config.display(),
            home.is_some()
        );
        let catalog = crate::workspace_config::IcedCatalog::read(config)?;
        let key = crate::workspace_config::iced_layout_key(saved);
        let saved = catalog
            .workspaces
            .iter()
            .find(|w| crate::workspace_config::iced_layout_key(w) == key)
            .ok_or_else(|| anyhow::anyhow!("Iced workspace {} is no longer open", saved.name))?;
        let local = !saved.is_legacy_remote()
            && !matches!(
                saved.location,
                Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })
            );
        let mut facts = crate::work_sidebar::WorkspaceFacts {
            id,
            name: saved.name.clone(),
            dir: saved.dir.clone().into(),
            home: saved.home_dir().into(),
            local,
            repos: vec![],
            removed: saved.repos_removed.iter().map(PathBuf::from).collect(),
            auto_added: vec![],
            open_chats: saved
                .tabs
                .iter()
                .filter_map(|t| t.chat_session_id.clone())
                .collect(),
            owned_chats: saved.chats.clone(),
            chat_owners: catalog
                .workspaces
                .iter()
                .flat_map(|w| w.chats.iter().map(|chat| (chat.clone(), w.name.clone())))
                .collect(),
            running_by_repo: HashMap::new(),
            selected_checkout: None,
        };
        let store = task_source.load()?;
        let equal = |a: &Path, b: &Path| {
            a.canonicalize().unwrap_or_else(|_| a.to_owned())
                == b.canonicalize().unwrap_or_else(|_| b.to_owned())
        };
        let workspace_root = match &saved.location {
            Some(gitterm_core::remote::config::WorkspaceLocationConfig::Local { root }) => {
                Path::new(root)
            }
            _ => Path::new(&saved.dir),
        };
        let mut tasks: Vec<_> = store.tasks().iter().filter(|t| t.lifecycle != TaskLifecycle::Archived && match (&t.workspace.location, &saved.location) {
            (WorkspaceLocationIdentity::Local { directory }, _) if local => equal(directory, Path::new(&saved.dir)) || equal(directory, workspace_root),
            (WorkspaceLocationIdentity::RemoteAgent { remote_id: a, root: ar }, Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { remote_id: b, root: br, .. })) => a == b && ar == br,
            _ => false,
        }).cloned().collect();
        tasks.sort_by(|a, b| {
            crate::tasks::sort_bucket(a)
                .cmp(&crate::tasks::sort_bucket(b))
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        let workers = store
            .delegations()
            .iter()
            .rev()
            .filter(|d| {
                d.dismissed_at.is_none()
                    && (saved
                        .tabs
                        .iter()
                        .any(|t| t.session_uid.as_deref() == Some(&d.parent.session_uid))
                        || d.parent
                            .chat_session_id
                            .as_ref()
                            .is_some_and(|id| saved.chats.contains(id))
                        || (local && equal(&d.parent.cwd, Path::new(&saved.dir))))
            })
            .cloned()
            .collect();
        let mut errors = vec![];
        let mut entries = if local {
            home.map(|home| crate::workspace_config::read_iced_chat_index(config, home))
                .unwrap_or_else(|| {
                    previous.map_or_else(Vec::new, |data| data.indexed_chats.clone())
                })
        } else {
            vec![]
        };
        // The host can later use this snapshot for recent-repo shortcuts. Keep
        // the index's order and message times, excluding dead directories as Iced does.
        let mut repo_recency = vec![];
        for entry in entries.iter().filter(|entry| !entry.dead_cwd) {
            let repo = entry.group_root();
            if !repo_recency.iter().any(|(seen, _)| seen == repo) {
                repo_recency.push((repo.to_owned(), entry.mtime));
            }
        }
        let indexed = !entries.is_empty();
        let links = if !indexed {
            vec![]
        } else {
            iced_chat_links(&catalog, &store)
        };
        gitterm_core::chat_facts::resolve_tree(&mut entries, &links);
        let indexed_chats = entries.clone();
        let infos: Vec<_> = store.tasks().iter().map(TaskInfo::from).collect();
        let source = if saved.chats.is_empty() {
            Source::Folder(saved.dir.clone().into())
        } else {
            Source::Owned(saved.chats.clone())
        };
        let summary = SummaryPlan {
            source: source.clone(),
            tasks: infos.clone(),
            focus: focus.clone(),
            now: SystemTime::now(),
        }
        .build(&entries);
        // Saved facts supply repo/PR ownership without inventing Chats rows.
        {
            let mut pending: Vec<_> = saved
                .chats
                .iter()
                .cloned()
                .chain(facts.open_chats.iter().cloned())
                .map(|id| (id.clone(), id))
                .collect();
            let mut seen = std::collections::HashSet::new();
            while let Some((chat, title)) = pending.pop() {
                if !seen.insert(chat.clone()) {
                    continue;
                }
                let mut loaded = None;
                for backend in chats::ChatBackend::ALL {
                    let path = match gitterm_core::chat_facts::facts_path(
                        &gitterm_core::chat_facts::facts_dir(config),
                        backend,
                        &chat,
                    ) {
                        Ok(path) => path,
                        Err(error) => {
                            errors.push(error);
                            break;
                        }
                    };
                    match gitterm_core::chat_facts::load_facts(&path) {
                        Ok(Some(facts)) => {
                            loaded = Some((backend, path, facts));
                            break;
                        }
                        Ok(None) => {}
                        Err(error) => errors.push(error),
                    }
                }
                let Some((backend, path, chat_facts)) = loaded else {
                    continue;
                };
                pending.extend(chat_facts.children.iter().map(|c| {
                    (
                        c.session_id.clone(),
                        c.title.clone().unwrap_or_else(|| c.session_id.clone()),
                    )
                }));
                if entries.iter().any(|entry| entry.id == chat) {
                    continue;
                }
                entries.push(chats::ChatIndexEntry {
                    id: chat,
                    backend,
                    path: path.clone(),
                    cwd: saved.dir.clone().into(),
                    repo_root: None,
                    is_worktree: false,
                    branch: None,
                    title,
                    latest: None,
                    mtime: std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH),
                    size: 0,
                    dead_cwd: !Path::new(&saved.dir).exists(),
                    root_id: None,
                    facts: Some(chat_facts),
                });
            }
        }
        gitterm_core::chat_facts::resolve_tree(&mut entries, &links);
        let task_worktrees: Vec<PathBuf> = store
            .tasks()
            .iter()
            .filter_map(|t| t.worktree.path.clone())
            .collect();
        let mut git = WorkGit(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
        );
        let branches = vec![];
        if local {
            for dir in &saved.repos {
                let path = PathBuf::from(dir);
                let run_command = catalog
                    .repos
                    .iter()
                    .find(|r| r.dir == *dir && r.key().is_local())
                    .and_then(|r| r.run_command.clone());
                facts.repos.push(crate::work_sidebar::RepoFacts {
                    dir: path.clone(),
                    local,
                    run_command,
                });
            }
        } else {
            errors.push("The overview reads this Mac's chats and checkouts; this workspace is on another machine.".into());
        }
        let work_entries = match &source {
            Source::Owned(roots) => chats_under(&entries, roots),
            Source::Folder(dir) => work_chats(&entries, dir),
        };
        // Iced also lists repos with recognized write work and delegated worker
        // targets, even if no file-edit count has been recorded for them yet.
        if local {
            let mut candidates: Vec<PathBuf> =
                saved.repos_added.iter().map(PathBuf::from).collect();
            for chat in &work_entries {
                if let Some(chat_facts) = &chat.facts {
                    candidates.extend(chat_facts.repo_work.iter().cloned());
                    candidates.extend(
                        chat_facts
                            .repos_touched
                            .iter()
                            .filter(|repo| repo.edits > 0)
                            .map(|repo| repo.dir.clone()),
                    );
                    candidates.extend(
                        chat_facts
                            .checkouts
                            .iter()
                            .filter(|checkout| checkout.edits > 0)
                            .map(|checkout| checkout.repo.clone()),
                    );
                    for child in &chat_facts.children {
                        if child.kind == gitterm_core::chat_facts::ChildKind::Worker
                            && let Some(task) =
                                child.task_id.as_deref().and_then(|id| store.get(id))
                            && task
                                .repository
                                .common_dir
                                .file_name()
                                .is_some_and(|name| name == ".git")
                        {
                            candidates
                                .extend(task.repository.common_dir.parent().map(Path::to_path_buf));
                        }
                    }
                }
                for delegation in store
                    .delegations()
                    .iter()
                    .filter(|d| d.parent.chat_session_id.as_deref() == Some(&chat.id))
                {
                    candidates.extend(delegation.options.checkout.iter().cloned());
                    candidates.extend(delegation.options.add_dirs.iter().cloned());
                    if let DelegationChild::TaskSession { task_id, .. } = &delegation.child
                        && let Some(task) = store.get(task_id)
                        && task
                            .repository
                            .common_dir
                            .file_name()
                            .is_some_and(|name| name == ".git")
                    {
                        candidates
                            .extend(task.repository.common_dir.parent().map(Path::to_path_buf));
                    }
                }
            }
            for dir in candidates {
                if !dir.is_dir()
                    || task_worktrees
                        .iter()
                        .any(|tree: &PathBuf| dir.starts_with(tree))
                {
                    continue;
                }
                let dir = git.main_checkout(&dir);
                if facts.home.starts_with(&dir)
                    || facts.removed.contains(&dir)
                    || facts.repos.iter().any(|repo| repo.dir == dir)
                {
                    continue;
                }
                let run_command = catalog
                    .repos
                    .iter()
                    .find(|repo| Path::new(&repo.dir) == dir && repo.key().is_local())
                    .and_then(|repo| repo.run_command.clone());
                facts.repos.push(crate::work_sidebar::RepoFacts {
                    dir,
                    local: true,
                    run_command,
                });
            }
        }
        for repo in &facts.repos {
            let common =
                gitterm_core::work::documents::GitDiscovery::git_common_dir(&mut git, &repo.dir)
                    .ok();
            let running = store
                .tasks()
                .iter()
                .filter(|task| {
                    task.lifecycle == TaskLifecycle::Running
                        && common
                            .as_ref()
                            .is_some_and(|common| equal(&task.repository.common_dir, common))
                })
                .count();
            if running > 0 {
                facts.running_by_repo.insert(repo.dir.clone(), running);
            }
        }
        let written = work_entries
            .iter()
            .filter_map(|e| e.facts.as_ref())
            .flat_map(|f| f.documents.iter().cloned())
            .collect();
        let request = documents::ScanRequest {
            docs_dir: Path::new(&saved.dir).join("docs"),
            written,
            repos: facts.repos.iter().map(|r| r.dir.clone()).collect(),
            task_worktrees,
        };
        let documents = if local {
            documents::scan(&request, &mut git)
        } else {
            Default::default()
        };
        let inputs = Inputs {
            workspace: id,
            source,
            tasks: infos,
            focus,
            documents: local.then_some(request),
        };
        let watch =
            gitterm_core::prs::WatchFile::load(&gitterm_core::prs::WatchFile::path_in(config));
        let plans =
            gitterm_core::merge_plan::load_plans(&gitterm_core::merge_plan::plans_dir(config));
        let roots: Vec<_> = if saved.chats.is_empty() {
            summary
                .coordinators
                .iter()
                .filter(|root| root.recent)
                .map(|root| (root.id.clone(), root.title.clone()))
                .collect()
        } else {
            saved
                .chats
                .iter()
                .map(|id| {
                    let title = indexed_chats
                        .iter()
                        .find(|entry| entry.id == *id)
                        .map(|entry| entry.title.clone())
                        .unwrap_or_else(|| id.chars().take(8).collect());
                    (id.clone(), title)
                })
                .collect()
        };
        let pull_requests = roots
            .iter()
            .filter_map(|(id, title)| {
                let mut keys: Vec<_> = plans
                    .get(id)
                    .into_iter()
                    .flat_map(|plan| &plan.entries)
                    .map(|entry| entry.key())
                    .collect();
                let roots = vec![id.clone()];
                for entry in chats_under(&entries, &roots) {
                    for pr in entry.facts.iter().flat_map(|facts| &facts.prs) {
                        if !keys.iter().any(|key| key.id() == pr.key().id()) {
                            keys.push(pr.key());
                        }
                    }
                }
                let prs: Vec<_> = keys
                    .into_iter()
                    .map(|key| {
                        watch
                            .prs
                            .get(&key.id())
                            .cloned()
                            .unwrap_or_else(|| gitterm_core::prs::WatchedPr::new(key))
                    })
                    .collect();
                (!prs.is_empty()).then(|| crate::work_sidebar::PullRequestGroup {
                    root: id.clone(),
                    title: title.clone(),
                    prs,
                })
            })
            .collect();
        let issues = gitterm_core::issues::IssueTitlesFile::load(
            &gitterm_core::issues::IssueTitlesFile::path_in(config),
        );
        let mut data = Self {
            facts,
            tasks,
            workers,
            running_task_ids: store
                .tasks()
                .iter()
                .filter(|task| task.lifecycle == TaskLifecycle::Running)
                .map(|task| task.task_id.clone())
                .collect(),
            repo_recency,
            pull_requests,
            indexed_chats,
            issues,
            collapsed_issue_groups: saved.collapsed_issue_groups.clone(),
            inputs,
            summary,
            branches,
            documents,
            errors,
        };
        // Real work can own hundreds of checkouts. Publish its overview before
        // probing status/base candidates for every branch, as Iced does.
        eprintln!(
            "[work-sidebar] overview {} (workspace {id}): {} repos, {} chats, {} documents in {:?}",
            config.display(),
            data.facts.repos.len(),
            data.summary.coordinators.len(),
            data.documents.documents.len(),
            started.elapsed()
        );
        data.publish_partial(previous, &mut publish);
        if local && measure_branches {
            // Iced only lists checkouts owned by the focused work: facts edits,
            // its child task worktrees, and issue-key matches. A shared repo's
            // unrelated branches must never be presented as this work's edits.
            let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
            let key_repos: Vec<_> = data
                .summary
                .key_repos
                .iter()
                .map(|p| canonical(p))
                .collect();
            let mut candidates = data.summary.candidates.clone();
            for candidate in &mut candidates {
                candidate.checkout = canonical(&candidate.checkout);
                candidate.repo = candidate.repo.as_deref().map(canonical);
            }
            let mut probes = key_repos.clone();
            probes.extend(candidates.iter().filter_map(|c| c.repo.clone()));
            probes.extend(
                candidates
                    .iter()
                    .filter(|c| c.repo.is_none())
                    .map(|c| c.checkout.clone()),
            );
            let mut lists = HashMap::new();
            let mut seen = std::collections::HashSet::new();
            for probe in probes {
                if !seen.insert(probe.clone()) || !probe.is_dir() {
                    continue;
                }
                match gitterm_core::work::documents::GitDiscovery::list_worktrees(&mut git, &probe)
                {
                    Ok(list) if !list.is_empty() => {
                        lists.insert(list[0].path.clone(), list);
                    }
                    Ok(_) => {}
                    Err(error) => data.errors.push(error),
                }
            }
            let owned = resolve_owned(&candidates, &data.summary.issue_keys, &key_repos, &lists);
            // Publish ownership/counts before the slower status/base reads. The
            // subtitle must not say "no live checkout" while these are pending.
            data.branches = owned
                .into_iter()
                .map(|mut owned| {
                    owned.gone = !owned.checkout.is_dir();
                    BranchStatus {
                        owned,
                        base: None,
                        status: None,
                        vs_base: None,
                        error: None,
                        stamp: GitdirStamp::default(),
                        checked_at: SystemTime::now(),
                    }
                })
                .collect();
            data.publish_partial(previous, &mut publish);
            for branch in &mut data.branches {
                *branch = git.branch(branch.owned.clone());
            }
        }
        eprintln!(
            "[work-sidebar] finished {} (workspace {id}): {} branches in {:?}",
            config.display(),
            data.branches.len(),
            started.elapsed()
        );
        if !measure_branches {
            data.retain_repos(previous);
        }
        Ok(data)
    }

    fn retain_repos(&mut self, previous: Option<&Self>) {
        if let Some(previous) = previous.filter(|previous| {
            previous.facts.id == self.facts.id && previous.facts.dir == self.facts.dir
        }) {
            // A partial harness/status read cannot remove a repo or its facts.
            // The completed read is authoritative, including removals.
            self.facts.repos = previous.facts.repos.clone();
            self.facts.running_by_repo = previous.facts.running_by_repo.clone();
            self.summary.repos = previous.summary.repos.clone();
            self.branches = previous.branches.clone();
        }
    }

    fn publish_partial(&self, previous: Option<&Self>, publish: &mut impl FnMut(&Self)) {
        let mut partial = self.clone();
        partial.retain_repos(previous);
        publish(&partial);
    }

    pub fn apply(&self, state: &mut gitterm_core::work::state::State) {
        use gitterm_core::work::state::*;
        use std::time::{Instant, SystemTime};
        state.reduce(Intent::SetInputs(self.inputs.clone()));
        let effects = state.reduce(Intent::Open {
            workspace: self.facts.id,
            now: Instant::now(),
            wall_time: SystemTime::now(),
        });
        for effect in effects {
            match effect {
                Effect::BuildSummary { stamp, .. } => {
                    for refresh in state.reduce(Intent::SummaryFinished {
                        stamp,
                        result: Box::new(Ok(self.summary.clone())),
                        now: Instant::now(),
                        visible: true,
                    }) {
                        if let Effect::Refresh { stamp, .. } = refresh {
                            state.reduce(Intent::RefreshFinished {
                                stamp,
                                result: RefreshOutcome {
                                    mode: RefreshMode::Full,
                                    branches: self.branches.clone(),
                                    errors: self.errors.clone(),
                                    git_runs: 0,
                                    skipped_unchanged: 0,
                                    took: Default::default(),
                                },
                                now: Instant::now(),
                                visible: true,
                            });
                        }
                    }
                }
                Effect::ScanDocuments { stamp, .. } => {
                    state.reduce(Intent::DocumentsFinished {
                        stamp,
                        result: Ok(self.documents.clone()),
                        now: Instant::now(),
                    });
                }
                // Index backfill would write Iced data and is never executed.
                _ => {}
            }
        }
    }
}

// Read-only counterpart of Iced chat_facts::store_links; no facts are written.
fn iced_chat_links(
    catalog: &crate::workspace_config::IcedCatalog,
    store: &gitterm_core::tasks::TaskStore,
) -> Vec<gitterm_core::chat_facts::ChildLink> {
    use gitterm_core::{
        chat_facts::{ChatChild, ChildKind, ChildLink},
        tasks::*,
    };
    use std::collections::HashMap;
    let delegations = store.delegations();
    let tasks = store.tasks();
    let mut uid_to_chat: HashMap<String, String> = catalog
        .workspaces
        .iter()
        .flat_map(|w| &w.tabs)
        .filter_map(|t| Some((t.session_uid.clone()?, t.chat_session_id.clone()?)))
        .collect();
    for delegation in delegations {
        if let Some(chat) = &delegation.parent.chat_session_id {
            uid_to_chat
                .entry(delegation.parent.session_uid.clone())
                .or_insert_with(|| chat.clone());
        }
    }
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
        let kind = match delegation.kind {
            DelegationKind::Review => ChildKind::Review,
            DelegationKind::Implement => ChildKind::Worker,
            DelegationKind::Consult => ChildKind::Consult,
        };
        let title = delegation
            .brief
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| {
                let mut title: String = line.chars().take(90).collect();
                if line.chars().count() > 90 {
                    title.push('…');
                }
                title
            });
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

struct WorkGit(tokio::runtime::Runtime);
impl gitterm_core::work::documents::GitDiscovery for WorkGit {
    fn list_worktrees(
        &mut self,
        repo: &std::path::Path,
    ) -> Result<Vec<gitterm_core::work::summary::WorktreeInfo>, String> {
        let mut rows = vec![];
        self.0.block_on(gitterm_core::git_facts::run_git_records(
            repo,
            &[
                "--no-optional-locks",
                "worktree",
                "list",
                "--porcelain",
                "-z",
            ],
            gitterm_core::git_facts::GIT_TIMEOUT,
            b'\0',
            |record| {
                let field = std::str::from_utf8(record)
                    .map_err(|error| format!("read worktrees in {}: {error}", repo.display()))?;
                if let Some(path) = field.strip_prefix("worktree ") {
                    rows.push(gitterm_core::work::summary::WorktreeInfo {
                        path: std::path::Path::new(path)
                            .canonicalize()
                            .unwrap_or_else(|_| path.into()),
                        branch: None,
                    });
                } else if let Some(branch) = field.strip_prefix("branch refs/heads/")
                    && let Some(row) = rows.last_mut()
                {
                    row.branch = Some(branch.into());
                }
                Ok(true)
            },
        ))?;
        Ok(rows)
    }
    fn git_common_dir(&mut self, root: &std::path::Path) -> Result<std::path::PathBuf, String> {
        let path = self.text(
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        Ok(path.trim().into())
    }
}
impl WorkGit {
    fn main_checkout(&self, dir: &std::path::Path) -> std::path::PathBuf {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
        let Some(root) = gitterm_core::workers::checkout_root(&dir) else {
            return dir;
        };
        if root.join(".git").is_dir() {
            return dir;
        }
        match self.text(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ) {
            Ok(common) => {
                let common = std::path::Path::new(common.trim());
                if common.file_name().is_some_and(|name| name == ".git")
                    && let (Some(main), Ok(below)) = (common.parent(), dir.strip_prefix(&root))
                {
                    return main.join(below);
                }
            }
            Err(error) => eprintln!("[work-sidebar] fold repo {}: {error}", dir.display()),
        }
        dir
    }
    fn text(&self, root: &std::path::Path, args: &[&str]) -> Result<String, String> {
        let mut read_args = vec!["--no-optional-locks"];
        read_args.extend_from_slice(args);
        let result = self.0.block_on(gitterm_core::git_facts::git_text(
            root,
            &read_args,
            gitterm_core::git_facts::GIT_TIMEOUT,
        ));
        if let Err(error) = &result {
            eprintln!(
                "[work-sidebar] git {} in {}: {error}",
                args.join(" "),
                root.display()
            );
        }
        result
    }
    fn branch(
        &self,
        owned: gitterm_core::work::summary::OwnedBranch,
    ) -> gitterm_core::work::summary::BranchStatus {
        use gitterm_core::{git_facts::*, work::summary::*};
        let mut errors = vec![];
        let status = if owned.gone {
            None
        } else {
            let mut counts = StatusCounts::default();
            match self.0.block_on(run_git_lines(
                &owned.checkout,
                &[
                    "--no-optional-locks",
                    "status",
                    "--porcelain=v2",
                    "--branch",
                ],
                GIT_TIMEOUT,
                |line| counts.observe(line, STATUS_COUNT_CAP),
            )) {
                Ok(()) => Some(counts),
                Err(error) => {
                    errors.push(error);
                    None
                }
            }
        };
        let mut candidates = base_candidates(
            owned.task.as_ref().map(|t| t.base.as_str()),
            status
                .as_ref()
                .and_then(|s| s.head.as_deref())
                .or(owned.branch.as_deref()),
            status.as_ref().and_then(|s| s.upstream.as_deref()),
        );
        let (base, vs_base) = if owned.gone {
            (None, None)
        } else {
            match self
                .0
                .block_on(measure_bases(&owned.checkout, &mut candidates, &mut 0))
            {
                Ok(measured) => choose_nearest(&candidates, &measured)
                    .map_or((None, None), |(base, counts)| (Some(base), Some(counts))),
                Err(error) => {
                    errors.push(error);
                    (None, None)
                }
            }
        };
        BranchStatus {
            owned,
            base,
            status,
            vs_base,
            error: (!errors.is_empty()).then(|| errors.join("; ")),
            stamp: GitdirStamp::default(),
            checked_at: std::time::SystemTime::now(),
        }
    }
}

#[cfg(test)]
mod work_index_tests {
    use super::IcedWorkData;
    use gitterm_core::{
        chat_facts::{self, ChatFacts, PrRef},
        chats::ChatBackend,
        work::{state::State, summary::Focus},
    };

    #[test]
    fn partial_publications_retain_four_repos_and_facts_until_the_full_read() {
        use gitterm_core::work::summary::{RepoTouched, merge_repo_groups};
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let config = root.path().join("catalog");
        let repos: Vec<_> = ["home", "api", "tools", "server"]
            .map(|name| root.path().join(name))
            .into_iter()
            .collect();
        std::fs::create_dir_all(&config).unwrap();
        for repo in &repos {
            std::fs::create_dir_all(repo).unwrap();
        }
        let write_catalog = |repos: &[std::path::PathBuf]| {
            std::fs::write(
                config.join("workspaces.json"),
                serde_json::to_vec(&serde_json::json!({
                    "active_workspace": 0,
                    "workspaces": [{"name": "Fixture", "abbrev": "F", "color": "blue",
                        "dir": repos[0], "repos": repos, "tabs": []}],
                    "repos": []
                }))
                .unwrap(),
            )
            .unwrap();
        };
        write_catalog(&repos);
        let catalog = crate::workspace_config::IcedCatalog::read(&config).unwrap();
        let saved = &catalog.workspaces[0];
        let mut first =
            IcedWorkData::read_with_home(&config, None, 1, saved, Focus::Latest).unwrap();
        assert_eq!(first.facts.repos.len(), 4);
        // The last completed read's measured facts must survive an incomplete read.
        first.summary.repos = vec![RepoTouched {
            dir: repos[3].clone(),
            edits: 7,
        }];
        first.facts.running_by_repo.insert(repos[3].clone(), 2);
        first.branches.push(crate::work_sidebar::tests::branch(
            repos[3].to_str().unwrap(),
            "feature",
            false,
        ));
        write_catalog(&repos[..3]);
        let mut publications = 0;
        let mut assert_retained = |data: &IcedWorkData| {
            publications += 1;
            assert_eq!(data.facts.repos.len(), 4);
            assert_eq!(data.summary.repos, first.summary.repos);
            assert_eq!(data.facts.running_by_repo, first.facts.running_by_repo);
            let listed = data
                .facts
                .repos
                .iter()
                .map(|repo| repo.dir.clone())
                .collect::<Vec<_>>();
            let groups = merge_repo_groups(
                &listed,
                &data.facts.removed,
                &data.summary.repos,
                &data.branches,
            );
            assert_eq!(groups.len(), 4);
            assert_eq!(groups[3].edits, Some(7));
            assert_eq!(groups[3].rows.len(), 1);
            assert_eq!(groups[3].rows[0].status, first.branches[0].status);
            let mut state = State::default();
            data.apply(&mut state);
            assert_eq!(state.pane(1).unwrap().summary.repos, first.summary.repos);
            assert_eq!(state.pane(1).unwrap().branches.len(), 1);
        };
        let partial = IcedWorkData::read_with_progress(
            &config,
            &super::TaskSource::iced(&config),
            None,
            1,
            saved,
            Focus::Latest,
            Some(&first),
            false,
            &mut assert_retained,
        )
        .unwrap();
        assert_retained(&partial);
        let full = IcedWorkData::read_with_progress(
            &config,
            &super::TaskSource::iced(&config),
            None,
            1,
            saved,
            Focus::Latest,
            Some(&partial),
            true,
            &mut assert_retained,
        )
        .unwrap();
        assert_eq!(
            publications, 4,
            "saved facts, partial result, overview, ownership"
        );
        assert_eq!(full.facts.repos.len(), 3);
        assert!(full.summary.repos.is_empty());
        assert!(full.facts.running_by_repo.is_empty());
        assert!(full.branches.is_empty());
    }

    #[test]
    fn partial_publish_retains_indexed_titles_and_full_read_replaces_the_snapshot() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let config = root.path().join("catalog");
        let home = root.path().join("harness-home");
        let cwd = root.path().join("work");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let owned = ["indexed-root-chat", "never-indexed-chat"];
        std::fs::write(
            config.join("workspaces.json"),
            serde_json::to_vec(&serde_json::json!({
                "active_workspace": 0,
                "workspaces": [{"name": "Fixture", "abbrev": "F", "color": "blue",
                    "dir": cwd, "chats": owned, "tabs": []}],
                "repos": []
            }))
            .unwrap(),
        )
        .unwrap();
        let facts_dir = chat_facts::facts_dir(&config);
        std::fs::create_dir_all(&facts_dir).unwrap();
        for (number, id) in owned.iter().enumerate() {
            let facts = ChatFacts {
                prs: vec![PrRef {
                    repo: "example/project".into(),
                    number: number as u64 + 1,
                    url: format!("https://github.com/example/project/pull/{}", number + 1),
                }],
                ..Default::default()
            };
            std::fs::write(
                chat_facts::facts_path(&facts_dir, ChatBackend::Claude, id).unwrap(),
                serde_json::to_vec(&facts).unwrap(),
            )
            .unwrap();
        }
        let transcript = home.join(".claude/projects/fixture/indexed-root-chat.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            [
                serde_json::json!({"type": "user", "cwd": cwd,
                    "timestamp": "2026-11-20T12:00:00Z", "message": {"content": "Request"}}),
                serde_json::json!({"type": "ai-title", "aiTitle": "Retained title"}),
            ]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();
        let catalog = crate::workspace_config::IcedCatalog::read(&config).unwrap();
        let saved = &catalog.workspaces[0];
        let first =
            IcedWorkData::read_with_home(&config, Some(&home), 1, saved, Focus::Latest).unwrap();
        let mut state = State::default();
        first.apply(&mut state);
        assert_eq!(state.pane(1).unwrap().summary.coordinators.len(), 1);
        assert_eq!(first.indexed_chats.len(), 1);
        assert_eq!(first.pull_requests[0].title, "Retained title");
        assert_eq!(first.pull_requests[1].title, "never-in");
        std::fs::remove_file(transcript).unwrap();
        let mut publications = 0;
        let partial = IcedWorkData::read_with_progress(
            &config,
            &super::TaskSource::iced(&config),
            None,
            1,
            saved,
            Focus::Latest,
            Some(&first),
            false,
            |data| {
                publications += 1;
                data.apply(&mut state);
                let chats = &state.pane(1).unwrap().summary.coordinators;
                assert_eq!(chats.len(), 1);
                assert_eq!(chats[0].id, "indexed-root-chat");
                assert_eq!(chats[0].title, "Retained title");
                assert_eq!(data.pull_requests[0].title, "Retained title");
                assert_eq!(data.pull_requests[1].title, "never-in");
            },
        )
        .unwrap();
        assert_eq!(publications, 1);
        assert_eq!(partial.indexed_chats.len(), 1);
        std::fs::remove_file(
            chat_facts::facts_path(&facts_dir, ChatBackend::Claude, owned[0]).unwrap(),
        )
        .unwrap();
        let full = IcedWorkData::read_with_progress(
            &config,
            &super::TaskSource::iced(&config),
            Some(&home),
            1,
            saved,
            Focus::Latest,
            Some(&partial),
            true,
            |data| {
                data.apply(&mut state);
                assert!(state.pane(1).unwrap().summary.coordinators.is_empty());
                assert_eq!(data.pull_requests.len(), 1);
                assert_eq!(data.pull_requests[0].root, "never-indexed-chat");
            },
        )
        .unwrap();
        full.apply(&mut state);
        assert!(state.pane(1).unwrap().summary.coordinators.is_empty());
        assert!(full.indexed_chats.is_empty());
        assert_eq!(full.pull_requests.len(), 1);
        assert_eq!(full.pull_requests[0].title, "never-in");
    }
}
