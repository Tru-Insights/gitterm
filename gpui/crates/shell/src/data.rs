//! Frontend-neutral sample port. The later core adapter supplies these facts;
//! entities, docking, selection and drafts remain owned by the GPUI shell.
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
}
#[derive(Clone, Debug)]
pub struct FileNode {
    pub name: String,
    pub children: Vec<FileNode>,
}

pub trait ShellData {
    fn workspaces(&self) -> Vec<Workspace>;
    fn sessions(&self, workspace: WorkspaceId) -> Vec<Session>;
    fn status(&self, workspace: WorkspaceId) -> GitStatus;
    fn files(&self, workspace: WorkspaceId) -> Vec<FileNode>;
}

pub struct SampleData;
impl ShellData for SampleData {
    fn workspaces(&self) -> Vec<Workspace> {
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
    fn sessions(&self, workspace: WorkspaceId) -> Vec<Session> {
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
    fn status(&self, workspace: WorkspaceId) -> GitStatus {
        let repository = self
            .workspaces()
            .into_iter()
            .find(|w| w.id == workspace)
            .expect("sample workspace exists")
            .name;
        GitStatus {
            repository,
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
    fn files(&self, _: WorkspaceId) -> Vec<FileNode> {
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
