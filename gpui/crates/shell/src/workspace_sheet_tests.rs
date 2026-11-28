//! GPUI's test platform only: no native window, filesystem catalog or GUI launch.
use super::*;
use gitterm_core::{park::Key, work::catalog::ExistingWorkStatus};
use gpui::{TestAppContext, VisualTestContext};
use gpui_component::Root;
use std::{cell::RefCell, rc::Rc};

type Outcomes = Rc<RefCell<Vec<WorkspaceSheetOutcome>>>;

fn fixture(
    cx: &mut TestAppContext,
    draft: NewWorkSheet,
    existing: Vec<ExistingWork>,
) -> (Entity<WorkspaceSheet>, &mut VisualTestContext, Outcomes) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let outcomes = Outcomes::default();
    let mut sheet = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| {
            WorkspaceSheet::new(draft, existing, Some("/fixture/home".into()), window, cx)
        });
        sheet = Some(view.clone());
        let sink = outcomes.clone();
        cx.subscribe(&view, move |_, _, event: &WorkspaceSheetOutcome, _| {
            sink.borrow_mut().push(event.clone());
        })
        .detach();
        Root::new(view, window, cx)
    });
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    cx.run_until_parked();
    (sheet.unwrap(), cx, outcomes)
}

fn name(view: &Entity<WorkspaceSheet>, value: &str, cx: &mut VisualTestContext) {
    view.update_in(cx, |view, window, cx| {
        view.name.update(cx, |input, cx| {
            input.replace_all(value.to_owned(), window, cx)
        });
    });
    cx.run_until_parked();
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    cx.run_until_parked();
}

fn click(selector: &'static str, cx: &mut VisualTestContext) {
    draw(cx);
    let bounds = cx.debug_bounds(selector).expect("visible sheet control");
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
}

#[gpui::test]
fn validation_messages_and_order_match_iced(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (view, cx, outcomes) = fixture(cx, NewWorkSheet::default(), vec![]);
    cx.simulate_keystrokes("enter");
    cx.read(|cx| {
        assert_eq!(
            view.read(cx).draft().error.as_deref(),
            Some("Name the work, for example \"Cloud migration\".")
        );
    });
    assert!(outcomes.borrow().is_empty());
    name(&view, " Cloud   migration ", cx);
    view.update(cx, |view, cx| {
        assert_eq!(view.sheet.error, None);
        view.sheet.decisions_repo = Some("/fixture/other".into());
        view.submit(cx);
    });
    cx.read(|cx| {
        assert_eq!(
            view.read(cx).draft().error.as_deref(),
            Some("The decisions repo /fixture/other is not one of the starting repos.")
        )
    });
    assert!(outcomes.borrow().is_empty());
    name(&view, "   ", cx);
    view.update(cx, |view, cx| view.submit(cx));
    cx.read(|cx| {
        assert_eq!(
            view.read(cx).draft().error.as_deref(),
            Some("Name the work, for example \"Cloud migration\".")
        )
    });
}

#[gpui::test]
fn enter_emits_normalized_create_with_core_start(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let repos = vec![PathBuf::from("/fixture/web"), PathBuf::from("/fixture/api")];
    let expected = WorkStart::new(repos.clone(), Some(repos[1].clone())).unwrap();
    let (view, cx, outcomes) = fixture(
        cx,
        NewWorkSheet {
            repos,
            decisions_repo: expected.decisions_repo.clone(),
            ..Default::default()
        },
        vec![],
    );
    name(&view, "  Cloud \t migration  ", cx);
    cx.simulate_keystrokes("enter");
    assert_eq!(
        *outcomes.borrow(),
        vec![WorkspaceSheetOutcome::Create {
            name: "Cloud migration".into(),
            start: expected
        }]
    );
}

#[gpui::test]
fn matches_and_duplicate_label_follow_core_catalog(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    use ExistingWorkStatus::*;
    let work = [
        ("Cloud done", Done),
        ("Cloud parked", Parked),
        ("Cloud open", Open),
        ("Cloud open second", Open),
        ("Cloud older parked", Parked),
        ("Cloud oldest parked", Parked),
        ("Other", Open),
    ]
    .into_iter()
    .map(|(name, status)| ExistingWork {
        name: name.into(),
        status,
        action: None,
    })
    .collect();
    let (view, cx, _) = fixture(cx, NewWorkSheet::default(), work);
    cx.read(|cx| {
        let sheet = view.read(cx);
        assert_eq!(
            sheet.matching_work(),
            existing_work_matches(&sheet.existing, "")
        );
        assert!(sheet.matching_work().iter().all(|item| item.status != Done));
        assert_eq!(sheet.create_label(), "Create workspace");
    });
    name(&view, "cLoUd", cx);
    cx.read(|cx| {
        let sheet = view.read(cx);
        assert_eq!(
            sheet.matching_work(),
            existing_work_matches(&sheet.existing, "cLoUd")
        );
        assert_eq!(
            sheet
                .matching_work()
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            [
                "Cloud open",
                "Cloud open second",
                "Cloud parked",
                "Cloud older parked",
                "Cloud oldest parked"
            ]
        );
    });
    name(&view, " cloud   done ", cx);
    cx.read(|cx| {
        assert_eq!(view.read(cx).matching_work()[0].status, Done);
        assert_eq!(view.read(cx).create_label(), "Create another");
    });
    cx.simulate_keystrokes("enter");
}

