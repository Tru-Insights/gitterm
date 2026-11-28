//! Shared V6 view preferences; Preview retains its isolated layout behavior.
use super::*;

gpui::actions!(gitterm_iced, [SearchPrev]);

#[derive(Default)]
struct LiveRegions(HashSet<WorkspaceId>);
impl gpui::Global for LiveRegions {}

impl Shell {
    pub(super) fn live_preferences(cx: &App) -> bool {
        cx.try_global::<workspace_config::LiveWorkspaces>()
            .is_some()
    }

    pub(super) fn save_view_preferences(
        &self,
        edit: impl FnOnce(&mut gitterm_core::settings::DesktopSettings) + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = cx.try_global::<workspace_config::LiveWorkspaces>() else {
            return;
        };
        let fonts = theme::font_sizes(cx);
        let receipt = store.0.edit_settings(move |settings| {
            edit(settings);
            // Iced clears the legacy combined size when saving any preference.
            settings.font_size = None;
            settings.ui_font_size = fonts.ui;
            settings.terminal_font_size = fonts.terminal;
        });
        let save = cx
            .background_executor()
            .spawn(async move { receipt.wait() });
        cx.spawn(async move |shell, cx| {
            if let Err(error) = save.await {
                let _ = shell.update(cx, |shell, cx| {
                    shell.owner.update(cx, |owner, cx| {
                        owner.error = Some(format!("Save V6 view preferences: {error}"));
                        cx.notify();
                    });
                });
            }
        })
        .detach();
    }

