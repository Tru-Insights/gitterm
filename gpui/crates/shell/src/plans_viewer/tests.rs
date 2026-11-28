use super::{DocumentSource, Request, SURFACE, content, plans};
use crate::WorkspaceId;
use gpui::TestAppContext;
use std::path::Path;

fn data(document: &content::Document) -> serde_json::Value {
    let text = document
        .html()
        .split("type=\"application/json\">")
        .nth(1)
        .unwrap()
        .split("</script>")
        .next()
        .unwrap();
    serde_json::from_str(text).unwrap()
}

fn request(home: &Path) -> Request {
    Request {
        workspace: WorkspaceId(1),
        home: home.into(),
        source: DocumentSource::Plans,
        name: Some("guide.md".into()),
        dark: true,
    }
}

#[::core::prelude::v1::test]
fn plans_viewer_same_documents_list_as_iced_placeholder_workspace() {
    let temp = tempfile::tempdir().unwrap();
    for dir in [
        ".plans/nested",
        ".plans/.hidden",
        ".plans/_archive",
        "docs",
        "Work/docs",
    ] {
        std::fs::create_dir_all(temp.path().join(dir)).unwrap();
    }
    for (path, text) in [
        (".plans/guide.md", "# Zebra"),
        (".plans/nested/second.md", "# Alpha"),
        (".plans/_archive/old.md", "# Old"),
        (".plans/.hidden/private.md", "# Hidden"),
        (".plans/.secret.md", "# Secret"),
        (".plans/no.MD", "# Upper"),
        (".plans/no.txt", "# Text"),
        ("docs/guide.md", "# Docs"),
        ("Work/docs/extra.md", "# Outside Iced routes"),
    ] {
        std::fs::write(temp.path().join(path), text).unwrap();
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        temp.path().join("docs/guide.md"),
        temp.path().join(".plans/link.md"),
    )
    .unwrap();
    let mut req = request(temp.path());
    let payload = data(&req.document());
    assert_eq!(
        payload["plans"],
        serde_json::json!([
            {"name":"nested/second.md", "title":"Alpha"}, {"name":"guide.md", "title":"Zebra"}
        ])
    );
    let iced = plans::list_documents(&req.root());
    assert_eq!(payload["plans"], serde_json::to_value(iced.plans).unwrap());
    assert_eq!(payload["markdown"], "# Zebra");
    req.source = DocumentSource::Docs;
    let payload = data(&req.document());
    assert_eq!(
        payload["plans"],
        serde_json::json!([{"name":"guide.md", "title":"Docs"}])
    );
    assert_eq!(payload["markdown"], "# Docs");
    assert_eq!(payload["route"], "/docs");
}

#[::core::prelude::v1::test]
fn plans_viewer_shared_asset_keeps_controls_and_inert_document_data() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".plans")).unwrap();
    let markdown = "# Fixture\n## Section\n</script><script>throw 'must stay data'</script>";
    std::fs::write(temp.path().join(".plans/guide.md"), markdown).unwrap();
    let document = request(temp.path()).document();
    assert_eq!(data(&document)["markdown"], markdown);
    assert!(!document.html().contains("</script><script>throw"));
    for shared in [
        "function renderMarkdown",
        "function wrapH2Sections",
        "function setupCollapsing",
        "function setupTOCLinks",
        "function setupDrop",
        "reader.readAsText(file)",
        "nav=hide",
    ] {
        assert!(document.html().contains(shared), "{shared}");
    }
    assert!(
        !document
            .html()
            .contains(gitterm_core::content::bar_hint::BAR_HINT_SLOT)
    );
    assert!(document.page_url().is_none());
    std::fs::write(temp.path().join(".plans/guide.md"), b"\xef\xbb\xbf# \xff").unwrap();
    assert_eq!(
        data(&request(temp.path()).document())["markdown"],
        "# \u{fffd}"
    );
}

