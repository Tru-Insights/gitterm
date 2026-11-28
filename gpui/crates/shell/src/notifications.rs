//! Live's adapter for Iced's needs-you policy. No persistence or transcript reads.
use crate::{
    Shell, WorkspaceDock,
    data::SessionId,
    owner::WorkspaceOwner,
    pane::{Pane, PaneContent},
    workspace_bar::AttentionKind,
};
use gitterm_core::{
    attention::{
        AttentionReason, PULSE_INTERVAL, harness_event_attention_reason,
        terminal_title_attention_reason,
    },
    attention_notifications::{
        INPUT_REQUIRED as INPUT, NeedsYouNotifier, NeedsYouObservation, WORKER_BLOCKED as BLOCKED,
        needs_you_notification_script, strip_title_status_glyphs, terminal_title_shows_working,
    },
    chat::types::HarnessEvent,
    park::WakeReason,
    task_mcp::HandoffStatus,
    tasks::DelegationKind,
    tasks::{TaskAttentionReason, TaskLifecycle},
};
use gpui::{App, Context, Subscription, WeakEntity};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

/// Iced's native tab attention, reduced before the view renders. A worker's
/// report replaces the prior reason; visiting acknowledges delegation results.
#[derive(Default)]
pub(crate) struct ChatAttention {
    reason: Option<AttentionReason>,
    front: bool,
    pending: HashSet<String>,
    since: Option<Instant>,
    pub(crate) wake: Option<WakeReason>,
}
impl ChatAttention {
    pub(crate) fn event(&mut self, event: &HarnessEvent, pending: bool) {
        match event {
            HarnessEvent::RuntimeRequest { request_id, .. } => {
                self.pending.insert(request_id.clone());
            }
            HarnessEvent::RuntimeRequestResolved { request_id } => {
                self.pending.remove(request_id);
            }
            _ => {}
        }
        if !pending {
            self.pending.clear();
        }
        if let Some(reason) = harness_event_attention_reason(event) {
            if self.wake.is_none() {
                self.wake = WakeReason::for_attention(reason);
            }
            if reason == AttentionReason::CompletedUnread && self.front {
                self.reason = None;
                self.since = None;
            } else {
                self.set(reason);
            }
        } else if matches!(event, HarnessEvent::RuntimeRequestResolved { .. }) && !pending {
            self.working();
        }
    }
    fn set(&mut self, reason: AttentionReason) {
        if self.reason != Some(reason) {
            self.since = Some(Instant::now());
        }
        self.reason = Some(reason);
    }

    fn needs_you(&self) -> Option<&'static str> {
        self.reason
            .map(AttentionReason::label)
            .filter(|reason| matches!(*reason, INPUT | BLOCKED))
    }
    pub(crate) fn working(&mut self) {
        if self.reason == Some(AttentionReason::HumanInputRequired) {
            self.reason = None;
        }
    }
    pub(crate) fn visit(&mut self) {
        if matches!(
            self.reason,
            Some(
                AttentionReason::CompletedUnread
                    | AttentionReason::DelegationReady(_)
                    | AttentionReason::DelegationFailed(_)
                    | AttentionReason::DelegationBlocked(_)
            )
        ) {
            self.reason = None;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Target {
    Tab(u64),
    Task(String),
}
struct PaneSnapshot {
    id: u64,
    pane: WeakEntity<Pane>,
    repo_title: String,
    front: bool,
    _attention: Option<Subscription>,
}
pub(crate) struct Notifications {
    notifier: NeedsYouNotifier<Target>,
    pub(crate) enabled: bool,
    panes: Vec<PaneSnapshot>,
    pulse_bright: bool,
    pulse_task: Option<gpui::Task<()>>,
    pub(crate) woke: std::collections::HashMap<u64, WakeReason>,
}
impl Notifications {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            notifier: NeedsYouNotifier::new(now),
            enabled: true,
            panes: vec![],
            pulse_bright: false,
            pulse_task: None,
            woke: Default::default(),
        }
    }
}

