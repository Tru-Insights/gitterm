// Included in live_writes: all provider files, binaries and services are synthetic.
fn sidebar_entry(id: &str, cwd: &Path, backend: gitterm_core::chats::ChatBackend) -> gitterm_core::chats::ChatIndexEntry {
    gitterm_core::chats::ChatIndexEntry {
        id: id.into(), backend, path: cwd.join(format!("{id}.jsonl")), cwd: cwd.into(),
        repo_root: None, is_worktree: false, branch: None, title: "Placeholder chat".into(), latest: None,
        mtime: std::time::UNIX_EPOCH, size: 0, dead_cwd: !cwd.exists(), facts: None, root_id: None,
    }
}
fn sidebar_index(shell: &mut crate::Shell, entries: Vec<gitterm_core::chats::ChatIndexEntry>) {
    use gitterm_core::find::state::{Intent, Effect};
    let mut state = shell.find_state.borrow_mut();
    let effects = state.reduce(Intent::LoadIndex { machine: None, force: true, now: Instant::now() });
    let stamp = effects.into_iter().find_map(|e| if let Effect::LoadIndex(stamp) = e { Some(stamp) } else { None }).unwrap();
    state.reduce(Intent::IndexFinished { stamp, result: Ok(entries), now: Instant::now() });
}

