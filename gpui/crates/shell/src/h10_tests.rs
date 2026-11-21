//! Viewer Esc hint, browser open and workspace-pick Usage close, on GPUI's
//! test platform. The platform opener is recorded, never launched.
use super::*;
use gpui::{Modifiers, TestAppContext, VisualTestContext};
use std::path::PathBuf;

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}
fn click(selector: &'static str, cx: &mut VisualTestContext) {
    draw(cx);
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    draw(cx);
}
fn open_viewer(shell: &Entity<Shell>, path: PathBuf, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let workspace = shell.active_workspace(cx).unwrap();
            shell.open_file_viewer(
                &FileOpen {
                    workspace,
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
fn opened(out: &std::path::Path, name: &str, cx: &mut VisualTestContext) -> PathBuf {
    let expected = out.join(name);
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    loop {
        draw(cx);
        if crate::content::actions::OPENED
            .lock()
            .unwrap()
            .contains(&expected)
        {
            return expected;
        }
        assert!(Instant::now() < deadline, "{name} was not opened");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[gpui::test]
fn viewer_headers_show_the_esc_hint_and_browser_opens_what_iced_opens(cx: &mut TestAppContext) {
    let (root, shell, cx) = super::h7_tests::fixture(cx);
    let work = root.path().join("work");
    let out = tempfile::tempdir().unwrap();
    crate::content::actions::TEST_OUT_DIR
        .with(|dir| *dir.borrow_mut() = Some(out.path().to_path_buf()));
    std::fs::write(work.join("h10guide.md"), "# Ten heading").unwrap();
    std::fs::write(work.join("h10raw.txt"), "alpha <b>\nbeta").unwrap();

    open_viewer(&shell, work.join("h10guide.md"), cx);
    assert!(cx.debug_bounds("viewer-esc-hint").is_some(), "rendered");
    click("viewer-open-browser", cx);
    let page = opened(out.path(), "gitterm-gpui-h10guide_preview.html", cx);
    let html = std::fs::read_to_string(&page).unwrap();
    assert!(html.contains("Ten heading"), "{html}");

    open_viewer(&shell, work.join("h10raw.txt"), cx);
    assert!(cx.debug_bounds("viewer-esc-hint").is_some(), "raw");
    click("viewer-open-browser", cx);
    let page = opened(out.path(), "gitterm-gpui-h10raw_file.html", cx);
    let html = std::fs::read_to_string(&page).unwrap();
    assert!(html.contains("GitTerm V6 File Viewer"));
    assert!(html.contains("alpha &lt;b&gt;"), "{html}");
    assert!(html.contains("line-numbers"));
    cx.read(|cx| assert_eq!(shell.read(cx).owner.read(cx).error, None));
}

#[gpui::test]
fn bar_pick_closes_usage_but_attention_and_restore_paths_keep_it(cx: &mut TestAppContext) {
    let (_root, shell, cx) = super::h7_tests::fixture(cx);
    let ids: Vec<WorkspaceId> = cx.read(|cx| {
        shell
            .read(cx)
            .workspaces
            .iter()
            .map(|w| w.workspace.id)
            .collect()
    });
    assert!(ids.len() >= 2);
    let set_usage = |open: bool, cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                if shell.usage_open != open {
                    shell.toggle_usage(window, cx);
                }
            })
        });
        draw(cx);
    };
    let usage = |cx: &mut VisualTestContext| cx.read(|cx| shell.read(cx).usage_open);
    for id in [
        cx.read(|cx| shell.read(cx).active_workspace(cx).unwrap()),
        ids[1],
    ] {
        set_usage(true, cx);
        click(
            Box::leak(format!("pstrip-workspace-{}", id.0).into_boxed_str()),
            cx,
        );
        assert!(!usage(cx), "bar pick of {id:?} closes Usage");
    }
    for index in [0, 1] {
        set_usage(true, cx);
        cx.update(|window, cx| shell.update(cx, |s, cx| s.select_workspace(index, window, cx)));
        assert!(!usage(cx), "Ctrl+digit {index}");
    }
    set_usage(true, cx);
    let (workspace, session) = cx.read(|cx| {
        let s = shell.read(cx);
        (s.active_workspace(cx).unwrap(), s.active(cx).active_session)
    });
    if let Some(session) = session {
        cx.update(|window, cx| {
            shell.update(cx, |s, cx| {
                s.select_host_session(workspace, session, window, cx)
            })
        });
    }
    cx.update(|window, cx| shell.update(cx, |s, cx| s.select_workspace_id(ids[0], window, cx)));
    assert!(usage(cx), "Activate paths leave Usage open");
}
