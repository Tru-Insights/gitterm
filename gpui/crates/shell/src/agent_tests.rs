//! W31/W31a/W31b/L06: Work's agent summary link opens the Agent pane, its
//! sections fold with counts, an item opens the read-only viewer, `← Work`
//! returns. A fixture agent home stands in for the user's config.
use super::*;
use gpui::{Modifiers, TestAppContext, VisualTestContext};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}
fn click(selector: &str, cx: &mut VisualTestContext) {
    let selector: &'static str = selector.to_owned().leak();
    draw(cx);
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    draw(cx);
}
fn shown(selector: &str, cx: &mut VisualTestContext) -> bool {
    let selector: &'static str = selector.to_owned().leak();
    draw(cx);
    cx.debug_bounds(selector).is_some()
}
fn bytes(dirs: &[&Path]) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    for dir in dirs {
        walk(dir, &mut out);
    }
    out
}
fn mode(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> SidebarMode {
    cx.read(|cx| shell.read(cx).active(cx).store.regions.sidebar_mode)
}

#[gpui::test]
fn agent_link_opens_folded_sections_items_open_the_viewer_and_back_returns(
    cx: &mut TestAppContext,
) {
    let home = tempfile::tempdir_in("/private/tmp").unwrap();
    let home = home.path().to_path_buf();
    std::fs::create_dir(home.join("commands")).unwrap();
    std::fs::write(home.join("commands/review.md"), "# Review skill").unwrap();
    std::fs::write(home.join("commands/plan.md"), "# Plan skill").unwrap();
    std::fs::write(
        home.join("settings.json"),
        r#"{"enabledPlugins":{"fmt@kit":true},"hooks":{"Stop":[]},"model":"m"}"#,
    )
    .unwrap();
    std::fs::write(home.join(".mcp.json"), r#"{"mcpServers":{"docs":{}}}"#).unwrap();
    cx.update(|cx| cx.set_global(agent_sidebar::AgentHome(home.clone())));
    let (root, shell, cx) = super::h7_tests::fixture(cx);
    let work = root.path().join("work");

    // Work's line, as Iced's overview::agent_config_line.
    assert!(shown("work-agent-config", cx), "summary link");
    let workspace = cx.read(|cx| shell.read(cx).active_workspace(cx).unwrap());
    let line = cx.read(|cx| shell.read(cx).agent_sidebars[&workspace].read(cx).summary());
    assert_eq!(line.as_deref(), Some("skills 2 · MCPs 1 · hooks 1"));

    // Iced re-reads when the Agent view opens: a project server appears.
    std::fs::write(work.join(".mcp.json"), r#"{"mcpServers":{"local":{}}}"#).unwrap();
    let before = bytes(&[&home, &work]);
    click("work-agent-config", cx);
    assert_eq!(mode(&shell, cx), SidebarMode::Agent);
    assert!(shown("agent-back-work", cx));
    assert!(!shown("work-sidebar", cx), "Work replaced by Agent");

    let sections = [
        ("skills", "Skills", 2),
        ("plugins", "Plugins", 1),
        ("mcp_servers", "MCP Servers", 2),
        ("hooks", "Hooks", 1),
        ("settings", "Settings", 1),
    ];
    let mut order = vec![];
    for (key, label, count) in sections {
        assert!(shown(&format!("agent-label-{label}"), cx), "{label}");
        assert!(shown(&format!("agent-count-{key}-{count}"), cx), "{key}");
        assert!(shown(&format!("agent-chevron-{key}-▶"), cx), "{key} folded");
        assert!(!shown(&format!("agent-item-{key}-0"), cx), "{key} folded");
        order.push(
            cx.debug_bounds(format!("agent-label-{label}").leak())
                .unwrap()
                .origin
                .y,
        );
        click(&format!("agent-section-{key}"), cx);
        assert!(shown(&format!("agent-chevron-{key}-▼"), cx), "{key} open");
        for index in 0..count {
            assert!(
                shown(&format!("agent-item-{key}-{index}"), cx),
                "{key} {index}"
            );
        }
        click(&format!("agent-section-{key}"), cx);
        assert!(
            shown(&format!("agent-chevron-{key}-▶"), cx),
            "{key} refolded"
        );
        assert!(!shown(&format!("agent-item-{key}-0"), cx));
    }
    assert!(order.is_sorted(), "Iced's section order: {order:?}");

    // Items sort by name, case-insensitively: plan, review.
    click("agent-section-skills", cx);
    click("agent-item-skills-1", cx);
    assert!(shown("agent-selected-skills-1", cx), "selected highlight");
    assert!(!shown("agent-selected-skills-0", cx));
    let file = home.join("commands/review.md");
    let id = crate::content_pane_id(workspace, &file);
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(shell.owner.read(cx).error, None);
        assert!(shell.active(cx).panes.contains_key(&id), "viewer pane");
        assert_eq!(
            shell.content.read(cx).selected.as_deref(),
            Some(id.as_str())
        );
    });
    // Settings items open the agent's settings file.
    click("agent-section-settings", cx);
    click("agent-item-settings-0", cx);
    let settings = crate::content_pane_id(workspace, &home.join("settings.json"));
    cx.read(|cx| assert!(shell.read(cx).active(cx).panes.contains_key(&settings)));
    assert!(
        mode(&shell, cx) == SidebarMode::Agent,
        "viewer keeps the pane"
    );

    click("agent-back-work", cx);
    assert_eq!(mode(&shell, cx), SidebarMode::Work);
    assert!(shown("work-agent-config", cx));
    let line = cx.read(|cx| shell.read(cx).agent_sidebars[&workspace].read(cx).summary());
    assert_eq!(line.as_deref(), Some("skills 2 · MCPs 2 · hooks 1"));
    assert_eq!(bytes(&[&home, &work]), before, "nothing in the pane writes");
}

#[gpui::test]
fn without_an_agent_home_the_pane_reads_nothing_and_work_hides_the_link(cx: &mut TestAppContext) {
    let (_root, shell, cx) = super::h7_tests::fixture(cx);
    assert!(!shown("work-agent-config", cx));
    let workspace = cx.read(|cx| shell.read(cx).active_workspace(cx).unwrap());
    // A path the pane does not list never gains an agent viewer root.
    let stray = PathBuf::from("/private/tmp/not-listed.md");
    cx.read(|cx| assert_eq!(shell.read(cx).agent_file_root(workspace, &stray, cx), None));
}
