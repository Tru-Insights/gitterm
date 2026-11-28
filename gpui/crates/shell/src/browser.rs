//! Iced's managed Chrome controls and memory-only evidence, using the task
//! host's browser service. The embedded Page view has a separate identity/store.
use crate::{
    Shell,
    task_host::GpuiTaskHost,
    theme::Palette,
    workspace_bar::{Browser, BrowserAction, Colors},
};
use gitterm_browser::{
    browser_control::{
        BrowserCaptureMode, BrowserControlService, BrowserEvidence, BrowserLaunchOptions,
        BrowserState, BrowserStatus, BrowserTelemetrySnapshot,
    },
    browser_engine::BrowserEngine,
};
use gitterm_core::preview::service::{BrowserAccess, ServiceContext};
use gpui::{
    App, Context, FocusHandle, Image, ImageFormat, Window, div, img, prelude::*, px, rgb, rgba,
};
use gpui_component::{
    Sizable as _,
    button::{Button, ButtonVariants as _},
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

#[derive(Clone)]
pub(crate) struct Host {
    pub browser: BrowserControlService,
    pub engine: BrowserEngine,
    pub runtime: tokio::runtime::Handle,
    pub services: Arc<ServiceContext>,
    pub admission: gitterm_browser::browser_mcp::BrowserUiAdmission,
}
impl Host {
    async fn action(&self, action: BrowserAction) -> Result<Option<BrowserStatus>, String> {
        let _admitted = self.admission.enter()?;
        let (tool, access) = match action {
            BrowserAction::Open => ("browser_open", BrowserAccess::Launch),
            BrowserAction::Focus => ("browser_target_focus", BrowserAccess::Interact),
            BrowserAction::Disconnect => ("browser_disconnect", BrowserAccess::Interact),
            BrowserAction::Evidence => return Ok(None),
        };
        self.services
            .browser(tool, access)
            .map_err(|e| e.notice().to_string())?;
        match action {
            BrowserAction::Open => self
                .browser
                .launch(BrowserLaunchOptions::default())
                .await
                .map(Some)
                .map_err(|e| format!("could not open managed browser: {e}")),
            BrowserAction::Focus => self
                .browser
                .focus()
                .await
                .map(|_| None)
                .map_err(|e| format!("could not focus managed browser: {e}")),
            BrowserAction::Disconnect => {
                tokio::time::timeout(Duration::from_secs(8), self.browser.disconnect())
                    .await
                    .map_err(|_| {
                        "could not disconnect managed browser: operation timed out after 8 seconds"
                            .to_string()
                    })?
                    .map_err(|e| format!("could not disconnect managed browser: {e}"))?;
                self.browser
                    .status()
                    .await
                    .map(Some)
                    .map_err(|e| format!("could not refresh managed browser status: {e}"))
            }
            BrowserAction::Evidence => unreachable!(),
        }
    }
}

pub(crate) struct State {
    available: bool,
    status: Option<BrowserStatus>,
    action: Option<BrowserAction>,
    error: Option<String>,
    sticky: bool,
    telemetry: BrowserTelemetrySnapshot,
    labels: HashMap<String, String>,
    images: HashMap<u64, Arc<Image>>,
    pub evidence_open: bool,
    focus: FocusHandle,
    return_focus: Option<FocusHandle>,
    _telemetry: Option<gpui::Task<()>>,
}
impl State {
    pub fn new(cx: &App) -> Self {
        Self {
            available: false,
            status: None,
            action: None,
            error: None,
            sticky: false,
            telemetry: Default::default(),
            labels: Default::default(),
            images: Default::default(),
            evidence_open: false,
            focus: cx.focus_handle(),
            return_focus: None,
            _telemetry: None,
        }
    }
    fn running(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|s| s.state == BrowserState::Running)
    }
    pub fn bar(&self, c: Colors) -> Browser {
        let (mut hint, mut color) = if !self.available {
            ("Browser unavailable".to_string(), c.red)
        } else if let Some(action) = self.action {
            (
                match action {
                    BrowserAction::Open => "Opening browser…",
                    BrowserAction::Focus => "Focusing browser…",
                    _ => "Disconnecting…",
                }
                .into(),
                c.yellow,
            )
        } else if let Some(error) = &self.error {
            let first = error.lines().next().unwrap_or(error).trim();
            let short: String = first.chars().take(40).collect();
            (
                format!(
                    "Browser · {short}{}",
                    if first.chars().count() > 40 {
                        "…"
                    } else {
                        ""
                    }
                ),
                c.red,
            )
        } else if self.running() {
            ("Browser live".into(), c.green)
        } else {
            ("Browser idle".into(), c.muted)
        };
        let mut driving = false;
        if self.available && self.action.is_none() && self.error.is_none() {
            if let Some(activity) = &self.telemetry.active_agent_operation {
                hint = format!("Agent · {}", activity.operation.active_label());
                color = Palette::for_mode(c.dark).lavender;
                driving = true;
            } else if let Some(outcome) = &self.telemetry.last_agent_operation {
                let age = outcome.finished_at.elapsed().unwrap_or_default();
                if outcome.error.is_some() && age <= Duration::from_secs(8) {
                    hint = format!("Agent · {} failed", outcome.operation.active_label());
                    color = c.red;
                } else if outcome.error.is_none() && age <= Duration::from_secs(2) {
                    hint = format!("Agent · {}", outcome.operation.completed_label());
                    color = c.green;
                }
            }
        }
        Browser {
            hint,
            color,
            driving,
            action: (self.available && self.action.is_none()).then_some(
                if self.running() && self.error.is_none() {
                    BrowserAction::Focus
                } else {
                    BrowserAction::Open
                },
            ),
            has_evidence: !self.images.is_empty(),
        }
    }
    fn telemetry(&mut self, telemetry: BrowserTelemetrySnapshot) {
        // The status poll may finish after a newer telemetry notification.
        if telemetry.revision < self.telemetry.revision {
            return;
        }
        self.images
            .retain(|id, _| telemetry.evidence.iter().any(|e| e.id == *id));
        for capture in &telemetry.evidence {
            self.images.entry(capture.id).or_insert_with(|| {
                Arc::new(Image::from_bytes(
                    ImageFormat::Png,
                    capture.screenshot_png.as_ref().clone(),
                ))
            });
        }
        self.telemetry = telemetry;
    }
}

