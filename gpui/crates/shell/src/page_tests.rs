//! Embedded Page for PRs and issues (W17, W29, V04a, V07a) on GPUI's test
//! platform: the test host loads no WebKit view and the opener is recorded.
use super::*;
use gitterm_core::page_store::PageStore;
use gpui::{TestAppContext, VisualTestContext};
use work_sidebar::WorkSidebarOutcome;

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
fn open(shell: &Entity<Shell>, url: &str, store: PageStore, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.work_outcome(
                &WorkSidebarOutcome::OpenPage {
                    url: url.into(),
                    store,
                },
                window,
                cx,
            )
        })
    });
    draw(cx);
}
/// The active workspace's visited Page: its URL and store.
pub(super) fn page(
    shell: &Entity<Shell>,
    cx: &mut VisualTestContext,
) -> Option<(String, PageStore)> {
    cx.read(|cx| {
        let shell = shell.read(cx);
        let workspace = shell.active_workspace(cx)?;
        let id = format!("workspace-{}-page", workspace.0);
        let content = shell.content.read(cx);
        content
            .is_visiting(workspace, &id)
            .then(|| content.page(&id))
            .flatten()
            .map(|(url, store)| (url.to_owned(), store.clone()))
    })
}

#[gpui::test]
fn page_header_opens_in_browser_and_close_or_escape_return_to_the_session(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (_root, shell, cx) = super::h4_tests::fixture(cx);
    let (workspace, session) = cx.read(|cx| {
        let state = shell.read(cx).active(cx);
        (state.workspace.id, state.active_session.unwrap())
    });
    let issue = "https://linear.app/sample-a/issue/SAMPLE-1/x";
    let linear = PageStore::Linear("sample-a".into());
    for escape in [false, true] {
        open(&shell, issue, linear.clone(), cx);
        assert_eq!(page(&shell, cx), Some((issue.into(), linear.clone())));
        assert!(cx.debug_bounds("page-url").is_some());
        assert!(cx.debug_bounds("page-store").is_some());
        // The Page header replaces the file viewer's controls.
        assert!(cx.debug_bounds("close-file-viewer").is_none());
        assert!(cx.debug_bounds("viewer-open-browser").is_none());
        let [url, store, browser, close] =
            ["page-url", "page-store", "page-open-browser", "page-close"]
                .map(|selector| cx.debug_bounds(selector).unwrap());
        assert!(url.right() <= store.left());
        assert!(store.right() <= browser.left());
        assert!(
            browser.right() <= close.left(),
            "Iced order: … Open in browser ×"
        );
        if escape {
            cx.simulate_keystrokes("escape");
            draw(cx);
        } else {
            click(cx, "page-open-browser");
            assert_eq!(cx.opened_url().as_deref(), Some(issue));
            click(cx, "page-close");
        }
        assert!(cx.debug_bounds("page-close").is_none());
        assert_eq!(page(&shell, cx), None);
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert_eq!(shell.active(cx).active_session, Some(session));
            let id = format!("workspace-{}-page", workspace.0);
            assert!(!shell.content.read(cx).is_visiting(workspace, &id));
            assert!(shell.content.read(cx).selected.is_none());
        });
    }
}

#[gpui::test]
fn pages_reuse_or_rebuild_by_store_and_refuse_non_http_urls(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (_root, shell, cx) = super::h4_tests::fixture(cx);
    let first = "https://github.com/example/gitterm/pull/1";
    let second = "https://github.com/example/gitterm/pull/2";
    let login = PageStore::GitHub("sample-login".into());
    open(&shell, first, login.clone(), cx);
    open(&shell, second, login.clone(), cx);
    assert_eq!(page(&shell, cx), Some((second.into(), login)));
    open(&shell, first, PageStore::Default, cx);
    assert_eq!(page(&shell, cx), Some((first.into(), PageStore::Default)));
    assert!(cx.debug_bounds("page-store").is_some());
    click(cx, "page-close");
    for url in [
        "file:///etc/hosts",
        "javascript:alert(1)",
        "gitterm://open",
        "not a url",
    ] {
        open(&shell, url, PageStore::Default, cx);
        assert_eq!(page(&shell, cx), None, "{url}");
        assert!(cx.debug_bounds("page-close").is_none(), "{url}");
    }
    assert!(cx.opened_url().is_none());
}

