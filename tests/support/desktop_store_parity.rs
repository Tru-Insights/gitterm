//! Synthetic cross-host persistence contract, exercising the actual Iced DTOs.
use super::config;
use gitterm_core::desktop_store::{DesktopStore, LoadState};
use gitterm_core::settings::DesktopSettings;
use gitterm_core::workspace_config::WorkspacesFile;
use serde_json::{json, Value};
use std::path::Path;

fn fixture() -> Value {
    let mut file: Value = serde_json::from_str(include_str!(
        "../fixtures/core/persistence/new.workspaces.json"
    ))
    .unwrap();
    file["workspaces"][0]["claude_in_chrome"] = json!(true);
    file["workspaces"][0]["active_tab"] = json!(1);
    file["workspaces"][0]["tabs"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "dir":"/fixture/extra/sub", "repo_dir":"/fixture/extra",
            "startup_command":"fixture-start", "tab_kind":"agent",
            "agent_config":{"backend":"claude", "model":"fixture-model"},
            "chat_session_id":"fixture-chat-2", "task_id":"fixture-task-2",
            "task_session_id":"fixture-task-session-2", "worker_shutdown_refused":true,
            "session_uid":"fixture-session-2"
        }));
    let remote = json!({
        "label":"Fixture remote", "host_name":"fixture.invalid",
        "ssh_target":"fixture@fixture.invalid", "identity_file":"/fixture/key",
        "tmux_path":"/fixture/tmux", "session_name":"fixture-remote",
        "remote_dir":"/fixture/remote", "shell_command":"fixture-shell",
        "codex_command":"fixture-codex", "claude_command":"fixture-claude"
    });
    file["remote_sessions"] = json!([remote]);
    file["remote_agents"] = json!([{
        "id":"fixture-agent", "name":"Fixture Agent", "endpoint":"http://fixture.invalid:1",
        "auth":{"type":"token", "token_ref":"env:FIXTURE_TOKEN"},
        "shell_command":"fixture-shell", "session_commands":{"claude":"fixture-claude"}
    }]);
    file["collapsed_repo_groups"] = json!(["local", "agent:fixture-agent"]);
    let mut workspace = file["workspaces"][0].clone();
    workspace["name"] = json!("Fixture Remote");
    workspace["location"] = json!({"kind":"remote_agent", "remote_id":"fixture-agent",
        "workspace_id":"fixture-workspace", "root":"/fixture/remote"});
    file["workspaces"].as_array_mut().unwrap().push(workspace);
    let mut legacy = file["workspaces"][2].clone();
    legacy["name"] = json!("Fixture Legacy Remote");
    legacy["remote_session"] = remote;
    file["workspaces"].as_array_mut().unwrap().push(legacy);
    file
}

fn core_file(store: &DesktopStore) -> WorkspacesFile {
    match store.snapshot().wait().unwrap().workspaces.loaded {
        LoadState::Valid(file) => file,
        other => panic!("expected valid: {other:?}"),
    }
}

fn write_iced(root: &Path) -> (Vec<u8>, Vec<u8>) {
    let iced: config::WorkspacesFile = serde_json::from_value(fixture()).unwrap();
    let workspaces = serde_json::to_vec_pretty(&iced).unwrap();
    let iced: config::Config =
        serde_json::from_str(include_str!("../fixtures/stage45/settings/full.input.json")).unwrap();
    let settings = serde_json::to_vec_pretty(&iced).unwrap();
    std::fs::write(root.join("workspaces.json"), &workspaces).unwrap();
    std::fs::write(root.join("settings.json"), &settings).unwrap();
    (workspaces, settings)
}

