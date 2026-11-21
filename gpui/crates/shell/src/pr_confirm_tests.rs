//! TRU-274 PR 22b: Iced's PR action confirmation panel in the Work sidebar.
use super::tests::{
    click_control, click_sidebar, draw_sidebar, fixture, subscribe_outcomes, tree_snapshot,
};
use super::*;
use crate::pr_confirm::{Model, Tone};
use gitterm_core::{
    merge_plan::{MergeMethod, PrKey, PrState},
    prs::{PullCore, WatchedPr},
};

fn pr(number: u64, title: &str, state: PrState) -> WatchedPr {
    let mut pr = WatchedPr::new(PrKey::new("example/gitterm", number));
    pr.snapshot.core = Some(PullCore {
        title: title.into(),
        author: "someone".into(),
        state: Some(state),
        ..Default::default()
    });
    pr
}

/// PR 1 open (plan step 1 → the next to merge), PR 2 open (waits for 1,
/// plan method squash), PR 3 a draft. `github-accounts.json` maps the owner.
fn sidebar_with_prs() -> WorkSidebar {
    let mut fixture = fixture(false);
    fixture.pr_accounts = [("EXAMPLE".to_string(), " fixture ".to_string())].into();
    fixture.pr_plans.insert(
        "root".into(),
        serde_json::from_value(serde_json::json!({"root_chat_id":"root","entries":[
            {"repo":"example/gitterm","number":1,"step":1},
            {"repo":"example/gitterm","number":2,"step":2,"method":"squash"}
        ]}))
        .unwrap(),
    );
    fixture.pull_requests.push(PullRequestGroup {
        root: "root".into(),
        title: "Chat".into(),
        prs: vec![
            pr(1, "Next change", PrState::Open),
            pr(2, "Second change", PrState::Open),
            pr(3, "Draft change", PrState::Draft),
        ],
    });
    for number in 1..=3 {
        fixture
            .open
            .insert(Fold::Pr(format!("example/gitterm#{number}")));
    }
    fixture
}

fn model(sidebar: &Entity<WorkSidebar>, cx: &mut gpui::VisualTestContext) -> Model {
    sidebar.read_with(cx, |s, _| {
        s.confirm_model(s.confirm.as_ref().expect("the panel is open"))
    })
}

fn open_panel(
    cx: &mut gpui::TestAppContext,
) -> (
    Entity<WorkSidebar>,
    &mut gpui::VisualTestContext,
    std::rc::Rc<std::cell::RefCell<Vec<WorkSidebarOutcome>>>,
) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let (sidebar, cx) = cx.add_window_view(|_, _| sidebar_with_prs());
    let events = subscribe_outcomes(&sidebar, cx);
    cx.simulate_resize(gpui::size(px(800.), px(1500.)));
    draw_sidebar(cx);
    (sidebar, cx, events)
}

#[gpui::test]
fn approve_merge_and_ready_open_iceds_panel_with_its_labels(cx: &mut gpui::TestAppContext) {
    let (sidebar, cx, events) = open_panel(cx);
    let before = sidebar.read_with(cx, |s, _| s.error.clone());

    // Iced work_prs.rs:2773, 2784, 2812-2815, 2867, 2901, 2907.
    click_control(&sidebar, cx, "Second change", "Approve");
    let approve = model(&sidebar, cx);
    assert_eq!(approve.heading, "Approve example/gitterm#2");
    assert_eq!(approve.pr_title.as_deref(), Some("Second change"));
    assert_eq!(
        approve
            .live
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>(),
        ["open", "no checks", "no review"]
    );
    assert_eq!(
        approve.account_line,
        "Runs as fixture (github_accounts maps example to it)"
    );
    assert_eq!(approve.account_tone, Tone::Secondary);
    assert!(approve.methods.is_none(), "only Merge picks a method");
    assert_eq!(
        approve.command,
        "GH_TOKEN=\"$(gh auth token --user fixture)\" gh pr review 2 --repo example/gitterm --approve"
    );
    assert_eq!(
        (approve.close_label, approve.run_label),
        ("Cancel", "Approve")
    );
    assert!(approve.run_enabled && approve.error.is_none());
    assert_eq!(approve.plan_note, None, "a ready row has no plan note");
    for selector in ["pr-confirm", "pr-confirm-cancel", "pr-confirm-run"] {
        assert!(cx.debug_bounds(selector).is_some(), "{selector} drawn");
    }
    sidebar.update(cx, |s, cx| s.confirm_close(cx));
    draw_sidebar(cx);
    assert!(cx.debug_bounds("pr-confirm").is_none());

    // Iced work_prs.rs:2792-2810: where the plan stands on it.
    click_control(&sidebar, cx, "Next change", "Approve");
    assert_eq!(
        model(&sidebar, cx).plan_note,
        Some(("the plan's next to merge".into(), Tone::Accent))
    );
    sidebar.update(cx, |s, cx| s.confirm_close(cx));
    click_control(&sidebar, cx, "Second change", "Merge");
    let merge = model(&sidebar, cx);
    assert_eq!(merge.heading, "Merge example/gitterm#2");
    assert_eq!(merge.run_label, "Merge");
    assert!(
        merge
            .command
            .ends_with("gh pr merge 2 --repo example/gitterm --squash")
    );
    sidebar.update(cx, |s, cx| s.confirm_close(cx));

    click_control(&sidebar, cx, "Draft change", "Ready for review");
    let ready = model(&sidebar, cx);
    assert_eq!(ready.heading, "Ready for review example/gitterm#3");
    assert_eq!(ready.run_label, "Ready for review");
    assert_eq!(ready.live[0], ("draft".to_string(), Tone::Muted));
    assert!(
        ready
            .command
            .ends_with("gh pr ready 3 --repo example/gitterm")
    );
    assert!(ready.methods.is_none());

    assert_eq!(sidebar.read_with(cx, |s, _| s.error.clone()), before);
    assert!(events.borrow().is_empty(), "opening writes nothing");
}

