//! Settings sheet (A02–A10): Preview refusals and Live V6 parity.
use crate::*;
use gpui::{TestAppContext, VisualTestContext};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

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
        serde_json::json!({"enabled": false, "future_guidelines": {"keep": 1}}).to_string(),
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
            "active_workspace": 0, "future_document": {"keep": 1},
            "repos": [{"dir": work, "env": {"FIXTURE_KEY": "one", "ZED": "last"}, "future_repo": {"keep": 2}}, {"dir":other}],
            "workspaces": [{
                "name": "Fixture", "abbrev": "FX", "color": "peach", "dir": work,
                "repos": [work, other], "home": work, "decisions_repo": other,
                "env": {"FIXTURE_KEY":"one", "ZED":"last", "AAA":"first"},
                "claude_in_chrome": true, "guidelines_off": true,
                "env_overrides": {"AAA": "first"}, "future_workspace": {"keep": 3},
                "active_tab":0,
                "tabs": [{"dir": work, "future_tab": {"keep": 4}}, {"dir": other.join("sub"), "repo_dir":other}]
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
    let _test_app = crate::test_app::Cleanup::new(cx);
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
    let _test_app = crate::test_app::Cleanup::new(cx);
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
    let _test_app = crate::test_app::Cleanup::new(cx);
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
    let _test_app = crate::test_app::Cleanup::new(cx);
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

fn live_fixture(cx: &mut TestAppContext) -> (Fixture, &mut VisualTestContext) {
    let (fixture, cx) = fixture(cx, true);
    let desktop = gitterm_core::desktop_store::DesktopStore::open(&fixture.source).unwrap();
    cx.update(|_, cx| cx.set_global(workspace_config::LiveWorkspaces(desktop)));
    click(cx, "pstrip-settings");
    (fixture, cx)
}

fn saved(f: &Fixture) -> gitterm_core::workspace_config::WorkspacesFile {
    serde_json::from_slice(&std::fs::read(f.source.join("workspaces.json")).unwrap()).unwrap()
}

fn wait_complete(f: &Fixture, cx: &mut VisualTestContext) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        draw(cx);
        if !cx.read(|cx| f.shell.read(cx).owner.read(cx).workspace_save_pending) {
            break;
        }
        assert!(Instant::now() < deadline, "settings save timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_saved(f: &Fixture, cx: &mut VisualTestContext) {
    wait_complete(f, cx);
    assert_eq!(notice(f, cx), None);
}

fn assert_saved(f: &Fixture, expected: &gitterm_core::workspace_config::WorkspacesFile) {
    let wire: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.source.join("workspaces.json")).unwrap()).unwrap();
    assert_eq!(wire, serde_json::to_value(expected).unwrap());
    assert_eq!(wire["future_document"], serde_json::json!({"keep":1}));
    assert_eq!(
        wire["repos"][0]["future_repo"],
        serde_json::json!({"keep":2})
    );
    assert_eq!(
        wire["workspaces"][0]["future_workspace"],
        serde_json::json!({"keep":3})
    );
    assert_eq!(
        wire["workspaces"][0]["tabs"][0]["future_tab"],
        serde_json::json!({"keep":4})
    );
}

