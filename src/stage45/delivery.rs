//! TRU-274 H4c: delivering finished results into the requesting chat,
//! pinned through the Iced paths (Tick, turn end, Send, Dismiss). The chat's
//! Claude harness is a channel port: a submission is a `SendUserMessage` on
//! it, and a dropped receiver is a rejected submission. No binary runs.
use super::*;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use HarnessCommand as Command;

const CHAT: usize = 10;

/// Tab 10 is a local Claude chat with a store; `ids` are finished consults
/// it requested, oldest first.
fn delivery_app(dir: &Path, ids: &[&str]) -> App {
    let mut app = delegations_app(dir);
    let caller = app.delegation_caller_for_tab(CHAT).unwrap();
    for id in ids {
        let new = gitterm::delegations::consult_delegation(
            &task_mcp::ConsultDelegationRequest {
                brief: format!("Inspect {id}"),
                ..Default::default()
            },
            &caller,
            None,
            None,
        )
        .unwrap();
        let mut delegation = gitterm::tasks::Delegation::new_requested(new, "t0");
        delegation.delegation_id = id.to_string();
        let store = app.task_host.store_mut().as_mut().unwrap();
        store.insert_delegation(delegation).unwrap();
        store
            .complete_delegation(
                id,
                gitterm::tasks::DelegationResult {
                    handoff: Some(TaskHandoff {
                        summary: format!("Result {id}"),
                        decisions: vec![],
                        next_steps: vec![],
                        blockers: vec![],
                        updated_by_session_id: None,
                        updated_at: "t1".into(),
                    }),
                    findings: None,
                    reviewed: None,
                },
                "t1",
            )
            .unwrap();
    }
    app
}

fn delegations_app(dir: &Path) -> App {
    let mut app = app_with_tabs(dir);
    app.task_host
        .replace_store(Some(TaskStore::load(dir.join("tasks.json")).unwrap()));
    app.workspaces[0].dir = dir.into();
    app.workspaces[0].location = WorkspaceLocation::Local { root: dir.into() };
    app.workspaces[0].tabs[0].repo_path = dir.into();
    app.workspaces[0].tabs[0].kind =
        TabKind::Agent(AgentSession::new(tab::AgentBackendConfig::Claude {
            model: "default".into(),
            permission_mode: None,
            effort: None,
        }));
    app
}

fn session(app: &mut App) -> &mut AgentSession {
    app.workspaces[0].tabs[0].agent_session_mut().unwrap()
}

/// A live harness port on the chat; the receiver sees each submission.
fn attach_chat(app: &mut App) -> UnboundedReceiver<Command> {
    let (commands, receiver) = unbounded_channel();
    session(app).harness = Some(gitterm::harness::HarnessSession::Claude(
        gitterm::harness::claude::ClaudeSession::from_port(commands),
    ));
    receiver
}

/// The prompts submitted since the last call.
fn submitted(port: &mut UnboundedReceiver<Command>) -> Vec<String> {
    let mut prompts = Vec::new();
    while let Ok(command) = port.try_recv() {
        if let Command::SendUserMessage(prompt) = command {
            prompts.push(prompt.text);
        }
    }
    prompts
}

fn busy(app: &mut App) {
    session(app).reduce(ChatIntent::SubmissionAccepted);
}

/// The chat's turn ends, as its turn-end event leaves it.
fn turn_ends(app: &mut App) {
    session(app).reduce(ChatIntent::Event(tab::AgentEvent::Other(
        json!({"type": "done"}),
    )));
    let _ = app.flush_held_delegation(CHAT);
}

fn tick(app: &mut App) {
    let _ = app.auto_deliver_delegations();
}

fn delivered(app: &App, id: &str) -> bool {
    app.task_host
        .store()
        .as_ref()
        .unwrap()
        .delegation(id)
        .unwrap()
        .is_delivered()
}

fn saved_delivered(dir: &Path, id: &str) -> bool {
    TaskStore::load(dir.join("tasks.json"))
        .unwrap()
        .delegation(id)
        .unwrap()
        .is_delivered()
}

fn held(app: &App) -> Vec<String> {
    app.task_host
        .delegations()
        .held(CHAT)
        .map(|held| held.iter().map(|s| s.delegation_id.clone()).collect())
        .unwrap_or_default()
}

/// One result reaches its chat once, however many flushes follow: Tick,
/// turn end and a manual Send after acceptance never resubmit it.
#[test]
fn delivery_pending_flush_does_not_resubmit() {
    if isolated("delivery_pending_flush_does_not_resubmit", "delivery") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = delivery_app(dir.path(), &["a"]);
    let mut port = attach_chat(&mut app);
    tick(&mut app);
    let first = submitted(&mut port);
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(first[0].contains("Result a"), "{first:?}");
    assert!(delivered(&app, "a"));
    assert!(saved_delivered(dir.path(), "a"));
    assert!(held(&app).is_empty());
    // The accepted submission started a turn: a second Tick in it, its
    // turn end and a Tick after it submit nothing more.
    tick(&mut app);
    turn_ends(&mut app);
    tick(&mut app);
    let _ = app.update_event(Event::DelegationSendRequested(CHAT, "a".into(), vec![]));
    assert_eq!(submitted(&mut port), Vec::<String>::new());
    assert!(held(&app).is_empty());
}

