//! Settings sheet (A02–A10): fixture values shown, nothing written.
use crate::*;
use gpui::{TestAppContext, VisualTestContext};
use std::path::{Path, PathBuf};

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}
fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
}

struct Fixture {
    _root: tempfile::TempDir,
    source: PathBuf,
    shell: Entity<Shell>,
}

fn fixture(cx: &mut TestAppContext, guidelines_file: bool) -> (Fixture, &mut VisualTestContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    cx.update(install_app_menus);
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let source = root.path().join("iced");
    let work = root.path().join("work");
    let other = root.path().join("other");
    for dir in [&source, &work, &other] {
        std::fs::create_dir(dir).unwrap();
    }
    std::fs::write(source.join("settings.json"), r#"{"console_height":240}"#).unwrap();
    std::fs::write(
        source.join("profiles.json"),
        r#"{"alpha":{"env":{"FIXTURE_KEY":"one"}},"beta":{"env":{"OTHER":"x"}}}"#,
    )
    .unwrap();
    std::fs::write(
        source.join("agent-guidelines.json"),
        serde_json::json!({"enabled": false}).to_string(),
    )
    .unwrap();
    if guidelines_file {
        std::fs::write(
            source.join("agent-guidelines.md"),
            "fixture guidelines text",
        )
        .unwrap();
    }
    std::fs::write(
        source.join("workspaces.json"),
        serde_json::json!({
            "active_workspace": 0,
            "repos": [{"dir": work, "env": {"FIXTURE_KEY": "one", "ZED": "last"}}],
            "workspaces": [{
                "name": "Fixture", "abbrev": "FX", "color": "peach", "dir": work,
                "repos": [work, other], "decisions_repo": other,
                "claude_in_chrome": true, "guidelines_off": true,
                "env_overrides": {"AAA": "first"},
                "tabs": [{"dir": work}]
            }]
        })
        .to_string(),
    )
    .unwrap();
    let mut store = preview::tests::guarded_store(&root.path().join("preview"));
    let model = workspace_config::load_iced_workspaces_from(&mut store, &source);
    let layout = store.guard.as_ref().unwrap().preview().layout();
    let settings = workspace_config::load_iced_settings_from(&source);
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
        Shell::with_owner_settings(
            Rc::new(data::LocalData(owner.clone())),
            owner,
            &layout,
            true,
            &settings,
            window,
            cx,
        )
        .unwrap()
    });
    cx.simulate_resize(size(px(1400.), px(900.)));
    draw(cx);
    (
        Fixture {
            _root: root,
            source,
            shell,
        },
        cx,
    )
}

fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(&path).unwrap(),
            )
        })
        .collect();
    files.sort();
    files
}

fn notice(f: &Fixture, cx: &mut VisualTestContext) -> Option<String> {
    cx.read(|cx| f.shell.read(cx).owner.read(cx).error.clone())
}

#[gpui::test]
fn gear_opens_sheet_with_fixture_values_and_esc_closes(cx: &mut TestAppContext) {
    let (f, cx) = fixture(cx, true);
    assert!(cx.debug_bounds("settings-card").is_none());
    click(cx, "pstrip-settings");
    assert!(cx.debug_bounds("settings-card").is_some());
    let model = cx.read(|cx| {
        f.shell
            .read(cx)
            .settings_sheet
            .as_ref()
            .unwrap()
            .read(cx)
            .model()
            .clone()
    });
    assert_eq!(model.name, "Fixture");
    assert_eq!(model.color, gitterm_core::settings::WorkspaceColor::Peach);
    assert_eq!(
        model.env,
        [
            ("AAA".to_owned(), "first".to_owned()),
            ("FIXTURE_KEY".to_owned(), "one".to_owned()),
            ("ZED".to_owned(), "last".to_owned())
        ]
    );
    // Only `alpha`'s keys all match the layered env.
    assert_eq!(
        model.profiles,
        Some(vec![("alpha".to_owned(), true), ("beta".to_owned(), false)])
    );
    assert!(!model.guidelines_enabled);
    assert!(!model.guidelines_workspace);
    assert!(model.chrome);
    assert!(model.guidelines_repo.is_some());
    assert!(
        model.decisions.starts_with("other \u{00B7} "),
        "{}",
        model.decisions
    );
    assert!(!model.decisions.contains("not listed"));
    for selector in [
        "settings-title",
        "settings-rename",
        "settings-color-Peach",
        "settings-profile-alpha",
        "settings-guidelines-all",
        "settings-guidelines-workspace",
        "settings-guidelines-repo",
        "settings-edit-guidelines",
        "settings-chrome",
        "settings-decisions",
        "settings-env-remove-ZED",
        "settings-key",
        "settings-value",
        "settings-env-add",
        "settings-close",
    ] {
        assert!(cx.debug_bounds(selector).is_some(), "missing {selector}");
    }
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert!(cx.debug_bounds("settings-card").is_none());
    assert!(cx.read(|cx| f.shell.read(cx).settings_sheet.is_none()));
    // The Close control closes too.
    click(cx, "pstrip-settings");
    click(cx, "settings-close");
    assert!(cx.debug_bounds("settings-card").is_none());
}

