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