    pub(super) fn restore_view_preferences(
        workspaces: &mut [WorkspaceDock],
        settings: &workspace_config::IcedSettings,
        bottom_height: f32,
        cx: &mut App,
    ) -> f32 {
        let live = Self::live_preferences(cx);
        // Preview has no V6 writer. Its most recently saved divider wins over
        // the read-only Iced seed, even when another workspace was dragged.
        let restored = if live {
            None
        } else {
            workspaces
                .iter()
                .filter_map(|state| {
                    let modified = std::fs::metadata(&state.store.path).ok()?.modified().ok()?;
                    Some((
                        modified,
                        &state.store.path,
                        state.store.regions.sidebar_width,
                    ))
                })
                .max_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)))
                .map(|(_, _, width)| width)
        };
        let width = layout::clamp_sidebar(restored.unwrap_or(settings.sidebar_width));
        for state in workspaces {
            let regions = &mut state.store.regions;
            if live {
                cx.default_global::<LiveRegions>()
                    .0
                    .insert(state.workspace.id);
                // Iced's settings.json is authoritative over older GPUI layouts.
                regions.fonts = None;
                regions.chat_font_size = None;
                regions.chat_width = None;
                regions.bottom_height = bottom_height;
                regions.bottom_visible = settings.console_expanded;
            }
            regions.resize_sidebar(width);
        }
        width
    }

    pub(super) fn toggle_view_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = !gpui_component::Theme::global(cx).mode.is_dark();
        theme::apply(dark, Some(window), cx);
        self.save_view_preferences(
            move |settings| {
                settings.theme = if dark { "dark" } else { "light" }.into();
            },
            cx,
        );
        self.refresh_view_appearance(cx);
        cx.notify();
    }

    pub(super) fn refresh_view_appearance(&self, cx: &mut Context<Self>) {
        let colors = terminal_palette(cx);
        for state in &self.workspaces {
            for pane in state.panes.values() {
                pane.update(cx, |pane, cx| {
                    match &pane.content {
                        pane::PaneContent::Chat(view) => view.update(cx, |view, cx| {
                            view.set_appearance(theme::chat_appearance(cx), cx)
                        }),
                        pane::PaneContent::Terminal(view) => {
                            // Keep the PTY/session alive while applying the new palette.
                            if view.read(cx).config().colors.background() != colors.background() {
                                let mut config = view.read(cx).config().clone();
                                config.colors = colors.clone();
                                view.update(cx, |view, cx| view.update_config(config, cx));
                            }
                        }
                        _ => {}
                    }
                });
            }
        }
    }

    pub(super) fn toggle_log_mirroring(&mut self, cx: &mut Context<Self>) {
        if !Self::live_preferences(cx) {
            self.preview_unavailable("Terminal log mirroring", cx);
            return;
        }
        let enabled = !self.bottom.read(cx).input.mirroring;
        self.bottom.update(cx, |bottom, cx| {
            let mut input = bottom.input.clone();
            input.mirroring = enabled;
            bottom.set_input(input, cx);
        });
        self.save_view_preferences(
            move |settings| {
                settings.terminal_log_mirroring_enabled = enabled;
            },
            cx,
        );
        self.sync_log_mirroring(cx);
        cx.notify();
    }

    pub(super) fn sync_log_mirroring(&mut self, cx: &mut Context<Self>) {
        // Cancel publication before clearing, including a pending server lock.
        self.terminal_mirror = None;
        if !Self::live_preferences(cx) {
            return;
        }
        let Some(state) = cx
            .try_global::<task_host::GpuiTaskHost>()
            .and_then(|host| host.0.read(cx).log_state())
            .cloned()
        else {
            return;
        };
        let enabled = self.bottom.read(cx).input.mirroring;
        self.terminal_mirror = Some(cx.spawn(async move |shell, cx| {
            if !enabled {
                state.terminals.write().await.clear();
                return;
            }
            loop {
                let Ok(jobs) = shell.update(cx, |shell, cx| {
                    let mut jobs = Vec::new();
                    // Iced's queue_log_server_sync includes main tabs in every
                    // workspace, including hidden tabs, but not bottom terminals.
                    for workspace in &shell.workspaces {
                        let bottom = bottom_pane_ids(workspace, cx);
                        for session in &workspace.sessions {
                            let id = session_pane_id(session.id);
                            if bottom.contains(&id) {
                                continue;
                            }
                            let Some(pane) = workspace.panes.get(&id) else {
                                continue;
                            };
                            let pane = pane.read(cx);
                            let pane::PaneContent::Terminal(view) = &pane.content else {
                                continue;
                            };
                            let Some(target) = &pane.session else {
                                continue;
                            };
                            let snapshot = gitterm_core::log_server::TerminalSnapshot {
                                tab_id: target.tab_id,
                                tab_name: pane
                                    .task_folder(cx)
                                    .and_then(|path| {
                                        path.file_name().map(|s| s.to_string_lossy().into_owned())
                                    })
                                    .unwrap_or_else(|| pane.title.clone()),
                                content: String::new(),
                            };
                            jobs.push((snapshot, view.read(cx).backend().export_job()));
                        }
                    }
                    jobs
                }) else {
                    break;
                };
                let mut snapshots = HashMap::new();
                let mut error = None;
                for (mut snapshot, job) in jobs {
                    let result = match job {
                        Ok(job) => loop {
                            match job.poll() {
                                gitterm_terminal::JobState::Ready(result) => break result,
                                gitterm_terminal::JobState::Pending => {
                                    cx.background_executor()
                                        .timer(std::time::Duration::from_millis(20))
                                        .await;
                                }
                            }
                        },
                        Err(error) => Err(error),
                    };
                    match result {
                        Ok(content) => {
                            snapshot.content = content;
                            snapshots.insert(snapshot.tab_id, snapshot);
                        }
                        Err(failed) => {
                            if error.is_none() || failed != gitterm_terminal::ServiceError::Busy {
                                error = Some(failed);
                            }
                        }
                    }
                }
                let retry = error == Some(gitterm_terminal::ServiceError::Busy);
                if let Some(error) = error {
                    if error != gitterm_terminal::ServiceError::Busy {
                        let _ = shell.update(cx, |shell, cx| {
                            shell.owner.update(cx, |owner, cx| {
                                owner.error = Some(format!("Mirror terminal logs: {error:?}"));
                                cx.notify();
                            });
                        });
                    }
                } else {
                    // A single cancellable task orders exports, publication and
                    // opt-out clearing; late export workers cannot republish.
                    *state.terminals.write().await = snapshots;
                }
                cx.background_executor()
                    .timer(if retry {
                        std::time::Duration::from_millis(100)
                    } else {
                        std::time::Duration::from_secs(15)
                    })
                    .await;
            }
        }));
    }

    pub(super) fn save_global_geometry(&mut self, cx: &mut Context<Self>) {
        if !Self::live_preferences(cx) || self.active_workspace(cx).is_none() {
            return;
        }
        let regions = &self.active(cx).store.regions;
        let (height, expanded) = (regions.bottom_height, regions.bottom_visible);
        let width = self.sidebar_width;
        for state in &mut self.workspaces {
            state.store.regions.bottom_height = height;
            state.store.regions.bottom_visible = expanded;
            state.store.regions.resize_sidebar(width);
        }
        self.save_view_preferences(
            move |settings| {
                settings.sidebar_width = width;
                settings.console_height = height;
                settings.console_expanded = expanded;
            },
            cx,
        );
    }

    pub(super) fn refresh_global_geometry(&mut self, window_height: f32, cx: &mut Context<Self>) {
        if !Self::live_preferences(cx) {
            return;
        }
        let input = self.bottom.read(cx).input.clone();
        cx.default_global::<LiveRegions>().0.retain(|id| {
            self.workspaces
                .iter()
                .any(|state| state.workspace.id == *id)
        });
        for state in &mut self.workspaces {
            if cx
                .default_global::<LiveRegions>()
                .0
                .insert(state.workspace.id)
            {
                state.store.regions.bottom_height = input.height;
                state.store.regions.bottom_visible = input.expanded;
                state.store.regions.resize_sidebar(self.sidebar_width);
            }
        }
        let active = self.active_workspace(cx);
        let Some(state) = self
            .workspaces
            .iter()
            .find(|state| Some(state.workspace.id) == active)
        else {
            return;
        };
        let height = bottom_panel::clamp_height(state.store.regions.bottom_height, window_height);
        let resized_window = input.window_height != window_height;
        if resized_window {
            for state in &mut self.workspaces {
                state.store.regions.bottom_height = height;
            }
        }
        let state = self
            .workspaces
            .iter()
            .find(|state| Some(state.workspace.id) == active)
            .unwrap();
        let regions = &state.store.regions;
        if self.resize.is_none()
            && ((!resized_window && regions.bottom_height != input.height)
                || regions.bottom_visible != input.expanded)
        {
            self.save_global_geometry(cx);
        }
    }

    pub(super) fn finish_preference_resize(&mut self, cx: &mut Context<Self>) {
        if self.resize.is_some() {
            self.save_global_geometry(cx);
        }
    }
}

