//! Read-only view facts. Live data reads the application owner on the GPUI thread.
use gpui::App;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceId(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionId(pub u64);

#[derive(Clone, Debug)]
pub struct Workspace {
    pub id: WorkspaceId,
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
    fn workspaces(&self, cx: &App) -> Vec<Workspace>;
    fn sessions(&self, workspace: WorkspaceId, _cx: &App) -> Vec<Session>;
    fn status(&self, workspace: WorkspaceId, cx: &App) -> GitStatus;
    fn files(&self, workspace: WorkspaceId, cx: &App) -> Vec<FileNode>;
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
    fn workspaces(&self, cx: &App) -> Vec<Workspace> {
        self.0
            .read(cx)
            .model
            .workspaces
            .iter()
            .map(|w| Workspace {
                id: w.id,
                name: w
                    .folder
                    .file_name()
                    .unwrap_or(w.folder.as_os_str())
                    .to_string_lossy()
                    .into_owned(),
                status: w.status.branch_name.clone(),
                repositories: vec![w.status.repo_name.clone()],
            })
            .collect()
    }
    fn sessions(&self, workspace: WorkspaceId, _: &App) -> Vec<Session> {
        vec![Session {
            id: SessionId(workspace.0 * 10 + 2),
            title: "Shell".into(),
            kind: SessionKind::Terminal,
        }]
    }
    fn status(&self, workspace: WorkspaceId, cx: &App) -> GitStatus {
        self.0.read(cx).model.status_view(workspace)
    }
    fn files(&self, workspace: WorkspaceId, cx: &App) -> Vec<FileNode> {
        SampleData.files(workspace, cx)
    }
}
