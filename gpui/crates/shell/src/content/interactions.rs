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
    fixture_with_preview(cx, false)
}

fn fixture_with_preview(
    cx: &mut TestAppContext,
    preview: bool,
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
        let store = preview.then(|| crate::preview::tests::guarded_store(&config_root));
        let owner = cx.new(|cx| WorkspaceOwner::new(model, store, cx));
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
                    path: path.clone(),
                    changed: None,
                    from_files: true,
                },
                window,
                cx,
            );
        })
    });
    wait_for_document(shell, &path, cx);

    draw(cx);
}

#[gpui::test]
fn copy_all_uses_displayed_snapshot_including_large_preview_and_browser_still_uses_full_file(
    cx: &mut TestAppContext,
) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (folder, config, shell, cx) = fixture(cx);
    let out = tempfile::tempdir().unwrap();
    super::actions::TEST_OUT_DIR.with(|dir| *dir.borrow_mut() = Some(out.path().to_path_buf()));
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
    std::fs::write(&raw, "changed on disk after opening").unwrap();
    click(cx, "viewer-copy-all");
    cx.read(|cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().as_deref(),
            Some(preview.text.as_str())
        )
    });
    assert_eq!(tree_bytes(config.path()), copy_config_before);
    std::fs::write(&raw, &text).unwrap();
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
        let expected = out.path().join(format!(
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
fn editor_controls_request_each_route_and_nonlocal_header_is_disabled(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
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
            assert!(shell.owner.read(cx).error.is_none());
            assert_eq!(
                shell.editor_requests.first(),
                Some(&content::actions::OpenEditor::configured(
                    path.clone(),
                    None
                ))
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
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).editor_requests.last(),
            Some(&content::actions::OpenEditor::terminal(edit_path.clone()))
        )
    });
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
    let _test_app = crate::test_app::Cleanup::new(cx);
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
        assert!(shell.read(cx).owner.read(cx).error.is_none());
        assert_eq!(
            shell.read(cx).editor_requests.last(),
            Some(&content::actions::OpenEditor::terminal(
                "/fixture/a.txt".into()
            ))
        );
    });
    assert!(cx.debug_bounds("viewer-open-browser").is_none());
    click(cx, "close-file-viewer");
    cx.read(|cx| assert!(shell.read(cx).content.read(cx).visits.is_empty()));
    // Reopen and exercise the keyboard route independently from the back click.
    let retained_review = review.entity_id();
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
            let review = shell.owner.read(cx).reviews[&WorkspaceId(1)].clone();
            assert_ne!(review.entity_id(), retained_review);
            review.focus_handle(cx).focus(window, cx);
        })
    });
    draw(cx);
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.read(|cx| assert!(shell.read(cx).content.read(cx).visits.is_empty()));
}

#[gpui::test]
fn work_branch_review_marks_the_sidebar_checkout_until_the_review_closes(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (_folder, _config, shell, cx) = fixture(cx);
    let checkout = PathBuf::from("/private/tmp/gt-branch-selection-fixture");
    let marked = |cx: &mut VisualTestContext| {
        draw(cx);
        cx.read(|cx| {
            shell
                .read(cx)
                .work_sidebars
                .get(&WorkspaceId(1))
                .and_then(|sidebar| sidebar.read(cx).review_checkout().map(PathBuf::from))
        })
    };
    assert_eq!(marked(cx), None);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.work_outcome(
                &work_sidebar::WorkSidebarOutcome::OpenReview {
                    checkout: checkout.clone(),
                    vs_base: false,
                },
                window,
                cx,
            );
        })
    });
    assert_eq!(marked(cx), Some(checkout.clone()));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.close_file_viewer(WorkspaceId(1), window, cx)
        })
    });
    assert_eq!(marked(cx), None, "closing the review clears the highlight");
}

