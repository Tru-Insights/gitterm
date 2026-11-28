//! Workspace slide presentation and the debug Pi action. No alternate child launcher.
use super::*;
use gitterm_core::workspace::{self, slide::Slide};
use gpui::{RenderOnce, WeakEntity};

#[derive(Default)]
struct SlideViewport {
    slide: Slide,
    width: f32,
}

#[derive(gpui::IntoElement)]
pub(super) struct WorkspaceSlide {
    pub shell: WeakEntity<Shell>,
    pub panels: HashMap<WorkspaceId, gpui::Stateful<gpui::Div>>,
    pub background_work: Vec<(WorkspaceId, Entity<work_sidebar::WorkSidebar>)>,
}
impl RenderOnce for WorkspaceSlide {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let mut contents = self.panels;
        let viewport =
            window.use_keyed_state("workspace-slide-state", cx, |_, _| SlideViewport::default());
        let Some(shell) = self.shell.upgrade() else {
            return div();
        };
        let host = shell.read(cx);
        let input = host.bar_input(cx);
        let chips: Vec<_> = input
            .workspaces
            .iter()
            .map(|w| w.placement.clone())
            .collect();
        let active = input
            .workspaces
            .iter()
            .position(|w| Some(w.id) == input.active)
            .unwrap_or(0);
        let from = viewport
            .read(cx)
            .slide
            .pinned_from(input.active.map(|id| id.0).unwrap_or(0))
            .and_then(|id| input.workspaces.iter().position(|w| w.id.0 == id));
        let order = workspace::slide_order(&chips, active, from, input.repos_on_bar);
        let position = order.iter().position(|&i| i == active).unwrap_or(0);
        let selected = input.active.map(|id| id.0).unwrap_or(0);
        let width = viewport.read(cx).width;
        let was_moving = viewport.read(cx).slide.moving();
        viewport.update(cx, |viewport, _| {
            viewport
                .slide
                .sync(selected, position, width, Instant::now());
            viewport.slide.tick(position, Instant::now());
        });
        #[cfg(test)]
        cx.set_global(SlideProbe(viewport.downgrade()));
        let offset = viewport.read(cx).slide.offset;
        let moving = viewport.read(cx).slide.moving();
        if moving || was_moving {
            let tick = viewport.clone();
            window.on_next_frame(move |_, cx| tick.update(cx, |_, cx| cx.notify()));
        }
        let mut panels = div().relative().size_full().overflow_hidden();
        if moving {
            for (pos, &index) in order.iter().enumerate() {
                let id = input.workspaces[index].id;
                let content = if pos.abs_diff(position) <= 1 {
                    contents.remove(&id)
                } else {
                    None
                };
                panels = panels.child(
                    div()
                        .id(("workspace-slide-panel", id.0))
                        .debug_selector(move || format!("workspace-slide-panel-{}", id.0))
                        .bg(rgb(Palette::current(cx).base))
                        .absolute()
                        .left(px(pos as f32 * width - offset))
                        .top_0()
                        .w(px(width))
                        .h_full()
                        .overflow_hidden()
                        .children(content),
                );
            }
        } else if let Some(id) = input.active {
            panels = panels.child(
                div()
                    .id(("workspace-slide-panel", id.0))
                    .debug_selector(move || format!("workspace-slide-panel-{}", id.0))
                    .size_full()
                    .overflow_hidden()
                    .children(contents.remove(&id)),
            );
        }
        // A neighbour's Work sidebar is now mounted in the strip; do not mount
        // the same retained entity a second time for background fact loading.
        let background_work = self
            .background_work
            .into_iter()
            .filter_map(|(id, sidebar)| {
                let state = shell
                    .read(cx)
                    .workspaces
                    .iter()
                    .find(|w| w.workspace.id == id)?;
                let visible = !contents.contains_key(&id)
                    && !state.store.regions.sidebar_collapsed
                    && state.store.regions.sidebar_mode == SidebarMode::Work;
                (!visible).then_some(sidebar)
            })
            .collect::<Vec<_>>();
        panels = panels.child(
            div()
                .absolute()
                .size(px(0.))
                .invisible()
                .overflow_hidden()
                .children(background_work),
        );
        let weak = self.shell;
        panels.child(
            gpui::canvas(
                move |bounds, window, _| window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal),
                move |bounds, hitbox, window, cx| {
                    let measured = f32::from(bounds.size.width);
                    if viewport.read(cx).width != measured {
                        viewport.update(cx, |state, cx| {
                            state.width = measured;
                            cx.notify();
                        });
                    }
                    let ids: Vec<_> = order.iter().map(|&i| input.workspaces[i].id).collect();
                    let count = ids.len();
                    let peek_state = viewport.clone();
                    window.on_mouse_event(
                        move |event: &gpui::MouseMoveEvent, phase, window, cx| {
                            if phase == gpui::DispatchPhase::Capture {
                                let peek = if bounds.contains(&event.position) {
                                    workspace::slide::edge_peek(
                                        f32::from(event.position.x - bounds.origin.x),
                                        measured,
                                        position,
                                        count,
                                    )
                                } else {
                                    (false, false)
                                };
                                if peek_state.read(cx).slide.peek != peek {
                                    peek_state.update(cx, |state, cx| {
                                        state.slide.peek = peek;
                                        cx.notify();
                                    });
                                }
                            }
                            let _ = window;
                        },
                    );
                    // Leave vertical scrolling with the terminal/chat. The strip
                    // only consumes the horizontal component of this gesture.
                    window.on_mouse_event(move |event: &gpui::ScrollEvent, phase, window, cx| {
                        if !hitbox.is_hovered(window) || !bounds.contains(&event.position) {
                            return;
                        }
                        let delta = match event.delta {
                            gpui::ScrollDelta::Pixels(delta)
                                if delta.x != px(0.) && phase == gpui::DispatchPhase::Capture =>
                            {
                                -f32::from(delta.x)
                            }
                            gpui::ScrollDelta::Lines(delta)
                                if delta.x != 0. && phase == gpui::DispatchPhase::Capture =>
                            {
                                -delta.x * 60.
                            }
                            _ => return,
                        };
                        let target = viewport.update(cx, |state, cx| {
                            let target = state.slide.scroll(delta, ids.len(), Instant::now());
                            cx.notify();
                            target
                        });
                        if let Some(target) = target.and_then(|pos| ids.get(pos)).copied() {
                            let _ = weak.update(cx, |shell, cx| {
                                shell.select_workspace_id(target, window, cx)
                            });
                            cx.stop_propagation();
                        }
                    });
                },
            )
            .absolute()
            .inset_0(),
        )
    }
}

