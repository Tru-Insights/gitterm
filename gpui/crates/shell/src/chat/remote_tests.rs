//! Synthetic agent streams exercise Live's actual controller and core supervisor.
use super::*;
use gitterm_core::{
    chat::{
        state::AgentEvent,
        types::{HarnessCommand, RuntimeRequestKind},
    },
    harness::{
        claude::ClaudeSessionConfig,
        remote_chat::{self, RemoteChatTransport},
    },
    remote::{client::*, config::RemoteAgentConfig},
    workspace_config::WorkspaceConfig,
};
use gitterm_gpui_chat::{PermissionDecision, adapter::ReceiptState};
use std::sync::{Mutex, mpsc};
use tokio::sync::mpsc as channel;
use tokio_stream::wrappers::UnboundedReceiverStream;

struct Call {
    open: ClaudeChatOpen,
    commands: channel::UnboundedReceiver<String>,
    frames: channel::UnboundedSender<Result<ClaudeChatFrame, RemoteAgentClientError>>,
    legacy: Option<channel::UnboundedSender<Result<String, RemoteAgentClientError>>>,
}
struct Fake {
    config: RemoteAgentClientConfig,
    persistent: bool,
    calls: mpsc::Sender<Call>,
    history: Mutex<Vec<AgentEvent>>,
    running: Mutex<Option<String>>,
}
struct Transport(Arc<Fake>);
impl std::ops::Deref for Transport {
    type Target = Fake;
    fn deref(&self) -> &Fake {
        &self.0
    }
}
impl RemoteChatTransport for Transport {
    fn config(&self) -> &RemoteAgentClientConfig {
        &self.config
    }
    async fn handshake(&self) -> std::result::Result<RemoteAgentHandshake, RemoteAgentClientError> {
        Ok(RemoteAgentHandshake {
            remote_id: self.config.remote_id.clone(),
            endpoint: self.config.endpoint.clone(),
            agent_name: "Placeholder".into(),
            agent_version: "fixture".into(),
            protocol_version: 1,
            capabilities: vec![
                if self.persistent {
                    "claude_chat_persistent"
                } else {
                    "claude_chat"
                }
                .into(),
            ],
            home_dir: "/remote/home".into(),
        })
    }
    async fn claude_chat_attach(
        &self,
        open: ClaudeChatOpen,
        commands: channel::UnboundedReceiver<String>,
    ) -> std::result::Result<ClaudeChatFrameStream, RemoteAgentClientError> {
        let (frames, rx) = channel::unbounded_channel();
        self.calls
            .send(Call {
                open,
                commands,
                frames,
                legacy: None,
            })
            .unwrap();
        Ok(Box::pin(UnboundedReceiverStream::new(rx)))
    }
    async fn claude_chat(
        &self,
        start: String,
        commands: channel::UnboundedReceiver<String>,
    ) -> std::result::Result<ClaudeChatEventStream, RemoteAgentClientError> {
        let (events, rx) = channel::unbounded_channel();
        let (frames, _) = channel::unbounded_channel();
        self.calls
            .send(Call {
                open: ClaudeChatOpen::Start { start_json: start },
                commands,
                frames,
                legacy: Some(events),
            })
            .unwrap();
        Ok(Box::pin(UnboundedReceiverStream::new(rx)))
    }
}
struct Client(Arc<Fake>);
impl crate::remote_client::RemoteClient for Client {
    fn chat_runtime(
        &self,
        _: &RemoteAgentConfig,
        config: ClaudeSessionConfig,
        attach: Option<String>,
    ) -> std::result::Result<(Session, channel::UnboundedReceiver<HarnessEvent>), String> {
        assert!(config.mcp_servers.is_empty());
        assert!(config.snapshot_facts_dir.is_none());
        Ok(match attach {
            Some(id) => remote_chat::attach_remote(Transport(self.0.clone()), config, id),
            None => remote_chat::spawn_remote(Transport(self.0.clone()), config),
        })
    }
    fn chat_history(
        &self,
        _: &RemoteAgentConfig,
        _: &str,
    ) -> std::result::Result<Vec<AgentEvent>, String> {
        Ok(self.0.history.lock().unwrap().clone())
    }
    fn running_chat(
        &self,
        _: &RemoteAgentConfig,
        _: &str,
    ) -> std::result::Result<Option<String>, String> {
        Ok(self.0.running.lock().unwrap().clone())
    }
    fn handshake(
        &self,
        _: &RemoteAgentConfig,
    ) -> std::result::Result<RemoteAgentHandshake, String> {
        Ok(RemoteAgentHandshake {
            remote_id: self.0.config.remote_id.clone(),
            endpoint: self.0.config.endpoint.clone(),
            agent_name: "Placeholder".into(),
            agent_version: "fixture".into(),
            protocol_version: 1,
            capabilities: vec!["claude_chat".into(), "claude_chat_persistent".into()],
            home_dir: "/remote/home".into(),
        })
    }
    fn list_dir(
        &self,
        _: &RemoteAgentConfig,
        _: String,
    ) -> gitterm_core::remote::source::SourceDirListing {
        unreachable!("no filesystem lookup")
    }
    fn list_chats(
        &self,
        _: &RemoteAgentConfig,
    ) -> std::result::Result<Vec<gitterm_core::chats::ChatIndexEntry>, String> {
        Ok(vec![])
    }
    fn list_sessions(
        &self,
        _: &gitterm_core::remote::ssh_sessions::RemoteHostConfig,
    ) -> gitterm_core::remote::ssh_sessions::RemoteSessionsSnapshot {
        unreachable!()
    }
    fn list_phones(
        &self,
        _: &RemoteAgentConfig,
    ) -> std::result::Result<Vec<gitterm_core::remote::phone_panel::PairedPhoneInfo>, String> {
        unreachable!()
    }
}
struct Fixture {
    _root: tempfile::TempDir,
    preview: Arc<gitterm_core::preview::Preview>,
    workspace: WorkspaceConfig,
    agent: RemoteAgentConfig,
    fake: Arc<Fake>,
    calls: mpsc::Receiver<Call>,
    shutdown: Arc<RuntimeShutdown>,
    store: gitterm_core::desktop_store::DesktopStore,
}
impl Fixture {
    fn new(cx: &mut gpui::TestAppContext, persistent: bool) -> Self {
        cx.background_executor.allow_parking();
        let (root, preview) = tests::fixture();
        let live = root.path().join("live");
        std::fs::create_dir(&live).unwrap();
        let services =
            gitterm_core::preview::service::ServiceContext::production(&live, "synthetic").unwrap();
        let remote_id = format!(
            "placeholder-{}",
            root.path().file_name().unwrap().to_string_lossy()
        );
        let workspace: WorkspaceConfig = serde_json::from_value(serde_json::json!({"name":"Placeholder","abbrev":"P","color":"blue","dir":"/remote/project","guidelines_off":true,"location":{"kind":"remote_agent","remote_id":remote_id,"workspace_id":"fixture","root":"/remote/project"},"tabs":[]})).unwrap();
        std::fs::write(
            live.join("workspaces.json"),
            serde_json::to_vec(&serde_json::json!({"active_workspace":0,"workspaces":[workspace]}))
                .unwrap(),
        )
        .unwrap();
        let store = gitterm_core::desktop_store::DesktopStore::open(&live).unwrap();
        cx.update(|cx| {
            gpui_component::init(cx);
            policy::install_live(services, cx).unwrap();
            cx.set_global(crate::workspace_config::LiveWorkspaces(store.clone()));
        });
        let mut agent: RemoteAgentConfig = serde_json::from_value(serde_json::json!({"id":"placeholder","name":"Placeholder","endpoint":"http://placeholder.invalid:7777","auth":{"type":"token","token_ref":"env:UNUSED_PLACEHOLDER"}})).unwrap();
        agent.id = remote_id;
        std::fs::write(
            live.join("remote-agents.json"),
            serde_json::to_vec(&serde_json::json!({"remote_agents":[agent]})).unwrap(),
        )
        .unwrap();
        let (calls, rx) = mpsc::channel();
        Self {
            _root: root,
            preview,
            workspace,
            agent: agent.clone(),
            fake: Arc::new(Fake {
                config: agent.into(),
                persistent,
                calls,
                history: Mutex::new(vec![]),
                running: Mutex::new(None),
            }),
            calls: rx,
            shutdown: RuntimeShutdown::new(),
            store,
        }
    }
    fn pane(&self, id: Option<&str>, window: &mut Window, cx: &mut Context<ChatPane>) -> ChatPane {
        let tab = resume::tab(
            PathBuf::from(&self.workspace.dir).as_path(),
            "placeholder-caller".into(),
            id.map(str::to_owned),
        );
        self.saved_pane(tab, window, cx)
    }
    fn saved_pane(
        &self,
        tab: gitterm_core::workspace_config::WorkspaceTabConfig,
        window: &mut Window,
        cx: &mut Context<ChatPane>,
    ) -> ChatPane {
        ChatPane::resume(
            self.shutdown.clone(),
            self.preview.clone(),
            self.workspace.clone(),
            tab,
            Some(remote::Binding {
                agent: self.agent.clone(),
                client: Arc::new(Client(self.fake.clone())),
            }),
            window,
            cx,
        )
        .unwrap()
    }
}
fn wait(
    cx: &mut gpui::VisualTestContext,
    mut predicate: impl FnMut(&mut gpui::VisualTestContext) -> bool,
) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(6);
    loop {
        cx.run_until_parked();
        if predicate(cx) {
            return;
        }
        assert!(
            std::time::Instant::now() < end,
            "synthetic remote stream stalled"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
fn call(f: &Fixture, cx: &mut gpui::VisualTestContext) -> Call {
    let mut value = None;
    wait(cx, |_| {
        value = f.calls.try_recv().ok();
        value.is_some()
    });
    value.unwrap()
}
fn opened(call: &Call, head: u64, gap: bool) {
    call.frames
        .send(Ok(ClaudeChatFrame::Opened(ClaudeChatOpened {
            chat_id: "placeholder-chat".into(),
            attached: true,
            claude_session_id: "placeholder-session".into(),
            head_seq: head,
            gap,
        })))
        .unwrap();
}
fn event(call: &Call, event: HarnessEvent, seq: u64, redelivered: bool) {
    let json = serde_json::to_string(&event).unwrap();
    if let Some(events) = &call.legacy {
        events.send(Ok(json)).unwrap();
    } else {
        call.frames
            .send(Ok(ClaudeChatFrame::Event {
                json,
                seq,
                redelivered,
            }))
            .unwrap();
    }
}
fn send(pane: &Entity<ChatPane>, cx: &mut gpui::VisualTestContext) {
    pane.update(cx, |pane, cx| {
        assert_eq!(
            pane.submit_prompt("synthetic prompt".into(), cx).state,
            ReceiptState::ChannelAccepted
        );
    });
}
fn receive_command(call: &mut Call, cx: &mut gpui::VisualTestContext) -> serde_json::Value {
    let mut value = None;
    wait(cx, |_| {
        value = call.commands.try_recv().ok();
        value.is_some()
    });
    serde_json::from_str(&value.unwrap()).unwrap()
}
fn detach(pane: &Entity<ChatPane>, call: Call, f: &Fixture, cx: &mut gpui::VisualTestContext) {
    pane.update(cx, |pane, cx| pane.quit_runtime(cx));
    let mut commands = call.commands;
    wait(cx, |_| match commands.try_recv() {
        Err(channel::error::TryRecvError::Disconnected) => true,
        Err(_) => false,
        Ok(command) => panic!("quit sent {command}"),
    });
    drop(call.frames);
    drop(call.legacy);
    f.shutdown.drain().unwrap();
}

#[gpui::test]
fn golive20b_remote_start_permission_close_and_legacy(cx: &mut gpui::TestAppContext) {
    let f = Fixture::new(cx, false);
    let (pane, cx) = cx.add_window_view(|window, cx| f.pane(None, window, cx));
    send(&pane, cx);
    let mut stream = call(&f, cx);
    let ClaudeChatOpen::Start { start_json } = &stream.open else {
        panic!("start")
    };
    let config: serde_json::Value = serde_json::from_str(start_json).unwrap();
    assert_eq!(config["cwd"], "/remote/project");
    assert!(config["resume"].is_null());
    let prompt = receive_command(&mut stream, cx);
    assert_eq!(
        prompt,
        serde_json::to_value(HarnessCommand::SendUserMessage(
            gitterm_core::chat::types::UserPrompt {
                text: "synthetic prompt".into(),
                images: vec![],
                id: Some("placeholder-caller-1".into())
            }
        ))
        .unwrap()
    );
    event(
        &stream,
        HarnessEvent::RuntimeRequest {
            request_id: "permission".into(),
            tool_use_id: "tool".into(),
            kind: RuntimeRequestKind::Permission {
                tool_name: "Write".into(),
                input: serde_json::json!({"file_path":"/remote/project/file"}),
                suggestions: vec![],
                decision_reason: None,
            },
        },
        1,
        false,
    );
    wait(cx, |cx| {
        pane.read_with(cx, |pane, _| {
            pane.controller.adapter.state().has_pending_requests()
        })
    });
    pane.update(cx, |pane, cx| {
        let identity = pane
            .controller
            .adapter
            .state()
            .request_identity(&pane.controller.source, &[], "permission")
            .unwrap();
        assert_eq!(
            pane.dispatch(
                ChatIntent::Answer {
                    identity,
                    answer: Box::new(ChatIntent::Permission {
                        request_id: "permission".into(),
                        decision: PermissionDecision::Deny {
                            reason: "synthetic denial".into()
                        }
                    })
                },
                cx
            )
            .state,
            ReceiptState::ChannelAccepted
        );
    });
    let answer = receive_command(&mut stream, cx);
    assert_eq!(answer["data"]["request_id"], "permission");
    assert!(answer.to_string().contains("synthetic denial"));
    let suggestion = serde_json::json!({"type":"addRules","destination":"localSettings","rules":[{"toolName":"Write","ruleContent":"/remote/project/**"}],"behavior":"allow"});
    event(
        &stream,
        HarnessEvent::RuntimeRequest {
            request_id: "remember".into(),
            tool_use_id: "tool-remember".into(),
            kind: RuntimeRequestKind::Permission {
                tool_name: "Write".into(),
                input: serde_json::json!({}),
                suggestions: vec![suggestion.clone()],
                decision_reason: None,
            },
        },
        2,
        false,
    );
    wait(cx, |cx| {
        pane.read_with(cx, |pane, _| {
            pane.controller
                .adapter
                .state()
                .request_identity(&pane.controller.source, &[], "remember")
                .is_some()
        })
    });
    pane.update(cx, |pane, cx| {
        let identity = pane
            .controller
            .adapter
            .state()
            .request_identity(&pane.controller.source, &[], "remember")
            .unwrap();
        assert_eq!(
            pane.dispatch(
                ChatIntent::Answer {
                    identity,
                    answer: Box::new(ChatIntent::Permission {
                        request_id: "remember".into(),
                        decision: PermissionDecision::AllowAlways {
                            suggestion: suggestion.clone()
                        }
                    })
                },
                cx
            )
            .state,
            ReceiptState::ChannelAccepted
        );
    });
    let answer: HarnessCommand = serde_json::from_value(receive_command(&mut stream, cx)).unwrap();
    assert!(
        matches!(answer, HarnessCommand::Answer { decision: gitterm_core::chat::types::RuntimeDecision::Allow { remember: Some(ref value), .. }, .. } if value == &suggestion)
    );
    pane.update(cx, |pane, cx| pane.close(cx));
    assert_eq!(
        receive_command(&mut stream, cx),
        serde_json::to_value(HarnessCommand::Shutdown).unwrap()
    );
    drop(stream);
    f.shutdown.drain().unwrap();
}

#[gpui::test]
fn golive20b_remote_resume_and_quit_detaches(cx: &mut gpui::TestAppContext) {
    let f = Fixture::new(cx, true);
    *f.fake.history.lock().unwrap() = vec![AgentEvent::user_prompt("saved history")];
    let (pane, cx) =
        cx.add_window_view(|window, cx| f.pane(Some("placeholder-session"), window, cx));
    wait(cx, |cx| pane.read_with(cx, |pane, _| !pane.history_loading));
    assert!(f.calls.try_recv().is_err(), "resume history never launches");
    send(&pane, cx);
    let mut stream = call(&f, cx);
    let ClaudeChatOpen::Start { start_json } = &stream.open else {
        panic!("resume")
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(start_json).unwrap()["resume"],
        "placeholder-session"
    );
    opened(&stream, 0, false);
    receive_command(&mut stream, cx);
    detach(&pane, stream, &f, cx);
}

#[gpui::test]
fn golive20b_remote_restore_attaches_without_launch_and_repairs_gap(cx: &mut gpui::TestAppContext) {
    let f = Fixture::new(cx, true);
    *f.fake.running.lock().unwrap() = Some("placeholder-chat".into());
    *f.fake.history.lock().unwrap() = vec![AgentEvent::user_prompt("saved history")];
    let (pane, cx) =
        cx.add_window_view(|window, cx| f.pane(Some("placeholder-session"), window, cx));
    let stream = call(&f, cx);
    assert_eq!(
        stream.open,
        ClaudeChatOpen::Chat {
            chat_id: "placeholder-chat".into(),
            after_seq: None
        }
    );
    opened(&stream, 0, false);
    event(&stream, HarnessEvent::TextDelta("once".into()), 1, false);
    event(
        &stream,
        HarnessEvent::TextDelta("duplicate".into()),
        1,
        false,
    );
    wait(cx, |cx| {
        pane.read_with(cx, |pane, _| {
            pane.saved.records.iter().any(
                |r| matches!(&r.entry, Entry::Harness(HarnessEvent::TextDelta(s)) if s == "once"),
            )
        })
    });
    assert!(pane.read_with(cx, |pane, _| !pane.saved.records.iter().any(
        |r| matches!(&r.entry, Entry::Harness(HarnessEvent::TextDelta(s)) if s == "duplicate")
    )));
    drop(stream);
    let reconnect = call(&f, cx);
    assert_eq!(
        reconnect.open,
        ClaudeChatOpen::Chat {
            chat_id: "placeholder-chat".into(),
            after_seq: Some(1)
        }
    );
    opened(&reconnect, 1, false);
    event(
        &reconnect,
        HarnessEvent::TextDelta("reconnect duplicate".into()),
        1,
        false,
    );
    event(
        &reconnect,
        HarnessEvent::TextDelta("after reconnect".into()),
        2,
        false,
    );
    wait(cx, |cx| {
        pane.read_with(cx, |pane, _| pane.saved.records.iter().any(|r| matches!(&r.entry, Entry::Harness(HarnessEvent::TextDelta(s)) if s.contains("after reconnect"))))
    });
    assert!(pane.read_with(cx, |pane, _| !pane.saved.records.iter().any(|r| matches!(&r.entry, Entry::Harness(HarnessEvent::TextDelta(s)) if s.contains("reconnect duplicate")))));
    drop(reconnect);
    let reconnect = call(&f, cx);
    assert_eq!(
        reconnect.open,
        ClaudeChatOpen::Chat {
            chat_id: "placeholder-chat".into(),
            after_seq: Some(2)
        }
    );
    *f.fake.history.lock().unwrap() = vec![AgentEvent::user_prompt("repaired history")];
    opened(&reconnect, 20, true);
    event(
        &reconnect,
        HarnessEvent::TextDelta("too old".into()),
        10,
        false,
    );
    event(
        &reconnect,
        HarnessEvent::TextDelta("new event".into()),
        21,
        false,
    );
    wait(cx, |cx| {
        pane.read_with(cx, |pane, _| pane.user_prompts() == ["repaired history"] && pane.saved.records.iter().any(|r| matches!(&r.entry, Entry::Harness(HarnessEvent::TextDelta(s)) if s == "new event")))
    });
    assert!(pane.read_with(cx, |pane, _| !pane.saved.records.iter().any(|r| matches!(&r.entry, Entry::Harness(HarnessEvent::TextDelta(s)) if s == "too old" || s == "once"))));
    // Save actual confirmed Iced DTOs, detach, then reconstruct from disk.
    pane.update(cx, |pane, cx| {
        pane.persist_live_tab(cx).unwrap();
    });
    f.shutdown.drain().unwrap();
    let bytes = std::fs::read(f.store.root().join("workspaces.json")).unwrap();
    let saved: gitterm_core::workspace_config::WorkspacesFile =
        serde_json::from_slice(&bytes).unwrap();
    let tab = saved.workspaces[0].tabs[0].clone();
    detach(&pane, reconnect, &f, cx);
    pane.update(cx, |pane, _| pane.live_claim = None);
    pane.update_in(cx, |pane, window, cx| {
        *pane = f.saved_pane(tab, window, cx);
    });
    let stream = call(&f, cx);
    assert_eq!(
        stream.open,
        ClaudeChatOpen::Chat {
            chat_id: "placeholder-chat".into(),
            after_seq: None
        }
    );
    opened(&stream, 0, false);
    detach(&pane, stream, &f, cx);
}

#[gpui::test]
fn golive20b_shell_new_remote_chat_uses_remote_cwd_and_close_stops(cx: &mut gpui::TestAppContext) {
    let f = Fixture::new(cx, true);
    let mut store = crate::preview::tests::guarded_store(f.preview.root());
    let model = crate::workspace_config::load_iced_workspaces_from(&mut store, f.store.root());
    let layout = store.guard.as_ref().unwrap().preview().layout();
    let settings = crate::workspace_config::load_iced_settings_from(f.store.root());
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
        let mut shell = crate::Shell::with_owner_settings(
            Rc::new(crate::data::LocalData(owner.clone())),
            owner,
            &layout,
            true,
            &settings,
            window,
            cx,
        )
        .unwrap();
        shell.set_remote_client(Arc::new(Client(f.fake.clone())));
        shell
    });
    shell.update_in(cx, |shell, window, cx| {
        shell.new_claude_chat(None, window, cx);
        shell.new_claude_chat(None, window, cx);
    });
    let pane = shell.read_with(cx, |shell, cx| {
        assert!(
            shell.owner.read(cx).error.is_none(),
            "{:?}",
            shell.owner.read(cx).error
        );
        shell
            .owner
            .read(cx)
            .chats
            .values()
            .next()
            .expect("native chat mounted in remote workspace")
            .clone()
    });
    pane.read_with(cx, |pane, _| {
        assert!(pane.is_remote());
        assert_eq!(pane.cwd(), std::path::Path::new("/remote/project"));
    });
    send(&pane, cx);
    let mut stream = call(&f, cx);
    assert!(matches!(stream.open, ClaudeChatOpen::Start { .. }));
    opened(&stream, 0, false);
    receive_command(&mut stream, cx);
    let target = pane.read_with(cx, |pane, _| pane.target());
    shell.update_in(cx, |shell, window, cx| {
        shell.close_target(&target, window, cx)
    });
    assert_eq!(
        receive_command(&mut stream, cx),
        serde_json::to_value(HarnessCommand::Shutdown).unwrap()
    );
    drop(stream);
    shell.read_with(cx, |shell, cx| {
        shell.owner.read(cx).chat_shutdown.drain().unwrap()
    });
}