#[gpui::test]
fn work_branch_outcome_reviews_given_checkout_with_given_source(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
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

#[gpui::test]
fn chat_edit_review_changed_file_opens_owning_checkout_in_preview(cx: &mut TestAppContext) {
    chat_edit_review(cx, false);
}

#[gpui::test]
fn chat_edit_review_committed_file_opens_against_base_in_preview(cx: &mut TestAppContext) {
    chat_edit_review(cx, true);
}

fn chat_edit_review(cx: &mut TestAppContext, committed: bool) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (_folder, _config, shell, cx) = fixture_with_preview(cx, true);
    let repo = tempfile::tempdir_in("/private/tmp").unwrap();
    let git = |root: &Path, args: &[&str]| {
        let output = gitterm_core::git_status::git_command()
            .current_dir(root)
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
    git(repo.path(), &["init", "-qb", "main"]);
    std::fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "base"]);
    let checkout = repo.path().join("linked");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "-qb",
            "feature",
            checkout.to_str().unwrap(),
        ],
    );
    std::fs::create_dir(checkout.join("tests")).unwrap();
    let file = checkout.join("tests/[edit].rs");
    std::fs::write(&file, "fn placeholder() {}\n").unwrap();
    if committed {
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-qm", "edit"]);
    }
    let before = tree_bytes(repo.path());
    let chat = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.new_claude_chat(None, window, cx);
            let state = shell.active(cx);
            let pane::PaneContent::PreviewChat(chat) = &state.panes
                [&session_pane_id(state.active_session.unwrap())]
                .read(cx)
                .content
            else {
                panic!("chat pane")
            };
            chat.clone()
        })
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_chat_file(&chat, &file, (true, None), window, cx);
        })
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        draw(cx);
        let loaded = cx.read(|cx| {
            let shell = shell.read(cx);
            let Some(visit) = shell.content.read(cx).visits.get(&WorkspaceId(1)) else {
                return false;
            };
            let pane = &shell.active(cx).panes[&visit.pane];
            let pane::PaneContent::Review(review) = &pane.read(cx).content else {
                panic!("aggregate review pane")
            };
            let Some(pane) = review.read(cx).branch_review(cx) else {
                return false;
            };
            assert!(pane.error.is_none(), "{:?}", pane.error);
            if pane.loaded.is_none() {
                return false;
            }
            assert_eq!(pane.target.checkout, checkout);
            assert_eq!(
                pane.source,
                if committed {
                    gitterm_core::review::model::ReviewSource::VsBase
                } else {
                    gitterm_core::review::model::ReviewSource::Uncommitted
                }
            );
            assert_eq!(pane.focus.as_deref(), Some("tests/[edit].rs"));
            assert!(!pane.hide_noise, "a focused test file must stay visible");
            assert!(
                pane.loaded
                    .as_ref()
                    .unwrap()
                    .0
                    .file("tests/[edit].rs")
                    .is_some()
            );
            assert!(
                shell.owner.read(cx).reviews.is_empty(),
                "aggregate review must not use workspace status"
            );
            true
        });
        if loaded {
            break;
        }
        assert!(Instant::now() < deadline, "chat edit review did not load");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        tree_bytes(repo.path()),
        before,
        "review must not write checkout or Git metadata"
    );
}

