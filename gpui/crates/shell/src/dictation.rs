//! Composer ownership and GPUI scheduling only; capture and Whisper live in core.
use super::*;

#[cfg(feature = "stt")]
pub(crate) use enabled::Dictation;

#[cfg(feature = "stt")]
mod enabled {
    use super::*;
    use gitterm_core::dictation::{
        Recorder, Transcriber, recording_diagnostics, should_transcribe,
    };
    use gitterm_gpui_chat::ChatView;
    use std::sync::{Arc, Mutex};

    type Job = Box<dyn FnOnce() -> Result<String, Failure> + Send>;

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum Failure {
        Capture(String),
        Model(String),
        Transcription(String),
    }
    impl std::fmt::Display for Failure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Capture(error) => write!(f, "[STT] Failed to start recording: {error}"),
                Self::Model(error) => write!(f, "[STT] {error}"),
                Self::Transcription(error) => write!(f, "[STT] Error: {error}"),
            }
        }
    }

    /// The seam for synthetic capture/transcription in headless tests.
    trait Engine {
        fn start(&mut self) -> Result<(), String>;
        fn stop(&mut self) -> Option<Job>;
    }
    #[derive(Default)]
    struct CoreEngine {
        recorder: Option<Recorder>,
    }
    impl Engine for CoreEngine {
        fn start(&mut self) -> Result<(), String> {
            self.recorder = Some(Recorder::start(None, Arc::new(Mutex::new(Vec::new())))?);
            Ok(())
        }
        fn stop(&mut self) -> Option<Job> {
            // CPAL requires stopping/dropping the stream on its starting thread.
            let recorder = self.recorder.take()?;
            let rate = recorder.sample_rate();
            let samples = recorder.stop();
            eprintln!("{}", recording_diagnostics(&samples, rate));
            if !should_transcribe(&samples, rate) {
                return None;
            }
            Some(Box::new(move || {
                let root = gitterm_core::config_paths::desktop_config_dir()
                    .map_err(|error| Failure::Model(error.to_string()))?;
                let model = root.join("models").join("ggml-base.en.bin");
                // Loading and inference both stay off the UI thread.
                Transcriber::load(&model)
                    .map_err(Failure::Model)?
                    .transcribe(samples, rate)
                    .map_err(Failure::Transcription)
            }))
        }
    }

    #[derive(Clone)]
    struct Origin {
        pane: gpui::WeakEntity<Pane>,
        view: gpui::WeakEntity<ChatView>,
    }
    enum Phase {
        Idle,
        Recording(Origin),
        Transcribing(Origin),
    }
    pub(crate) struct Dictation {
        enabled: bool,
        phase: Phase,
        engine: Box<dyn Engine>,
        bright: bool,
        pulse: Option<gpui::Task<()>>,
    }
    impl Dictation {
        pub(crate) fn new(enabled: bool) -> Self {
            Self {
                enabled,
                phase: Phase::Idle,
                engine: Box::<CoreEngine>::default(),
                bright: false,
                pulse: None,
            }
        }
        fn finish(
            &mut self,
            result: Result<String, Failure>,
        ) -> Result<Option<(Origin, String)>, Failure> {
            let Phase::Transcribing(origin) = std::mem::replace(&mut self.phase, Phase::Idle)
            else {
                return Ok(None);
            };
            result.map(|text| (!text.is_empty()).then_some((origin, text)))
        }
        fn recording(&self) -> bool {
            matches!(self.phase, Phase::Recording(_))
        }
        fn toggle(&mut self, origin: Option<Origin>) -> Result<Option<Job>, Failure> {
            if !self.enabled || matches!(self.phase, Phase::Transcribing(_)) {
                return Ok(None);
            }
            if let Phase::Recording(origin) = std::mem::replace(&mut self.phase, Phase::Idle) {
                self.pulse = None;
                if let Some(job) = self.engine.stop() {
                    self.phase = Phase::Transcribing(origin);
                    return Ok(Some(job));
                }
            } else if let Some(origin) = origin {
                self.engine.start().map_err(Failure::Capture)?;
                self.phase = Phase::Recording(origin);
            }
            Ok(None)
        }
    }

    impl Shell {
        fn dictation_origin(&self, cx: &App) -> Option<Origin> {
            let pane = self.active_chat_pane(cx)?;
            let view = match &pane.read(cx).content {
                pane::PaneContent::Chat(view) => view.clone(),
                pane::PaneContent::PreviewChat(chat) => chat.read(cx).dictation_view(),
                _ => return None,
            };
            Some(Origin {
                pane: pane.downgrade(),
                view: view.downgrade(),
            })
        }
        pub(crate) fn toggle_dictation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            let origin = self.dictation_origin(cx);
            let was_recording = self.dictation.recording();
            match self.dictation.toggle(origin) {
                Ok(Some(job)) => {
                    cx.spawn_in(window, async move |shell, cx| {
                        let result = cx.background_executor().spawn(async move { job() }).await;
                        _ = shell.update_in(cx, |shell, window, cx| {
                            shell.finish_dictation(result, window, cx);
                        });
                    })
                    .detach();
                }
                Ok(None) => {}
                Err(error) => eprintln!("{error}"),
            }
            if !was_recording && self.dictation.recording() {
                self.dictation.pulse = Some(cx.spawn(async move |shell, cx| {
                    loop {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(500))
                            .await;
                        if shell
                            .update(cx, |shell, cx| {
                                shell.dictation.bright = !shell.dictation.bright;
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
            }
            cx.notify();
        }
        fn finish_dictation(
            &mut self,
            result: Result<String, Failure>,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            match self.dictation.finish(result) {
                Ok(Some((origin, text))) => {
                    if let Some(view) = self.dictation_target(&origin) {
                        let focus = self
                            .active_chat_pane(cx)
                            .is_some_and(|pane| pane.entity_id() == origin.pane.entity_id());
                        view.update(cx, |view, cx| {
                            view.insert_dictation(&text, focus, window, cx);
                        });
                    }
                }
                Ok(None) => {}
                Err(error) => eprintln!("{error}"),
            }
            cx.notify();
        }
        fn dictation_target(&self, origin: &Origin) -> Option<Entity<ChatView>> {
            // A closed pane can still be retained by docking or a queued event.
            // Check membership as well as weak identity before touching its draft.
            self.workspaces
                .iter()
                .any(|state| {
                    state
                        .panes
                        .values()
                        .any(|pane| pane.entity_id() == origin.pane.entity_id())
                })
                .then(|| origin.view.upgrade())
                .flatten()
        }
        pub(crate) fn dictation_enabled(&self) -> bool {
            self.dictation.enabled
        }
        pub(crate) fn dictation_indicator(&self, cx: &App) -> Option<workspace_bar::News> {
            if !self.dictation.enabled {
                return None;
            }
            let colors =
                workspace_bar::Colors::new(gpui_component::Theme::global(cx).mode.is_dark());
            let (label, color) = match self.dictation.phase {
                Phase::Recording(_) => (
                    "● REC",
                    if self.dictation.bright {
                        colors.red
                    } else {
                        colors.peach
                    },
                ),
                Phase::Transcribing(_) => ("…", colors.yellow),
                Phase::Idle => ("○", colors.overlay0),
            };
            Some(workspace_bar::News {
                label: label.into(),
                color,
            })
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use gpui::{TestAppContext, VisualTestContext};
        use std::cell::Cell;

        struct FakeEngine {
            starts: Rc<Cell<usize>>,
            denied: bool,
            result: Option<Result<String, Failure>>,
        }
        impl Engine for FakeEngine {
            fn start(&mut self) -> Result<(), String> {
                self.starts.set(self.starts.get() + 1);
                if self.denied {
                    Err("microphone access denied".into())
                } else {
                    Ok(())
                }
            }
            fn stop(&mut self) -> Option<Job> {
                self.result
                    .take()
                    .map(|result| Box::new(move || result) as Job)
            }
        }
        fn install(
            shell: &Entity<Shell>,
            result: Option<Result<String, Failure>>,
            cx: &mut VisualTestContext,
        ) -> Rc<Cell<usize>> {
            let starts = Rc::new(Cell::new(0));
            shell.update(cx, |shell, _| {
                shell.dictation.engine = Box::new(FakeEngine {
                    starts: starts.clone(),
                    denied: false,
                    result,
                });
            });
            starts
        }
        fn start_stop(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> (Origin, Job) {
            shell.update(cx, |shell, cx| {
                let origin = shell.dictation_origin(cx).unwrap();
                assert!(
                    shell
                        .dictation
                        .toggle(Some(origin.clone()))
                        .unwrap()
                        .is_none()
                );
                let job = shell
                    .dictation
                    .toggle(Some(origin.clone()))
                    .unwrap()
                    .unwrap();
                (origin, job)
            })
        }
        #[gpui::test]
        fn switched_or_closed_chat_cannot_redirect_a_transcript(cx: &mut TestAppContext) {
            let _test_app = crate::test_app::Cleanup::new(cx);
            let (_root, shell, cx) = h6_tests::sample(cx);
            install(&shell, Some(Ok("spoken".into())), cx);
            let (origin, job) = start_stop(&shell, cx);
            shell.update_in(cx, |shell, window, cx| {
                shell.select_workspace(1, window, cx)
            });
            cx.run_until_parked();
            let other = shell.update(cx, |shell, cx| shell.dictation_origin(cx).unwrap());
            assert_ne!(origin.view.entity_id(), other.view.entity_id());
            let changed = Rc::new(RefCell::new(Vec::new()));
            let _observers = shell.update(cx, |_, cx| {
                [
                    origin.view.upgrade().unwrap(),
                    other.view.upgrade().unwrap(),
                ]
                .map(|view| {
                    let changed = changed.clone();
                    cx.observe(&view, move |_, view, _| {
                        changed.borrow_mut().push(view.entity_id())
                    })
                })
            });
            shell.update_in(cx, |shell, window, cx| {
                shell.finish_dictation(job(), window, cx)
            });
            cx.run_until_parked();
            assert!(changed.borrow().contains(&origin.view.entity_id()));
            assert!(!changed.borrow().contains(&other.view.entity_id()));

            // Hold the original pane alive after removing it, as a queued event
            // or dock can. Completion must not update it or the replacement chat.
            shell.update_in(cx, |shell, window, cx| {
                shell.select_workspace(0, window, cx)
            });
            install(&shell, Some(Ok("late".into())), cx);
            let (origin, job) = start_stop(&shell, cx);
            let pane = origin.pane.upgrade().unwrap();
            let closed_view = origin.view.upgrade().unwrap();
            shell.update_in(cx, |shell, window, cx| {
                // Sample chats have no runtime target until a host supplies one.
                let target = sessions::fresh_target();
                pane.update(cx, |pane, _| pane.session = Some(target.clone()));
                shell.close_target(&target, window, cx);
            });
            cx.run_until_parked();
            changed.borrow_mut().clear();
            shell.update_in(cx, |shell, window, cx| {
                shell.finish_dictation(job(), window, cx)
            });
            cx.run_until_parked();
            assert!(origin.pane.upgrade().is_some());
            assert!(changed.borrow().is_empty());
            drop((pane, closed_view));
        }
        #[gpui::test]
        fn switch_while_recording_keeps_the_starting_composer(cx: &mut TestAppContext) {
            let _test_app = crate::test_app::Cleanup::new(cx);
            let (_root, shell, cx) = h6_tests::sample(cx);
            install(&shell, Some(Ok("spoken".into())), cx);
            let origin = shell.update(cx, |shell, cx| {
                let origin = shell.dictation_origin(cx).unwrap();
                assert!(
                    shell
                        .dictation
                        .toggle(Some(origin.clone()))
                        .unwrap()
                        .is_none()
                );
                origin
            });
            shell.update_in(cx, |shell, window, cx| {
                shell.select_workspace(1, window, cx)
            });
            shell.update(cx, |shell, cx| {
                let other = shell.dictation_origin(cx);
                let job = shell.dictation.toggle(other).unwrap().unwrap();
                let (target, _) = shell.dictation.finish(job()).unwrap().unwrap();
                assert_eq!(target.view.entity_id(), origin.view.entity_id());
            });
        }
        #[gpui::test]
        fn second_toggle_during_transcription_keeps_the_original_job(cx: &mut TestAppContext) {
            let _test_app = crate::test_app::Cleanup::new(cx);
            let (_root, shell, cx) = h6_tests::sample(cx);
            let starts = install(&shell, Some(Ok("spoken".into())), cx);
            let (origin, job) = start_stop(&shell, cx);
            shell.update_in(cx, |shell, window, cx| {
                shell.select_workspace(1, window, cx)
            });
            shell.update(cx, |shell, cx| {
                let other = shell.dictation_origin(cx);
                assert!(shell.dictation.toggle(other).unwrap().is_none());
                assert_eq!(starts.get(), 1);
                let (target, _) = shell.dictation.finish(job()).unwrap().unwrap();
                assert_eq!(target.view.entity_id(), origin.view.entity_id());
                assert!(matches!(shell.dictation.phase, Phase::Idle));
            });
        }
        #[gpui::test]
        fn silent_empty_and_failed_results_return_to_idle(cx: &mut TestAppContext) {
            let _test_app = crate::test_app::Cleanup::new(cx);
            let (_root, shell, cx) = h6_tests::sample(cx);
            install(&shell, None, cx); // core filtered silent/short audio
            shell.update(cx, |shell, cx| {
                let origin = shell.dictation_origin(cx);
                assert!(shell.dictation.toggle(origin.clone()).unwrap().is_none());
                assert!(shell.dictation.toggle(origin).unwrap().is_none());
                assert!(matches!(shell.dictation.phase, Phase::Idle));
            });
            install(&shell, Some(Ok(String::new())), cx);
            let (_, job) = start_stop(&shell, cx);
            shell.update(cx, |shell, _| {
                assert!(shell.dictation.finish(job()).unwrap().is_none());
                assert!(matches!(shell.dictation.phase, Phase::Idle));
            });
            let missing = tempfile::tempdir().unwrap();
            let path = missing.path().join("ggml-base.en.bin");
            let error = Transcriber::load(&path).err().unwrap();
            assert!(error.starts_with("Model not found. Download it with:"));
            install(&shell, Some(Err(Failure::Model(error.clone()))), cx);
            let (_, job) = start_stop(&shell, cx);
            shell.update(cx, |shell, _| {
                let failure = shell.dictation.finish(job()).err().unwrap();
                assert_eq!(failure.to_string(), format!("[STT] {error}"));
                assert!(matches!(shell.dictation.phase, Phase::Idle));
                shell.dictation.engine = Box::new(FakeEngine {
                    starts: Rc::new(Cell::new(0)),
                    denied: true,
                    result: None,
                });
            });
            shell.update(cx, |shell, cx| {
                let origin = shell.dictation_origin(cx);
                let failure = shell.dictation.toggle(origin).err().unwrap();
                assert_eq!(
                    failure.to_string(),
                    "[STT] Failed to start recording: microphone access denied"
                );
                assert!(matches!(shell.dictation.phase, Phase::Idle));
            });
        }
        #[gpui::test]
        fn shortcut_indicator_and_half_second_pulse_match_iced(cx: &mut TestAppContext) {
            let _test_app = crate::test_app::Cleanup::new(cx);
            let (_root, shell, cx) = h6_tests::sample(cx);
            install(&shell, None, cx);
            let mic = |shell: &Entity<Shell>, cx: &VisualTestContext| {
                cx.read(|cx| shell.read(cx).dictation_indicator(cx).unwrap())
            };
            assert_eq!(mic(&shell, cx).label, "○");
            cx.simulate_keystrokes("ctrl-space");
            cx.run_until_parked();
            assert_eq!(mic(&shell, cx).label, "● REC");
            assert_eq!(
                mic(&shell, cx).color,
                workspace_bar::Colors::new(true).peach
            );
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(499));
            cx.run_until_parked();
            assert_eq!(
                mic(&shell, cx).color,
                workspace_bar::Colors::new(true).peach
            );
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(1));
            cx.run_until_parked();
            assert_eq!(mic(&shell, cx).color, workspace_bar::Colors::new(true).red);
            cx.simulate_keystrokes("ctrl-space");
            cx.run_until_parked();
            assert_eq!(mic(&shell, cx).label, "○");
            install(&shell, Some(Ok("spoken".into())), cx);
            let (_, job) = start_stop(&shell, cx);
            assert_eq!(mic(&shell, cx).label, "…");
            assert_eq!(
                mic(&shell, cx).color,
                workspace_bar::Colors::new(true).yellow
            );
            shell.update_in(cx, |shell, window, cx| {
                shell.finish_dictation(job(), window, cx)
            });
            assert_eq!(mic(&shell, cx).label, "○");
        }
    }
}

#[cfg(not(feature = "stt"))]
impl Shell {
    pub(crate) fn toggle_dictation(&mut self, _: &mut Window, _: &mut Context<Self>) {}
    pub(crate) fn dictation_enabled(&self) -> bool {
        false
    }
    pub(crate) fn dictation_indicator(&self, _: &App) -> Option<workspace_bar::News> {
        None
    }
}
