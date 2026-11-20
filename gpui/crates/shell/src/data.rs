//! Read-only view facts. Live data reads the application owner on the GPUI thread.
use gpui::App;
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
    pub repo_recency: Vec<(std::path::PathBuf, std::time::SystemTime)>,
    pub pull_requests: Vec<(String, Vec<gitterm_core::prs::WatchedPr>)>,
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
        id: u64,
        saved: &gitterm_core::workspace_config::WorkspaceConfig,
        focus: gitterm_core::work::summary::Focus,
        mut publish: impl FnMut(&Self),
    ) -> anyhow::Result<Self> {
        #[cfg(test)]
        let home: Option<std::path::PathBuf> = None;
        #[cfg(not(test))]
        let home = dirs::home_dir();
        // Saved facts can populate Work independently of a slow full harness
        // index. This is a fresh read, never a second persisted index/cache.
        if home.is_some() {
            Self::read_with_progress(config, None, id, saved, focus.clone(), false, &mut publish)?;
        }
        Self::read_with_progress(config, home.as_deref(), id, saved, focus, true, publish)
    }
    #[cfg(test)]
    pub(crate) fn read_with_home(
        config: &std::path::Path,
        home: Option<&std::path::Path>,
        id: u64,
        saved: &gitterm_core::workspace_config::WorkspaceConfig,
        focus: gitterm_core::work::summary::Focus,
    ) -> anyhow::Result<Self> {
        Self::read_with_progress(config, home, id, saved, focus, true, |_| {})
    }
    pub(crate) fn read_with_progress(
        config: &std::path::Path,
        home: Option<&std::path::Path>,
        id: u64,
        saved: &gitterm_core::workspace_config::WorkspaceConfig,
        focus: gitterm_core::work::summary::Focus,
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
        let store = TaskStore::load(TaskStore::path_for_config_root(config))?;
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
                .unwrap_or_default()
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
        // A partial index must not erase owned chats whose transcript is absent.
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
                let path = match gitterm_core::chat_facts::facts_path(
                    &gitterm_core::chat_facts::facts_dir(config),
                    chats::ChatBackend::Claude,
                    &chat,
                ) {
                    Ok(path) => path,
                    Err(error) => {
                        errors.push(error);
                        continue;
                    }
                };
                let chat_facts = match gitterm_core::chat_facts::load_facts(&path) {
                    Ok(facts) => facts,
                    Err(error) => {
                        errors.push(error);
                        None
                    }
                };
                if !indexed && let Some(facts) = &chat_facts {
                    pending.extend(facts.children.iter().map(|c| {
                        (
                            c.session_id.clone(),
                            c.title.clone().unwrap_or_else(|| c.session_id.clone()),
                        )
                    }));
                }
                if entries.iter().any(|entry| entry.id == chat) {
                    continue;
                }
                entries.push(chats::ChatIndexEntry {
                    id: chat,
                    backend: chats::ChatBackend::Claude,
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
                    facts: chat_facts,
                });
            }
        }
        gitterm_core::chat_facts::resolve_tree(&mut entries, &links);
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
        let task_worktrees = store
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
                let common =
                    gitterm_core::work::documents::GitDiscovery::git_common_dir(&mut git, &path)
                        .ok();
                let running = store
                    .tasks()
                    .iter()
                    .filter(|t| {
                        t.lifecycle == TaskLifecycle::Running
                            && common
                                .as_ref()
                                .is_some_and(|common| equal(&t.repository.common_dir, common))
                    })
                    .count();
                if running > 0 {
                    facts.running_by_repo.insert(path, running);
                }
            }
        } else {
            errors.push("The overview reads this Mac's chats and checkouts; this workspace is on another machine.".into());
        }
        let work_entries = match &source {
            Source::Owned(roots) => chats_under(&entries, roots),
            Source::Folder(dir) => work_chats(&entries, dir),
        };
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
        let pull_requests = summary
            .coordinators
            .iter()
            .filter(|root| summary.focused.contains(&root.id))
            .filter_map(|root| {
                let mut keys: Vec<_> = plans
                    .get(&root.id)
                    .into_iter()
                    .flat_map(|plan| &plan.entries)
                    .map(|entry| entry.key())
                    .collect();
                let roots = vec![root.id.clone()];
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
                (!prs.is_empty()).then(|| (root.title.clone(), prs))
            })
            .collect();
        let issues = gitterm_core::issues::IssueTitlesFile::load(
            &gitterm_core::issues::IssueTitlesFile::path_in(config),
        );
        let mut data = Self {
            facts,
            tasks,
            workers,
            repo_recency,
            pull_requests,
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
        publish(&data);
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
            for mut owned in
                resolve_owned(&candidates, &data.summary.issue_keys, &key_repos, &lists)
            {
                owned.gone = !owned.checkout.is_dir();
                data.branches.push(git.branch(owned));
            }
        }
        eprintln!(
            "[work-sidebar] finished {} (workspace {id}): {} branches in {:?}",
            config.display(),
            data.branches.len(),
            started.elapsed()
        );
        Ok(data)
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
        let text = self.text(repo, &["worktree", "list", "--porcelain", "-z"])?;
        let mut rows = vec![];
        for field in text.split('\0') {
            if let Some(path) = field.strip_prefix("worktree ") {
                rows.push(gitterm_core::work::summary::WorktreeInfo {
                    path: path.into(),
                    branch: None,
                });
            } else if let Some(branch) = field.strip_prefix("branch refs/heads/")
                && let Some(row) = rows.last_mut()
            {
                row.branch = Some(branch.into());
            }
        }
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
