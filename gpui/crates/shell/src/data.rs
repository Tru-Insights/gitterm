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
