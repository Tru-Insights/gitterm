//! GPUI intents resolve identities here; all selection, cadence and completion
//! policy remains in gitterm-core. No pane owns this model or its jobs.
use crate::data::{ChangedFile, GitStatus, WorkspaceId};
use gitterm_core::{
    status::{GitStatusSnapshot, GitStatusState, RequestPrelude, StatusCommand, StatusEvent},
    workspace::{SelectionCommand, SelectionEvent, SelectionState, WorkspaceFact},
};
use std::{path::PathBuf, time::Instant};

pub struct LocalWorkspace {
    pub id: WorkspaceId,
    pub name: String,
    pub folder: PathBuf,
    pub root: PathBuf,
    pub status: GitStatusState,
    /// False until the first status result arrives. Only the active workspace
    /// is polled (as in Iced), so others show placeholders until visited.
    pub checked: bool,
    pub(crate) remote: bool,
}
#[derive(Debug)]
pub enum Intent {
    SelectIndex(usize),
    Activate(WorkspaceId),
    Opened,
    EnterGit,
    Maintenance {
        focused: bool,
    },
    Wake,
    ContextChanged {
        workspace: WorkspaceId,
        root: PathBuf,
    },
    SelectSession {
        workspace: WorkspaceId,
        session: crate::data::SessionId,
        root: PathBuf,
    },
    Completed(GitStatusSnapshot),
}
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Selection(SelectionCommand),
    Status {
        owner: usize,
        command: StatusCommand,
    },
}
#[derive(Default)]
pub struct WorkspaceStatus {
    pub selection: SelectionState,
    pub workspaces: Vec<LocalWorkspace>,
    pub usage_open: bool,
    // Samples use the same selection reducer without creating status owners.
    sample_roster: Vec<WorkspaceFact>,
    next_id: u64,
    /// A completion from a context we left cannot re-root the current tab.
    context_roots: std::collections::HashMap<WorkspaceId, std::collections::HashSet<PathBuf>>,
    session_status: std::collections::HashMap<crate::data::SessionId, (GitStatusState, bool)>,
    pub(crate) active_sessions: std::collections::HashMap<WorkspaceId, crate::data::SessionId>,
    pub(crate) session_contexts:
        std::collections::HashMap<crate::data::SessionId, crate::data::SessionContext>,
}
impl WorkspaceStatus {
    pub fn sample(ids: impl Iterator<Item = WorkspaceId>) -> Self {
        Self {
            sample_roster: ids
                .map(|id| WorkspaceFact {
                    id: id.0,
                    parked: false,
                })
                .collect(),
            ..Self::default()
        }
    }
    pub fn add(&mut self, folder: PathBuf, now: Instant) -> WorkspaceId {
        if let Some(workspace) = self.workspaces.iter().find(|w| w.folder == folder) {
            return workspace.id;
        }
        self.next_id += 1;
        let id = WorkspaceId(self.next_id);
        let name = folder
            .file_name()
            .unwrap_or(folder.as_os_str())
            .to_string_lossy()
            .into_owned();
        self.workspaces.push(LocalWorkspace {
            id,
            name: name.clone(),
            root: folder.clone(),
            folder,
            status: GitStatusState::new(name, false, now),
            checked: false,
            remote: false,
        });
        id
    }
    pub(crate) fn from_iced(catalog: &crate::workspace_config::IcedCatalog) -> Self {
        let now = Instant::now();
        let mut model = Self::default();
        for (index, saved) in catalog.workspaces.iter().enumerate() {
            let folder = PathBuf::from(&saved.dir);
            let workspace = WorkspaceId(index as u64 + 1);
            let sessions = crate::data::iced_sessions_with_key(
                saved,
                &crate::workspace_config::iced_layout_key(saved),
            );
            for (session, tab) in sessions.iter().zip(&saved.tabs) {
                model.session_contexts.insert(
                    session.id,
                    crate::data::SessionContext {
                        cwd: tab.dir.clone().into(),
                        terminal_dir: Some(tab.dir.clone().into()),
                        repo: tab.repo_dir.as_ref().unwrap_or(&tab.dir).into(),
                        task_root: None,
                        task_context: tab.task_id.is_some(),
                        panel_repo: None,
                        follow: Default::default(),
                        observed_events: 0,
                        follow_listing: false,
                        follow_pending: Vec::new(),
                    },
                );
            }
            let active = sessions.get(crate::data::iced_active_tab(saved));
            if let Some(active) = active {
                model.active_sessions.insert(workspace, active.id);
            }
            let root = active
                .and_then(|session| model.session_contexts.get(&session.id))
                .map(|tab| tab.panel_root())
                .unwrap_or_else(|| folder.clone());
            model.workspaces.push(LocalWorkspace {
                id: WorkspaceId(index as u64 + 1),
                name: saved.name.clone(),
                root,
                folder,
                status: GitStatusState::new(saved.name.clone(), false, now),
                checked: false,
                remote: matches!(
                    saved.location,
                    Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })
                ) || saved.is_legacy_remote(),
            });
        }
        model.next_id = model.workspaces.len() as u64;
        if let Some(workspace) = model.workspaces.get(catalog.active) {
            model.dispatch(Intent::Activate(workspace.id), now);
        }
        model
    }
    pub fn active(&self) -> Option<WorkspaceId> {
        self.selection.active().map(WorkspaceId)
    }
    pub fn folders(&self) -> Vec<PathBuf> {
        self.workspaces.iter().map(|w| w.folder.clone()).collect()
    }
    pub fn status_view(&self, id: WorkspaceId) -> GitStatus {
        let workspace = self
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .expect("owned workspace");
        let status = &workspace.status;
        GitStatus {
            checked: workspace.checked,
            repository: status.repo_name.clone(),
            branch: status.branch_name.clone(),
            loading: status.git_status_loading,
            error: status.git_status_error.clone(),
            is_git_repo: status.is_git_repo,
            files: [&status.staged, &status.unstaged, &status.untracked]
                .into_iter()
                .flatten()
                .map(|f| ChangedFile {
                    path: f.path.clone(),
                    status: if f.status == "?" {
                        "??".into()
                    } else {
                        f.status.clone()
                    },
                    staged: f.is_staged,
                })
                .collect(),
        }
    }
    pub fn dispatch(&mut self, intent: Intent, now: Instant) -> Vec<Command> {
        match intent {
            Intent::SelectIndex(_) | Intent::Activate(_) => {
                let facts: Vec<_> = if self.sample_roster.is_empty() {
                    self.workspaces
                        .iter()
                        .map(|w| WorkspaceFact {
                            id: w.id.0,
                            parked: false,
                        })
                        .collect()
                } else {
                    self.sample_roster.clone()
                };
                let event = match intent {
                    Intent::SelectIndex(index) => {
                        SelectionEvent::Select(facts.get(index).map(|w| w.id))
                    }
                    Intent::Activate(id) => SelectionEvent::Activate(id.0),
                    _ => unreachable!(),
                };
                let commands = self.selection.reduce(event, &facts, self.usage_open, now);
                for command in &commands {
                    if let SelectionCommand::SetUsageOpen(open) = command {
                        self.usage_open = *open;
                    }
                }
                commands.into_iter().map(Command::Selection).collect()
            }
            Intent::Completed(snapshot) => {
                let Some(workspace) = self
                    .workspaces
                    .iter_mut()
                    .find(|w| w.id.0 as usize == snapshot.tab_id)
                else {
                    return vec![];
                };
                if workspace.root != snapshot.repo_path
                    && self
                        .context_roots
                        .get(&workspace.id)
                        .is_some_and(|roots| roots.contains(&snapshot.repo_path))
                {
                    return vec![];
                }
                // Plain local roots may adopt the collector's discovered toplevel;
                // the explicitly seeded folder remains the persisted identity.
                if snapshot.error.is_none() && workspace.root != snapshot.repo_path {
                    workspace.root = snapshot.repo_path.clone();
                    workspace.status.reduce(StatusEvent::Reset, now);
                }
                workspace.checked = true;
                workspace
                    .status
                    .reduce(StatusEvent::Completed(snapshot), now);
                vec![]
            }
            Intent::SelectSession {
                workspace,
                session,
                root,
            } => {
                let previous = self.active_sessions.insert(workspace, session);
                let Some(local) = self
                    .workspaces
                    .iter_mut()
                    .find(|local| local.id == workspace)
                else {
                    return vec![];
                };
                let mut first = false;
                if previous != Some(session) {
                    if let Some(previous) = previous {
                        self.session_status
                            .insert(previous, (local.status.clone(), local.checked));
                    }
                    if let Some((status, checked)) = self.session_status.get(&session) {
                        local.status = status.clone();
                        local.checked = *checked;
                    } else {
                        local.status = GitStatusState::new(local.name.clone(), false, now);
                        local.checked = false;
                        first = true;
                    }
                }
                let changed = local.root != root;
                if first && !changed {
                    return Self::status_commands(
                        local,
                        StatusEvent::Request {
                            root,
                            prelude: RequestPrelude::TabOpened,
                        },
                        now,
                    );
                }
                self.dispatch(Intent::ContextChanged { workspace, root }, now)
            }
            Intent::ContextChanged { workspace, root } => {
                let Some(workspace) = self.workspaces.iter_mut().find(|w| w.id == workspace) else {
                    return vec![];
                };
                if workspace.root == root {
                    return vec![];
                }
                self.context_roots
                    .entry(workspace.id)
                    .or_default()
                    .extend([workspace.root.clone(), root.clone()]);
                workspace.root = root.clone();
                workspace.checked = false;
                workspace.status.reduce(StatusEvent::Reset, now);
                Self::status_commands(
                    workspace,
                    StatusEvent::Request {
                        root,
                        prelude: RequestPrelude::Refresh,
                    },
                    now,
                )
            }
            Intent::Wake => {
                for workspace in &mut self.workspaces {
                    workspace.status.reduce(StatusEvent::Wake, now);
                }
                vec![]
            }
            intent => {
                let active = self.active();
                let Some(workspace) = self.workspaces.iter_mut().find(|w| Some(w.id) == active)
                else {
                    return vec![];
                };
                let root = workspace.root.clone();
                let event = match intent {
                    Intent::Opened => StatusEvent::Request {
                        root,
                        prelude: RequestPrelude::TabOpened,
                    },
                    Intent::EnterGit => StatusEvent::Request {
                        root,
                        prelude: RequestPrelude::Refresh,
                    },
                    Intent::Maintenance { focused } => {
                        StatusEvent::VisibilityChanged { focused, root }
                    }
                    _ => unreachable!(),
                };
                Self::status_commands(workspace, event, now)
            }
        }
    }
    fn status_commands(
        workspace: &mut LocalWorkspace,
        event: StatusEvent,
        now: Instant,
    ) -> Vec<Command> {
        if workspace.remote {
            return vec![];
        }
        workspace
            .status
            .reduce(event, now)
            .into_iter()
            .map(|command| Command::Status {
                owner: workspace.id.0 as usize,
                command,
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "workspace_status_tests.rs"]
mod tests;

#[cfg(test)]
mod tab_context_tests {
    use super::*;

    #[test]
    fn tab_context_gap2_late_status_cannot_restore_the_previous_tabs_checkout() {
        let now = Instant::now();
        let mut model = WorkspaceStatus::default();
        let workspace = model.add("/tmp/fixture-main".into(), now);
        model.dispatch(
            Intent::ContextChanged {
                workspace,
                root: "/tmp/fixture-task".into(),
            },
            now,
        );
        model.dispatch(
            Intent::Completed(GitStatusSnapshot {
                tab_id: workspace.0 as usize,
                repo_path: "/tmp/fixture-main".into(),
                repo_name: "main".into(),
                branch_name: "main".into(),
                is_git_repo: true,
                staged: vec![],
                unstaged: vec![],
                untracked: vec![],
                error: None,
            }),
            now,
        );
        assert_eq!(model.workspaces[0].root, PathBuf::from("/tmp/fixture-task"));
        assert!(!model.workspaces[0].checked);
        assert!(
            model
                .dispatch(
                    Intent::ContextChanged {
                        workspace,
                        root: "/tmp/fixture-task".into()
                    },
                    now
                )
                .is_empty(),
            "ordinary same-root tab switches retain the poll cadence"
        );
    }
    #[test]
    fn tab_context_gap2_git_status_and_poll_clock_return_with_each_session() {
        let now = Instant::now();
        let mut model = WorkspaceStatus::default();
        let workspace = model.add("/tmp/fixture-main".into(), now);
        let first = crate::data::SessionId(41);
        let second = crate::data::SessionId(42);
        let first_root = PathBuf::from("/tmp/fixture-main");
        let second_root = PathBuf::from("/tmp/fixture-task");
        model.dispatch(
            Intent::SelectSession {
                workspace,
                session: first,
                root: first_root.clone(),
            },
            now,
        );
        model.workspaces[0].status.branch_name = "first".into();
        model.workspaces[0].checked = true;
        model.workspaces[0].status.git_poll_interval_ms = 8000;
        model.dispatch(
            Intent::SelectSession {
                workspace,
                session: second,
                root: second_root.clone(),
            },
            now,
        );
        assert!(!model.workspaces[0].checked);
        model.workspaces[0].status.branch_name = "second".into();
        model.workspaces[0].checked = true;
        model.dispatch(
            Intent::SelectSession {
                workspace,
                session: first,
                root: first_root,
            },
            now,
        );
        assert_eq!(model.workspaces[0].status.branch_name, "first");
        // Cross-root switches reset cadence, as Iced refresh_panels_for_context does.
        assert_eq!(
            model.workspaces[0].status.git_poll_interval_ms,
            GitStatusState::new("fixture".into(), false, now).git_poll_interval_ms
        );
        model.dispatch(
            Intent::SelectSession {
                workspace,
                session: second,
                root: second_root,
            },
            now,
        );
        assert_eq!(model.workspaces[0].status.branch_name, "second");
    }
}