#[gpui::test]
fn review_host_saves_marks_under_the_preview_root_and_opens_files_like_the_tree(
    cx: &mut TestAppContext,
) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let folder = tempfile::tempdir_in("/private/tmp").unwrap();
    let config = tempfile::tempdir_in("/private/tmp").unwrap();
    let preview = config.path().join("preview");
    let root = folder.path().to_path_buf();
    let config_root = config.path().to_path_buf();
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let mut model = WorkspaceStatus::default();
        model.add(root, Instant::now());
        let owner = cx.new(|cx| {
            WorkspaceOwner::new(
                model,
                Some(crate::preview::tests::guarded_store(&preview)),
                cx,
            )
        });
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
    let opened = Rc::new(std::cell::RefCell::new(Vec::new()));
    let host = cx.update(|_, cx| {
        let owner = shell.read(cx).owner.clone();
        let sink = opened.clone();
        let subscription = cx.subscribe(&owner, move |_, event: &FileOpen, _| {
            sink.borrow_mut()
                .push((event.workspace, event.path.clone(), event.from_files))
        });
        std::mem::forget(subscription);
        shell.update(cx, |shell, cx| shell.review_host(WorkspaceId(1), cx))
    });
    // No Iced catalog here: nothing of Iced's is read, so no turn source either.
    assert!(host.iced_marks_dir.is_none() && host.snapshot_roots.is_none());
    assert!(host.active_chat.is_none());
    let preview = config.path().join("preview");
    assert_eq!(host.preview_marks_dir.as_deref(), Some(preview.as_path()));
    (host.save_marks.unwrap())(br#"{"checkouts":{}}"#.to_vec()).unwrap();
    let saved = preview.join("review-viewed.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !saved.is_file() {
        assert!(Instant::now() < deadline, "marks were not written");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(std::fs::read(&saved).unwrap(), br#"{"checkouts":{}}"#);
    cx.update(|_, cx| (host.open_file.unwrap())("/fixture/a.rs".into(), cx));
    assert_eq!(
        *opened.borrow(),
        [(WorkspaceId(1), PathBuf::from("/fixture/a.rs"), true)]
    );
}

#[gpui::test]
fn work_sidebar_and_chat_navigation_reach_the_shared_editor_request(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (folder, _config, shell, cx) = fixture_with_preview(cx, true);
    let path = folder.path().join("file with 'quotes'.rs");
    std::fs::write(&path, "unchanged").unwrap();
    draw(cx);
    let sidebar = cx.read(|cx| {
        let shell = shell.read(cx);
        shell.work_sidebars[&shell.active(cx).workspace.id].clone()
    });
    cx.update(|_, cx| {
        sidebar.update(cx, |_, cx| {
            cx.emit(work_sidebar::WorkSidebarOutcome::OpenEditor {
                dir: folder.path().to_path_buf(),
            });
        });
    });
    draw(cx);
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).editor_requests,
            [content::actions::OpenEditor::configured(
                folder.path().to_path_buf(),
                None
            )]
        )
    });
    let chat = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.new_claude_chat(None, window, cx);
            let state = shell.active(cx);
            let pane::PaneContent::PreviewChat(chat) = &state.panes
                [&session_pane_id(state.active_session.unwrap())]
                .read(cx)
                .content
            else {
                panic!("chat pane")
            };
            chat.clone()
        })
    });
    draw(cx);
    // The checkout menu uses the configured editor, including in Preview.
    click(cx, "composer-checkout");
    click(cx, "checkout-open-editor");
    cx.read(|cx| {
        let requests = &shell.read(cx).editor_requests;
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests.last(),
            Some(&content::actions::OpenEditor::configured(
                folder.path().to_path_buf(),
                None
            ))
        );
    });
    // A file link retains Iced's viewer route; its header opens that file.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_chat_file(
                &chat,
                Path::new("file with 'quotes'.rs"),
                (false, Some(27)),
                window,
                cx,
            )
        })
    });
    draw(cx);
    click(cx, "viewer-open-editor");
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).editor_requests.last(),
            Some(&content::actions::OpenEditor::configured(
                path.clone(),
                Some(27)
            ))
        )
    });
    // A refused link cannot replace the current document's editor line.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_chat_file(
                &chat,
                Path::new("/outside-checkout.rs"),
                (false, Some(99)),
                window,
                cx,
            );
        })
    });
    draw(cx);
    click(cx, "viewer-open-editor");
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).editor_requests.last(),
            Some(&content::actions::OpenEditor::configured(
                path.clone(),
                Some(27)
            ))
        )
    });
    assert_eq!(std::fs::read_to_string(path).unwrap(), "unchanged");
}