/// One session projection feeds the strip, workspace chips, rail and keyboard jump.
#[derive(Default)]
pub(crate) struct SessionAttention {
    pub kind: Option<AttentionKind>,
    pub pending: usize,
    pub running: bool,
    pub reason: Option<AttentionReason>,
    pub waiting: Duration,
}
fn kind(reason: AttentionReason) -> AttentionKind {
    match reason.priority() {
        0 => AttentionKind::NeedsYou,
        1 => AttentionKind::Failed,
        _ => AttentionKind::Review,
    }
}
fn live_attention(pane: &Pane, cx: &App) -> SessionAttention {
    match &pane.content {
        PaneContent::PreviewChat(view) => {
            let chat = view.read(cx);
            let attention = &chat.notification_attention;
            SessionAttention {
                kind: attention.reason.map(kind),
                pending: attention.pending.len(),
                running: chat.is_working(),
                reason: attention.reason,
                waiting: attention
                    .since
                    .map(|since| since.elapsed())
                    .unwrap_or_default(),
            }
        }
        PaneContent::Terminal(_) => {
            let title = pane.notification_terminal_title();
            let reason = if pane.needs_input(cx) {
                Some(AttentionReason::HumanInputRequired)
            } else {
                title.and_then(terminal_title_attention_reason)
            };
            SessionAttention {
                kind: reason.map(kind),
                reason,
                running: title.is_some_and(terminal_title_shows_working),
                ..Default::default()
            }
        }
        _ => SessionAttention::default(),
    }
}
impl Shell {
    pub(crate) fn attention_pulse(&self, cx: &App) -> bool {
        self.owner
            .read(cx)
            .notifications
            .as_ref()
            .is_some_and(|s| s.pulse_bright)
    }
    pub(crate) fn session_attention(
        &self,
        state: &WorkspaceDock,
        id: SessionId,
        cx: &App,
    ) -> SessionAttention {
        let mut live = state
            .panes
            .get(&crate::session_pane_id(id))
            .map(|p| live_attention(p.read(cx), cx))
            .unwrap_or_default();
        if crate::session_needs_input(state, id, cx) {
            live.kind = Some(AttentionKind::NeedsYou);
        }
        let owner = self.owner.read(cx);
        let tab = owner.iced_workspace(state.workspace.id).and_then(|saved| {
            owner
                .iced_sessions(saved)
                .iter()
                .position(|session| session.id == id)
                .and_then(|i| saved.tabs.get(i))
        });
        let sidebar = self
            .work_sidebars
            .get(&state.workspace.id)
            .map(|s| s.read(cx));
        let tasks = sidebar
            .map(|s| s.tasks())
            .or_else(|| self.roster_facts.as_ref().map(|f| f.tasks.as_slice()))
            .unwrap_or_default();
        let workers = sidebar
            .map(|s| s.workers())
            .or_else(|| self.roster_facts.as_ref().map(|f| f.workers.as_slice()))
            .unwrap_or_default();
        let target = state
            .panes
            .get(&crate::session_pane_id(id))
            .and_then(|pane| {
                let pane = pane.read(cx);
                match &pane.content {
                    PaneContent::PreviewChat(view) => Some(view.read(cx).target()),
                    _ => pane.session.clone(),
                }
            });
        let task_id = tab
            .and_then(|t| t.task_id.as_deref())
            .or_else(|| target.as_ref().and_then(|t| t.task_id.as_deref()));
        let task = task_id.and_then(|id| tasks.iter().find(|t| t.task_id == id));
        let durable = crate::session_strip::attention(
            task.map(|t| &t.attention),
            workers,
            tab.and_then(|t| t.session_uid.as_deref())
                .or_else(|| target.as_ref().map(|t| t.session_uid.as_str())),
        );
        let live_kind = live.kind;
        live.kind = live
            .kind
            .into_iter()
            .chain(durable)
            .min_by_key(|kind| match kind {
                AttentionKind::NeedsYou => 0,
                AttentionKind::Failed => 1,
                AttentionKind::Review => 3,
            });
        if live.kind != live_kind {
            live.reason = None;
            live.waiting = Duration::ZERO;
        }
        live
    }
}

/// The same 200ms level observation as Iced's CheckMenu; never installed in
/// Preview. The owner retains this task across dock reconstruction.
pub(crate) fn start(cx: &mut Context<WorkspaceOwner>) -> gpui::Task<()> {
    cx.spawn(async move |owner, cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            if owner
                .update(cx, |owner, cx| {
                    for script in owner.notification_scripts(Instant::now(), cx) {
                        // Tests exercise the exact script effect without invoking osascript.
                        #[cfg(not(test))]
                        gitterm_core::attention_notifications::post_notification_script(script);
                        #[cfg(test)]
                        let _ = script;
                    }
                })
                .is_err()
            {
                break;
            }
        }
    })
}