impl Shell {
    pub(super) fn slide_workspace_content(
        &self,
        state: &WorkspaceDock,
        active_workspace: bool,
        cx: &App,
    ) -> gpui::Stateful<gpui::Div> {
        if active_workspace
            && self.selected_task.is_some()
            && let Some(panel) = &self.task_panel
            && state
                .active_session
                .and_then(|id| self.session_task(id, cx))
                .is_none()
        {
            return div()
                .id("task-overview-host")
                .flex_1()
                .h_full()
                .child(task_panel::overview_view(panel, self.task_inputs(cx), cx));
        }
        #[cfg(test)]
        if self.dock_fixture {
            return div()
                .id("dock-fixture")
                .flex_1()
                .min_w_0()
                .h_full()
                .child(state.dock.clone());
        }
        if state.active_session.is_none() {
            return div()
                .id("session-content")
                .debug_selector(|| "session-content".into())
                .flex_1()
                .min_w_0()
                .h_full();
        }
        let selected = self
            .content
            .read(cx)
            .selected
            .as_ref()
            .and_then(|id| state.panes.get(id));
        let bottom = bottom_pane_ids(state, cx);
        let active = state.active_session.map(session_pane_id);
        let pane = selected
            .or_else(|| {
                active
                    .as_ref()
                    .filter(|id| pane_id_visible(state, id, cx) && !bottom.contains(id))
                    .and_then(|id| state.panes.get(id))
            })
            .or_else(|| {
                state
                    .panes
                    .iter()
                    .find(|(id, _)| {
                        !id.ends_with("-git")
                            && !id.ends_with("-files")
                            && pane_id_visible(state, id, cx)
                            && !bottom.contains(id)
                    })
                    .map(|(_, pane)| pane)
            });
        div()
            .id("session-content")
            .debug_selector(|| "session-content".into())
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .when(
                pane.is_some_and(|pane| {
                    matches!(
                        pane.read(cx).content,
                        pane::PaneContent::Chat(_) | pane::PaneContent::PreviewChat(_)
                    )
                }),
                |pane| pane.font_family(".SystemUIFont"),
            )
            .when(
                pane.is_some_and(|p| matches!(p.read(cx).content, pane::PaneContent::Terminal(_))),
                |host| {
                    host.capture_any_mouse_down(|event, _, cx| {
                        if event.button == gpui::MouseButton::Right {
                            cx.stop_propagation();
                        }
                    })
                    .capture_any_mouse_up(|event, _, cx| {
                        if event.button == gpui::MouseButton::Right {
                            cx.stop_propagation();
                        }
                    })
                },
            )
            .children(pane.cloned())
    }
}