#[cfg(test)]
#[path = "view_preferences_tests.rs"]
mod tests;

// shortcut: the terminal renderer derives dim ANSI shades; use Iced's explicit
// dim colors when the renderer exposes custom dim palette entries.
fn terminal_palette(cx: &App) -> gitterm_terminal::ColorPalette {
    let dark = gpui_component::Theme::global(cx).mode.is_dark();
    let p = Palette::for_mode(dark);
    macro_rules! color {
        ($builder:expr, $method:ident, $color:expr) => {{
            let value: u32 = $color;
            $builder.$method((value >> 16) as u8, (value >> 8) as u8, value as u8)
        }};
    }
    let palette = gitterm_terminal::ColorPalette::builder();
    let palette = color!(palette, background, p.base);
    let palette = color!(palette, foreground, p.text);
    let palette = color!(palette, cursor, p.text);
    let palette = color!(palette, black, if dark { 0x45475a } else { 0x5c5f77 });
    let palette = color!(palette, red, p.red);
    let palette = color!(palette, green, p.green);
    let palette = color!(palette, yellow, p.yellow);
    let palette = color!(palette, blue, p.blue);
    let palette = color!(palette, magenta, p.pink);
    let palette = color!(palette, cyan, p.teal);
    let palette = color!(palette, white, if dark { 0xbac2de } else { 0xacb0be });
    let palette = color!(
        palette,
        bright_black,
        if dark { 0x585b70 } else { 0x6c6f85 }
    );
    let palette = color!(palette, bright_red, p.red);
    let palette = color!(palette, bright_green, p.green);
    let palette = color!(palette, bright_yellow, p.yellow);
    let palette = color!(palette, bright_blue, p.blue);
    let palette = color!(palette, bright_magenta, p.pink);
    let palette = color!(palette, bright_cyan, p.teal);
    color!(
        palette,
        bright_white,
        if dark { 0xa6adc8 } else { 0xbcc0cc }
    )
    .build()
}