impl Shell {
    /// Selection/catalog facts only; live attention is read on the owner's
    /// poll, including hidden panes whose render loop does not run.
    pub(crate) fn sync_notification_panes(&self, cx: &mut Context<Self>) {
        if self.owner.read(cx).notifications.is_none() {
            return;
        }
        let active = self.owner.read(cx).model.active();
        let mut panes = Vec::new();
        for state in &self.workspaces {
            for session in &state.sessions {
                let Some(pane) = state.panes.get(&crate::session_pane_id(session.id)) else {
                    continue;
                };
                let chat = match &pane.read(cx).content {
                    PaneContent::PreviewChat(chat) => Some(chat.clone()),
                    _ => None,
                };
                let weak_pane = pane.downgrade();
                let attention = chat.map(|chat| {
                    cx.observe(&chat, move |_, _, cx| {
                        // The dock's pane observer has the window needed to return
                        // a running park; delivery follows the chat's update.
                        if let Some(pane) = weak_pane.upgrade() {
                            pane.update(cx, |_, cx| cx.notify());
                        }
                        cx.notify();
                    })
                });
                panes.push(PaneSnapshot {
                    id: session.id.0,
                    _attention: attention,
                    pane: pane.downgrade(),
                    repo_title: self.data.status(state.workspace.id, cx).repository,
                    front: active == Some(state.workspace.id)
                        && state.active_session == Some(session.id),
                });
            }
        }
        for entry in &panes {
            let chat = entry
                .pane
                .upgrade()
                .and_then(|pane| match &pane.read(cx).content {
                    PaneContent::PreviewChat(chat) => Some(chat.clone()),
                    _ => None,
                });
            if let Some(chat) = chat {
                chat.update(cx, |chat, _| {
                    chat.notification_attention.front = entry.front;
                    if entry.front {
                        chat.notification_attention.visit();
                    }
                });
            }
        }
        self.owner.update(cx, |owner, cx| {
            let active_uid = self
                .workspaces
                .iter()
                .find(|w| Some(w.workspace.id) == active)
                .map(|w| w.runtime_uid);
            let state = owner.notifications.as_mut().unwrap();
            state.panes = panes;
            if let Some(uid) = active_uid {
                state.woke.remove(&uid);
            }
            owner.sync_attention_pulse(cx);
        });
    }
}

