//! Live's adapter for Iced's needs-you policy. No persistence or transcript reads.
use crate::{
    Shell,
    owner::WorkspaceOwner,
    pane::{Pane, PaneContent},
};
use gitterm_core::{
    attention_notifications::{
        INPUT_REQUIRED as INPUT, NeedsYouNotifier, NeedsYouObservation, WORKER_BLOCKED as BLOCKED,
        needs_you_notification_script, strip_title_status_glyphs, terminal_title_shows_working,
    },
    chat::types::{HarnessEvent, TurnStatus},
    task_mcp::HandoffStatus,
    tasks::{TaskAttentionReason, TaskLifecycle},
};
use gpui::{App, Context, WeakEntity};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

/// Iced's native tab attention, reduced before the view renders. A worker's
/// report replaces the prior reason; visiting acknowledges delegation results.
#[derive(Default)]
pub(crate) struct ChatAttention {
    reason: Option<&'static str>,
    front: bool,
}
impl ChatAttention {
    pub(crate) fn event(&mut self, event: &HarnessEvent, pending: bool) {
        match event {
            HarnessEvent::RuntimeRequest { .. } => self.reason = Some(INPUT),
            HarnessEvent::RuntimeRequestResolved { .. } if !pending => self.working(),
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            } => {
                self.reason = (!self.front).then_some("Ready to review");
            }
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Failed(_),
                ..
            }
            | HarnessEvent::Error(_) => self.reason = Some("Agent failed"),
            _ => {}
        }
    }
    fn needs_you(&self) -> Option<&'static str> {
        self.reason
            .filter(|reason| matches!(*reason, INPUT | BLOCKED))
    }
    pub(crate) fn working(&mut self) {
        if self.reason == Some(INPUT) {
            self.reason = None;
        }
    }
    pub(crate) fn visit(&mut self) {
        if matches!(
            self.reason,
            Some(BLOCKED | "Ready to review" | "Worker done")
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
}
pub(crate) struct Notifications {
    notifier: NeedsYouNotifier<Target>,
    pub(crate) enabled: bool,
    panes: Vec<PaneSnapshot>,
}
impl Notifications {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            notifier: NeedsYouNotifier::new(now),
            enabled: true,
            panes: vec![],
        }
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
        let panes = self
            .workspaces
            .iter()
            .flat_map(|state| {
                state.sessions.iter().filter_map(|session| {
                    let pane = state.panes.get(&crate::session_pane_id(session.id))?;
                    Some(PaneSnapshot {
                        id: session.id.0,
                        pane: pane.downgrade(),
                        repo_title: self.data.status(state.workspace.id, cx).repository,
                        front: active == Some(state.workspace.id)
                            && state.active_session == Some(session.id),
                    })
                })
            })
            .collect();
        let panes: Vec<PaneSnapshot> = panes;
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
        self.owner.update(cx, |owner, _| {
            owner.notifications.as_mut().unwrap().panes = panes;
        });
    }
}

impl WorkspaceOwner {
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
                chat.notification_attention.reason = Some(if status == HandoffStatus::Blocked {
                    BLOCKED
                } else {
                    "Worker done"
                });
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitterm_core::chat::{runtime::RuntimeEvent, types::RuntimeRequestKind};
    use gpui::{AppContext, Entity, TestAppContext};
    use std::rc::Rc;

    fn request(id: &str, question: bool) -> HarnessEvent {
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
        assert_eq!(attention.reason, Some("Ready to review"));
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
        assert_eq!(attention.reason, Some(INPUT));
        attention.event(
            &HarnessEvent::RuntimeRequestResolved {
                request_id: "question".into(),
            },
            false,
        );
        assert_eq!(attention.reason, None);
        attention.reason = Some(BLOCKED);
        attention.working();
        assert_eq!(attention.reason, Some(BLOCKED)); // Iced clears only HumanInputRequired
        attention.visit();
        assert_eq!(attention.reason, None);
    }
}