#[gpui::test]
fn emits_each_existing_identity_and_cancel(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let key = Key {
        name: "Cloud migration".into(),
        dir: "/fixture/work".into(),
    };
    let actions = [
        ExistingWorkAction::Switch(42),
        ExistingWorkAction::Resume(key.clone()),
        ExistingWorkAction::Reopen(key),
    ];
    let work = actions
        .iter()
        .enumerate()
        .map(|(index, action)| ExistingWork {
            name: format!("Cloud {index}"),
            status: [
                ExistingWorkStatus::Open,
                ExistingWorkStatus::Parked,
                ExistingWorkStatus::Done,
            ][index],
            action: Some(action.clone()),
        })
        .collect();
    let (_, cx, outcomes) = fixture(
        cx,
        NewWorkSheet {
            name: "Cloud".into(),
            ..Default::default()
        },
        work,
    );
    for selector in [
        "workspace-sheet-existing-0",
        "workspace-sheet-existing-1",
        "workspace-sheet-existing-2",
    ] {
        click(selector, cx);
    }
    click("workspace-sheet-cancel", cx);
    assert_eq!(
        *outcomes.borrow(),
        actions
            .into_iter()
            .map(|id| WorkspaceSheetOutcome::OpenExisting { id })
            .chain([WorkspaceSheetOutcome::Cancel])
            .collect::<Vec<_>>()
    );
}

#[gpui::test]
fn rendered_create_and_repo_controls_emit_outcomes(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let repo = PathBuf::from("/fixture/home/web");
    let (view, cx, outcomes) = fixture(
        cx,
        NewWorkSheet {
            recent: vec![repo.clone()],
            ..Default::default()
        },
        vec![],
    );
    click("workspace-sheet-create", cx);
    assert!(outcomes.borrow().is_empty());
    name(&view, "Cloud migration", cx);
    click("workspace-sheet-recent-0", cx);
    cx.read(|cx| {
        assert_eq!(
            view.read(cx).sheet.repos.as_slice(),
            std::slice::from_ref(&repo)
        )
    });
    click("workspace-sheet-remove-0", cx);
    cx.read(|cx| assert!(view.read(cx).sheet.repos.is_empty()));
    click("workspace-sheet-add-repo", cx);
    assert_eq!(
        *outcomes.borrow(),
        [WorkspaceSheetOutcome::PickFolder {
            purpose: FolderPickerPurpose::NewWork
        }]
    );
    view.update_in(cx, |view, window, cx| {
        view.folder_picker_finished(Some(repo.clone()), window, cx)
    });
    click("workspace-sheet-create", cx);
    assert_eq!(
        outcomes.borrow().last(),
        Some(&WorkspaceSheetOutcome::Create {
            name: "Cloud migration".into(),
            start: WorkStart::new(vec![repo], None).unwrap()
        })
    );
}

#[gpui::test]
fn card_keeps_iced_width_padding_and_control_order_in_both_themes(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (view, cx, _) = fixture(
        cx,
        NewWorkSheet {
            name: "Cloud migration".into(),
            recent: vec!["/fixture/repo".into()],
            ..Default::default()
        },
        vec![],
    );
    for dark in [true, false] {
        cx.update(|window, cx| crate::theme::apply(dark, Some(window), cx));
        draw(cx);
        let card = cx.debug_bounds("workspace-sheet-card").unwrap();
        let name = cx.debug_bounds("workspace-sheet-name").unwrap();
        let add = cx.debug_bounds("workspace-sheet-add-repo").unwrap();
        let recent = cx.debug_bounds("workspace-sheet-recent-0").unwrap();
        let cancel = cx.debug_bounds("workspace-sheet-cancel").unwrap();
        let create = cx.debug_bounds("workspace-sheet-create").unwrap();
        assert_eq!(card.size.width, px(568.));
        // Iced's 520px content with 24px padding, border drawn inside.
        assert_eq!(name.size.width, px(520.));
        assert_eq!(name.left() - card.left(), px(24.));
        assert!(name.top() < add.top() && add.top() < recent.top() && recent.top() < cancel.top());
        assert!(cancel.right() < create.left());
    }
    view.update_in(cx, |view, window, cx| {
        view.add_repo("/fixture/repo".into(), window, cx)
    });
    draw(cx);
    let decisions = cx.debug_bounds("workspace-sheet-decisions").unwrap();
    let create = cx.debug_bounds("workspace-sheet-create").unwrap();
    assert!(decisions.bottom() < create.top());
}