#[gpui::test]
fn live_env_add_remove_and_profile_match_iced_handlers(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = live_fixture(cx);
    let untouched_settings = std::fs::read(f.source.join("settings.json")).unwrap();
    let mut expected = saved(&f);
    expected.normalize_repos();
    // Iced WorkspaceSettingsEnvAdd trims both inputs, updating existing overrides.
    let sheet = cx.read(|cx| f.shell.read(cx).settings_sheet.clone().unwrap());
    sheet.update_in(cx, |sheet, window, cx| {
        sheet
            .key
            .update(cx, |input, cx| input.set_value(" AAA ", window, cx));
        sheet
            .value
            .update(cx, |input, cx| input.set_value(" edited ", window, cx));
    });
    click(cx, "settings-env-add");
    wait_saved(&f, cx);
    expected.workspaces[0]
        .env_overrides
        .insert("AAA".into(), "edited".into());
    expected.workspaces[0].env = gitterm_core::workspace_config::layered_env(
        &expected.repos[0].env,
        &expected.workspaces[0].env_overrides,
    );
    assert_saved(&f, &expected);
    sheet.read_with(cx, |sheet, cx| {
        assert!(sheet.key.read(cx).value().is_empty());
        assert!(sheet.value.read(cx).value().is_empty());
    });
    // Add to the shared home repo via Enter, then remove from it.
    sheet.update_in(cx, |sheet, window, cx| {
        sheet
            .key
            .update(cx, |input, cx| input.set_value(" NEW ", window, cx));
        sheet
            .value
            .update(cx, |input, cx| input.set_value(" value ", window, cx));
        sheet.key.update(cx, |input, cx| input.focus(window, cx));
    });
    cx.simulate_keystrokes("enter");
    wait_saved(&f, cx);
    expected.repos[0].env.insert("NEW".into(), "value".into());
    expected.workspaces[0]
        .env
        .insert("NEW".into(), "value".into());
    assert_saved(&f, &expected);
    click(cx, "settings-env-remove-NEW");
    wait_saved(&f, cx);
    expected.repos[0].env.remove("NEW");
    expected.workspaces[0].env.remove("NEW");
    assert_saved(&f, &expected);
    click(cx, "settings-env-remove-AAA");
    wait_saved(&f, cx);
    expected.workspaces[0].env_overrides.clear();
    expected.workspaces[0].env.remove("AAA");
    assert_saved(&f, &expected);
    // Iced WorkspaceSettingsApplyProfile replaces the home env, clearing overrides.
    click(cx, "settings-profile-beta");
    wait_saved(&f, cx);
    expected.repos[0].env = std::collections::HashMap::from([("OTHER".into(), "x".into())]);
    expected.workspaces[0].env = expected.repos[0].env.clone();
    assert_saved(&f, &expected);
    assert_eq!(
        std::fs::read(f.source.join("settings.json")).unwrap(),
        untouched_settings
    );
}

#[gpui::test]
fn live_guidelines_all_workspace_and_repo_match_iced_handlers(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = live_fixture(cx);
    let before = std::fs::read(f.source.join("workspaces.json")).unwrap();
    let mut expected = saved(&f);
    click(cx, "settings-guidelines-all");
    wait_saved(&f, cx);
    assert_eq!(
        std::fs::read(f.source.join("workspaces.json")).unwrap(),
        before
    );
    let path = f.source.join("agent-guidelines.json");
    let wire: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        wire,
        serde_json::json!({"enabled":true,"off_repos":[],"future_guidelines":{"keep":1}})
    );
    click(cx, "settings-guidelines-workspace");
    wait_saved(&f, cx);
    expected.workspaces[0].guidelines_off = false;
    assert_saved(&f, &expected);
    let workspaces_before_repo = std::fs::read(f.source.join("workspaces.json")).unwrap();
    click(cx, "settings-guidelines-repo");
    wait_saved(&f, cx);
    let mut iced = gitterm_core::guidelines::GuidelinesSettings::default();
    iced.set_repo_off(Path::new(&expected.workspaces[0].tabs[0].dir), true);
    let wire: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        wire["off_repos"],
        serde_json::to_value(&iced.off_repos).unwrap()
    );
    assert_eq!(wire["future_guidelines"], serde_json::json!({"keep":1}));
    assert_eq!(
        std::fs::read(f.source.join("workspaces.json")).unwrap(),
        workspaces_before_repo
    );
    click(cx, "settings-guidelines-repo");
    wait_saved(&f, cx);
    assert!(
        gitterm_core::guidelines::load_settings(&f.source)
            .off_repos
            .is_empty()
    );
}

#[gpui::test]
fn live_chrome_matches_iced_claude_in_chrome_toggled(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = live_fixture(cx);
    let mut expected = saved(&f);
    click(cx, "settings-chrome");
    wait_saved(&f, cx);
    expected.workspaces[0].claude_in_chrome = false;
    assert_saved(&f, &expected);
    click(cx, "settings-chrome");
    wait_saved(&f, cx);
    expected.workspaces[0].claude_in_chrome = true;
    assert_saved(&f, &expected);
}

#[gpui::test]
fn live_decisions_repo_matches_iced_decisions_repo_picked(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = live_fixture(cx);
    let mut expected = saved(&f);
    let sheet = cx.read(|cx| f.shell.read(cx).settings_sheet.clone().unwrap());
    sheet.update(cx, |sheet, cx| {
        sheet.decisions.update(cx, |_, cx| {
            cx.emit(gpui_component::select::SelectEvent::<
                Vec<super::DecisionsChoice>,
            >::Confirm(Some(Some(
                expected.workspaces[0].dir.clone(),
            ))))
        });
    });
    wait_saved(&f, cx);
    expected.workspaces[0].decisions_repo = Some(expected.workspaces[0].dir.clone());
    assert_saved(&f, &expected);
    sheet.update(cx, |sheet, cx| {
        sheet.decisions.update(cx, |_, cx| {
            cx.emit(gpui_component::select::SelectEvent::<
                Vec<super::DecisionsChoice>,
            >::Confirm(Some(None)))
        });
    });
    wait_saved(&f, cx);
    expected.workspaces[0].decisions_repo = None;
    assert_saved(&f, &expected);
}