#[gpui::test]
fn find_sidebar_live_resume_session_chat_and_rescue_match_iced(cx: &mut TestAppContext) {
    if child("find_sidebar_live_resume_session_chat_and_rescue_match_iced") { return; }
    use crate::find_sidebar::{FindOutcome, ResumeMode};
    use gitterm_core::chats::{ChatBackend, ChatRef};
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    let cwd = git.repo.join("nested");
    std::fs::create_dir(&cwd).unwrap();
    let rescue = git.repo.join("deleted/nested");
    golive10_workspace(&fixture, &git.repo, json!([]), json!([]));
    let (host, _) = start(&fixture, cx);
    let (shell, vcx) = golive10_shell(&fixture, &host, cx);
    let entries = vec![sidebar_entry("placeholder-cli", &cwd, ChatBackend::Codex), sidebar_entry("placeholder-chat", &cwd, ChatBackend::Claude), sidebar_entry("placeholder-rescue", &rescue, ChatBackend::Claude)];
    shell.update_in(vcx, |shell, window, cx| {
        sidebar_index(shell, entries);
        let owner = shell.find_owner(cx);
        shell.find_outcome(&owner, &FindOutcome::Resume { id: ChatRef::local("placeholder-cli"), mode: ResumeMode::Session }, window, cx);
        assert!(shell.owner.read(cx).error.is_none(), "{:?}", shell.owner.read(cx).error);
    });
    wait_until(vcx, |_| fake.file("ready", 1).exists());
    assert_eq!(fake.text("pwd", 1).trim(), cwd.to_str().unwrap());
    assert!(fake.text("args", 1).lines().collect::<Vec<_>>().windows(2).any(|args| args == ["resume", "placeholder-cli"]));
    assert!(fake.text("env", 1).contains("PLACEHOLDER_ENV=workspace"));
    assert!(fake.text("env", 1).contains("PLACEHOLDER_REPO=yes"));
    shell.update_in(vcx, |shell, window, cx| {
        let owner = shell.find_owner(cx);
        let before = shell.active(cx).sessions.len();
        shell.find_outcome(&owner, &FindOutcome::Resume { id: ChatRef::local("placeholder-cli"), mode: ResumeMode::Chat }, window, cx);
        assert_eq!(shell.active(cx).sessions.len(), before, "registry wins over requested mode");
        shell.find_outcome(&owner, &FindOutcome::Resume { id: ChatRef::local("placeholder-chat"), mode: ResumeMode::Chat }, window, cx);
        let id = shell.active(cx).active_session.unwrap();
        let pane = shell.active(cx).panes[&crate::session_pane_id(id)].read(cx);
        let crate::pane::PaneContent::PreviewChat(chat) = &pane.content else { panic!("native chat expected") };
        assert!(chat.read(cx).matches_chat("placeholder-chat"));
        assert_eq!(chat.read(cx).cwd(), cwd.as_path());
        shell.select_work_chat(shell.active(cx).workspace.id.0, "placeholder-chat", window, cx);
        assert_eq!(shell.active(cx).active_session, Some(id));
        assert_eq!(shell.active(cx).sessions.len(), before + 1);
        // Ordinary resume must not silently recreate a deleted directory.
        shell.find_outcome(&owner, &FindOutcome::Resume { id: ChatRef::local("placeholder-rescue"), mode: ResumeMode::Session }, window, cx);
        assert!(!rescue.exists());
        assert!(shell.owner.read(cx).error.as_deref().unwrap().contains("Recreate directory"));
        shell.find_outcome(&owner, &FindOutcome::Resume { id: ChatRef::local("placeholder-rescue"), mode: ResumeMode::RecreateDirectory }, window, cx);
        assert!(rescue.is_dir());
    });
    wait_until(vcx, |_| fake.file("ready", 2).exists());
    assert_eq!(fake.text("pwd", 2).trim(), rescue.to_str().unwrap());
    assert!(fake.text("args", 2).lines().collect::<Vec<_>>().windows(2).any(|args| args == ["--resume", "placeholder-rescue"]));
    host.update(vcx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn work_sidebar_find_sidebar_live_go_to_closed_owner_and_focus_child(cx: &mut TestAppContext) {
    if child("work_sidebar_find_sidebar_live_go_to_closed_owner_and_focus_child") { return; }
    use crate::find_sidebar::FindOutcome;
    use gitterm_core::chats::{ChatBackend, ChatRef};
    cx.background_executor.allow_parking();
    let _fake = LiveFake::new();
    let (fixture, git) = fixture();
    golive10_workspace(&fixture, &git.repo, json!([]), json!([]));
    let path = fixture.iced.join("workspaces.json");
    let mut file: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    file["workspaces"].as_array_mut().unwrap().push(json!({"name":"Owner", "abbrev":"OW", "dir":git.outside, "color":"green", "chats":["placeholder-root"], "active":false, "parked":{"note":"Placeholder", "parked_at":"2026-01-01T00:00:00Z"}, "tabs":[]}));
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
    let (config, _) = fixture.config();
    let lock = Arc::new(HostLock::acquire(&fixture.iced).unwrap());
    let desktop = crate::workspace_config::live_desktop_store(lock.clone()).unwrap();
    let services = Arc::new(ServiceContext::production(&fixture.iced, "owner-test").unwrap().for_gpui_live());
    let host = cx.update(|cx| TaskHostOwner::start_live(config, lock, services, cx)).unwrap();
    cx.update(|cx| cx.set_global(crate::workspace_config::LiveWorkspaces(desktop)));
    let (shell, vcx) = golive10_shell(&fixture, &host, cx);
    shell.update_in(vcx, |shell, window, cx| {
        let mut entry = sidebar_entry("placeholder-child", &git.repo, ChatBackend::Claude);
        entry.root_id = Some("placeholder-root".into());
        sidebar_index(shell, vec![entry]);
        let owner = shell.find_owner(cx);
        shell.find_outcome(&owner, &FindOutcome::GoToChatOwner { id: ChatRef::local("placeholder-child") }, window, cx);
    });
    wait_until(vcx, |cx| shell.read_with(cx, |s, cx| s.active(cx).workspace.name == "Owner" && !s.owner.read(cx).workspace_save_pending));
    shell.update_in(vcx, |shell, window, cx| {
        assert!(shell.owner.read(cx).error.is_none(), "{:?}", shell.owner.read(cx).error);
        let owner_id = shell.active(cx).workspace.id;
        let session = shell.active(cx).active_session;
        let before = shell.active(cx).sessions.len();
        shell.select_workspace_id(crate::WorkspaceId(1), window, cx);
        shell.work_outcome(&crate::work_sidebar::WorkSidebarOutcome::GoToChatOwner { chat: "placeholder-child".into() }, window, cx);
        assert_eq!(shell.active(cx).workspace.id, owner_id);
        assert_eq!(shell.active(cx).active_session, session);
        assert_eq!(shell.active(cx).sessions.len(), before);
    });
    let fresh = fixture._dir.path().join("unowned");
    std::fs::create_dir(&fresh).unwrap();
    shell.update_in(vcx, |shell, window, cx| {
        sidebar_index(shell, vec![sidebar_entry("placeholder-unowned", &fresh, ChatBackend::Claude)]);
        shell.resume_indexed_chat(&ChatRef::local("placeholder-unowned"), None, crate::find_sidebar::ResumeMode::Chat, None, window, cx).unwrap();
    });
    wait_until(vcx, |cx| shell.read_with(cx, |s, cx| s.active(cx).workspace.name == "unowned" && !s.owner.read(cx).workspace_save_pending));
    shell.read_with(vcx, |s, cx| {
        assert!(s.owner.read(cx).error.is_none(), "{:?}", s.owner.read(cx).error);
        let saved = s.owner.read(cx).iced_workspace(s.active(cx).workspace.id).unwrap();
        assert_eq!(saved.dir, fresh.to_str().unwrap());
        assert!(saved.chats.is_empty(), "a resumed folder is not a new owning workspace");
    });
    host.update(vcx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn work_sidebar_find_sidebar_live_index_older_chats_writes_and_refreshes(cx: &mut TestAppContext) {
    if child("work_sidebar_find_sidebar_live_index_older_chats_writes_and_refreshes") { return; }
    use crate::find_sidebar::FindOutcome;
    cx.background_executor.allow_parking();
    let _fake = LiveFake::new();
    let (fixture, git) = fixture();
    golive10_workspace(&fixture, &git.repo, json!([]), json!([]));
    let (config, _) = fixture.config();
    let lock = Arc::new(HostLock::acquire(&fixture.iced).unwrap());
    let desktop = crate::workspace_config::live_desktop_store(lock.clone()).unwrap();
    let services = Arc::new(ServiceContext::production(&fixture.iced, "backfill-test").unwrap().for_gpui_live());
    let host = cx.update(|cx| TaskHostOwner::start_live(config, lock, services, cx)).unwrap();
    cx.update(|cx| cx.set_global(crate::workspace_config::LiveWorkspaces(desktop)));
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let projects = home.join(".claude/projects/placeholder");
    std::fs::create_dir_all(&projects).unwrap();
    let (shell, vcx) = golive10_shell(&fixture, &host, cx);
    for (id, work) in [("placeholder-work", true), ("placeholder-find", false)] {
        let transcript = json!({"type":"user", "cwd":git.repo, "timestamp":"2026-01-01T00:00:00Z", "message":{"content":"Implement TRU-274 placeholder"}}).to_string()+"\n";
        std::fs::write(projects.join(format!("{id}.jsonl")), transcript).unwrap();
        shell.update_in(vcx, |shell, window, cx| {
            shell.find_home = Some(home.clone());
            if work { shell.work_outcome(&crate::work_sidebar::WorkSidebarOutcome::IndexOlderChats, window, cx); }
            else { let owner = shell.find_owner(cx); shell.find_outcome(&owner, &FindOutcome::IndexOlderChats, window, cx); }
            assert!(shell.chat_backfill_running);
        });
        wait_until(vcx, |cx| shell.read_with(cx, |shell, _| !shell.chat_backfill_running));
        let facts = gitterm_core::chat_facts::load_facts(&fixture.iced.join(format!("chat-meta/claude-{id}.json"))).unwrap().unwrap();
        assert!(facts.backfilled && facts.documents_backfilled);
        assert_eq!(facts.issue_keys, ["TRU-274"]);
        wait_until(vcx, |cx| shell.read_with(cx, |shell, _| shell.find_state.borrow().index(&None).is_some_and(|index| index.entries.iter().any(|e| e.id == id && e.facts.is_some()))));
    }
    host.update(vcx, |host, cx| host.quit(cx));
}


fn sidebar_review_catalog(fixture: &Fixture, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = fixture.iced.join("workspaces.json");
    let mut file: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut file);
    std::fs::write(path, serde_json::to_vec(&file).unwrap()).unwrap();
}

fn sidebar_review_start(fixture: &Fixture, cx: &mut TestAppContext) -> Entity<TaskHostOwner> {
    let (config, _) = fixture.config();
    let lock = Arc::new(HostLock::acquire(&fixture.iced).unwrap());
    let desktop = crate::workspace_config::live_desktop_store(lock.clone()).unwrap();
    let services = Arc::new(ServiceContext::production(&fixture.iced, std::process::id().to_string()).unwrap().for_gpui_live());
    let host = cx.update(|cx| TaskHostOwner::start_live(config, lock, services, cx)).unwrap();
    cx.update(|cx| cx.set_global(crate::workspace_config::LiveWorkspaces(desktop)));
    host
}

#[gpui::test]
fn work_sidebar_review_f2_done_owner_discards_old_sessions_on_chat_resume(cx: &mut TestAppContext) {
    if child("work_sidebar_review_f2_done_owner_discards_old_sessions_on_chat_resume") { return; }
    use gitterm_core::chats::{ChatBackend, ChatRef};
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    golive10_workspace(&fixture, &git.repo, json!([]), json!([]));
    sidebar_review_catalog(&fixture, |file| {
        file["workspaces"].as_array_mut().unwrap().push(json!({
            "name":"Done owner", "abbrev":"DO", "dir":git.outside, "color":"green",
            "chats":["placeholder-requested"], "active":false,
            "done_at":"2026-01-01T00:00:00Z",
            "parked":{"note":"Finished", "parked_at":"2026-01-01T00:00:00Z"},
            "active_tab":0, "tabs":[{"dir":git.repo,"startup_command":"claude --resume placeholder-old", "chat_session_id":"placeholder-old"}],
            "bottom_terminals":[{"dir":git.repo}], "env":{"KEEP":"yes"}
        }));
    });
    let host = sidebar_review_start(&fixture, cx);
    let (shell, vcx) = golive10_shell(&fixture, &host, cx);
    shell.update_in(vcx, |shell, window, cx| {
        sidebar_index(shell, vec![sidebar_entry("placeholder-requested", &git.repo, ChatBackend::Claude)]);
        shell.go_to_chat_owner(&ChatRef::local("placeholder-requested"), window, cx);
    });
    wait_until(vcx, |cx| shell.read_with(cx, |s, cx| s.active(cx).workspace.name == "Done owner" && !s.owner.read(cx).workspace_save_pending));
    shell.read_with(vcx, |s, cx| {
        assert!(s.owner.read(cx).error.is_none(), "{:?}", s.owner.read(cx).error);
        let saved = s.owner.read(cx).iced_workspace(s.active(cx).workspace.id).unwrap();
        assert!(saved.done_at.is_none());
        assert_eq!(saved.chats, ["placeholder-requested"]);
        assert_eq!(saved.env["KEEP"], "yes");
        assert!(saved.bottom_terminals.is_empty());
        assert!(saved.tabs.iter().all(|tab| tab.startup_command.is_none() && tab.chat_session_id.as_deref() != Some("placeholder-old")));
        assert!(crate::bottom_pane_ids(s.active(cx), cx).is_empty());
        let session = s.active(cx).sessions.iter().find(|session| Some(session.id) == s.active(cx).active_session).unwrap();
        assert_eq!(s.find_conversation(s.active(cx), session, cx), Some(ChatRef::local("placeholder-requested")));
    });
    // Repeat from a Done workspace still mounted in this process, including
    // its stopped bottom-pane inventory rather than only the persisted DTOs.
    shell.update_in(vcx, |shell, window, cx| {
        let workspace = shell.active(cx).workspace.id;
        let entry = sidebar_entry("placeholder-stale-native", &git.repo, ChatBackend::Claude);
        shell.open_saved_live_chat(workspace, "placeholder-stale-native", Some(entry), window, cx).unwrap();
        shell.new_terminal_at(gpui_component::dock::DockPlacement::Bottom, Some(git.repo.clone()), window, cx);
        assert!(!crate::bottom_pane_ids(shell.active(cx), cx).is_empty());
        let target = shell.active(cx).park_target(cx);
        shell.park_intent(Some(&target), gitterm_core::park::Intent::Close(target.uid), window, cx);
    });
    wait_until(vcx, |cx| shell.read_with(cx, |s, cx| !s.owner.read(cx).workspace_save_pending));
    shell.update_in(vcx, |shell, window, cx| {
        shell.go_to_chat_owner(&ChatRef::local("placeholder-requested"), window, cx);
    });
    wait_until(vcx, |cx| shell.read_with(cx, |s, cx| !s.owner.read(cx).workspace_save_pending));
    shell.read_with(vcx, |s, cx| {
        assert_eq!(s.active(cx).workspace.name, "Done owner");
        assert!(crate::bottom_pane_ids(s.active(cx), cx).is_empty());
        assert!(s.active(cx).sessions.iter().all(|session| s.find_conversation(s.active(cx), session, cx) != Some(ChatRef::local("placeholder-stale-native"))));
    });
    host.update(vcx, |host, cx| host.quit(cx));
    assert!(!fake.file("ready", 1).exists(), "Done work ran an old startup command");
}

#[gpui::test]
fn find_sidebar_review_f3_parked_owner_focuses_requested_chat_after_save(cx: &mut TestAppContext) {
    if child("find_sidebar_review_f3_parked_owner_focuses_requested_chat_after_save") { return; }
    use gitterm_core::chats::{ChatBackend, ChatRef};
    use crate::find_sidebar::ResumeMode;
    cx.background_executor.allow_parking();
    let _fake = LiveFake::new();
    let (fixture, git) = fixture();
    golive10_workspace(&fixture, &git.repo, json!([]), json!([]));
    sidebar_review_catalog(&fixture, |file| {
        file["workspaces"][0]["chats"] = json!(["placeholder-wanted"]);
        file["workspaces"].as_array_mut().unwrap().push(json!({"name":"Other", "abbrev":"OT", "dir":git.outside, "color":"green", "tabs":[]}));
    });
    let host = sidebar_review_start(&fixture, cx);
    let (shell, vcx) = golive10_shell(&fixture, &host, cx);
    for mode in [gitterm_core::park::Mode::Stop, gitterm_core::park::Mode::KeepRunning] {
        shell.update_in(vcx, |shell, window, cx| {
            let workspace = crate::WorkspaceId(1);
            for id in ["placeholder-wanted", "placeholder-other"] {
                let entry = sidebar_entry(id, &git.repo, ChatBackend::Claude);
                sidebar_index(shell, vec![entry.clone()]);
                shell.resume_indexed_chat(&ChatRef::local(id), Some(entry), ResumeMode::Chat, Some(workspace), window, cx).unwrap();
            }
            sidebar_index(shell, vec![sidebar_entry("placeholder-wanted", &git.repo, ChatBackend::Claude)]);
            let target = shell.workspace(workspace).park_target(cx);
            shell.park_workspace(&target, mode, window, cx);
        });
        wait_until(vcx, |cx| shell.read_with(cx, |s, cx| !s.owner.read(cx).workspace_save_pending));
        shell.update_in(vcx, |shell, window, cx| {
            assert_eq!(shell.active(cx).workspace.name, "Other");
            if mode == gitterm_core::park::Mode::Stop {
                shell.go_to_chat_owner(&ChatRef::local("placeholder-wanted"), window, cx);
            } else {
                shell.resume_indexed_chat(&ChatRef::local("placeholder-wanted"), None, ResumeMode::Chat, None, window, cx).unwrap();
            }
        });
        wait_until(vcx, |cx| shell.read_with(cx, |s, cx| !s.owner.read(cx).workspace_save_pending));
        shell.read_with(vcx, |s, cx| {
            assert_eq!(s.active(cx).workspace.name, "Placeholder");
            let active = s.active(cx);
            let session = active.sessions.iter().find(|session| Some(session.id) == active.active_session).unwrap();
            assert_eq!(s.find_conversation(active, session, cx), Some(ChatRef::local("placeholder-wanted")));
            assert_eq!(active.sessions.iter().filter(|session| s.find_conversation(active, session, cx) == Some(ChatRef::local("placeholder-wanted"))).count(), 1);
        });
    }
    host.update(vcx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn find_sidebar_review_f4_recreate_managed_worktree_uses_repo_workspace(cx: &mut TestAppContext) {
    if child("find_sidebar_review_f4_recreate_managed_worktree_uses_repo_workspace") { return; }
    use gitterm_core::chats::{ChatBackend, ChatRef};
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    let deleted = fixture._dir.path().join("deleted-task-worktree/nested");
    let mut record = task("placeholder-task", TaskLifecycle::Ready);
    record.workspace.location = gitterm_core::tasks::WorkspaceLocationIdentity::Local { directory: git.repo.clone() };
    record.worktree.path = Some(deleted.parent().unwrap().into());
    record.worktree.state = gitterm_core::tasks::TaskWorktreeState::Missing;
    fixture.write_iced(vec![record], vec![]);
    golive10_workspace(&fixture, &git.repo, json!([]), json!([]));
    let host = sidebar_review_start(&fixture, cx);
    let (shell, vcx) = golive10_shell(&fixture, &host, cx);
    shell.update_in(vcx, |shell, window, cx| {
        let entry = sidebar_entry("placeholder-rescue", &deleted, ChatBackend::Claude);
        shell.resume_indexed_chat(&ChatRef::local("placeholder-rescue"), Some(entry), crate::find_sidebar::ResumeMode::RecreateDirectory, None, window, cx).unwrap();
    });
    wait_until(vcx, |_| fake.file("ready", 1).exists());
    shell.read_with(vcx, |s, cx| {
        assert_eq!(s.active(cx).workspace.name, "Placeholder");
        let saved = s.owner.read(cx).iced_workspace(s.active(cx).workspace.id).unwrap();
        assert_eq!(saved.dir, git.repo.to_str().unwrap());
        assert_eq!(s.owner.read(cx).iced_catalog().unwrap().workspaces.len(), 1);
    });
    assert!(deleted.is_dir());
    assert_eq!(fake.text("pwd", 1).trim(), deleted.canonicalize().unwrap().to_str().unwrap());
    assert!(fake.text("args", 1).contains("placeholder-rescue"));
    host.update(vcx, |host, cx| host.quit(cx));
}

#[gpui::test]
fn work_sidebar_review_f5_missing_cwd_still_navigates_to_open_or_parked_owner(cx: &mut TestAppContext) {
    if child("work_sidebar_review_f5_missing_cwd_still_navigates_to_open_or_parked_owner") { return; }
    use gitterm_core::chats::{ChatBackend, ChatRef};
    cx.background_executor.allow_parking();
    let fake = LiveFake::new();
    let (fixture, git) = fixture();
    let missing = fixture._dir.path().join("missing-recorded-directory");
    golive10_workspace(&fixture, &git.repo, json!([]), json!([]));
    sidebar_review_catalog(&fixture, |file| {
        file["workspaces"].as_array_mut().unwrap().push(json!({"name":"Owner", "abbrev":"OW", "dir":git.outside, "color":"green", "chats":["placeholder-missing"], "tabs":[]}));
    });
    let host = sidebar_review_start(&fixture, cx);
    let (shell, vcx) = golive10_shell(&fixture, &host, cx);
    for parked in [false, true] {
        shell.update_in(vcx, |shell, window, cx| {
            sidebar_index(shell, vec![sidebar_entry("placeholder-missing", &missing, ChatBackend::Claude)]);
            shell.select_workspace_id(crate::WorkspaceId(1), window, cx);
            shell.go_to_chat_owner(&ChatRef::local("placeholder-missing"), window, cx);
        });
        wait_until(vcx, |cx| shell.read_with(cx, |s, cx| !s.owner.read(cx).workspace_save_pending));
        shell.read_with(vcx, |s, cx| {
            assert_eq!(s.active(cx).workspace.name, "Owner", "parked={parked}");
            assert!(s.owner.read(cx).error.as_deref().unwrap().contains("Recreate directory"));
            assert!(s.active(cx).sessions.iter().all(|session| s.find_conversation(s.active(cx), session, cx) != Some(ChatRef::local("placeholder-missing"))));
        });
        assert!(!missing.exists());
        if !parked {
            shell.update_in(vcx, |shell, window, cx| {
                let target = shell.active(cx).park_target(cx);
                shell.park_workspace(&target, gitterm_core::park::Mode::Stop, window, cx);
            });
            wait_until(vcx, |cx| shell.read_with(cx, |s, cx| !s.owner.read(cx).workspace_save_pending));
        }
    }
    host.update(vcx, |host, cx| host.quit(cx));
    assert!(!fake.file("ready", 1).exists());
}