#[test]
fn complete_dtos_have_iced_field_order_and_omission_rules() {
    let iced: config::WorkspacesFile = serde_json::from_value(fixture()).unwrap();
    let mut core: WorkspacesFile = serde_json::from_value(fixture()).unwrap();
    assert_eq!(
        serde_json::to_value(&iced).unwrap(),
        serde_json::to_value(&core).unwrap()
    );
    // HashMap seeds differ per deserialization, even between two Iced loads.
    // Share only map layouts to test actual serializer field order byte for byte.
    for (core, iced) in core.workspaces.iter_mut().zip(&iced.workspaces) {
        core.env = iced.env.clone();
        core.env_overrides = iced.env_overrides.clone();
    }
    for (core, iced) in core.repos.iter_mut().zip(&iced.repos) {
        core.env = iced.env.clone();
    }
    for (core, iced) in core.remote_agents.iter_mut().zip(&iced.remote_agents) {
        core.session_commands = iced.session_commands.clone();
    }
    assert_eq!(
        serde_json::to_vec_pretty(&core).unwrap(),
        serde_json::to_vec_pretty(&iced).unwrap()
    );
    for input in [
        include_str!("../fixtures/stage45/settings/full.input.json"),
        include_str!("../fixtures/stage45/settings/missing.input.json"),
        include_str!("../fixtures/stage45/settings/legacy.input.json"),
    ] {
        let iced: config::Config = serde_json::from_str(input).unwrap();
        let bytes = serde_json::to_vec_pretty(&iced).unwrap();
        // Parsing Iced's output also covers builds whose Config omits voice.
        let core: DesktopSettings = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(serde_json::to_vec_pretty(&core).unwrap(), bytes);
    }
}

#[test]
fn no_edit_saves_are_byte_identical_including_multikey_environment_maps() {
    let temp = tempfile::tempdir().unwrap();
    let (workspaces, settings) = write_iced(temp.path());
    let owner = DesktopStore::open(temp.path()).unwrap();
    owner.save_workspaces().wait().unwrap();
    owner.save_settings().wait().unwrap();
    owner.drain().wait().unwrap();
    assert_eq!(
        std::fs::read(temp.path().join("workspaces.json")).unwrap(),
        workspaces
    );
    assert_eq!(
        std::fs::read(temp.path().join("settings.json")).unwrap(),
        settings
    );
}