#[gpui::test]
fn merge_method_defaults_like_iced_and_is_selectable_locally(cx: &mut gpui::TestAppContext) {
    let (sidebar, cx, events) = open_panel(cx);
    let root = tempfile::tempdir().unwrap();
    let before = tree_snapshot(root.path());

    // Iced work_prs.rs:1540-1546, 2840-2864: the plan's method, else merge.
    click_control(&sidebar, cx, "Second change", "Merge");
    let planned = model(&sidebar, cx);
    let (methods, hint) = planned.methods.clone().unwrap();
    assert_eq!(
        methods,
        [
            (MergeMethod::Merge, false),
            (MergeMethod::Squash, true),
            (MergeMethod::Rebase, false)
        ]
    );
    assert_eq!(hint, "(the plan's)");
    click_sidebar(cx, "pr-confirm-method-rebase");
    let rebase = model(&sidebar, cx);
    assert_eq!(
        rebase.methods.clone().unwrap().0,
        [
            (MergeMethod::Merge, false),
            (MergeMethod::Squash, false),
            (MergeMethod::Rebase, true)
        ]
    );
    assert!(rebase.command.ends_with("--rebase"), "{}", rebase.command);
    click_sidebar(cx, "pr-confirm-method-merge");
    assert!(model(&sidebar, cx).command.ends_with("--merge"));
    sidebar.update(cx, |s, cx| s.confirm_close(cx));

    click_control(&sidebar, cx, "Next change", "Merge");
    let unplanned = model(&sidebar, cx);
    let (methods, hint) = unplanned.methods.unwrap();
    assert_eq!(
        methods,
        [
            (MergeMethod::Merge, true),
            (MergeMethod::Squash, false),
            (MergeMethod::Rebase, false)
        ]
    );
    assert_eq!(hint, "(default; the plan names none)");
    assert!(events.borrow().is_empty(), "choosing a method is local");
    assert_eq!(tree_snapshot(root.path()), before);
}

#[gpui::test]
fn cancel_and_escape_close_the_panel_with_no_other_effect(cx: &mut gpui::TestAppContext) {
    let (sidebar, cx, events) = open_panel(cx);
    let notice = sidebar.read_with(cx, |s, _| s.error.clone());

    click_control(&sidebar, cx, "Second change", "Approve");
    click_sidebar(cx, "pr-confirm-cancel");
    sidebar.read_with(cx, |s, _| {
        assert!(s.confirm.is_none());
        assert_eq!(s.error, notice);
    });
    assert!(cx.debug_bounds("pr-confirm").is_none());

    // Iced main.rs:28332-28337: Escape sends ActionClose.
    click_control(&sidebar, cx, "Second change", "Merge");
    cx.simulate_keystrokes("escape");
    sidebar.read_with(cx, |s, _| assert!(s.confirm.is_none()));
    assert!(events.borrow().is_empty());
    assert_eq!(sidebar.read_with(cx, |s, _| s.error.clone()), notice);
}

#[gpui::test]
fn execute_shows_the_preview_notice_and_changes_nothing(cx: &mut gpui::TestAppContext) {
    let (sidebar, cx, events) = open_panel(cx);
    let root = tempfile::tempdir().unwrap();
    let before = tree_snapshot(root.path());
    for (row, label, verb) in [
        ("Second change", "Approve", "Approve"),
        ("Second change", "Merge", "Merge"),
        ("Draft change", "Ready for review", "Ready for review"),
    ] {
        click_control(&sidebar, cx, row, label);
        click_sidebar(cx, "pr-confirm-run");
        let notice = format!("{verb} · not available in preview");
        assert_eq!(
            sidebar.read_with(cx, |s, _| s.error.clone()).as_deref(),
            Some(notice.as_str())
        );
        // Iced's failed-run shape: the message in the error slot, Close.
        let shown = model(&sidebar, cx);
        assert_eq!(shown.error.as_deref(), Some(notice.as_str()));
        assert_eq!(shown.close_label, "Close");
        click_sidebar(cx, "pr-confirm-cancel");
        sidebar.read_with(cx, |s, _| assert!(s.confirm.is_none()));
    }
    assert!(events.borrow().is_empty(), "no outcome reaches the host");
    assert_eq!(tree_snapshot(root.path()), before);
}

