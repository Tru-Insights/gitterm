//! Actual bar outcomes on the headless platform; all paths and processes are fixtures.
use super::*;
use crate::remote_client::RemoteClient;
use crate::remote_shell_tests::{FakeRemote, click, draw};
use gitterm_core::{
    desktop_store::DesktopStore,
    remote::{
        client::RemoteAgentHandshake,
        config::RemoteAgentConfig,
        phone_panel::PairedPhoneInfo,
        source::SourceDirListing,
        ssh_sessions::{RemoteHostConfig, RemoteSessionsSnapshot},
    },
    workspace_config::WorkspacesFile,
};
use gpui::{Entity, TestAppContext, VisualTestContext};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// Subprocess isolation also keeps PATH/HOME out of concurrent GPUI tests.
fn child(name: &str) -> bool {
    if std::env::var("GITTERM_BAR_TEST").as_deref() == Ok(name) {
        return false;
    }
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let bin = root.path().join("bin");
    for path in [&home, &bin] {
        std::fs::create_dir(path).unwrap();
    }
    let ssh = bin.join("ssh");
    std::fs::write(&ssh, "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$GITTERM_BAR_SSH_LOG\"\nprintf 'box-app\\t1\\t0\\n'\n").unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("workspace_bar::live_tests::{name}"),
            "--nocapture",
        ])
        .env("GITTERM_BAR_TEST", name)
        .env("HOME", &home)
        .env("GITTERM_V6_CONFIG_DIR", root.path().join("v6"))
        // No fallback to a real ssh, even if a fixture misconfigures the command.
        .env("PATH", &bin)
        .env("GITTERM_BAR_SSH_LOG", root.path().join("ssh-argv"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child {name}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

struct BarRemote(FakeRemote);
impl RemoteClient for BarRemote {
    fn handshake(&self, remote: &RemoteAgentConfig) -> Result<RemoteAgentHandshake, String> {
        self.0.handshake(remote)
    }
    fn list_dir(&self, remote: &RemoteAgentConfig, dir: String) -> SourceDirListing {
        self.0.list_dir(remote, dir)
    }
    fn list_chats(
        &self,
        remote: &RemoteAgentConfig,
    ) -> Result<Vec<gitterm_core::chats::ChatIndexEntry>, String> {
        self.0.list_chats(remote)
    }
    fn list_sessions(&self, host: &RemoteHostConfig) -> RemoteSessionsSnapshot {
        // This is Iced's collector, executing only the subprocess's fake ssh.
        gitterm_core::remote::ssh_sessions::collect_remote_sessions(host)
    }
    fn list_phones(&self, remote: &RemoteAgentConfig) -> Result<Vec<PairedPhoneInfo>, String> {
        self.0.list_phones(remote)
    }
}
fn session() -> serde_json::Value {
    serde_json::json!({"label":"app","host_name":"box","ssh_target":"dev@box.example", "identity_file":"/tmp/placeholder-key","tmux_path":"/opt/bin/tmux", "session_name":"box-app","remote_dir":"/srv/app","shell_command":"/bin/sh -il"})
}
fn fixture(
    cx: &mut TestAppContext,
    live: bool,
    closed: bool,
) -> (
    tempfile::TempDir,
    Entity<crate::Shell>,
    Arc<BarRemote>,
    &mut VisualTestContext,
) {
    let (root, shell, cx) = crate::h2_tests::fixture_with(cx, false, |file| {
        for workspace in file["workspaces"].as_array_mut().unwrap() {
            workspace["tabs"] = serde_json::json!([]);
            workspace["chats"] = serde_json::json!([]);
        }
        file["future_top"] = serde_json::json!({"keep":17});
        file["workspaces"][0]["future_workspace"] = serde_json::json!([1, 2, 3]);
        file["remote_sessions"] = serde_json::json!([session()]);
        file["remote_agents"] = serde_json::json!([{"id":"example","name":"Example", "endpoint":"http://127.0.0.1:9", "auth":{"type":"token","token_ref":"env:PLACEHOLDER_UNSET"}}]);
        file["collapsed_repo_groups"] = serde_json::json!(["agent:example"]);
        if closed {
            file["workspaces"].as_array_mut().unwrap().push(serde_json::json!({
                "name":"Saved remote", "abbrev":"SR", "dir":"/tmp/saved-home", "color":"pink", "active":false,
                "tabs":[{"dir":"/tmp/saved-home","startup_command":"MUST NOT RUN"}],
                "remote_session":session(), "guidelines_off":true, "env_overrides":{"PLACEHOLDER":"saved"},
                "future_workspace":{"keep":true}, "parked":{"note":"note","parked_at":"2026-01-01T00:00:00Z"}
            }));
        }
    });
    let client = Arc::new(BarRemote(FakeRemote::default()));
    cx.update(|_, cx| {
        shell.update(cx, |shell, _| shell.set_remote_client(client.clone()));
        if live {
            let source = root.path().join("iced");
            let store = DesktopStore::open(&source).unwrap();
            cx.set_global(crate::workspace_config::LiveWorkspaces(store));
            let services =
                gitterm_core::preview::service::ServiceContext::production(&source, "bar-test")
                    .unwrap();
            crate::chat::policy::install_live(services, cx).unwrap();
        }
    });
    draw(cx);
    (root, shell, client, cx)
}
fn saved(root: &Path) -> WorkspacesFile {
    serde_json::from_slice(&std::fs::read(root.join("iced/workspaces.json")).unwrap()).unwrap()
}
fn outcome(shell: &Entity<crate::Shell>, outcome: Outcome, cx: &mut VisualTestContext) {
    shell.update_in(cx, |shell, window, cx| {
        shell.bar_outcome(outcome, window, cx)
    });
    draw(cx);
}
fn menu_action(
    shell: &Entity<crate::Shell>,
    machine: &str,
    label: &str,
    cx: &mut VisualTestContext,
) {
    let (position, index) = shell.read_with(cx, |shell, cx| {
        let input = shell.bar_input(cx);
        let machines = input.machines();
        let position = machines.iter().position(|m| m.key == machine).unwrap();
        let index = input
            .repo_actions(&machines[position], true)
            .iter()
            .position(|l| l == label)
            .unwrap();
        (position, index)
    });
    click("pstrip-repos", cx);
    click(&format!("pstrip-repo-action-{position}-{index}"), cx);
}

#[gpui::test]
fn workspace_bar_live_add_repo_reuses_work_picker_and_iced_repo_fields(cx: &mut TestAppContext) {
    if child("workspace_bar_live_add_repo_reuses_work_picker_and_iced_repo_fields") {
        return;
    }
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let (root, shell, _, cx) = fixture(cx, true, false);
    let path = root.path().join("added-repo");
    std::fs::create_dir(&path).unwrap();
    let before = saved(root.path());
    menu_action(&shell, "local", "Add a repo to Folder…", cx);
    let picker = shell.read_with(cx, |shell, _| {
        shell
            .folder_picker()
            .expect("same Work sidebar picker")
            .clone()
    });
    picker.update_in(cx, |picker, window, cx| {
        picker.navigate(
            gitterm_core::remote::source::SourcePath::Local(path.clone()),
            window,
            cx,
        )
    });
    draw(cx);
    picker.update(cx, |picker, cx| picker.confirm(cx));
    draw(cx);
    let after = saved(root.path());
    let mut expected = before;
    let target = gitterm_core::workspace_config::WorkspaceTarget::of(&expected.workspaces[0]);
    expected
        .edit_repo(
            &target,
            gitterm_core::workspace_config::RepoEdit::Add {
                dir: path,
                detected_command: None,
            },
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[gpui::test]
fn workspace_bar_live_hide_show_saves_iced_collapsed_group(cx: &mut TestAppContext) {
    if child("workspace_bar_live_hide_show_saves_iced_collapsed_group") {
        return;
    }
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let (root, shell, _, cx) = fixture(cx, true, false);
    let before = serde_json::to_value(saved(root.path())).unwrap();
    for (label, groups) in [
        (
            "Keep repos in this menu only",
            vec!["agent:example", "local"],
        ),
        ("Show repos on the bar too", vec!["agent:example"]),
    ] {
        menu_action(&shell, "local", label, cx);
        let mut expected = before.clone();
        expected["collapsed_repo_groups"] = serde_json::json!(groups);
        assert_eq!(serde_json::to_value(saved(root.path())).unwrap(), expected);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.bar_input(cx).repos_on_bar, groups.len() == 1)
        });
    }
}

#[gpui::test]
fn workspace_bar_live_reconnect_retries_handshake_without_ssh_or_writes(cx: &mut TestAppContext) {
    if child("workspace_bar_live_reconnect_retries_handshake_without_ssh_or_writes") {
        return;
    }
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let (root, shell, client, cx) = fixture(cx, true, false);
    let before = crate::remote_shell_tests::snapshot(&root.path().join("iced"));
    shell.update(cx, |shell, _| {
        shell.remote.connections.insert(
            "example".into(),
            crate::remote_shell::Connection::Unreachable,
        );
    });
    let count = client.0.calls().len();
    menu_action(&shell, "agent:example", "Reconnect…", cx);
    assert_eq!(client.0.calls()[count..], ["handshake example"]);
    shell.read_with(cx, |shell, _| {
        assert!(matches!(
            shell.remote.connections.get("example"),
            Some(crate::remote_shell::Connection::Connected { .. })
        ))
    });
    assert_eq!(
        crate::remote_shell_tests::snapshot(&root.path().join("iced")),
        before
    );
    assert!(!PathBuf::from(std::env::var_os("GITTERM_BAR_SSH_LOG").unwrap()).exists());
}

#[gpui::test]
fn workspace_bar_live_remote_session_opens_and_refreshes_with_iced_ssh_argv(
    cx: &mut TestAppContext,
) {
    if child("workspace_bar_live_remote_session_opens_and_refreshes_with_iced_ssh_argv") {
        return;
    }
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let (root, shell, _, cx) = fixture(cx, true, false);
    let before = serde_json::to_value(saved(root.path())).unwrap();
    click("pstrip-repos", cx);
    click("pstrip-remote-workspace-0", cx);
    let after = saved(root.path());
    let new = &after.workspaces[2];
    assert_eq!(new.name, "box:app");
    assert_eq!(new.abbrev, "BO");
    assert_eq!(new.dir, std::env::var("HOME").unwrap());
    assert_eq!(new.color, WorkspaceColor::Lavender);
    assert!(new.tabs.is_empty());
    assert_eq!(new.home.as_deref(), Some(new.dir.as_str()));
    assert_eq!(new.repos, std::slice::from_ref(&new.dir));
    assert_eq!(serde_json::to_value(new.remote_session.as_ref().unwrap()).unwrap(), serde_json::to_value(serde_json::from_value::<gitterm_core::workspace_config::RemoteSessionConfig>(session()).unwrap()).unwrap());
    assert_eq!(after.active_workspace, 2);
    let value = serde_json::to_value(&after).unwrap();
    assert_eq!(value["workspaces"][0], before["workspaces"][0]);
    assert_eq!(value["workspaces"][1], before["workspaces"][1]);
    assert_eq!(value["future_top"], before["future_top"]);
    let argv = std::fs::read_to_string(std::env::var_os("GITTERM_BAR_SSH_LOG").unwrap()).unwrap();
    assert_eq!(
        argv.lines().collect::<Vec<_>>(),
        [
            "-i",
            "/tmp/placeholder-key",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "dev@box.example",
            "/opt/bin/tmux list-sessions -F '#{session_name}\t#{session_windows}\t#{session_attached}' 2>/dev/null || true"
        ]
    );
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.active_workspace(cx), Some(WorkspaceId(3)));
        assert!(shell.bar_input(cx).remote_workspaces.is_empty());
        assert_eq!(shell.owner.read(cx).iced_catalog().unwrap().repos.len(), 2);
        assert_eq!(shell.remote.sessions.sessions[0].name, "box-app");
    });
    outcome(&shell, Outcome::OpenRemoteWorkspace("box-app".into()), cx);
    assert_eq!(saved(root.path()).workspaces.len(), 3);
}

