//! Dictation destination ownership and GPUI scheduling only; capture and Whisper live in core.
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
        view: Destination,
    }
    #[derive(Clone)]
    enum Destination {
        Chat(gpui::WeakEntity<ChatView>),
        Terminal(gpui::WeakEntity<gitterm_terminal::TerminalView>),
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
            let state = self
                .workspaces
                .iter()
                .find(|state| Some(state.workspace.id) == self.active_workspace(cx))?;
            let pane = state.panes.get(&session_pane_id(state.active_session?))?;
            let view = match &pane.read(cx).content {
                pane::PaneContent::Chat(view) => Destination::Chat(view.downgrade()),
                pane::PaneContent::PreviewChat(chat) => {
                    Destination::Chat(chat.read(cx).dictation_view().downgrade())
                }
                pane::PaneContent::Terminal(view) => Destination::Terminal(view.downgrade()),
                _ => return None,
            };
            Some(Origin {
                pane: pane.downgrade(),
                view,
            })
        }
        pub(crate) fn toggle_dictation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            if !chat::policy::is_live(cx) {
                self.preview_unavailable("Dictation", cx);
                return;
            }
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
                    if let Some(view) = self.dictation_target(&origin, cx) {
                        match view {
                            Destination::Chat(view) => {
                                if let Some(view) = view.upgrade() {
                                    let focus = self.active_chat_pane(cx).is_some_and(|pane| {
                                        pane.entity_id() == origin.pane.entity_id()
                                    });
                                    view.update(cx, |view, cx| {
                                        view.insert_dictation(&text, focus, window, cx);
                                    });
                                }
                            }
                            Destination::Terminal(view) => {
                                // Iced routes completion through Event::Terminal,
                                // which drops writes behind a capture layer.
                                if !self.pty_blocked(cx)
                                    && let Some(view) = view.upgrade()
                                {
                                    let bytes =
                                        gitterm_core::dictation::terminal_transcript_bytes(text);
                                    if !view.read(cx).write_input_bytes(&bytes) {
                                        eprintln!(
                                            "[STT] Failed to write transcript to originating terminal"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => eprintln!("{error}"),
            }
            cx.notify();
        }
        fn dictation_target(&self, origin: &Origin, cx: &App) -> Option<Destination> {
            // A closed pane can still be retained by docking or a queued event.
            // Also reject a restarted/replaced terminal within the same pane.
            let pane = self
                .workspaces
                .iter()
                .flat_map(|state| state.panes.values())
                .find(|pane| pane.entity_id() == origin.pane.entity_id())?;
            let matches = match (&origin.view, &pane.read(cx).content) {
                (Destination::Chat(view), pane::PaneContent::Chat(current)) => {
                    view.entity_id() == current.entity_id()
                }
                (Destination::Chat(view), pane::PaneContent::PreviewChat(current)) => {
                    view.entity_id() == current.read(cx).dictation_view().entity_id()
                }
                (Destination::Terminal(view), pane::PaneContent::Terminal(current)) => {
                    view.entity_id() == current.entity_id()
                }
                _ => false,
            };
            matches.then(|| origin.view.clone())
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
        use std::{cell::Cell, path::PathBuf};

        impl Origin {
            fn chat(&self) -> &gpui::WeakEntity<ChatView> {
                let Destination::Chat(view) = &self.view else {
                    panic!("expected chat origin")
                };
                view
            }
        }

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
        struct CapturedTerminal {
            view: Entity<gitterm_terminal::TerminalView>,
            received: PathBuf,
            writes: Arc<std::sync::atomic::AtomicUsize>,
        }
        fn wait_for(mut ready: impl FnMut() -> bool) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !ready() {
                assert!(std::time::Instant::now() < deadline, "fixture PTY deadline");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        fn terminal_fixture(
            shell: &Entity<Shell>,
            root: &Path,
            bracketed: bool,
            cx: &mut VisualTestContext,
        ) -> CapturedTerminal {
            let received = root.join("received");
            // An owned raw PTY captures bytes in a temp file; no shell commands,
            // host startup scripts, microphone, or transcript files are involved.
            let script = r#"import os, sys, tty
tty.setraw(0)
out = open(sys.argv[1], 'wb', buffering=0)
os.write(1, (b'\x1b[?2004h' if sys.argv[2] == 'yes' else b'') + b'FIXTURE_READY')
while True:
    data = os.read(0, 4096)
    if not data:
        break
    out.write(data)
"#;
            let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let view = cx.new(|cx| {
                let backend = gitterm_terminal::Backend::new(gitterm_terminal::BackendSettings {
                    program: "/usr/bin/python3".into(),
                    args: vec![
                        "-u".into(),
                        "-c".into(),
                        script.into(),
                        received.display().to_string(),
                        if bracketed { "yes" } else { "no" }.into(),
                    ],
                    working_directory: Some(root.to_path_buf()),
                    ..Default::default()
                })
                .unwrap();
                let writes = writes.clone();
                gitterm_terminal::TerminalView::new(backend, Default::default(), cx)
                    .with_input_callback(move || {
                        writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    })
            });
            wait_for(|| {
                view.update(cx, |view, _| {
                    view.backend_mut().sync();
                    view.backend()
                        .get_all_text()
                        .is_some_and(|text| text.contains("FIXTURE_READY"))
                })
            });
            shell.update_in(cx, |shell, window, cx| {
                let id = SessionId(shell.active_workspace(cx).unwrap().0 * 10 + 2);
                shell.select_session_id(id, window, cx);
                let pane = shell.active(cx).panes[&session_pane_id(id)].clone();
                pane.update(cx, |pane, _| {
                    pane.content = pane::PaneContent::Terminal(view.clone())
                });
                view.read(cx).focus_handle().clone().focus(window, cx);
            });
            CapturedTerminal {
                view,
                received,
                writes,
            }
        }
        #[gpui::test]
        fn focused_terminal_receives_iced_bytes_with_or_without_bracketed_paste(
            cx: &mut TestAppContext,
        ) {
            let _test_app = crate::test_app::Cleanup::new(cx);
            let (root, shell, cx) = h6_tests::sample(cx);
            for bracketed in [false, true] {
                let capture = tempfile::tempdir_in(root.path()).unwrap();
                let terminal = terminal_fixture(&shell, capture.path(), bracketed, cx);
                let text = "  spoken café\r\nnext\n\x1b[201~  ";
                let starts = install(&shell, Some(Ok(text.into())), cx);
                cx.update(|_, cx| {
                    if !chat::policy::is_live(cx) {
                        let services = gitterm_core::preview::service::ServiceContext::production(
                            &root.path().join("live"),
                            "terminal-dictation-test",
                        )
                        .unwrap();
                        chat::policy::install_live(services, cx).unwrap();
                    }
                });
                shell.update_in(cx, |shell, window, cx| shell.toggle_dictation(window, cx));
                assert_eq!(starts.get(), 1);
                shell.update_in(cx, |shell, window, cx| shell.toggle_dictation(window, cx));
                cx.run_until_parked();
                wait_for(|| std::fs::read(&terminal.received).unwrap().len() >= text.len());
                assert_eq!(std::fs::read(&terminal.received).unwrap(), text.as_bytes());
                assert_eq!(terminal.writes.load(std::sync::atomic::Ordering::SeqCst), 1);
            }
        }
        #[gpui::test]
        fn terminal_focus_move_during_recording_keeps_origin(cx: &mut TestAppContext) {
            let _test_app = crate::test_app::Cleanup::new(cx);
            let (root, shell, cx) = h6_tests::sample(cx);
            let terminal = terminal_fixture(&shell, root.path(), true, cx);
            install(&shell, Some(Ok("spoken".into())), cx);
            shell.update(cx, |shell, cx| {
                let origin = shell.dictation_origin(cx);
                assert!(shell.dictation.toggle(origin).unwrap().is_none());
            });
            shell.update_in(cx, |shell, window, cx| {
                shell.select_workspace(1, window, cx)
            });
            let other_root = tempfile::tempdir_in(root.path()).unwrap();
            let other = terminal_fixture(&shell, other_root.path(), true, cx);
            let job = shell.update(cx, |shell, cx| {
                let other = shell.dictation_origin(cx);
                shell.dictation.toggle(other).unwrap().unwrap()
            });
            shell.update_in(cx, |shell, window, cx| {
                shell.finish_dictation(job(), window, cx)
            });
            wait_for(|| std::fs::read(&terminal.received).unwrap() == b"spoken");
            assert_eq!(other.writes.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(std::fs::read(other.received).unwrap().is_empty());
        }
        #[gpui::test]
        fn terminal_closed_mid_recording_does_not_write_elsewhere(cx: &mut TestAppContext) {
            let _test_app = crate::test_app::Cleanup::new(cx);
            let (root, shell, cx) = h6_tests::sample(cx);
            let terminal = terminal_fixture(&shell, root.path(), true, cx);
            install(&shell, Some(Ok("late".into())), cx);
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
            // Retain both entities: membership must reject even a still-live pane.
            let pane = origin.pane.upgrade().unwrap();
            shell.update_in(cx, |shell, window, cx| {
                let target = pane.read(cx).session.clone().unwrap();
                shell.close_target(&target, window, cx);
                assert!(shell.dictation_target(&origin, cx).is_none());
                shell.select_workspace(1, window, cx);
            });
            let other_root = tempfile::tempdir_in(root.path()).unwrap();
            let other = terminal_fixture(&shell, other_root.path(), false, cx);
            let job = shell.update(cx, |shell, cx| {
                let origin = shell.dictation_origin(cx);
                shell.dictation.toggle(origin).unwrap().unwrap()
            });
            shell.update_in(cx, |shell, window, cx| {
                shell.finish_dictation(job(), window, cx)
            });
            assert_eq!(terminal.writes.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(other.writes.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(std::fs::read(terminal.received).unwrap().is_empty());
            assert!(std::fs::read(other.received).unwrap().is_empty());
            drop((pane, terminal.view));
        }
        #[gpui::test]
        fn terminal_dictation_respects_iced_capture_layers(cx: &mut TestAppContext) {
            let _test_app = crate::test_app::Cleanup::new(cx);
            let (root, shell, cx) = h6_tests::sample(cx);
            let terminal = terminal_fixture(&shell, root.path(), false, cx);
            install(&shell, Some(Ok("spoken".into())), cx);
            let (_, job) = start_stop(&shell, cx);
            shell.update_in(cx, |shell, window, cx| {
                shell.open_task_switcher(window, cx);
                assert!(shell.pty_blocked(cx));
                shell.finish_dictation(job(), window, cx);
            });
            assert_eq!(terminal.writes.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(std::fs::read(terminal.received).unwrap().is_empty());
        }
        #[gpui::test]
        fn preview_dictation_refuses_before_capture(cx: &mut TestAppContext) {
            let _test_app = crate::test_app::Cleanup::new(cx);
            let (root, shell, cx) = h6_tests::sample(cx);
            let terminal = terminal_fixture(&shell, root.path(), true, cx);
            let starts = install(&shell, Some(Ok("never delivered".into())), cx);
            shell.update_in(cx, |shell, window, cx| {
                shell.toggle_dictation(window, cx);
                assert!(matches!(shell.dictation.phase, Phase::Idle));
                assert_eq!(
                    shell.owner.read(cx).error.as_deref(),
                    Some("Dictation is not available in preview")
                );
            });
            assert_eq!(starts.get(), 0);
            assert_eq!(terminal.writes.load(std::sync::atomic::Ordering::SeqCst), 0);
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
            assert_ne!(origin.chat().entity_id(), other.chat().entity_id());
            let changed = Rc::new(RefCell::new(Vec::new()));
            let _observers = shell.update(cx, |_, cx| {
                [
                    origin.chat().upgrade().unwrap(),
                    other.chat().upgrade().unwrap(),
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
            assert!(changed.borrow().contains(&origin.chat().entity_id()));
            assert!(!changed.borrow().contains(&other.chat().entity_id()));

            // Hold the original pane alive after removing it, as a queued event
            // or dock can. Completion must not update it or the replacement chat.
            shell.update_in(cx, |shell, window, cx| {
                shell.select_workspace(0, window, cx)
            });
            install(&shell, Some(Ok("late".into())), cx);
            let (origin, job) = start_stop(&shell, cx);
            let pane = origin.pane.upgrade().unwrap();
            let closed_view = origin.chat().upgrade().unwrap();
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
                assert_eq!(target.chat().entity_id(), origin.chat().entity_id());
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
                assert_eq!(target.chat().entity_id(), origin.chat().entity_id());
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
            cx.update(|_, cx| {
                let services = gitterm_core::preview::service::ServiceContext::production(
                    &_root.path().join("live"),
                    "dictation-test",
                )
                .unwrap();
                chat::policy::install_live(services, cx).unwrap();
            });
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
