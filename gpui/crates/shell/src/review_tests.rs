use super::{EntryAction, FilesView};
use crate::*;
use gitterm_gpui_review::model::Group;
use gpui::{TestAppContext, VisualTestContext};
use std::time::Duration;

fn wait(cx: &mut VisualTestContext, predicate: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        if cx.read(&predicate) {
            return;
        }
        assert!(Instant::now() < deadline, "review outcome did not arrive");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[gpui::test]
fn git_status_opens_review(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    exercise_review(cx, true);
}

#[gpui::test]
fn files_open_text_then_git_review(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    exercise_review(cx, false);
}

fn exercise_review(cx: &mut TestAppContext, git_first: bool) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let repo = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let config_root = config.path().canonicalize().unwrap();
    let git = |args: &[&str]| {
        let output = gitterm_core::git_status::git_command()
            .args(args)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q", "-b", "review-fixture"]);
    for name in ["first.txt", "second.txt", "unchanged.txt"] {
        std::fs::write(repo.path().join(name), "before\n").unwrap();
    }
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.test",
        "commit",
        "-qm",
        "fixture",
    ]);
    std::fs::write(repo.path().join("first.txt"), "after\n").unwrap();
    std::fs::write(repo.path().join("second.txt"), "second\n").unwrap();
    let root = repo.path().canonicalize().unwrap();
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let mut model = WorkspaceStatus::default();
        model.add(root.clone(), Instant::now());
        let owner = cx.new(|cx| {
            WorkspaceOwner::new(
                model,
                Some(crate::preview::tests::guarded_store(config_root.as_path())),
                cx,
            )
        });
        Shell::with_owner(
            Rc::new(data::LocalData(owner.clone())),
            owner,
            config_root.as_path(),
            true,
            window,
            cx,
        )
        .unwrap()
    });
    let owner = cx.read(|cx| shell.read(cx).owner.clone());
    let workspace = WorkspaceId(1);
    wait(cx, |cx| {
        owner.read(cx).model.status_view(workspace).files.len() == 2
    });
    let review_id = format!("workspace-{}-review", layout::workspace_layout_key(&root));
    // Exercise the Files leaf handler through the same owner subscribed by Shell.
    let files = cx.update(|_, cx| {
        cx.new(|cx| FilesView::new(workspace, Rc::new(data::LocalData(owner.clone())), cx))
    });
    let open_from_files = |name: &str, cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            files.update(cx, |files, cx| {
                let id: gpui::SharedString = name.to_string().into();
                files
                    .actions
                    .insert(id.clone(), EntryAction::Open(root.join(name)));
                files.activate(&id, cx);
            })
        });
    };
    open_from_files("unchanged.txt", cx);
    cx.run_until_parked();
    cx.read(|cx| assert!(!shell.read(cx).active(cx).panes.contains_key(&review_id)));
    let open_change = |name: &str, cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            owner.update(cx, |owner, cx| {
                let file = owner
                    .model
                    .status_view(workspace)
                    .files
                    .into_iter()
                    .find(|f| f.path == name)
                    .unwrap();
                owner.open_changed_file(workspace, file, cx);
            })
        });
    };
    if !git_first {
        open_from_files("second.txt", cx);
        cx.run_until_parked();
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert!(!shell.active(cx).panes.contains_key(&review_id));
            assert_eq!(
                shell.content.read(cx).selected.as_ref(),
                Some(&content_pane_id(workspace, &root.join("second.txt")))
            );
        });
    }
    open_change("second.txt", cx);
    cx.run_until_parked();
    let (pane, review) = cx.read(|cx| {
        let pane = shell.read(cx).active(cx).panes[&review_id].clone();
        let pane::PaneContent::Review(review) = &pane.read(cx).content else {
            panic!("expected review")
        };
        (pane.clone(), review.clone())
    });
    wait(cx, |cx| !review.read(cx).is_loading());
    cx.read(|cx| {
        let model = review.read(cx).model();
        assert!(model.error.is_none(), "{:?}", model.error);
        assert_eq!(
            model.files[model.selected.unwrap()].key,
            (Group::Unstaged, "second.txt".into())
        );
        let dock = shell.read(cx).active(cx).dock.read(cx);
        assert!(
            dock.layout(DockPlacement::Center)
                .unwrap()
                .contains_panel(PanelId::from(pane.entity_id()))
        );
    });
    open_change("first.txt", cx);
    cx.run_until_parked();
    cx.read(|cx| {
        assert_eq!(
            shell.read(cx).active(cx).panes[&review_id].entity_id(),
            pane.entity_id()
        );
        let model = review.read(cx).model();
        assert_eq!(model.files[model.selected.unwrap()].key.1, "first.txt");
        assert_eq!(owner.read(cx).reviews.len(), 1);
    });
    // Same-status content edits stay stale until a file-set change triggers a load.
    std::fs::write(root.join("first.txt"), "new patch\n").unwrap();
    cx.update(|_, cx| {
        owner.update(cx, |owner, cx| {
            owner.dispatch(
                Intent::Maintenance { focused: true },
                Instant::now() + Duration::from_secs(60),
                cx,
            )
        })
    });
    wait(cx, |cx| {
        !owner.read(cx).model.status_view(workspace).loading
    });
    cx.read(|cx| {
        assert!(!review.read(cx).is_loading());
        assert!(
            review
                .read(cx)
                .model()
                .files
                .iter()
                .all(|f| !f.patch.contains("+new patch"))
        );
    });
    std::fs::write(root.join("added.txt"), "added\n").unwrap();
    cx.update(|_, cx| {
        owner.update(cx, |owner, cx| {
            owner.dispatch(
                Intent::Maintenance { focused: true },
                Instant::now() + Duration::from_secs(120),
                cx,
            )
        });
    });
    wait(cx, |cx| {
        let model = review.read(cx).model();
        model.files.iter().any(|f| f.key.1 == "added.txt")
            && model
                .files
                .iter()
                .any(|f| f.key.1 == "first.txt" && f.patch.contains("+new patch"))
    });
    // Closing Review suppresses status-triggered loads, and showing it catches up.
    cx.update(|window, cx| {
        let dock = shell.read(cx).active(cx).dock.clone();
        dock.update(cx, |dock, cx| dock.remove_panel(pane.clone(), window, cx));
    });
    cx.run_until_parked();
    cx.read(|cx| assert!(!pane_visible(shell.read(cx).active(cx), "review", cx)));
    std::fs::write(root.join("hidden.txt"), "hidden\n").unwrap();
    cx.update(|_, cx| {
        owner.update(cx, |owner, cx| {
            owner.dispatch(
                Intent::Maintenance { focused: true },
                Instant::now() + Duration::from_secs(180),
                cx,
            )
        });
    });
    wait(cx, |cx| {
        !owner.read(cx).model.status_view(workspace).loading
    });
    cx.read(|cx| {
        assert!(
            owner
                .read(cx)
                .model
                .status_view(workspace)
                .files
                .iter()
                .any(|f| f.path == "hidden.txt")
        );
        assert!(!review.read(cx).is_loading());
        assert!(
            review
                .read(cx)
                .model()
                .files
                .iter()
                .all(|f| f.key.1 != "hidden.txt")
        );
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.show_workspace_pane(workspace, &review_id, window, cx);
        })
    });
    wait(cx, |cx| {
        review
            .read(cx)
            .model()
            .files
            .iter()
            .any(|f| f.key.1 == "hidden.txt")
    });
    // A saved lazy Review ID is pruned on a new shell, without refusing saves.
    cx.update(|window, cx| {
        let state = shell.read(cx).active(cx);
        state.store.save(&state.dock.read(cx).dump(cx)).unwrap();
        let mut model = WorkspaceStatus::default();
        model.add(root.clone(), Instant::now());
        let owner = cx.new(|cx| {
            WorkspaceOwner::new(
                model,
                Some(crate::preview::tests::guarded_store(config_root.as_path())),
                cx,
            )
        });
        let restored = cx.new(|cx| {
            Shell::with_owner(
                Rc::new(data::LocalData(owner.clone())),
                owner,
                config_root.as_path(),
                true,
                window,
                cx,
            )
            .unwrap()
        });
        let state = restored.read(cx).active(cx);
        assert!(state.error.is_none());
        assert!(!state.store.is_refused());
        assert!(!state.panes.contains_key(&review_id));
        state.store.save(&state.dock.read(cx).dump(cx)).unwrap();
    });
}
