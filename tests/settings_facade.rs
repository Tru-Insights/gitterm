//! Exercise the actual root config module without starting the app.
#![allow(dead_code)]

#[path = "../src/config.rs"]
mod config;

mod theme {
    pub enum AppTheme {
        Dark,
        Light,
    }
}

#[path = "support/iced_named_work.rs"]
mod iced_named_work;

#[test]
fn named_work_workspace_bytes_and_roots_are_pinned() {
    let old =
        r#"{"name":"Cloud migration","abbrev":"CL","dir":"/work/cloud","color":"blue","tabs":[]}"#;
    let mut cfg: config::WorkspaceConfig = serde_json::from_str(old).unwrap();
    let baseline = serde_json::to_string(&cfg).unwrap();
    assert_eq!(
        baseline,
        r#"{"collapsed_issue_groups":["done"],"name":"Cloud migration","abbrev":"CL","dir":"/work/cloud","color":"blue","tabs":[],"run_command":null,"bottom_terminals":[],"active":true}"#
    );
    let file: config::WorkspacesFile =
        serde_json::from_str(&format!(r#"{{"workspaces":[{old}],"active_workspace":0}}"#)).unwrap();
    let file_bytes = serde_json::to_string(&file).unwrap();
    assert_eq!(
        file_bytes,
        format!(r#"{{"workspaces":[{baseline}],"active_workspace":0,"remote_sessions":[]}}"#)
    );
    let file_back: config::WorkspacesFile = serde_json::from_str(&file_bytes).unwrap();
    assert_eq!(serde_json::to_string(&file_back).unwrap(), file_bytes);
    for active in [false, true] {
        for done_at in [None, Some("invalid but present".to_string())] {
            cfg.active = active;
            cfg.done_at = done_at;
            assert_eq!(cfg.is_parked(), !cfg.active && cfg.done_at.is_none());
        }
    }
    cfg.done_at = None;
    for home in [None, Some(String::new()), Some("/repo/api".into())] {
        cfg.home = home;
        assert_eq!(cfg.home_dir(), cfg.home.as_deref().unwrap_or(&cfg.dir));
    }
    cfg.home = None;
    assert_eq!(cfg.home_dir(), "/work/cloud");
    cfg.home = Some("/repo/api".into());
    cfg.repos = vec!["/repo/api".into()];
    cfg.chats = vec!["root-chat".into()];
    cfg.decisions_repo = Some("/repo/api".into());
    cfg.active = false;
    assert!(cfg.is_parked());
    cfg.done_at = Some("2026-11-01T00:00:00Z".into());
    assert!(!cfg.is_parked());
    assert_eq!(cfg.home_dir(), "/repo/api");
    let bytes = serde_json::to_vec(&cfg).unwrap();
    let back: config::WorkspaceConfig = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(serde_json::to_vec(&back).unwrap(), bytes);
    assert!(!String::from_utf8(bytes)
        .unwrap()
        .contains("claims_new_chats"));
}

#[test]
fn core_workspace_dtos_preserve_iced_serialized_bytes() {
    for input in [
        include_str!("fixtures/core/persistence/old.workspaces.json"),
        include_str!("fixtures/core/persistence/new.workspaces.json"),
    ] {
        let iced: config::WorkspacesFile = serde_json::from_str(input).unwrap();
        let mut core: gitterm_core::workspace_config::WorkspacesFile =
            serde_json::from_str(input).unwrap();
        assert_eq!(
            serde_json::to_value(&iced).unwrap(),
            serde_json::to_value(&core).unwrap()
        );
        // HashMap iteration is randomized independently of the wire DTO. Reuse
        // the map layouts after comparing values so the byte check pins field order.
        for (core, iced) in core.workspaces.iter_mut().zip(&iced.workspaces) {
            core.env = iced.env.clone();
            core.env_overrides = iced.env_overrides.clone();
        }
        for (core, iced) in core.repos.iter_mut().zip(&iced.repos) {
            core.env = iced.env.clone();
        }
        assert_eq!(
            serde_json::to_vec(&iced).unwrap(),
            serde_json::to_vec(&core).unwrap()
        );
    }
    let input = include_str!("fixtures/stage45/sessions/workspaces.saved.json").trim();
    let core: Vec<gitterm_core::workspace_config::WorkspaceConfig> =
        serde_json::from_str(input).unwrap();
    assert_eq!(serde_json::to_string(&core).unwrap(), input);
}

#[path = "support/desktop_store_parity.rs"]
mod desktop_store_parity;

#[test]
fn shared_park_helpers_preserve_iced_bytes_and_cross_read_both_hosts() {
    use gitterm_core::park::{self, Key};
    let fixture = r#"{"name":"Old Iced work","abbrev":"OI","dir":"/fixture/work","color":"blue","tabs":[{"dir":"/fixture/repo","session_uid":"caller","startup_command":"echo preserved"}],"chats":["chat"],"env":{"KEEP":"value"}}"#;
    let iced: config::WorkspaceConfig = serde_json::from_str(fixture).unwrap();
    let key = Key {
        name: iced.name.clone(),
        dir: iced.dir.clone(),
    };
    let info = config::ParkedInfo {
        note: "keep this note".into(),
        parked_at: "2026-11-27T00:00:00Z".into(),
    };
    // Frozen pre-extraction operations, performed on the very same map clone
    // so randomized HashMap iteration cannot mask a byte regression.
    let mut expected = iced.clone();
    expected.active = false;
    expected.parked = Some(info.clone());
    let actual = park::parked_config(iced, info);
    assert_eq!(
        serde_json::to_vec(&actual).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    let mut configs = vec![actual];
    assert!(park::finish_parked(&mut configs, &key, "done-stamp".into()));
    expected.done_at = Some("done-stamp".into());
    assert_eq!(
        serde_json::to_vec(&configs[0]).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    assert!(park::reopen_done(&mut configs, &key));
    expected.done_at = None;
    assert_eq!(
        serde_json::to_vec(&configs[0]).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    let gpui: gitterm_core::workspace_config::WorkspaceConfig =
        serde_json::from_slice(&serde_json::to_vec(&configs[0]).unwrap()).unwrap();
    let iced: config::WorkspaceConfig =
        serde_json::from_slice(&serde_json::to_vec(&gpui).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(iced).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[test]
fn workspace_settings_legacy_remote_saved_bytes_match_iced_runtime_sequence() {
    use gitterm_core::{
        desktop_store::DesktopStore,
        workspace_config::WorkspaceTarget,
        workspace_settings::{Edit, EnvEdit},
    };
    let root = tempfile::tempdir().unwrap();
    let input = serde_json::json!({"active_workspace":0,"workspaces":[{
        "name":"Remote fixture","abbrev":"RF","color":"blue","dir":root.path().join("stand-in"),"tabs":[],
        "remote_session":{"label":"fixture","host_name":"fixture.invalid","ssh_target":"fixture.invalid",
            "identity_file":"","session_name":"fixture","remote_dir":"/fixture"},
        "env":{"KEY":"old"},"env_overrides":{"KEY":"old"}
    }]});
    let mut iced: config::WorkspacesFile = serde_json::from_value(input.clone()).unwrap();
    let initial: gitterm_core::workspace_config::WorkspacesFile =
        serde_json::from_value(input.clone()).unwrap();
    let target = WorkspaceTarget::of(&initial.workspaces[0]);
    std::fs::write(root.path().join("workspaces.json"), input.to_string()).unwrap();
    let store = DesktopStore::open(root.path()).unwrap();
    iced.normalize_repos();
    // Frozen Iced set_workspace_env + workspace_config: its runtime home repo
    // retains the original env when the override changes and is then removed.
    let mut home_env = iced.workspaces[0].env.clone();
    for value in [Some("new"), None] {
        let ws = &mut iced.workspaces[0];
        let env = if ws.env_overrides.contains_key("KEY") {
            &mut ws.env_overrides
        } else {
            &mut home_env
        };
        if let Some(value) = value {
            env.insert("KEY".into(), value.into());
        } else {
            env.remove("KEY");
        }
        ws.env = config::layered_env(&home_env, &ws.env_overrides);
        store
            .edit_workspace_settings(
                target.clone(),
                Edit::Env(EnvEdit::Set {
                    key: "KEY".into(),
                    value: value.map(str::to_owned),
                }),
            )
            .wait()
            .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("workspaces.json")).unwrap(),
            serde_json::to_vec_pretty(&iced).unwrap()
        );
    }
    assert_eq!(iced.workspaces[0].env["KEY"], "old");
}