#[gpui::test]
fn escape_cancels_with_name_focused(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (view, cx, outcomes) = fixture(cx, NewWorkSheet::default(), vec![]);
    name(&view, "Unsubmitted draft", cx);
    cx.simulate_keystrokes("escape");
    assert_eq!(*outcomes.borrow(), vec![WorkspaceSheetOutcome::Cancel]);
}

#[gpui::test]
fn picker_preserves_draft_and_owns_enter_escape_until_finished(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (view, cx, outcomes) = fixture(cx, NewWorkSheet::default(), vec![]);
    name(&view, "Cloud migration", cx);
    view.update(cx, |view, cx| view.browse(cx));
    cx.simulate_keystrokes("enter escape");
    view.update(cx, |view, cx| view.browse(cx));
    assert_eq!(
        *outcomes.borrow(),
        vec![WorkspaceSheetOutcome::PickFolder {
            purpose: FolderPickerPurpose::NewWork
        }]
    );
    view.update_in(cx, |view, window, cx| {
        view.folder_picker_finished(None, window, cx)
    });
    cx.read(|cx| assert_eq!(view.read(cx).draft().name, "Cloud migration"));
    view.update(cx, |view, cx| view.browse(cx));
    view.update_in(cx, |view, window, cx| {
        view.folder_picker_finished(Some("/fixture/repo".into()), window, cx)
    });
    cx.read(|cx| {
        assert_eq!(
            view.read(cx).draft().repos,
            [PathBuf::from("/fixture/repo")]
        )
    });
    cx.simulate_keystrokes("escape");
    assert_eq!(
        outcomes.borrow().last(),
        Some(&WorkspaceSheetOutcome::Cancel)
    );
}

#[gpui::test]
fn repo_shortcuts_add_remove_and_decisions_use_core_rules(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let recent = (0..8)
        .map(|index| PathBuf::from(format!("/fixture/home/repo-{index}")))
        .collect::<Vec<_>>();
    let (view, cx, _) = fixture(
        cx,
        NewWorkSheet {
            recent: recent.clone(),
            ..Default::default()
        },
        vec![],
    );
    view.update_in(cx, |view, window, cx| {
        assert_eq!(view.sheet.recent_shortcuts().len(), 5);
        view.add_repo(recent[0].clone(), window, cx);
        view.add_repo(recent[0].clone(), window, cx);
        view.add_repo(recent[1].clone(), window, cx);
        assert_eq!(view.sheet.repos, recent[..2]);
        assert_eq!(
            view.sheet.recent_shortcuts(),
            recent[2..7].iter().collect::<Vec<_>>()
        );
        let choices = decisions_choices(&view.sheet, view.home.as_deref());
        assert_eq!(
            choices
                .iter()
                .map(|choice| choice.label.as_ref())
                .collect::<Vec<_>>(),
            ["None", "repo-0 · ~/repo-0", "repo-1 · ~/repo-1"]
        );
        view.decisions.update(cx, |_, cx| {
            cx.emit(SelectEvent::<Vec<DecisionsChoice>>::Confirm(Some(Some(
                recent[1].clone(),
            ))))
        });
    });
    cx.run_until_parked();
    view.update_in(cx, |view, window, cx| {
        assert_eq!(view.sheet.decisions_repo, Some(recent[1].clone()));
        view.remove_repo(&recent[0], window, cx);
        assert_eq!(view.sheet.decisions_repo, Some(recent[1].clone()));
        view.remove_repo(&recent[1], window, cx);
        assert_eq!(view.sheet.decisions_repo, None);
        assert!(view.sheet.repos.is_empty());
    });
}

#[test]
fn tilde_display_respects_home_boundaries() {
    let home = Some(Path::new("/fixture/home"));
    assert_eq!(shown(Path::new("/fixture/home"), home), "~");
    assert_eq!(shown(Path::new("/fixture/home/repo"), home), "~/repo");
    assert_eq!(
        shown(Path::new("/fixture/home-other/repo"), home),
        "/fixture/home-other/repo"
    );
}

#[gpui::test]
fn gap7_new_work_sheet_tracks_configured_ui_font_size(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (sheet, cx, _) = fixture(cx, NewWorkSheet::default(), vec![]);
    let initial = cx
        .debug_bounds("workspace-sheet-title")
        .unwrap()
        .size
        .height;
    cx.update(|_, cx| {
        let mut fonts = crate::theme::font_sizes(cx);
        fonts.ui = 20.;
        crate::theme::set_font_sizes(fonts, cx);
    });
    sheet.update(cx, |_, cx| cx.notify());
    draw(cx);
    let enlarged = cx
        .debug_bounds("workspace-sheet-title")
        .unwrap()
        .size
        .height;
    assert!(
        enlarged > initial,
        "sheet title retained the constant UI font size"
    );
}