#[gpui::test]
fn write_controls_change_nothing_and_show_their_notice(cx: &mut TestAppContext) {
    let (f, cx) = fixture(cx, true);
    let before = snapshot(&f.source);
    click(cx, "pstrip-settings");
    let cases = [
        ("settings-color-Blue", "Color"),
        ("settings-profile-beta", "Profile beta"),
        ("settings-env-remove-ZED", "Remove ZED"),
        ("settings-env-add", "Add environment variable"),
        ("settings-guidelines-all", "Guidelines for all workspaces"),
        (
            "settings-guidelines-workspace",
            "Guidelines for this workspace",
        ),
        ("settings-guidelines-repo", "Guidelines for this repo"),
        ("settings-chrome", "Claude in Chrome"),
        ("settings-decisions", "Decisions repo"),
    ];
    for (selector, label) in cases {
        click(cx, selector);
        let expected = format!("{label} \u{00B7} not available in preview");
        assert_eq!(
            notice(&f, cx).as_deref(),
            Some(expected.as_str()),
            "{selector}"
        );
        assert!(cx.debug_bounds("settings-notice").is_some(), "{selector}");
    }
    // Typing a draft is local; Enter would add, so it only shows the notice.
    click(cx, "settings-key");
    cx.simulate_input("NEWKEY");
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(
        notice(&f, cx).as_deref(),
        Some("Add environment variable \u{00B7} not available in preview")
    );
    // Rename: the field opens as a draft; submitting writes nothing.
    click(cx, "settings-rename");
    cx.simulate_input("X");
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(
        notice(&f, cx).as_deref(),
        Some("Rename \u{00B7} not available in preview")
    );
    let model_name = cx.read(|cx| {
        f.shell
            .read(cx)
            .settings_sheet
            .as_ref()
            .unwrap()
            .read(cx)
            .model()
            .name
            .clone()
    });
    assert_eq!(model_name, "Fixture");
    assert_eq!(
        snapshot(&f.source),
        before,
        "fixture files stay byte-identical"
    );
}

#[gpui::test]
fn edit_guidelines_opens_viewer_when_file_exists(cx: &mut TestAppContext) {
    let (f, cx) = fixture(cx, true);
    let before = snapshot(&f.source);
    click(cx, "pstrip-settings");
    click(cx, "settings-edit-guidelines");
    assert!(cx.debug_bounds("settings-card").is_none(), "sheet closes");
    let opened = cx.read(|cx| {
        let shell = f.shell.read(cx);
        let state = shell.active(cx);
        let path = f.source.join("agent-guidelines.md");
        state
            .panes
            .contains_key(&content_pane_id(state.workspace.id, &path))
    });
    assert!(opened, "guidelines file opens in the viewer");
    assert_eq!(snapshot(&f.source), before);
}

#[gpui::test]
fn edit_guidelines_without_file_shows_notice_and_creates_nothing(cx: &mut TestAppContext) {
    let (f, cx) = fixture(cx, false);
    let before = snapshot(&f.source);
    click(cx, "pstrip-settings");
    click(cx, "settings-edit-guidelines");
    assert_eq!(
        notice(&f, cx).as_deref(),
        Some("Edit guidelines \u{00B7} not available in preview")
    );
    assert!(
        cx.debug_bounds("settings-card").is_some(),
        "sheet stays open"
    );
    assert_eq!(snapshot(&f.source), before);
}
