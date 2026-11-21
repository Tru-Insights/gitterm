//! Ready and Parked lists and the park sheet's facts (A15/D25/D26, A16/D18, D14).
//! Headless, fixture config only; write controls must leave every file untouched.
use super::*;
use crate::data::WorkspaceId;
use crate::h2_tests::{click, draw, fixture_catalog, fixture_with};
use crate::park_sheet::ReadyHome;
use gitterm_core::park::Info;
use gpui::{Entity, TestAppContext, VisualTestContext, px, size};

const FILES: [&str; 3] = ["workspaces.json", "tasks.json", "pr-watch.json"];

fn files(root: &std::path::Path) -> Vec<Vec<u8>> {
    FILES
        .iter()
        .map(|name| std::fs::read(root.join("iced").join(name)).unwrap())
        .collect()
}
fn notice(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Option<String> {
    cx.read(|cx| shell.read(cx).owner.read(cx).error.clone())
}

#[gpui::test]
fn ready_count_opens_iced_list_with_open_resume_close(cx: &mut TestAppContext) {
    let (root, shell, cx) = fixture_catalog(cx, true);
    cx.simulate_resize(size(px(1600.), px(800.)));
    draw(cx);
    let before = files(root.path());
    click(cx, "pstrip-ready");
    cx.read(|cx| {
        let sheet = shell.read(cx).park_sheet.as_ref().expect("list open");
        let groups = sheet.read(cx).ready_groups().expect("ready content");
        let summary: Vec<_> = groups
            .iter()
            .map(|g| {
                (
                    g.name.as_str(),
                    g.button_label(),
                    g.next.len(),
                    g.news.len(),
                )
            })
            .collect();
        // Open workspaces first, then parked, as Iced's homes.
        assert_eq!(
            summary,
            [("Named work", "Open", 1, 1), ("Parked 0", "Resume", 1, 1)]
        );
        assert_eq!(groups[0].news[0].text, "checks passed");
        assert!(matches!(groups[1].home, ReadyHome::Parked(_)));
    });
    // Resume on a parked home is a launch/write: notice, list dismissed, no writes.
    click(cx, "ready-go-1");
    assert_eq!(
        notice(&shell, cx).as_deref(),
        Some("Resume · not available in preview")
    );
    cx.read(|cx| assert!(shell.read(cx).park_sheet.is_none()));
    // Open selects the live workspace (navigation only) and dismisses the list.
    click(cx, "pstrip-ready");
    click(cx, "ready-go-0");
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert!(shell.park_sheet.is_none());
        assert_eq!(shell.active_workspace(cx), Some(WorkspaceId(2)));
    });
    // Close and Escape dismiss.
    click(cx, "pstrip-ready");
    click(cx, "ready-close");
    cx.read(|cx| assert!(shell.read(cx).park_sheet.is_none()));
    click(cx, "pstrip-ready");
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.read(|cx| assert!(shell.read(cx).park_sheet.is_none()));
    assert_eq!(files(root.path()), before);
}

#[gpui::test]
fn ready_list_without_news_says_so(cx: &mut TestAppContext) {
    let (_root, shell, cx) = fixture_with(cx, false, |_| {});
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.open_ready_list(window, cx)));
    draw(cx);
    cx.read(|cx| {
        let sheet = shell.read(cx).park_sheet.as_ref().unwrap();
        assert!(sheet.read(cx).ready_groups().unwrap().is_empty());
    });
}

#[gpui::test]
fn parked_count_opens_iced_list_with_resume_done_and_notices(cx: &mut TestAppContext) {
    let (root, shell, cx) = fixture_catalog(cx, true);
    cx.simulate_resize(size(px(1600.), px(800.)));
    draw(cx);
    let before = files(root.path());
    click(cx, "pstrip-parked");
    cx.read(|cx| {
        let sheet = shell.read(cx).park_sheet.as_ref().expect("list open");
        let rows = sheet.read(cx).parked_rows().expect("parked content");
        assert_eq!(rows.len(), 16);
        assert_eq!(rows[0].id.name, "Parked 0");
        assert_eq!(rows[0].owned_label(), "1 chat");
        assert_eq!(rows[1].owned_label(), "folder");
        assert_eq!(rows[0].note(), "No note");
        // Iced's PR news line for the parked home.
        assert_eq!(
            rows[0].pr_news.as_ref().map(|n| n.0.as_str()),
            Some("fixture/repo#2 checks passed")
        );
    });
    // Iced's labels only; GPUI's old `Mark done` / `Stop terminals` are gone.
    assert!(cx.debug_bounds("parked-resume-0").is_some());
    assert!(cx.debug_bounds("parked-done-0").is_some());
    // Done writes Iced's file: notice, list stays open.
    click(cx, "parked-done-0");
    assert_eq!(
        notice(&shell, cx).as_deref(),
        Some("Done · not available in preview")
    );
    cx.read(|cx| assert!(shell.read(cx).park_sheet.is_some()));
    // Resume launches the saved sessions: notice, list dismissed.
    click(cx, "parked-resume-0");
    assert_eq!(
        notice(&shell, cx).as_deref(),
        Some("Resume · not available in preview")
    );
    cx.read(|cx| assert!(shell.read(cx).park_sheet.is_none()));
    click(cx, "pstrip-parked");
    click(cx, "parked-close");
    cx.read(|cx| assert!(shell.read(cx).park_sheet.is_none()));
    click(cx, "pstrip-parked");
    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.read(|cx| assert!(shell.read(cx).park_sheet.is_none()));
    assert_eq!(files(root.path()), before);
}

#[gpui::test]
fn a_live_park_lists_first_as_running_and_resume_returns_it(cx: &mut TestAppContext) {
    let (root, shell, cx) = fixture_catalog(cx, true);
    cx.simulate_resize(size(px(1600.), px(800.)));
    let before = files(root.path());
    cx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            shell.workspaces[0].parked = Some(Parked {
                info: Info::new("Where I was", "2026-11-20T00:00:00Z".into()),
                running: true,
                done: false,
            });
            cx.notify();
        })
    });
    draw(cx);
    click(cx, "pstrip-parked");
    cx.read(|cx| {
        let rows = shell
            .read(cx)
            .park_sheet
            .as_ref()
            .unwrap()
            .read(cx)
            .parked_rows()
            .unwrap()
            .to_vec();
        assert_eq!(rows.len(), 17);
        assert!(rows[0].running);
        assert_eq!(rows[0].id.name, "Folder");
        assert_eq!(rows[0].note(), "Where I was");
        assert!(!rows[1].running);
    });
    // A live park is this preview's own state: Resume returns it to the bar.
    click(cx, "parked-resume-0");
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert!(shell.park_sheet.is_none());
        assert!(shell.workspaces[0].parked.is_none());
    });
    assert_eq!(files(root.path()), before);
}

#[gpui::test]
fn park_sheet_facts_carry_shared_repos_and_other_chats(cx: &mut TestAppContext) {
    let (_root, shell, cx) = fixture_with(cx, false, |file| {
        for workspace in file["workspaces"].as_array_mut().unwrap() {
            workspace["repos"] = serde_json::json!(["/fixture/shared"]);
        }
    });
    cx.read(|cx| {
        let sheet = shell.read(cx).workspace_sheet(cx).expect("sheet");
        assert_eq!(sheet.title(), "Close Folder?");
        let lines = sheet.lines();
        assert!(
            lines.contains(&"Chats in other workspaces keep running.".to_string()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"shared stays open in Named work.".to_string()),
            "{lines:?}"
        );
    });
}