impl WorkspaceOwner {
    fn sync_attention_pulse(&mut self, cx: &mut Context<Self>) {
        let active = self.notifications.as_ref().is_some_and(|state| {
            state.panes.iter().any(|entry| {
                entry.pane.upgrade().is_some_and(|pane| {
                    live_attention(pane.read(cx), cx).reason
                        == Some(AttentionReason::HumanInputRequired)
                })
            })
        });
        let Some(state) = self.notifications.as_mut() else {
            return;
        };
        if !active {
            state.pulse_task = None;
        } else if state.pulse_task.is_none() {
            // Like Iced's conditional subscription, the first edge starts a
            // full 500ms interval, and resolving the last request cancels it.
            state.pulse_task = Some(cx.spawn(async move |owner, cx| {
                loop {
                    cx.background_executor().timer(PULSE_INTERVAL).await;
                    if owner
                        .update(cx, |owner, cx| {
                            if let Some(state) = &mut owner.notifications {
                                state.pulse_bright = !state.pulse_bright;
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }
    }

    fn notification_scripts(&mut self, now: Instant, cx: &mut Context<Self>) -> Vec<String> {
        let Some(state) = self.notifications.as_mut().filter(|s| s.enabled) else {
            return vec![];
        };
        let host = cx
            .try_global::<crate::task_host::GpuiTaskHost>()
            .map(|h| h.0.read(cx));
        let mut observations = Vec::new();
        let mut titles = std::collections::HashMap::new();
        let mut tabbed = HashSet::new();
        let mut front_task = None;
        for entry in &state.panes {
            let Some(pane) = entry.pane.upgrade() else {
                continue;
            };
            let pane = pane.read(cx);
            let target = Target::Tab(entry.id);
            let task = pane.task_session(cx);
            let terminal = pane.notification_terminal_title().or_else(|| {
                let session = task.as_ref()?.task_session_id.as_deref()?;
                host?.notification_terminal_title(session)
            });
            if entry.front {
                front_task = task.as_ref().and_then(|t| t.task_id.clone());
            }
            let (needs_you, running, requires_work, has_attention) = match &pane.content {
                PaneContent::PreviewChat(chat) => {
                    let chat = chat.read(cx);
                    (
                        chat.notification_attention.needs_you(),
                        chat.is_working(),
                        false,
                        chat.notification_attention.reason.is_some(),
                    )
                }
                PaneContent::Terminal(_) => {
                    let input =
                        pane.needs_input(cx) || terminal.is_some_and(|t| t.starts_with('✳'));
                    let working = terminal.is_some_and(terminal_title_shows_working)
                        || task
                            .as_ref()
                            .and_then(|t| t.task_session_id.as_deref())
                            .is_some_and(|id| {
                                host.is_some_and(|h| h.notification_session_working(id))
                            });
                    (input.then_some(INPUT), working, true, input)
                }
                _ => continue,
            };
            if has_attention && let Some(task) = task.and_then(|t| t.task_id) {
                tabbed.insert(task);
            }
            let topic = terminal
                .map(strip_title_status_glyphs)
                .filter(|s| !s.is_empty());
            let folder_title = pane.task_folder(cx).and_then(|folder| {
                host.and_then(|host| host.folder_labels.get(&folder))
                    .and_then(|label| label.clone())
            });
            let title = pane
                .notification_chat_title
                .clone()
                .or_else(|| topic.map(str::to_owned))
                .or(folder_title)
                .unwrap_or_else(|| entry.repo_title.clone());
            titles.insert(target.clone(), title);
            observations.push(NeedsYouObservation {
                target,
                needs_you,
                running,
                front: entry.front,
                requires_work,
            });
        }
        if let Some(store) = host.and_then(|h| h.host().store().as_ref()) {
            for task in store.tasks() {
                if task.lifecycle == TaskLifecycle::Archived
                    || tabbed.contains(&task.task_id)
                    || task.attention.reason != Some(TaskAttentionReason::RequiresInput)
                {
                    continue;
                }
                let target = Target::Task(task.task_id.clone());
                titles.insert(
                    target.clone(),
                    match &task.issue {
                        Some(issue) => format!("{} {}", issue.key, task.title),
                        None => task.title.clone(),
                    },
                );
                observations.push(NeedsYouObservation {
                    target,
                    needs_you: Some(INPUT),
                    running: false,
                    front: front_task.as_deref() == Some(&task.task_id),
                    requires_work: false,
                });
            }
        }
        state
            .notifier
            .observe(now, state.enabled, observations)
            .into_iter()
            .map(|nudge| needs_you_notification_script(&titles[&nudge.target], nudge.label))
            .collect()
    }
}

/// A report is an event, not a scan of durable blocked records: restored
/// results do not become new parent attention after the grace period.
pub(crate) fn worker_reported(parent: &str, status: HandoffStatus, cx: &mut App) {
    let Some(owner) = cx
        .try_global::<crate::work_sidebar::IcedWorkSource>()
        .and_then(|source| source.owner.upgrade())
    else {
        return;
    };
    owner.update(cx, |owner, cx| {
        let Some(state) = &owner.notifications else {
            return;
        };
        let Some(chat) = owner.chats.get(parent) else {
            return;
        };
        let front = state.panes.iter().any(|entry| {
            entry.front && entry.pane.upgrade().is_some_and(|p| {
            matches!(&p.read(cx).content, PaneContent::PreviewChat(pane) if pane == chat)
        })
        });
        if !front {
            chat.update(cx, |chat, _| {
                chat.notification_attention
                    .set(if status == HandoffStatus::Blocked {
                        AttentionReason::DelegationBlocked(DelegationKind::Implement)
                    } else {
                        AttentionReason::DelegationReady(DelegationKind::Implement)
                    });
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitterm_core::chat::{
        runtime::RuntimeEvent,
        types::{RuntimeRequestKind, TurnStatus},
    };
    use gpui::{AppContext, Entity, TestAppContext};
    use std::rc::Rc;

    pub(super) fn request(id: &str, question: bool) -> HarnessEvent {
        HarnessEvent::RuntimeRequest {
            request_id: id.into(),
            tool_use_id: id.into(),
            kind: if question {
                RuntimeRequestKind::Question {
                    questions: serde_json::json!([{"question":"Fixture?"}]),
                }
            } else {
                RuntimeRequestKind::Permission {
                    tool_name: "fixture-tool".into(),
                    input: serde_json::json!({}),
                    suggestions: vec![],
                    decision_reason: None,
                }
            },
        }
    }

    #[gpui::test]
    fn attention_notifications_live_owner_uses_hidden_chat_events_and_never_posts(
        cx: &mut TestAppContext,
    ) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        cx.update(gpui_component::init);
        let start = Instant::now();
        let at = |s| start + Duration::from_secs(s);
        let fixture = tempfile::tempdir_in("/private/tmp").unwrap();
        let owner = cx.new(|cx| WorkspaceOwner::new(Default::default(), None, cx));
        // Production's Preview path installs neither notifier nor polling task.
        assert!(owner.read_with(cx, |owner, _| owner.notifications.is_none()));
        let saved = crate::chat::SavedChat {
            uid: "fixture-parent".into(),
            cwd: fixture.path().into(),
            provider: fixture.path().join("provider"),
            session_id: Some("fixture-chat".into()),
            records: vec![],
        };
        let shutdown = owner.read_with(cx, |owner, _| owner.chat_shutdown.clone());
        let (chat, vcx) = cx.add_window_view(|window, cx| {
            crate::chat::ChatPane::history(shutdown, saved, window, cx)
        });
        let pane: Entity<Pane> = vcx.update(|window, cx| {
            cx.new(|cx| {
                Pane::new(
                    "fixture-pane".into(),
                    "Fixture \"topic\"".into(),
                    crate::pane::PaneKind::PreviewChat(chat.clone()),
                    crate::data::WorkspaceId(1),
                    Rc::new(crate::data::SampleData),
                    window,
                    cx,
                )
            })
        });
        pane.update(vcx, |pane, _| {
            pane.notification_chat_title = Some("Fixture \"topic\"".into())
        });
        let uid = chat.read_with(vcx, |chat, _| chat.target().session_uid);
        owner.update(vcx, |owner, _| {
            owner.chats.insert(uid.clone(), chat.clone());
            let mut state = Notifications::new(start);
            state.panes.push(PaneSnapshot {
                id: 1,
                _attention: None,
                pane: pane.downgrade(),
                repo_title: "fixture-repo".into(),
                front: false,
            });
            owner.notifications = Some(state);
        });
        vcx.update(|_, cx| {
            cx.set_global(crate::work_sidebar::IcedWorkSource {
                config: fixture.path().into(),
                workspaces: vec![],
                repos: vec![],
                owner: owner.downgrade(),
            })
        });
        // No window render or provider runtime is needed for either request kind.
        let event = |event, cx: &mut gpui::VisualTestContext| {
            chat.update(cx, |chat, cx| {
                let source = chat.runtime_source();
                chat.receive_runtime_event(&source, RuntimeEvent::Harness(event), cx);
            })
        };
        let poll = |seconds, cx: &mut gpui::VisualTestContext| {
            owner.update(cx, |owner, cx| owner.notification_scripts(at(seconds), cx))
        };
        event(request("permission", false), vcx);
        assert!(poll(4, vcx).is_empty());
        assert!(poll(5, vcx).is_empty()); // restored edge was spent during grace
        event(
            HarnessEvent::RuntimeRequestResolved {
                request_id: "permission".into(),
            },
            vcx,
        );
        poll(6, vcx);
        event(request("question", true), vcx);
        assert_eq!(
            poll(7, vcx),
            [r#"display notification "Input or approval needed" with title "Fixture \"topic\"""#]
        );
        assert!(poll(8, vcx).is_empty());
        // A worker report replaces the native reason and has its own repeat key.
        vcx.update(|_, cx| worker_reported(&uid, HandoffStatus::Blocked, cx));
        assert_eq!(
            poll(9, vcx),
            [r#"display notification "Worker blocked" with title "Fixture \"topic\"""#]
        );
        assert!(poll(100, vcx).is_empty());
        // Visiting acknowledges worker attention; a new report in front is quiet.
        owner.update(vcx, |owner, _| {
            owner.notifications.as_mut().unwrap().panes[0].front = true
        });
        chat.update(vcx, |chat, _| chat.notification_attention.visit());
        poll(101, vcx);
        vcx.update(|_, cx| worker_reported(&uid, HandoffStatus::Blocked, cx));
        assert!(poll(102, vcx).is_empty());
        // The setting disables policy observation, so enabling sees the new edge.
        owner.update(vcx, |owner, _| {
            let state = owner.notifications.as_mut().unwrap();
            state.panes[0].front = false;
            state.enabled = false;
        });
        vcx.update(|_, cx| worker_reported(&uid, HandoffStatus::Blocked, cx));
        assert!(poll(103, vcx).is_empty());
        owner.update(vcx, |owner, _| {
            owner.notifications.as_mut().unwrap().enabled = true
        });
        assert_eq!(poll(104, vcx).len(), 1);
    }

    #[test]
    fn attention_notifications_interrupted_turn_keeps_iced_attention_and_results_are_quiet() {
        let start = Instant::now();
        let mut attention = ChatAttention::default();
        let mut notifier = NeedsYouNotifier::new(start);
        attention.event(&request("question", true), true);
        let observe = |attention: &ChatAttention| NeedsYouObservation {
            target: 1u8,
            needs_you: attention.needs_you(),
            running: false,
            front: attention.front,
            requires_work: false,
        };
        assert_eq!(
            notifier
                .observe(start + Duration::from_secs(5), true, [observe(&attention)])
                .len(),
            1
        );
        let completed = |status| HarnessEvent::TurnCompleted {
            status,
            usage: serde_json::Value::Null,
            cost_usd: None,
            rate_limit: None,
        };
        attention.event(&completed(TurnStatus::Interrupted), false);
        assert_eq!(attention.needs_you(), Some(INPUT));
        assert!(
            notifier
                .observe(start + Duration::from_secs(70), true, [observe(&attention)])
                .is_empty()
        );
        attention.event(&completed(TurnStatus::Completed), false);
        assert_eq!(attention.reason, Some(AttentionReason::CompletedUnread));
        assert!(
            notifier
                .observe(start + Duration::from_secs(71), true, [observe(&attention)])
                .is_empty()
        );
        attention.visit();
        assert_eq!(attention.reason, None);
        attention.front = true;
        attention.event(&completed(TurnStatus::Completed), false);
        assert_eq!(attention.reason, None);
        attention.event(
            &completed(TurnStatus::Failed("fixture failure".into())),
            false,
        );
        assert!(attention.reason.is_some());
        attention.visit(); // Iced leaves failures raised on a visit.
        assert!(attention.reason.is_some());
        assert!(
            notifier
                .observe(start + Duration::from_secs(72), true, [observe(&attention)])
                .is_empty()
        );
    }

    #[test]
    fn attention_notifications_chat_keeps_input_until_all_requests_resolve() {
        let mut attention = ChatAttention::default();
        attention.event(&request("permission", false), true);
        attention.event(&request("question", true), true);
        attention.event(
            &HarnessEvent::RuntimeRequestResolved {
                request_id: "permission".into(),
            },
            true,
        );
        assert_eq!(attention.reason, Some(AttentionReason::HumanInputRequired));
        attention.event(
            &HarnessEvent::RuntimeRequestResolved {
                request_id: "question".into(),
            },
            false,
        );
        assert_eq!(attention.reason, None);
        attention.reason = Some(AttentionReason::DelegationBlocked(
            DelegationKind::Implement,
        ));
        attention.working();
        assert_eq!(
            attention.reason,
            Some(AttentionReason::DelegationBlocked(
                DelegationKind::Implement
            ))
        ); // Iced clears only HumanInputRequired
        attention.visit();
        assert_eq!(attention.reason, None);
    }
}

#[cfg(test)]
mod chrome_tests {
    use super::*;
    use crate::{
        chat::{ChatPane, SavedChat},
        data::SessionKind,
    };
    use gitterm_core::chat::{runtime::RuntimeEvent, types::TurnStatus};
    use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};

    fn fixture(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        Entity<Shell>,
        Entity<ChatPane>,
        &mut VisualTestContext,
    ) {
        cx.update(crate::install_app_menus);
        let (root, shell, cx) = crate::h2_tests::fixture_catalog(cx, false);
        let chat = cx.update(|window, cx| {
            let shutdown = shell.read(cx).owner.read(cx).chat_shutdown.clone();
            cx.new(|cx| {
                ChatPane::history(
                    shutdown,
                    SavedChat {
                        uid: "fixture-attention".into(),
                        cwd: root.path().into(),
                        provider: root.path().join("provider"),
                        session_id: Some("fixture-chat".into()),
                        records: vec![],
                    },
                    window,
                    cx,
                )
            })
        });
        shell.update(cx, |shell, cx| {
            let owner = shell.owner.clone();
            owner.update(cx, |owner, _| {
                owner.notifications = Some(Notifications::new(Instant::now()))
            });
            let state = &mut shell.workspaces[1];
            let session = &mut state.sessions[1];
            session.kind = SessionKind::Chat;
            state.panes[&crate::session_pane_id(session.id)].update(cx, |pane, _| {
                pane.content = PaneContent::PreviewChat(chat.clone());
            });
            shell.sync_notification_panes(cx);
        });
        (root, shell, chat, cx)
    }
    fn event(chat: &Entity<ChatPane>, event: HarnessEvent, cx: &mut VisualTestContext) {
        chat.update(cx, |chat, cx| {
            let source = chat.runtime_source();
            chat.receive_runtime_event(&source, RuntimeEvent::Harness(event), cx);
        });
    }
    fn completed(status: TurnStatus) -> HarnessEvent {
        HarnessEvent::TurnCompleted {
            status,
            usage: serde_json::Value::Null,
            cost_usd: None,
            rate_limit: None,
        }
    }

    #[gpui::test]
    fn tru274_pending_native_chat_lights_tab_chip_dot_and_ctrl_backtick(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (_root, shell, chat, cx) = fixture(cx);
        for question in [false, true] {
            event(&chat, super::tests::request("request", question), cx);
            cx.read(|cx| {
                let shell = shell.read(cx);
                let bar = shell.bar_input(cx);
                let chip = bar
                    .workspaces
                    .iter()
                    .find(|w| w.id == shell.workspaces[1].workspace.id)
                    .unwrap();
                assert_eq!(chip.attention.as_ref().unwrap().count, 1);
                assert_eq!(
                    chip.attention.as_ref().unwrap().kind,
                    AttentionKind::NeedsYou
                );
                let dot = shell
                    .spine_input(cx)
                    .dots(crate::workspace_bar::Colors::new(true))
                    .into_iter()
                    .find(|d| d.workspace == chip.id)
                    .unwrap();
                assert_eq!(dot.urgency, Some(crate::spine::Urgency::NeedsYou));
            });
            cx.update(|window, cx| {
                let focus = shell.read(cx).focus.clone();
                focus.focus(window, cx);
            });
            cx.simulate_keystrokes("ctrl-`");
            cx.run_until_parked();
            cx.read(|cx| {
                let shell = shell.read(cx);
                assert_eq!(
                    shell.active_workspace(cx),
                    Some(shell.workspaces[1].workspace.id)
                );
                let strip = shell.strip_input(cx);
                let tab = strip
                    .sessions
                    .iter()
                    .find(|s| s.id == strip.active.unwrap())
                    .unwrap();
                assert_eq!(tab.pending_requests, 1);
                assert_eq!(tab.attention, Some(AttentionKind::NeedsYou));
                assert_eq!(
                    tab.icon(true, true, crate::workspace_bar::Colors::new(true))
                        .0,
                    "● "
                );
            });
            event(
                &chat,
                HarnessEvent::RuntimeRequestResolved {
                    request_id: "request".into(),
                },
                cx,
            );
            shell.update_in(cx, |shell, window, cx| {
                shell.select_workspace_id(shell.workspaces[0].workspace.id, window, cx)
            });
        }
        for (status, expected) in [
            (TurnStatus::Completed, AttentionKind::Review),
            (TurnStatus::Failed("fixture".into()), AttentionKind::Failed),
        ] {
            event(&chat, completed(status), cx);
            cx.read(|cx| {
                assert_eq!(
                    shell.read(cx).chrome_workspaces(cx)[1]
                        .attention
                        .as_ref()
                        .unwrap()
                        .kind,
                    expected
                )
            });
            shell.update_in(cx, |s, window, cx| s.attention_jump(window, cx));
            cx.read(|cx| {
                assert_eq!(
                    shell.read(cx).strip_input(cx).active,
                    Some(shell.read(cx).workspaces[1].sessions[1].id)
                )
            });
            shell.update_in(cx, |s, window, cx| {
                s.select_workspace_id(s.workspaces[0].workspace.id, window, cx)
            });
        }
    }

    #[gpui::test]
    fn tru274_pulse_toggles_on_iced_500ms_interval(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (_root, shell, chat, cx) = fixture(cx);
        event(&chat, super::tests::request("request", false), cx);
        let owner = cx.read(|cx| shell.read(cx).owner.clone());
        let _task = owner.update(cx, |_, cx| start(cx));
        cx.run_until_parked();
        let bright = |cx: &mut VisualTestContext| {
            cx.read(|cx| {
                let s = shell.read(cx);
                let values = [
                    s.strip_input(cx).pulse_bright,
                    s.bar_input(cx).pulse_bright,
                    s.spine_input(cx).pulse_bright,
                ];
                assert!(values.iter().all(|v| *v == values[0]));
                values[0]
            })
        };
        assert!(!bright(cx));
        cx.executor().advance_clock(Duration::from_millis(499));
        cx.run_until_parked();
        assert!(!bright(cx));
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert!(bright(cx));
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        assert!(!bright(cx));
        event(
            &chat,
            HarnessEvent::RuntimeRequestResolved {
                request_id: "request".into(),
            },
            cx,
        );
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(250));
        cx.run_until_parked();
        event(&chat, super::tests::request("next", true), cx);
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(499));
        cx.run_until_parked();
        assert!(!bright(cx));
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert!(bright(cx), "new input restarts the full Iced interval");
        event(&chat, completed(TurnStatus::Failed("fixture".into())), cx);
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        assert!(
            bright(cx),
            "failed indicators leave the phase static like Iced"
        );
    }

    #[gpui::test]
    fn tru274_parked_chat_wakes_with_iced_reason_badge(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (_root, shell, chat, cx) = fixture(cx);
        for (event_value, expected) in [
            (
                super::tests::request("request", false),
                WakeReason::NeedsYou,
            ),
            (completed(TurnStatus::Completed), WakeReason::Finished),
            (
                completed(TurnStatus::Failed("fixture".into())),
                WakeReason::Failed,
            ),
            (HarnessEvent::Error("fixture".into()), WakeReason::Failed),
        ] {
            shell.update(cx, |shell, _| {
                shell.workspaces[1].parked = Some(crate::park::Parked {
                    info: gitterm_core::park::Info::new("fixture", "fixture".into()),
                    running: true,
                    done: false,
                });
            });
            event(&chat, event_value, cx);
            cx.run_until_parked();
            shell.update_in(cx, |shell, window, cx| {
                assert!(shell.workspaces[1].on_bar());
                assert_eq!(shell.chrome_workspaces(cx)[1].woke, Some(expected));
                assert_eq!(
                    shell.active_workspace(cx),
                    Some(shell.workspaces[0].workspace.id),
                    "wake never steals focus"
                );
                shell.select_workspace_id(shell.workspaces[1].workspace.id, window, cx);
                shell.sync_notification_panes(cx);
                assert_eq!(shell.chrome_workspaces(cx)[1].woke, None);
                shell.select_workspace_id(shell.workspaces[0].workspace.id, window, cx);
            });
        }
    }

    #[gpui::test]
    fn tru274_cli_title_attention_reaches_same_chrome_and_jump(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (_root, shell, _chat, cx) = fixture(cx);
        shell.update_in(cx, |shell, window, cx| {
            let state = &shell.workspaces[1];
            let id = state.sessions[0].id;
            let pane = state.panes[&crate::session_pane_id(id)].clone();
            // A deferred terminal has no PTY and no process.
            pane.update(cx, |pane, cx| {
                pane.content = PaneContent::Terminal(cx.new(|cx| {
                    gitterm_terminal::TerminalView::new_deferred(
                        Default::default(),
                        Default::default(),
                        cx,
                    )
                }));
                pane.terminal_title = Some("✳ Fixture".into());
            });
            assert_eq!(
                shell.session_attention(&shell.workspaces[1], id, cx).kind,
                Some(AttentionKind::NeedsYou)
            );
            assert_eq!(
                shell.chrome_workspaces(cx)[1]
                    .attention
                    .as_ref()
                    .unwrap()
                    .count,
                1
            );
            shell.attention_jump(window, cx);
            assert_eq!(shell.strip_input(cx).active, Some(id));
            pane.update(cx, |pane, _| pane.terminal_title = Some("◐ Fixture".into()));
            assert!(
                shell
                    .session_attention(&shell.workspaces[1], id, cx)
                    .kind
                    .is_none()
            );
        });
    }
}