/// A rejected submission leaves the result undelivered and it is sent on
/// the next Tick. An accepted one whose receipt cannot be saved is never
/// sent again; the receipt is saved on a later Tick.
#[test]
fn delivery_rejection_and_receipt_failure_preserve_retry() {
    if isolated(
        "delivery_rejection_and_receipt_failure_preserve_retry",
        "delivery",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = delivery_app(dir.path(), &["a"]);
    drop(attach_chat(&mut app));
    tick(&mut app);
    assert!(!delivered(&app, "a"));
    assert!(!saved_delivered(dir.path(), "a"));
    assert!(held(&app).is_empty());
    // The dead port was dropped; a live one takes the retry.
    assert!(session(&mut app).harness.is_none());
    let mut port = attach_chat(&mut app);
    tick(&mut app);
    assert_eq!(submitted(&mut port).len(), 1);
    assert!(delivered(&app, "a"));

    let dir = tempfile::tempdir().unwrap();
    let mut app = delivery_app(dir.path(), &["b"]);
    let mut port = attach_chat(&mut app);
    let path = dir.path().join("tasks.json");
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    tick(&mut app);
    assert_eq!(submitted(&mut port).len(), 1);
    // Accepted: the receipt is kept in memory although the save failed.
    assert!(delivered(&app, "b"));
    turn_ends(&mut app);
    tick(&mut app);
    assert_eq!(submitted(&mut port), Vec::<String>::new());
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, bytes).unwrap();
    assert!(!saved_delivered(dir.path(), "b"));
    tick(&mut app);
    assert!(saved_delivered(dir.path(), "b"));
    assert_eq!(submitted(&mut port), Vec::<String>::new());
}

/// A held result whose chat changed identity before its turn ended is not
/// submitted to the tab that now stands there; it waits for its own chat
/// and goes there once it is back.
#[test]
fn delivery_for_a_changed_chat_is_dropped_and_waits_for_its_chat() {
    if isolated(
        "delivery_for_a_changed_chat_is_dropped_and_waits_for_its_chat",
        "delivery",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = delivery_app(dir.path(), &["a"]);
    let mut port = attach_chat(&mut app);
    busy(&mut app);
    tick(&mut app);
    assert_eq!(held(&app), ["a"]);
    let uid = app.workspaces[0].tabs[0].session_uid.clone();
    app.workspaces[0].tabs[0].session_uid = "another chat".into();
    turn_ends(&mut app);
    tick(&mut app);
    assert_eq!(submitted(&mut port), Vec::<String>::new());
    assert!(held(&app).is_empty());
    assert!(!delivered(&app, "a"));
    assert_eq!(app.delegation_waiting_counts.get(&uid), Some(&1));
    app.workspaces[0].tabs[0].session_uid = uid.clone();
    tick(&mut app);
    assert_eq!(submitted(&mut port).len(), 1);
    assert!(delivered(&app, "a"));
    tick(&mut app);
    assert_eq!(app.delegation_waiting_counts.get(&uid), None);
}

/// Results that finish while the chat is mid-turn are held (the card shows
/// it) and go together when the turn ends. A manual Send while busy joins
/// the hold; a Dismiss takes its result out of it.
#[test]
fn delivery_is_held_while_the_chat_is_busy() {
    if isolated("delivery_is_held_while_the_chat_is_busy", "delivery") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = delivery_app(dir.path(), &["a", "b", "c"]);
    app.delegations_config.auto_tell = false;
    let mut port = attach_chat(&mut app);
    busy(&mut app);
    tick(&mut app);
    assert!(held(&app).is_empty(), "automatic delivery is off");
    for id in ["a", "b", "a"] {
        let _ = app.update_event(Event::DelegationSendRequested(CHAT, id.into(), vec![]));
    }
    assert_eq!(held(&app), ["a", "b"]);
    tick(&mut app);
    assert_eq!(submitted(&mut port), Vec::<String>::new());
    let _ = app.update_event(Event::DelegationDismissRequested(CHAT, "b".into()));
    assert_eq!(held(&app), ["a"]);
    turn_ends(&mut app);
    let sent = submitted(&mut port);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("Result a") && !sent[0].contains("Result b"));
    assert!(delivered(&app, "a") && !delivered(&app, "b") && !delivered(&app, "c"));

    // Automatic delivery holds every undelivered finished result for the
    // turn end, as one prompt. Pinned as today: that includes the dismissed
    // one, since a dismiss only hides a finished card.
    app.delegations_config.auto_tell = true;
    tick(&mut app);
    assert_eq!(held(&app), ["b", "c"]);
    assert_eq!(submitted(&mut port), Vec::<String>::new());
    turn_ends(&mut app);
    let sent = submitted(&mut port);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(
        sent[0].contains("Result b") && sent[0].contains("Result c"),
        "{sent:?}"
    );
    assert!(delivered(&app, "b") && delivered(&app, "c"));
    assert!(held(&app).is_empty());
}