use gitterm_core::chat::{
    state::RuntimeStamp,
    state::{AgentEvent, Effect as PiEffect, Intent as PiIntent, State},
    types::{AgentBackend, AgentBackendConfig, UserPrompt},
};
use gitterm_core::harness::pi::runtime as pi;
use gitterm_gpui_chat::{
    ChatView,
    adapter::{Presentation, Receipt, ReceiptState, Update},
};
use std::path::PathBuf;

type PiEvents = tokio::sync::mpsc::UnboundedReceiver<gitterm_core::chat::state::AgentEvent>;

struct DebugPi {
    config: AgentBackendConfig,
    state: State,
    source: RuntimeStamp,
    target: gitterm_core::session::Target,
    runtime: Option<pi::AgentTaskHandle>,
    cwd: PathBuf,
    root: PathBuf,
    workspace: Option<gitterm_core::workspace_config::WorkspaceConfig>,
    program: PathBuf,
    pending: Vec<Receipt>,
}
impl DebugPi {
    fn record(&mut self, event: AgentEvent) -> Update {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let PiEffect::Recorded { mut before, after } = self.state.reduce(PiIntent::Record {
            event: event.clone(),
            now_ms: Some(now),
        }) else {
            unreachable!()
        };
        before.push(event);
        before.extend(after);
        Update {
            presentation: Some(Presentation::Append(before)),
            ..Default::default()
        }
    }
    fn receive(&mut self, event: AgentEvent) -> Update {
        let PiEffect::Event(effects) = self.state.reduce(PiIntent::Event(event.clone())) else {
            unreachable!()
        };
        let mut update = self.record(event);
        if effects.outcome.is_some() {
            update.receipts = std::mem::take(&mut self.pending)
                .into_iter()
                .map(|mut receipt| {
                    receipt.state = ReceiptState::Resolved;
                    receipt
                })
                .collect();
        }
        update.event = Some(effects);
        update
    }
    fn start(&mut self) -> Result<PiEvents, String> {
        use gitterm_core::{
            guidelines,
            turn_snapshots::{Recorder, SnapshotRoots},
        };
        let instructions = guidelines::for_launch(
            &self.root,
            &self.cwd,
            self.workspace.as_ref().is_some_and(|w| w.guidelines_off),
            guidelines::InstructionFile::AgentsMd,
        );
        let places = self
            .workspace
            .as_ref()
            .and_then(guidelines::config_document_places);
        if let Some(places) = &places
            && let Err(error) = std::fs::create_dir_all(&places.docs_dir)
        {
            eprintln!(
                "[guidelines] cannot create the work's docs folder {}: {error}",
                places.docs_dir.display()
            );
        }
        let instructions = guidelines::launch_instructions(
            instructions,
            places.as_ref(),
            false,
            &self.root.join("launch-instructions"),
        );
        let roots = SnapshotRoots::new(self.root.join("turn-snapshots"), std::env::temp_dir())?;
        let cwd = self.cwd.clone();
        let chat = self.target.session_uid.clone();
        let facts = gitterm_core::chat_facts::facts_dir(&self.root);
        let runtime = pi::spawn_agent_task(
            pi::Launch {
                config: self.config.clone(),
                cwd: self.cwd.clone(),
                guidelines_file: instructions.map(|g| g.path),
                program: self.program.clone(),
            },
            move |closed| {
                Recorder::new(roots, cwd, Some(chat), Some(facts), dirs::home_dir())
                    .with_close_token(closed)
            },
        );
        let events = runtime.take_event_receiver().expect("new Pi receiver");
        self.runtime = Some(runtime);
        self.state.begin_runtime();
        self.source = self.state.runtime_stamp(self.target.clone());
        Ok(events)
    }
    fn send(
        &mut self,
        intent: gitterm_gpui_chat::ChatIntent,
    ) -> (Receipt, Update, Option<PiEvents>) {
        let mut events = None;
        let mut update = Update::default();
        let result = match intent {
            gitterm_gpui_chat::ChatIntent::Send { text, images } => (|| {
                if text.trim().is_empty() && images.is_empty() {
                    return Err("The prompt is empty".into());
                }
                gitterm_core::chat::types::validate_images(&images)?;
                let PiEffect::Submit(submission) = self.state.reduce(PiIntent::Submit {
                    prompt: UserPrompt {
                        text,
                        images,
                        id: None,
                    },
                    backend: AgentBackend::Pi,
                    process_running: self.runtime.is_some(),
                    just_spawned: false,
                    message_id: String::new(),
                }) else {
                    unreachable!()
                };
                if self.runtime.is_none() {
                    events = Some(self.start()?);
                }
                let result = self
                    .runtime
                    .as_ref()
                    .expect("Pi manager started")
                    .submit_prompt(submission.prompt.text);
                if result.is_ok() {
                    self.state.reduce(PiIntent::SubmissionAccepted);
                } else {
                    self.runtime = None;
                }
                update = self.record(submission.echo);
                if submission.images_not_sent > 0
                    && let Some(Presentation::Append(events)) = &mut update.presentation
                {
                    // Iced's review note is transient; it does not fail the turn.
                    events.push(AgentEvent::Other(serde_json::json!({"type":"error", "message":format!("Images are only sent to Claude; this message went without its {} image(s).", submission.images_not_sent)})));
                }

                result
            })(),
            gitterm_gpui_chat::ChatIntent::Stop => {
                let effect = self.state.reduce(PiIntent::Stop {
                    runtime: self.source.clone(),
                    target: self.target.clone(),
                    authority: gitterm_core::session::State {
                        target: self.target.clone(),
                        control: gitterm_core::session::TurnControl::TaskHandle,
                        task_live: false,
                        last_tab: false,
                    },
                });
                if matches!(effect, PiEffect::CancelLegacyTurn)
                    && let Some(runtime) = &self.runtime
                {
                    runtime.request_stop();
                }
                Ok(())
            }
            _ => Err("Pi does not support this command".into()),
        };
        let receipt = match result {
            Ok(()) => Receipt {
                state: ReceiptState::ChannelAccepted,
                ..Receipt::rejected("")
            },
            Err(error) => Receipt::rejected(error),
        };
        if receipt.state == ReceiptState::ChannelAccepted {
            self.pending.push(receipt.clone());
        }
        (receipt, update, events)
    }
}