/// Iced `paired_browser_evidence`: latest plus best matching target of the
/// same caller. Never compare images belonging to different callers.
fn paired(evidence: &[BrowserEvidence]) -> Vec<&BrowserEvidence> {
    let Some(anchor) = evidence.iter().max_by_key(|e| e.id) else {
        return vec![];
    };
    let candidates = || {
        evidence
            .iter()
            .filter(|e| e.owner == anchor.owner && e.target != anchor.target)
    };
    let partner = candidates()
        .filter(|e| {
            e.label == anchor.label
                && e.capture_mode == anchor.capture_mode
                && e.viewport == anchor.viewport
        })
        .max_by_key(|e| e.id)
        .or_else(|| {
            candidates()
                .filter(|e| e.label == anchor.label)
                .max_by_key(|e| e.id)
        })
        .or_else(|| candidates().max_by_key(|e| e.id));
    evidence
        .iter()
        .filter(|e| e.id == anchor.id || partner.is_some_and(|p| p.id == e.id))
        .take(2)
        .collect()
}

fn host(cx: &App) -> Option<Host> {
    cx.try_global::<GpuiTaskHost>()?.0.read(cx).browser_host()
}

/// Called before every local terminal launch, with that pane's fresh identity.
/// Provider command rewriting uses Iced's helper; bearer stays in the env.
pub(crate) fn terminal_environment(
    launch: &mut gitterm_terminal::TerminalLaunch,
    caller: &str,
    cx: &App,
) {
    let Some(host) = cx.try_global::<GpuiTaskHost>() else {
        return;
    };
    let env = host.0.read(cx).browser_environment(caller);
    if let Some((_, endpoint)) = env
        .iter()
        .find(|(k, _)| k == gitterm_browser::browser_mcp::BROWSER_MCP_URL_ENV)
        && let Some(command) = &mut launch.startup_command
    {
        *command = gitterm_browser::browser_mcp::configure_browser_command(command, endpoint);
    }
    launch
        .env
        .extend(env.into_iter().map(|(k, v)| (k, Some(v))));
    // Live's interactive CLI launches also need Iced's shell integration
    // (including the Codex wrapper), not just startup-command rewriting.
    if let Some(host) = host.0.read(cx).browser_host()
        && !host.services.is_preview()
        && let Some(cwd) = launch.cwd.clone()
    {
        let values: Vec<_> = launch
            .env
            .iter()
            .filter_map(|(k, v)| v.as_deref().map(|v| (k.as_str(), v)))
            .collect();
        let spec = gitterm_core::task_host::command::terminal_command(
            &cwd,
            launch.startup_command.as_deref(),
            &values,
            host.services.state_root(),
            gitterm_browser::browser_mcp::configure_browser_command,
        );
        launch.program = spec.program;
        launch.args = spec.args;
        launch
            .env
            .extend(spec.env.into_iter().map(|(k, v)| (k, Some(v))));
        launch.startup_command = None;
    }
}