#[test]
fn iced_core_edit_save_iced_save_core_preserves_the_full_document() {
    let temp = tempfile::tempdir().unwrap();
    let (workspaces, settings) = write_iced(temp.path());
    let mut expected: config::WorkspacesFile = serde_json::from_slice(&workspaces).unwrap();
    expected.workspaces[2].parked.as_mut().unwrap().note = "Edited parked note".into();
    expected.workspaces[0].tabs[1].session_uid = Some("edited-session".into());
    let mut expected_settings: config::Config = serde_json::from_slice(&settings).unwrap();
    expected_settings.theme = "dark".into();
    let owner = DesktopStore::open(temp.path()).unwrap();
    owner
        .edit_workspaces(|file| {
            file.workspaces[2].parked.as_mut().unwrap().note = "Edited parked note".into();
            file.workspaces[0].tabs[1].session_uid = Some("edited-session".into());
        })
        .wait()
        .unwrap();
    owner
        .edit_settings(|file| file.theme = "dark".into())
        .wait()
        .unwrap();
    owner.drain().wait().unwrap();
    drop(owner);
    // Same deserialize/pretty-serialize paths as Iced's load/save, at explicit
    // fixture paths so these tests never enter the real desktop config root.
    let iced: config::WorkspacesFile =
        serde_json::from_slice(&std::fs::read(temp.path().join("workspaces.json")).unwrap())
            .unwrap();
    assert_eq!(
        serde_json::to_value(&iced).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(iced.workspaces.len(), 7);
    assert_eq!(iced.active_workspace, 1);
    std::fs::write(
        temp.path().join("workspaces.json"),
        serde_json::to_vec_pretty(&iced).unwrap(),
    )
    .unwrap();
    let iced_settings: config::Config =
        serde_json::from_slice(&std::fs::read(temp.path().join("settings.json")).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(&iced_settings).unwrap(),
        serde_json::to_value(&expected_settings).unwrap()
    );
    std::fs::write(
        temp.path().join("settings.json"),
        serde_json::to_vec_pretty(&iced_settings).unwrap(),
    )
    .unwrap();
    let owner = DesktopStore::open(temp.path()).unwrap();
    assert_eq!(
        serde_json::to_value(core_file(&owner)).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    let LoadState::Valid(settings) = owner.snapshot().wait().unwrap().settings.loaded else {
        panic!("valid settings")
    };
    assert_eq!(
        serde_json::to_value(settings).unwrap(),
        serde_json::to_value(expected_settings).unwrap()
    );
}

#[test]
fn normalization_is_explicit_and_matches_iced_without_filtering_or_reordering() {
    let temp = tempfile::tempdir().unwrap();
    let (bytes, _) = write_iced(temp.path());
    let mut iced: config::WorkspacesFile = serde_json::from_slice(&bytes).unwrap();
    iced.normalize_repos();
    let owner = DesktopStore::open(temp.path()).unwrap();
    let raw = core_file(&owner);
    assert!(raw.workspaces[2].home.is_none());
    let mut normalized = raw.clone();
    normalized.normalize_repos();
    assert_eq!(
        serde_json::to_value(&normalized).unwrap(),
        serde_json::to_value(&iced).unwrap()
    );
    assert_eq!(raw.active_workspace, normalized.active_workspace);
    assert_eq!(
        raw.workspaces.iter().map(|w| &w.name).collect::<Vec<_>>(),
        normalized
            .workspaces
            .iter()
            .map(|w| &w.name)
            .collect::<Vec<_>>()
    );
    assert!(normalized
        .repos
        .iter()
        .all(|r| r.bottom_terminals.is_empty()));
    assert_eq!(normalized.workspaces[0].bottom_terminals.len(), 2);
    // Like Iced's load(), owner loading is raw. Like load_normalized(), a Live
    // caller can explicitly normalize in its serialized mutation when migrating.
    owner
        .edit_workspaces(|file| file.normalize_repos())
        .wait()
        .unwrap();
    assert_eq!(
        serde_json::to_value(core_file(&owner)).unwrap(),
        serde_json::to_value(&iced).unwrap()
    );
}

#[test]
fn unknown_fields_drop_on_mutation_and_no_gpui_fields_appear() {
    let temp = tempfile::tempdir().unwrap();
    let mut value = fixture();
    value["future_top"] = json!("unknown");
    value["workspaces"][2]["future_workspace"] = json!(true);
    value["workspaces"][0]["tabs"][0]["future_tab"] = json!(42);
    let iced: config::WorkspacesFile = serde_json::from_value(value.clone()).unwrap();
    std::fs::write(
        temp.path().join("workspaces.json"),
        serde_json::to_vec_pretty(&value).unwrap(),
    )
    .unwrap();
    let owner = DesktopStore::open(temp.path()).unwrap();
    owner.edit_workspaces(|_| {}).wait().unwrap();
    let saved: Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("workspaces.json")).unwrap())
            .unwrap();
    assert_eq!(saved, serde_json::to_value(iced).unwrap());
}

#[test]
fn worktree_root_omission_is_relative_to_the_explicit_owner_root() {
    let temp = tempfile::tempdir().unwrap();
    let owner = DesktopStore::open(temp.path()).unwrap();
    let default = owner.root().join("worktrees");
    owner
        .edit_settings(move |file| file.task_worktree_root.0 = Some(default))
        .wait()
        .unwrap();
    let path = temp.path().join("settings.json");
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(saved.get("task_worktree_root").is_none());
    owner
        .edit_settings(|file| file.task_worktree_root.0 = Some("/fixture/custom-worktrees".into()))
        .wait()
        .unwrap();
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["task_worktree_root"], "/fixture/custom-worktrees");
    for input in [r#"{"task_worktree_root":null}"#, r#"{"stt_enabled":null}"#] {
        assert!(serde_json::from_str::<DesktopSettings>(input).is_err());
    }
}
