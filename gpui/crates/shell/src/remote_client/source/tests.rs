use super::*;
use crate::{
    WorkspaceOwner,
    data::WorkspaceId,
    workspace_status::{Intent, WorkspaceStatus},
};
use ::core::prelude::v1::test;
use gitterm_core::{
    content::source::{
        SourceDirEntry, SourceDirListing, SourceFileContent, SourceGitFile, SourceGitStatus,
        SourcePath,
    },
    git_diff::{DiffLineKind, FileDiffLine},
    remote::{
        client::RemoteAgentHandshake,
        phone_panel::PairedPhoneInfo,
        ssh_sessions::{RemoteHostConfig, RemoteSessionsSnapshot},
    },
};
use gpui::*;
use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

const ROOT: &str = "/placeholder/remote/project";
#[derive(Default)]
pub(crate) struct FakeSource {
    calls: Mutex<Vec<String>>,
    fail: std::sync::atomic::AtomicBool,
}
impl FakeSource {
    fn record(&self, spec: &Spec, call: String) -> Result<(), String> {
        assert_eq!(spec.remote.id, "placeholder");
        assert_eq!(spec.remote.endpoint, "http://remote.example:7777");
        assert_eq!(spec.workspace_id, "placeholder-workspace");
        assert_eq!(spec.root, ROOT);
        self.calls.lock().unwrap().push(call);
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            Err("placeholder remote unavailable".into())
        } else {
            Ok(())
        }
    }
}
impl RemoteClient for FakeSource {
    fn source_list(
        &self,
        spec: &Spec,
        dir: String,
        hidden: bool,
    ) -> Result<SourceDirListing, String> {
        self.record(spec, format!("list:{dir}:{hidden}"))?;
        let (entries, parent) = if dir == ROOT {
            (
                vec![SourceDirEntry {
                    name: "src".into(),
                    path: SourcePath::Remote(format!("{ROOT}/src")),
                    is_dir: true,
                    is_git_repo: false,
                }],
                None,
            )
        } else {
            assert_eq!(dir, format!("{ROOT}/src"));
            (
                vec![SourceDirEntry {
                    name: "example.rs".into(),
                    path: SourcePath::Remote(format!("{ROOT}/src/example.rs")),
                    is_dir: false,
                    is_git_repo: false,
                }],
                Some(SourcePath::Remote(ROOT.into())),
            )
        };
        Ok(SourceDirListing {
            tab_id: 0,
            current_dir: SourcePath::Remote(dir.clone()),
            display_dir: if dir == ROOT {
                "project/".into()
            } else {
                "project/src/".into()
            },
            parent,
            entries,
            error: None,
        })
    }
    fn source_read(&self, spec: &Spec, path: String) -> Result<SourceFileContent, String> {
        self.record(spec, format!("read:{path}"))?;
        assert_eq!(path, format!("{ROOT}/src/example.rs"));
        let data = b"fn remote_example() {}\n".to_vec();
        Ok(SourceFileContent {
            total_size: data.len() as u64,
            data,
            truncated: false,
        })
    }
    fn source_status(&self, spec: &Spec) -> Result<SourceGitStatus, String> {
        self.record(spec, "status".into())?;
        let file = |staged| SourceGitFile {
            path: "src/example.rs".into(),
            status: "M".into(),
            is_staged: staged,
        };
        Ok(SourceGitStatus {
            root: SourcePath::Remote(ROOT.into()),
            is_git_repo: true,
            repo_name: "project".into(),
            branch_name: "placeholder-branch".into(),
            staged: vec![file(true)],
            unstaged: vec![file(false)],
            untracked: vec![SourceGitFile {
                path: "new.txt".into(),
                status: "?".into(),
                is_staged: false,
            }],
        })
    }
    fn source_diff(
        &self,
        spec: &Spec,
        path: String,
        staged: bool,
    ) -> Result<Vec<FileDiffLine>, String> {
        self.record(spec, format!("diff:{path}:{staged}"))?;
        Ok(vec![
            FileDiffLine {
                content: "@@ -0,0 +1,1 @@".into(),
                kind: DiffLineKind::Header,
                old_line: None,
                new_line: None,
            },
            FileDiffLine {
                content: format!("remote {path} staged={staged}"),
                kind: DiffLineKind::Addition,
                old_line: None,
                new_line: Some(1),
            },
        ])
    }
    fn handshake(&self, _: &RemoteAgentConfig) -> Result<RemoteAgentHandshake, String> {
        panic!("unexpected handshake")
    }
    fn list_dir(&self, _: &RemoteAgentConfig, _: String) -> SourceDirListing {
        panic!("picker request used for workspace")
    }
    fn list_chats(
        &self,
        _: &RemoteAgentConfig,
    ) -> Result<Vec<gitterm_core::chats::ChatIndexEntry>, String> {
        panic!("unexpected chats")
    }
    fn list_sessions(&self, _: &RemoteHostConfig) -> RemoteSessionsSnapshot {
        panic!("unexpected SSH")
    }
    fn list_phones(&self, _: &RemoteAgentConfig) -> Result<Vec<PairedPhoneInfo>, String> {
        panic!("unexpected phones")
    }
}
pub(crate) fn fixture(
    cx: &mut TestAppContext,
    live: bool,
) -> (tempfile::TempDir, Entity<WorkspaceOwner>, Arc<FakeSource>) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("settings.json"), b"{}").unwrap();
    let document = serde_json::json!({"active_workspace":0,"remote_agents":[{"id":"placeholder","name":"Placeholder Mac","endpoint":"http://remote.example:7777","auth":{"type":"token","token_ref":"env:PLACEHOLDER_UNUSED"}}],"workspaces":[{"name":"Placeholder","abbrev":"PH","dir":ROOT,"color":"blue","location":{"kind":"remote_agent","remote_id":"placeholder","workspace_id":"placeholder-workspace","root":ROOT},"tabs":[{"dir":ROOT,"session_uid":"placeholder-tab"}]}]});
    std::fs::write(
        root.path().join("workspaces.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    let file = serde_json::from_value(document).unwrap();
    let catalog = crate::workspace_config::IcedCatalog::from_document(root.path(), file);
    let model = WorkspaceStatus::from_iced(&catalog);
    let mut store = crate::workspace_config::WorkspaceStore::new(root.path());
    store.iced = Some(catalog);
    if live {
        let services = gitterm_core::preview::service::ServiceContext::production(
            root.path(),
            "placeholder-source-test",
        )
        .unwrap();
        cx.update(|cx| crate::chat::policy::install_live(services, cx).unwrap());
    }
    let fake = Arc::new(FakeSource::default());
    let owner = cx.new(|cx| {
        let mut owner = WorkspaceOwner::new(model, Some(store), cx);
        owner.source_client = fake.clone();
        owner
    });
    cx.background_executor.allow_parking();
    (root, owner, fake)
}
pub(crate) fn wait(cx: &mut TestAppContext, mut done: impl FnMut(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if cx.read(&mut done) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "remote source completion timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
struct TestView;
impl Render for TestView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

#[gpui::test]
fn remote_parity_live_directory_listing_and_file_preview(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let (_root, owner, fake) = fixture(cx, true);
    let workspace = WorkspaceId(1);
    owner.update(cx, |o, cx| {
        o.dispatch(Intent::Activate(workspace), Instant::now(), cx)
    });
    wait(cx, |cx| {
        owner.read(cx).files[&workspace]
            .rows()
            .iter()
            .any(|row| matches!(row, crate::files::Row::Entry {entry, ..} if entry.name == "src"))
    });
    owner.update(cx, |o, cx| {
        o.files
            .get_mut(&workspace)
            .unwrap()
            .set_root(format!("{ROOT}/src").into());
        o.toggle_directory(workspace, format!("{ROOT}/src").into(), cx);
    });
    wait(cx, |cx| {
        owner.read(cx).files[&workspace].rows().iter().any(
            |row| matches!(row, crate::files::Row::Entry {entry, ..} if entry.name == "example.rs"),
        )
    });
    owner.read_with(cx, |o, _| {
        let navigation = o.files[&workspace].remote_navigation().unwrap();
        assert_eq!(navigation.current, Path::new(&format!("{ROOT}/src")));
        assert_eq!(navigation.parent.as_deref(), Some(Path::new(ROOT)));
        assert_eq!(navigation.display, "project/src/");
    });
    let source = owner.read_with(cx, |o, cx| o.remote_source(workspace, cx).unwrap().unwrap());
    cx.update(gpui_component::init);
    let (_, vcx) = cx.add_window_view(|_, _| TestView);
    let content = vcx
        .update(|window, cx| cx.new(|cx| crate::content::window::ContentWindow::new(window, cx)));
    vcx.update(|window, cx| {
        content.update(cx, |content, cx| {
            content.open(
                "remote".into(),
                crate::content::window::ContentRequest {
                    remote: Some(source),
                    root: ROOT.into(),
                    relative: "src/example.rs".into(),
                    workspace: 1,
                    pane: 7,
                    dark: true,
                },
                window,
                cx,
            );
            content.select(Some("remote".into()));
        })
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        vcx.executor().advance_clock(Duration::from_millis(40));
        vcx.run_until_parked();
        if vcx.read(|cx| content.read(cx).document("remote").is_some()) {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    vcx.read(|cx| {
        assert!(
            content
                .read(cx)
                .document("remote")
                .unwrap()
                .html()
                .contains("remote_example")
        )
    });
    vcx.read(|cx| {
        let content = content.read(cx);
        assert_eq!(
            content.source("remote"),
            Some((ROOT.into(), "src/example.rs".into()))
        );
        assert!(
            content
                .remote_html(Path::new(ROOT), Path::new("src/example.rs"))
                .unwrap()
                .unwrap()
                .contains("remote_example")
        );
        assert!(
            content
                .displayed_text(Path::new(ROOT), Path::new("src/example.rs"))
                .unwrap()
                .contains("remote_example")
        );
    });
    assert!(
        fake.calls
            .lock()
            .unwrap()
            .contains(&format!("read:{ROOT}/src/example.rs"))
    );
}

#[gpui::test]
fn remote_parity_live_status_and_file_diffs(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let (_root, owner, fake) = fixture(cx, true);
    let workspace = WorkspaceId(1);
    owner.update(cx, |o, cx| {
        o.dispatch(Intent::Activate(workspace), Instant::now(), cx);
        o.dispatch(Intent::Opened, Instant::now(), cx);
    });
    wait(cx, |cx| owner.read(cx).model.status_view(workspace).checked);
    owner.read_with(cx, |o, _| {
        let status = o.model.status_view(workspace);
        assert_eq!(status.branch, "placeholder-branch");
        assert_eq!(status.files.len(), 3);
        assert!(status.error.is_none());
    });
    cx.update(gpui_component::init);
    let (review, vcx) = cx.add_window_view(|window, cx| {
        gitterm_gpui_review::ReviewView::new(Default::default(), window, cx)
    });
    vcx.update(|_, cx| {
        owner.update(cx, |o, cx| {
            o.reviews.insert(workspace, review.clone());
            o.set_review_visible(workspace, true, cx);
        })
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        vcx.run_until_parked();
        if vcx.read(|cx| !review.read(cx).is_loading()) {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    vcx.read(|cx| {
        let model = review.read(cx).model();
        assert!(model.error.is_none(), "{:?}", model.error);
        assert_eq!(model.files.len(), 3);
        for file in &model.files {
            let parsed = gpui_component::diff::DiffFile::parse(&file.patch).unwrap();
            assert_eq!(parsed.len(), 1);
            assert_eq!(parsed[0].path().as_str(), file.key.1);
        }
        assert!(
            model.files[0]
                .patch
                .contains("+remote src/example.rs staged=true")
        );
        assert!(
            model.files[1]
                .patch
                .contains("+remote src/example.rs staged=false")
        );
        assert!(
            model.files[2]
                .patch
                .contains("+remote new.txt staged=false")
        );
    });
    assert!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "status")
    );
}

#[gpui::test]
fn remote_parity_preview_refuses_files_status_and_diff(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let (_root, owner, fake) = fixture(cx, false);
    let workspace = WorkspaceId(1);
    owner.read_with(cx, |o, cx| {
        assert!(
            o.remote_source(workspace, cx)
                .err()
                .unwrap()
                .contains("not available in preview")
        )
    });
    owner.update(cx, |o, cx| {
        o.dispatch(Intent::Activate(workspace), Instant::now(), cx);
        o.dispatch(Intent::Opened, Instant::now(), cx);
    });
    wait(cx, |cx| {
        owner.read(cx).model.status_view(workspace).checked && owner.read(cx).files[&workspace].rows().iter().any(|row| matches!(row, crate::files::Row::Error {message, ..} if message.contains("not available in preview")))
    });
    owner.read_with(cx, |o, _| {
        assert!(
            o.model
                .status_view(workspace)
                .error
                .unwrap()
                .contains("not available in preview")
        )
    });
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[test]
fn remote_parity_capped_untracked_preview_is_a_renderable_patch() {
    let line = |content: &str, kind| FileDiffLine {
        content: content.into(),
        kind,
        old_line: None,
        new_line: None,
    };
    let path = "placeholder \"file\"\t.rs";
    let patch = remote_patch(
        path,
        &[
            line("@@ -0,0 +1,9000 @@ (new file)", DiffLineKind::Header),
            line("placeholder", DiffLineKind::Addition),
            line(
                "... truncated to first 3000 lines (9000 total)",
                DiffLineKind::Header,
            ),
        ],
    );
    let parsed = gpui_component::diff::DiffFile::parse(&patch).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].path().as_str(), path);
    assert!(patch.contains("... truncated to first 3000 lines (9000 total)"));
}

#[gpui::test]
fn remote_parity_live_failures_surface_without_local_fallback(cx: &mut TestAppContext) {
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let (_root, owner, fake) = fixture(cx, true);
    fake.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let workspace = WorkspaceId(1);
    owner.update(cx, |owner, cx| {
        owner.dispatch(Intent::Activate(workspace), Instant::now(), cx);
        owner.dispatch(Intent::Opened, Instant::now(), cx);
    });
    wait(cx, |cx| {
        owner.read(cx).model.status_view(workspace).checked
        && owner.read(cx).files[&workspace].rows().iter().any(|row| matches!(row, crate::files::Row::Error { message, .. } if message.contains("placeholder remote unavailable")))
    });
    let source = owner.read_with(cx, |owner, cx| {
        assert_eq!(
            owner.model.status_view(workspace).error.as_deref(),
            Some("placeholder remote unavailable")
        );
        owner.remote_source(workspace, cx).unwrap().unwrap()
    });
    assert!(
        crate::content::Document::remote_file(
            &source,
            Path::new(&format!("{ROOT}/src/example.rs")),
            true,
            14.
        )
        .unwrap()
        .html()
        .contains("Could not load remote file: placeholder remote unavailable")
    );
    assert_eq!(
        source.review(1).unwrap_err(),
        "placeholder remote unavailable"
    );
}