impl Shell {
    fn browser_labels(&self, cx: &App) -> HashMap<String, String> {
        self.workspaces
            .iter()
            .flat_map(|w| w.panes.values())
            .filter_map(|p| {
                let p = p.read(cx);
                let target = match &p.content {
                    crate::pane::PaneContent::PreviewChat(chat) => Some(chat.read(cx).target()),
                    _ => p.session.clone(),
                }?;
                Some((
                    target.session_uid,
                    p.terminal_title.clone().unwrap_or_else(|| p.title.clone()),
                ))
            })
            .collect()
    }
    pub(crate) fn start_browser_poll(&mut self, cx: &mut Context<Self>) {
        if let Some(host) = host(cx) {
            let browser = host.browser;
            let mut changes = browser.telemetry_changes();
            self.browser._telemetry =
                Some(cx.spawn(async move |shell: gpui::WeakEntity<Self>, cx| {
                    while changes.changed().await.is_ok() {
                        let telemetry = browser.telemetry().await;
                        if shell
                            .update(cx, |shell, cx| {
                                shell.browser.telemetry(telemetry);
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
        }
        cx.spawn(async move |shell: gpui::WeakEntity<Self>, cx| {
            loop {
                let job = shell.update(cx, |shell, cx| {
                    let Some(host) = host(cx) else {
                        shell.browser.available = false;
                        return None;
                    };
                    shell.browser.available = true;
                    if shell.browser.action.is_some() {
                        return None;
                    }
                    let labels = shell.browser_labels(cx);
                    let previous = shell.browser.labels.clone();
                    let runtime = host.runtime.clone();
                    let admitted = host.admission.enter().ok()?;
                    Some(runtime.spawn(async move {
                        let _admitted = admitted;
                        let sync = if previous != labels {
                            let released: HashSet<_> = previous
                                .keys()
                                .filter(|key| !labels.contains_key(*key))
                                .cloned()
                                .collect();
                            async {
                                host.engine.release_owners(&released).await?;
                                host.browser.set_owner_labels(labels.clone()).await
                            }
                            .await
                            .map_err(|e| format!("could not sync browser target owners: {e}"))
                        } else {
                            Ok(())
                        };
                        let status =
                            host.browser.status().await.map_err(|e| {
                                format!("could not refresh managed browser status: {e}")
                            });
                        let telemetry = host.browser.telemetry().await;
                        (sync, labels, status, telemetry)
                    }))
                });
                let Ok(job) = job else {
                    break;
                };
                if let Some(job) = job {
                    let result = job.await;
                    if shell
                        .update(cx, |shell, cx| {
                            match result {
                                Ok((sync, labels, status, telemetry)) => {
                                    let sync_error = match sync {
                                        Ok(()) => {
                                            shell.browser.labels = labels;
                                            None
                                        }
                                        Err(error) => {
                                            eprintln!("{error}");
                                            Some(error)
                                        }
                                    };
                                    // A user action may have started while status was loading.
                                    if shell.browser.action.is_none() {
                                        match status {
                                            Ok(status) => {
                                                shell.browser.status = Some(status);
                                                if !shell.browser.sticky {
                                                    shell.browser.error = None;
                                                }
                                            }
                                            Err(error) => {
                                                shell.browser.error = Some(error);
                                                shell.browser.sticky = false;
                                            }
                                        }
                                    }
                                    if let Some(error) = sync_error {
                                        shell.browser.error = Some(error);
                                    }
                                    shell.browser.telemetry(telemetry);
                                }
                                Err(error) => {
                                    shell.browser.error =
                                        Some(format!("managed browser refresh failed: {error}"))
                                }
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        })
        .detach();
    }
    pub(crate) fn browser_action(
        &mut self,
        action: BrowserAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action == BrowserAction::Evidence {
            if !self.browser.images.is_empty() {
                self.browser.return_focus = window.focused(cx);
                self.browser.evidence_open = true;
                self.browser.focus.focus(window, cx);
                cx.notify();
            }
            return;
        }
        if self.browser.action.is_some() {
            return;
        }
        let Some(host) = host(cx) else {
            self.owner.update(cx, |owner, cx| {
                owner.error = Some("Managed browser controls are unavailable".into());
                cx.notify();
            });
            return;
        };
        self.browser.action = Some(action);
        self.browser.error = None;
        self.browser.sticky = false;
        let runtime = host.runtime.clone();
        let job = runtime.spawn(async move { host.action(action).await });
        cx.spawn(async move |shell: gpui::WeakEntity<Self>, cx| {
            let result = job
                .await
                .unwrap_or_else(|e| Err(format!("managed browser action failed: {e}")));
            let _ = shell.update(cx, |shell, cx| {
                shell.browser.action = None;
                match result {
                    Ok(status) => {
                        if let Some(status) = status {
                            shell.browser.status = Some(status);
                        }
                        shell.browser.error = None;
                        shell.browser.sticky = false;
                    }
                    Err(error) => {
                        eprintln!("{error}");
                        shell.browser.error = Some(error.clone());
                        shell.browser.sticky = true;
                        shell.owner.update(cx, |owner, cx| {
                            owner.error = Some(error);
                            cx.notify();
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    pub(crate) fn close_browser_evidence(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.browser.evidence_open {
            return false;
        }
        self.browser.evidence_open = false;
        self.browser
            .return_focus
            .take()
            .unwrap_or_else(|| self.focus.clone())
            .focus(window, cx);
        cx.notify();
        true
    }
    pub(crate) fn browser_evidence_view(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.browser.evidence_open {
            return None;
        }
        let captures = paired(&self.browser.telemetry.evidence);
        let p = Palette::current(cx);
        let text =
            |label: String, color| div().text_size(px(11.)).text_color(rgb(color)).child(label);
        let mut row = div().flex().gap(px(12.)).flex_1().min_h_0();
        for capture in &captures {
            let mode = match capture.capture_mode {
                BrowserCaptureMode::Viewport => "viewport",
                BrowserCaptureMode::FullPage => "full page",
            };
            row = row.child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(6.))
                    .p(px(10.))
                    .bg(rgb(p.base))
                    .child(text(
                        format!("{} · {}", capture.target, capture.label),
                        p.blue,
                    ))
                    .child(text(
                        if capture.title.trim().is_empty() {
                            "Untitled page".into()
                        } else {
                            capture.title.clone()
                        },
                        p.text,
                    ))
                    .child(text(
                        capture
                            .display_url
                            .clone()
                            .unwrap_or_else(|| "URL unavailable".into()),
                        p.subtext0,
                    ))
                    .child(text(
                        format!(
                            "{} × {} · {mode} · {} console · {} network",
                            capture.viewport.width,
                            capture.viewport.height,
                            capture.console_error_count,
                            capture.failed_request_count
                        ),
                        if capture.console_error_count + capture.failed_request_count > 0 {
                            p.red
                        } else {
                            p.subtext0
                        },
                    ))
                    .children(self.browser.images.get(&capture.id).map(|image| {
                        img(image.clone())
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .object_fit(gpui::ObjectFit::Contain)
                    })),
            );
        }
        let size = window.viewport_size();
        let mut card = div()
            .id("browser-evidence-card")
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(16.))
            .w((size.width - px(80.)).max(px(320.)).min(px(1320.)))
            .h((size.height - px(100.)).max(px(300.)).min(px(780.)))
            .bg(rgb(p.mantle))
            .border_1()
            .border_color(rgb(p.border))
            .rounded(px(8.))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(text("Browser evidence comparison".into(), p.text))
                    .child(
                        Button::new("browser-evidence-close")
                            .debug_selector(|| "browser-evidence-close".into())
                            .ghost()
                            .small()
                            .label("Close")
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.close_browser_evidence(window, cx);
                            })),
                    ),
            )
            .child(text(
                format!(
                    "{} capture{} · memory only",
                    captures.len(),
                    if captures.len() == 1 { "" } else { "s" }
                ),
                p.subtext0,
            ))
            .child(text(
                "Best-matching labels, capture modes, and viewports are paired automatically"
                    .into(),
                p.blue,
            ));
        if let Some(capture) = captures.last() {
            card = card.child(text(
                format!(
                    "{} · {} × {} · {} · {}s ago",
                    capture.label,
                    capture.viewport.width,
                    capture.viewport.height,
                    if capture.viewport.mobile {
                        "mobile"
                    } else {
                        "desktop"
                    },
                    capture.captured_at.elapsed().unwrap_or_default().as_secs()
                ),
                p.subtext0,
            ));
        }
        if let Some(outcome) = &self.browser.telemetry.last_agent_operation
            && let Some(error) = &outcome.error
        {
            card = card.child(text(
                format!("{} failed: {error}", outcome.operation.active_label()),
                p.red,
            ));
        }
        Some(
            div()
                .id("browser-evidence")
                .debug_selector(|| "browser-evidence".into())
                .absolute()
                .inset_0()
                .occlude()
                .track_focus(&self.browser.focus)
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x11111bdd))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|shell, _, window, cx| {
                        shell.close_browser_evidence(window, cx);
                    }),
                )
                .on_key_down(
                    cx.listener(|shell, event: &gpui::KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            shell.close_browser_evidence(window, cx);
                        }
                        cx.stop_propagation();
                    }),
                )
                .child(card.child(row))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitterm_browser::browser_control::BrowserViewport;
    use std::time::SystemTime;
    fn evidence(id: u64, owner: &str, target: &str, label: &str) -> BrowserEvidence {
        BrowserEvidence {
            id,
            owner: owner.into(),
            target: target.into(),
            label: label.into(),
            capture_mode: BrowserCaptureMode::Viewport,
            title: "Fixture page".into(),
            display_url: Some("https://example.com/fixture".into()),
            viewport: BrowserViewport {
                width: 800,
                height: 600,
                device_scale_factor: 1.,
                mobile: false,
            },
            captured_at: SystemTime::now(),
            console_error_count: 1,
            failed_request_count: 2,
            screenshot_png: Arc::new(vec![]),
        }
    }
    #[test]
    fn browser_evidence_pairs_same_caller_and_prefers_matching_label() {
        let captures = vec![
            evidence(1, "fixture-a", "design", "settings"),
            evidence(2, "fixture-a", "other", "other"),
            evidence(3, "fixture-b", "foreign", "settings"),
            evidence(4, "fixture-a", "implementation", "settings"),
        ];
        assert_eq!(
            paired(&captures).iter().map(|e| e.id).collect::<Vec<_>>(),
            [1, 4]
        );
    }
    #[gpui::test]
    fn browser_evidence_ignores_older_poll_results(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut state = State::new(cx);
            state.telemetry(BrowserTelemetrySnapshot {
                revision: 2,
                evidence: vec![evidence(2, "fixture", "design", "settings")],
                ..Default::default()
            });
            state.telemetry(BrowserTelemetrySnapshot {
                revision: 1,
                evidence: vec![evidence(1, "fixture", "older", "settings")],
                ..Default::default()
            });
            assert_eq!(state.telemetry.revision, 2);
            assert!(state.images.contains_key(&2));
            assert!(!state.images.contains_key(&1));
        });
    }
    #[gpui::test]
    fn browser_bar_uses_status_busy_errors_and_agent_telemetry(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut state = State::new(cx);
            let colors = Colors::new(true);
            assert_eq!(state.bar(colors).action, None);
            state.available = true;
            assert_eq!(state.bar(colors).action, Some(BrowserAction::Open));
            state.status = Some(BrowserStatus {
                state: BrowserState::Running,
                profile_dir: "/tmp/fixture-profile".into(),
                devtools_port: None,
                process_id: None,
                target_id: None,
                active_target: None,
                detail: None,
            });
            assert_eq!(state.bar(colors).action, Some(BrowserAction::Focus));
            state.action = Some(BrowserAction::Open);
            assert_eq!(state.bar(colors).action, None);
            assert_eq!(state.bar(colors).hint, "Opening browser…");
            state.action = None;
            state.telemetry.active_agent_operation =
                Some(gitterm_browser::browser_control::BrowserOperationActivity {
                    id: 1,
                    operation: gitterm_browser::browser_control::BrowserOperation::Click,
                    started_at: SystemTime::now(),
                });
            assert!(state.bar(colors).driving);
            state.error = Some("fixture error".into());
            assert_eq!(state.bar(colors).hint, "Browser · fixture error");
            assert!(!state.bar(colors).driving);
            assert_eq!(state.bar(colors).action, Some(BrowserAction::Open));
        });
    }
    #[gpui::test]
    fn browser_evidence_blocks_background_and_escape_restores_focus(cx: &mut gpui::TestAppContext) {
        let (_root, shell, cx) = crate::h7_tests::fixture(cx);
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.browser.telemetry(BrowserTelemetrySnapshot {
                    evidence: vec![evidence(1, "fixture", "design", "settings")],
                    ..Default::default()
                });
                let focus = shell.focus.clone();
                focus.focus(window, cx);
                shell.browser_action(BrowserAction::Evidence, window, cx);
                assert!(shell.browser.evidence_open);
                assert!(shell.pty_blocked(cx));
                assert_eq!(
                    shell.shell_modal(window, cx),
                    Some(crate::app_menus::Modal::BrowserEvidence)
                );
                assert!(shell.browser_evidence_view(window, cx).is_some());
                assert!(!crate::app_menus::Modal::BrowserEvidence.allows("WorkspaceSelect"));
                shell.escape(&crate::app_menus::Escape, window, cx);
                assert!(!shell.browser.evidence_open);
                assert!(focus.is_focused(window));
            });
        });
    }
}
