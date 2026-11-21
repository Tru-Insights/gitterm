//! Headless mounted viewer controls use GPUI's fake clipboard and URL opener.
use crate::*;
use gpui::{TestAppContext, VisualTestContext};
use std::{path::PathBuf, time::Duration};

fn fixture(
    cx: &mut TestAppContext,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Entity<Shell>,
    &mut VisualTestContext,
) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let folder = tempfile::tempdir_in("/private/tmp").unwrap();
    let config = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = folder.path().to_path_buf();
    let config_root = config.path().to_path_buf();
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let mut model = WorkspaceStatus::default();
        model.add(root, Instant::now());
        let owner = cx.new(|cx| WorkspaceOwner::new(model, None, cx));
        Shell::with_owner(
            Rc::new(data::LocalData(owner.clone())),
            owner,
            &config_root,
            true,
            window,
            cx,
        )
        .unwrap()
    });
    cx.simulate_resize(size(px(1500.), px(900.)));
    (folder, config, shell, cx)
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    draw(cx);
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
}

fn open(shell: &Entity<Shell>, path: PathBuf, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_file_viewer(
                &FileOpen {
                    workspace: WorkspaceId(1),
                    path,
                    changed: None,
                    from_files: true,
                },
                window,
                cx,
            );
        })
    });
    draw(cx);
}

#[gpui::test]
fn copy_all_and_browser_buttons_use_full_file_and_test_platform(cx: &mut TestAppContext) {
    let (folder, config, shell, cx) = fixture(cx);
    let text = format!("{}\nTHE END\n", "whole file & <text>\n".repeat(70000));
    let raw = folder.path().join("whole file.txt");
    std::fs::write(&raw, &text).unwrap();
    let markdown = folder.path().join("guide.md");
    std::fs::write(&markdown, "# Guide\n[Link](https://example.test)").unwrap();
    let long_markdown = folder.path().join("guide.MARKDOWN");
    std::fs::write(&long_markdown, "# Markdown extension").unwrap();
    let html = folder.path().join("controls.html");
    std::fs::write(
        &html,
        "<input type='checkbox'><textarea>Select me</textarea>",
    )
    .unwrap();
    let before = std::fs::read_dir(folder.path()).unwrap().count();
    let preview =
        super::Document::local_file(folder.path(), Path::new("whole file.txt"), true).unwrap();
    assert!(
        !preview.html().contains("THE END"),
        "fixture must exceed the preview limit"
    );
    open(&shell, raw.clone(), cx);
    let copy_config_before = tree_bytes(config.path());
    click(cx, "viewer-copy-all");
    cx.read(|cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().as_deref(),
            Some(text.as_str())
        )
    });
    assert_eq!(tree_bytes(config.path()), copy_config_before);
    for path in [&raw, &markdown, &long_markdown, &html] {
        open(&shell, path.clone(), cx);
        assert_eq!(cx.debug_bounds("viewer-copy-all").is_some(), path == &raw);
        let browser_config_before = tree_bytes(config.path());
        click(cx, "viewer-open-browser");
        assert_eq!(tree_bytes(config.path()), browser_config_before);
        let suffix = if super::actions::is_rendered(path) {
            "preview"
        } else {
            "file"
        };
        let expected = std::env::temp_dir().join(format!(
            "gitterm-gpui-{}_{suffix}.html",
            path.file_stem().unwrap().to_string_lossy()
        ));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !super::actions::OPENED.lock().unwrap().contains(&expected) {
            assert!(
                std::time::Instant::now() < deadline,
                "{} not opened",
                expected.display()
            );
            draw(cx);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_ne!(
            std::fs::read_to_string(&expected).unwrap(),
            "",
            "the opened page is the rendered/served page, never the raw file"
        );
    }
    assert_eq!(std::fs::read_dir(folder.path()).unwrap().count(), before);
    assert_eq!(std::fs::read_to_string(raw).unwrap(), text);
    cx.read(|cx| assert!(shell.read(cx).active(cx).sessions.is_empty()));
}