#[::core::prelude::v1::test]
fn plans_viewer_read_rejects_traversal_and_symlink_escape() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".plans")).unwrap();
    std::fs::write(temp.path().join("outside.md"), "outside fixture").unwrap();
    let mut req = request(temp.path());
    for name in [
        "../outside.md",
        "_archive/old.md",
        ".hidden.md",
        "missing.md",
    ] {
        req.name = Some(name.into());
        let payload = data(&req.document());
        assert!(payload["markdown"].is_null());
        assert!(payload["error"].is_string());
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            temp.path().join("outside.md"),
            temp.path().join(".plans/escape.md"),
        )
        .unwrap();
        req.name = Some("escape.md".into());
        let payload = data(&req.document());
        assert!(payload["markdown"].is_null());
        assert_eq!(payload["error"], "invalid document path");
    }
}

fn wait_for_document(shell: &gpui::Entity<crate::Shell>, cx: &mut gpui::VisualTestContext) {
    // The production coordinator drains navigation completions on its 30ms
    // clock. Advance the test clock instead of sleeping or bypassing that path.
    for _ in 0..10 {
        cx.run_until_parked();
        if cx.read(|cx| {
            shell
                .read(cx)
                .plans_content
                .read(cx)
                .document(SURFACE)
                .is_some()
        }) {
            return;
        }
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(30));
    }
    panic!("dedicated viewer did not complete navigation");
}

#[gpui::test]
fn plans_viewer_work_document_stays_dedicated_and_returns_to_same_pane(cx: &mut TestAppContext) {
    let _app = crate::test_app::Cleanup::new(cx);
    let (root, shell, cx) = crate::h7_tests::fixture(cx);
    let plans = root.path().join("work/.plans");
    std::fs::create_dir(&plans).unwrap();
    std::fs::write(
        plans.join("guide.md"),
        "# Placeholder\n## Overview\nText\n### Details\nMore",
    )
    .unwrap();
    let before = cx.read(|cx| shell.read(cx).active(cx).panes.len());
    let sidebar = cx.read(|cx| shell.read(cx).work_sidebars[&WorkspaceId(1)].clone());
    sidebar.update(cx, |_, cx| {
        cx.emit(crate::work_sidebar::WorkSidebarOutcome::OpenDocument {
            workspace: 1,
            path: plans.join("guide.md"),
        })
    });
    wait_for_document(&shell, cx);
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert!(shell.plans_open);
        assert_eq!(shell.active(cx).panes.len(), before);
        assert!(shell.selected_files.is_empty());
        let doc = shell.plans_content.read(cx).document(SURFACE).unwrap();
        assert_eq!(data(doc)["name"], "guide.md");
        assert!(
            data(doc)["markdown"]
                .as_str()
                .unwrap()
                .contains("## Overview")
        );
        assert!(shell.content.read(cx).covered());
    });
    let content = cx.read(|cx| shell.read(cx).plans_content.clone());
    content.update(cx, |content, cx| {
        content.close_requested = true; // WebKit's Escape bridge completion.
        cx.notify();
    });
    cx.run_until_parked();
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert!(!shell.plans_open);
        assert!(shell.plans_content.read(cx).selected.is_none());
        assert_eq!(shell.active(cx).panes.len(), before);
    });
}

#[gpui::test]
fn plans_viewer_workspace_switch_uses_iced_home_not_session_checkout(cx: &mut TestAppContext) {
    let _app = crate::test_app::Cleanup::new(cx);
    let (root, shell, cx) = crate::h7_tests::fixture(cx);
    for home in ["work", "second"] {
        let dir = root.path().join(home).join(".plans");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("guide.md"), format!("# {home}")).unwrap();
    }
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.toggle_plans(window, cx)));
    cx.run_until_parked();
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.pick_workspace_id(WorkspaceId(2), window, cx);
            shell.sync_plans_viewer(window, cx);
        })
    });
    wait_for_document(&shell, cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        let req = shell.plans_request.as_ref().unwrap();
        assert_eq!(req.home, root.path().join("second"));
        let payload = data(shell.plans_content.read(cx).document(SURFACE).unwrap());
        assert_eq!(
            payload["plans"],
            serde_json::json!([{"name":"guide.md", "title":"second"}])
        );
        assert!(payload["name"].is_null());
    });
}

#[path = "page_tests.rs"]
mod page_tests;
