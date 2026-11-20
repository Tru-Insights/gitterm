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

mod tab {
    pub use gitterm_core::chat::types::AgentBackendConfig;
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