#[gpui::test]
fn account_line_and_run_gate_follow_iced(cx: &mut gpui::TestAppContext) {
    let (sidebar, cx, _) = open_panel(cx);
    sidebar.update(cx, |s, cx| {
        s.pr_accounts = [("example".to_string(), "not a login".to_string())].into();
        cx.notify();
    });
    click_control(&sidebar, cx, "Second change", "Approve");
    let bad = model(&sidebar, cx);
    assert_eq!(
        bad.account_line,
        "github_accounts maps example to \"not a login\", which is not a GitHub login; fix github-accounts.json"
    );
    assert_eq!(bad.account_tone, Tone::Danger);
    assert!(!bad.run_enabled);
    click_sidebar(cx, "pr-confirm-run");
    sidebar.read_with(cx, |s, _| {
        assert!(
            s.confirm.as_ref().unwrap().error.is_none(),
            "run is disabled"
        )
    });
    sidebar.update(cx, |s, cx| {
        s.confirm_close(cx);
        s.pr_accounts.clear();
        cx.notify();
    });
    click_control(&sidebar, cx, "Second change", "Approve");
    let none = model(&sidebar, cx);
    assert_eq!(
        none.account_line,
        "Runs as gh's active account: github_accounts has no entry for example"
    );
    assert!(none.command.starts_with("gh pr review 2"));
    assert!(none.run_enabled);
}

#[gpui::test]
fn disabled_actions_and_direct_notices_do_not_open_the_panel(cx: &mut gpui::TestAppContext) {
    let (sidebar, cx, events) = open_panel(cx);
    sidebar.update(cx, |s, cx| {
        s.pull_requests[0].prs[1].snapshot.check_runs = vec![gitterm_core::prs::CheckItem {
            name: "test".into(),
            outcome: gitterm_core::prs::CheckOutcome::Pending,
        }];
        cx.notify();
    });
    draw_sidebar(cx);
    // Iced's disabled buttons (work_prs.rs:2168, 2331) do nothing at all.
    for label in ["Approve (checks running)", "Merge (checks running)"] {
        click_control(&sidebar, cx, "Second change", label);
        sidebar.read_with(cx, |s, _| {
            assert!(s.confirm.is_none(), "{label}");
            assert!(s.error.is_none(), "{label}");
        });
    }
    // Review has no confirmation in Iced; it stays a direct preview notice.
    click_control(&sidebar, cx, "Second change", "Review");
    sidebar.read_with(cx, |s, _| {
        assert!(s.confirm.is_none());
        assert_eq!(
            s.error.as_deref(),
            Some("Review · not available in preview")
        );
    });
    assert!(events.borrow().is_empty());
}

#[gpui::test]
fn gh_active_login_decides_own_pr_and_names_the_account(cx: &mut gpui::TestAppContext) {
    let has_approve = |s: &WorkSidebar| {
        s.rows()
            .into_iter()
            .find(|r| r.label == "Second change")
            .unwrap()
            .detail_controls
            .iter()
            .any(|c| c.label.starts_with("Approve"))
    };
    let (sidebar, cx, _) = open_panel(cx);
    sidebar.update(cx, |s, cx| {
        // No mapping: the author is compared with gh's active login.
        s.pr_accounts.clear();
        s.pull_requests[0].prs[1]
            .snapshot
            .core
            .as_mut()
            .unwrap()
            .author = "example-dev".into();
        cx.notify();
    });
    sidebar.read_with(cx, |s, _| assert!(has_approve(s), "no login read: not own"));
    sidebar.update(cx, |s, cx| {
        s.active_login = Some("Example-Dev".into());
        cx.notify();
    });
    sidebar.read_with(cx, |s, _| {
        assert!(!has_approve(s), "gh's active login owns it")
    });
    sidebar.update(cx, |s, cx| {
        s.confirm_close(cx);
        cx.notify();
    });
    click_control(&sidebar, cx, "Second change", "Merge");
    assert_eq!(
        model(&sidebar, cx).account_line,
        "Runs as gh's active account (Example-Dev): github_accounts has no entry for example"
    );
    // A mapped account wins over the active login.
    sidebar.update(cx, |s, cx| {
        s.confirm_close(cx);
        s.pr_accounts = [("example".to_string(), "someone-else".to_string())].into();
        cx.notify();
    });
    sidebar.read_with(cx, |s, _| assert!(has_approve(s), "the mapping wins"));
}