#[gpui::test]
fn review_host_live_marks_use_iced_file_and_reader(cx: &mut TestAppContext) {
    review_host_marks_mode(true, cx);
}

#[gpui::test]
fn review_host_preview_marks_leave_iced_bytes_untouched(cx: &mut TestAppContext) {
    review_host_marks_mode(false, cx);
}

fn review_host_marks_mode(live: bool, cx: &mut TestAppContext) {
    use gitterm_core::review::{
        model::ReviewSource,
        state::{Effect, Intent},
        viewed::{VIEWED_FILE, ViewedMarks, ViewedState},
    };
    use gitterm_gpui_review::model::AggregateModel;
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (root, shell, cx) = crate::h2_tests::fixture_catalog(cx, false);
    let iced = root.path().join("iced");
    let iced_path = iced.join(VIEWED_FILE);
    let initial = br#"{"checkouts":{"/tmp/other-checkout":{"other.rs":"existing"}}}"#;
    std::fs::write(&iced_path, initial).unwrap();
    let host = cx.update(|_, cx| {
        if live {
            let services = gitterm_core::preview::service::ServiceContext::production(
                &iced,
                "review-marks-test",
            )
            .unwrap();
            crate::chat::policy::install_live(services, cx).unwrap();
        }
        shell.update(cx, |shell, cx| shell.review_host(WorkspaceId(1), cx))
    });
    let repo = edit_card_repo();
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (target, _) = executor
        .block_on(gitterm_gpui_review::repo::aggregate_target(
            repo.path().into(),
        ))
        .unwrap();
    let mut model = AggregateModel::default();
    model.dispatch(Intent::MarksLoaded(
        gitterm_gpui_review::marks::load_merged(
            host.iced_marks_dir.as_deref(),
            host.preview_marks_dir.as_deref(),
        ),
    ));
    let effects = model.dispatch(Intent::Open {
        target,
        source: ReviewSource::Uncommitted,
        hide_noise: false,
        focus: None,
        page: AggregateModel::page(),
    });
    let stamp = effects
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Collect(stamp) => Some(stamp),
            _ => None,
        })
        .unwrap();
    model.dispatch(executor.block_on(gitterm_gpui_review::repo::collect_aggregate(stamp)));
    let file = model.projection().files[0].file.clone();
    let intent = model.viewed_intent(&file.path).unwrap();
    assert!(
        model
            .dispatch(intent)
            .iter()
            .any(|effect| matches!(effect, Effect::SaveMarks))
    );
    let marks = model.state().marks().unwrap().as_ref().unwrap();
    let overlay = gitterm_gpui_review::marks::overlay(
        host.iced_marks_dir.as_deref(),
        host.preview_marks_dir.as_deref(),
        marks,
    )
    .unwrap();
    (host.save_marks.as_ref().unwrap())(overlay.encode().unwrap()).unwrap();
    let saved_dir = if live {
        iced.clone()
    } else {
        host.preview_marks_dir.clone().unwrap()
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if ViewedMarks::load(&saved_dir)
            .unwrap()
            .state(repo.path(), &file)
            == ViewedState::Viewed
        {
            break;
        }
        assert!(Instant::now() < deadline, "viewed marks not saved");
        std::thread::sleep(Duration::from_millis(10));
    }
    if live {
        assert!(host.preview_marks_dir.is_none());
        assert_eq!(
            ViewedMarks::load(&iced).unwrap(),
            *marks,
            "Iced's reader sees the mark"
        );
        assert_eq!(
            std::fs::read(&iced_path).unwrap(),
            marks.encode().unwrap(),
            "Iced's saved bytes"
        );
        assert!(!root.path().join("preview").join(VIEWED_FILE).exists());
    } else {
        assert_eq!(std::fs::read(&iced_path).unwrap(), initial);
        let merged =
            gitterm_gpui_review::marks::load_merged(Some(&iced), Some(&saved_dir)).unwrap();
        assert_eq!(merged.state(repo.path(), &file), ViewedState::Viewed);
    }
}