impl Shell {
    pub(super) fn debug_agent_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.live_chat_enabled(cx) {
            self.preview_unavailable("Debug agent tab", cx);
            return;
        }
        if self.owner.read(cx).workspace_save_pending {
            return;
        }
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        let saved = self.owner.read(cx).iced_workspace(workspace).cloned();
        let active_dir = self
            .active(cx)
            .active_session
            .and_then(|id| self.active(cx).panes.get(&session_pane_id(id)))
            .and_then(|pane| pane.read(cx).task_folder(cx));
        let workspace_dir = saved
            .as_ref()
            .map(|w| PathBuf::from(&w.dir))
            .or_else(|| self.data.terminal_cwd(workspace, cx));
        let Some(workspace_dir) = workspace_dir else {
            return;
        };
        let launch = pi::debug_launch(
            &workspace_dir,
            active_dir.as_deref(),
            std::env::var("PI_MODEL"),
            std::process::id(),
        );
        let cwd = launch.cwd;
        let Some(preview) = self.owner.read(cx).preview() else {
            return;
        };
        let chat::policy::ChatLaunchPolicy::Live { services } = chat::policy::current(preview, cx)
        else {
            return;
        };
        let config = launch.config;
        let target = sessions::fresh_target();
        let mut state = State::default();
        state.begin_runtime();
        let source = state.runtime_stamp(target.clone());
        let controller = Rc::new(RefCell::new(DebugPi {
            config: config.clone(),
            state,
            source,
            target: target.clone(),
            runtime: None,
            cwd: cwd.clone(),
            root: services.state_root().to_owned(),
            workspace: saved,
            program: launch.program,
            pending: vec![],
        }));
        #[cfg(test)]
        if cx.try_global::<DebugPiFixture>().is_some() {
            let fixture = cx.global_mut::<DebugPiFixture>();
            controller.borrow_mut().program = fixture.program.clone();
            fixture.controller = Some(Rc::downgrade(&controller));
        }
        let view = cx.new(|cx| {
            let mut view = ChatView::new(vec![], window, cx);
            view.set_backend(gitterm_core::chat::types::AgentBackend::Pi, cx);
            view.set_read_only(None, cx);
            view.set_appearance(theme::chat_appearance(cx), cx);
            view.set_width(theme::chat_width(cx), cx);
            view
        });
        let weak_view = view.downgrade();
        let handle = window.window_handle();
        let control = controller.clone();
        view.update(cx, |view, cx| {
            view.set_intent_callback(
                Rc::new(move |intent, cx| {
                    let (receipt, update, events) = control.borrow_mut().send(intent);
                    if let Some(mut events) = events {
                        let event_source = control.borrow().source.clone();
                        let events_control = Rc::downgrade(&control);
                        let events_view = weak_view.clone();
                        cx.defer(move |cx| {
                            let _ = cx.update_window(handle, |_, window, cx| {
                                let _ = events_view.update(cx, |_, cx| {
                                    cx.spawn_in(window, async move |view, cx| {
                                        while let Some(event) = events.recv().await {
                                            if view
                                                .update_in(cx, |view, window, cx| {
                                                    let Some(control) = events_control.upgrade()
                                                    else {
                                                        return;
                                                    };
                                                    let mut pi = control.borrow_mut();
                                                    if !pi
                                                        .state
                                                        .accepts_stream(&event_source, &pi.target)
                                                    {
                                                        return;
                                                    }
                                                    let update = pi.receive(event);
                                                    view.present(
                                                        &pi.source, &pi.state, update, window, cx,
                                                    );
                                                })
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                    })
                                    .detach();
                                });
                            });
                        });
                    }
                    let control = control.clone();
                    let view = weak_view.clone();
                    cx.defer(move |cx| {
                        let _ = cx.update_window(handle, |_, window, cx| {
                            let _ = view.update(cx, |view, cx| {
                                let pi = control.borrow();
                                view.present(&pi.source, &pi.state, update, window, cx);
                            });
                        });
                    });
                    receipt
                }),
                cx,
            );
        });
        self._subscriptions.push(view.update(cx, |_, cx| {
            cx.on_release(move |_, _| {
                if let Some(runtime) = &controller.borrow().runtime {
                    runtime.close_turn_snapshots();
                }
            })
        }));
        let mut id = layout::stable_path_hash(Path::new(&target.session_uid));
        while self
            .workspaces
            .iter()
            .any(|w| w.sessions.iter().any(|s| s.id.0 == id))
        {
            id = id.wrapping_add(1);
        }
        let id = SessionId(id);
        let pane_id = session_pane_id(id);
        let pane = cx.new(|cx| {
            let mut pane = Pane::new(
                pane_id.clone(),
                "Pi".into(),
                PaneKind::IcedSession(String::new()),
                workspace,
                self.data.clone(),
                window,
                cx,
            );
            pane.content = pane::PaneContent::Chat(view);
            pane.session = Some(target.clone());
            pane.terminal_cwd = Some(cwd.clone());
            pane.imported_tab = Some(gitterm_core::workspace_config::WorkspaceTabConfig {
                dir: cwd.to_string_lossy().into_owned(),
                repo_dir: self
                    .owner
                    .read(cx)
                    .iced_workspace(workspace)
                    .map(|w| w.dir.clone()),
                tab_kind: Some("agent".into()),
                agent_config: Some(config),
                session_uid: Some(target.session_uid),
                extra: Default::default(),
                startup_command: None,
                chat_session_id: None,
                task_id: None,
                task_session_id: None,
                worker_shutdown_refused: false,
            });
            pane
        });
        self._subscriptions
            .push(sessions::subscribe(&pane, window, cx));
        self.registry
            .borrow_mut()
            .insert(pane_id.clone(), pane.downgrade());
        let state = self
            .workspaces
            .iter_mut()
            .find(|w| w.workspace.id == workspace)
            .expect("active workspace");
        state.panes.insert(pane_id.clone(), pane);
        state.sessions.push(Session {
            id,
            title: "Pi".into(),
            kind: data::SessionKind::Chat,
        });
        state.active_session = Some(id);
        self.show_pane(&pane_id, window, cx);
    }
}