#[gpui::test]
fn live_color_matches_iced_workspace_settings_color_change(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = live_fixture(cx);
    let mut expected = saved(&f);
    let untouched = std::fs::read(f.source.join("agent-guidelines.json")).unwrap();
    click(cx, "settings-color-Blue");
    wait_saved(&f, cx);
    expected.workspaces[0].color = gitterm_core::settings::WorkspaceColor::Blue;
    assert_saved(&f, &expected);
    let before = std::fs::read(f.source.join("workspaces.json")).unwrap();
    click(cx, "settings-color-Blue");
    wait_saved(&f, cx);
    assert_eq!(
        std::fs::read(f.source.join("workspaces.json")).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read(f.source.join("agent-guidelines.json")).unwrap(),
        untouched
    );
}

#[gpui::test]
fn live_failed_save_keeps_sheet_and_owner_at_acknowledged_settings(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt;
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = live_fixture(cx);
    let path = f.source.join("workspaces.json");
    let before = std::fs::read(&path).unwrap();
    std::fs::set_permissions(&f.source, std::fs::Permissions::from_mode(0o555)).unwrap();
    click(cx, "settings-chrome");
    wait_complete(&f, cx);
    std::fs::set_permissions(&f.source, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(notice(&f, cx).unwrap().contains("workspaces.json"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    cx.read(|cx| {
        let shell = f.shell.read(cx);
        assert!(shell.settings_sheet.as_ref().unwrap().read(cx).model.chrome);
        assert!(
            shell
                .owner
                .read(cx)
                .iced_workspace(WorkspaceId(1))
                .unwrap()
                .claude_in_chrome
        );
    });
}

#[gpui::test]
fn live_guidelines_repo_follows_current_session_and_survives_model_refresh(
    cx: &mut TestAppContext,
) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = live_fixture(cx);
    let expected = saved(&f);
    let repo_b = gitterm_core::guidelines::repo_key(Path::new(&expected.workspaces[0].repos[1]));
    f.shell.update_in(cx, |shell, window, cx| {
        shell.close_settings_sheet(window, cx);
        let session = shell.active(cx).sessions[1].id;
        shell.select_session_id(session, window, cx);
        assert_eq!(
            shell
                .owner
                .read(cx)
                .iced_workspace(WorkspaceId(1))
                .unwrap()
                .active_tab,
            Some(0)
        );
        shell.open_settings_sheet(WorkspaceId(1), window, cx);
    });
    draw(cx);
    // An unrelated save rebuilds the model too: it must still use B.
    click(cx, "settings-chrome");
    wait_saved(&f, cx);
    cx.read(|cx| {
        let sheet = f.shell.read(cx).settings_sheet.as_ref().unwrap().read(cx);
        assert_eq!(
            sheet.model.guidelines_repo.as_ref().unwrap().0,
            repo_b.display().to_string()
        );
    });
    click(cx, "settings-guidelines-repo");
    wait_saved(&f, cx);
    assert_eq!(
        gitterm_core::guidelines::load_settings(&f.source).off_repos,
        vec![repo_b]
    );
}

fn set_env_draft(f: &Fixture, cx: &mut VisualTestContext, key: &str, value: &str) {
    let sheet = cx.read(|cx| f.shell.read(cx).settings_sheet.clone().unwrap());
    sheet.update_in(cx, |sheet, window, cx| {
        sheet
            .key
            .update(cx, |input, cx| input.set_value(key.to_owned(), window, cx));
        sheet.value.update(cx, |input, cx| {
            input.set_value(value.to_owned(), window, cx)
        });
        sheet.key.update(cx, |input, cx| input.focus(window, cx));
    });
    draw(cx);
}

fn assert_env_draft(f: &Fixture, cx: &mut VisualTestContext, key: &str, value: &str) {
    cx.read(|cx| {
        let sheet = f.shell.read(cx).settings_sheet.as_ref().unwrap().read(cx);
        assert_eq!(sheet.key.read(cx).value().as_ref(), key);
        assert_eq!(sheet.value.read(cx).value().as_ref(), value);
    });
}

#[gpui::test]
fn live_failed_env_add_retains_draft_for_successful_retry(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt;
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = live_fixture(cx);
    set_env_draft(&f, cx, " NEW ", " value ");
    let before = std::fs::read(f.source.join("workspaces.json")).unwrap();
    std::fs::set_permissions(&f.source, std::fs::Permissions::from_mode(0o555)).unwrap();
    click(cx, "settings-env-add");
    wait_complete(&f, cx);
    std::fs::set_permissions(&f.source, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(notice(&f, cx).unwrap().contains("workspaces.json"));
    assert_eq!(
        std::fs::read(f.source.join("workspaces.json")).unwrap(),
        before
    );
    assert_env_draft(&f, cx, " NEW ", " value ");
    click(cx, "settings-env-add");
    wait_saved(&f, cx);
    assert_env_draft(&f, cx, "", "");
    assert_eq!(saved(&f).workspaces[0].env["NEW"], "value");
}

#[gpui::test]
fn live_pending_save_visibly_disables_writes_and_preserves_env_draft(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = live_fixture(cx);
    set_env_draft(&f, cx, "NEW", "value");
    // Model an in-flight owner save (including saves started outside the sheet).
    // Its blocking receipt must not run on GPUI's deterministic test executor.
    f.shell.update(cx, |shell, cx| {
        shell.owner.update(cx, |owner, cx| {
            owner.workspace_save_pending = true;
            cx.notify();
        })
    });
    draw(cx);
    assert!(cx.debug_bounds("settings-saving").is_some());
    for selector in [
        "settings-guidelines-all",
        "settings-guidelines-workspace",
        "settings-guidelines-repo",
        "settings-chrome",
        "settings-color-Blue",
        "settings-profile-beta",
        "settings-env-remove-ZED",
        "settings-env-add",
        "settings-rename",
        "settings-decisions",
    ] {
        click(cx, selector);
    }
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_env_draft(&f, cx, "NEW", "value");
    cx.read(|cx| {
        let sheet = f.shell.read(cx).settings_sheet.as_ref().unwrap().read(cx);
        assert!(sheet.save_pending);
        assert!(sheet.rename.is_none());
        assert_eq!(
            sheet.model.color,
            gitterm_core::settings::WorkspaceColor::Peach
        );
    });
    f.shell.update(cx, |shell, cx| {
        shell.owner.update(cx, |owner, cx| {
            owner.workspace_save_pending = false;
            cx.notify();
        })
    });
    draw(cx);
    assert!(cx.debug_bounds("settings-saving").is_none());
    click(cx, "settings-chrome");
    wait_saved(&f, cx);
    let actual = saved(&f);
    assert!(!actual.workspaces[0].claude_in_chrome);
    assert!(actual.workspaces[0].guidelines_off);
    assert!(!actual.workspaces[0].env.contains_key("NEW"));
    assert!(actual.workspaces[0].env.contains_key("ZED"));
    assert!(!gitterm_core::guidelines::load_settings(&f.source).enabled);
    // The same controls work after acknowledgement.
    click(cx, "settings-guidelines-all");
    wait_saved(&f, cx);
    click(cx, "settings-env-add");
    wait_saved(&f, cx);
    assert_eq!(saved(&f).workspaces[0].env["NEW"], "value");
    assert_env_draft(&f, cx, "", "");
}

#[gpui::test]
fn live_env_add_acknowledgement_preserves_a_newer_draft(cx: &mut TestAppContext) {
    let _test_app = crate::test_app::Cleanup::new(cx);
    let (f, cx) = live_fixture(cx);
    set_env_draft(&f, cx, "FIRST", "one");
    // Start the real save and edit the draft before yielding to the executor.
    f.shell.update_in(cx, |shell, window, cx| {
        shell.save_sheet_edit(
            WorkspaceId(1),
            super::Edit::Env(super::EnvEdit::Set {
                key: "FIRST".into(),
                value: Some("one".into()),
            }),
            window,
            cx,
        );
        let sheet = shell.settings_sheet.clone().unwrap();
        sheet.update(cx, |sheet, cx| {
            assert_eq!(sheet.key.read(cx).value().as_ref(), "FIRST");
            sheet
                .key
                .update(cx, |input, cx| input.set_value("SECOND", window, cx));
            sheet
                .value
                .update(cx, |input, cx| input.set_value("two", window, cx));
        });
    });
    wait_saved(&f, cx);
    assert_env_draft(&f, cx, "SECOND", "two");
    assert_eq!(saved(&f).workspaces[0].env["FIRST"], "one");
    click(cx, "settings-env-add");
    wait_saved(&f, cx);
    assert_eq!(saved(&f).workspaces[0].env["SECOND"], "two");
    assert_env_draft(&f, cx, "", "");
}
