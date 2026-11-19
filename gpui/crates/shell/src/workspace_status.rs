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
    pub folder: PathBuf,
    pub root: PathBuf,
    pub status: GitStatusState,
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
            root: folder.clone(),
            folder,
            status: GitStatusState::new(name, false, now),
        });
        id
    }
    pub fn active(&self) -> Option<WorkspaceId> {
        self.selection.active().map(WorkspaceId)
    }
    pub fn folders(&self) -> Vec<PathBuf> {
        self.workspaces.iter().map(|w| w.folder.clone()).collect()
    }
    pub fn status_view(&self, id: WorkspaceId) -> GitStatus {
        let status = &self
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .expect("owned workspace")
            .status;
        GitStatus {
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
                // Plain local roots may adopt the collector's discovered toplevel;
                // the explicitly seeded folder remains the persisted identity.
                if snapshot.error.is_none() && workspace.root != snapshot.repo_path {
                    workspace.root = snapshot.repo_path.clone();
                    workspace.status.reduce(StatusEvent::Reset, now);
                }
                workspace
                    .status
                    .reduce(StatusEvent::Completed(snapshot), now);
                vec![]
            }
            Intent::ContextChanged { workspace, root } => {
                let Some(workspace) = self.workspaces.iter_mut().find(|w| w.id == workspace) else {
                    return vec![];
                };
                workspace.root = root.clone();
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