#[gpui::test]
fn editor_controls_only_notice_and_nonlocal_header_is_disabled(cx: &mut TestAppContext) {
    let (folder, config, shell, cx) = fixture(cx);
    let path = folder.path().join("source.rs");
    let edit_path = folder.path().join("other.rs");
    std::fs::write(&path, "fn main() {}\n").unwrap();
    std::fs::write(&edit_path, "fn other() {}\n").unwrap();
    open(&shell, path.clone(), cx);
    let panes = cx.read(|cx| shell.read(cx).active(cx).panes.len());
    let config_before = tree_bytes(config.path());
    click(cx, "viewer-open-editor");
    let assert_notice = |cx: &mut VisualTestContext| {
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert_eq!(
                shell.owner.read(cx).error.as_deref(),
                Some("Open in editor · not available in preview")
            );
            assert_eq!(shell.active(cx).panes.len(), panes);
            assert!(shell.active(cx).sessions.is_empty());
        })
    };
    assert_notice(cx);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.owner.update(cx, |owner, _| owner.error = None);
            shell.git_action(
                WorkspaceId(1),
                &pane::git_view::GitAction::EditFile(edit_path.clone()),
                window,
                cx,
            );
        })
    });
    assert_notice(cx);
    cx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            shell.owner.update(cx, |owner, _| owner.error = None);
            shell.local = false;
            cx.notify();
        })
    });
    click(cx, "viewer-open-editor");
    cx.read(|cx| assert!(shell.read(cx).owner.read(cx).error.is_none()));
    cx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            shell.local = true;
            shell
                .owner
                .update(cx, |owner, _| owner.model.workspaces[0].remote = true);
            cx.notify();
        })
    });
    click(cx, "viewer-open-editor");
    cx.read(|cx| assert!(shell.read(cx).owner.read(cx).error.is_none()));
    assert!(cx.opened_url().is_none());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "fn main() {}\n");
    assert_eq!(
        tree_bytes(config.path()),
        config_before,
        "editor controls must not write files"
    );
}

#[gpui::test]
fn diff_navigation_back_and_edit_notice_are_mounted(cx: &mut TestAppContext) {
    let (_folder, _config, shell, cx) = fixture(cx);
    let review = cx.update(|window, cx| shell.update(cx, |shell, cx| {
        use gitterm_core::status::{GitFileStatus, GitStatusSnapshot};
        use gitterm_gpui_review::model::{Group, Patches};
        let snapshot = GitStatusSnapshot { is_git_repo: true, repo_path: "/fixture".into(), unstaged: ["a.txt", "b.txt", "c.txt"].map(|path| GitFileStatus { path: path.into(), status: "M".into(), is_staged: false }).into(), tab_id: 0, repo_name: "Fixture".into(), branch_name: "feature".into(), staged: vec![], untracked: vec![], error: None };
        let patches: Patches = snapshot.unstaged.iter().map(|file| ((Group::Unstaged, file.path.clone()), format!("diff --git a/{0} b/{0}\n--- a/{0}\n+++ b/{0}\n@@ -1 +1 @@\n-before\n+after\n", file.path).into())).collect();
        let changed = data::ChangedFile { path: "a.txt".into(), status: "M".into(), staged: false };
        shell.open_file_viewer(&FileOpen { workspace: WorkspaceId(1), path: "/fixture/a.txt".into(), changed: Some(changed), from_files: false }, window, cx);
        let pane = shell.active(cx).panes.values().find(|pane| matches!(pane.read(cx).content, pane::PaneContent::Review(_))).unwrap();
        let pane::PaneContent::Review(review) = &pane.read(cx).content else { unreachable!() };
        let review = review.clone();
        review.update(cx, |view, cx| view.replace(&snapshot, &patches, cx).unwrap());
        review
    }));
    draw(cx);
    assert_eq!(super::session::DIFF_BACK_LABEL, "Back to Terminal");
    assert_eq!(super::session::DIFF_HINT, "j/k: navigate Esc: back");
    for (key, selected) in [
        ("j", 1),
        ("k", 0),
        ("shift-g", 2),
        ("j", 2),
        ("g", 0),
        ("k", 0),
    ] {
        cx.simulate_keystrokes(key);
        draw(cx);
        cx.read(|cx| assert_eq!(review.read(cx).model().selected, Some(selected), "{key}"));
    }
    cx.simulate_keystrokes("e");
    draw(cx);
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).owner.read(cx).error.as_deref(),
            Some("Open in editor · not available in preview")
        )
    });
    assert!(cx.debug_bounds("viewer-open-browser").is_none());
    click(cx, "close-file-viewer");
    cx.read(|cx| assert!(shell.read(cx).content.read(cx).visits.is_empty()));
    // Reopen and exercise the keyboard route independently from the back click.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_file_viewer(
                &FileOpen {
                    workspace: WorkspaceId(1),
                    path: "/fixture/a.txt".into(),
                    changed: Some(data::ChangedFile {
                        path: "a.txt".into(),
                        status: "M".into(),
                        staged: false,
                    }),
                    from_files: false,
                },
                window,
                cx,
            );
            review.focus_handle(cx).focus(window, cx);
        })
    });
    draw(cx);
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.read(|cx| assert!(shell.read(cx).content.read(cx).visits.is_empty()));
}