/// Run a main-frame navigation through the visited Page's policy, as its
/// WebKit delegate would. Returns whether WebKit would load it.
fn navigate(shell: &Entity<Shell>, url: &str, cx: &mut VisualTestContext) -> bool {
    let allowed = cx.update(|window, cx| {
        let shell = shell.read(cx);
        let id = format!("workspace-{}-page", shell.active_workspace(cx).unwrap().0);
        shell.content.clone().update(cx, |content, cx| {
            content.simulate_page_navigation(&id, url, false, true, window, cx)
        })
    });
    draw(cx);
    allowed
}
/// The visited Page's host policy; the same `Rc` means the same host.
fn host(
    shell: &Entity<Shell>,
    cx: &mut VisualTestContext,
) -> std::rc::Rc<std::cell::RefCell<content::host::PagePolicy>> {
    cx.read(|cx| {
        let shell = shell.read(cx);
        let id = format!("workspace-{}-page", shell.active_workspace(cx).unwrap().0);
        shell
            .content
            .read(cx)
            .page_policy(&id)
            .expect("a Page entry")
    })
}
fn same(
    a: &std::rc::Rc<std::cell::RefCell<content::host::PagePolicy>>,
    b: &std::rc::Rc<std::cell::RefCell<content::host::PagePolicy>>,
) -> bool {
    std::rc::Rc::ptr_eq(a, b)
}

#[gpui::test]
fn page_links_follow_the_shared_decision(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (_root, shell, cx) = super::h4_tests::fixture(cx);
    let first = "https://github.com/example/gitterm/pull/1";
    open(&shell, first, PageStore::Default, cx);
    let built = host(&shell, cx);
    // Allow: loads in place, and the header follows the page.
    let second = "https://github.com/example/gitterm/pull/2";
    assert!(navigate(&shell, second, cx));
    assert_eq!(page(&shell, cx), Some((second.into(), PageStore::Default)));
    assert!(cx.opened_url().is_none());
    // External: the browser opens it; the Page stays.
    assert!(!navigate(&shell, "https://example.com/elsewhere", cx));
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/elsewhere")
    );
    assert_eq!(page(&shell, cx), Some((second.into(), PageStore::Default)));
    // Cancel: nothing happens.
    assert!(!navigate(&shell, "javascript:alert(1)", cx));
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/elsewhere")
    );
    assert_eq!(page(&shell, cx), Some((second.into(), PageStore::Default)));
    assert!(same(&host(&shell, cx), &built));
    // Reopen: the URL opens in the Page with the decided store, rebuilt.
    let issue = "https://linear.app/sample-a/issue/SAMPLE-1/x";
    assert!(!navigate(&shell, issue, cx));
    let linear = PageStore::Linear("sample-a".into());
    assert_eq!(page(&shell, cx), Some((issue.into(), linear.clone())));
    assert!(
        !same(&host(&shell, cx), &built),
        "another store rebuilds the Page"
    );
    assert!(cx.debug_bounds("page-close").is_some());
    // The reopened Page decides against its own URL and store.
    let next = "https://linear.app/sample-a/issue/SAMPLE-2/y";
    assert!(navigate(&shell, next, cx));
    assert_eq!(page(&shell, cx), Some((next.into(), linear)));
}

#[gpui::test]
fn a_closed_page_stays_hidden_and_reopening_reuses_it(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (_root, shell, cx) = super::h4_tests::fixture(cx);
    let first = "https://github.com/example/gitterm/pull/1";
    let login = PageStore::GitHub("sample-login".into());
    open(&shell, first, login.clone(), cx);
    let built = host(&shell, cx);
    for close in ["page-close", "escape"] {
        if close == "escape" {
            cx.simulate_keystrokes("escape");
            draw(cx);
        } else {
            click(cx, close);
        }
        assert_eq!(page(&shell, cx), None);
        assert!(
            same(&host(&shell, cx), &built),
            "{close} hides, not destroys"
        );
        open(&shell, first, login.clone(), cx);
        assert_eq!(page(&shell, cx), Some((first.into(), login.clone())));
        assert!(
            same(&host(&shell, cx), &built),
            "same store after {close} reuses"
        );
    }
    // A reopen queued by the closed Page is stale and dropped, as Iced's is.
    click(cx, "page-close");
    let id = cx.read(|cx| {
        format!(
            "workspace-{}-page",
            shell.read(cx).active_workspace(cx).unwrap().0
        )
    });
    cx.update(|window, cx| {
        let content = shell.read(cx).content.clone();
        content.update(cx, |content, cx| {
            let issue = "https://linear.app/sample-a/issue/SAMPLE-1/x";
            assert!(!content.simulate_page_navigation(&id, issue, false, true, window, cx));
        })
    });
    draw(cx);
    assert_eq!(page(&shell, cx), None);
    assert!(same(&host(&shell, cx), &built));
    // Another store rebuilds.
    open(&shell, first, PageStore::Default, cx);
    assert!(!same(&host(&shell, cx), &built));
}