#[cfg(test)]
struct DebugPiFixture {
    program: PathBuf,
    controller: Option<std::rc::Weak<RefCell<DebugPi>>>,
}
#[cfg(test)]
impl gpui::Global for DebugPiFixture {}

/// Finish the real slide state before headless interaction tests target controls.
#[cfg(test)]
pub(super) fn settle_slide(cx: &mut gpui::VisualTestContext, position: usize) {
    cx.update(|_, cx| {
        let viewport = cx.global::<SlideProbe>().0.upgrade().unwrap();
        viewport.update(cx, |state, cx| {
            let now = Instant::now();
            state
                .slide
                .tick(position, now + std::time::Duration::from_secs(1));
            state
                .slide
                .tick(position, now + std::time::Duration::from_secs(2));
            cx.notify();
        });
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}

#[cfg(test)]
struct SlideProbe(WeakEntity<SlideViewport>);
#[cfg(test)]
impl gpui::Global for SlideProbe {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use std::{os::unix::fs::PermissionsExt, time::Duration};

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn workspace_swipe_gesture_sequence_changes_workspace_and_edge_peek(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (_root, shell, cx) = h6_tests::sample(cx);
        let bounds = cx.debug_bounds("workspace-slide-host").unwrap();
        cx.simulate_mouse_move(bounds.center(), None, gpui::Modifiers::none());
        draw(cx);
        cx.read(|cx| {
            assert_eq!(
                cx.global::<SlideProbe>()
                    .0
                    .upgrade()
                    .unwrap()
                    .read(cx)
                    .width,
                f32::from(bounds.size.width)
            )
        });
        let first = cx.read(|cx| shell.read(cx).active_workspace(cx));
        let scroll = |cx: &mut VisualTestContext, x: f32| {
            cx.simulate_event(gpui::ScrollEvent {
                position: bounds.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(x), px(0.))),
                touch_phase: gpui::TouchPhase::Moved,
                ..Default::default()
            });
            draw(cx);
        };
        scroll(cx, -f32::from(bounds.size.width) * 0.49);
        cx.read(|cx| assert_eq!(shell.read(cx).active_workspace(cx), first));
        scroll(cx, -f32::from(bounds.size.width) * 0.02);
        cx.read(|cx| {
            assert_ne!(shell.read(cx).active_workspace(cx), first);
            let viewport = cx.global::<SlideProbe>().0.upgrade().unwrap();
            assert_eq!(viewport.read(cx).slide.from, first.map(|id| id.0));
            assert!(viewport.read(cx).slide.moving());
        });
        cx.simulate_mouse_move(
            gpui::point(bounds.origin.x + px(29.), bounds.center().y),
            None,
            gpui::Modifiers::none(),
        );
        draw(cx);
        cx.read(|cx| {
            let viewport = cx.global::<SlideProbe>().0.upgrade().unwrap();
            assert_eq!(viewport.read(cx).slide.peek, (true, false));
        });
        cx.simulate_mouse_move(
            gpui::point(bounds.origin.x + px(30.), bounds.center().y),
            None,
            gpui::Modifiers::none(),
        );
        draw(cx);
        cx.read(|cx| {
            let viewport = cx.global::<SlideProbe>().0.upgrade().unwrap();
            assert_eq!(viewport.read(cx).slide.peek, (false, false));
        });
    }

    fn scroll_panel(cx: &mut VisualTestContext, fraction: f32) {
        let bounds = cx.debug_bounds("workspace-slide-host").unwrap();
        cx.simulate_mouse_move(bounds.center(), None, gpui::Modifiers::none());
        draw(cx);
        cx.simulate_event(gpui::ScrollEvent {
            position: bounds.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(-bounds.size.width * fraction, px(0.))),
            touch_phase: gpui::TouchPhase::Moved,
            ..Default::default()
        });
        draw(cx);
    }

    fn assert_x(cx: &mut VisualTestContext, selector: &'static str, expected: gpui::Pixels) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing {selector}"));
        assert!(
            (f32::from(bounds.origin.x - expected)).abs() < 1.,
            "{selector}: {bounds:?}, expected x {expected:?}"
        );
    }

    #[gpui::test]
    fn workspace_slide_open_sidebar_and_neighbour_are_inside_strip_mid_slide_and_settled(
        cx: &mut TestAppContext,
    ) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (_root, shell, cx) = h6_tests::sample(cx);
        cx.update(|_, cx| {
            shell.update(cx, |shell, cx| {
                for (index, state) in shell.workspaces.iter_mut().enumerate() {
                    state.store.regions.sidebar_collapsed = false;
                    state.store.regions.sidebar_mode = SidebarMode::Files;
                    state.store.regions.sidebar_width = 240. + index as f32 * 40.;
                }
                cx.notify();
            })
        });
        draw(cx);
        let host = cx.debug_bounds("workspace-slide-host").unwrap();
        let sidebar = cx.debug_bounds("sidebar").unwrap();
        let divider = cx.debug_bounds("sidebar-divider").unwrap();
        let main = cx.debug_bounds("workspace-main-1").unwrap();
        let fixed = [
            "pstrip-spine",
            "pstrip-session-strip",
            "pstrip-workspace-bar",
        ]
        .map(|selector| (selector, cx.debug_bounds(selector).unwrap()));
        assert_eq!(sidebar.origin.x, host.origin.x);
        assert_eq!(sidebar.size.width, px(240.));
        assert_eq!(main.origin.x, divider.right());
        assert!(cx.debug_bounds("workspace-sidebar-2").is_none());
        scroll_panel(cx, 0.25);
        let shift = host.size.width * 0.25;
        assert_x(cx, "sidebar", sidebar.origin.x - shift);
        assert_x(cx, "sidebar-divider", divider.origin.x - shift);
        assert_x(cx, "workspace-main-1", main.origin.x - shift);
        assert_x(
            cx,
            "workspace-sidebar-2",
            host.origin.x + host.size.width - shift,
        );
        assert_eq!(
            cx.debug_bounds("workspace-sidebar-2").unwrap().size.width,
            px(280.)
        );
        assert_x(
            cx,
            "workspace-main-2",
            host.origin.x + host.size.width - shift + px(284.),
        );
        assert!(
            cx.debug_bounds("workspace-sidebar-3").is_none(),
            "distant panels are placeholders"
        );
        for (selector, bounds) in fixed {
            assert_eq!(
                cx.debug_bounds(selector).unwrap(),
                bounds,
                "{selector} stays outside the strip"
            );
        }
        settle_slide(cx, 0);
        assert_x(cx, "sidebar", sidebar.origin.x);
        assert_x(cx, "sidebar-divider", divider.origin.x);
        assert_x(cx, "workspace-main-1", main.origin.x);
        assert!(cx.debug_bounds("workspace-sidebar-2").is_none());
    }

    #[gpui::test]
    fn workspace_slide_open_viewer_and_neighbour_are_inside_strip_mid_slide_and_settled(
        cx: &mut TestAppContext,
    ) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        cx.update(install_app_menus);
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let source = root.path().join("iced");
        let work = root.path().join("work");
        let neighbour = root.path().join("neighbour");
        for dir in [&source, &work, &neighbour] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(work.join("file.txt"), "placeholder viewer text").unwrap();
        std::fs::write(source.join("workspaces.json"), serde_json::json!({
            "active_workspace": 0,
            "workspaces": [
                {"name":"Work", "abbrev":"W", "color":"blue", "dir":work, "repos":[work], "tabs":[]},
                {"name":"Neighbour", "abbrev":"N", "color":"green", "dir":neighbour, "repos":[neighbour], "tabs":[]}
            ]
        }).to_string()).unwrap();
        let mut store = preview::tests::guarded_store(&root.path().join("preview"));
        let model = workspace_config::load_iced_workspaces_from(&mut store, &source);
        let layout = store.guard.as_ref().unwrap().preview().layout();
        let (shell, cx) = cx.add_window_view(|window, cx| {
            let owner = cx.new(|cx| WorkspaceOwner::new(model, Some(store), cx));
            Shell::with_owner(
                Rc::new(data::LocalData(owner.clone())),
                owner,
                &layout,
                true,
                window,
                cx,
            )
            .unwrap()
        });
        cx.simulate_resize(size(px(1400.), px(800.)));
        draw(cx);
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.open_file_viewer(
                    &FileOpen {
                        workspace: WorkspaceId(1),
                        path: root.path().join("work/file.txt"),
                        changed: None,
                        from_files: true,
                    },
                    window,
                    cx,
                );
                cx.notify();
            })
        });
        draw(cx);
        settle_slide(cx, 0);
        let host = cx.debug_bounds("workspace-slide-host").unwrap();
        let close = cx.debug_bounds("close-file-viewer").unwrap();
        let main = cx.debug_bounds("workspace-main-1").unwrap();
        scroll_panel(cx, 0.25);
        assert_x(
            cx,
            "close-file-viewer",
            close.origin.x - host.size.width * 0.25,
        );
        assert_x(
            cx,
            "workspace-main-1",
            main.origin.x - host.size.width * 0.25,
        );
        assert_x(
            cx,
            "workspace-sidebar-2",
            host.origin.x + host.size.width * 0.75,
        );
        settle_slide(cx, 0);
        assert_x(cx, "close-file-viewer", close.origin.x);
        assert_x(cx, "workspace-main-1", main.origin.x);
        assert!(cx.debug_bounds("workspace-sidebar-2").is_none());
        // The same retained viewer is rendered when it belongs to the outgoing
        // neighbour after crossing Iced's halfway selection threshold.
        scroll_panel(cx, 0.75);
        cx.read(|cx| assert_eq!(shell.read(cx).active_workspace(cx), Some(WorkspaceId(2))));
        assert_x(
            cx,
            "close-file-viewer",
            close.origin.x - host.size.width * 0.75,
        );
        settle_slide(cx, 1);
        assert!(cx.debug_bounds("close-file-viewer").is_none());
        assert_x(cx, "sidebar", host.origin.x);
    }

    #[gpui::test]
    fn debug_agent_tab_cmd_shift_g_preview_refuses_without_creating_a_tab(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (_root, shell, cx) = h6_tests::sample(cx);
        let before = cx.read(|cx| shell.read(cx).active(cx).sessions.len());
        cx.simulate_keystrokes("cmd-shift-g");
        draw(cx);
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert_eq!(shell.active(cx).sessions.len(), before);
            assert_eq!(
                shell.owner.read(cx).error.as_deref(),
                Some("Debug agent tab is not available in preview")
            );
        });
    }

    #[gpui::test]
    fn debug_agent_tab_cmd_shift_g_matches_iced_argv_env_cwd_with_fake_child(
        cx: &mut TestAppContext,
    ) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (root, shell, cx) = h5_tests::fixture(cx);
        let live = root.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        // The fixture has no repo instructions; disable global guidelines explicitly.
        std::fs::write(
            live.join(gitterm_core::guidelines::SETTINGS_FILE),
            r#"{"enabled":false}"#,
        )
        .unwrap();
        let capture = root.path().join("child.json");
        let program = root.path().join("fake-pi");
        std::fs::write(
            &program,
            format!(
                r##"#!/usr/bin/python3
import json, os, sys
capture = {capture}
with open(capture + '.new', 'w') as f:
    json.dump({{"argv": sys.argv[1:], "cwd": os.getcwd(), "path": os.environ.get('PATH')}}, f)
os.replace(capture + '.new', capture)
print('{{"type":"done"}}', flush=True)
"##,
                capture = serde_json::to_string(&capture).unwrap()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        cx.update(|_, cx| {
            chat::policy::install_live(
                gitterm_core::preview::service::ServiceContext::production(
                    &live,
                    "debug-pi-fixture",
                )
                .unwrap(),
                cx,
            )
            .unwrap();
            cx.set_global(DebugPiFixture {
                program,
                controller: None,
            });
        });
        cx.update(|window, cx| shell.read(cx).focus_handle(cx).focus(window, cx));
        let cwd = root.path().join("work/nested");
        std::fs::create_dir(&cwd).unwrap();
        cx.update(|_, cx| {
            shell.update(cx, |shell, cx| {
                let state = shell.active(cx);
                let pane = state.panes[&session_pane_id(state.active_session.unwrap())].clone();
                pane.update(cx, |pane, _| pane.terminal_cwd = Some(cwd.clone()));
            });
        });
        let before = cx.read(|cx| shell.read(cx).active(cx).sessions.len());
        cx.simulate_keystrokes("cmd-shift-g");
        draw(cx);
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert_eq!(shell.active(cx).sessions.len(), before + 1);
            let pane = shell.active(cx).panes
                [&session_pane_id(shell.active(cx).active_session.unwrap())]
                .read(cx);
            assert!(matches!(pane.content, pane::PaneContent::Chat(_)));
            assert!(matches!(
                pane.imported_tab.as_ref().unwrap().agent_config,
                Some(AgentBackendConfig::Pi {
                    thinking: Some(true),
                    ..
                })
            ));
        });
        assert!(!capture.exists(), "Iced also waits for the first prompt");
        let composer = cx
            .debug_bounds("composer-input")
            .expect("debug Pi composer");
        cx.simulate_click(composer.center(), gpui::Modifiers::none());
        draw(cx);
        cx.simulate_input("Synthetic prompt");
        cx.simulate_keystrokes("enter");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !capture.exists() {
            draw(cx);
            assert!(
                Instant::now() < deadline,
                "fake Pi did not receive the prompt"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let captured: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&capture).unwrap()).unwrap();
        let controller = cx.read(|cx| {
            cx.global::<DebugPiFixture>()
                .controller
                .as_ref()
                .unwrap()
                .upgrade()
                .unwrap()
        });
        while !controller.borrow().pending.is_empty() {
            draw(cx);
            assert!(
                Instant::now() < deadline,
                "Pi completion did not release the composer"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let config = pi::debug_config(std::env::var("PI_MODEL"), std::process::id());
        let expected = pi::build_command(&config, "Synthetic prompt", None).unwrap();
        assert_eq!(
            expected.as_std().get_envs().count(),
            0,
            "Iced inherits the environment without overlays"
        );
        assert_eq!(
            captured["argv"],
            serde_json::json!(
                expected
                    .as_std()
                    .get_args()
                    .map(|arg| arg.to_str().unwrap())
                    .collect::<Vec<_>>()
            )
        );
        assert_eq!(
            captured["cwd"],
            serde_json::json!(cwd.canonicalize().unwrap())
        );
        assert_eq!(
            captured["path"],
            serde_json::json!(std::env::var("PATH").ok())
        );
        // A completed Pi turn must release the native composer's delivery fence.
        cx.simulate_input("Second synthetic prompt");
        cx.simulate_keystrokes("enter");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            draw(cx);
            let latest: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&capture).unwrap()).unwrap();
            if latest["argv"].as_array().unwrap().last().unwrap() == "Second synthetic prompt"
                && controller.borrow().pending.is_empty()
            {
                break;
            }
            assert!(Instant::now() < deadline, "second Pi turn was blocked");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