fn edit_card_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir_in("/private/tmp").unwrap();
    review_git(repo.path(), &["init", "-qb", "main"]);
    std::fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    review_git(repo.path(), &["add", "."]);
    review_git(repo.path(), &["commit", "-qm", "base"]);
    review_git(repo.path(), &["switch", "-qc", "feature"]);
    std::fs::create_dir(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/done.rs"), "fn done() {}\n").unwrap();
    review_git(repo.path(), &["add", "."]);
    review_git(repo.path(), &["commit", "-qm", "done"]);
    std::fs::write(repo.path().join("src/[pending].rs"), "fn pending() {}\n").unwrap();
    repo
}

fn review_git(root: &Path, args: &[&str]) {
    let output = gitterm_core::git_status::git_command()
        .current_dir(root)
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
}

fn wait_for_document(shell: &Entity<Shell>, path: &Path, cx: &mut VisualTestContext) {
    let id = crate::content_pane_id(WorkspaceId(1), path);
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        cx.executor().advance_clock(Duration::from_millis(30));
        draw(cx);
        if cx.read(|cx| shell.read(cx).content.read(cx).document(&id).is_some()) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "document did not load: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[gpui::test]
fn clicking_open_file_reuses_unchanged_snapshot_and_reloads_iced_change_signature(
    cx: &mut TestAppContext,
) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (folder, _config, shell, cx) = fixture(cx);
    let path = folder.path().join("reload.txt");
    std::fs::write(&path, "before").unwrap();
    let id = crate::content_pane_id(WorkspaceId(1), &path);
    open(&shell, path.clone(), cx);
    let (signature, address, panes) = cx.read(|cx| {
        let shell = shell.read(cx);
        let doc = shell.content.read(cx).document(&id).unwrap();
        (
            doc.signature.unwrap(),
            doc.html().as_ptr() as usize,
            shell.active(cx).panes.len(),
        )
    });
    open(&shell, path.clone(), cx);
    cx.read(|cx| {
        assert_eq!(
            shell
                .read(cx)
                .content
                .read(cx)
                .document(&id)
                .unwrap()
                .html()
                .as_ptr() as usize,
            address
        )
    });
    // Same byte length: mtime is essential, not just size.
    std::fs::write(&path, "AFTER!").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(100)),
        )
        .unwrap();
    open(&shell, path.clone(), cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        let doc = shell.content.read(cx).document(&id).unwrap();
        assert_eq!(doc.text, "AFTER!");
        assert_ne!(doc.signature, Some(signature));
        assert_eq!(shell.active(cx).panes.len(), panes);
    });
    // Preserve mtime while changing length: both parts of Iced's signature matter.
    std::fs::write(&path, "longer snapshot").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(100)),
        )
        .unwrap();
    open(&shell, path, cx);
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).content.read(cx).document(&id).unwrap().text,
            "longer snapshot"
        )
    });
}

#[gpui::test]
fn closing_file_viewer_clears_files_row_selection(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (folder, _config, shell, cx) = fixture(cx);
    let path = folder.path().join("selected.txt");
    std::fs::write(&path, "snapshot").unwrap();
    let owner = shell.read_with(cx, |shell, _| shell.owner.clone());
    let files = cx.new(|cx| {
        crate::files_view::FilesView::new(
            WorkspaceId(1),
            Rc::new(data::LocalData(owner.clone())),
            cx,
        )
    });
    owner.update(cx, |owner, cx| {
        owner.open_file(WorkspaceId(1), path.clone(), cx)
    });
    wait_for_document(&shell, &path, cx);
    assert_eq!(
        files.read_with(cx, |view, _| view.selected_file().map(Path::to_path_buf)),
        Some(path)
    );
    click(cx, "close-file-viewer");
    assert!(files.read_with(cx, |view, _| view.selected_file().is_none()));
}