#[gpui::test]
fn work_branch_outcome_reviews_given_checkout_with_given_source(cx: &mut TestAppContext) {
    use gitterm_core::review::model::ReviewSource;
    let (_folder, _config, shell, cx) = fixture(cx);
    let repo = tempfile::tempdir_in("/private/tmp").unwrap();
    let git = |args: &[&str]| {
        let output = gitterm_core::git_status::git_command()
            .current_dir(repo.path())
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    git(&["init", "-qb", "main"]);
    std::fs::write(repo.path().join("change.txt"), "base\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    git(&["switch", "-qc", "feature"]);
    std::fs::write(repo.path().join("change.txt"), "branch\n").unwrap();
    git(&["commit", "-qam", "branch"]);
    let root = repo.path().to_owned();
    let before = tree_bytes(repo.path());
    for (vs_base, expected) in [
        (true, ReviewSource::VsBase),
        (false, ReviewSource::Uncommitted),
    ] {
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.work_outcome(
                    &work_sidebar::WorkSidebarOutcome::OpenReview {
                        checkout: root.clone(),
                        vs_base,
                    },
                    window,
                    cx,
                );
            })
        });
        let review = cx.read(|cx| {
            let shell = shell.read(cx);
            let visit = &shell.content.read(cx).visits[&WorkspaceId(1)];
            let pane = &shell.active(cx).panes[&visit.pane];
            let pane::PaneContent::Review(review) = &pane.read(cx).content else {
                panic!("Work route did not open Review")
            };
            assert!(
                shell.owner.read(cx).reviews.is_empty(),
                "branch review must not subscribe to home status"
            );
            review.clone()
        });
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            draw(cx);
            if cx.read(|cx| {
                review
                    .read(cx)
                    .branch_review(cx)
                    .is_some_and(|pane| pane.loaded.is_some())
            }) {
                break;
            }
            assert!(Instant::now() < deadline, "branch review did not load");
            std::thread::sleep(Duration::from_millis(10));
        }
        cx.read(|cx| {
            let view = review.read(cx);
            let pane = view.branch_review(cx).unwrap();
            assert_eq!(pane.target.checkout, root);
            assert_eq!(pane.source, expected);
            assert!(pane.error.is_none(), "{:?}", pane.error);
            if vs_base {
                assert_eq!(pane.loaded.as_ref().unwrap().0.files.len(), 1);
            } else {
                assert!(pane.loaded.as_ref().unwrap().0.files.is_empty());
            }
        });
    }
    assert_eq!(
        tree_bytes(repo.path()),
        before,
        "review must leave checkout and Git metadata unchanged"
    );
}

fn tree_bytes(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().into(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = Default::default();
    visit(root, root, &mut files);
    files
}