#[gpui::test]
fn workspace_bar_live_remote_reopens_saved_settings_like_iced(cx: &mut TestAppContext) {
    if child("workspace_bar_live_remote_reopens_saved_settings_like_iced") {
        return;
    }
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let (root, shell, _, cx) = fixture(cx, true, true);
    click("pstrip-repos", cx);
    click("pstrip-remote-workspace-0", cx);
    let file = saved(root.path());
    let saved = &file.workspaces[2];
    assert!(saved.active && saved.parked.is_none() && saved.done_at.is_none());
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.active_workspace(cx), Some(WorkspaceId(3)))
    });
    assert_eq!(saved.name, "Saved remote");
    assert_eq!(saved.color, WorkspaceColor::Pink);
    assert_eq!(saved.dir, "/tmp/saved-home");
    assert!(saved.guidelines_off && saved.tabs.is_empty());
    assert_eq!(saved.env_overrides["PLACEHOLDER"], "saved");
    assert_eq!(
        saved.extra["future_workspace"],
        serde_json::json!({"keep":true})
    );
}

#[gpui::test]
fn workspace_bar_preview_all_four_actions_refuse_without_writes_or_ssh(cx: &mut TestAppContext) {
    if child("workspace_bar_preview_all_four_actions_refuse_without_writes_or_ssh") {
        return;
    }
    let _cleanup = crate::test_app::Cleanup::new(cx);
    let (root, shell, client, cx) = fixture(cx, false, false);
    let before = crate::remote_shell_tests::snapshot(&root.path().join("iced"));
    let calls = client.0.calls();
    for (action, label) in [
        (Outcome::AddRepo, "Add a repo to Folder…"),
        (
            Outcome::ToggleReposZone("local".into()),
            "Keep repos in this menu only",
        ),
        (Outcome::Reconnect("agent:example".into()), "Reconnect…"),
        (Outcome::OpenRemoteWorkspace("box-app".into()), "+ box:app"),
    ] {
        outcome(&shell, action, cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(
                shell.owner.read(cx).error.as_deref(),
                Some(format!("{label} · not available in preview").as_str())
            );
            assert!(shell.folder_picker().is_none());
            assert_eq!(shell.workspaces.len(), 2);
        });
        assert_eq!(
            crate::remote_shell_tests::snapshot(&root.path().join("iced")),
            before
        );
        assert_eq!(client.0.calls(), calls);
    }
    assert!(!PathBuf::from(std::env::var_os("GITTERM_BAR_SSH_LOG").unwrap()).exists());
}
